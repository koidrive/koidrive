// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::collections::btree_map;
use std::fmt;
use std::ops::Deref;
use std::path::{Path, PathBuf};

use crate::{Entry, Mount};

/// The entries of a [`Snapshot`](crate::Snapshot) by relative path, read as a map, with the mount points among them.
#[derive(Clone, Default)]
pub struct Entries {
    all: BTreeMap<PathBuf, Entry>,
    mounts: BTreeMap<PathBuf, Mount>,
}

impl Entries {
    /// Every entry that is a mount point, with the volume mounted there.
    pub fn mounts(&self) -> &BTreeMap<PathBuf, Mount> {
        &self.mounts
    }

    pub fn insert(&mut self, path: PathBuf, entry: Entry) -> Option<Entry> {
        self.index(&path, &entry);
        self.all.insert(path, entry)
    }

    pub fn remove(&mut self, path: &Path) -> Option<Entry> {
        self.mounts.remove(path);
        self.all.remove(path)
    }

    /// Moves every entry of `other` in, replacing the entries at the same paths.
    pub fn append(&mut self, other: &mut BTreeMap<PathBuf, Entry>) {
        for (path, entry) in other.iter() {
            self.index(path, entry);
        }
        self.all.append(other);
    }

    fn index(&mut self, path: &Path, entry: &Entry) {
        match entry.mount {
            Some(mount) => self.mounts.insert(path.to_path_buf(), mount),
            None => self.mounts.remove(path),
        };
    }
}

impl Deref for Entries {
    type Target = BTreeMap<PathBuf, Entry>;

    fn deref(&self) -> &Self::Target {
        &self.all
    }
}

impl PartialEq for Entries {
    fn eq(&self, other: &Self) -> bool {
        self.all == other.all
    }
}

impl Eq for Entries {}

impl fmt::Debug for Entries {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.all.fmt(f)
    }
}

impl Extend<(PathBuf, Entry)> for Entries {
    fn extend<I: IntoIterator<Item = (PathBuf, Entry)>>(&mut self, entries: I) {
        for (path, entry) in entries {
            self.insert(path, entry);
        }
    }
}

impl FromIterator<(PathBuf, Entry)> for Entries {
    fn from_iter<I: IntoIterator<Item = (PathBuf, Entry)>>(entries: I) -> Self {
        let mut built = Self::default();
        built.extend(entries);
        built
    }
}

impl From<BTreeMap<PathBuf, Entry>> for Entries {
    fn from(mut entries: BTreeMap<PathBuf, Entry>) -> Self {
        let mut built = Self::default();
        built.append(&mut entries);
        built
    }
}

impl IntoIterator for Entries {
    type Item = (PathBuf, Entry);
    type IntoIter = btree_map::IntoIter<PathBuf, Entry>;

    fn into_iter(self) -> Self::IntoIter {
        self.all.into_iter()
    }
}

impl<'a> IntoIterator for &'a Entries {
    type Item = (&'a PathBuf, &'a Entry);
    type IntoIter = btree_map::Iter<'a, PathBuf, Entry>;

    fn into_iter(self) -> Self::IntoIter {
        self.all.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EntryKind, FileId};

    fn dir(ino: u128, mount: Option<Mount>) -> Entry {
        Entry {
            kind: EntryKind::Dir,
            id: FileId {
                ino,
                birth_ns: None,
            },
            size: 0,
            mtime_ns: 0,
            executable: false,
            linked: false,
            mount,
        }
    }

    fn mounts_of(entries: &Entries) -> BTreeMap<PathBuf, Mount> {
        (entries.iter())
            .filter_map(|(path, entry)| Some((path.clone(), entry.mount?)))
            .collect()
    }

    #[test]
    fn the_mounts_follow_every_write_to_the_entries() {
        let mut entries: Entries = [
            ("a".into(), dir(1, Some(Mount::Unidentified))),
            ("b".into(), dir(2, None)),
        ]
        .into_iter()
        .collect();
        assert_eq!(entries.mounts(), &mounts_of(&entries));
        assert_eq!(entries.mounts().len(), 1);

        entries.insert("a".into(), dir(1, None));
        entries.insert("b/c".into(), dir(3, Some(Mount::Unidentified)));
        assert_eq!(entries.mounts(), &mounts_of(&entries));

        entries.append(&mut BTreeMap::from([
            ("b/c".into(), dir(3, None)),
            ("d".into(), dir(4, Some(Mount::Unidentified))),
        ]));
        assert_eq!(entries.mounts(), &mounts_of(&entries));

        entries.remove(Path::new("d"));
        entries.extend([("e".into(), dir(5, Some(Mount::Unidentified)))]);
        assert_eq!(entries.mounts(), &mounts_of(&entries));
        assert_eq!(
            entries.mounts().keys().collect::<Vec<_>>(),
            [Path::new("e")]
        );
    }
}
