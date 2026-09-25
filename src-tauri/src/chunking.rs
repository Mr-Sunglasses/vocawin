//! Splits a long recording into windows an encoder-decoder model can decode.
//!
//! Canary and Moonshine decode a whole take in one pass. Past about half a
//! minute of real speech they lose text without an error: Canary skips whole
//! sentences and Moonshine repeats a phrase until its token budget runs out.
//! Those models get the take in windows of at most `WINDOW_SECONDS` plus
//! `SEARCH_SECONDS`, cut in the quietest stretch near each even split, and
//! the window texts are joined. GigaAM is split the same way because its
//! encoder rejects very long takes. Parakeet and SenseVoice decode takes whole.
//!
//! Windows cover every sample. Deciding up front which audio is silence
//! would sometimes drop quiet speech, so nothing is left out here; a window
//! that decodes to nothing is decoded again from `speech_starts` instead.

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
/// `speech_starts` measures a window's opening level over this long, and a
/// sound only starts a retry after at least this long of pause.
const OPENING_SECONDS: f32 = 0.5;
/// Sound is this many times the opening's RMS (about 10 dB up).
const SOUND_OVER_OPENING: f32 = 3.0;
/// Lowest RMS that counts as sound, `silence.rs`'s minimum.
const MINIMUM_SOUND_RMS: f32 = 0.004;
/// Most retry points `speech_starts` offers for one window.
const MAXIMUM_STARTS: usize = 3;

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

/// Where to retry a window that opens on a pause, earliest first: just
/// before each sound (`LEAD_SECONDS` early) that follows at least
/// `OPENING_SECONDS` of pause, at most `MAXIMUM_STARTS` of them.
///
/// The pause is measured from the window's own opening, so a noisy room
/// counts as a pause while speech is clearly louder. A sound of any length
/// counts, so a short word is never skipped; if the sound was a click and
/// the retry from it still decodes to nothing, the next start is past it.
/// Empty when the window opens on sound or has none.
pub fn speech_starts(samples: &[f32]) -> Vec<usize> {
    let rms: Vec<f32> = samples
        .chunks(FRAME)
        .map(|frame| (frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32).sqrt())
        .collect();
    let opening_frames = seconds(OPENING_SECONDS) / FRAME;
    if rms.len() <= opening_frames {
        return Vec::new();
    }
    let mut opening = rms[..opening_frames].to_vec();
    opening.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let threshold = (opening[opening.len() / 2] * SOUND_OVER_OPENING).max(MINIMUM_SOUND_RMS);
    let mut starts = Vec::new();
    let mut quiet_since = 0;
    for (index, value) in rms.iter().enumerate() {
        if *value < threshold {
            continue;
        }
        if index - quiet_since >= opening_frames {
            starts.push((index * FRAME).saturating_sub(seconds(LEAD_SECONDS)));
            if starts.len() == MAXIMUM_STARTS {
                break;
            }
        }
        quiet_since = index + 1;
    }
    starts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(seconds_long: f32) -> Vec<f32> {
        (0..seconds(seconds_long))
            .map(|i| (i as f32 * 0.07).sin() * 0.3)
            .collect()
    }

    fn noise(seconds_long: f32, amplitude: f32) -> Vec<f32> {
        (0..seconds(seconds_long))
            .map(|i| if i % 2 == 0 { amplitude } else { -amplitude })
            .collect()
    }

    fn click(samples: &mut [f32], at_seconds: f32) {
        for sample in &mut samples[seconds(at_seconds)..seconds(at_seconds) + FRAME] {
            *sample = 1.0;
        }
    }

    fn assert_covers(ranges: &[Range<usize>], total: usize) {
        assert_eq!(ranges.first().map(|r| r.start), Some(0));
        assert_eq!(ranges.last().map(|r| r.end), Some(total));
        for pair in ranges.windows(2) {
            assert_eq!(pair[0].end, pair[1].start);
        }
    }

    fn assert_starts_before(start: usize, sound_seconds: f32) {
        let sound = seconds(sound_seconds);
        assert!(
            start <= sound && start + seconds(LEAD_SECONDS + 0.05) >= sound,
            "start {start} for sound at {sound_seconds}s"
        );
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

    #[test]
    fn a_noisy_pause_past_the_search_range_stays_in_the_windows() {
        // An 8 s pause of room noise only 20 dB under the speech covers the
        // whole 17.5-22.5 s search range. No audio is left out: the next
        // window opens on the rest of the pause, and `speech_starts` finds
        // where its speech begins.
        let mut samples = tone(16.0);
        samples.extend(noise(8.0, 0.02));
        samples.extend(tone(16.0));
        let ranges = windows(&samples);
        assert_covers(&ranges, samples.len());
        let starts = speech_starts(&samples[ranges[1].clone()]);
        let resumes = 24.0 - ranges[1].start as f32 / SAMPLE_RATE as f32;
        assert_eq!(starts.len(), 1, "{starts:?}");
        assert_starts_before(starts[0], resumes);
    }

    #[test]
    fn quiet_speech_past_a_loud_click_is_never_left_out() {
        let mut samples = tone(17.0);
        samples.extend(
            (0..seconds(9.0))
                .map(|i| (i as f32 * 0.07).sin() * 0.02)
                .collect::<Vec<_>>(),
        );
        samples.extend(tone(14.0));
        click(&mut samples, 5.0);
        assert_covers(&windows(&samples), samples.len());
    }

    #[test]
    fn speech_starts_find_a_real_lead_in() {
        for (level, lead) in [(0.0, 1.5), (0.004, 1.5), (0.02, 2.0), (0.004, 0.6)] {
            let mut samples = noise(lead, level);
            samples.extend(tone(3.0));
            let starts = speech_starts(&samples);
            assert_eq!(starts.len(), 1, "level {level}, lead {lead}: {starts:?}");
            assert_starts_before(starts[0], lead);
        }
    }

    #[test]
    fn a_short_word_in_the_pause_is_tried_first() {
        // A 60 ms word at 1.0 s, then 1.5 s more pause before speech.
        let mut samples = noise(1.0, 0.004);
        samples.extend(tone(0.06));
        samples.extend(noise(1.5, 0.004));
        samples.extend(tone(3.0));
        let starts = speech_starts(&samples);
        assert_eq!(starts.len(), 2, "{starts:?}");
        assert_starts_before(starts[0], 1.0);
        assert_starts_before(starts[1], 2.56);
    }

    #[test]
    fn a_click_in_the_pause_does_not_block_the_speech() {
        // A click 0.2 s in is too close to the start to retry from; one at
        // 0.9 s is tried first, then the speech at 1.5 s after it.
        for level in [0.0, 0.004, 0.02] {
            let mut samples = noise(1.5, level);
            click(&mut samples, 0.2);
            click(&mut samples, 0.9);
            samples.extend(tone(3.0));
            let starts = speech_starts(&samples);
            assert_eq!(starts.len(), 2, "level {level}: {starts:?}");
            assert_starts_before(starts[0], 0.9);
            assert_starts_before(starts[1], 1.5);
        }
    }

    #[test]
    fn speech_starts_leave_speech_openings_alone() {
        // Speech right away, a short breath first, quiet speech that gets
        // louder, or no sound at all: nothing to retry from.
        assert!(speech_starts(&tone(3.0)).is_empty());
        let mut short = noise(0.3, 0.004);
        short.extend(tone(3.0));
        assert!(speech_starts(&short).is_empty());
        let mut rising: Vec<f32> = (0..seconds(2.0))
            .map(|i| (i as f32 * 0.07).sin() * 0.15)
            .collect();
        rising.extend(tone(3.0));
        assert!(speech_starts(&rising).is_empty());
        assert!(speech_starts(&noise(3.0, 0.004)).is_empty());
        assert!(speech_starts(&[]).is_empty());
    }

    #[test]
    fn no_more_than_three_starts() {
        let mut samples = Vec::new();
        for _ in 0..6 {
            samples.extend(noise(0.8, 0.004));
            samples.extend(tone(0.06));
        }
        assert_eq!(speech_starts(&samples).len(), MAXIMUM_STARTS);
    }
}
