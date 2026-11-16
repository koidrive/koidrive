// SPDX-License-Identifier: Apache-2.0

mod common;

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use common::bind;
use koidrive_fs::{
    Base, Dir, FileId, Location, PARKED_DIR, PRIVATE_DIR, Snapshot, Space, Unresolved,
};

fn id(root: &Path, path: &str) -> FileId {
    Snapshot::scan(root).unwrap().entries[Path::new(path)].id
}

fn dir(root: &Path, path: &str) -> Dir {
    Dir {
        name: Path::new(path).file_name().unwrap().into(),
        id: id(root, path),
    }
}

fn at(dirs: Vec<Dir>, name: &str) -> Location {
    Location::At {
        base: Base::Root,
        dirs,
        name: name.into(),
    }
}

fn listing(path: &Path) -> Vec<PathBuf> {
    let mut names: Vec<PathBuf> = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    names.sort();
    names
}

/// Asserts the precondition of a permission test. Returns false when the
/// permission change does not block reads because the test runs as root.
fn blocks_reads<T: std::fmt::Debug>(test: &str, probe: std::io::Result<T>) -> bool {
    use std::io::Write;

    match probe {
        Err(err) => {
            assert_eq!(
                err.kind(),
                std::io::ErrorKind::PermissionDenied,
                "{test}: {err}"
            );
            true
        }
        Ok(found) => {
            assert!(
                rustix::process::geteuid().is_root(),
                "{test}: the permission change did not block reads ({found:?})"
            );
            let _ = writeln!(
                std::io::stderr(),
                "SKIPPED {test}: running as root, so permissions do not block reads"
            );
            false
        }
    }
}

#[test]
fn location_resolves_through_folders_with_their_identities() {
    let dir_ = tempfile::tempdir().unwrap();
    let root = dir_.path();
    fs::create_dir_all(root.join("a/b")).unwrap();
    let space = bind(root);

    let location = at(vec![dir(root, "a"), dir(root, "a/b")], "f.txt");
    let (folder, name) = space.parent(&location).unwrap();
    assert_eq!(name, "f.txt");
    assert_eq!(folder.id(), id(root, "a/b"));
    assert_eq!(folder.path(), Path::new("a/b"));
    space.recheck(&folder).unwrap();

    let top_level = at(Vec::new(), "g");
    let (top, name) = space.parent(&top_level).unwrap();
    assert_eq!(name, "g");
    assert_eq!(top.path(), Path::new(""));
    space.recheck(&top).unwrap();
}

#[test]
fn parent_replaced_by_a_symlink_is_not_followed() {
    let outside = tempfile::tempdir().unwrap();
    let dir_ = tempfile::tempdir().unwrap();
    let root = dir_.path();
    fs::create_dir_all(root.join("a/b")).unwrap();
    let location = at(vec![dir(root, "a"), dir(root, "a/b")], "f.txt");
    fs::create_dir(outside.path().join("b")).unwrap();
    let before = listing(outside.path());

    fs::rename(root.join("a/b"), root.join("old-b")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("b"), root.join("a/b")).unwrap();
    let space = bind(root);
    assert!(matches!(
        space.parent(&location),
        Err(Unresolved::NotAFolder(path)) if path == Path::new("a/b")
    ));
    assert_eq!(listing(outside.path()), before);
}

#[test]
fn parent_replaced_by_another_folder_fails_its_identity_check() {
    let dir_ = tempfile::tempdir().unwrap();
    let root = dir_.path();
    fs::create_dir_all(root.join("a/b")).unwrap();
    let location = at(vec![dir(root, "a"), dir(root, "a/b")], "f.txt");
    let expected = id(root, "a/b");

    fs::rename(root.join("a/b"), root.join("old-b")).unwrap();
    fs::create_dir(root.join("a/b")).unwrap();
    let space = bind(root);
    match space.parent(&location) {
        Err(Unresolved::Replaced {
            path,
            expected: wanted,
            found,
        }) => {
            assert_eq!(path, Path::new("a/b"));
            assert_eq!(wanted, expected);
            assert_eq!(found, id(root, "a/b"));
        }
        other => panic!("expected a replaced folder, got {other:?}"),
    }
}

#[test]
fn parent_moved_out_of_the_root_fails_the_recheck() {
    let outside = tempfile::tempdir().unwrap();
    let dir_ = tempfile::tempdir().unwrap();
    let root = dir_.path();
    fs::create_dir_all(root.join("a/b")).unwrap();
    let space = bind(root);
    let location = at(vec![dir(root, "a"), dir(root, "a/b")], "f.txt");
    let (folder, _) = space.parent(&location).unwrap();

    fs::rename(root.join("a"), outside.path().join("a")).unwrap();
    assert!(matches!(
        space.recheck(&folder),
        Err(Unresolved::Missing(path)) if path == Path::new("a")
    ));

    fs::rename(outside.path().join("a"), root.join("c")).unwrap();
    assert!(matches!(
        space.recheck(&folder),
        Err(Unresolved::Missing(path)) if path == Path::new("a")
    ));
}

#[test]
fn ancestor_swapped_after_resolution_fails_the_recheck() {
    let dir_ = tempfile::tempdir().unwrap();
    let root = dir_.path();
    fs::create_dir_all(root.join("a/b")).unwrap();
    let space = bind(root);
    let location = at(vec![dir(root, "a"), dir(root, "a/b")], "f.txt");
    let (folder, _) = space.parent(&location).unwrap();

    fs::rename(root.join("a"), root.join("old-a")).unwrap();
    fs::create_dir_all(root.join("a/b")).unwrap();
    assert!(matches!(
        space.recheck(&folder),
        Err(Unresolved::Replaced { path, .. }) if path == Path::new("a")
    ));
}

#[test]
fn parked_base_checks_the_parked_folder_identity() {
    let dir_ = tempfile::tempdir().unwrap();
    let root = dir_.path();
    let parked = root.join(PRIVATE_DIR).join(PARKED_DIR);
    fs::create_dir_all(parked.join("node-1/sub")).unwrap();
    fs::create_dir_all(root.join("x")).unwrap();
    let space = bind(root);
    let node_id = |path: &Path| {
        use std::os::unix::fs::MetadataExt;
        let meta = fs::symlink_metadata(path).unwrap();
        u128::from(meta.ino())
    };

    let parked_scan = Snapshot::scan(&parked).unwrap();
    let node = parked_scan.entries[Path::new("node-1")].id;
    let sub = parked_scan.entries[Path::new("node-1/sub")].id;
    let location = Location::At {
        base: Base::Parked {
            name: "node-1".into(),
            id: node,
        },
        dirs: vec![Dir {
            name: "sub".into(),
            id: sub,
        }],
        name: "f.txt".into(),
    };
    let (folder, name) = space.parent(&location).unwrap();
    assert_eq!(name, "f.txt");
    assert_eq!(folder.id().ino, node_id(&parked.join("node-1/sub")));
    space.recheck(&folder).unwrap();

    let parked_entry = Location::Parked("node-2".into());
    let (holder, name) = space.parent(&parked_entry).unwrap();
    assert_eq!(name, "node-2");
    assert_eq!(holder.path(), Path::new(".koidrive/parked"));

    let wrong = Location::At {
        base: Base::Parked {
            name: "node-1".into(),
            id: id(root, "x"),
        },
        dirs: Vec::new(),
        name: "f.txt".into(),
    };
    assert!(matches!(
        space.parent(&wrong),
        Err(Unresolved::Replaced { path, .. }) if path == Path::new(".koidrive/parked/node-1")
    ));
}

#[test]
fn names_that_are_not_one_component_are_refused() {
    let dir_ = tempfile::tempdir().unwrap();
    let root = dir_.path();
    fs::create_dir(root.join("a")).unwrap();
    let space = bind(root);
    for name in ["", ".", "..", "a/b", "/etc", "nul\0"] {
        assert!(
            matches!(
                space.parent(&at(Vec::new(), name)),
                Err(Unresolved::BadName(_))
            ),
            "{name:?}"
        );
        let through = Dir {
            name: OsString::from(name),
            id: id(root, "a"),
        };
        assert!(
            matches!(
                space.parent(&at(vec![through], "f")),
                Err(Unresolved::BadName(_))
            ),
            "{name:?}"
        );
    }
}

#[test]
fn unreadable_parent_is_not_missing() {
    let dir_ = tempfile::tempdir().unwrap();
    let root = dir_.path();
    fs::create_dir_all(root.join("a/b")).unwrap();
    let location = at(vec![dir(root, "a"), dir(root, "a/b")], "f.txt");
    fs::set_permissions(root.join("a"), fs::Permissions::from_mode(0o000)).unwrap();
    let probe = fs::read_dir(root.join("a/b"));
    if !blocks_reads("unreadable_parent_is_not_missing", probe) {
        fs::set_permissions(root.join("a"), fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }

    let space = bind(root);
    let resolved = space.parent(&location);
    fs::set_permissions(root.join("a"), fs::Permissions::from_mode(0o755)).unwrap();
    match resolved {
        Err(Unresolved::Io { path, err }) => {
            assert_eq!(path, Path::new("a"));
            assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        }
        other => panic!("expected a permission error, got {other:?}"),
    }
}

#[test]
fn private_folders_are_single_names_inside_the_private_folder() {
    let outside = tempfile::tempdir().unwrap();
    let dir_ = tempfile::tempdir().unwrap();
    let root = dir_.path();
    fs::create_dir_all(root.join(PRIVATE_DIR).join(PARKED_DIR)).unwrap();
    let space = bind(root);
    let parked = space.private(&[PARKED_DIR]).unwrap();
    assert_eq!(parked.path(), Path::new(".koidrive/parked"));

    let absolute = outside.path().to_str().unwrap();
    for parts in [
        vec![".."],
        vec!["../..", "outside"],
        vec![absolute],
        vec!["parked/.."],
        vec![""],
        vec!["."],
    ] {
        assert!(
            matches!(space.private(&parts), Err(Unresolved::BadName(_))),
            "{parts:?}"
        );
    }
}
#[test]
fn bind_needs_a_folder_and_held_follows_the_linked_path() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    let moved = dir.path().join("moved");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("f"), "f").unwrap();
    let file = fs::File::open(root.join("f")).unwrap();
    assert_eq!(
        Space::bind(file.into(), &root.join("f"))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::NotADirectory
    );

    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&root, &link).unwrap();
    let space = Space::bind(fs::File::open(&link).unwrap().into(), &link).unwrap();
    assert_eq!(space.id(), bind(&root).id());
    assert!(space.held().unwrap());

    fs::rename(&root, &moved).unwrap();
    assert!(!space.held().unwrap());
    assert!(matches!(
        space.parent(&at(Vec::new(), "f")),
        Err(Unresolved::RootWithdrawn(path)) if path == link
    ));
    fs::create_dir(&root).unwrap();
    assert!(!space.held().unwrap());
    fs::remove_dir(&root).unwrap();
    fs::write(&root, "a file").unwrap();
    assert!(!space.held().unwrap());
    fs::remove_file(&root).unwrap();
    fs::rename(&moved, &root).unwrap();
    assert!(space.held().unwrap());
    assert!(space.parent(&at(Vec::new(), "f")).is_ok());
}

#[test]
fn errors_quote_the_paths_they_name() {
    let missing = Unresolved::Missing(PathBuf::from("Secret Plans/budget 2026.xlsx"));
    assert_eq!(
        missing.to_string(),
        "\"Secret Plans/budget 2026.xlsx\" is missing"
    );
    let io = Unresolved::Io {
        path: PathBuf::from("Jiyeon Passport/scan"),
        err: std::io::Error::from_raw_os_error(13),
    };
    assert!(
        io.to_string()
            .starts_with("\"Jiyeon Passport/scan\": Permission denied (os error 13)"),
        "{io}"
    );
}
