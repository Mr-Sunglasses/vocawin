//! Removes silence from a recording before it reaches the speech model
//! (VocaMac "Skip Silence Before Transcribing", on by default).
//!
//! Leading and trailing silence is dropped and long pauses are shortened, so
//! the model decodes less audio and has no empty stretch to hallucinate over
//! (Whisper's "Thank you." on a silent clip). A recording with no signal at
//! all is never decoded.
//!
//! VocaMac finds speech with Silero VAD. VocaWin has no VAD model on disk, so
//! this uses frame energy against the recording's own noise floor. The
//! decision and assembly rules are VocaMac's: pad speech by 0.2 s, keep at
//! most 0.4 s of each pause, and leave the audio alone when trimming would
//! save less than 10 %, or when it is unsure.

use std::ops::Range;

const SAMPLE_RATE: usize = 16_000;
/// 30 ms frames.
const FRAME: usize = 480;
const SPEECH_PADDING: f32 = 0.2;
const MAXIMUM_PAUSE: f32 = 0.4;
const MINIMUM_SAVINGS: f32 = 0.1;
/// Pauses shorter than this stay inside one stretch of speech.
const MINIMUM_SILENCE: f32 = 0.3;
/// Shorter bursts (a click, a breath) are not speech.
const MINIMUM_SPEECH: f32 = 0.12;
/// Below this peak the microphone heard nothing (about -46 dBFS).
const NO_SIGNAL_PEAK: f32 = 0.005;
/// Speech must be this far above the noise floor.
const FLOOR_RATIO: f32 = 3.0;
const MINIMUM_THRESHOLD: f32 = 0.004;
/// Speech frames need at least this share of the loudest frame's energy.
const LOUDEST_RATIO: f32 = 0.08;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Decode these sample ranges, in order, joined by short pauses.
    Trim(Vec<Range<usize>>),
    /// Decode the recording as recorded.
    Keep,
    /// Nothing was said; skip the decode.
    NoSpeech,
}

fn seconds(value: f32) -> usize {
    (value * SAMPLE_RATE as f32) as usize
}

/// Decide what to decode from 16 kHz mono samples.
pub fn decide(samples: &[f32]) -> Decision {
    if samples.is_empty() {
        return Decision::NoSpeech;
    }
    let peak = samples.iter().fold(0.0_f32, |max, s| max.max(s.abs()));
    if peak < NO_SIGNAL_PEAK {
        return Decision::NoSpeech;
    }
    let energies: Vec<f32> = samples
        .chunks(FRAME)
        .map(|frame| (frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32).sqrt())
        .collect();
    let mut sorted = energies.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let floor = sorted[sorted.len() / 20];
    let loudest = sorted[sorted.len() - 1];
    // Never demand more than a fraction of the loudest frame, so a take with
    // little silence to measure does not lose its quieter words.
    let threshold = (floor * FLOOR_RATIO)
        .max(MINIMUM_THRESHOLD)
        .min(loudest * LOUDEST_RATIO);

    // Frames over the threshold, merged across short pauses.
    let mut stretches: Vec<Range<usize>> = Vec::new();
    let gap_frames = seconds(MINIMUM_SILENCE) / FRAME;
    for (index, energy) in energies.iter().enumerate() {
        if *energy < threshold {
            continue;
        }
        match stretches.last_mut() {
            Some(last) if index - last.end <= gap_frames => last.end = index + 1,
            _ => stretches.push(index..index + 1),
        }
    }
    let minimum_frames = (seconds(MINIMUM_SPEECH) / FRAME).max(1);
    let padding = seconds(SPEECH_PADDING);
    let speech: Vec<Range<usize>> = stretches
        .into_iter()
        .filter(|stretch| stretch.len() >= minimum_frames)
        .map(|stretch| {
            let start = (stretch.start * FRAME).saturating_sub(padding);
            let end = (stretch.end * FRAME + padding).min(samples.len());
            start..end
        })
        .collect();
    plan(speech, samples.len())
}

/// VocaMac's decision from padded speech ranges.
fn plan(speech: Vec<Range<usize>>, total: usize) -> Decision {
    let ranges: Vec<Range<usize>> = speech
        .into_iter()
        .map(|r| r.start.min(total)..r.end.min(total))
        .filter(|r| !r.is_empty())
        .collect();
    if ranges.is_empty() {
        // Quiet or whispered speech: let the model decide.
        return Decision::Keep;
    }
    let kept = assembled_length(&ranges);
    if ((total - kept.min(total)) as f32) < total as f32 * MINIMUM_SAVINGS {
        return Decision::Keep;
    }
    Decision::Trim(ranges)
}

/// Join speech ranges, keeping at most `MAXIMUM_PAUSE` of each gap.
pub fn apply(ranges: &[Range<usize>], samples: &[f32]) -> Vec<f32> {
    let pause = seconds(MAXIMUM_PAUSE);
    let mut output = Vec::with_capacity(assembled_length(ranges));
    let mut previous_end: Option<usize> = None;
    for range in ranges {
        let lower = range.start.max(previous_end.unwrap_or(0));
        if lower >= range.end || range.end > samples.len() {
            continue;
        }
        if let Some(previous) = previous_end {
            if lower > previous {
                let gap = lower - previous;
                if gap <= pause {
                    output.extend_from_slice(&samples[previous..lower]);
                } else {
                    // Keep the edges: the tail of one word, the breath before the next.
                    let half = pause / 2;
                    output.extend_from_slice(&samples[previous..previous + half]);
                    output.extend_from_slice(&samples[lower - (pause - half)..lower]);
                }
            }
        }
        output.extend_from_slice(&samples[lower..range.end]);
        previous_end = Some(range.end);
    }
    output
}

fn assembled_length(ranges: &[Range<usize>]) -> usize {
    let pause = seconds(MAXIMUM_PAUSE);
    let mut total = 0;
    let mut previous_end: Option<usize> = None;
    for range in ranges {
        let lower = range.start.max(previous_end.unwrap_or(0));
        if lower >= range.end {
            continue;
        }
        if let Some(previous) = previous_end {
            if lower > previous {
                total += (lower - previous).min(pause);
            }
        }
        total += range.end - lower;
        previous_end = Some(range.end);
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(seconds_long: f32, amplitude: f32) -> Vec<f32> {
        (0..seconds(seconds_long))
            .map(|i| (i as f32 * 0.07).sin() * amplitude)
            .collect()
    }

    fn silence(seconds_long: f32) -> Vec<f32> {
        (0..seconds(seconds_long))
            .map(|i| if i % 2 == 0 { 0.0008 } else { -0.0008 })
            .collect()
    }

    #[test]
    fn nothing_heard_skips_the_decode() {
        assert_eq!(decide(&silence(2.0)), Decision::NoSpeech);
        assert_eq!(decide(&[]), Decision::NoSpeech);
    }

    #[test]
    fn leading_and_trailing_silence_is_cut() {
        let mut samples = silence(2.0);
        samples.extend(tone(1.0, 0.3));
        samples.extend(silence(2.0));
        let Decision::Trim(ranges) = decide(&samples) else {
            panic!("expected a trim");
        };
        let trimmed = apply(&ranges, &samples);
        // One second of speech plus 0.2 s of padding each side.
        assert!(
            trimmed.len() >= seconds(1.3) && trimmed.len() <= seconds(1.5),
            "{}",
            trimmed.len()
        );
    }

    #[test]
    fn long_pauses_shrink_but_short_ones_stay() {
        let mut samples = tone(1.0, 0.3);
        samples.extend(silence(3.0));
        samples.extend(tone(1.0, 0.3));
        let Decision::Trim(ranges) = decide(&samples) else {
            panic!("expected a trim");
        };
        let trimmed = apply(&ranges, &samples);
        assert!(trimmed.len() < seconds(3.0), "{}", trimmed.len());

        let mut close = tone(1.0, 0.3);
        close.extend(silence(0.2));
        close.extend(tone(1.0, 0.3));
        assert_eq!(decide(&close), Decision::Keep);
    }

    #[test]
    fn assembled_length_matches_apply() {
        let samples = vec![0.1; 64_000];
        let ranges = vec![0..8_000, 40_000..48_000];
        assert_eq!(apply(&ranges, &samples).len(), assembled_length(&ranges));
        assert_eq!(assembled_length(&ranges), 16_000 + seconds(MAXIMUM_PAUSE));
    }
}
