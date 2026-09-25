//! Usage stats, kept only on this PC (`stats.json`), like VocaMac's Stats
//! page: totals, speaking pace, streaks, and time saved over typing.
//!
//! Counted when a dictation is typed into another app. Test dictations and
//! history retries do not count. Clearing history leaves stats alone; Reset
//! clears them.

use chrono::{Datelike, Duration, Local, NaiveDate};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// Typing speed used for "time saved", the figure VocaMac uses.
const TYPING_WORDS_PER_MINUTE: f64 = 40.0;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DayTotals {
    pub dictations: u64,
    pub words: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    pub dictations: u64,
    pub words: u64,
    pub characters: u64,
    /// Milliseconds of speech that produced typed text.
    pub audio_ms: u64,
    /// Local calendar day (`YYYY-MM-DD`) → totals.
    #[serde(default)]
    pub days: BTreeMap<String, DayTotals>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DayPoint {
    pub day: String,
    pub words: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub dictations: u64,
    pub words: u64,
    pub characters: u64,
    pub audio_minutes: f64,
    pub words_per_minute: u64,
    pub time_saved_minutes: u64,
    pub words_today: u64,
    pub words_this_week: u64,
    pub current_streak: u32,
    pub longest_streak: u32,
    pub active_days: u32,
    /// The last 14 days, oldest first, for the chart.
    pub recent: Vec<DayPoint>,
}

pub fn load(path: &Path) -> Stats {
    fs::read_to_string(path)
        .ok()
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default()
}

fn save(path: &Path, stats: &Stats) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("Could not save stats: {error}"))?;
    }
    let serialized = serde_json::to_vec_pretty(stats)
        .map_err(|error| format!("Could not save stats: {error}"))?;
    fs::write(path, serialized).map_err(|error| format!("Could not save stats: {error}"))
}

pub fn word_count(text: &str) -> u64 {
    text.split_whitespace()
        .filter(|word| word.chars().any(char::is_alphanumeric))
        .count() as u64
}

fn day_key(date: NaiveDate) -> String {
    date.format("%Y-%m-%d").to_string()
}

pub fn add(stats: &mut Stats, text: &str, audio_ms: u64, today: NaiveDate) {
    let words = word_count(text);
    stats.dictations += 1;
    stats.words += words;
    stats.characters += text.trim().chars().count() as u64;
    stats.audio_ms += audio_ms;
    let day = stats.days.entry(day_key(today)).or_default();
    day.dictations += 1;
    day.words += words;
}

pub fn record(path: &Path, text: &str, audio_ms: u64) -> Result<(), String> {
    let mut stats = load(path);
    add(&mut stats, text, audio_ms, Local::now().date_naive());
    save(path, &stats)
}

pub fn reset(path: &Path) -> Result<(), String> {
    if path.exists() {
        fs::remove_file(path).map_err(|error| format!("Could not reset stats: {error}"))?;
    }
    Ok(())
}

pub fn summarize(stats: &Stats, today: NaiveDate) -> Summary {
    let active: Vec<NaiveDate> = stats
        .days
        .iter()
        .filter(|(_, totals)| totals.dictations > 0)
        .filter_map(|(day, _)| NaiveDate::parse_from_str(day, "%Y-%m-%d").ok())
        .collect();

    // Longest run of consecutive days.
    let mut longest = 0u32;
    let mut run = 0u32;
    let mut previous: Option<NaiveDate> = None;
    for day in &active {
        run = match previous {
            Some(before) if *day - before == Duration::days(1) => run + 1,
            _ => 1,
        };
        longest = longest.max(run);
        previous = Some(*day);
    }

    // Current streak: ends today, or yesterday when today has none yet.
    let has = |day: NaiveDate| {
        stats
            .days
            .get(&day_key(day))
            .is_some_and(|t| t.dictations > 0)
    };
    let mut cursor = if has(today) {
        today
    } else {
        today - Duration::days(1)
    };
    let mut current = 0u32;
    while has(cursor) {
        current += 1;
        cursor -= Duration::days(1);
    }

    let words_on = |day: NaiveDate| stats.days.get(&day_key(day)).map_or(0, |t| t.words);
    let week_start = today - Duration::days(today.weekday().num_days_from_monday() as i64);
    let mut words_this_week = 0;
    let mut day = week_start;
    while day <= today {
        words_this_week += words_on(day);
        day += Duration::days(1);
    }
    let recent = (0..14)
        .rev()
        .map(|back| {
            let day = today - Duration::days(back);
            DayPoint {
                day: day_key(day),
                words: words_on(day),
            }
        })
        .collect();

    let audio_minutes = stats.audio_ms as f64 / 60_000.0;
    let words_per_minute = if audio_minutes >= 0.1 {
        (stats.words as f64 / audio_minutes).round() as u64
    } else {
        0
    };
    let typing_minutes = stats.words as f64 / TYPING_WORDS_PER_MINUTE;
    let time_saved_minutes = (typing_minutes - audio_minutes).max(0.0).round() as u64;

    Summary {
        dictations: stats.dictations,
        words: stats.words,
        characters: stats.characters,
        audio_minutes,
        words_per_minute,
        time_saved_minutes,
        words_today: words_on(today),
        words_this_week,
        current_streak: current,
        longest_streak: longest,
        active_days: active.len() as u32,
        recent,
    }
}

pub fn summary(path: &Path) -> Summary {
    summarize(&load(path), Local::now().date_naive())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn words_count_only_real_words() {
        assert_eq!(word_count("Hello there, friend!"), 3);
        assert_eq!(word_count("  — 🎉 "), 0);
        assert_eq!(word_count("It costs $5"), 3);
    }

    #[test]
    fn totals_pace_and_time_saved() {
        let mut stats = Stats::default();
        let today = date("2026-09-25");
        // 120 words over one minute of speech.
        add(&mut stats, &"word ".repeat(120), 60_000, today);
        let summary = summarize(&stats, today);
        assert_eq!(summary.dictations, 1);
        assert_eq!(summary.words, 120);
        assert_eq!(summary.words_per_minute, 120);
        // Typing 120 words at 40 wpm is 3 minutes; speaking took 1.
        assert_eq!(summary.time_saved_minutes, 2);
        assert_eq!(summary.words_today, 120);
        assert_eq!(summary.recent.len(), 14);
        assert_eq!(summary.recent.last().unwrap().words, 120);
    }

    #[test]
    fn streaks_follow_consecutive_days() {
        let mut stats = Stats::default();
        for day in [
            "2026-09-01",
            "2026-09-02",
            "2026-09-03",
            "2026-09-10",
            "2026-09-23",
            "2026-09-24",
        ] {
            add(&mut stats, "hello world", 1_000, date(day));
        }
        // Today has nothing yet: the streak that ended yesterday still counts.
        let summary = summarize(&stats, date("2026-09-25"));
        assert_eq!(summary.current_streak, 2);
        assert_eq!(summary.longest_streak, 3);
        assert_eq!(summary.active_days, 6);
        // A gap of a day ends it.
        assert_eq!(summarize(&stats, date("2026-09-26")).current_streak, 0);
    }

    #[test]
    fn week_starts_on_monday() {
        let mut stats = Stats::default();
        add(&mut stats, "one two", 1_000, date("2026-09-20")); // Sunday
        add(&mut stats, "three four five", 1_000, date("2026-09-21")); // Monday
        let summary = summarize(&stats, date("2026-09-25"));
        assert_eq!(summary.words_this_week, 3);
    }
}
