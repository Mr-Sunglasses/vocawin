//! Keeps one loaded ONNX model between takes, as `whisper_cache` does for
//! Whisper and Handy does for every engine.
//!
//! A take `take`s the model out as a `Lease`, decodes, and `put`s it back, so
//! a history retry that runs alongside simply loads its own copy. `preload`
//! starts a load when the hotkey goes down; a take that ends while that load
//! runs waits for it instead of loading a second copy.
//!
//! `unload` (idle, auto-pause, model switch or removal) and each new preload
//! bump a generation. A load or lease from an older generation is dropped
//! instead of kept, so an unload is never undone by work already in flight.

use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

pub struct ModelSlot<M> {
    state: Mutex<SlotState<M>>,
    loaded: Condvar,
}

struct SlotState<M> {
    /// The model at rest, its key (model id and accelerator), and when it
    /// was last used.
    cached: Option<(String, M, Instant)>,
    /// Key of the preload in progress.
    loading: Option<String>,
    /// Keys of models taken out by a decode (one entry per lease).
    in_use: Vec<String>,
    generation: u64,
}

/// A model taken out for one decode. Give it back with `put` or `release`.
pub struct Lease<M> {
    pub model: M,
    key: String,
    generation: u64,
}

impl<M> ModelSlot<M> {
    pub const fn new() -> Self {
        Self {
            state: Mutex::new(SlotState {
                cached: None,
                loading: None,
                in_use: Vec::new(),
                generation: 0,
            }),
            loaded: Condvar::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SlotState<M>> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The model for `key`: the cached one, the one a preload is finishing,
    /// or a fresh `load`. A cached model for another key is dropped.
    pub fn take(&self, key: &str, load: impl FnOnce() -> Result<M, String>) -> Result<Lease<M>, String> {
        let mut state = self.lock();
        while state.loading.as_deref() == Some(key) {
            state = self
                .loaded
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        let generation = state.generation;
        let cached = match state.cached.take() {
            Some((cached_key, model, _)) if cached_key == key => Some(model),
            _ => None,
        };
        state.in_use.push(key.to_string());
        drop(state);
        let model = match cached {
            Some(model) => Ok(model),
            None => load(),
        };
        match model {
            Ok(model) => Ok(Lease {
                model,
                key: key.to_string(),
                generation,
            }),
            Err(error) => {
                self.finish_use(key);
                Err(error)
            }
        }
    }

    fn finish_use(&self, key: &str) -> std::sync::MutexGuard<'_, SlotState<M>> {
        let mut state = self.lock();
        if let Some(position) = state.in_use.iter().position(|used| used == key) {
            state.in_use.remove(position);
        }
        state
    }

    /// Keeps the leased model for the next take, replacing whatever was
    /// kept, unless the slot was unloaded or moved on since it was taken.
    pub fn put(&self, lease: Lease<M>) {
        let mut state = self.finish_use(&lease.key);
        if lease.generation == state.generation {
            state.cached = Some((lease.key, lease.model, Instant::now()));
        }
    }

    /// Returns a lease without keeping its model (a failed decode).
    pub fn release(&self, lease: Lease<M>) {
        let state = self.finish_use(&lease.key);
        drop(state);
        drop(lease);
    }

    /// Loads `key` now unless it is already kept, loading, or in use.
    /// Blocks the caller, so run it on its own thread.
    pub fn preload(&self, key: &str, load: impl FnOnce() -> Result<M, String>) -> Result<(), String> {
        let generation = {
            let mut state = self.lock();
            let cached = state.cached.as_ref().is_some_and(|(cached, _, _)| cached == key);
            let busy = state.loading.as_deref() == Some(key) || state.in_use.iter().any(|used| used == key);
            if cached || busy {
                return Ok(());
            }
            // A newer choice wins: an older preload still running is
            // dropped when it finishes.
            state.generation += 1;
            state.cached = None;
            state.loading = Some(key.to_string());
            state.generation
        };
        let result = load();
        let mut state = self.lock();
        if state.loading.as_deref() == Some(key) {
            state.loading = None;
        }
        let outcome = match result {
            Ok(model) if state.generation == generation => {
                state.cached = Some((key.to_string(), model, Instant::now()));
                Ok(())
            }
            Ok(_) => Ok(()),
            Err(error) => Err(error),
        };
        drop(state);
        self.loaded.notify_all();
        outcome
    }

    /// True while a model is kept or out for a decode.
    pub fn is_loaded(&self) -> bool {
        let state = self.lock();
        state.cached.is_some() || !state.in_use.is_empty()
    }

    /// Drops the kept model, and makes work in flight drop its model too.
    pub fn unload(&self) {
        let mut state = self.lock();
        state.generation += 1;
        let model = state.cached.take();
        drop(state);
        drop(model);
    }

    /// Unloads a kept model unused for `idle`. True when one was unloaded.
    pub fn unload_if_idle(&self, idle: Duration) -> bool {
        let mut state = self.lock();
        let stale = state
            .cached
            .as_ref()
            .is_some_and(|(_, _, used)| used.elapsed() >= idle);
        if stale {
            state.generation += 1;
            let model = state.cached.take();
            drop(state);
            drop(model);
        }
        stale
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn a_kept_model_is_reused_and_another_key_reloads() {
        let slot = ModelSlot::<u32>::new();
        let loads = AtomicUsize::new(0);
        let load = |value| {
            loads.fetch_add(1, Ordering::SeqCst);
            Ok(value)
        };
        let lease = slot.take("a|cpu", || load(1)).unwrap();
        slot.put(lease);
        assert!(slot.is_loaded());
        let lease = slot.take("a|cpu", || load(99)).unwrap();
        assert_eq!(lease.model, 1);
        assert!(slot.is_loaded(), "in use still counts as loaded");
        slot.put(lease);
        let lease = slot.take("b|cpu", || load(2)).unwrap();
        assert_eq!(lease.model, 2);
        slot.release(lease);
        assert!(!slot.is_loaded(), "a released lease is not kept");
        assert_eq!(loads.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_take_waits_for_the_preload_of_its_model() {
        let slot = Arc::new(ModelSlot::<u32>::new());
        let loads = Arc::new(AtomicUsize::new(0));
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let preloading = {
            let slot = Arc::clone(&slot);
            let loads = Arc::clone(&loads);
            std::thread::spawn(move || {
                slot.preload("a|cpu", || {
                    started_tx.send(()).unwrap();
                    std::thread::sleep(Duration::from_millis(100));
                    loads.fetch_add(1, Ordering::SeqCst);
                    Ok(7)
                })
            })
        };
        started_rx.recv().unwrap();
        let lease = slot
            .take("a|cpu", || {
                loads.fetch_add(1, Ordering::SeqCst);
                Ok(0)
            })
            .unwrap();
        preloading.join().unwrap().unwrap();
        assert_eq!(lease.model, 7);
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn an_unload_during_a_preload_or_decode_sticks() {
        let slot = Arc::new(ModelSlot::<u32>::new());
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (unloaded_tx, unloaded_rx) = std::sync::mpsc::channel::<()>();
        let preloading = {
            let slot = Arc::clone(&slot);
            std::thread::spawn(move || {
                slot.preload("a|cpu", || {
                    started_tx.send(()).unwrap();
                    unloaded_rx.recv().unwrap();
                    Ok(1)
                })
            })
        };
        started_rx.recv().unwrap();
        slot.unload();
        unloaded_tx.send(()).unwrap();
        preloading.join().unwrap().unwrap();
        assert!(!slot.is_loaded(), "the preload's model was dropped");

        let lease = slot.take("a|cpu", || Ok(2)).unwrap();
        slot.unload();
        slot.put(lease);
        assert!(!slot.is_loaded(), "a lease from before the unload is dropped");
    }

    #[test]
    fn a_new_model_preloads_while_an_older_one_is_still_loading() {
        let slot = Arc::new(ModelSlot::<u32>::new());
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel::<()>();
        let older = {
            let slot = Arc::clone(&slot);
            std::thread::spawn(move || {
                slot.preload("a|cpu", || {
                    started_tx.send(()).unwrap();
                    finish_rx.recv().unwrap();
                    Ok(1)
                })
            })
        };
        started_rx.recv().unwrap();
        slot.preload("b|cpu", || Ok(2)).unwrap();
        finish_tx.send(()).unwrap();
        older.join().unwrap().unwrap();
        // B is kept; A finished later and was dropped instead of replacing it.
        let lease = slot.take("b|cpu", || panic!("b should be kept")).unwrap();
        assert_eq!(lease.model, 2);
    }

    #[test]
    fn a_model_in_use_is_not_preloaded_again() {
        let slot = ModelSlot::<u32>::new();
        let lease = slot.take("a|cpu", || Ok(1)).unwrap();
        slot.preload("a|cpu", || panic!("a is in use")).unwrap();
        slot.put(lease);
        assert!(slot.is_loaded());
    }

    #[test]
    fn a_failed_preload_is_reported_and_a_take_loads_normally() {
        let slot = ModelSlot::<u32>::new();
        assert!(slot.preload("b|cpu", || Err("missing".into())).is_err());
        assert!(!slot.is_loaded());
        assert_eq!(slot.take("b|cpu", || Ok(3)).unwrap().model, 3);
    }

    #[test]
    fn idle_models_unload() {
        let slot = ModelSlot::<u32>::new();
        let lease = slot.take("a|cpu", || Ok(1)).unwrap();
        slot.put(lease);
        assert!(!slot.unload_if_idle(Duration::from_secs(60)));
        assert!(slot.unload_if_idle(Duration::ZERO));
        assert!(!slot.is_loaded());
    }
}
