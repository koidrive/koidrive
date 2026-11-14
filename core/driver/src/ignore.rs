// SPDX-License-Identifier: Apache-2.0

//! The ignore rules of a linked folder as the driver holds them (design §5.10). The matcher is the engine's
//! ([`koidrive_engine::ignore`]): the built-in exclusions, this device's `[sync]` list and the `.koiignore` files of
//! the folder. [`matcher`] builds the part that is the same for every folder of the profile, and `scan_rules` gives
//! `core/fs` the form its scans and watchers apply, which also reads the `.koiignore` files. The engine's trees take the
//! same list through [`local_rules`] and the same files with each observation, so a scan leaves out exactly the paths
//! the engine excludes.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::sync::Arc;

use koidrive_engine::ignore::{LocalRules, Matcher as Rules, RuleFile};
use koidrive_engine::names::name_key;
use koidrive_fs::{EntryKind, Ignore, Ignored};
use koidrive_ignore::{Kind, LocalRulesError, Reason, Status};

use crate::config::SyncRules;

pub trait Matcher: Send + Sync {
    fn is_excluded(&self, path: &Path, dir: bool) -> bool;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Unfiltered;

impl Matcher for Unfiltered {
    fn is_excluded(&self, _path: &Path, _dir: bool) -> bool {
        false
    }
}

fn names(path: &Path) -> Vec<String> {
    path.iter()
        .map(|name| name.to_string_lossy().into_owned())
        .collect()
}

fn status(rules: &Rules, path: &Path, dir: bool) -> Status {
    let kind = if dir { Kind::Dir } else { Kind::File };
    rules.status(&names(path), kind)
}

struct Device(Rules);

impl Matcher for Device {
    fn is_excluded(&self, path: &Path, dir: bool) -> bool {
        status(&self.0, path, dir) != Status::Included
    }
}

pub fn local_rules(sync: &SyncRules) -> Result<LocalRules, LocalRulesError> {
    let include: Vec<String> = sync
        .include
        .iter()
        .map(|path| {
            let dir = if path.ends_with('/') { "/" } else { "" };
            format!("{}{dir}", path.trim_matches('/'))
        })
        .collect();
    LocalRules::new(&include, &sync.exclude)
}

pub fn matcher(sync: &SyncRules) -> Result<Arc<dyn Matcher>, LocalRulesError> {
    Ok(Arc::new(Device(Rules::new(local_rules(sync)?))))
}

/// Whether the root holds entries other than the engine's built-in exclusions, before any user rules apply.
pub fn has_sync_entries(root: &Path) -> std::io::Result<bool> {
    let matcher = matcher(&SyncRules::default()).map_err(std::io::Error::other)?;
    std::fs::read_dir(root)?.try_fold(false, |found, entry| {
        let entry = entry?;
        let kind = entry.file_type()?;
        Ok(found || !matcher.is_excluded(Path::new(&entry.file_name()), kind.is_dir()))
    })
}

struct Scanned {
    device: Arc<dyn Matcher>,
    files: Rules,
}

impl Ignore for Scanned {
    fn set_rules(&mut self, dir: &Path, rules: Option<&[u8]>) {
        let rules = rules.map(RuleFile::parse);
        self.files.set_rules(&names(dir), rules, std::iter::empty());
    }

    fn status(&self, path: &Path, kind: EntryKind) -> Ignored {
        let dir = kind == EntryKind::Dir;
        match status(&self.files, path, dir) {
            Status::Excluded(Reason::BuiltIn) => Ignored::BuiltIn,
            Status::Excluded(_) => Ignored::Rule,
            Status::Included if self.device.is_excluded(path, dir) => Ignored::Rule,
            Status::Included => Ignored::No,
        }
    }

    fn key(&self, name: &OsStr) -> OsString {
        name_key(&name.to_string_lossy()).into()
    }
}

pub(crate) fn scan_rules(device: &Arc<dyn Matcher>) -> koidrive_fs::Rules {
    koidrive_fs::Rules::new(Box::new(Scanned {
        device: device.clone(),
        files: Rules::new(LocalRules::default()),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_entries_use_the_engine_built_ins_and_name_keys() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(!has_sync_entries(root).unwrap());
        for name in [".ds_store", "THUMBS.DB", "Desktop.INI", "Icon\r"] {
            std::fs::write(root.join(name), b"metadata").unwrap();
            assert!(!has_sync_entries(root).unwrap(), "{name:?}");
        }
        std::fs::create_dir(root.join(".KOIDRIVE")).unwrap();
        std::fs::write(root.join(".KOIDRIVE/local"), b"state").unwrap();
        assert!(!has_sync_entries(root).unwrap());
        std::fs::write(root.join("report.docx"), b"draft").unwrap();
        assert!(has_sync_entries(root).unwrap());
        std::fs::remove_file(root.join("report.docx")).unwrap();
        std::fs::create_dir(root.join("Documents")).unwrap();
        assert!(has_sync_entries(root).unwrap());
    }

    #[test]
    fn sync_entries_do_not_apply_unreviewed_ignore_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".koiignore"), b"*\n").unwrap();
        std::fs::write(dir.path().join("report.docx"), b"draft").unwrap();
        assert!(has_sync_entries(dir.path()).unwrap());
    }

    #[test]
    fn sync_entries_propagate_directory_errors() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert_eq!(
            has_sync_entries(&root.join("missing")).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        std::fs::write(root.join("file"), b"not a directory").unwrap();
        assert!(has_sync_entries(&root.join("file")).is_err());
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = has_sync_entries(root);
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }

    fn sync(include: &[&str], exclude: &[&str]) -> SyncRules {
        SyncRules {
            include: include.iter().map(|path| (*path).to_owned()).collect(),
            exclude: exclude.iter().map(|line| (*line).to_owned()).collect(),
        }
    }

    #[test]
    fn the_platform_files_and_the_state_folder_are_excluded_without_any_configuration() {
        let matcher = matcher(&SyncRules::default()).unwrap();
        for name in [
            ".DS_Store",
            "Thumbs.db",
            "desktop.ini",
            "Icon\r",
            "docs/.DS_Store",
        ] {
            assert!(matcher.is_excluded(Path::new(name), false), "{name:?}");
        }
        assert!(matcher.is_excluded(Path::new(".koidrive"), true));
        assert!(!matcher.is_excluded(Path::new("docs/.koidrive"), true));
        assert!(!matcher.is_excluded(Path::new("docs/notes.txt"), false));
    }

    #[test]
    fn an_include_list_leaves_out_everything_else_and_an_exclude_pattern_narrows_it() {
        let matcher = matcher(&sync(&["/Work//", "Notes/todo.txt"], &["*.tmp"])).unwrap();
        assert!(!matcher.is_excluded(Path::new("Work"), true));
        assert!(!matcher.is_excluded(Path::new("Work/plan.txt"), false));
        assert!(matcher.is_excluded(Path::new("Work/plan.tmp"), false));
        assert!(
            matcher.is_excluded(Path::new("Work"), false),
            "the entry names a folder"
        );
        assert!(
            !matcher.is_excluded(Path::new("Notes"), true),
            "a folder on the way stays"
        );
        assert!(!matcher.is_excluded(Path::new("Notes/todo.txt"), false));
        assert!(matcher.is_excluded(Path::new("Notes/other.txt"), false));
        assert!(matcher.is_excluded(Path::new("Photos"), true));
        assert!(!matcher.is_excluded(Path::new("Notes/.koiignore"), false));
    }

    #[test]
    fn a_scan_asks_the_device_list_the_built_in_exclusions_and_the_rules_files_it_read() {
        let device = matcher(&sync(&[], &["/private/"])).unwrap();
        let mut scanned = Scanned {
            device,
            files: Rules::new(LocalRules::default()),
        };
        let status = |scanned: &Scanned, path: &str, kind| scanned.status(Path::new(path), kind);
        assert_eq!(status(&scanned, "private", EntryKind::Dir), Ignored::Rule);
        assert_eq!(
            status(&scanned, "a/Thumbs.db", EntryKind::File),
            Ignored::BuiltIn
        );
        assert_eq!(status(&scanned, "a/b.log", EntryKind::File), Ignored::No);

        scanned.set_rules(Path::new("a"), Some(b"*.log\n"));
        assert_eq!(status(&scanned, "a/b.log", EntryKind::File), Ignored::Rule);
        assert_eq!(status(&scanned, "b.log", EntryKind::File), Ignored::No);
        scanned.set_rules(Path::new("a"), None);
        assert_eq!(status(&scanned, "a/b.log", EntryKind::File), Ignored::No);
    }
}
