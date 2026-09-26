//! Finds speech with Silero VAD, as VocaMac does, for Skip Silence.
//!
//! The model (Silero VAD v4, MIT, 1.8 MB) is compiled into the app: it is
//! not a speech model, and trimming must work offline from the first take.
//! Handy bundles the same file. It runs on CPU, one 32 ms frame at a time,
//! about 0.1 ms per frame, so a minute of audio takes a fraction of a second
//! after the take.
//!
//! The rules are VocaMac's `SpeechActivityTrimmer`: stretches shorter than
//! 0.25 s are dropped, pauses shorter than 0.3 s stay inside speech, each
//! stretch is padded by 0.2 s, and a recording whose peak probability stays
//! under 0.15 has no speech. Speech starts at a probability of 0.3, Handy's
//! threshold: at VocaMac's 0.5, a first word under fan noise ("Hello, can
//! you…") fell below it and was cut.

use std::ops::Range;
use std::sync::Mutex;

use ort::session::Session;
use ort::value::Tensor;

use crate::silence::Decision;

const MODEL: &[u8] = include_bytes!("../resources/silero_vad_v4.onnx");
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

/// What to decode from 16 kHz mono `samples`.
pub fn decide(samples: &[f32]) -> Result<Decision, String> {
    if samples.is_empty() {
        return Ok(Decision::NoSpeech);
    }
    let probabilities = probabilities(samples)?;
    let peak = probabilities.iter().copied().fold(0.0_f32, f32::max);
    let speech = speech_ranges(&probabilities, samples.len());
    Ok(crate::silence::plan_detected(speech, peak, NO_SPEECH_PROBABILITY, samples.len()))
}

/// Speech probability for each 32 ms frame (the last one zero-padded).
fn probabilities(samples: &[f32]) -> Result<Vec<f32>, String> {
    let mut guard = SESSION.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.is_none() {
        let failed = |error: &dyn std::fmt::Display| format!("Could not load the voice detector: {error}");
        let session = Session::builder()
            .map_err(|error| failed(&error))?
            .with_intra_threads(1)
            .map_err(|error| failed(&error))?
            .commit_from_memory(MODEL)
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
    let gap_frames = seconds(MINIMUM_SILENCE) / FRAME;
    let mut stretches: Vec<Range<usize>> = Vec::new();
    for (index, probability) in probabilities.iter().enumerate() {
        if *probability < SPEECH_THRESHOLD {
            continue;
        }
        match stretches.last_mut() {
            Some(last) if index - last.end < gap_frames => last.end = index + 1,
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
    fn digital_silence_is_no_speech() {
        assert_eq!(decide(&vec![0.0; SAMPLE_RATE * 2]).unwrap(), Decision::NoSpeech);
    }

    #[test]
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
        assert_ne!(decide(&voiced).unwrap(), Decision::NoSpeech);
    }
}
