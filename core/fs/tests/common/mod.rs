// SPDX-License-Identifier: Apache-2.0

#![allow(dead_code)]

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use koidrive_fs::{
    Action, Base, Contents, Dir, Displaced, Effect, Entry, EntryKind, FileId, Location, Marker,
    Outcome, PRIVATE_DIR, QUARANTINE_DIR, STAGING_DIR, Snapshot, Space, Step, TRASH_DIR,
    read_marker,
};
use koidrive_ignore::PLATFORM_FILES;

pub const SPACE: &str = "space-1";
pub const COUNTER: u64 = 5;
pub const ID: &str = "e1";
pub const SYNCED: &[u8] = b"synced";
pub const REMOTE: &[u8] = b"remote";
pub const RULES: &[u8] = b".env\n";
pub const EDITED: &[u8] = b"edited";
pub const OTHER: &[u8] = b"someone else";
pub const RECOVERED: &str = "docs/report (recovered 2023-11-14).txt";
pub const NOW: u64 = 1_700_000_000;

pub fn now() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(NOW)
}

pub fn marker() -> Marker {
    Marker {
        space_id: SPACE.into(),
        effects: COUNTER,
        token: None,
    }
}

pub struct Bytes;

fn read(mut file: &File) -> Vec<u8> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    bytes
}

impl Contents for Bytes {
    fn displaced(&mut self, file: &File) -> std::io::Result<bool> {
        Ok(read(file) == SYNCED)
    }

    fn written(&mut self, file: &File) -> std::io::Result<bool> {
        Ok(read(file) == REMOTE)
    }

    fn along(&mut self, _: usize, file: &File) -> std::io::Result<bool> {
        Ok(read(file) == RULES)
    }

    fn displaced_content(&self) -> Option<Vec<u8>> {
        Some(SYNCED.to_vec())
    }
}

pub fn same(file: &File, content: &[u8]) -> std::io::Result<bool> {
    Ok(read(file) == content)
}

pub fn bind(root: &Path) -> Space {
    Space::bind(File::open(root).unwrap().into(), root).unwrap()
}

pub fn apply_one(
    space: &Space,
    effect: &Effect,
    hook: &mut dyn FnMut(Step),
) -> std::io::Result<Outcome> {
    let mut section = space.section(&marker(), hook)?;
    section.apply(effect, &mut Bytes, now())?;
    let [outcome] = section.finish()?.try_into().unwrap();
    Ok(outcome)
}

pub fn recover_one(
    space: &Space,
    effect: &Effect,
    hook: &mut dyn FnMut(Step),
) -> std::io::Result<Outcome> {
    let mut section = space.section(&marker(), hook)?;
    section.recover(effect, &mut Bytes, now())?;
    let [outcome] = section.finish()?.try_into().unwrap();
    Ok(outcome)
}

pub fn assert_identity(root: &Path, outcome: &Outcome, path: Option<&str>) {
    if let Outcome::Done { identity } = outcome {
        let expected = path.map(|path| entries(root, &[path])[0].id);
        assert_eq!(*identity, expected, "{outcome:?}");
    }
}

pub fn entries(root: &Path, paths: &[&str]) -> Vec<Entry> {
    let snapshot = Snapshot::scan(root).unwrap();
    paths
        .iter()
        .map(|path| snapshot.entries[Path::new(path)])
        .collect()
}

pub fn dir(name: &str, entry: &Entry) -> Dir {
    Dir {
        name: name.into(),
        id: entry.id,
    }
}

pub fn at(dirs: Vec<Dir>, name: &str) -> Location {
    Location::At {
        base: Base::Root,
        dirs,
        name: name.into(),
    }
}

pub fn stage(space: &Space, bytes: &[u8]) {
    let mut file = space.stage(ID).unwrap();
    file.write_all(bytes).unwrap();
    drop(file);
    space.finish_staged(ID, false).unwrap();
}

pub fn rewrite_keeping_mtime(path: &Path, bytes: &[u8]) {
    let before = fs::metadata(path).unwrap();
    assert_eq!(before.len(), bytes.len() as u64);
    let file = OpenOptions::new().write(true).open(path).unwrap();
    file.write_all_at(bytes, 0).unwrap();
    file.set_modified(before.modified().unwrap()).unwrap();
}

pub fn private_names(root: &Path, folder: &str) -> Vec<String> {
    let Ok(listing) = fs::read_dir(root.join(PRIVATE_DIR).join(folder)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = listing
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

pub fn trashed(root: &Path) -> Vec<Vec<u8>> {
    private_names(root, TRASH_DIR)
        .iter()
        .map(|name| fs::read(root.join(PRIVATE_DIR).join(TRASH_DIR).join(name)).unwrap())
        .collect()
}

pub fn contents(path: PathBuf) -> Option<Vec<u8>> {
    match fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => panic!("{err}"),
    }
}

pub fn assert_settled(root: &Path) {
    assert_eq!(private_names(root, QUARANTINE_DIR), Vec::<String>::new());
    assert_eq!(private_names(root, STAGING_DIR), Vec::<String>::new());
    assert_eq!(read_marker(root).unwrap(), Some(marker()));
}

pub struct Scenario {
    pub name: &'static str,
    pub paths: &'static [&'static str],
    pub setup: fn(&Path, &Space),
    pub build: fn(&[Entry]) -> Effect,
    pub disturb: fn(&Path),
    pub check: fn(&Path, &Outcome),
    pub done_from: Option<Step>,
}

fn report(root: &Path, space: &Space) {
    fs::create_dir(root.join("docs")).unwrap();
    fs::write(root.join("docs/report.txt"), SYNCED).unwrap();
    stage(space, REMOTE);
}

fn edited_report(root: &Path, space: &Space) {
    report(root, space);
    rewrite_keeping_mtime(&root.join("docs/report.txt"), EDITED);
}

fn replacement(entries: &[Entry]) -> Effect {
    let at = at(vec![dir("docs", &entries[0])], "report.txt");
    Effect {
        id: ID.into(),
        node: "node".into(),
        action: Action::WriteFile {
            at: at.clone(),
            replaces: Some(Displaced {
                at,
                entry: entries[1],
            }),
        },
    }
}

fn folder_removal(entries: &[Entry]) -> Effect {
    Effect {
        id: ID.into(),
        node: "node".into(),
        action: Action::RemoveDir {
            at: at(vec![dir("docs", &entries[0])], "empty"),
            id: entries[1].id,
            along: vec![],
        },
    }
}

fn take_report(root: &Path) {
    let path = root.join("docs/report.txt");
    if contents(path.clone()).is_none() {
        fs::write(path, OTHER).unwrap();
    }
}

pub const SCENARIOS: [Scenario; 7] = [
    Scenario {
        name: "replace",
        paths: &["docs", "docs/report.txt"],
        setup: report,
        build: replacement,
        disturb: |_| {},
        check: |root, outcome| {
            assert_identity(root, outcome, Some("docs/report.txt"));
            let target = contents(root.join("docs/report.txt"));
            if matches!(outcome, Outcome::Done { .. }) {
                assert_eq!(target.as_deref(), Some(REMOTE));
                assert_eq!(trashed(root), [SYNCED]);
            } else {
                assert_eq!(target.as_deref(), Some(SYNCED));
                assert!(trashed(root).is_empty());
            }
        },
        done_from: Some(Step::Quarantined),
    },
    Scenario {
        name: "write_new",
        paths: &["docs"],
        setup: |root, space| {
            fs::create_dir(root.join("docs")).unwrap();
            stage(space, REMOTE);
        },
        build: |entries| Effect {
            id: ID.into(),
            node: "node".into(),
            action: Action::WriteFile {
                at: at(vec![dir("docs", &entries[0])], "new.txt"),
                replaces: None,
            },
        },
        disturb: |_| {},
        check: |root, outcome| {
            assert_identity(root, outcome, Some("docs/new.txt"));
            let expected = matches!(outcome, Outcome::Done { .. }).then_some(REMOTE);
            assert_eq!(contents(root.join("docs/new.txt")).as_deref(), expected);
        },
        done_from: Some(Step::Placed),
    },
    Scenario {
        name: "create_dir",
        paths: &["docs"],
        setup: |root, _| fs::create_dir(root.join("docs")).unwrap(),
        build: |entries| Effect {
            id: ID.into(),
            node: "node".into(),
            action: Action::CreateDir {
                at: at(vec![dir("docs", &entries[0])], "sub"),
            },
        },
        disturb: |_| {},
        check: |root, outcome| {
            assert_identity(root, outcome, Some("docs/sub"));
            assert_eq!(
                root.join("docs/sub").is_dir(),
                matches!(outcome, Outcome::Done { .. })
            );
        },
        done_from: Some(Step::Placed),
    },
    Scenario {
        name: "move",
        paths: &["docs", "docs/a.txt", "other"],
        setup: |root, _| {
            fs::create_dir_all(root.join("docs")).unwrap();
            fs::create_dir_all(root.join("other")).unwrap();
            fs::write(root.join("docs/a.txt"), SYNCED).unwrap();
        },
        build: |entries| Effect {
            id: ID.into(),
            node: "node".into(),
            action: Action::Move {
                from: at(vec![dir("docs", &entries[0])], "a.txt"),
                id: entries[1].id,
                to: at(vec![dir("other", &entries[2])], "b.txt"),
            },
        },
        disturb: |_| {},
        check: |root, outcome| {
            assert_identity(root, outcome, None);
            let (from, to) = (
                contents(root.join("docs/a.txt")),
                contents(root.join("other/b.txt")),
            );
            if matches!(outcome, Outcome::Done { .. }) {
                assert_eq!((from, to.as_deref()), (None, Some(SYNCED)));
            } else {
                assert_eq!((from.as_deref(), to), (Some(SYNCED), None));
            }
        },
        done_from: Some(Step::Placed),
    },
    Scenario {
        name: "remove_file",
        paths: &["docs", "docs/report.txt"],
        setup: |root, _| {
            fs::create_dir(root.join("docs")).unwrap();
            fs::write(root.join("docs/report.txt"), SYNCED).unwrap();
        },
        build: |entries| Effect {
            id: ID.into(),
            node: "node".into(),
            action: Action::RemoveFile {
                displaced: Displaced {
                    at: at(vec![dir("docs", &entries[0])], "report.txt"),
                    entry: entries[1],
                },
            },
        },
        disturb: |_| {},
        check: |root, outcome| {
            assert_identity(root, outcome, None);
            let target = contents(root.join("docs/report.txt"));
            if matches!(outcome, Outcome::Done { .. }) {
                assert_eq!(target, None);
                assert_eq!(trashed(root), [SYNCED]);
            } else {
                assert_eq!(target.as_deref(), Some(SYNCED));
                assert!(trashed(root).is_empty());
            }
        },
        done_from: Some(Step::Quarantined),
    },
    Scenario {
        name: "remove_dir",
        paths: &["docs", "docs/empty"],
        setup: |root, _| fs::create_dir_all(root.join("docs/empty")).unwrap(),
        build: folder_removal,
        disturb: |_| {},
        check: |root, outcome| {
            assert_identity(root, outcome, None);
            let done = matches!(outcome, Outcome::Done { .. });
            assert_eq!(root.join("docs/empty").is_dir(), !done);
            let trashed_dirs = private_names(root, TRASH_DIR).len();
            assert_eq!(trashed_dirs, usize::from(done));
        },
        done_from: Some(Step::Quarantined),
    },
    Scenario {
        name: "remove_dir_with_platform_files",
        paths: &["docs", "docs/empty"],
        setup: |root, _| {
            fs::create_dir_all(root.join("docs/empty")).unwrap();
            for name in PLATFORM_FILES {
                fs::write(root.join("docs/empty").join(name), "view").unwrap();
            }
        },
        build: folder_removal,
        disturb: |_| {},
        check: |root, outcome| {
            assert_identity(root, outcome, None);
            let trash = private_names(root, TRASH_DIR);
            if matches!(outcome, Outcome::Done { .. }) {
                assert!(!root.join("docs/empty").exists());
                let [folder] = &trash[..] else {
                    panic!("one trashed folder: {trash:?}");
                };
                let folder = root.join(PRIVATE_DIR).join(TRASH_DIR).join(folder);
                assert_eq!(fs::read_dir(folder).unwrap().count(), 0);
            } else {
                let left = fs::read_dir(root.join("docs/empty")).unwrap().count();
                assert_eq!(left, PLATFORM_FILES.len());
                assert!(trash.is_empty());
            }
        },
        done_from: Some(Step::Quarantined),
    },
];

pub const RETURNS: [Scenario; 4] = [
    Scenario {
        name: "edited",
        paths: &["docs", "docs/report.txt"],
        setup: edited_report,
        build: replacement,
        disturb: |_| {},
        check: |root, _| {
            assert_eq!(
                contents(root.join("docs/report.txt")).as_deref(),
                Some(EDITED)
            );
            assert!(trashed(root).is_empty());
        },
        done_from: None,
    },
    Scenario {
        name: "edited_name_taken",
        paths: &["docs", "docs/report.txt"],
        setup: edited_report,
        build: replacement,
        disturb: take_report,
        check: |root, outcome| {
            let report = contents(root.join("docs/report.txt"));
            let recovered = contents(root.join(RECOVERED));
            if report.as_deref() == Some(EDITED) {
                assert_eq!(recovered, None);
            } else {
                assert_eq!(report.as_deref(), Some(OTHER));
                assert_eq!(recovered.as_deref(), Some(EDITED));
            }
            if let Outcome::Failed {
                recovered: Some(path),
                ..
            } = outcome
            {
                assert_eq!(path, Path::new(RECOVERED));
            }
            assert!(trashed(root).is_empty());
        },
        done_from: None,
    },
    Scenario {
        name: "edited_folder_moved",
        paths: &["docs", "docs/report.txt"],
        setup: edited_report,
        build: replacement,
        disturb: |root| fs::rename(root.join("docs"), root.join("moved")).unwrap(),
        check: |root, outcome| {
            let name = Path::new(RECOVERED).file_name().unwrap();
            let moved = contents(root.join("moved/report.txt"));
            let recovered = contents(root.join(name));
            if moved.is_some() {
                assert_eq!((moved.as_deref(), recovered), (Some(EDITED), None));
            } else {
                assert_eq!(recovered.as_deref(), Some(EDITED));
            }
            if let Outcome::Failed {
                recovered: Some(path),
                ..
            } = outcome
            {
                assert_eq!(path, Path::new(name));
            }
            assert!(!root.join("docs").exists());
            assert_eq!(
                fs::read_dir(root.join("moved")).unwrap().count(),
                usize::from(moved.is_some())
            );
            assert!(trashed(root).is_empty());
        },
        done_from: None,
    },
    Scenario {
        name: "name_taken",
        paths: &["docs", "docs/report.txt"],
        setup: report,
        build: replacement,
        disturb: take_report,
        check: |root, outcome| {
            assert_identity(root, outcome, Some("docs/report.txt"));
            let report = contents(root.join("docs/report.txt"));
            if matches!(outcome, Outcome::Done { .. }) {
                assert_eq!(report.as_deref(), Some(REMOTE));
                assert_eq!(trashed(root), [SYNCED]);
            } else if report.as_deref() == Some(SYNCED) {
                assert!(trashed(root).is_empty());
            } else {
                assert_eq!(report.as_deref(), Some(OTHER));
                assert_eq!(trashed(root), [SYNCED]);
            }
        },
        done_from: Some(Step::Placed),
    },
];

pub const FOLDER_MOVED: [Scenario; 2] = [
    Scenario {
        name: "remove_file_folder_moved",
        paths: SCENARIOS[4].paths,
        setup: SCENARIOS[4].setup,
        build: SCENARIOS[4].build,
        disturb: move_docs,
        check: |root, outcome| {
            assert_identity(root, outcome, None);
            let target = contents(root.join("moved/report.txt"));
            if matches!(outcome, Outcome::Done { .. }) {
                assert_eq!(target, None);
                assert_eq!(trashed(root), [SYNCED]);
            } else {
                assert_eq!(target.as_deref(), Some(SYNCED));
                assert!(trashed(root).is_empty());
            }
        },
        done_from: Some(Step::Quarantined),
    },
    Scenario {
        name: "remove_dir_folder_moved",
        paths: SCENARIOS[5].paths,
        setup: SCENARIOS[5].setup,
        build: folder_removal,
        disturb: move_docs,
        check: |root, outcome| {
            assert_identity(root, outcome, None);
            let done = matches!(outcome, Outcome::Done { .. });
            assert_eq!(root.join("moved/empty").is_dir(), !done);
            let trashed_dirs = private_names(root, TRASH_DIR).len();
            assert_eq!(trashed_dirs, usize::from(done));
        },
        done_from: Some(Step::Quarantined),
    },
];

fn move_docs(root: &Path) {
    fs::rename(root.join("docs"), root.join("moved")).unwrap();
}

pub fn scenario(name: &str) -> &'static Scenario {
    SCENARIOS
        .iter()
        .chain(&RETURNS)
        .chain(&FOLDER_MOVED)
        .find(|scenario| scenario.name == name)
        .unwrap()
}

pub fn prepare(scenario: &Scenario, root: &Path) -> Effect {
    fs::create_dir_all(root).unwrap();
    let space = bind(root);
    (scenario.setup)(root, &space);
    (scenario.build)(&entries(root, scenario.paths))
}

pub fn encode(entries: &[Entry]) -> String {
    entries
        .iter()
        .map(|entry| {
            format!(
                "{} {} {} {} {} {}\n",
                matches!(entry.kind, EntryKind::Dir),
                entry.id.ino,
                entry
                    .id
                    .birth_ns
                    .map_or("-".to_string(), |ns| ns.to_string()),
                entry.size,
                entry.mtime_ns,
                entry.executable
            )
        })
        .collect()
}

pub fn decode(text: &str) -> Vec<Entry> {
    text.lines()
        .map(|line| {
            let fields: Vec<&str> = line.split(' ').collect();
            Entry {
                kind: if fields[0] == "true" {
                    EntryKind::Dir
                } else {
                    EntryKind::File
                },
                id: FileId {
                    ino: fields[1].parse().unwrap(),
                    birth_ns: (fields[2] != "-").then(|| fields[2].parse().unwrap()),
                },
                size: fields[3].parse().unwrap(),
                mtime_ns: fields[4].parse().unwrap(),
                ctime: koidrive_fs::Ctime::Settled { at: 0, mode: 0 },
                executable: fields[5] == "true",
                linked: false,
                mount: None,
            }
        })
        .collect()
}
