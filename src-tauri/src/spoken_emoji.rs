//! Turns "<descriptor> emoji" into the glyph: "I'm so sad, crying emoji"
//! becomes "I'm so sad, 😭".
//!
//! Port of VocaMac `SpokenEmoji` (from VocaPhone). The phrase table is the
//! same generated `suggestions.tsv` with `spoken-aliases.tsv` laid over it,
//! and `tests/fixtures/spoken-emoji.tsv` is the shared contract.
//!
//! Conservative: a trigger with no recognized descriptor stays as spoken, only
//! a space or hyphen joins a descriptor to its trigger, the longest descriptor
//! wins without splitting a longer name, and talking *about* an emoji ("send a
//! fire emoji") keeps its words.

use regex::Regex;
use std::collections::HashMap;
use std::sync::OnceLock;

const TRIGGER_WORDS: &[&str] = &["emoji", "emojis", "emoticon", "emoticons"];
const PLURAL_TRIGGERS: &[&str] = &["emojis", "emoticons"];
const MAXIMUM_REPEAT: usize = 10;

/// Words the table generator drops when it joins a multi-word name.
const NAME_STOP: &[&str] = &[
    "with", "and", "of", "the", "a", "in", "on", "at", "to", "for", "or",
];

/// `korea` in the table is the DPRK flag, which almost nobody means.
const SPOKEN_BLOCKLIST: &[&str] = &["korea"];

/// Right before a descriptor, these mean the emoji is being talked about.
const REFERENCE_WORDS: &[&str] = &[
    "a",
    "an",
    "the",
    "this",
    "that",
    "these",
    "those",
    "my",
    "your",
    "his",
    "her",
    "its",
    "our",
    "their",
    "any",
    "some",
    "which",
    "what",
    "what's",
    "whats",
    "no",
    "every",
    "each",
    "favorite",
    "favourite",
    "same",
    "i",
    "we",
    "they",
    "he",
    "she",
];

/// Words that are part of emoji names: a match right after one is likely the
/// tail of a longer name the table does not have.
const NAME_WORDS: &[&str] = &[
    "with", "of", "face", "faces", "hand", "hands", "man", "woman", "men", "women", "person",
    "people", "boy", "girl", "baby", "old", "skin", "tone", "red", "orange", "yellow", "green",
    "blue", "purple", "brown", "black", "white", "pink", "grey", "gray", "light", "dark", "medium",
    "broken",
];

const QUANTITY_WORDS: &[&str] = &[
    "one",
    "eleven",
    "twelve",
    "twenty",
    "thirty",
    "forty",
    "fifty",
    "sixty",
    "seventy",
    "eighty",
    "ninety",
    "hundred",
    "hundreds",
    "thousand",
    "thousands",
    "million",
    "millions",
    "dozen",
    "dozens",
    "many",
    "few",
    "several",
];

const SKIN_TONES: &[(&[&str], &str)] = &[
    (&["medium", "light"], "\u{1F3FC}"),
    (&["medium", "dark"], "\u{1F3FE}"),
    (&["light"], "\u{1F3FB}"),
    (&["medium"], "\u{1F3FD}"),
    (&["dark"], "\u{1F3FF}"),
];

/// Unicode 16 `Emoji_Modifier_Base` (emoji-data.txt).
const MODIFIER_BASES: &[(u32, u32)] = &[
    (0x261D, 0x261D),
    (0x26F9, 0x26F9),
    (0x270A, 0x270D),
    (0x1F385, 0x1F385),
    (0x1F3C2, 0x1F3C4),
    (0x1F3C7, 0x1F3C7),
    (0x1F3CA, 0x1F3CC),
    (0x1F442, 0x1F443),
    (0x1F446, 0x1F450),
    (0x1F466, 0x1F478),
    (0x1F47C, 0x1F47C),
    (0x1F481, 0x1F483),
    (0x1F485, 0x1F487),
    (0x1F48F, 0x1F48F),
    (0x1F491, 0x1F491),
    (0x1F4AA, 0x1F4AA),
    (0x1F574, 0x1F575),
    (0x1F57A, 0x1F57A),
    (0x1F590, 0x1F590),
    (0x1F595, 0x1F596),
    (0x1F645, 0x1F647),
    (0x1F64B, 0x1F64F),
    (0x1F6A3, 0x1F6A3),
    (0x1F6B4, 0x1F6B6),
    (0x1F6C0, 0x1F6C0),
    (0x1F6CC, 0x1F6CC),
    (0x1F90C, 0x1F90C),
    (0x1F90F, 0x1F90F),
    (0x1F918, 0x1F91F),
    (0x1F926, 0x1F926),
    (0x1F930, 0x1F939),
    (0x1F93C, 0x1F93E),
    (0x1F977, 0x1F977),
    (0x1F9B5, 0x1F9B6),
    (0x1F9B8, 0x1F9B9),
    (0x1F9BB, 0x1F9BB),
    (0x1F9CD, 0x1F9CF),
    (0x1F9D1, 0x1F9DD),
    (0x1FAC3, 0x1FAC5),
    (0x1FAF0, 0x1FAF8),
];

/// Marks that end or separate a sentence, in any script.
const UNIVERSAL_MARKS: &str = ".!?。！？।۔။។།؟,;:،、၊…";

const MASK_OPEN: char = '\u{F8FE}';
const MASK_CLOSE: char = '\u{F8FF}';

fn contains(list: &[&str], word: &str) -> bool {
    list.contains(&word)
}

fn count_word(word: &str) -> Option<usize> {
    Some(match word {
        "two" => 2,
        "three" => 3,
        "four" => 4,
        "five" => 5,
        "six" => 6,
        "seven" => 7,
        "eight" => 8,
        "nine" => 9,
        "ten" => 10,
        _ => return None,
    })
}

fn parse_int(word: &str) -> Option<usize> {
    if word.is_empty() || !word.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    word.parse().ok()
}

// MARK: - Table

struct EmojiTable {
    glyphs: HashMap<String, String>,
    widest_key: usize,
}

fn parse_table(text: &str, table: &mut HashMap<String, String>, override_existing: bool) {
    for line in text.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((word, glyph)) = line.split_once('\t') else {
            continue;
        };
        let word = word.to_lowercase();
        if word.is_empty() || glyph.is_empty() {
            continue;
        }
        if override_existing || !table.contains_key(&word) {
            table.insert(word, glyph.to_string());
        }
    }
}

fn table() -> &'static EmojiTable {
    static TABLE: OnceLock<EmojiTable> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut glyphs = HashMap::with_capacity(4_000);
        parse_table(
            include_str!("../resources/emoji/suggestions.tsv"),
            &mut glyphs,
            false,
        );
        // Aliases are parsed on their own (first wins), then laid over.
        let mut aliases = HashMap::new();
        parse_table(
            include_str!("../resources/emoji/spoken-aliases.tsv"),
            &mut aliases,
            false,
        );
        glyphs.extend(aliases);
        let widest_key = glyphs.keys().map(|k| k.chars().count()).max().unwrap_or(0);
        EmojiTable { glyphs, widest_key }
    })
}

fn glyph_for_spoken(key: &str) -> Option<&'static str> {
    if contains(SPOKEN_BLOCKLIST, key) || key.chars().count() < 2 {
        return None;
    }
    table().glyphs.get(key).map(String::as_str)
}

// MARK: - Protected spans

/// URLs, email addresses, bare domains, decimals and times, ordinals, and
/// dotted initialisms, masked so no descriptor is taken out of one.
struct ProtectedSpans {
    text: String,
    tokens: Vec<String>,
}

fn protected_expression() -> &'static Regex {
    static EXPRESSION: OnceLock<Regex> = OnceLock::new();
    EXPRESSION.get_or_init(|| {
        let hostname = r"\b(?:[\w-]+\.)+(?:[a-z]{2,24}|[A-Z]{2,24})\b";
        let path = r#"(?:/[^\s]*[^\s.,;:!?"“”'\)\]])?"#;
        let pattern = format!(
            r#"((?i:https?)://[^\s]+[^\s.,;:!?"“”'\)\]]|[\w.+-]+@(?:[\w-]+\.)+[A-Za-z]{{2,}}|{hostname}{path}|\d+(?:[.,:/]\d+)+|\d+(?i:st|nd|rd|th)\b|(?:[A-Za-z]\.){{2,}})"#
        );
        Regex::new(&pattern).expect("protected span pattern")
    })
}

impl ProtectedSpans {
    fn mask(text: &str) -> Self {
        let mut result = String::with_capacity(text.len());
        let mut tokens = Vec::new();
        let mut copied = 0;
        for found in protected_expression().find_iter(text) {
            result.push_str(&text[copied..found.start()]);
            result.push(MASK_OPEN);
            result.push_str(&tokens.len().to_string());
            result.push(MASK_CLOSE);
            tokens.push(found.as_str().to_string());
            copied = found.end();
        }
        result.push_str(&text[copied..]);
        Self {
            text: result,
            tokens,
        }
    }

    fn restore(&self, masked: &str) -> String {
        if self.tokens.is_empty() {
            return masked.to_string();
        }
        let mut result = String::with_capacity(masked.len());
        let mut chars = masked.chars().peekable();
        while let Some(c) = chars.next() {
            if c != MASK_OPEN {
                result.push(c);
                continue;
            }
            let mut digits = String::new();
            while let Some(d) = chars.peek().copied().filter(char::is_ascii_digit) {
                digits.push(d);
                chars.next();
            }
            let index = digits.parse::<usize>().ok();
            if chars.peek() == Some(&MASK_CLOSE) {
                if let Some(token) = index.and_then(|i| self.tokens.get(i)) {
                    chars.next();
                    result.push_str(token);
                    continue;
                }
            }
            result.push(c);
            result.push_str(&digits);
        }
        result
    }
}

// MARK: - Words

#[derive(Clone, Copy)]
struct Word {
    start: usize,
    end: usize,
}

/// `[A-Za-z0-9]+(?:['’][A-Za-z0-9]+)*%?`, skipping a masked span's index.
fn words_in(text: &str) -> Vec<Word> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut words = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].1.is_ascii_alphanumeric() {
            i += 1;
            continue;
        }
        let masked = i > 0 && chars[i - 1].1 == MASK_OPEN;
        let start = chars[i].0;
        let mut j = i;
        loop {
            while j < chars.len() && chars[j].1.is_ascii_alphanumeric() {
                j += 1;
            }
            if j + 1 < chars.len()
                && (chars[j].1 == '\'' || chars[j].1 == '’')
                && chars[j + 1].1.is_ascii_alphanumeric()
            {
                j += 1;
                continue;
            }
            break;
        }
        if j < chars.len() && chars[j].1 == '%' {
            j += 1;
        }
        let end = if j < chars.len() {
            chars[j].0
        } else {
            text.len()
        };
        if !masked {
            words.push(Word { start, end });
        }
        i = j;
    }
    words
}

struct Scan<'a> {
    text: &'a str,
    words: Vec<Word>,
}

impl<'a> Scan<'a> {
    fn lower(&self, index: usize) -> String {
        let word = self.words[index];
        self.text[word.start..word.end].to_lowercase()
    }

    fn gap(&self, index: usize) -> &str {
        &self.text[self.words[index].end..self.words[index + 1].start]
    }

    fn is_joiner(&self, index: usize) -> bool {
        matches!(self.gap(index), " " | "-" | "‑")
    }
}

// MARK: - Conversion

struct Descriptor {
    glyph: String,
    start: usize,
    count: usize,
}

/// Replaces every `<descriptor> emoji` span with its glyph. `insert` writes
/// each glyph run; the pipeline passes one that masks it from formatting.
pub fn glyphs(text: &str, insert: &mut dyn FnMut(String) -> String) -> String {
    let has_j = text.bytes().any(|b| b | 0x20 == b'j');
    if text.is_empty() || !(has_j || text.to_lowercase().contains("emoticon")) {
        return text.to_string();
    }
    let spans = ProtectedSpans::mask(text);
    let scan = Scan {
        text: &spans.text,
        words: words_in(&spans.text),
    };
    if scan.words.is_empty() {
        return text.to_string();
    }

    let mut result = String::new();
    let mut copied = 0usize;
    let mut changed = false;
    let mut previous_was_glyph = false;
    for index in 0..scan.words.len() {
        let trigger = scan.lower(index);
        if !contains(TRIGGER_WORDS, &trigger) {
            continue;
        }
        let repeat_after = count_after(index, &scan);
        let last_word = repeat_after.map(|(_, last)| last).unwrap_or(index);
        let Some(mut found) = descriptor(
            index,
            &scan,
            contains(PLURAL_TRIGGERS, &trigger),
            ends_clause(last_word, &scan),
        ) else {
            continue;
        };
        if found.start < copied {
            continue;
        }
        let mut end = scan.words[index].end;
        if found.count == 1 {
            if let Some((count, last)) = repeat_after {
                found.count = count;
                end = scan.words[last].end;
            }
        }
        let between = &scan.text[copied..found.start];
        if previous_was_glyph {
            result.push_str(&separating(between));
        } else {
            result.push_str(between);
        }
        result.push_str(&insert(found.glyph.repeat(found.count)));
        copied = end;
        changed = true;
        previous_was_glyph = true;
    }
    if !changed {
        return text.to_string();
    }
    result.push_str(&closing(&scan.text[copied..], text));
    spans.restore(&result)
}

/// An emoji ends the sentence, so a lone trailing full stop goes. "!" and
/// "?" stay: they carry meaning the speaker put there.
fn closing(tail: &str, source: &str) -> String {
    let terminator = sentence_terminator(source);
    if tail.trim() == terminator {
        String::new()
    } else {
        tail.to_string()
    }
}

fn sentence_terminator(text: &str) -> &'static str {
    if text.chars().any(|c| "。、！？".contains(c)) {
        return "。";
    }
    if text.chars().any(|c| "،؟".contains(c)) {
        return ".";
    }
    if text.contains('۔') {
        return "۔";
    }
    let danda_script = text.chars().any(|c| {
        let v = c as u32;
        (0x0900..=0x097F).contains(&v)
            || (0x0980..=0x09FF).contains(&v)
            || (0x0A00..=0x0A7F).contains(&v)
    });
    if text.contains('।') || danda_script {
        return "।";
    }
    "."
}

/// Nobody punctuates a run of emoji: "😭, 😭" collapses to "😭 😭".
fn separating(between: &str) -> String {
    if !between.is_empty()
        && between
            .chars()
            .all(|c| c.is_whitespace() || UNIVERSAL_MARKS.contains(c))
    {
        " ".to_string()
    } else {
        between.to_string()
    }
}

fn descriptor(trigger: usize, scan: &Scan, plural: bool, ends_clause: bool) -> Option<Descriptor> {
    // (lowercased word, start offset)
    let mut parts: Vec<(String, usize)> = Vec::new();
    let mut full_length = 0usize;
    let mut index = trigger as isize - 1;
    while index >= 0 && scan.is_joiner(index as usize) {
        let raw = scan.lower(index as usize);
        if contains(TRIGGER_WORDS, &raw) {
            break;
        }
        let length = raw.chars().count();
        if full_length + length > table().widest_key + 24 {
            break;
        }
        parts.insert(0, (raw, scan.words[index as usize].start));
        full_length += length;
        index -= 1;
    }
    if parts.is_empty() {
        return None;
    }
    let names: Vec<String> = parts.iter().map(|(word, _)| word.clone()).collect();

    // "thumbs up dark skin tone emoji": the tone after the name.
    let mut trailing_tone: Option<&'static str> = None;
    let mut part_count = parts.len();
    if let Some((modifier, length)) = skin_tone(part_count, &names) {
        trailing_tone = Some(modifier);
        part_count -= length;
        if part_count == 0 {
            return None;
        }
    }
    let parts = &parts[..part_count];
    let names = &names[..part_count];

    for start in 0..parts.len() {
        let tail = &names[start..];
        let lookup = if let Some(glyph) = glyph_for_spoken(&tail.concat()) {
            Some((glyph, start))
        } else if let Some(first) = tail.iter().position(|w| !contains(NAME_STOP, w)) {
            let filtered: String = tail
                .iter()
                .filter(|w| !contains(NAME_STOP, w))
                .map(String::as_str)
                .collect();
            if tail.iter().any(|w| contains(NAME_STOP, w)) {
                glyph_for_spoken(&filtered).map(|glyph| (glyph, start + first))
            } else {
                None
            }
        } else {
            None
        };
        let Some((glyph, first)) = lookup else {
            continue;
        };

        let mut result = Descriptor {
            glyph: glyph.to_string(),
            start: parts[first].1,
            count: 1,
        };
        let mut before = first;
        let mut tone = trailing_tone;
        if tone.is_none() {
            if let Some((modifier, length)) = skin_tone(before, names) {
                tone = Some(modifier);
                before -= length;
                result.start = parts[before].1;
            }
        }
        if let Some(tone) = tone {
            result.glyph = applying(tone, glyph)?;
        }
        // "fire fire fire emoji": the name said again is a count.
        let name = &names[first..];
        let mut repeats = 1;
        while before >= name.len()
            && repeats < MAXIMUM_REPEAT
            && names[before - name.len()..before] == *name
        {
            before -= name.len();
            repeats += 1;
        }
        if repeats > 1 {
            result.count = repeats;
            result.start = parts[before].1;
        }
        if !names[..before].iter().any(|w| !contains(NAME_STOP, w)) {
            return Some(result);
        }
        let previous = names[before - 1].as_str();
        if plural {
            if let Some(count) = count_word(previous).or_else(|| parse_int(previous)) {
                if (2..=MAXIMUM_REPEAT).contains(&count) {
                    result.count = count;
                    result.start = parts[before - 1].1;
                    return Some(result);
                }
            }
        }
        if parse_int(previous).is_some()
            || count_word(previous).is_some()
            || contains(QUANTITY_WORDS, previous)
            || contains(REFERENCE_WORDS, previous)
            || contains(NAME_WORDS, previous)
        {
            return None;
        }
        return if ends_clause { Some(result) } else { None };
    }
    None
}

/// A skin tone ("medium dark skin tone") whose last word is just before `end`.
fn skin_tone(end: usize, words: &[String]) -> Option<(&'static str, usize)> {
    if end < 3 || words[end - 1] != "tone" || words[end - 2] != "skin" {
        return None;
    }
    for (name, modifier) in SKIN_TONES {
        if end - 2 >= name.len() && words[end - 2 - name.len()..end - 2] == **name {
            return Some((modifier, name.len() + 2));
        }
    }
    None
}

fn is_modifier_base(c: char) -> bool {
    let v = c as u32;
    MODIFIER_BASES
        .iter()
        .any(|(low, high)| (*low..=*high).contains(&v))
}

/// `glyph` with a skin tone, or `None` when it cannot take one.
fn applying(tone: &str, glyph: &str) -> Option<String> {
    let mut scalars: Vec<char> = glyph.chars().collect();
    if !scalars.first().is_some_and(|c| is_modifier_base(*c)) {
        return None;
    }
    // The tone replaces the presentation selector: "✌️" + tone is "✌🏿".
    if scalars.len() > 1 && scalars[1] == '\u{FE0F}' {
        scalars.remove(1);
    }
    let mut toned = String::new();
    toned.push(scalars[0]);
    toned.push_str(tone);
    toned.extend(&scalars[1..]);
    Some(toned)
}

/// Whether the phrase ending at word `index` ends its clause.
fn ends_clause(index: usize, scan: &Scan) -> bool {
    let mut word = index;
    while word + 1 < scan.words.len() {
        if !scan.is_joiner(word) {
            return word == index;
        }
        word += 1;
        if contains(TRIGGER_WORDS, &scan.lower(word)) {
            return true;
        }
        if word - index > 6 {
            return false;
        }
    }
    word == index
}

/// "fire emoji times three", "fire emoji x3".
fn count_after(trigger: usize, scan: &Scan) -> Option<(usize, usize)> {
    if trigger + 1 >= scan.words.len() || scan.gap(trigger) != " " {
        return None;
    }
    let next = scan.lower(trigger + 1);
    let mut last = trigger + 1;
    let mut count = None;
    if let Some(rest) = next.strip_prefix('x').filter(|r| !r.is_empty()) {
        count = parse_int(rest);
    }
    if count.is_none()
        && (next == "times" || next == "x")
        && trigger + 2 < scan.words.len()
        && scan.is_joiner(trigger + 1)
    {
        let word = scan.lower(trigger + 2);
        count = count_word(&word).or_else(|| parse_int(&word));
        last = trigger + 2;
    }
    count
        .filter(|count| (2..=MAXIMUM_REPEAT).contains(count))
        .map(|count| (count, last))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spoken_numbers::fixtures;

    const CASES: &str = include_str!("../tests/fixtures/spoken-emoji.tsv");
    const PLAIN: &str = include_str!("../tests/fixtures/spoken-forms-plain.txt");

    fn plain(text: &str) -> String {
        glyphs(text, &mut |glyph| glyph)
    }

    #[test]
    fn shared_spoken_emoji_cases() {
        let cases = fixtures::cases(CASES);
        assert!(cases.len() > 100);
        let mut failures = Vec::new();
        for case in &cases {
            let output = plain(&case.input);
            if output != case.expected {
                failures.push(format!(
                    "line {} {:?} -> {:?}, expected {:?} ({})",
                    case.line, case.input, output, case.expected, case.note
                ));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn converting_twice_changes_nothing() {
        for case in fixtures::cases(CASES) {
            let once = plain(&case.input);
            assert_eq!(plain(&once), once, "line {}", case.line);
        }
    }

    #[test]
    fn plain_dictation_survives() {
        for line in PLAIN.lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            assert_eq!(plain(line), line);
        }
    }
}
