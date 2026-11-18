// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::marker::PhantomData;

use unicode_normalization::UnicodeNormalization;

use crate::NameKey;

pub(crate) fn key<K: NameKey>(name: &str) -> String {
    K::key(name).nfc().collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LineError {
    NotUtf8,
    ContainsNul,
    UnclosedBracket,
    UnknownClass(String),
    TrailingBackslash,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidLine {
    pub line: u32,
    pub error: LineError,
}

/// The parsed rules of one `.koiignore` file.
pub struct RuleFile<K> {
    rules: Vec<Rule>,
    invalid: Vec<InvalidLine>,
    source: Vec<u8>,
    key: PhantomData<fn() -> K>,
}

impl<K> Clone for RuleFile<K> {
    fn clone(&self) -> Self {
        Self {
            rules: self.rules.clone(),
            invalid: self.invalid.clone(),
            source: self.source.clone(),
            key: PhantomData,
        }
    }
}

impl<K> PartialEq for RuleFile<K> {
    fn eq(&self, other: &Self) -> bool {
        self.rules == other.rules && self.invalid == other.invalid
    }
}

impl<K> fmt::Debug for RuleFile<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RuleFile")
            .field("rules", &self.rules)
            .field("invalid", &self.invalid)
            .finish()
    }
}

impl<K> Default for RuleFile<K> {
    fn default() -> Self {
        Self {
            rules: Vec::new(),
            invalid: Vec::new(),
            source: Vec::new(),
            key: PhantomData,
        }
    }
}

impl<K: NameKey> RuleFile<K> {
    /// Parses a rules file. Never fails: a line that cannot be used is listed in
    /// [`RuleFile::invalid_lines`] and skipped.
    ///
    /// A UTF-8 byte order mark at the start is skipped. Lines end at `\n`, with a `\r` before it
    /// removed. Blank lines and lines starting with `#` are skipped. Trailing spaces are removed
    /// unless escaped with `\`.
    pub fn parse(bytes: &[u8]) -> Self {
        let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
        let mut lines: Vec<&[u8]> = bytes.split(|&b| b == b'\n').collect();
        if lines.last().is_some_and(|last| last.is_empty()) {
            lines.pop();
        }
        let mut file = Self {
            source: bytes.to_vec(),
            ..Self::default()
        };
        for (index, line) in lines.into_iter().enumerate() {
            let number = u32::try_from(index + 1).unwrap_or(u32::MAX);
            if line.is_empty() || line[0] == b'#' {
                continue;
            }
            let line = trim_trailing_spaces(line.strip_suffix(b"\r").unwrap_or(line));
            match compile::<K>(line, number) {
                Ok(Some(rule)) => file.rules.push(rule),
                Ok(None) => {}
                Err(error) => file.invalid.push(InvalidLine {
                    line: number,
                    error,
                }),
            }
        }
        file
    }

    /// The lines that are not used, in file order.
    pub fn invalid_lines(&self) -> &[InvalidLine] {
        &self.invalid
    }

    /// The bytes the rules were parsed from, after the byte order mark.
    pub fn source(&self) -> &[u8] {
        &self.source
    }

    pub(crate) fn last_match(
        &self,
        path: &[char],
        basename: &[char],
        is_dir: bool,
    ) -> Option<&Rule> {
        self.rules
            .iter()
            .rev()
            .find(|rule| rule.matches::<K>(path, basename, is_dir))
    }
}

/// Git's `trim_trailing_spaces`: drops a run of spaces at the end unless the first of them is
/// escaped. A line ending in a lone `\` is left as it is.
fn trim_trailing_spaces(line: &[u8]) -> &[u8] {
    let mut last_space = None;
    let mut i = 0;
    while i < line.len() {
        match line[i] {
            b' ' => {
                last_space.get_or_insert(i);
            }
            b'\\' => {
                i += 1;
                if i == line.len() {
                    return line;
                }
                last_space = None;
            }
            _ => last_space = None,
        }
        i += 1;
    }
    &line[..last_space.unwrap_or(line.len())]
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Rule {
    pub(crate) line: u32,
    pub(crate) negated: bool,
    dir_only: bool,
    pathname: bool,
    tokens: Vec<Token>,
}

impl Rule {
    fn matches<K: NameKey>(&self, path: &[char], basename: &[char], is_dir: bool) -> bool {
        if self.dir_only && !is_dir {
            return false;
        }
        let text = if self.pathname { path } else { basename };
        wildmatch::<K>(&self.tokens, text, self.pathname)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    Literal(char),
    Slash,
    AnyChar,
    Star,
    /// `**` at the start or end of the pattern or between slashes. Before a slash that is not
    /// escaped it also matches zero directories with that slash (`or_nothing`), as in git's
    /// `wildmatch`: `**/b` matches `b`, and `**\/b` needs a directory.
    DoubleStar {
        or_nothing: bool,
    },
    Class(Class),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Class {
    negated: bool,
    items: Vec<ClassItem>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ClassItem {
    Char(char),
    Range(char, char),
    Posix(Posix),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Posix {
    Alnum,
    Alpha,
    Blank,
    Cntrl,
    Digit,
    Graph,
    Lower,
    Print,
    Punct,
    Space,
    Upper,
    Xdigit,
}

impl Posix {
    fn named(name: &str) -> Option<Self> {
        Some(match name {
            "alnum" => Self::Alnum,
            "alpha" => Self::Alpha,
            "blank" => Self::Blank,
            "cntrl" => Self::Cntrl,
            "digit" => Self::Digit,
            "graph" => Self::Graph,
            "lower" => Self::Lower,
            "print" => Self::Print,
            "punct" => Self::Punct,
            "space" => Self::Space,
            "upper" => Self::Upper,
            "xdigit" => Self::Xdigit,
            _ => return None,
        })
    }

    /// Git's ASCII classes. Under folding, `upper` also matches a lowercase letter whose
    /// uppercase form has it as key, as git does with `core.ignorecase`.
    fn matches<K: NameKey>(self, c: char) -> bool {
        match self {
            Self::Alnum => c.is_ascii_alphanumeric(),
            Self::Alpha => c.is_ascii_alphabetic(),
            Self::Blank => c == ' ' || c == '\t',
            Self::Cntrl => c.is_ascii_control(),
            Self::Digit => c.is_ascii_digit(),
            Self::Graph => c.is_ascii_graphic(),
            Self::Lower => c.is_ascii_lowercase(),
            Self::Print => matches!(c, ' '..='~'),
            Self::Punct => c.is_ascii_punctuation(),
            Self::Space => matches!(c, ' ' | '\t' | '\n' | '\r'),
            Self::Upper => {
                c.is_ascii_uppercase()
                    || (c.is_ascii_lowercase() && folds_to::<K>(c.to_ascii_uppercase(), c))
            }
            Self::Xdigit => c.is_ascii_hexdigit(),
        }
    }
}

fn folds_to<K: NameKey>(upper: char, c: char) -> bool {
    single_key::<K>(upper) == Some(c)
}

fn single_key<K: NameKey>(c: char) -> Option<char> {
    let folded = key::<K>(c.encode_utf8(&mut [0; 4]));
    let mut chars = folded.chars();
    chars.next().filter(|_| chars.next().is_none())
}

impl Class {
    fn matches<K: NameKey>(&self, c: char) -> bool {
        let found = self.items.iter().any(|item| match *item {
            ClassItem::Char(member) => member == c,
            ClassItem::Range(low, high) => {
                (low..=high).contains(&c) || {
                    let mut upper = c.to_uppercase();
                    match (upper.next(), upper.next()) {
                        (Some(u), None) if u != c => {
                            (low..=high).contains(&u) && folds_to::<K>(u, c)
                        }
                        _ => false,
                    }
                }
            }
            ClassItem::Posix(class) => class.matches::<K>(c),
        });
        found != self.negated
    }
}

/// Compiles one trimmed line, as git's `parse_path_pattern` does. `Ok(None)` is a pattern
/// that matches nothing and is not an error in git either, such as `!` or `/`.
fn compile<K: NameKey>(line: &[u8], number: u32) -> Result<Option<Rule>, LineError> {
    if line.is_empty() {
        return Ok(None);
    }
    if line.contains(&0) {
        return Err(LineError::ContainsNul);
    }
    let text: String = std::str::from_utf8(line)
        .map_err(|_| LineError::NotUtf8)?
        .nfc()
        .collect();
    let (negated, body) = match text.strip_prefix('!') {
        Some(rest) => (true, rest),
        None => (false, text.as_str()),
    };
    let (dir_only, body) = match body.strip_suffix('/') {
        Some(rest) => (true, rest),
        None => (false, body),
    };
    let pathname = body.contains('/');
    let body = if pathname {
        body.strip_prefix('/').unwrap_or(body)
    } else {
        body
    };
    let tokens = tokenize::<K>(body)?;
    if tokens.is_empty() {
        return Ok(None);
    }
    Ok(Some(Rule {
        line: number,
        negated,
        dir_only,
        pathname,
        tokens,
    }))
}

fn tokenize<K: NameKey>(body: &str) -> Result<Vec<Token>, LineError> {
    let chars: Vec<char> = body.chars().collect();
    let mut raw = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => {
                let escaped = *chars.get(i + 1).ok_or(LineError::TrailingBackslash)?;
                raw.push(if escaped == '/' {
                    Token::Slash
                } else {
                    Token::Literal(escaped)
                });
                i += 2;
            }
            '*' => {
                let start = i;
                while chars.get(i) == Some(&'*') {
                    i += 1;
                }
                let after_slash = matches!(raw.last(), None | Some(Token::Slash));
                let before_slash = match chars.get(i) {
                    None | Some('/') => true,
                    Some('\\') => chars.get(i + 1) == Some(&'/'),
                    Some(_) => false,
                };
                raw.push(if i - start >= 2 && after_slash && before_slash {
                    Token::DoubleStar {
                        or_nothing: chars.get(i) == Some(&'/'),
                    }
                } else {
                    Token::Star
                });
            }
            '?' => {
                raw.push(Token::AnyChar);
                i += 1;
            }
            '[' => {
                let (class, next) = parse_class::<K>(&chars, i)?;
                raw.push(Token::Class(class));
                i = next;
            }
            '/' => {
                raw.push(Token::Slash);
                i += 1;
            }
            c => {
                raw.push(Token::Literal(c));
                i += 1;
            }
        }
    }

    let mut tokens = Vec::with_capacity(raw.len());
    let mut run = String::new();
    for token in raw {
        if let Token::Literal(c) = token {
            run.push(c);
            continue;
        }
        tokens.extend(key::<K>(&run).chars().map(Token::Literal));
        run.clear();
        tokens.push(token);
    }
    tokens.extend(key::<K>(&run).chars().map(Token::Literal));
    Ok(tokens)
}

fn parse_class<K: NameKey>(chars: &[char], open: usize) -> Result<(Class, usize), LineError> {
    let mut j = open + 1;
    let negated = matches!(chars.get(j), Some('!' | '^'));
    if negated {
        j += 1;
    }
    let mut items = Vec::new();
    let mut prev: Option<char> = None;
    let mut first = true;
    loop {
        let c = *chars.get(j).ok_or(LineError::UnclosedBracket)?;
        if c == ']' && !first {
            j += 1;
            break;
        }
        first = false;
        if c == '\\' {
            let escaped = *chars.get(j + 1).ok_or(LineError::UnclosedBracket)?;
            items.push(ClassItem::Char(escaped));
            prev = Some(escaped);
            j += 2;
        } else if c == '-' && prev.is_some() && chars.get(j + 1).is_some_and(|&next| next != ']') {
            let mut high = chars[j + 1];
            j += 2;
            if high == '\\' {
                high = *chars.get(j).ok_or(LineError::UnclosedBracket)?;
                j += 1;
            }
            items.push(ClassItem::Range(prev.take().unwrap_or(high), high));
        } else if c == '[' && chars.get(j + 1) == Some(&':') {
            let start = j + 2;
            let close = chars[start..]
                .iter()
                .position(|&c| c == ']')
                .map(|offset| start + offset)
                .ok_or(LineError::UnclosedBracket)?;
            if close == start || chars[close - 1] != ':' {
                items.push(ClassItem::Char('['));
                prev = Some('[');
                j += 1;
                continue;
            }
            let name: String = chars[start..close - 1].iter().collect();
            let class = Posix::named(&name).ok_or(LineError::UnknownClass(name))?;
            items.push(ClassItem::Posix(class));
            prev = None;
            j = close + 1;
        } else {
            items.push(ClassItem::Char(c));
            prev = Some(c);
            j += 1;
        }
    }

    let items = items
        .into_iter()
        .filter_map(|item| match item {
            ClassItem::Char(c) => single_key::<K>(c).map(ClassItem::Char),
            other => Some(other),
        })
        .collect();
    Ok((Class { negated, items }, j))
}

fn wildmatch<K: NameKey>(tokens: &[Token], text: &[char], pathname: bool) -> bool {
    let n = tokens.len();
    let mut current = vec![false; 2 * n + 1];
    let mut next = vec![false; 2 * n + 1];
    let mut pending = Vec::new();
    enter(tokens, &mut current, 0, &mut pending);
    for &c in text {
        next.fill(false);
        let mut alive = false;
        let slash_blocked = pathname && c == '/';
        for (i, token) in tokens.iter().enumerate() {
            if current[i] {
                let advance = match token {
                    Token::Literal(literal) => *literal == c,
                    Token::Slash => c == '/',
                    Token::AnyChar => !slash_blocked,
                    Token::Class(class) => !slash_blocked && class.matches::<K>(c),
                    Token::Star | Token::DoubleStar { .. } => false,
                };
                if advance {
                    enter(tokens, &mut next, i + 1, &mut pending);
                    alive = true;
                }
            }
            if current[n + 1 + i] && !(slash_blocked && *token == Token::Star) {
                next[n + 1 + i] = true;
                enter(tokens, &mut next, i + 1, &mut pending);
                alive = true;
            }
        }
        if !alive {
            return false;
        }
        std::mem::swap(&mut current, &mut next);
    }
    current[n]
}

fn enter(tokens: &[Token], states: &mut [bool], start: usize, pending: &mut Vec<usize>) {
    let n = tokens.len();
    pending.push(start);
    while let Some(i) = pending.pop() {
        if states[i] {
            continue;
        }
        states[i] = true;
        match tokens.get(i) {
            Some(Token::Star) => {
                states[n + 1 + i] = true;
                pending.push(i + 1);
            }
            Some(Token::DoubleStar { or_nothing }) => {
                states[n + 1 + i] = true;
                pending.push(i + 1);
                if *or_nothing {
                    pending.push(i + 2);
                }
            }
            _ => {}
        }
    }
}
