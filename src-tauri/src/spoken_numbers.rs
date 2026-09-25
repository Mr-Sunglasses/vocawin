//! Rewrites dictated English number words as digits: "six pm at the office"
//! becomes "6 pm at the office".
//!
//! Port of VocaMac `SpokenNumbers` (itself ported from VocaPhone). The shared
//! cases in `tests/fixtures/spoken-numbers.tsv` are the contract all three
//! clients meet, so the same transcript gives the same text on every device.
//!
//! Deliberately conservative. A run of number words converts only when the
//! whole run reads as one number ("twenty three" is 23, "six seven" stays
//! words). A lone "one" stays a word unless a unit follows it or another number
//! pairs with it. Idioms keep their words ("high five", "cloud nine"). Digit
//! strings need a cue ("my number is"), years need "in"/"since"/"year", and
//! spoken times need "am"/"pm".
//!
//! With `symbols`, the stage also writes what digits usually travel with:
//! "50%", "$5.50", "-5", "21st" and "June 22".
//!
//! English only. The word lists match nothing in another language, so other
//! transcripts pass through untouched.

use std::collections::HashSet;

/// The largest number written out in full. "three trillion" still converts,
/// as "3 trillion".
const MAXIMUM: i64 = 999_999_999_999;

/// Numbers at or above this get thousands separators ("12,500"). Four-digit
/// numbers do not: most of them are years or codes.
const GROUPING_THRESHOLD: i64 = 10_000;

/// Units that make a bare "one" a quantity rather than a pronoun. Hard units
/// only: "one day I'll get to it" is ordinary English.
const QUANTIFYING_UNITS: &[&str] = &[
    "am",
    "pm",
    "o'clock",
    "oclock",
    "hour",
    "hours",
    "hr",
    "hrs",
    "minute",
    "minutes",
    "min",
    "mins",
    "second",
    "seconds",
    "sec",
    "secs",
    "percent",
    "dollar",
    "dollars",
    "rupee",
    "rupees",
    "euro",
    "euros",
    "pound",
    "pounds",
    "cent",
    "cents",
    "kg",
    "kilo",
    "kilos",
    "kilogram",
    "kilograms",
    "gram",
    "grams",
    "mg",
    "km",
    "kilometre",
    "kilometres",
    "kilometer",
    "kilometers",
    "mile",
    "miles",
    "metre",
    "metres",
    "meter",
    "meters",
    "litre",
    "litres",
    "liter",
    "liters",
    "ml",
    "degree",
    "degrees",
    "star",
    "stars",
    "kb",
    "mb",
    "gb",
    "tb",
    "mph",
    "kmph",
];

const ORDINAL_VALUES: &[(&str, i64)] = &[
    ("first", 1),
    ("second", 2),
    ("third", 3),
    ("fourth", 4),
    ("fifth", 5),
    ("sixth", 6),
    ("seventh", 7),
    ("eighth", 8),
    ("ninth", 9),
    ("tenth", 10),
    ("eleventh", 11),
    ("twelfth", 12),
    ("thirteenth", 13),
    ("fourteenth", 14),
    ("fifteenth", 15),
    ("sixteenth", 16),
    ("seventeenth", 17),
    ("eighteenth", 18),
    ("nineteenth", 19),
    ("twentieth", 20),
    ("thirtieth", 30),
    ("fortieth", 40),
    ("fiftieth", 50),
    ("sixtieth", 60),
    ("seventieth", 70),
    ("eightieth", 80),
    ("ninetieth", 90),
    ("hundredth", 100),
    ("thousandth", 1_000),
    ("millionth", 1_000_000),
];

const FRACTION_NOUNS: &[&str] = &[
    "half", "halves", "quarter", "quarters", "thirds", "fourths", "fifths", "sixths", "sevenths",
    "eighths", "ninths", "tenths",
];

const PLURAL_SCALES: &[&str] = &[
    "hundreds",
    "thousands",
    "millions",
    "billions",
    "trillions",
    "dozens",
];

const PAIRING_WORDS: &[&str] = &["or", "to", "and"];

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
];

const ZERO_WORDS: &[&str] = &["oh", "o", "nought"];

const DIGIT_CUES: &[&str] = &[
    "number",
    "phone",
    "mobile",
    "cell",
    "code",
    "pin",
    "otp",
    "extension",
    "ext",
    "room",
    "zip",
    "zipcode",
    "postcode",
    "flight",
    "account",
    "card",
    "id",
    "passcode",
    "ticket",
    "order",
    "reference",
    "ref",
    "gate",
    "apartment",
    "apt",
    "suite",
    "invoice",
    "tracking",
    "serial",
];

const CUE_FILLERS: &[&str] = &["is", "was", "it's", "its", "at", "number", "code"];

const YEAR_CUES: &[&str] = &["in", "since", "year", "circa"];

const COUNT_NOUNS: &[&str] = &[
    "day", "days", "week", "weeks", "month", "months", "year", "years", "time", "times", "people",
    "things", "items", "copies", "points",
];

const FIXED_PHRASES: &[(&[&str], &str)] = &[
    (&["twenty", "four", "seven"], "24/7"),
    (&["twenty", "four", "by", "seven"], "24/7"),
];

const IDIOMS: &[&[&str]] = &[
    &["high", "five"],
    &["high", "fives"],
    &["take", "five"],
    &["cloud", "nine"],
    &["forty", "winks"],
    &["zero", "dark", "thirty"],
    &["zero", "sum"],
    &["one", "sec"],
    &["ocean's", "eleven"],
    &["ocean’s", "eleven"],
    &["big", "three"],
    &["big", "four"],
    &["three", "musketeers"],
    &["seven", "seas"],
    &["four", "by", "four"],
    &["nine", "lives"],
    &["hang", "ten"],
    &["magnificent", "seven"],
    &["famous", "five"],
    &["deep", "six"],
    &["six", "feet", "under"],
    &["seven", "deadly", "sins"],
];

const ORDINAL_LEADER_EXTRAS: &[&str] = &["my", "your", "his", "her", "its", "our", "their"];

const ORDINAL_FOLLOWERS: &[&str] = &[
    "of", "at", "in", "on", "by", "for", "from", "to", "through", "until", "and", "or", "but",
    "so", "then", "i", "we", "you", "he", "she", "they", "it", "is", "was", "will",
];

fn currency_symbol(word: &str) -> Option<&'static str> {
    match word {
        "dollar" | "dollars" => Some("$"),
        "euro" | "euros" => Some("€"),
        "rupee" | "rupees" => Some("₹"),
        _ => None,
    }
}

fn repeat_count(word: &str) -> Option<usize> {
    match word {
        "double" => Some(2),
        "triple" => Some(3),
        _ => None,
    }
}

fn ordinal_value(word: &str) -> Option<i64> {
    ORDINAL_VALUES
        .iter()
        .find(|(name, _)| *name == word)
        .map(|(_, value)| *value)
}

/// "second" is decided from context by `is_ordinal_second`.
fn is_ordinal_word(word: &str) -> bool {
    word != "second" && ordinal_value(word).is_some()
}

fn contains(list: &[&str], word: &str) -> bool {
    list.contains(&word)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Token {
    Unit(i64),
    Teen(i64),
    Tens(i64),
    Hundred,
    Scale(i64),
    And,
    Point,
}

impl Token {
    fn opens_a_number(self) -> bool {
        matches!(self, Token::Unit(_) | Token::Teen(_) | Token::Tens(_))
    }

    fn is_connector(self) -> bool {
        matches!(self, Token::And | Token::Point)
    }

    fn is_scale(self) -> bool {
        matches!(self, Token::Scale(_))
    }

    /// Only the connectors are checked here, so "between five and ten" stays
    /// two numbers. Everything else is left to `parse`.
    fn may_extend(self, tokens: &[Token]) -> bool {
        if tokens.last().is_some_and(|t| t.is_scale()) && tokens.contains(&Token::Point) {
            return false;
        }
        match self {
            Token::And => matches!(tokens.last(), Some(Token::Hundred) | Some(Token::Scale(_))),
            Token::Point => {
                tokens.last().is_some_and(|t| !t.is_connector()) && !tokens.contains(&Token::Point)
            }
            _ => true,
        }
    }
}

fn word_token(key: &str) -> Option<Token> {
    let unit = match key {
        "zero" => Some(0),
        "one" => Some(1),
        "two" => Some(2),
        "three" => Some(3),
        "four" => Some(4),
        "five" => Some(5),
        "six" => Some(6),
        "seven" => Some(7),
        "eight" => Some(8),
        "nine" => Some(9),
        _ => None,
    };
    if let Some(value) = unit {
        return Some(Token::Unit(value));
    }
    if let Some(value) = teen_value(key) {
        return Some(Token::Teen(value));
    }
    if let Some(value) = tens_value(key) {
        return Some(Token::Tens(value));
    }
    match key {
        "thousand" => Some(Token::Scale(1_000)),
        "million" => Some(Token::Scale(1_000_000)),
        "billion" => Some(Token::Scale(1_000_000_000)),
        "trillion" => Some(Token::Scale(1_000_000_000_000)),
        "hundred" => Some(Token::Hundred),
        "and" => Some(Token::And),
        "point" => Some(Token::Point),
        _ => None,
    }
}

fn teen_value(key: &str) -> Option<i64> {
    Some(match key {
        "ten" => 10,
        "eleven" => 11,
        "twelve" => 12,
        "thirteen" => 13,
        "fourteen" => 14,
        "fifteen" => 15,
        "sixteen" => 16,
        "seventeen" => 17,
        "eighteen" => 18,
        "nineteen" => 19,
        _ => return None,
    })
}

fn tens_value(key: &str) -> Option<i64> {
    Some(match key {
        "twenty" => 20,
        "thirty" => 30,
        // A model that writes the misspelling is still saying forty.
        "forty" | "fourty" => 40,
        "fifty" => 50,
        "sixty" => 60,
        "seventy" => 70,
        "eighty" => 80,
        "ninety" => 90,
        _ => return None,
    })
}

fn scale_name(value: i64) -> Option<&'static str> {
    match value {
        1_000_000 => Some("million"),
        1_000_000_000 => Some("billion"),
        1_000_000_000_000 => Some("trillion"),
        _ => None,
    }
}

/// The words of a transcript, lower-cased once, with byte ranges into it.
struct Words<'a> {
    text: &'a str,
    ranges: Vec<(usize, usize)>,
    lower: Vec<String>,
}

impl<'a> Words<'a> {
    /// `[A-Za-z]+(?:['’][A-Za-z]+)*`
    fn new(text: &'a str) -> Self {
        let mut ranges = Vec::new();
        let chars: Vec<(usize, char)> = text.char_indices().collect();
        let mut i = 0;
        while i < chars.len() {
            if !chars[i].1.is_ascii_alphabetic() {
                i += 1;
                continue;
            }
            let start = chars[i].0;
            let mut j = i;
            loop {
                while j < chars.len() && chars[j].1.is_ascii_alphabetic() {
                    j += 1;
                }
                if j + 1 < chars.len()
                    && (chars[j].1 == '\'' || chars[j].1 == '’')
                    && chars[j + 1].1.is_ascii_alphabetic()
                {
                    j += 1;
                    continue;
                }
                break;
            }
            let end = if j < chars.len() {
                chars[j].0
            } else {
                text.len()
            };
            ranges.push((start, end));
            i = j;
        }
        let lower = ranges
            .iter()
            .map(|(start, end)| text[*start..*end].to_lowercase())
            .collect();
        Self {
            text,
            ranges,
            lower,
        }
    }

    fn count(&self) -> usize {
        self.ranges.len()
    }

    fn original(&self, index: usize) -> &str {
        let (start, end) = self.ranges[index];
        &self.text[start..end]
    }

    fn lower(&self, index: usize) -> &str {
        &self.lower[index]
    }

    /// Text between word `index` and the next one.
    fn gap(&self, index: usize) -> &str {
        &self.text[self.ranges[index].1..self.ranges[index + 1].0]
    }

    /// A single space, or the hyphen of "twenty-three".
    fn is_joiner(&self, index: usize) -> bool {
        matches!(self.gap(index), " " | "-" | "‑")
    }

    fn token(&self, index: usize) -> Option<Token> {
        self.token_decimal(index, false)
    }

    fn token_decimal(&self, index: usize, is_decimal: bool) -> Option<Token> {
        if let Some(token) = word_token(&self.lower[index]) {
            return Some(token);
        }
        // Past the decimal point "oh" is a zero: "three point oh".
        if is_decimal && contains(ZERO_WORDS, &self.lower[index]) {
            return Some(Token::Unit(0));
        }
        None
    }

    fn matches(&self, phrase: &[&str], at: usize) -> bool {
        if at + phrase.len() > self.count() {
            return false;
        }
        for (offset, word) in phrase.iter().enumerate() {
            if self.lower[at + offset] != *word {
                return false;
            }
            if offset > 0 && !self.is_joiner(at + offset - 1) {
                return false;
            }
        }
        true
    }
}

/// Words `first..=last` and what to write for them. `None` leaves the words
/// as spoken and moves past them.
struct Match {
    first: usize,
    last: usize,
    replacement: Option<String>,
}

struct Phrase {
    value: i64,
    decimals: String,
    scale_word: Option<&'static str>,
}

impl Phrase {
    fn formatted(&self) -> String {
        let whole = if self.scale_word.is_none() && self.value >= GROUPING_THRESHOLD {
            grouped(self.value)
        } else {
            self.value.to_string()
        };
        let number = if self.decimals.is_empty() {
            whole
        } else {
            format!("{whole}.{}", self.decimals)
        };
        match self.scale_word {
            Some(name) => format!("{number} {name}"),
            None => number,
        }
    }
}

fn grouped(value: i64) -> String {
    let digits: Vec<char> = value.to_string().chars().collect();
    let mut result = String::new();
    for (index, digit) in digits.iter().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            result.push(',');
        }
        result.push(*digit);
    }
    result
}

/// Rewrites every number phrase in `text` as digits.
pub fn digits(text: &str, symbols: bool) -> String {
    if text.is_empty() {
        return text.to_string();
    }
    let words = Words::new(text);
    if words.count() == 0 {
        return text.to_string();
    }
    let protected = idiom_words(&words);
    let mut result = String::new();
    let mut copied = 0usize;
    let mut changed = false;
    let mut index = 0usize;
    while index < words.count() {
        let found = fixed_phrase(index, &words)
            .or_else(|| time(index, &words, &protected))
            .or_else(|| digit_string(index, &words))
            .or_else(|| year(index, &words, &protected))
            .or_else(|| {
                if symbols {
                    day_of_month(index, &words)
                } else {
                    None
                }
            })
            .or_else(|| number(index, &words, &protected, symbols));
        let Some(found) = found else {
            index += 1;
            continue;
        };
        if let Some(replacement) = found.replacement {
            let start = words.ranges[found.first].0;
            result.push_str(&text[copied..start]);
            result.push_str(&replacement);
            copied = words.ranges[found.last].1;
            changed = true;
        }
        // A run that did not convert is skipped whole.
        index = found.last + 1;
    }
    if !changed {
        return text.to_string();
    }
    result.push_str(&text[copied..]);
    result
}

fn number(index: usize, words: &Words, protected: &HashSet<usize>, symbols: bool) -> Option<Match> {
    if protected.contains(&index) {
        return None;
    }
    // "oh seven" is "oh, seven" as often as it is 07.
    if index > 0 && contains(ZERO_WORDS, words.lower(index - 1)) && words.is_joiner(index - 1) {
        return None;
    }

    let mut tokens: Vec<Token>;
    let opens_with_a = is_article_before_a_multiplier(index, words);
    let opens_with_point = words.lower(index) == "point";
    if opens_with_a {
        tokens = vec![Token::Unit(1)];
    } else if opens_with_point {
        if !(index + 1 < words.count()
            && words.is_joiner(index)
            && matches!(words.token(index + 1), Some(Token::Unit(_))))
        {
            return None;
        }
        tokens = vec![Token::Unit(0), Token::Point];
    } else if words.lower(index) == "nought"
        && index + 1 < words.count()
        && words.is_joiner(index)
        && words.lower(index + 1) == "point"
    {
        tokens = vec![Token::Unit(0)];
    } else if let Some(first) = words.token(index).filter(|t| t.opens_a_number()) {
        tokens = vec![first];
    } else {
        return None;
    }

    let mut last = index;
    // Grow while the next word continues this number and only a space or a
    // hyphen separates them.
    while last + 1 < words.count() && !protected.contains(&(last + 1)) && words.is_joiner(last) {
        let Some(next) = words.token_decimal(last + 1, tokens.contains(&Token::Point)) else {
            break;
        };
        if !next.may_extend(&tokens) || starts_a_second_hundreds(next, &tokens, last + 1, words) {
            break;
        }
        tokens.push(next);
        last += 1;
    }
    // "two hundred and the rest": the connector goes back to being a word.
    while tokens.last().is_some_and(|t| t.is_connector()) && last > index {
        tokens.pop();
        last -= 1;
    }
    if opens_with_point && !(tokens.len() >= 3 && is_followed_by_quantifying_unit(last, words)) {
        return None;
    }
    // "a hundred times" is an idiom; "a hundred dollars" is a count.
    if opens_with_a && tokens.len() <= 2 && !is_followed_by_quantifying_unit(last, words) {
        return None;
    }
    let skip = |last: usize| Match {
        first: index,
        last,
        replacement: None,
    };
    let minimum = if opens_with_point { 2 } else { 0 };
    let Some(mut phrase) = (if tokens.len() > minimum {
        parse(&tokens)
    } else {
        None
    }) else {
        return Some(skip(last));
    };

    let next = if last + 1 < words.count() {
        Some(last + 1)
    } else {
        None
    };
    let space_after = next.is_some() && words.gap(last) == " ";

    if space_after {
        if let Some(next) = next {
            let next_word = words.lower(next);
            if contains(FRACTION_NOUNS, next_word) || contains(PLURAL_SCALES, next_word) {
                return Some(skip(last));
            }
        }
    }

    let is_ordinal = next.is_some_and(|next| {
        words.is_joiner(last)
            && (is_ordinal_word(words.lower(next))
                || is_ordinal_second(&tokens, index, last, words))
    });
    if is_ordinal {
        let next = next.unwrap_or(last);
        if !symbols {
            return Some(skip(last));
        }
        let Some(value) = compound_ordinal(&tokens, &phrase, words.lower(next)) else {
            return Some(skip(last));
        };
        // "June twenty second" is a date: "June 22", not "June 22nd".
        let date = index > 0 && is_month_before(index, words);
        return Some(Match {
            first: index,
            last: next,
            replacement: Some(if date {
                value.to_string()
            } else {
                ordinal(value)
            }),
        });
    }

    if phrase.decimals.is_empty() && phrase.scale_word.is_none() {
        if let Some((decimals, fraction_last)) = fraction(last, words) {
            phrase.decimals = decimals.to_string();
            last = fraction_last;
        } else if tokens == [Token::Unit(1)] && !is_quantity_one(index, words) {
            return None;
        }
    } else if tokens == [Token::Unit(1)] && !is_quantity_one(index, words) {
        return None;
    }

    let mut first = index;
    let mut formatted = phrase.formatted();
    if symbols {
        let mut sign = "";
        if let Some(minus) = negative_sign(index, words) {
            first = minus;
            sign = "-";
        }
        if let Some(percent) = percent_sign(last, words) {
            last = percent;
            formatted.push('%');
        } else if let Some((symbol, money_last, cents)) = currency(last, words) {
            last = money_last;
            if let Some(cents) = cents {
                if phrase.decimals.is_empty() && phrase.scale_word.is_none() {
                    formatted = format!("{formatted}.{cents}");
                }
            }
            formatted = format!("{symbol}{formatted}");
        } else if let Some(shared) = range_symbol(last, words) {
            // "twenty to thirty dollars" is "$20 to $30".
            formatted = if shared == "%" {
                format!("{formatted}%")
            } else {
                format!("{shared}{formatted}")
            };
        }
        formatted = format!("{sign}{formatted}");
    }
    Some(Match {
        first,
        last,
        replacement: Some(formatted),
    })
}

/// A lone "one" is a quantity when a unit follows it ("one pm") or another
/// number pairs with it ("one or two"). Only a plain space keeps a unit
/// attached: "one, pm" is not a time.
fn is_quantity_one(index: usize, words: &Words) -> bool {
    let last = index;
    if last + 1 < words.count()
        && words.gap(last) == " "
        && contains(QUANTIFYING_UNITS, words.lower(last + 1))
    {
        return true;
    }
    if last + 2 < words.count()
        && words.gap(last) == " "
        && words.gap(last + 1) == " "
        && contains(PAIRING_WORDS, words.lower(last + 1))
        && is_partner(words.lower(last + 2))
    {
        return true;
    }
    if index >= 2
        && words.gap(index - 1) == " "
        && words.gap(index - 2) == " "
        && contains(PAIRING_WORDS, words.lower(index - 1))
        && is_partner(words.lower(index - 2))
    {
        return true;
    }
    false
}

/// Another "one" cannot pair: "one to one" stays words.
fn is_partner(word: &str) -> bool {
    word != "one"
        && matches!(
            word_token(word),
            Some(Token::Unit(_)) | Some(Token::Teen(_)) | Some(Token::Tens(_))
        )
}

/// "and a half" or "and a quarter" right after a number.
fn fraction(last: usize, words: &Words) -> Option<(&'static str, usize)> {
    if !(last + 3 < words.count()
        && words.gap(last) == " "
        && words.gap(last + 1) == " "
        && words.gap(last + 2) == " "
        && words.lower(last + 1) == "and"
        && words.lower(last + 2) == "a")
    {
        return None;
    }
    match words.lower(last + 3) {
        "half" => Some(("5", last + 3)),
        "quarter" => Some(("25", last + 3)),
        _ => None,
    }
}

/// "twenty first" is 21, "one hundred second" is 102. "two first" and "one
/// hundredth" do not compose.
fn compound_ordinal(tokens: &[Token], phrase: &Phrase, word: &str) -> Option<i64> {
    if !phrase.decimals.is_empty() || phrase.scale_word.is_some() {
        return None;
    }
    let value = ordinal_value(word).filter(|value| *value < 100)?;
    match tokens.last() {
        Some(Token::Tens(_)) => (value < 10).then_some(phrase.value + value),
        Some(Token::Hundred) | Some(Token::Scale(_)) => Some(phrase.value + value),
        _ => None,
    }
}

fn ordinal(value: i64) -> String {
    let suffix = match (value % 10, value % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{value}{suffix}")
}

/// "May fifth" is "May 5", "the fifth of May" is "the 5th of May".
fn day_of_month(index: usize, words: &Words) -> Option<Match> {
    let value = ordinal_value(words.lower(index)).filter(|value| *value <= 31)?;
    if index > 0 && is_month_before(index, words) {
        return Some(Match {
            first: index,
            last: index,
            replacement: Some(value.to_string()),
        });
    }
    if index + 2 < words.count()
        && words.gap(index) == " "
        && words.gap(index + 1) == " "
        && words.lower(index + 1) == "of"
        && is_month(words.original(index + 2))
    {
        return Some(Match {
            first: index,
            last: index,
            replacement: Some(ordinal(value)),
        });
    }
    None
}

fn is_month_before(index: usize, words: &Words) -> bool {
    words.gap(index - 1) == " " && is_month(words.original(index - 1))
}

/// "May" and "March" are also ordinary words, so they count only when
/// capitalized.
fn is_month(word: &str) -> bool {
    let lower = word.to_lowercase();
    if !contains(MONTHS, &lower) {
        return false;
    }
    if lower == "may" || lower == "march" {
        return word.chars().next().is_some_and(|c| c.is_uppercase());
    }
    true
}

/// "minus five", unless a number comes before the sign: "ten minus five".
fn negative_sign(index: usize, words: &Words) -> Option<usize> {
    let sign = index.checked_sub(1)?;
    if !(words.gap(sign) == " " && matches!(words.lower(sign), "minus" | "negative")) {
        return None;
    }
    if sign > 0 && words.is_joiner(sign - 1) {
        if let Some(token) = words.token(sign - 1) {
            if token != Token::And && token != Token::Point {
                return None;
            }
        }
    }
    Some(sign)
}

fn percent_sign(last: usize, words: &Words) -> Option<usize> {
    if !(last + 1 < words.count() && words.gap(last) == " ") {
        return None;
    }
    if words.lower(last + 1) == "percent" {
        return Some(last + 1);
    }
    if last + 2 < words.count()
        && words.lower(last + 1) == "per"
        && words.gap(last + 1) == " "
        && words.lower(last + 2) == "cent"
    {
        return Some(last + 2);
    }
    None
}

/// "five dollars and fifty cents" is $5.50. Pounds are left out: "five
/// pounds" is a weight as often as money.
fn currency(last: usize, words: &Words) -> Option<(&'static str, usize, Option<String>)> {
    if !(last + 1 < words.count() && words.gap(last) == " ") {
        return None;
    }
    let symbol = currency_symbol(words.lower(last + 1))?;
    let unit = last + 1;
    let plain = Some((symbol, unit, None));
    let mut index = unit + 2;
    if !(index < words.count()
        && words.gap(unit) == " "
        && words.lower(unit + 1) == "and"
        && words.gap(unit + 1) == " ")
    {
        return plain;
    }
    let mut cents: i64;
    match words.token(index) {
        Some(Token::Unit(value)) if value > 0 => cents = value,
        Some(Token::Teen(value)) => cents = value,
        Some(Token::Tens(value)) => {
            cents = value;
            if index + 1 < words.count() && words.is_joiner(index) {
                if let Some(Token::Unit(unit_value)) = words.token(index + 1) {
                    if unit_value > 0 {
                        cents += unit_value;
                        index += 1;
                    }
                }
            }
        }
        _ => return plain,
    }
    if !(index + 1 < words.count()
        && words.gap(index) == " "
        && matches!(words.lower(index + 1), "cent" | "cents"))
    {
        return plain;
    }
    Some((symbol, index + 1, Some(format!("{cents:02}"))))
}

/// The symbol the second number of a range takes, for the first to share.
/// Not when only the second has a scale: "between two and three hundred
/// dollars" may be $200 to $300.
fn range_symbol(last: usize, words: &Words) -> Option<&'static str> {
    if !(last + 2 < words.count()
        && words.gap(last) == " "
        && contains(PAIRING_WORDS, words.lower(last + 1))
        && words.gap(last + 1) == " "
        && words.token(last + 2).is_some_and(|t| t.opens_a_number()))
    {
        return None;
    }
    let mut end = last + 2;
    while end + 1 < words.count() && end - last < 8 && words.is_joiner(end) {
        match words.token(end + 1) {
            Some(token) if token != Token::And => end += 1,
            _ => break,
        }
    }
    if (last + 2..=end)
        .any(|i| matches!(words.token(i), Some(Token::Hundred) | Some(Token::Scale(_))))
    {
        return None;
    }
    if percent_sign(end, words).is_some() {
        return Some("%");
    }
    currency(end, words).map(|(symbol, _, _)| symbol)
}

/// "seven thirty pm" → "7:30 pm". Only before "am" or "pm".
fn time(index: usize, words: &Words, protected: &HashSet<usize>) -> Option<Match> {
    if protected.contains(&index) || index + 1 >= words.count() || words.gap(index) != " " {
        return None;
    }
    let hour = match words.token(index) {
        Some(Token::Unit(value)) if value > 0 => value,
        Some(Token::Teen(value)) if value <= 12 => value,
        _ => return None,
    };
    let mut last = index + 1;
    let minutes;
    match words.token(last) {
        Some(Token::Teen(value)) => minutes = value,
        Some(Token::Tens(value)) if value <= 50 => {
            minutes = value;
            if last + 1 < words.count() && words.is_joiner(last) {
                if let Some(Token::Unit(unit)) = words.token(last + 1) {
                    if unit > 0 {
                        last += 1;
                        return time_match(index, last, hour, value + unit, words);
                    }
                }
            }
        }
        _ => {
            let word = words.lower(last);
            if !(contains(ZERO_WORDS, word) || word == "zero") {
                return None;
            }
            if !(last + 1 < words.count() && words.gap(last) == " ") {
                return None;
            }
            match words.token(last + 1) {
                Some(Token::Unit(unit)) if unit > 0 => {
                    last += 1;
                    minutes = unit;
                }
                _ => return None,
            }
        }
    }
    time_match(index, last, hour, minutes, words)
}

fn time_match(first: usize, last: usize, hour: i64, minutes: i64, words: &Words) -> Option<Match> {
    if last + 1 >= words.count() {
        return None;
    }
    let rest = &words.text[words.ranges[last].1..];
    if !starts_with_meridiem(rest) {
        return None;
    }
    Some(Match {
        first,
        last,
        replacement: Some(format!("{hour}:{minutes:02}")),
    })
}

/// `^ (?:[AaPp]\.?[Mm]\.?)(?![A-Za-z])`
fn starts_with_meridiem(rest: &str) -> bool {
    let chars: Vec<char> = rest.chars().collect();
    if chars.first() != Some(&' ') {
        return false;
    }
    let mut i = 1;
    if !chars
        .get(i)
        .is_some_and(|c| matches!(c, 'A' | 'a' | 'P' | 'p'))
    {
        return false;
    }
    i += 1;
    if chars.get(i) == Some(&'.') {
        i += 1;
    }
    if !chars.get(i).is_some_and(|c| matches!(c, 'M' | 'm')) {
        return false;
    }
    i += 1;
    if chars.get(i) == Some(&'.') {
        i += 1;
    }
    !chars.get(i).is_some_and(|c| c.is_ascii_alphabetic())
}

/// "my number is nine eight seven…" → "my number is 987…". Three digits or
/// more, and a cue word before it or an inner "oh".
fn digit_string(index: usize, words: &Words) -> Option<Match> {
    let mut digits = String::new();
    let mut last: isize = index as isize - 1;
    let mut has_inner_zero = false;
    let mut has_repeat = false;
    let mut next = index;
    while next < words.count() {
        if next > index && !words.is_joiner(next - 1) {
            break;
        }
        let word = words.lower(next);
        let repeated = repeat_count(word).and_then(|count| {
            if next + 1 < words.count() && words.gap(next) == " " {
                if let Some(Token::Unit(value)) = words.token(next + 1) {
                    return Some((count, value));
                }
            }
            None
        });
        if let Some((count, value)) = repeated {
            digits.push_str(&value.to_string().repeat(count));
            has_repeat = true;
            last = (next + 1) as isize;
            next += 2;
        } else if let Some(Token::Unit(value)) = words.token(next) {
            digits.push_str(&value.to_string());
            last = next as isize;
            next += 1;
        } else if next > index && contains(ZERO_WORDS, word) {
            digits.push('0');
            has_inner_zero = true;
            last = next as isize;
            next += 1;
        } else {
            break;
        }
    }
    if digits.len() < 3 || last < index as isize {
        return None;
    }
    let last = last as usize;
    let cued = has_cue(index, words);
    // An "oh" that ends the string is "four five oh" said to someone.
    if contains(ZERO_WORDS, words.lower(last)) && !cued {
        return None;
    }
    if !(cued || (has_inner_zero && !has_repeat)) {
        return None;
    }
    Some(Match {
        first: index,
        last,
        replacement: Some(digits),
    })
}

fn has_cue(index: usize, words: &Words) -> bool {
    let mut word = index as isize - 1;
    let mut fillers = 0;
    while word >= 0 && fillers <= 2 {
        let w = word as usize;
        let gap = words.gap(w);
        if !matches!(gap, " " | ": " | " #" | " # ") {
            return false;
        }
        if contains(DIGIT_CUES, words.lower(w)) {
            return true;
        }
        if !contains(CUE_FILLERS, words.lower(w)) {
            return false;
        }
        fillers += 1;
        word -= 1;
    }
    false
}

/// "in nineteen ninety nine" → "in 1999", "nineteen oh five" → "1905".
fn year(index: usize, words: &Words, protected: &HashSet<usize>) -> Option<Match> {
    if protected.contains(&index) || index + 1 >= words.count() || words.gap(index) != " " {
        return None;
    }
    let century = match words.token(index) {
        Some(Token::Teen(value)) if value >= 11 => value,
        Some(Token::Tens(20)) => 20,
        _ => return None,
    };
    let mut last = index + 1;
    let mut rest;
    let mut said_oh = false;
    match words.token(last) {
        Some(Token::Teen(value)) => rest = value,
        Some(Token::Tens(value)) => {
            rest = value;
            if last + 1 < words.count() && words.is_joiner(last) {
                if let Some(Token::Unit(unit)) = words.token(last + 1) {
                    if unit > 0 {
                        rest += unit;
                        last += 1;
                    }
                }
            }
        }
        _ => {
            if !(contains(ZERO_WORDS, words.lower(last))
                && last + 1 < words.count()
                && words.gap(last) == " ")
            {
                return None;
            }
            match words.token(last + 1) {
                Some(Token::Unit(unit)) if unit > 0 => {
                    rest = unit;
                    said_oh = true;
                    last += 1;
                }
                _ => return None,
            }
        }
    }
    let cued =
        index > 0 && words.gap(index - 1) == " " && contains(YEAR_CUES, words.lower(index - 1));
    if !(cued || said_oh) {
        return None;
    }
    // "in fifteen twenty minutes" is two numbers.
    if last + 1 < words.count()
        && words.gap(last) == " "
        && (contains(QUANTIFYING_UNITS, words.lower(last + 1))
            || contains(COUNT_NOUNS, words.lower(last + 1)))
    {
        return None;
    }
    Some(Match {
        first: index,
        last,
        replacement: Some((century * 100 + rest).to_string()),
    })
}

fn fixed_phrase(index: usize, words: &Words) -> Option<Match> {
    FIXED_PHRASES
        .iter()
        .find(|(phrase, _)| words.matches(phrase, index))
        .map(|(phrase, written)| Match {
            first: index,
            last: index + phrase.len() - 1,
            replacement: Some((*written).to_string()),
        })
}

/// Indices of number words inside idioms, which stay words.
fn idiom_words(words: &Words) -> HashSet<usize> {
    let mut protected = HashSet::new();
    for index in 0..words.count() {
        for idiom in IDIOMS {
            if words.matches(idiom, index) {
                protected.extend(index..index + idiom.len());
            }
        }
        // "double seven" outside a digit string.
        if repeat_count(words.lower(index)).is_some()
            && index + 1 < words.count()
            && words.gap(index) == " "
            && matches!(words.token(index + 1), Some(Token::Unit(_)))
        {
            protected.insert(index + 1);
        }
    }
    protected
}

/// Whether the "second" after this number makes it an ordinal ("the twenty
/// second of June") rather than a duration ("a twenty second delay").
fn is_ordinal_second(tokens: &[Token], first: usize, last: usize, words: &Words) -> bool {
    if !matches!(
        tokens.last(),
        Some(Token::Tens(_)) | Some(Token::Hundred) | Some(Token::Scale(_))
    ) {
        return false;
    }
    let second = last + 1;
    if !(second < words.count() && words.lower(second) == "second" && words.is_joiner(last)) {
        return false;
    }
    if first > 0 && words.gap(first - 1) == " " {
        let before = words.lower(first - 1);
        if before == "a" || before == "an" {
            return false;
        }
        if contains(MONTHS, before) || contains(ORDINAL_LEADER_EXTRAS, before) {
            return true;
        }
    }
    if second + 1 >= words.count() {
        return true;
    }
    let separator = words.gap(second);
    if separator
        .chars()
        .any(|c| ".!?;:".contains(c) || c == '\n' || c == '\r')
    {
        return true;
    }
    contains(ORDINAL_FOLLOWERS, words.lower(second + 1))
}

/// "a hundred and fifty", "a thousand five hundred".
fn is_article_before_a_multiplier(index: usize, words: &Words) -> bool {
    index + 1 < words.count()
        && words.lower(index) == "a"
        && words.is_joiner(index)
        && matches!(
            words.token(index + 1),
            Some(Token::Hundred) | Some(Token::Scale(_))
        )
}

fn is_followed_by_quantifying_unit(index: usize, words: &Words) -> bool {
    index + 1 < words.count()
        && words.gap(index) == " "
        && contains(QUANTIFYING_UNITS, words.lower(index + 1))
}

/// In "two hundred three hundred" the "three" starts a second number.
fn starts_a_second_hundreds(next: Token, tokens: &[Token], position: usize, words: &Words) -> bool {
    if !matches!(next, Token::Unit(_) | Token::Teen(_)) {
        return false;
    }
    let segment_has_hundred = tokens
        .iter()
        .rev()
        .take_while(|t| !t.is_scale())
        .any(|t| *t == Token::Hundred);
    if !segment_has_hundred || position + 1 >= words.count() || !words.is_joiner(position) {
        return false;
    }
    words.token(position + 1) == Some(Token::Hundred)
}

/// The value of a run, or `None` when the words are adjacent numbers.
fn parse(tokens: &[Token]) -> Option<Phrase> {
    let mut total = 0i64;
    let mut group = 0i64;
    let mut decimals = String::new();
    let mut is_decimal = false;
    let mut smallest_scale = i64::MAX;
    let mut previous: Option<Token> = None;

    for (position, token) in tokens.iter().enumerate() {
        if !may_follow(previous, *token, is_decimal) {
            return None;
        }
        match *token {
            Token::Unit(value) => {
                if is_decimal {
                    decimals.push_str(&value.to_string());
                } else {
                    group += value;
                }
            }
            Token::Teen(value) | Token::Tens(value) => group += value,
            Token::Hundred => {
                if group >= 100 {
                    return None;
                }
                group *= 100;
            }
            Token::Scale(value) => {
                // "1 million", "2.5 billion", "3 trillion".
                if position == tokens.len() - 1 && total == 0 {
                    if let Some(name) = scale_name(value) {
                        if group <= 0 && decimals.is_empty() {
                            return None;
                        }
                        return Some(Phrase {
                            value: group,
                            decimals,
                            scale_word: Some(name),
                        });
                    }
                }
                // Scales descend: "two million three thousand".
                if is_decimal || group <= 0 || value >= smallest_scale {
                    return None;
                }
                smallest_scale = value;
                total += group * value;
                group = 0;
            }
            Token::And => {}
            Token::Point => is_decimal = true,
        }
        previous = Some(*token);
    }
    if is_decimal && decimals.is_empty() {
        return None;
    }
    let value = total + group;
    if value > MAXIMUM {
        return None;
    }
    Some(Phrase {
        value,
        decimals,
        scale_word: None,
    })
}

/// The grammar as one question: can this follow that?
fn may_follow(previous: Option<Token>, token: Token, is_decimal: bool) -> bool {
    if is_decimal {
        return match token {
            Token::Unit(_) => true,
            Token::Scale(value) => value >= 1_000_000 && previous != Some(Token::Point),
            _ => false,
        };
    }
    match previous {
        None => token.opens_a_number(),
        Some(Token::Unit(value)) => match token {
            Token::Hundred | Token::Scale(_) => value > 0,
            Token::Point => true,
            _ => false,
        },
        Some(Token::Teen(_)) => matches!(token, Token::Hundred | Token::Scale(_) | Token::Point),
        Some(Token::Tens(_)) => match token {
            Token::Unit(value) => value > 0,
            Token::Scale(_) | Token::Point => true,
            _ => false,
        },
        Some(Token::Hundred) | Some(Token::Scale(_)) => match token {
            Token::Unit(value) => value > 0,
            Token::Teen(_) | Token::Tens(_) | Token::Scale(_) | Token::And | Token::Point => true,
            Token::Hundred => false,
        },
        Some(Token::And) => match token {
            Token::Unit(value) => value > 0,
            Token::Teen(_) | Token::Tens(_) => true,
            _ => false,
        },
        Some(Token::Point) => matches!(token, Token::Unit(_)),
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    /// One row of a shared case file: mode, input, expected, note, line.
    pub struct Case {
        pub mode: String,
        pub input: String,
        pub expected: String,
        pub note: String,
        pub line: usize,
    }

    /// Tab-separated `input`, `expected`, `note`, with `## <mode>` lines.
    pub fn cases(text: &str) -> Vec<Case> {
        let mut mode = String::new();
        let mut cases = Vec::new();
        for (offset, line) in text.split('\n').enumerate() {
            // A Windows checkout may turn the file's line endings into CRLF.
            let line = line.strip_suffix('\r').unwrap_or(line);
            if let Some(rest) = line.strip_prefix("## ") {
                mode = rest.trim().to_string();
                continue;
            }
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let columns: Vec<String> = line
                .split('\t')
                .map(|c| c.replace("\\n", "\n").replace("\\t", "\t"))
                .collect();
            assert!(columns.len() >= 2, "line {} needs two columns", offset + 1);
            cases.push(Case {
                mode: mode.clone(),
                input: columns[0].clone(),
                expected: columns[1].clone(),
                note: columns.get(2).cloned().unwrap_or_default(),
                line: offset + 1,
            });
        }
        cases
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CASES: &str = include_str!("../tests/fixtures/spoken-numbers.tsv");
    const PLAIN: &str = include_str!("../tests/fixtures/spoken-forms-plain.txt");

    #[test]
    fn shared_spoken_number_cases() {
        let cases = fixtures::cases(CASES);
        assert!(cases.len() > 200);
        let mut failures = Vec::new();
        for case in &cases {
            assert!(case.mode == "digits" || case.mode == "symbols");
            let output = digits(&case.input, case.mode == "symbols");
            if output != case.expected {
                failures.push(format!(
                    "line {} [{}] {:?} -> {:?}, expected {:?} ({})",
                    case.line, case.mode, case.input, output, case.expected, case.note
                ));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn converting_twice_changes_nothing() {
        for case in fixtures::cases(CASES) {
            let symbols = case.mode == "symbols";
            let once = digits(&case.input, symbols);
            assert_eq!(digits(&once, symbols), once, "line {}", case.line);
        }
    }

    #[test]
    fn plain_dictation_survives_both_modes() {
        for line in PLAIN.lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            assert_eq!(digits(line, false), line);
            assert_eq!(digits(line, true), line);
        }
    }
}
