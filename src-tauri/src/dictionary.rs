//! Personal dictionary applied to every engine's transcript, and snippets.
//!
//! Ported from VocaMac `DictionaryCorrector` and `SnippetExpander`. Whisper
//! takes the vocabulary as a recognition hint (`vocabulary::whisper_prompt`),
//! but the ONNX engines cannot, and Whisper still misspells. So after
//! transcription, for every engine:
//!
//! 1. Replacements: exact spoken forms → the user's text ("get hub" → GitHub).
//! 2. Vocabulary: spoken words whose letters match a term, ignoring case,
//!    spaces and punctuation ("voca win" → VocaWin).
//!
//! VocaMac also fuzzy-matches near-misses of long terms, guarded by the macOS
//! spell checker so a real word is never "corrected". VocaWin has no such
//! oracle yet, so only exact letter matches apply.

use fancy_regex::Regex;
use serde::{Deserialize, Serialize};

/// Longest run of spoken words that can join into one term.
const MAXIMUM_WINDOW: usize = 4;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Replacement {
    /// One or more spoken forms, comma-separated. Matching ignores case.
    pub heard: String,
    /// Exactly what to type instead.
    pub replacement: String,
}

impl Replacement {
    fn heard_forms(&self) -> Vec<String> {
        self.heard
            .split(',')
            .map(|form| form.trim().to_string())
            .filter(|form| !form.is_empty())
            .collect()
    }

    fn is_valid(&self) -> bool {
        !self.heard_forms().is_empty() && !self.replacement.trim().is_empty()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snippet {
    pub trigger: String,
    pub expansion: String,
}

pub struct Correction {
    pub text: String,
    /// Terms whose exact spelling later formatting must not touch
    /// (`iPhone` must not become `IPhone`).
    pub protected_terms: Vec<String>,
}

fn is_word_character(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `\b` before a word character, `(?<!\S)` before anything else; the same on
/// the right with `(?!\S)`.
fn bounded(literal: &str) -> String {
    let escaped = fancy_regex::escape(literal);
    let prefix = if literal.chars().next().is_some_and(is_word_character) {
        r"\b"
    } else {
        r"(?<!\S)"
    };
    let suffix = if literal.chars().last().is_some_and(is_word_character) {
        r"\b"
    } else {
        r"(?!\S)"
    };
    format!("({prefix}{escaped}{suffix})")
}

/// Every (start, end, index) hit of `literals` in `text`, longest literal
/// winning, in reading order. One combined pattern, so an expansion is never
/// re-scanned by a later literal.
fn hits(text: &str, literals: &[String]) -> Vec<(usize, usize, usize)> {
    if literals.is_empty() || text.is_empty() {
        return Vec::new();
    }
    let pattern = format!(
        "(?i){}",
        literals
            .iter()
            .map(|literal| bounded(literal))
            .collect::<Vec<_>>()
            .join("|")
    );
    let Ok(expression) = Regex::new(&pattern) else {
        return Vec::new();
    };
    expression
        .captures_iter(text)
        .filter_map(Result::ok)
        .filter_map(|captures| {
            let whole = captures.get(0)?;
            let group = (1..captures.len()).find(|i| captures.get(*i).is_some())?;
            Some((whole.start(), whole.end(), group - 1))
        })
        .collect()
}

pub fn correct(text: &str, vocabulary: &[String], replacements: &[Replacement]) -> Correction {
    let mut result = Correction {
        text: text.to_string(),
        protected_terms: Vec::new(),
    };
    if text.is_empty() {
        return result;
    }
    apply_replacements(replacements, &mut result);
    apply_vocabulary(vocabulary, &mut result);
    let mut seen = std::collections::HashSet::new();
    result
        .protected_terms
        .retain(|term| seen.insert(term.clone()));
    result
}

fn apply_replacements(replacements: &[Replacement], result: &mut Correction) {
    let mut pairs: Vec<(String, String)> = replacements
        .iter()
        .filter(|r| r.is_valid())
        .flat_map(|r| {
            r.heard_forms()
                .into_iter()
                .map(move |heard| (heard, r.replacement.clone()))
        })
        .collect();
    pairs.sort_by_key(|(heard, _)| std::cmp::Reverse(heard.chars().count()));
    if pairs.is_empty() {
        return;
    }
    let literals: Vec<String> = pairs.iter().map(|(heard, _)| heard.clone()).collect();
    let found = hits(&result.text, &literals);
    if found.is_empty() {
        return;
    }
    let mut output = result.text.clone();
    for (start, end, index) in found.into_iter().rev() {
        let replacement = &pairs[index].1;
        output.replace_range(start..end, replacement);
        if needs_protection(replacement) {
            result.protected_terms.push(replacement.clone());
        }
    }
    result.text = output;
}

/// Letters and digits only, lowercased: "Voca-Win" and "voca win" agree.
fn normalized(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

/// Letter keys that spell a term: its own letters, and for a term with "&"
/// also the spoken form ("R and D" → R&D).
fn exact_keys(term: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let key = normalized(term);
    if key.chars().count() >= 2 {
        keys.push(key.clone());
    }
    if term.contains('&') {
        let spoken = normalized(&term.replace('&', " and "));
        if spoken != key {
            keys.push(spoken);
        }
    }
    keys
}

/// `[\p{L}\p{N}]+(?:['’][\p{L}]+)*` tokens as byte ranges.
fn tokens(text: &str) -> Vec<(usize, usize)> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut found = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].1.is_alphanumeric() {
            i += 1;
            continue;
        }
        let start = chars[i].0;
        let mut j = i;
        while j < chars.len() && chars[j].1.is_alphanumeric() {
            j += 1;
        }
        while j + 1 < chars.len()
            && (chars[j].1 == '\'' || chars[j].1 == '’')
            && chars[j + 1].1.is_alphabetic()
        {
            j += 1;
            while j < chars.len() && chars[j].1.is_alphabetic() {
                j += 1;
            }
        }
        let end = if j < chars.len() {
            chars[j].0
        } else {
            text.len()
        };
        found.push((start, end));
        i = j;
    }
    found
}

fn apply_vocabulary(vocabulary: &[String], result: &mut Correction) {
    let mut exact: std::collections::HashMap<String, &String> = std::collections::HashMap::new();
    for term in vocabulary {
        for key in exact_keys(term) {
            exact.insert(key, term);
        }
    }
    if exact.is_empty() {
        return;
    }
    let text = result.text.clone();
    let found = tokens(&text);
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let mut index = 0;
    while index < found.len() {
        // How many tokens from here are joined only by spaces or hyphens.
        let mut joinable = 1;
        while index + joinable < found.len() && joinable < MAXIMUM_WINDOW {
            let gap = &text[found[index + joinable - 1].1..found[index + joinable].0];
            if !gap.chars().all(|c| c == ' ' || c == '-') {
                break;
            }
            joinable += 1;
        }
        let mut matched = false;
        for length in (1..=joinable.min(MAXIMUM_WINDOW)).rev() {
            let window = &found[index..index + length];
            let key: String = window
                .iter()
                .map(|(start, end)| normalized(&text[*start..*end]))
                .collect();
            let Some(term) = exact.get(&key) else {
                continue;
            };
            let (start, end) = (window[0].0, window[length - 1].1);
            if &text[start..end] != term.as_str() {
                edits.push((start, end, (*term).clone()));
            }
            if needs_protection(term) {
                result.protected_terms.push((*term).clone());
            }
            index += length;
            matched = true;
            break;
        }
        if !matched {
            index += 1;
        }
    }
    if edits.is_empty() {
        return;
    }
    let mut output = text;
    for (start, end, term) in edits.into_iter().rev() {
        output.replace_range(start..end, &term);
    }
    result.text = output;
}

/// Whether formatting could change the term's spelling: a lowercase start,
/// capitals after the first letter, or anything that isn't a letter.
pub fn needs_protection(term: &str) -> bool {
    let trimmed = term.trim();
    let Some(first) = trimmed.chars().next() else {
        return false;
    };
    if !trimmed.chars().any(char::is_alphabetic) {
        return false;
    }
    if first.is_alphabetic() && first.is_lowercase() {
        return true;
    }
    if trimmed
        .split(' ')
        .any(|word| word.chars().skip(1).any(char::is_uppercase))
    {
        return true;
    }
    trimmed
        .chars()
        .any(|c| !c.is_alphabetic() && c != ' ' && c != '\'' && c != '’')
}

/// Snippet triggers matched in the text, as (start, end, expansion), longest
/// trigger first, in reading order.
pub fn snippet_hits(text: &str, snippets: &[Snippet]) -> Vec<(usize, usize, String)> {
    let mut sorted: Vec<&Snippet> = snippets
        .iter()
        .filter(|s| !s.trigger.trim().is_empty())
        .collect();
    sorted.sort_by_key(|s| std::cmp::Reverse(s.trigger.trim().chars().count()));
    let literals: Vec<String> = sorted
        .iter()
        .map(|s| s.trigger.trim().to_string())
        .collect();
    hits(text, &literals)
        .into_iter()
        .map(|(start, end, index)| (start, end, sorted[index].expansion.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replacement(heard: &str, to: &str) -> Replacement {
        Replacement {
            heard: heard.into(),
            replacement: to.into(),
        }
    }

    #[test]
    fn replacements_match_whole_words_ignoring_case() {
        let rules = [replacement("get hub, git hub", "GitHub")];
        let result = correct("Push it to get hub and Git Hub.", &[], &rules);
        assert_eq!(result.text, "Push it to GitHub and GitHub.");
        assert_eq!(result.protected_terms, vec!["GitHub"]);
        assert_eq!(correct("forget hubris", &[], &rules).text, "forget hubris");
    }

    #[test]
    fn vocabulary_joins_spoken_words_by_their_letters() {
        let vocabulary = vec![
            "VocaWin".to_string(),
            "R&D".to_string(),
            "kubectl".to_string(),
        ];
        assert_eq!(
            correct("I use voca win daily", &vocabulary, &[]).text,
            "I use VocaWin daily"
        );
        assert_eq!(
            correct("the R and D team", &vocabulary, &[]).text,
            "the R&D team"
        );
        assert_eq!(
            correct("run Kubectl now", &vocabulary, &[]).text,
            "run kubectl now"
        );
        assert_eq!(
            correct("nothing here", &vocabulary, &[]).text,
            "nothing here"
        );
    }

    #[test]
    fn protection_covers_terms_formatting_could_change() {
        assert!(needs_protection("iPhone"));
        assert!(needs_protection("GitHub"));
        assert!(needs_protection("kubectl"));
        assert!(needs_protection("R&D"));
        assert!(!needs_protection("New York"));
        assert!(!needs_protection("Kanishk"));
    }

    #[test]
    fn snippets_prefer_the_longest_trigger() {
        let snippets = vec![
            Snippet {
                trigger: "my address".into(),
                expansion: "221B Baker Street".into(),
            },
            Snippet {
                trigger: "my address in full".into(),
                expansion: "221B Baker Street, London".into(),
            },
        ];
        let found = snippet_hits("send my address in full please", &snippets);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].2, "221B Baker Street, London");
        assert!(snippet_hits("my addresses", &snippets).is_empty());
    }
}
