//! Transcript → text to type. The stages that need no model, in VocaMac's
//! `DictationOutputPipeline` order:
//!
//! 1. Cleanup (optional): hesitations, single-letter stutters, spoken
//!    corrections.
//! 2. Personal dictionary: replacements and vocabulary spellings.
//! 3. Snippets: triggers become the user's saved text.
//! 4. Spoken emoji, then spoken numbers (both opt-in).
//! 5. Formatting: auto-capitalize and trailing space.
//!
//! Snippet expansions, protected dictionary terms and emoji glyphs are masked
//! with private-use placeholders from stage 3 on, so formatting never re-cases
//! text the user authored (`iPhone` stays `iPhone` at a sentence start).

use crate::{cleanup, dictionary, output, spoken_emoji, spoken_numbers};

/// First placeholder. The emoji stage masks its own spans at U+F8FE/U+F8FF,
/// so the lane stops well before that.
const PLACEHOLDER_BASE: u32 = 0xE000;
const PLACEHOLDER_LIMIT: u32 = 0xF800;

pub struct TextOptions<'a> {
    /// Engine language code ("en", "de"), or `None` for auto-detect.
    pub language: Option<&'a str>,
    pub cleanup: bool,
    pub vocabulary: &'a [String],
    pub replacements: &'a [dictionary::Replacement],
    pub snippets: &'a [dictionary::Snippet],
    pub numbers: bool,
    pub symbols: bool,
    pub emoji: bool,
    pub auto_capitalize: bool,
    pub trailing_space: bool,
}

struct Masked {
    text: String,
    values: Vec<String>,
}

impl Masked {
    /// A placeholder for `value`, or the value itself once the lane is full.
    fn push(&mut self, value: String) -> String {
        let code = PLACEHOLDER_BASE + self.values.len() as u32;
        match char::from_u32(code).filter(|_| code < PLACEHOLDER_LIMIT) {
            Some(placeholder) => {
                self.values.push(value);
                placeholder.to_string()
            }
            None => value,
        }
    }

    fn restore(&self, text: &str) -> String {
        text.chars()
            .map(|c| {
                let code = c as u32;
                if (PLACEHOLDER_BASE..PLACEHOLDER_LIMIT).contains(&code) {
                    if let Some(value) = self.values.get((code - PLACEHOLDER_BASE) as usize) {
                        return value.clone();
                    }
                }
                c.to_string()
            })
            .collect()
    }
}

/// Words that are common in English and rare elsewhere, for transcripts whose
/// engine did not say which language it heard.
const COMMON_ENGLISH: &[&str] = &[
    "the", "a", "an", "and", "to", "i", "you", "it", "is", "of", "that", "we", "this", "in", "for",
    "on", "my", "so", "but", "what", "can", "be", "are", "was", "with", "have", "do", "me", "your",
    "not", "just", "how", "okay", "ok", "yes", "hello", "hi", "thanks", "please", "let's", "i'm",
];

/// Whether unlabelled text reads as English: Latin letters, and either a
/// common English word or too short to tell.
pub fn likely_english(text: &str) -> bool {
    let letters: Vec<char> = text.chars().filter(|c| c.is_alphabetic()).collect();
    if letters.is_empty() {
        return false;
    }
    let ascii = letters.iter().filter(|c| c.is_ascii()).count();
    if ascii * 10 < letters.len() * 9 {
        return false;
    }
    let words: Vec<String> = text
        .split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect();
    words.len() <= 2 || words.iter().any(|w| COMMON_ENGLISH.contains(&w.as_str()))
}

pub fn process(raw: &str, options: &TextOptions) -> String {
    let mut input = raw.trim().to_string();
    if input.is_empty() {
        return String::new();
    }

    if options.cleanup {
        let english = match options.language {
            Some(code) => code.split('-').next() == Some("en"),
            None => likely_english(&input),
        };
        let (text, removed) = if english {
            cleanup::remove_hesitations(&input)
        } else {
            cleanup::remove_other_language_hesitations(&input, options.language)
        };
        input = text;
        if removed && input.trim().is_empty() {
            return String::new();
        }
        input = cleanup::collapse_stutters(&input).0;
        input = cleanup::resolve_corrections(&input).0;
    }

    let correction = dictionary::correct(&input, options.vocabulary, options.replacements);
    let mut snippets = options.snippets.to_vec();
    snippets.extend(
        correction
            .protected_terms
            .iter()
            .map(|term| dictionary::Snippet {
                trigger: term.clone(),
                expansion: term.clone(),
            }),
    );

    let mut masked = Masked {
        text: String::new(),
        values: Vec::new(),
    };
    let source = correction.text;
    let mut copied = 0;
    for (start, end, expansion) in dictionary::snippet_hits(&source, &snippets) {
        if start < copied {
            continue;
        }
        masked.text.push_str(&source[copied..start]);
        let placeholder = masked.push(expansion);
        masked.text.push_str(&placeholder);
        copied = end;
    }
    masked.text.push_str(&source[copied..]);

    if options.emoji {
        let text = std::mem::take(&mut masked.text);
        masked.text = spoken_emoji::glyphs(&text, &mut |glyph| masked.push(glyph));
    }
    if options.numbers {
        masked.text = spoken_numbers::digits(&masked.text, options.symbols);
    }
    let formatted = output::apply_output_polish(
        &masked.text,
        options.auto_capitalize,
        options.trailing_space,
    );
    masked.restore(&formatted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dictionary::{Replacement, Snippet};

    fn options<'a>() -> TextOptions<'a> {
        TextOptions {
            language: None,
            cleanup: true,
            vocabulary: &[],
            replacements: &[],
            snippets: &[],
            numbers: false,
            symbols: false,
            emoji: false,
            auto_capitalize: true,
            trailing_space: false,
        }
    }

    #[test]
    fn default_pipeline_cleans_and_formats() {
        assert_eq!(
            process("um let's do it tomorrow, oh, no, Wednesday", &options()),
            "Let's do it Wednesday"
        );
        assert_eq!(process("Uh", &options()), "");
        assert_eq!(process("  hello there  ", &options()), "Hello there");
    }

    #[test]
    fn cleanup_off_keeps_every_word() {
        let mut opts = options();
        opts.cleanup = false;
        assert_eq!(process("um hello", &opts), "Um hello");
    }

    #[test]
    fn german_keeps_um() {
        let mut opts = options();
        opts.language = Some("de");
        assert_eq!(
            process("wir treffen uns um 5 Uhr", &opts),
            "Wir treffen uns um 5 Uhr"
        );
    }

    #[test]
    fn dictionary_terms_keep_their_case_at_a_sentence_start() {
        let vocabulary = vec!["iPhone".to_string()];
        let mut opts = options();
        opts.vocabulary = &vocabulary;
        assert_eq!(process("i phone is here", &opts), "iPhone is here");
    }

    #[test]
    fn snippets_expand_and_are_not_recased() {
        let snippets = vec![Snippet {
            trigger: "my email".into(),
            expansion: "me@example.com".into(),
        }];
        let replacements = vec![Replacement {
            heard: "get hub".into(),
            replacement: "GitHub".into(),
        }];
        let mut opts = options();
        opts.snippets = &snippets;
        opts.replacements = &replacements;
        assert_eq!(process("my email", &opts), "me@example.com");
        assert_eq!(
            process("send get hub link to my email", &opts),
            "Send GitHub link to me@example.com"
        );
    }

    #[test]
    fn spoken_forms_are_opt_in() {
        let mut opts = options();
        assert_eq!(process("three fire emojis", &opts), "Three fire emojis");
        opts.emoji = true;
        assert_eq!(process("three fire emojis", &opts), "🔥🔥🔥");
        opts.numbers = true;
        opts.symbols = true;
        assert_eq!(
            process("it costs five dollars, party emoji", &opts),
            "It costs $5, 🎉"
        );
    }

    #[test]
    fn trailing_space_is_added_after_restoring() {
        let mut opts = options();
        opts.trailing_space = true;
        assert_eq!(process("hello", &opts), "Hello ");
    }

    #[test]
    fn english_detection_is_conservative() {
        assert!(likely_english("send it to me"));
        assert!(likely_english("Uh"));
        assert!(!likely_english("wir treffen uns um fünf Uhr heute"));
        assert!(!likely_english("मुझे तीन कॉपी चाहिए"));
    }
}
