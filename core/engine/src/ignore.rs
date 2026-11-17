// SPDX-License-Identifier: Apache-2.0

pub use koidrive_ignore::Source;

use crate::names::name_key;

/// The kind of rules file a file of this name is: `.koiignore` or `.gitignore`, or a name that [`name_key`] takes
/// for the same name, such as `.KOIIGNORE`.
pub fn rules_source(name: &str) -> Option<Source> {
    Source::of::<Key>(name)
}

/// Whether a file of this name is a rules file ([`rules_source`]).
pub fn is_rules_file(name: &str) -> bool {
    rules_source(name).is_some()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key;

impl koidrive_ignore::NameKey for Key {
    fn key(name: &str) -> String {
        name_key(name)
    }
}

/// The rules in force on this device: the built-in exclusions, the device-local list and the
/// rules files.
pub type Matcher = koidrive_ignore::Matcher<Key>;
/// The parsed rules of one rules file.
pub type RuleFile = koidrive_ignore::RuleFile<Key>;
/// This device's include and exclude lists, and whether it follows `.gitignore` files.
pub type LocalRules = koidrive_ignore::LocalRules<Key>;
