//! Finds speech with Silero VAD, as VocaMac does, for Skip Silence.
//!
//! The model (Silero VAD v4, MIT, 1.8 MB) is not in the installer (AGENTS.md:
//! no bundled models). Like VocaMac, it is fetched next to the speech models:
//! every model Download also fetches it once (`download`), pinned to the
//! v4.0 release and checked against its SHA-256. Until it is there, Skip
//! Silence trims by loudness (`silence::decide`). It runs on CPU, one 32 ms
//! frame at a time, about 0.1 ms per frame, so a minute of audio takes a
//! fraction of a second after the take.
//!
//! The rules are VocaMac's `SpeechActivityTrimmer`: stretches shorter than
//! 0.25 s are dropped, pauses shorter than 0.3 s stay inside speech, each
//! stretch is padded by 0.2 s, and a recording whose peak probability stays
//! under 0.15 has no speech. Speech starts at a probability of 0.3, Handy's
//! threshold: at VocaMac's 0.5, a first word under fan noise ("Hello, can
//! you…") fell below it and was cut.

use std::ops::Range;
use std::path::Path;
use std::sync::Mutex;

use ort::session::Session;
use ort::value::Tensor;

use crate::silence::Decision;

/// The detector's file in the models folder.
pub const FILE: &str = "silero_vad_v4.onnx";
/// snakers4/silero-vad at the v4.0 tag (the file Handy also ships).
const URL: &str = "https://github.com/snakers4/silero-vad/raw/v4.0/files/silero_vad.onnx";
const SHA256: &str = "a35ebf52fd3ce5f1469b2a36158dba761bc47b973ea3382b3186ca15b1f5af28";
const SAMPLE_RATE: usize = 16_000;
/// 32 ms, the frame Silero v4 is trained on at 16 kHz.
const FRAME: usize = 512;
const SPEECH_THRESHOLD: f32 = 0.3;
const MINIMUM_SPEECH: f32 = 0.25;
const MINIMUM_SILENCE: f32 = 0.3;
const SPEECH_PADDING: f32 = 0.2;
const NO_SPEECH_PROBABILITY: f32 = 0.15;

/// Built on first use and kept; decisions lock it one take at a time.
static SESSION: Mutex<Option<Session>> = Mutex::new(None);

fn seconds(value: f32) -> usize {
    (value * SAMPLE_RATE as f32) as usize
}

/// Fetches the detector into `models` unless it is already there. Best
/// effort: a failure leaves Skip Silence on loudness.
pub async fn download(models: &Path) -> Result<(), String> {
    let destination = models.join(FILE);
    if destination.is_file() {
        return Ok(());
    }
    let bytes = reqwest::get(URL)
        .await
        .and_then(|response| response.error_for_status())
        .map_err(|error| format!("Could not download the voice detector: {error}"))?
        .bytes()
        .await
        .map_err(|error| format!("Voice detector download interrupted: {error}"))?;
    verify(&bytes)?;
    let staging = models.join(format!("{FILE}.part"));
    tokio::fs::write(&staging, &bytes)
        .await
        .map_err(|error| format!("Could not save the voice detector: {error}"))?;
    tokio::fs::rename(&staging, &destination)
        .await
        .map_err(|error| format!("Could not save the voice detector: {error}"))
}

/// The downloaded bytes must be the pinned release.
fn verify(bytes: &[u8]) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    if hex == SHA256 {
        Ok(())
    } else {
        Err("The voice detector download did not match its checksum.".into())
    }
}

/// What to decode from 16 kHz mono `samples`, or an error when the detector
/// is not downloaded yet or cannot run (the caller trims by loudness).
pub fn decide(models: &Path, samples: &[f32]) -> Result<Decision, String> {
    if samples.is_empty() {
        return Ok(Decision::NoSpeech);
    }
    let probabilities = probabilities(&models.join(FILE), samples)?;
    let peak = probabilities.iter().copied().fold(0.0_f32, f32::max);
    let speech = speech_ranges(&probabilities, samples.len());
    Ok(crate::silence::plan_detected(speech, peak, NO_SPEECH_PROBABILITY, samples.len()))
}

/// Speech probability for each 32 ms frame (the last one zero-padded).
fn probabilities(model: &Path, samples: &[f32]) -> Result<Vec<f32>, String> {
    let mut guard = SESSION.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.is_none() {
        if !model.is_file() {
            return Err("The voice detector is not downloaded yet".into());
        }
        let failed = |error: &dyn std::fmt::Display| format!("Could not load the voice detector: {error}");
        let session = Session::builder()
            .map_err(|error| failed(&error))?
            .with_intra_threads(1)
            .map_err(|error| failed(&error))?
            .commit_from_file(model)
            .map_err(|error| failed(&error))?;
        *guard = Some(session);
    }
    let session = guard.as_mut().ok_or("Voice detector missing after load")?;
    // Recurrent state starts fresh for every take.
    let mut h = vec![0.0_f32; 2 * 64];
    let mut c = vec![0.0_f32; 2 * 64];
    let mut output = Vec::with_capacity(samples.len().div_ceil(FRAME));
    for chunk in samples.chunks(FRAME) {
        let mut frame = chunk.to_vec();
        frame.resize(FRAME, 0.0);
        let tensor = |shape: Vec<i64>, data: Vec<f32>| {
            Tensor::from_array((shape, data.into_boxed_slice()))
                .map_err(|error| format!("Voice detector input: {error}"))
        };
        let rate = Tensor::from_array((vec![1_i64], vec![SAMPLE_RATE as i64].into_boxed_slice()))
            .map_err(|error| format!("Voice detector input: {error}"))?;
        let outputs = session
            .run(ort::inputs![
                "input" => tensor(vec![1, FRAME as i64], frame)?,
                "sr" => rate,
                "h" => tensor(vec![2, 1, 64], std::mem::take(&mut h))?,
                "c" => tensor(vec![2, 1, 64], std::mem::take(&mut c))?,
            ])
            .map_err(|error| format!("Voice detector failed: {error}"))?;
        let read = |name: &str| {
            outputs[name]
                .try_extract_tensor::<f32>()
                .map(|(_, data)| data.to_vec())
                .map_err(|error| format!("Voice detector output {name}: {error}"))
        };
        output.push(read("output")?.first().copied().unwrap_or(0.0));
        h = read("hn")?;
        c = read("cn")?;
    }
    Ok(output)
}

/// Padded speech ranges in samples from per-frame probabilities.
fn speech_ranges(probabilities: &[f32], total: usize) -> Vec<Range<usize>> {
    let mut stretches: Vec<Range<usize>> = Vec::new();
    for (index, probability) in probabilities.iter().enumerate() {
        if *probability < SPEECH_THRESHOLD {
            continue;
        }
        match stretches.last_mut() {
            // Measured in samples: 9 frames are 288 ms, still a short pause.
            Some(last) if (index - last.end) * FRAME < seconds(MINIMUM_SILENCE) => {
                last.end = index + 1
            }
            _ => stretches.push(index..index + 1),
        }
    }
    let minimum_frames = seconds(MINIMUM_SPEECH).div_ceil(FRAME);
    let padding = seconds(SPEECH_PADDING);
    stretches
        .into_iter()
        .filter(|stretch| stretch.len() >= minimum_frames)
        .map(|stretch| {
            (stretch.start * FRAME).saturating_sub(padding)..(stretch.end * FRAME + padding).min(total)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(pattern: &str) -> Vec<f32> {
        pattern.chars().map(|c| if c == '#' { 0.9 } else { 0.05 }).collect()
    }

    #[test]
    fn short_pauses_stay_inside_speech_and_blips_go() {
        // 10 speech frames, a 4-frame pause (128 ms < 0.3 s), 10 more,
        // then a 2-frame blip (64 ms < 0.25 s) after a long pause.
        let probs = frames("..........##########....##########....................##..........");
        let total = probs.len() * FRAME;
        let ranges = speech_ranges(&probs, total);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].start, 10 * FRAME - seconds(0.2));
        assert_eq!(ranges[0].end, 34 * FRAME + seconds(0.2));
    }

    #[test]
    fn long_pauses_split_speech() {
        let probs = frames("##########..............##########");
        assert_eq!(speech_ranges(&probs, probs.len() * FRAME).len(), 2);
    }

    #[test]
    fn a_pause_just_under_the_minimum_stays_inside_speech() {
        // 9 frames = 288 ms < 0.3 s: one stretch, not two.
        let probs = frames("##########.........##########");
        assert_eq!(speech_ranges(&probs, probs.len() * FRAME).len(), 1);
        // 10 frames = 320 ms: two.
        let probs = frames("##########..........##########");
        assert_eq!(speech_ranges(&probs, probs.len() * FRAME).len(), 2);
    }

    #[test]
    fn only_the_pinned_release_is_accepted() {
        assert!(verify(b"not the model").is_err());
    }

    #[test]
    fn without_the_detector_the_caller_falls_back() {
        let empty = tempfile::tempdir().unwrap();
        assert!(decide(empty.path(), &vec![0.1; SAMPLE_RATE]).is_err());
    }

    /// Runs the real model: `VOCAWIN_TEST_MODELS=<dir with silero_vad_v4.onnx>
    /// cargo test -- --ignored`.
    fn test_models() -> std::path::PathBuf {
        std::env::var("VOCAWIN_TEST_MODELS")
            .expect("set VOCAWIN_TEST_MODELS")
            .into()
    }

    #[test]
    #[ignore]
    fn digital_silence_is_no_speech() {
        assert_eq!(decide(&test_models(), &vec![0.0; SAMPLE_RATE * 2]).unwrap(), Decision::NoSpeech);
    }

    #[test]
    #[ignore]
    fn a_voice_like_signal_is_not_thrown_away() {
        // Silero is not fooled into silence by a loud harmonic signal; the
        // take is decoded (kept or trimmed), never skipped.
        let voiced: Vec<f32> = (0..SAMPLE_RATE * 2)
            .map(|i| {
                let t = i as f32 / SAMPLE_RATE as f32;
                let envelope = (std::f32::consts::PI * 4.0 * t).sin().abs();
                envelope
                    * [150.0_f32, 300.0, 450.0, 600.0, 900.0]
                        .iter()
                        .map(|hz| (2.0 * std::f32::consts::PI * hz * t).sin() * 0.1)
                        .sum::<f32>()
            })
            .collect();
        assert_ne!(decide(&test_models(), &voiced).unwrap(), Decision::NoSpeech);
    }
}
