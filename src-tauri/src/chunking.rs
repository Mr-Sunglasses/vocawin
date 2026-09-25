//! Splits a long recording into windows an encoder-decoder model can decode.
//!
//! Canary and Moonshine decode a whole take in one pass. Past about half a
//! minute of real speech they lose text without an error: Canary skips whole
//! sentences and Moonshine repeats a phrase until its token budget runs out.
//! Those models get the take in windows of at most `WINDOW_SECONDS` plus
//! `SEARCH_SECONDS`, cut in the quietest stretch near each even split, and
//! the window texts are joined. CTC and transducer models (Parakeet,
//! SenseVoice, GigaAM) decode long takes whole and are not split.

use std::ops::Range;

const SAMPLE_RATE: usize = 16_000;
/// 30 ms frames.
const FRAME: usize = 480;
/// Takes up to this long are decoded whole; longer ones are split evenly
/// into windows no longer than this before each cut moves to a pause.
const WINDOW_SECONDS: f32 = 20.0;
/// How far a cut may move from its even split to land in a pause.
const SEARCH_SECONDS: f32 = 2.5;
/// A cut goes in the quietest run of this many frames (150 ms), so a stop
/// consonant inside a word does not pass for a pause.
const QUIET_FRAMES: usize = 5;
/// Frame energy (sum of squares) that always counts as quiet: 0.004 RMS,
/// `silence.rs`'s lowest speech threshold.
const QUIET_FLOOR: f32 = 0.004 * 0.004 * FRAME as f32;
/// Silence a window may open with. Moonshine returns nothing for a window
/// that starts on a second or more of silence, so a cut goes near the end
/// of its pause and the rest of the pause trails the previous window.
const LEAD_SECONDS: f32 = 0.2;

fn seconds(value: f32) -> usize {
    (value * SAMPLE_RATE as f32) as usize
}

/// Sample ranges to decode in order, covering all of `samples` without gaps.
pub fn windows(samples: &[f32]) -> Vec<Range<usize>> {
    let total = samples.len();
    let window = seconds(WINDOW_SECONDS);
    if total <= window {
        return vec![0..total];
    }
    let count = total.div_ceil(window);
    let search = seconds(SEARCH_SECONDS);
    let mut ranges = Vec::with_capacity(count);
    let mut start = 0;
    for index in 1..count {
        let even = total * index / count;
        let cut = cut_point(
            samples,
            even.saturating_sub(search),
            (even + search).min(total),
        );
        ranges.push(start..cut);
        start = cut;
    }
    ranges.push(start..total);
    ranges
}

/// Where to cut inside `lower..upper`: the pause holding the quietest
/// `QUIET_FRAMES` run, `LEAD_SECONDS` before the pause ends.
fn cut_point(samples: &[f32], lower: usize, upper: usize) -> usize {
    let energies: Vec<f32> = samples[lower..upper]
        .chunks_exact(FRAME)
        .map(|frame| frame.iter().map(|s| s * s).sum::<f32>())
        .collect();
    if energies.len() < QUIET_FRAMES {
        return (lower + upper) / 2;
    }
    let mut best = 0;
    let mut best_energy = f32::MAX;
    for (index, run) in energies.windows(QUIET_FRAMES).enumerate() {
        let energy: f32 = run.iter().sum();
        if energy < best_energy {
            best_energy = energy;
            best = index;
        }
    }
    let quiet = (best_energy / QUIET_FRAMES as f32 * 4.0).max(QUIET_FLOOR);
    let mut pause_end = best + QUIET_FRAMES;
    while pause_end < energies.len() && energies[pause_end] <= quiet {
        pause_end += 1;
    }
    let middle = lower + best * FRAME + QUIET_FRAMES * FRAME / 2;
    (lower + pause_end * FRAME)
        .saturating_sub(seconds(LEAD_SECONDS))
        .max(middle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(seconds_long: f32) -> Vec<f32> {
        (0..seconds(seconds_long))
            .map(|i| (i as f32 * 0.07).sin() * 0.3)
            .collect()
    }

    fn assert_covers(ranges: &[Range<usize>], total: usize) {
        assert_eq!(ranges.first().map(|r| r.start), Some(0));
        assert_eq!(ranges.last().map(|r| r.end), Some(total));
        for pair in ranges.windows(2) {
            assert_eq!(pair[0].end, pair[1].start);
        }
    }

    #[test]
    fn short_takes_are_decoded_whole() {
        let samples = tone(WINDOW_SECONDS);
        assert_eq!(windows(&samples), vec![0..samples.len()]);
        assert_eq!(windows(&[]), vec![0..0]);
    }

    #[test]
    fn long_takes_split_into_bounded_windows() {
        for length in [20.5, 31.0, 45.0, 60.0, 299.0] {
            let samples = tone(length);
            let ranges = windows(&samples);
            assert_covers(&ranges, samples.len());
            assert_eq!(
                ranges.len(),
                samples.len().div_ceil(seconds(WINDOW_SECONDS))
            );
            for range in &ranges {
                assert!(
                    range.len() <= seconds(WINDOW_SECONDS + 2.0 * SEARCH_SECONDS),
                    "{length}s take has a {}-sample window",
                    range.len()
                );
                assert!(
                    range.len() >= seconds(WINDOW_SECONDS / 2.0 - 2.0 * SEARCH_SECONDS),
                    "{length}s take has a {}-sample window",
                    range.len()
                );
            }
        }
    }

    #[test]
    fn cuts_land_in_a_nearby_pause() {
        // 40 s of speech splits evenly at 20 s; a pause at 18.5 s is in reach.
        let mut samples = tone(18.3);
        samples.extend(vec![0.0; seconds(0.4)]);
        samples.extend(tone(21.3));
        let ranges = windows(&samples);
        assert_eq!(ranges.len(), 2);
        let cut = ranges[0].end;
        assert!(
            cut > seconds(18.3) && cut < seconds(18.7),
            "cut at {:.2}s",
            cut as f32 / SAMPLE_RATE as f32
        );
    }

    #[test]
    fn a_long_pause_trails_the_window_before_it() {
        // Speech resumes at 19 s; the next window opens just before it.
        let mut samples = tone(17.0);
        samples.extend(vec![0.0; seconds(2.0)]);
        samples.extend(tone(19.0));
        let ranges = windows(&samples);
        assert_eq!(ranges.len(), 2);
        let cut = ranges[0].end;
        assert!(
            cut >= seconds(19.0 - LEAD_SECONDS - 0.05) && cut <= seconds(19.0),
            "cut at {:.2}s",
            cut as f32 / SAMPLE_RATE as f32
        );
    }
}
