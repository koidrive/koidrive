// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::fmt;

use crate::pattern::{LineError, RuleFile, key};
use crate::{NameKey, RULES_FILE};

pub const PLATFORM_FILES: [&str; 4] = [".DS_Store", "Thumbs.db", "desktop.ini", "Icon\r"];
const STATE_DIR: &str = ".koidrive";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Included,
    Excluded(Reason),
}

/// What excluded a path. For a path under an excluded directory, what excluded the topmost
/// excluded ancestor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// `.koidrive` at the root or a platform file. Nothing can include it.
    BuiltIn,
    /// The device-local include or exclude list.
    Local,
    /// Line `line` of the `.koiignore` file in the path's ancestor at `depth` (0 is the root).
    Rule { depth: usize, line: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Excluded { path: Vec<String>, reason: Reason },
    Included { path: Vec<String> },
}

/// Why a device-local list was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalRulesError {
    /// An include entry is empty, or has an empty, `.` or `..` component.
    Include { index: usize, entry: String },
    /// An exclude entry holds a line break, or is a line a `.koiignore` file could not use.
    Exclude {
        index: usize,
        entry: String,
        error: Option<LineError>,
    },
}

impl fmt::Display for LocalRulesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Include { index, entry } => {
                write!(
                    f,
                    "include entry {index} ({entry:?}) is not a relative path"
                )
            }
            Self::Exclude {
                index,
                entry,
                error: None,
            } => write!(f, "exclude entry {index} ({entry:?}) holds a line break"),
            Self::Exclude {
                index,
                entry,
                error: Some(error),
            } => write!(
                f,
                "exclude entry {index} ({entry:?}) is not a valid pattern: {error:?}"
            ),
        }
    }
}

impl std::error::Error for LocalRulesError {}

/// This device's include and exclude lists. They are never synced; see the crate documentation
/// for how they combine with `.koiignore` files.
pub struct LocalRules<K> {
    include: Vec<IncludePath>,
    exclude: RuleFile<K>,
}

impl<K> Clone for LocalRules<K> {
    fn clone(&self) -> Self {
        Self {
            include: self.include.clone(),
            exclude: self.exclude.clone(),
        }
    }
}

impl<K> PartialEq for LocalRules<K> {
    fn eq(&self, other: &Self) -> bool {
        self.include == other.include && self.exclude == other.exclude
    }
}

impl<K> fmt::Debug for LocalRules<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalRules")
            .field("include", &self.include)
            .field("exclude", &self.exclude)
            .finish()
    }
}

impl<K> Default for LocalRules<K> {
    fn default() -> Self {
        Self {
            include: Vec::new(),
            exclude: RuleFile::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct IncludePath {
    keys: Vec<String>,
    dir_only: bool,
}

impl<K: NameKey> LocalRules<K> {
    /// `include` holds paths relative to the root, with an optional leading `/` and an optional
    /// trailing `/` that limits the entry to a directory. `exclude` holds one `.koiignore` line
    /// per entry, relative to the root.
    pub fn new<I: AsRef<str>, E: AsRef<str>>(
        include: &[I],
        exclude: &[E],
    ) -> Result<Self, LocalRulesError> {
        let include = include
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let entry = entry.as_ref();
                let path = entry.strip_prefix('/').unwrap_or(entry);
                let (dir_only, path) = match path.strip_suffix('/') {
                    Some(path) => (true, path),
                    None => (false, path),
                };
                let components: Vec<&str> = path.split('/').collect();
                if components
                    .iter()
                    .any(|c| c.is_empty() || *c == "." || *c == "..")
                {
                    return Err(LocalRulesError::Include {
                        index,
                        entry: entry.to_owned(),
                    });
                }
                Ok(IncludePath {
                    keys: components.into_iter().map(key::<K>).collect(),
                    dir_only,
                })
            })
            .collect::<Result<_, _>>()?;

        let mut joined = Vec::new();
        for (index, entry) in exclude.iter().enumerate() {
            let entry = entry.as_ref();
            if entry.contains(['\n', '\r']) {
                return Err(LocalRulesError::Exclude {
                    index,
                    entry: entry.to_owned(),
                    error: None,
                });
            }
            joined.extend_from_slice(entry.as_bytes());
            joined.push(b'\n');
        }
        let rules = RuleFile::parse(&joined);
        if let Some(invalid) = rules.invalid_lines().first() {
            let index = invalid.line as usize - 1;
            return Err(LocalRulesError::Exclude {
                index,
                entry: exclude[index].as_ref().to_owned(),
                error: Some(invalid.error.clone()),
            });
        }
        Ok(Self {
            include,
            exclude: rules,
        })
    }
}

/// The rules in force on this device: the built-in exclusions, the device-local list and the
/// `.koiignore` files known so far, by directory.
pub struct Matcher<K> {
    local: LocalRules<K>,
    files: HashMap<Vec<String>, RuleFile<K>>,
    platform_files: Vec<String>,
    state_dir: String,
    rules_file: String,
}

impl<K> Clone for Matcher<K> {
    fn clone(&self) -> Self {
        Self {
            local: self.local.clone(),
            files: self.files.clone(),
            platform_files: self.platform_files.clone(),
            state_dir: self.state_dir.clone(),
            rules_file: self.rules_file.clone(),
        }
    }
}

/// Two matchers are equal when they hold the same device-local list and the same rules files.
impl<K> PartialEq for Matcher<K> {
    fn eq(&self, other: &Self) -> bool {
        self.local == other.local && self.files == other.files
    }
}

impl<K> fmt::Debug for Matcher<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Matcher")
            .field("local", &self.local)
            .field("files", &self.files)
            .finish_non_exhaustive()
    }
}

impl<K: NameKey> Matcher<K> {
    pub fn new(local: LocalRules<K>) -> Self {
        Self {
            local,
            files: HashMap::new(),
            platform_files: PLATFORM_FILES.iter().map(|name| key::<K>(name)).collect(),
            state_dir: key::<K>(STATE_DIR),
            rules_file: key::<K>(RULES_FILE),
        }
    }

    /// The status of a path relative to the root. The root itself is included.
    pub fn status<S: AsRef<str>>(&self, path: &[S], kind: Kind) -> Status {
        self.root().status_below(path, kind)
    }

    pub fn root(&self) -> Scope<'_, K> {
        let include = if self.local.include.is_empty() {
            Include::All
        } else {
            Include::Within((0..self.local.include.len()).collect())
        };
        Scope {
            matcher: self,
            dir: Vec::new(),
            text: Vec::new(),
            files: self
                .files
                .get(&Vec::new())
                .map(|f| (0, f))
                .into_iter()
                .collect(),
            include,
        }
    }

    /// Sets or removes the rules file of `dir` and returns the entries whose status changed.
    /// `entries` are the known paths, relative to the root; only those below `dir` are
    /// evaluated, since no other path depends on this file.
    pub fn set_rules<S: AsRef<str>>(
        &mut self,
        dir: &[S],
        rules: Option<RuleFile<K>>,
        entries: impl IntoIterator<Item = (Vec<String>, Kind)>,
    ) -> Vec<Change> {
        let dir_keys = keys::<K, S>(dir);
        let below: Vec<(Vec<String>, Kind)> = entries
            .into_iter()
            .filter(|(path, _)| {
                path.len() > dir_keys.len()
                    && path.iter().zip(&dir_keys).all(|(c, k)| key::<K>(c) == *k)
            })
            .collect();
        let before = self.statuses_below(dir, &below);
        match rules {
            Some(rules) => self.files.insert(dir_keys, rules),
            None => self.files.remove(&dir_keys),
        };
        let Some(before) = before else {
            return Vec::new();
        };
        let after = self.statuses_below(dir, &below).unwrap_or_default();
        changes(below, before, after)
    }

    /// The rules file of `dir`, the names from the root to it, if one is known.
    pub fn rules_file<S: AsRef<str>>(&self, dir: &[S]) -> Option<&RuleFile<K>> {
        self.files.get(&keys::<K, S>(dir))
    }

    /// Removes every rules file.
    pub fn clear_rules(&mut self) {
        self.files.clear();
    }

    /// Replaces the device-local list and returns the entries whose status changed. `entries` are
    /// all known paths, relative to the root.
    pub fn set_local(
        &mut self,
        local: LocalRules<K>,
        entries: impl IntoIterator<Item = (Vec<String>, Kind)>,
    ) -> Vec<Change> {
        let entries: Vec<(Vec<String>, Kind)> = entries.into_iter().collect();
        let root: &[&str] = &[];
        let before = self.statuses_below(root, &entries).unwrap_or_default();
        self.local = local;
        let after = self.statuses_below(root, &entries).unwrap_or_default();
        changes(entries, before, after)
    }

    fn statuses_below<S: AsRef<str>>(
        &self,
        dir: &[S],
        entries: &[(Vec<String>, Kind)],
    ) -> Option<Vec<Status>> {
        let mut scope = self.root();
        for name in dir {
            scope = scope.enter(name.as_ref()).ok()?;
        }
        Some(
            entries
                .iter()
                .map(|(path, kind)| scope.status_below(&path[dir.len()..], *kind))
                .collect(),
        )
    }
}

fn keys<K: NameKey, S: AsRef<str>>(path: &[S]) -> Vec<String> {
    path.iter().map(|name| key::<K>(name.as_ref())).collect()
}

fn changes(
    entries: Vec<(Vec<String>, Kind)>,
    before: Vec<Status>,
    after: Vec<Status>,
) -> Vec<Change> {
    entries
        .into_iter()
        .zip(before.into_iter().zip(after))
        .filter_map(|((path, _), statuses)| match statuses {
            (Status::Excluded(_), Status::Included) => Some(Change::Included { path }),
            (Status::Included, Status::Excluded(reason)) => Some(Change::Excluded { path, reason }),
            _ => None,
        })
        .collect()
}

#[derive(Clone, Debug)]
enum Include {
    All,
    Within(Vec<usize>),
}

pub struct Scope<'a, K> {
    matcher: &'a Matcher<K>,
    dir: Vec<String>,
    text: Vec<char>,
    files: Vec<(usize, &'a RuleFile<K>)>,
    include: Include,
}

impl<K> fmt::Debug for Scope<'_, K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scope")
            .field("path", &self.text.iter().collect::<String>())
            .finish_non_exhaustive()
    }
}

impl<'a, K: NameKey> Scope<'a, K> {
    pub fn status(&self, name: &str, kind: Kind) -> Status {
        self.evaluate(&key::<K>(name), kind).0
    }

    pub fn enter(&self, name: &str) -> Result<Scope<'a, K>, Reason> {
        let name = key::<K>(name);
        let (status, include) = self.evaluate(&name, Kind::Dir);
        if let Status::Excluded(reason) = status {
            return Err(reason);
        }
        let mut text = self.text.clone();
        if !self.dir.is_empty() {
            text.push('/');
        }
        text.extend(name.chars());
        let mut dir = self.dir.clone();
        dir.push(name);
        let mut files = self.files.clone();
        if let Some(file) = self.matcher.files.get(&dir) {
            files.push((dir.len(), file));
        }
        Ok(Scope {
            matcher: self.matcher,
            dir,
            text,
            files,
            include: include.unwrap_or(Include::All),
        })
    }

    fn status_below<S: AsRef<str>>(&self, path: &[S], kind: Kind) -> Status {
        let Some((last, dirs)) = path.split_last() else {
            return Status::Included;
        };
        let mut owned: Option<Scope<'a, K>> = None;
        for name in dirs {
            match owned.as_ref().unwrap_or(self).enter(name.as_ref()) {
                Ok(scope) => owned = Some(scope),
                Err(reason) => return Status::Excluded(reason),
            }
        }
        owned.as_ref().unwrap_or(self).status(last.as_ref(), kind)
    }

    fn evaluate(&self, name: &str, kind: Kind) -> (Status, Option<Include>) {
        let matcher = self.matcher;
        let depth = self.dir.len();
        if matcher.platform_files.iter().any(|f| f == name)
            || (depth == 0 && name == matcher.state_dir)
        {
            return (Status::Excluded(Reason::BuiltIn), None);
        }
        if kind == Kind::File && name == matcher.rules_file {
            return (Status::Included, None);
        }
        let Some(include) = self.include_step(name, kind) else {
            return (Status::Excluded(Reason::Local), None);
        };

        let mut text = self.text.clone();
        if depth > 0 {
            text.push('/');
        }
        text.extend(name.chars());
        let mut starts = vec![0];
        starts.extend(
            text.iter()
                .enumerate()
                .filter(|(_, c)| **c == '/')
                .map(|(i, _)| i + 1),
        );
        let basename = &text[starts[depth]..];
        let is_dir = kind == Kind::Dir;

        if matcher
            .local
            .exclude
            .last_match(&text, basename, is_dir)
            .is_some_and(|rule| !rule.negated)
        {
            return (Status::Excluded(Reason::Local), None);
        }
        for &(file_depth, file) in self.files.iter().rev() {
            if let Some(rule) = file.last_match(&text[starts[file_depth]..], basename, is_dir) {
                if rule.negated {
                    break;
                }
                let reason = Reason::Rule {
                    depth: file_depth,
                    line: rule.line,
                };
                return (Status::Excluded(reason), None);
            }
        }
        (Status::Included, Some(include))
    }

    fn include_step(&self, name: &str, kind: Kind) -> Option<Include> {
        let Include::Within(ids) = &self.include else {
            return Some(Include::All);
        };
        let depth = self.dir.len();
        let mut on_the_way = Vec::new();
        for &id in ids {
            let entry = &self.matcher.local.include[id];
            if entry.keys[depth] != name {
                continue;
            }
            if entry.keys.len() == depth + 1 {
                if !entry.dir_only || kind == Kind::Dir {
                    return Some(Include::All);
                }
            } else if kind == Kind::Dir {
                on_the_way.push(id);
            }
        }
        (!on_the_way.is_empty()).then_some(Include::Within(on_the_way))
    }
}
