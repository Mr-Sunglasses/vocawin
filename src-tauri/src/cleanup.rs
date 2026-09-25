//! Rule-based transcript cleanup, no model needed (VocaMac "Medium" level):
//! hesitation sounds ("um", "uh"), single-letter stutters ("I I I think"), and
//! spoken self-corrections ("tomorrow, oh, no, Wednesday" → "Wednesday").
//!
//! Ported from VocaMac `WritingStyleEngine` and `SpokenCorrectionResolver`.
//! VocaMac also collapses multi-letter stutters and cut-off words, but those
//! need a spelling dictionary to tell a fragment from a word; VocaWin keeps
//! only the rules that are safe without one.

use fancy_regex::Regex;
use std::sync::OnceLock;

// MARK: - Hesitations

fn english_hesitation() -> &'static Regex {
    static EXPRESSION: OnceLock<Regex> = OnceLock::new();
    EXPRESSION.get_or_init(|| {
        Regex::new(
            r"(?i)(?<![\p{L}\p{N}'’])(u+m+|u+h+m*|e+r+m+|h+m+)(?![\p{L}\p{N}'’])([,.!?;:…]*)",
        )
        .expect("hesitation pattern")
    })
}

/// Sounds that are not a word in any language the models write. Plain "um"
/// is excluded (German "um 5 Uhr"), as are "ah", "eh", "em", "am", "mm".
const UNIVERSAL_HESITATIONS: &str =
    "u+h+m*|u+m{2,}|h+m+|e+h{2,}|e+h+m+|a+h+m+|ä+h+m*|m{3,}|х+м+|м{3,}";

fn language_hesitations(language: Option<&str>) -> &'static [&'static str] {
    match language.and_then(|code| code.split('-').next()) {
        Some("de") => &["öh"],
        Some("fr") => &["euh", "heu"],
        _ => &[],
    }
}

/// Remove hesitation sounds from English text and repair what they leave:
/// "Hello, um, how are you?" → "Hello, how are you?". Returns whether any
/// were removed.
pub fn remove_hesitations(text: &str) -> (String, bool) {
    remove_matches(english_hesitation(), text)
}

/// Other languages lose only the universal sounds, plus the language's own
/// when it is known.
pub fn remove_other_language_hesitations(text: &str, language: Option<&str>) -> (String, bool) {
    let extras: Vec<String> = language_hesitations(language)
        .iter()
        .map(|word| fancy_regex::escape(word).into_owned())
        .collect();
    let mut alternatives = vec![UNIVERSAL_HESITATIONS.to_string()];
    alternatives.extend(extras);
    let pattern = format!(
        r"(?i)(?<![\p{{L}}\p{{N}}'’])({})(?![\p{{L}}\p{{N}}'’])([,.!?;:…]*)",
        alternatives.join("|")
    );
    match Regex::new(&pattern) {
        Ok(expression) => remove_matches(&expression, text),
        Err(_) => (text.to_string(), false),
    }
}

fn remove_matches(expression: &Regex, text: &str) -> (String, bool) {
    let ranges: Vec<(usize, usize)> = expression
        .find_iter(text)
        .filter_map(Result::ok)
        .map(|found| char_range(text, found.start(), found.end()))
        .collect();
    if ranges.is_empty() {
        return (text.to_string(), false);
    }
    (remove_word_runs(&ranges, text, true), true)
}

fn char_range(text: &str, start: usize, end: usize) -> (usize, usize) {
    (text[..start].chars().count(), text[..end].chars().count())
}

fn is_horizontal_space(c: char) -> bool {
    c == ' ' || c == '\t'
}

/// Delete whole words (char ranges) and repair what they leave behind. A
/// sentence end the words carried moves to the word before ("hello, um." →
/// "hello."), and a capitalized sentence opener hands its capital to the next
/// word ("Um I hope" → "I hope"). With `prose` off only the deletion happens.
fn remove_word_runs(ranges: &[(usize, usize)], text: &str, prose: bool) -> String {
    let mut result: Vec<char> = text.chars().collect();
    let mut sorted = ranges.to_vec();
    sorted.sort();
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for range in sorted {
        if let Some(last) = runs.last_mut() {
            let mut gap = last.1;
            while gap < range.0 && is_horizontal_space(result[gap]) {
                gap += 1;
            }
            if gap >= range.0 {
                last.0 = last.0.min(range.0);
                last.1 = last.1.max(range.1);
                continue;
            }
        }
        runs.push(range);
    }

    for (run_start, run_end) in runs.into_iter().rev() {
        let words: Vec<char> = result[run_start..run_end].to_vec();
        let trailing: Vec<char> = words
            .iter()
            .rev()
            .take_while(|c| ",.!?;:…".contains(**c))
            .copied()
            .collect();

        let mut previous = run_start as isize - 1;
        while previous >= 0 && is_horizontal_space(result[previous as usize]) {
            previous -= 1;
        }
        let at_sentence_start = previous < 0 || ".!?…\n\r".contains(result[previous as usize]);

        let mut end = run_end;
        while end < result.len() && is_horizontal_space(result[end]) {
            end += 1;
        }
        let start = if end == run_end {
            (previous + 1) as usize
        } else {
            run_start
        };
        result.drain(start..end);

        if !prose {
            continue;
        }
        if at_sentence_start {
            if words.first().is_some_and(|c| c.is_uppercase()) {
                let mut next = start;
                while next < result.len() && is_horizontal_space(result[next]) {
                    next += 1;
                }
                if next < result.len() {
                    let upper: Vec<char> = result[next].to_uppercase().collect();
                    if upper.len() == 1 && upper[0] != result[next] {
                        result[next] = upper[0];
                    }
                }
            }
        } else if let Some(terminal) = trailing.iter().find(|c| ".!?…".contains(**c)) {
            // `trailing` is reversed, so the first terminal found is the last one said.
            if previous >= 0 {
                let mut last = previous;
                while last >= 0 && ",;:".contains(result[last as usize]) {
                    result.remove(last as usize);
                    last -= 1;
                }
                if last >= 0 && !".!?…".contains(result[last as usize]) {
                    result.insert(last as usize + 1, *terminal);
                }
            }
        }
    }
    result.into_iter().collect()
}

// MARK: - Stutters

/// Collapse a single letter said three or more times: "I I I think" → "I
/// think". "no no no" and "I I think" are left as spoken.
pub fn collapse_stutters(text: &str) -> (String, usize) {
    let chars: Vec<char> = text.chars().collect();
    let mut tokens: Vec<(usize, usize, String)> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        let word: String = chars[start..i].iter().collect();
        tokens.push((start, i, word.to_lowercase()));
    }
    let mut removals = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        let key = &tokens[index].2;
        let mut end = index + 1;
        while end < tokens.len() && tokens[end].2 == *key {
            end += 1;
        }
        let is_stutter = key.chars().count() == 1 && key.chars().all(char::is_alphabetic);
        if end - index >= 3 && is_stutter {
            removals.extend(tokens[index + 1..end].iter().map(|t| (t.0, t.1)));
        }
        index = end;
    }
    if removals.is_empty() {
        return (text.to_string(), 0);
    }
    let count = removals.len();
    (remove_word_runs(&removals, text, false), count)
}

// MARK: - Spoken corrections

const DAYS: &[&str] = &[
    "today",
    "tomorrow",
    "yesterday",
    "tonight",
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "sunday",
    "weekend",
    "hoy",
    "mañana",
    "manana",
    "ayer",
    "lunes",
    "martes",
    "miércoles",
    "miercoles",
    "jueves",
    "viernes",
    "sábado",
    "sabado",
    "domingo",
    "aujourd'hui",
    "demain",
    "hier",
    "lundi",
    "mardi",
    "mercredi",
    "jeudi",
    "vendredi",
    "samedi",
    "dimanche",
    "heute",
    "morgen",
    "übermorgen",
    "gestern",
    "montag",
    "dienstag",
    "mittwoch",
    "donnerstag",
    "freitag",
    "samstag",
    "sonntag",
    "hoje",
    "amanhã",
    "amanha",
    "ontem",
    "segunda",
    "terça",
    "terca",
    "quarta",
    "quinta",
    "sexta",
    "oggi",
    "domani",
    "ieri",
    "lunedì",
    "lunedi",
    "martedì",
    "martedi",
    "mercoledì",
    "mercoledi",
    "giovedì",
    "giovedi",
    "venerdì",
    "venerdi",
    "sabato",
    "domenica",
    "aaj",
    "kal",
    "parso",
    "parson",
    "somvar",
    "somwar",
    "mangalvar",
    "mangalwar",
    "budhvar",
    "budhwar",
    "guruvar",
    "guruwar",
    "shukravar",
    "shukrawar",
    "shanivar",
    "shaniwar",
    "ravivar",
    "raviwar",
    "itvaar",
];

const MONTHS: &[&str] = &[
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
    "enero",
    "febrero",
    "marzo",
    "abril",
    "mayo",
    "junio",
    "julio",
    "agosto",
    "septiembre",
    "octubre",
    "noviembre",
    "diciembre",
    "janvier",
    "février",
    "fevrier",
    "mars",
    "avril",
    "mai",
    "juin",
    "juillet",
    "août",
    "aout",
    "septembre",
    "octobre",
    "novembre",
    "décembre",
    "decembre",
    "januar",
    "februar",
    "märz",
    "maerz",
    "juni",
    "juli",
    "oktober",
    "dezember",
    "janeiro",
    "fevereiro",
    "março",
    "marco",
    "maio",
    "junho",
    "julho",
    "setembro",
    "outubro",
    "novembro",
    "dezembro",
    "gennaio",
    "febbraio",
    "aprile",
    "maggio",
    "giugno",
    "luglio",
    "settembre",
    "ottobre",
    "dicembre",
];

const NUMBER_WORDS: &[&str] = &[
    "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven",
    "twelve", "fifteen", "twenty", "thirty", "forty", "fifty", "hundred", "noon", "midnight",
    "uno", "dos", "tres", "cuatro", "cinco", "seis", "siete", "ocho", "nueve", "diez", "deux",
    "trois", "quatre", "cinq", "sept", "huit", "neuf", "dix", "eins", "zwei", "drei", "vier",
    "fünf", "sechs", "sieben", "acht", "neun", "zehn", "ek", "paanch", "chhe", "saat", "aath",
    "nau",
];

const ARTICLES: &[&str] = &[
    "the", "on", "at", "in", "by", "el", "la", "los", "las", "le", "der", "die", "das", "am", "um",
    "à", "il", "lo",
];

const DAY_MODIFIERS: &[&str] = &[
    "next",
    "this",
    "last",
    "coming",
    "próximo",
    "prochain",
    "nächsten",
];

const NEGATIONS: &[&str] = &[
    "not", "no", "never", "cannot", "can't", "won't", "don't", "doesn't", "didn't", "isn't",
    "aren't", "nicht", "kein", "keine", "nie", "pas", "jamais", "nunca", "não", "nao", "non",
    "nahi", "nahin", "mat",
];

/// Cue phrases, longest first so "or no" wins over "no".
const CUES: &[&str] = &[
    "no no",
    "oh no",
    "no wait",
    "or no",
    "oh sorry",
    "or rather",
    "i mean",
    "make that",
    "scratch that",
    "no sorry",
    "sorry",
    "wait",
    "actually",
    "rather",
    "correction",
    "no",
    "perdón",
    "perdon",
    "mejor dicho",
    "digo",
    "o sea",
    "non",
    "pardon",
    "je veux dire",
    "plutôt",
    "plutot",
    "enfin",
    "nein",
    "ich meine",
    "besser gesagt",
    "quatsch",
    "não",
    "nao",
    "desculpa",
    "quer dizer",
    "aliás",
    "alias",
    "scusa",
    "cioè",
    "cioe",
    "anzi",
    "nahi nahi",
    "nahi",
    "nahin",
    "mera matlab",
    "matlab",
];

/// Cues that are also everyday words ("wait 5 minutes") count only when set
/// off by punctuation on both sides.
const PUNCTUATED_CUES: &[&str] = &[
    "no", "wait", "non", "nein", "não", "nao", "nahi", "nahin", "pardon", "enfin", "alias", "digo",
    "matlab",
];

const SPACE: &str = "[ \\t]";

fn alternation(words: &[&str]) -> String {
    let mut sorted: Vec<&str> = words.to_vec();
    sorted.sort_by_key(|word| std::cmp::Reverse(word.chars().count()));
    sorted
        .iter()
        .map(|word| fancy_regex::escape(word).into_owned())
        .collect::<Vec<_>>()
        .join("|")
}

fn value_pattern(group: &str) -> String {
    let day = format!(
        "(?:(?:{}){SPACE}+)?(?:{})(?:-feira)?",
        alternation(DAY_MODIFIERS),
        alternation(DAYS)
    );
    let month = alternation(MONTHS);
    let number = format!(
        "(?:\\d+(?:[:.]\\d+)?|{})(?:{SPACE}*(?:a\\.?m\\.?|p\\.?m\\.?|o'clock|minutes?|mins?|hours?|days?|weeks?|percent|%|uhr|heures?|horas?|ore|baje))?",
        alternation(NUMBER_WORDS)
    );
    format!(
        "(?<{group}>(?:(?:{}){SPACE}+)?(?:{day}|{month}|{number}))",
        alternation(ARTICLES)
    )
}

fn cue_pattern(phrases: &[&str]) -> String {
    phrases
        .iter()
        .map(|phrase| {
            phrase
                .split(' ')
                .map(|word| fancy_regex::escape(word).into_owned())
                .collect::<Vec<_>>()
                .join("[, \\t]+")
        })
        .collect::<Vec<_>>()
        .join("|")
}

fn correction_expression() -> &'static Regex {
    static EXPRESSION: OnceLock<Regex> = OnceLock::new();
    EXPRESSION.get_or_init(|| {
        let plain_cues: Vec<&str> = CUES
            .iter()
            .copied()
            .filter(|cue| !PUNCTUATED_CUES.contains(cue))
            .collect();
        let punctuated_cues: Vec<&str> = CUES
            .iter()
            .copied()
            .filter(|cue| PUNCTUATED_CUES.contains(cue))
            .collect();
        let separators = "[,.;…—–\\- \\t]";
        let punctuation = format!("{SPACE}*[,.;…—–\\-]{separators}*");
        let interjection = "(?:oh[, \\t]+)?";
        let plain = format!(
            "{separators}*{interjection}(?:{})(?![\\p{{L}}]){separators}+",
            cue_pattern(&plain_cues)
        );
        let punctuated = format!(
            "{punctuation}{interjection}(?:{})(?![\\p{{L}}]){punctuation}",
            cue_pattern(&punctuated_cues)
        );
        let pattern = format!(
            "(?i)(?<![\\p{{L}}\\p{{N}}])(?<correction>{}(?:{plain}|{punctuated}){})(?![\\p{{L}}\\p{{N}}])",
            value_pattern("first"),
            value_pattern("replacement")
        );
        Regex::new(&pattern).expect("correction pattern")
    })
}

#[derive(PartialEq, Eq)]
enum Kind {
    Day,
    Month,
    Number,
}

fn kind(value: &str) -> Option<Kind> {
    let lower = value.to_lowercase();
    let words: Vec<&str> = lower.split(' ').filter(|w| !w.is_empty()).collect();
    let core = words
        .iter()
        .rev()
        .find(|w| !ARTICLES.contains(w) && !DAY_MODIFIERS.contains(w))?;
    if DAYS.contains(core) || DAYS.contains(&core.replace("-feira", "").as_str()) {
        return Some(Kind::Day);
    }
    if MONTHS.contains(core) {
        return Some(Kind::Month);
    }
    if words
        .iter()
        .any(|w| w.chars().next().is_some_and(|c| c.is_numeric()) || NUMBER_WORDS.contains(w))
    {
        return Some(Kind::Number);
    }
    None
}

/// A full stop between the value and its replacement. An ellipsis is a
/// hesitation, not a sentence end.
fn ends_a_sentence(gap: &str) -> bool {
    let without_ellipsis = gap.replace('…', "");
    let mut cleaned = String::new();
    let chars: Vec<char> = without_ellipsis.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '.' && chars.get(i + 1) == Some(&'.') {
            while i < chars.len() && chars[i] == '.' {
                i += 1;
            }
            continue;
        }
        cleaned.push(chars[i]);
        i += 1;
    }
    cleaned.chars().any(|c| ".!?".contains(c))
}

fn contains_negation(clause: &str) -> bool {
    let normalized = clause.to_lowercase().replace('’', "'");
    normalized
        .split(|c: char| !(c.is_alphabetic() || c == '\''))
        .any(|word| NEGATIONS.contains(&word) || word.ends_with("n't"))
}

/// "at 5, no, 6" → "at 6"; "next Monday, no, Tuesday" → "next Tuesday".
fn carrying_leading_words(first: &str, replacement: &str) -> String {
    let leading = |value: &str| -> Vec<String> {
        value
            .split(' ')
            .take_while(|w| {
                let lower = w.to_lowercase();
                ARTICLES.contains(&lower.as_str()) || DAY_MODIFIERS.contains(&lower.as_str())
            })
            .map(str::to_string)
            .collect()
    };
    if !leading(replacement).is_empty() {
        return replacement.to_string();
    }
    let carried = leading(first);
    if carried.is_empty() {
        replacement.to_string()
    } else {
        let mut parts = carried;
        parts.push(replacement.to_string());
        parts.join(" ")
    }
}

/// Resolves explicit, compact self-corrections, returning the text and how
/// many were resolved.
pub fn resolve_corrections(text: &str) -> (String, usize) {
    let expression = correction_expression();
    let mut result = text.to_string();
    let mut count = 0;
    for _ in 0..8 {
        let mut replaced = None;
        for captures in expression.captures_iter(&result).filter_map(Result::ok) {
            let (Some(first), Some(replacement), Some(correction)) = (
                captures.name("first"),
                captures.name("replacement"),
                captures.name("correction"),
            ) else {
                continue;
            };
            let first_kind = kind(first.as_str());
            if first_kind.is_none()
                || first_kind != kind(replacement.as_str())
                || first.as_str().to_lowercase() == replacement.as_str().to_lowercase()
            {
                continue;
            }
            if ends_a_sentence(&result[first.end()..replacement.start()]) {
                continue;
            }
            let before = &result[..correction.start()];
            let clause = before
                .rsplit(|c: char| ".!?\n".contains(c))
                .next()
                .unwrap_or(before);
            if contains_negation(clause) {
                continue;
            }
            replaced = Some((
                correction.start(),
                correction.end(),
                carrying_leading_words(first.as_str(), replacement.as_str()),
            ));
            break;
        }
        let Some((start, end, value)) = replaced else {
            break;
        };
        result.replace_range(start..end, &value);
        count += 1;
    }
    (result, count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hesitation_cleanup_repairs_punctuation_and_case() {
        for (input, expected) in [
            ("Hello, um, how are you?", "Hello, how are you?"),
            (
                "How are you? Um I hope you're good.",
                "How are you? I hope you're good.",
            ),
            ("Um, so we ship Friday", "So we ship Friday"),
            ("hello world, um.", "hello world."),
            ("uh uh okay", "okay"),
            ("Umm, uhh, hmm, erm, uhm", ""),
            (
                "the summary says 5 mm and humming",
                "the summary says 5 mm and humming",
            ),
        ] {
            assert_eq!(remove_hesitations(input).0, expected, "{input}");
        }
        assert!(!remove_hesitations("nothing to remove").1);
    }

    #[test]
    fn other_languages_lose_only_universal_hesitations() {
        for (input, language, expected) in [
            (
                "Wir treffen uns um 5 Uhr, ähm, am Bahnhof",
                Some("de"),
                "Wir treffen uns um 5 Uhr, am Bahnhof",
            ),
            (
                "Eu moro em Lisboa, hmm, perto do rio",
                Some("pt"),
                "Eu moro em Lisboa, perto do rio",
            ),
            ("Alors euh on commence", Some("fr"), "Alors on commence"),
            ("Alors euh on commence", None, "Alors euh on commence"),
            ("Uhm, ja", None, "Ja"),
        ] {
            assert_eq!(
                remove_other_language_hesitations(input, language).0,
                expected,
                "{input}"
            );
        }
    }

    #[test]
    fn single_letter_stutters_collapse_but_words_stay() {
        for (input, expected) in [
            ("I I I think so", "I think so"),
            ("no no no, not that", "no no no, not that"),
            ("it is very very very good", "it is very very very good"),
            ("I I think", "I I think"),
        ] {
            assert_eq!(collapse_stutters(input).0, expected, "{input}");
        }
    }

    #[test]
    fn explicit_corrections_keep_the_replacement() {
        for (input, expected) in [
            ("Meet me at 2, actually 3 tomorrow", "Meet me at 3 tomorrow"),
            ("Use 2:00 rather 3:30", "Use 3:30"),
            (
                "let's do it tomorrow, oh, no, Wednesday",
                "let's do it Wednesday",
            ),
            (
                "let's do it tomorrow or no Wednesday",
                "let's do it Wednesday",
            ),
            ("see you on Monday, sorry, Tuesday.", "see you on Tuesday."),
            ("meet next Monday, no wait, Friday", "meet next Friday"),
            (
                "the launch is in March, I mean April",
                "the launch is in April",
            ),
            ("call me at 5, no, 6 pm", "call me at 6 pm"),
            (
                "give it 15 minutes, make that 20 minutes",
                "give it 20 minutes",
            ),
            (
                "lo hacemos mañana, no, el miércoles",
                "lo hacemos el miércoles",
            ),
            ("on se voit demain, non, mercredi", "on se voit mercredi"),
            (
                "wir treffen uns morgen, nein, Mittwoch",
                "wir treffen uns Mittwoch",
            ),
            ("vamos amanhã, não, quarta", "vamos quarta"),
            ("ci vediamo domani, anzi, giovedì", "ci vediamo giovedì"),
            (
                "meeting kal, nahi, parso rakhte hain",
                "meeting parso rakhte hain",
            ),
            ("deploy Monday, no, Tuesday", "deploy Tuesday"),
            ("Friday - wait - Saturday", "Saturday"),
            ("meet at 2, sorry, 3 pm", "meet at 3 pm"),
            ("call me at 5 p.m., no, 6 p.m.", "call me at 6 p.m."),
            (
                "let's do it tomorrow… no, Wednesday",
                "let's do it Wednesday",
            ),
        ] {
            assert_eq!(resolve_corrections(input).0, expected, "{input}");
        }
    }

    #[test]
    fn things_that_only_look_like_corrections_stay() {
        for text in [
            "meeting kal hai, nahi nahi, parso",
            "Are you coming tomorrow? No, Wednesday.",
            "I can't do Monday, no, Tuesday works",
            "Monday no problem",
            "tomorrow, no, not Wednesday",
            "No, Wednesday works for me",
            "I'll do it, no, really",
            "deploy Monday\nno, Tuesday",
            "deploy Monday, no,\nTuesday",
            "Press 1, wait 2 seconds, then press 3",
            "Boil for 10, wait 5 minutes, then drain",
            "We open Monday, no Sunday hours",
            "I work Monday, no Tuesday or Wednesday",
            "I was actually thrilled with the result",
            "We need 2. Sorry, 3 of us can't make it.",
            "Version 2. Actually 3 people asked",
            "Ship it Monday! Wait, Tuesday is a holiday",
        ] {
            assert_eq!(resolve_corrections(text).0, text, "{text}");
        }
    }
}
