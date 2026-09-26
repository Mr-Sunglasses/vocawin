//! Keeps one loaded ONNX model between takes, as `whisper_cache` does for
//! Whisper and Handy does for every engine.
//!
//! A take `take`s the model out, decodes, and `put`s it back, so a history
//! retry that runs alongside simply loads its own copy. `preload` starts a
//! load when the hotkey goes down; a take that ends while that load runs
//! waits for it instead of loading a second copy.

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
    /// Key of a load in progress.
    loading: Option<String>,
}

impl<M> ModelSlot<M> {
    pub const fn new() -> Self {
        Self {
            state: Mutex::new(SlotState {
                cached: None,
                loading: None,
            }),
            loaded: Condvar::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SlotState<M>> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The model for `key`: the cached one, the one a preload is finishing,
    /// or a fresh `load`. A cached model for another key is dropped.
    pub fn take(&self, key: &str, load: impl FnOnce() -> Result<M, String>) -> Result<M, String> {
        let mut state = self.lock();
        while state.loading.as_deref() == Some(key) {
            state = self
                .loaded
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        match state.cached.take() {
            Some((cached_key, model, _)) if cached_key == key => return Ok(model),
            _ => {}
        }
        drop(state);
        load()
    }

    /// Keeps `model` for the next take, replacing whatever was kept.
    pub fn put(&self, key: &str, model: M) {
        self.lock().cached = Some((key.to_string(), model, Instant::now()));
    }

    /// Loads `key` now unless it is already loaded or loading. Blocks the
    /// caller, so run it on its own thread.
    pub fn preload(&self, key: &str, load: impl FnOnce() -> Result<M, String>) -> Result<(), String> {
        {
            let mut state = self.lock();
            let cached = state.cached.as_ref().is_some_and(|(cached, _, _)| cached == key);
            if cached || state.loading.is_some() {
                return Ok(());
            }
            state.cached = None;
            state.loading = Some(key.to_string());
        }
        let result = load();
        let mut state = self.lock();
        state.loading = None;
        let outcome = match result {
            Ok(model) => {
                state.cached = Some((key.to_string(), model, Instant::now()));
                Ok(())
            }
            Err(error) => Err(error),
        };
        drop(state);
        self.loaded.notify_all();
        outcome
    }

    pub fn is_loaded(&self) -> bool {
        self.lock().cached.is_some()
    }

    pub fn unload(&self) {
        let model = self.lock().cached.take();
        drop(model);
    }

    /// Unloads a model unused for `idle`. True when one was unloaded.
    pub fn unload_if_idle(&self, idle: Duration) -> bool {
        let mut state = self.lock();
        let stale = state
            .cached
            .as_ref()
            .is_some_and(|(_, _, used)| used.elapsed() >= idle);
        if stale {
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
        let model = slot.take("a|cpu", || load(1)).unwrap();
        slot.put("a|cpu", model);
        assert!(slot.is_loaded());
        assert_eq!(slot.take("a|cpu", || load(99)).unwrap(), 1);
        assert!(!slot.is_loaded(), "taken while in use");
        slot.put("a|cpu", 1);
        assert_eq!(slot.take("b|cpu", || load(2)).unwrap(), 2);
        assert!(!slot.is_loaded(), "the other model was dropped");
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
        let model = slot
            .take("a|cpu", || {
                loads.fetch_add(1, Ordering::SeqCst);
                Ok(0)
            })
            .unwrap();
        preloading.join().unwrap().unwrap();
        assert_eq!(model, 7);
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn preload_skips_a_loaded_model_and_reports_a_failed_load() {
        let slot = ModelSlot::<u32>::new();
        slot.put("a|cpu", 1);
        slot.preload("a|cpu", || panic!("already loaded")).unwrap();
        assert!(slot.preload("b|cpu", || Err("missing".into())).is_err());
        assert!(!slot.is_loaded());
        // A take after the failed preload loads normally.
        assert_eq!(slot.take("b|cpu", || Ok(3)).unwrap(), 3);
    }

    #[test]
    fn idle_models_unload() {
        let slot = ModelSlot::<u32>::new();
        slot.put("a|cpu", 1);
        assert!(!slot.unload_if_idle(Duration::from_secs(60)));
        assert!(slot.unload_if_idle(Duration::ZERO));
        assert!(!slot.is_loaded());
        slot.put("a|cpu", 1);
        slot.unload();
        assert!(!slot.is_loaded());
    }
}
