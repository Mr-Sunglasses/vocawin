//! Whisper keep-alive cache with optional idle unload (opt-in).
//! Never / disabled keeps the model in RAM. A timeout unloads after quiet time.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

pub struct WhisperCache {
    commands: mpsc::Sender<CacheCommand>,
    loaded: Arc<AtomicBool>,
}

enum CacheCommand {
    Transcribe {
        model_path: PathBuf,
        pcm: Vec<f32>,
        language: Option<String>,
        use_gpu: bool,
        gpu_device: i32,
        keep_alive: bool,
        initial_prompt: String,
        reply: mpsc::Sender<Result<String, String>>,
    },
    /// Load a model ahead of its take (hotkey press). Queued before the
    /// take's Transcribe, so that take finds it loaded.
    Preload {
        model_path: PathBuf,
        use_gpu: bool,
        gpu_device: i32,
    },
    Unload,
    ConfigureIdle {
        enabled: bool,
        seconds: u32,
    },
}

impl WhisperCache {
    pub fn new() -> Self {
        let (commands, receiver) = mpsc::channel();
        let loaded = Arc::new(AtomicBool::new(false));
        let loaded_for_thread = loaded.clone();
        std::thread::Builder::new()
            .name("vocawin-whisper".into())
            .spawn(move || cache_thread_main(receiver, loaded_for_thread))
            .expect("Could not start Whisper cache thread");
        Self { commands, loaded }
    }

    pub fn is_loaded(&self) -> bool {
        self.loaded.load(Ordering::Relaxed)
    }

    pub fn transcribe(
        &self,
        model_path: PathBuf,
        pcm: Vec<f32>,
        language: Option<String>,
        use_gpu: bool,
        gpu_device: i32,
        keep_alive: bool,
        initial_prompt: String,
    ) -> Result<String, String> {
        let (reply, response) = mpsc::channel();
        self.commands
            .send(CacheCommand::Transcribe {
                model_path,
                pcm,
                language,
                use_gpu,
                gpu_device,
                keep_alive,
                initial_prompt,
                reply,
            })
            .map_err(|_| "Whisper cache thread is not running".to_string())?;
        response
            .recv()
            .map_err(|_| "Whisper cache thread did not respond".to_string())?
    }

    /// Starts loading `model_path` without waiting for it.
    pub fn preload(&self, model_path: PathBuf, use_gpu: bool, gpu_device: i32) {
        let _ = self.commands.send(CacheCommand::Preload {
            model_path,
            use_gpu,
            gpu_device,
        });
    }

    pub fn configure_idle(&self, enabled: bool, seconds: u32) {
        let _ = self
            .commands
            .send(CacheCommand::ConfigureIdle { enabled, seconds });
    }

    pub fn unload(&self) {
        let _ = self.commands.send(CacheCommand::Unload);
    }
}

fn cache_thread_main(commands: mpsc::Receiver<CacheCommand>, loaded: Arc<AtomicBool>) {
    let mut loaded_path: Option<PathBuf> = None;
    let mut context: Option<whisper_rs::WhisperContext> = None;
    let mut last_used = Instant::now();
    let mut idle_enabled = false;
    let mut idle_seconds = 300u32;

    loop {
        let timed_out = match commands.recv_timeout(Duration::from_secs(1)) {
            Ok(CacheCommand::Transcribe {
                model_path,
                pcm,
                language,
                use_gpu,
                gpu_device,
                keep_alive,
                initial_prompt,
                reply,
            }) => {
                let result = run_transcribe(
                    &mut loaded_path,
                    &mut context,
                    &model_path,
                    &pcm,
                    language.as_deref(),
                    use_gpu,
                    gpu_device,
                    keep_alive,
                    &initial_prompt,
                );
                loaded.store(context.is_some(), Ordering::Relaxed);
                if result.is_ok() {
                    last_used = Instant::now();
                }
                let _ = reply.send(result);
                false
            }
            Ok(CacheCommand::Preload {
                model_path,
                use_gpu,
                gpu_device,
            }) => {
                match ensure_loaded(&mut loaded_path, &mut context, &model_path, use_gpu, gpu_device) {
                    Ok(()) => last_used = Instant::now(),
                    Err(error) => crate::logbuf::debug(format!("Whisper preload failed: {error}")),
                }
                loaded.store(context.is_some(), Ordering::Relaxed);
                false
            }
            Ok(CacheCommand::Unload) => {
                if loaded_path.is_some() {
                    crate::logbuf::info("Whisper model unloaded.");
                }
                loaded_path = None;
                context = None;
                loaded.store(false, Ordering::Relaxed);
                false
            }
            Ok(CacheCommand::ConfigureIdle { enabled, seconds }) => {
                idle_enabled = enabled;
                idle_seconds = seconds.max(30);
                false
            }
            Err(mpsc::RecvTimeoutError::Timeout) => true,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };

        if timed_out
            && idle_enabled
            && context.is_some()
            && last_used.elapsed() >= Duration::from_secs(idle_seconds as u64)
        {
            loaded_path = None;
            context = None;
            loaded.store(false, Ordering::Relaxed);
        } else {
            loaded.store(context.is_some(), Ordering::Relaxed);
        }
    }
}

/// Loads `model_path` unless it is the model already loaded.
fn ensure_loaded(
    loaded_path: &mut Option<PathBuf>,
    context: &mut Option<whisper_rs::WhisperContext>,
    model_path: &PathBuf,
    use_gpu: bool,
    gpu_device: i32,
) -> Result<(), String> {
    if context.is_some() && loaded_path.as_ref() == Some(model_path) {
        return Ok(());
    }
    let mut context_params = whisper_rs::WhisperContextParameters::default();
    context_params.use_gpu(use_gpu);
    context_params.gpu_device(if gpu_device >= 0 { gpu_device } else { 0 });
    let next = whisper_rs::WhisperContext::new_with_params(
        model_path.to_string_lossy().as_ref(),
        context_params,
    )
    .map_err(|error| format!("Could not load Whisper model: {error}"))?;
    *context = Some(next);
    *loaded_path = Some(model_path.clone());
    crate::logbuf::info(format!(
        "Loaded Whisper model {} (gpu={use_gpu}, device={gpu_device})",
        model_path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("whisper")
    ));
    Ok(())
}

fn run_transcribe(
    loaded_path: &mut Option<PathBuf>,
    context: &mut Option<whisper_rs::WhisperContext>,
    model_path: &PathBuf,
    pcm: &[f32],
    language: Option<&str>,
    use_gpu: bool,
    gpu_device: i32,
    keep_alive: bool,
    initial_prompt: &str,
) -> Result<String, String> {
    ensure_loaded(loaded_path, context, model_path, use_gpu, gpu_device)?;
    let ctx = context
        .as_ref()
        .ok_or("Whisper context missing after load")?;
    let mut session = ctx
        .create_state()
        .map_err(|error| format!("Could not create Whisper session: {error}"))?;
    let mut parameters =
        whisper_rs::FullParams::new(whisper_rs::SamplingStrategy::Greedy { best_of: 1 });
    parameters.set_translate(false);
    parameters.set_language(language);
    parameters.set_print_special(false);
    parameters.set_print_progress(false);
    parameters.set_print_realtime(false);
    parameters.set_print_timestamps(false);
    // Engine field is initial_prompt (whisper.cpp has no vocabulary param).
    // Phone Android also sets carry_initial_prompt so the list survives
    // later 30s windows. whisper-rs 0.16 does not expose that flag; a single
    // PTT take is one window, so the prompt still reaches the decoder.
    let prompt = initial_prompt.replace('\0', "");
    if !prompt.is_empty() {
        parameters.set_initial_prompt(&prompt);
    }
    session
        .full(parameters, pcm)
        .map_err(|error| format!("Transcription failed: {error}"))?;
    let text = (0..session.full_n_segments())
        .filter_map(|index| {
            session
                .get_segment(index)
                .and_then(|segment| segment.to_str().ok().map(spoken_text))
        })
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if !keep_alive {
        *context = None;
        *loaded_path = None;
    }
    Ok(text)
}

/// A segment's text without whisper.cpp's non-speech markers. On silence or
/// noise Whisper writes tags such as `[BLANK_AUDIO]`, `[ Silence ]`,
/// `(music)` or `*sigh*` as ordinary text, and typing them is never right.
/// Square brackets are always markers: Whisper has no way to write dictated
/// words in them. Parentheses and asterisks count only when they are all
/// the segment holds, since spoken asides can use them.
fn spoken_text(segment: &str) -> String {
    let mut kept = String::with_capacity(segment.len());
    let mut rest = segment;
    while let Some(open) = rest.find('[') {
        let Some(close) = rest[open..].find(']') else {
            break;
        };
        kept.push_str(&rest[..open]);
        rest = &rest[open + close + 1..];
    }
    kept.push_str(rest);
    let text = kept.split_whitespace().collect::<Vec<_>>().join(" ");
    if only_markers(&text) {
        String::new()
    } else {
        text
    }
}

/// True when nothing but closed `[...]` / `(...)` / `*...*` groups, music
/// notes and punctuation is left. A group that never closes is speech.
fn only_markers(text: &str) -> bool {
    let mut inside: Option<char> = None;
    for ch in text.chars() {
        match inside {
            Some(close) if ch == close => inside = None,
            Some(_) => {}
            None => match ch {
                '[' => inside = Some(']'),
                '(' => inside = Some(')'),
                '*' => inside = Some('*'),
                _ if ch.is_alphanumeric() => return false,
                _ => {}
            },
        }
    }
    inside.is_none()
}

#[cfg(test)]
mod tests {
    use super::spoken_text;

    #[test]
    fn non_speech_markers_are_dropped() {
        for marker in [
            "[BLANK_AUDIO]",
            " [BLANK_AUDIO]",
            "[ Silence ]",
            "[MUSIC PLAYING]",
            "(silence)",
            "(upbeat music)",
            "*sigh*",
            "\u{266a}",
            "[BLANK_AUDIO] (wind blowing)",
        ] {
            assert_eq!(spoken_text(marker), "", "{marker:?}");
        }
    }

    #[test]
    fn speech_around_markers_is_kept() {
        assert_eq!(spoken_text(" Hello there."), "Hello there.");
        assert_eq!(spoken_text("[BLANK_AUDIO] Hello [MUSIC] world"), "Hello world");
        assert_eq!(spoken_text("Call me (maybe) later"), "Call me (maybe) later");
        assert_eq!(spoken_text("Five * three"), "Five * three");
        assert_eq!(spoken_text("an open [bracket"), "an open [bracket");
        assert_eq!(spoken_text("Hello [ Silence ] world"), "Hello world");
        assert_eq!(spoken_text("* more words"), "* more words");
        assert_eq!(spoken_text("(and then"), "(and then");
    }
}
