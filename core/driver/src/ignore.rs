// SPDX-License-Identifier: Apache-2.0

//! The ignore rules of a linked folder as the driver holds them (design §5.10). The matcher is the engine's
//! ([`koidrive_engine::ignore`]): the built-in exclusions, this device's `[sync]` list and the rules files of the
//! folder. [`matcher`] builds the part that is the same for every folder of the profile, and `scan_rules` gives
//! `core/fs` the form its scans and watchers apply, which also reads the rules files. The engine's trees take the
//! same list through [`local_rules`] and the same files with each observation, so a scan leaves out exactly the paths
//! the engine excludes.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use koidrive_engine::ignore::{LocalRules, Matcher as Rules, RuleFile};
use koidrive_engine::names::name_key;
use koidrive_fs::{EntryKind, Ignore, Ignored, Snapshot, Source};
use koidrive_ignore::{Kind, LocalRulesError, Reason, Status};

use crate::config::SyncRules;

/// Decides whether a path of a linked folder is excluded on this device whatever the folder's rules files say.
pub trait Matcher: Send + Sync {
    /// `path` is relative to the folder's root; `dir` says whether it names a folder.
    fn is_excluded(&self, path: &Path, dir: bool) -> bool;

    /// This device's lists and `gitignore` setting, which a scan applies together with the folder's rules files.
    fn local(&self) -> LocalRules {
        LocalRules::default()
    }
}

/// Excludes nothing of its own. The built-in exclusions and the rules files still apply to a scan.
#[derive(Clone, Copy, Debug, Default)]
pub struct Unfiltered;

impl Matcher for Unfiltered {
    fn is_excluded(&self, _path: &Path, _dir: bool) -> bool {
        false
    }
}

/// The names of a relative path. A name that is not UTF-8 is no name the server can hold, so how it is spelled here
/// does not matter.
fn names(path: &Path) -> Vec<String> {
    path.iter()
        .map(|name| name.to_string_lossy().into_owned())
        .collect()
}

fn status(rules: &Rules, path: &Path, dir: bool) -> Status {
    let kind = if dir { Kind::Dir } else { Kind::File };
    rules.status(&names(path), kind)
}

/// The built-in exclusions and this device's list.
struct Device {
    rules: Rules,
    local: LocalRules,
}

impl Matcher for Device {
    fn is_excluded(&self, path: &Path, dir: bool) -> bool {
        match status(&self.rules, path, dir) {
            Status::Excluded(Reason::BuiltIn | Reason::Local) => true,
            Status::Excluded(Reason::Default | Reason::Rule { .. }) | Status::Included => false,
        }
    }

    fn local(&self) -> LocalRules {
        self.local.clone()
    }
}

pub(crate) fn device(local: LocalRules) -> Arc<dyn Matcher> {
    Arc::new(Device {
        rules: Rules::new(local.clone()),
        local,
    })
}

/// This device's `[sync]` list and its `gitignore` setting in the form the matcher takes, for the engine's trees as
/// well. An include path is read as [`SyncRules::lines`] reads it, with any number of slashes around it.
pub fn local_rules(sync: &SyncRules) -> Result<LocalRules, LocalRulesError> {
    let include: Vec<String> = sync
        .include
        .iter()
        .map(|path| {
            let dir = if path.ends_with('/') { "/" } else { "" };
            format!("{}{dir}", path.trim_matches('/'))
        })
        .collect();
    Ok(LocalRules::new(&include, &sync.exclude)?.with_gitignore(sync.gitignore))
}

/// The matcher of this device's `[sync]` list and the built-in exclusions.
pub fn matcher(sync: &SyncRules) -> Result<Arc<dyn Matcher>, LocalRulesError> {
    Ok(device(local_rules(sync)?))
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

/// What `core/fs` asks while it scans: the device's matcher, and the rules files the scan has read so far.
struct Scanned {
    device: Arc<dyn Matcher>,
    files: Rules,
    gitignore: bool,
}

impl Ignore for Scanned {
    fn set_rules(&mut self, dir: &Path, source: Source, rules: Option<&[u8]>) {
        let rules = rules.map(RuleFile::parse);
        self.files
            .set_rules(&names(dir), source, rules, std::iter::empty());
    }

    fn status(&self, path: &Path, kind: EntryKind) -> Ignored {
        let dir = kind == EntryKind::Dir;
        match status(&self.files, path, dir) {
            Status::Excluded(Reason::BuiltIn) => Ignored::BuiltIn,
            Status::Excluded(Reason::Local | Reason::Default | Reason::Rule { .. }) => {
                Ignored::Rule
            }
            Status::Included if self.device.is_excluded(path, dir) => Ignored::Rule,
            Status::Included => Ignored::No,
        }
    }

    fn key(&self, name: &OsStr) -> OsString {
        name_key(&name.to_string_lossy()).into()
    }

    fn follows(&self, source: Source) -> bool {
        source == Source::Koiignore || self.gitignore
    }
}

/// The rules a new watcher of a folder scans with: `device`, the built-in exclusions, and the rules files, of which it
/// has read none yet. Its first scan reads and reports every one.
pub(crate) fn scan_rules(device: &Arc<dyn Matcher>) -> koidrive_fs::Rules {
    let local = device.local();
    koidrive_fs::Rules::new(Box::new(Scanned {
        device: device.clone(),
        gitignore: local.follows_gitignore(),
        files: Rules::new(local),
    }))
}

/// The files of a folder that only its `.gitignore` files leave out, and their size in bytes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitignoreOnly {
    pub files: u64,
    pub bytes: u64,
    /// The folders whose files the count leaves out, because the scan cannot list them or read their rules file.
    pub unreadable: Vec<PathBuf>,
}

struct Both {
    with: Rules,
    without: Rules,
}

struct Counted(Arc<Mutex<Both>>);

impl Ignore for Counted {
    fn set_rules(&mut self, dir: &Path, source: Source, rules: Option<&[u8]>) {
        let mut both = self.0.lock().unwrap();
        let Both { with, without } = &mut *both;
        for rules_of in [with, without] {
            let rules = rules.map(RuleFile::parse);
            rules_of.set_rules(&names(dir), source, rules, std::iter::empty());
        }
    }

    fn status(&self, path: &Path, kind: EntryKind) -> Ignored {
        match status(
            &self.0.lock().unwrap().without,
            path,
            kind == EntryKind::Dir,
        ) {
            Status::Excluded(Reason::BuiltIn) => Ignored::BuiltIn,
            Status::Excluded(Reason::Local | Reason::Default | Reason::Rule { .. }) => {
                Ignored::Rule
            }
            Status::Included => Ignored::No,
        }
    }

    fn key(&self, name: &OsStr) -> OsString {
        name_key(&name.to_string_lossy()).into()
    }
}

pub(crate) fn gitignore_only(root: &Path, local: LocalRules) -> io::Result<GitignoreOnly> {
    let both = Arc::new(Mutex::new(Both {
        with: Rules::new(local.clone().with_gitignore(true)),
        without: Rules::new(local.with_gitignore(false)),
    }));
    let mut rules = koidrive_fs::Rules::new(Box::new(Counted(both.clone())));
    let mut snapshot = Snapshot::default();
    let rescan = snapshot
        .rescan_with_rules(root, [PathBuf::new()], &mut rules)
        .map_err(|error| match error {
            koidrive_fs::Error::Io(error) => error,
            other => io::Error::other(other),
        })?;
    let both = both.lock().unwrap();
    let only = (snapshot.entries.iter())
        .filter(|(path, entry)| {
            entry.kind == EntryKind::File && status(&both.with, path, false) != Status::Included
        })
        .map(|(_, entry)| entry.size);
    Ok(GitignoreOnly {
        files: only.clone().count() as u64,
        bytes: only.sum(),
        unreadable: (rescan.unreadable.iter())
            .map(|folder| root.join(folder).components().collect())
            .collect(),
    })
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
    fn sync_entries_count_a_git_folder_like_any_other_entry_a_rule_leaves_out() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git/objects")).unwrap();
        std::fs::write(dir.path().join(".git/HEAD"), b"ref: refs/heads/main\n").unwrap();
        assert!(has_sync_entries(dir.path()).unwrap());
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
            ..SyncRules::default()
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
            gitignore: true,
        };
        let status = |scanned: &Scanned, path: &str, kind| scanned.status(Path::new(path), kind);
        assert_eq!(status(&scanned, "private", EntryKind::Dir), Ignored::Rule);
        assert_eq!(
            status(&scanned, "a/Thumbs.db", EntryKind::File),
            Ignored::BuiltIn
        );
        assert_eq!(status(&scanned, "a/b.log", EntryKind::File), Ignored::No);

        scanned.set_rules(Path::new("a"), Source::Koiignore, Some(b"*.log\n"));
        assert_eq!(status(&scanned, "a/b.log", EntryKind::File), Ignored::Rule);
        assert_eq!(status(&scanned, "b.log", EntryKind::File), Ignored::No);
        scanned.set_rules(Path::new("a"), Source::Koiignore, None);
        assert_eq!(status(&scanned, "a/b.log", EntryKind::File), Ignored::No);
    }

    #[test]
    fn a_gitignore_file_that_cannot_be_read_holds_its_folder_only_while_the_device_follows_them() {
        use std::os::unix::fs::PermissionsExt;

        if rustix::process::geteuid().is_root() {
            return;
        }
        for follow in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            std::fs::create_dir(root.join("sub")).unwrap();
            std::fs::write(root.join("sub/x.txt"), "x").unwrap();
            let file = root.join("sub/.gitignore");
            std::fs::write(&file, "x.txt\n").unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
            let mut rules = scan_rules(&device(LocalRules::default().with_gitignore(follow)));
            let mut snapshot = Snapshot::default();
            let rescan = snapshot.rescan_with_rules(root, [std::path::PathBuf::new()], &mut rules);
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
            let rescan = rescan.unwrap();
            assert_eq!(rescan.unreadable.is_empty(), !follow, "follow {follow}");
            assert_eq!(
                snapshot.entries.contains_key(Path::new("sub/x.txt")),
                !follow,
                "follow {follow}"
            );
        }
    }

    #[test]
    fn gitignore_only_names_the_folders_it_cannot_read_and_counts_the_rest() {
        use std::os::unix::fs::PermissionsExt;

        if rustix::process::geteuid().is_root() {
            return;
        }
        for locked in ["private", "private/.gitignore"] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            std::fs::create_dir(root.join("private")).unwrap();
            std::fs::write(root.join("private/.gitignore"), "*.log\n").unwrap();
            std::fs::write(root.join("private/a.log"), "abc").unwrap();
            std::fs::write(root.join(".gitignore"), "*.tmp\n").unwrap();
            std::fs::write(root.join("b.tmp"), "12").unwrap();
            let locked = root.join(locked);
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
            let counted = gitignore_only(root, LocalRules::default());
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
            let only = GitignoreOnly {
                files: 1,
                bytes: 2,
                unreadable: vec![root.join("private")],
            };
            assert_eq!(counted.unwrap(), only, "{locked:?}");
        }
    }

    #[test]
    fn a_scan_leaves_out_git_folders_until_a_koiignore_pattern_includes_them_and_keeps_the_device_list()
     {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for path in [
            "repo/.git/HEAD",
            "repo/.git/index.lock",
            "repo/README.md",
            "repo/sub/.git",
            "other/.git/HEAD",
        ] {
            std::fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
            std::fs::write(root.join(path), b"x").unwrap();
        }
        let scanned = |sync: &SyncRules| -> Vec<String> {
            let device = matcher(sync).unwrap();
            assert!(!device.is_excluded(Path::new("repo/.git"), true));
            let snapshot = Snapshot::scan_with_rules(root, &mut scan_rules(&device)).unwrap();
            (snapshot.entries.iter())
                .filter(|(_, entry)| entry.kind == EntryKind::File)
                .map(|(path, _)| path.to_string_lossy().into_owned())
                .collect()
        };
        for gitignore in [true, false] {
            let sync = SyncRules {
                gitignore,
                ..sync(&[], &["*.lock"])
            };
            assert_eq!(scanned(&sync), ["repo/README.md", "repo/sub/.git"]);
            std::fs::write(root.join("repo/.koiignore"), b"!.git/\n").unwrap();
            assert_eq!(
                scanned(&sync),
                [
                    "repo/.git/HEAD",
                    "repo/.koiignore",
                    "repo/README.md",
                    "repo/sub/.git"
                ]
            );
            std::fs::remove_file(root.join("repo/.koiignore")).unwrap();
        }
    }
}
