use crate::category::{Category, CategoryMap, extension};
use chrono::{DateTime, Days, Local, Months, NaiveDate, TimeZone};
use std::fmt;
use std::sync::Arc;

/// What a Query is matched against: one Entry's name and attributes.
#[derive(Clone, Copy, Debug)]
pub struct Candidate<'a> {
    pub name: &'a str,
    /// Full path with `/` separators; required only when [`Query::needs_path`] is true.
    pub path: Option<&'a str>,
    pub is_dir: bool,
    /// Logical Size in bytes.
    pub logical: u64,
    /// Modification time in nanoseconds since the Unix epoch.
    pub modified: i64,
}

/// Everything a Query's meaning depends on besides its text.
#[derive(Clone, Debug)]
pub struct QueryContext {
    pub categories: CategoryMap,
    pub now: DateTime<Local>,
}

impl QueryContext {
    pub fn new(categories: CategoryMap, now: DateTime<Local>) -> Self {
        Self { categories, now }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryError(pub String);

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for QueryError {}

/// A parsed search expression selecting which Entries count.
///
/// Syntax (a subset of Everything's): terms separated by spaces must all match; `a|b` matches
/// either and binds tighter than a space; `!term` excludes; `"quotes"` keep spaces; `*` and `?`
/// match the whole name; a term containing `/` or `\` matches the full path.
#[derive(Clone, Debug)]
pub struct Query {
    /// Every clause must hold; a clause holds when any of its literals does.
    clauses: Vec<Vec<Literal>>,
    needs_path: bool,
}

#[derive(Clone, Debug)]
struct Literal {
    negated: bool,
    term: Term,
}

#[derive(Clone, Debug)]
enum Term {
    Text(Text),
    /// Lower-cased extensions without the dot.
    Ext(Vec<String>),
    Type(Category, Arc<CategoryMap>),
    /// Inclusive bounds on Logical Size.
    Size {
        min: u64,
        max: u64,
    },
    /// Half-open bounds on modification time, in nanoseconds since the Unix epoch.
    Modified {
        from: i64,
        until: i64,
    },
    Regex(regex::Regex),
}

#[derive(Clone, Debug)]
struct Text {
    /// Lower-cased pattern, with `\` turned into `/` when matching paths.
    pattern: String,
    /// `*`/`?` present: the pattern must match the whole name (or path).
    wildcard: bool,
    /// The pattern contains a folder separator: match against the full path.
    on_path: bool,
}

impl Query {
    pub fn parse(text: &str, context: &QueryContext) -> Result<Query, QueryError> {
        let categories = Arc::new(context.categories.clone());
        let mut clauses: Vec<Vec<Literal>> = Vec::new();
        let mut join_next = false;
        for token in tokenize(text) {
            match token {
                Token::Or => join_next = !clauses.is_empty(),
                Token::Word(word) => {
                    let literal = Literal {
                        negated: word.negated,
                        term: Term::parse(&word, &categories, context)?,
                    };
                    match clauses.last_mut() {
                        Some(clause) if join_next => clause.push(literal),
                        _ => clauses.push(vec![literal]),
                    }
                    join_next = false;
                }
            }
        }
        let needs_path = clauses.iter().flatten().any(|l| matches!(&l.term, Term::Text(t) if t.on_path));
        Ok(Query { clauses, needs_path })
    }

    /// Whether candidates must carry their full path.
    pub fn needs_path(&self) -> bool {
        self.needs_path
    }

    pub fn matches(&self, candidate: &Candidate) -> bool {
        self.clauses
            .iter()
            .all(|clause| clause.iter().any(|literal| literal.holds(candidate)))
    }

    /// Whether the candidate survives every stand-alone negated term, such as `!*.tmp`. Files
    /// inside a matching folder count only if they pass these.
    pub fn passes_exclusions(&self, candidate: &Candidate) -> bool {
        self.clauses
            .iter()
            .filter(|clause| clause.len() == 1 && clause[0].negated)
            .all(|clause| clause[0].holds(candidate))
    }
}

impl Literal {
    fn holds(&self, candidate: &Candidate) -> bool {
        self.term.matches(candidate) != self.negated
    }
}

impl Term {
    fn matches(&self, candidate: &Candidate) -> bool {
        match self {
            Term::Text(text) => text.matches(candidate),
            Term::Ext(list) => {
                !candidate.is_dir && extension(candidate.name).is_some_and(|ext| list.iter().any(|e| e.eq_ignore_ascii_case(ext)))
            }
            Term::Type(category, map) => !candidate.is_dir && map.of_name(candidate.name) == *category,
            Term::Size { min, max } => !candidate.is_dir && (*min..=*max).contains(&candidate.logical),
            Term::Modified { from, until } => !candidate.is_dir && (*from..*until).contains(&candidate.modified),
            Term::Regex(regex) => regex.is_match(candidate.name),
        }
    }

    fn parse(word: &Word, categories: &Arc<CategoryMap>, context: &QueryContext) -> Result<Term, QueryError> {
        let Some((name, value)) = word.modifier() else {
            return Ok(Term::Text(Text::new(&word.text)));
        };
        let missing = || QueryError(format!("`{name}:` needs a value"));
        match name {
            "ext" => {
                let list: Vec<String> = value
                    .split(';')
                    .map(|e| e.trim().trim_start_matches('.').to_lowercase())
                    .filter(|e| !e.is_empty())
                    .collect();
                if list.is_empty() {
                    return Err(missing());
                }
                Ok(Term::Ext(list))
            }
            "type" => {
                if value.is_empty() {
                    return Err(missing());
                }
                let category = Category::parse(value).ok_or_else(|| {
                    let known: Vec<&str> = Category::ALL.iter().map(|c| c.name()).collect();
                    QueryError(format!("unknown type `{value}`; expected one of {}", known.join(", ")))
                })?;
                Ok(Term::Type(category, categories.clone()))
            }
            "size" => {
                let (min, max) = size_bounds(value).map_err(|e| QueryError(format!("size: {e}")))?;
                Ok(Term::Size { min, max })
            }
            "dm" => {
                let (from, until) = date_bounds(value, context.now).map_err(|e| QueryError(format!("dm: {e}")))?;
                Ok(Term::Modified { from, until })
            }
            "regex" => regex::RegexBuilder::new(value)
                .case_insensitive(true)
                .build()
                .map(Term::Regex)
                .map_err(|e| QueryError(format!("regex: {e}"))),
            _ => unreachable!("Word::modifier returns only known names"),
        }
    }
}

const MODIFIERS: &[&str] = &["ext", "type", "size", "dm", "regex"];

/// A comparison value: `>x`, `>=x`, `<x`, `<=x`, `=x`, `a..b` or a bare `x`.
enum Comparison<'a> {
    Exact(&'a str),
    Above(&'a str),
    AtLeast(&'a str),
    Below(&'a str),
    AtMost(&'a str),
    Between(&'a str, &'a str),
}

impl<'a> Comparison<'a> {
    fn parse(value: &'a str) -> Result<Self, String> {
        if value.is_empty() {
            return Err("needs a value".into());
        }
        Ok(if let Some((a, b)) = value.split_once("..") {
            Comparison::Between(a, b)
        } else if let Some(v) = value.strip_prefix(">=") {
            Comparison::AtLeast(v)
        } else if let Some(v) = value.strip_prefix("<=") {
            Comparison::AtMost(v)
        } else if let Some(v) = value.strip_prefix('>') {
            Comparison::Above(v)
        } else if let Some(v) = value.strip_prefix('<') {
            Comparison::Below(v)
        } else {
            Comparison::Exact(value.strip_prefix('=').unwrap_or(value))
        })
    }
}

/// Inclusive Logical Size bounds; an empty range is `min > max`.
fn size_bounds(value: &str) -> Result<(u64, u64), String> {
    Ok(match Comparison::parse(value)? {
        Comparison::Exact(v) => {
            let n = parse_size(v)?;
            (n, n)
        }
        Comparison::Above(v) => (parse_size(v)?.saturating_add(1), u64::MAX),
        Comparison::AtLeast(v) => (parse_size(v)?, u64::MAX),
        Comparison::Below(v) => match parse_size(v)? {
            0 => (1, 0),
            n => (0, n - 1),
        },
        Comparison::AtMost(v) => (0, parse_size(v)?),
        Comparison::Between(a, b) => (parse_size(a)?, parse_size(b)?),
    })
}

/// A byte count such as `512`, `10kb`, `1.5GB`, `2g`; units are powers of 1024.
fn parse_size(text: &str) -> Result<u64, String> {
    let lower = text.trim().to_ascii_lowercase();
    let split = lower.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(lower.len());
    let (number, unit) = lower.split_at(split);
    let power = match unit {
        "" | "b" => 0,
        "k" | "kb" => 1,
        "m" | "mb" => 2,
        "g" | "gb" => 3,
        "t" | "tb" => 4,
        _ => return Err(format!("unknown unit `{unit}`; use b, kb, mb, gb or tb")),
    };
    let value: f64 = number.parse().map_err(|_| format!("`{text}` is not a size"))?;
    Ok((value * 1024f64.powi(power)).round() as u64)
}

/// Half-open modification-time bounds in Unix nanoseconds.
fn date_bounds(value: &str, now: DateTime<Local>) -> Result<(i64, i64), String> {
    let span = |d: &str| date_span(d, now);
    Ok(match Comparison::parse(value)? {
        Comparison::Exact(d) => span(d)?,
        Comparison::Above(d) => (span(d)?.1, i64::MAX),
        Comparison::AtLeast(d) => (span(d)?.0, i64::MAX),
        Comparison::Below(d) => (i64::MIN, span(d)?.0),
        Comparison::AtMost(d) => (i64::MIN, span(d)?.1),
        Comparison::Between(a, b) => (span(a)?.0, span(b)?.1),
    })
}

/// The local-time span `[start, end)` named by `today`, `yesterday`, `YYYY`, `YYYY-MM` or
/// `YYYY-MM-DD`, in Unix nanoseconds.
fn date_span(text: &str, now: DateTime<Local>) -> Result<(i64, i64), String> {
    let invalid = || format!("`{text}` is not a date; use YYYY, YYYY-MM, YYYY-MM-DD, today or yesterday");
    let today = now.date_naive();
    let (start, end) = match text.to_ascii_lowercase().as_str() {
        "today" => (today, today + Days::new(1)),
        "yesterday" => (today - Days::new(1), today),
        _ => {
            let parts: Vec<&str> = text.split('-').collect();
            let number = |s: &str| s.parse::<u32>().map_err(|_| invalid());
            let year = parts[0].parse::<i32>().map_err(|_| invalid())?;
            match parts[..] {
                [_] => {
                    let start = NaiveDate::from_ymd_opt(year, 1, 1).ok_or_else(invalid)?;
                    (start, start.checked_add_months(Months::new(12)).ok_or_else(invalid)?)
                }
                [_, m] => {
                    let start = NaiveDate::from_ymd_opt(year, number(m)?, 1).ok_or_else(invalid)?;
                    (start, start.checked_add_months(Months::new(1)).ok_or_else(invalid)?)
                }
                [_, m, d] => {
                    let start = NaiveDate::from_ymd_opt(year, number(m)?, number(d)?).ok_or_else(invalid)?;
                    (start, start.checked_add_days(Days::new(1)).ok_or_else(invalid)?)
                }
                _ => return Err(invalid()),
            }
        }
    };
    Ok((local_midnight_nanos(start), local_midnight_nanos(end)))
}

fn local_midnight_nanos(date: NaiveDate) -> i64 {
    let midnight = date.and_hms_opt(0, 0, 0).expect("midnight exists");
    // Midnight can be skipped by a daylight-saving change; take the first instant after it.
    let local = Local.from_local_datetime(&midnight).earliest().unwrap_or_else(|| {
        Local
            .from_local_datetime(&(midnight + chrono::Duration::hours(1)))
            .earliest()
            .expect("valid local time")
    });
    local.timestamp_nanos_opt().unwrap_or(i64::MAX)
}

impl Text {
    fn new(raw: &str) -> Text {
        let on_path = raw.contains(['/', '\\']);
        let mut pattern = raw.to_lowercase();
        if on_path {
            pattern = pattern.replace('\\', "/");
        }
        Text {
            wildcard: pattern.contains(['*', '?']),
            pattern,
            on_path,
        }
    }

    fn matches(&self, candidate: &Candidate) -> bool {
        let subject = if self.on_path {
            candidate.path.expect("Query::needs_path requires candidates to carry their path")
        } else {
            candidate.name
        };
        if self.wildcard {
            wildcard_match(&self.pattern, &subject.to_lowercase())
        } else {
            contains_ignore_case(subject, &self.pattern)
        }
    }
}

/// Case-insensitive substring search; `needle` is already lower-cased.
fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    if haystack.is_ascii() && needle.is_ascii() {
        let (h, n) = (haystack.as_bytes(), needle.as_bytes());
        return n.is_empty() || h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n));
    }
    haystack.to_lowercase().contains(needle)
}

/// Whole-string match where `*` is any run of characters and `?` exactly one.
fn wildcard_match(pattern: &str, subject: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = subject.chars().collect();
    let (mut pi, mut si) = (0, 0);
    let mut backtrack: Option<(usize, usize)> = None;
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            backtrack = Some((pi, si));
            pi += 1;
        } else if let Some((star, matched)) = backtrack {
            pi = star + 1;
            si = matched + 1;
            backtrack = Some((star, matched + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

enum Token {
    Or,
    Word(Word),
}

#[derive(Default)]
struct Word {
    negated: bool,
    text: String,
    /// Byte offset in `text` where the first quoted part began.
    quoted_from: Option<usize>,
    /// The word's byte range in the Query text, including `!` and quotes.
    start: usize,
    end: usize,
}

impl Word {
    /// `(modifier, value)` when the word starts with an unquoted known modifier such as `ext:`.
    fn modifier(&self) -> Option<(&'static str, &str)> {
        let colon = self.text.find(':')?;
        if self.quoted_from.is_some_and(|q| q <= colon) {
            return None;
        }
        let name = MODIFIERS.iter().find(|m| m.eq_ignore_ascii_case(&self.text[..colon]))?;
        Some((name, &self.text[colon + 1..]))
    }
}

fn tokenize(text: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut current: Option<Word> = None;
    let mut in_quotes = false;
    let start = |at: usize| {
        move || Word {
            start: at,
            ..Word::default()
        }
    };
    for (at, c) in text.char_indices() {
        let end = at + c.len_utf8();
        match c {
            '"' => {
                in_quotes = !in_quotes;
                let word = current.get_or_insert_with(start(at));
                word.quoted_from.get_or_insert(word.text.len());
                word.end = end;
            }
            c if in_quotes => {
                let word = current.get_or_insert_with(start(at));
                word.text.push(c);
                word.end = end;
            }
            c if c.is_whitespace() => tokens.extend(current.take().map(Token::Word)),
            '|' => {
                tokens.extend(current.take().map(Token::Word));
                tokens.push(Token::Or);
            }
            '!' if current.as_ref().is_none_or(|w| w.text.is_empty() && !w.negated) => {
                let word = current.get_or_insert_with(start(at));
                word.negated = true;
                word.end = end;
            }
            c => {
                let word = current.get_or_insert_with(start(at));
                word.text.push(c);
                word.end = end;
            }
        }
    }
    tokens.extend(current.map(Token::Word));
    tokens
}

/// One clause of a Query's text as written: the words that `|` joins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClauseSource<'a> {
    /// The clause exactly as typed, from the start of its first word to the end of its last.
    pub source: &'a str,
    pub words: Vec<WordSource>,
}

/// One word of a [`ClauseSource`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WordSource {
    pub negated: bool,
    /// The known modifier the word starts with: `ext`, `type`, `size`, `dm` or `regex`.
    pub modifier: Option<&'static str>,
    /// The text after the modifier's colon, or the whole word; quotes removed.
    pub value: String,
}

/// Splits Query text into its clauses the way [`Query::parse`] groups them, keeping each
/// clause's source so a tool that edits some clauses can pass the others through untouched.
pub fn clauses(text: &str) -> Vec<ClauseSource<'_>> {
    let mut clauses: Vec<(usize, usize, Vec<WordSource>)> = Vec::new();
    let mut join_next = false;
    for token in tokenize(text) {
        match token {
            Token::Or => join_next = !clauses.is_empty(),
            Token::Word(word) => {
                let (modifier, value) = match word.modifier() {
                    Some((name, value)) => (Some(name), value.to_owned()),
                    None => (None, word.text.clone()),
                };
                let source = WordSource {
                    negated: word.negated,
                    modifier,
                    value,
                };
                match clauses.last_mut() {
                    Some((_, end, words)) if join_next => {
                        *end = word.end;
                        words.push(source);
                    }
                    _ => clauses.push((word.start, word.end, vec![source])),
                }
                join_next = false;
            }
        }
    }
    clauses
        .into_iter()
        .map(|(start, end, words)| ClauseSource {
            source: &text[start..end],
            words,
        })
        .collect()
}
