// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsStr;
use std::fs;
use std::io::Write;
#[cfg(target_os = "macos")]
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use koidrive_state_file::{
    AtomicFile, Publish, recover, remove_with_legacy_matching, write, write_with_legacy,
};

#[cfg(target_os = "macos")]
#[test]
fn immutable_legacy_stages_do_not_block_publication() {
    let mut publications = Vec::new();
    for (destination, name) in [
        ("leaving", "leaving.next"),
        ("update-check.json", "update-check.json.new"),
    ] {
        for publish in [Publish::Create, Publish::Replace] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(destination);
            if matches!(publish, Publish::Replace) {
                fs::write(&path, b"old state").unwrap();
            }
            let legacy = dir.path().join(name);
            fs::write(&legacy, b"obsolete state").unwrap();
            let stage = AtomicFile::prepare(&path).unwrap();
            stage.as_file().write_all(b"new state").unwrap();
            assert!(
                Command::new("chflags")
                    .arg("uchg")
                    .arg(&legacy)
                    .status()
                    .unwrap()
                    .success()
            );

            let result = std::panic::catch_unwind(|| {
                let published = stage.finish_with_legacy(publish, &[OsStr::new(name)]);
                let saved = fs::read(&path);
                let obsolete = fs::read(&legacy);
                let protected = fs::remove_file(&legacy);
                (published, saved, obsolete, protected)
            });

            assert!(
                Command::new("chflags")
                    .arg("nouchg")
                    .arg(&legacy)
                    .status()
                    .unwrap()
                    .success()
            );
            let (published, saved, obsolete, protected) = result.unwrap();
            publications.push((destination, publish, published));
            assert_eq!(saved.unwrap(), b"new state");
            assert_eq!(obsolete.unwrap(), b"obsolete state");
            assert_eq!(
                protected.unwrap_err().kind(),
                std::io::ErrorKind::PermissionDenied
            );
        }
    }
    assert!(
        publications.iter().all(|(_, _, result)| result.is_ok()),
        "{publications:?}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn immutable_legacy_stages_do_not_block_removal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    let legacy = dir.path().join("state.json.tmp");
    let removable = dir.path().join("state.json.new");
    fs::write(&path, b"saved state").unwrap();
    fs::write(&legacy, b"obsolete state").unwrap();
    fs::write(&removable, b"removable state").unwrap();
    assert!(
        Command::new("chflags")
            .arg("uchg")
            .arg(&legacy)
            .status()
            .unwrap()
            .success()
    );

    let result = std::panic::catch_unwind(|| {
        let removed = remove_with_legacy_matching(&path, |name| {
            name == "state.json.tmp" || name == "state.json.new"
        });
        let removed_again = remove_with_legacy_matching(&path, |name| name == "state.json.tmp");
        let obsolete = fs::read(&legacy);
        let protected = fs::remove_file(&legacy);
        (removed, removed_again, obsolete, protected)
    });

    assert!(
        Command::new("chflags")
            .arg("nouchg")
            .arg(&legacy)
            .status()
            .unwrap()
            .success()
    );
    let (removed, removed_again, obsolete, protected) = result.unwrap();
    assert!(removed.is_ok(), "{removed:?}");
    assert!(removed_again.is_ok(), "{removed_again:?}");
    assert!(!path.exists());
    assert!(!removable.exists());
    assert_eq!(obsolete.unwrap(), b"obsolete state");
    assert_eq!(
        protected.unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
}

#[cfg(target_os = "macos")]
#[test]
fn removing_an_immutable_destination_still_fails() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    let legacy = dir.path().join("state.json.tmp");
    fs::write(&path, b"saved state").unwrap();
    fs::write(&legacy, b"obsolete state").unwrap();
    assert!(
        Command::new("chflags")
            .arg("uchg")
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );

    let result = std::panic::catch_unwind(|| {
        let removed = remove_with_legacy_matching(&path, |name| name == "state.json.tmp");
        (removed, fs::read(&path), fs::read(&legacy))
    });

    assert!(
        Command::new("chflags")
            .arg("nouchg")
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    let (removed, saved, obsolete) = result.unwrap();
    assert_eq!(
        removed.unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert_eq!(saved.unwrap(), b"saved state");
    assert_eq!(obsolete.unwrap(), b"obsolete state");
}

#[cfg(target_os = "macos")]
#[test]
fn an_immutable_abandoned_stage_does_not_block_other_writes_or_removals() {
    let dir = tempfile::tempdir().unwrap();
    let stage = dir
        .path()
        .join(".koi-state-0123456789abcdef0123456789abcdef.tmp");
    fs::write(&stage, b"abandoned").unwrap();
    let path = dir.path().join("state.json");
    fs::write(&path, b"old").unwrap();
    assert!(
        Command::new("chflags")
            .arg("uchg")
            .arg(&stage)
            .status()
            .unwrap()
            .success()
    );

    let written = write(&path, b"new");
    let published = fs::read(&path);
    let removed = remove_with_legacy_matching(&path, |_| false);
    let remaining = fs::read(&stage);

    assert!(
        Command::new("chflags")
            .arg("nouchg")
            .arg(&stage)
            .status()
            .unwrap()
            .success()
    );
    assert!(written.is_ok(), "{written:?}");
    assert_eq!(published.unwrap(), b"new");
    assert!(removed.is_ok(), "{removed:?}");
    assert!(!path.exists());
    assert_eq!(remaining.unwrap(), b"abandoned");
    assert_eq!(recover(dir.path()).unwrap(), 1);
}

#[test]
fn matching_legacy_cleanup_stays_in_the_prepared_directory_after_an_ancestor_moves() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("parent");
    let moved = root.path().join("moved");
    fs::create_dir(&dir).unwrap();
    let path = dir.join("state.json");
    let name = "state.json.tmp";
    fs::write(dir.join(name), b"abandoned").unwrap();
    let stage = AtomicFile::prepare(&path).unwrap();
    stage.as_file().write_all(b"new state").unwrap();
    stage
        .finish_with_legacy_matching(Publish::Replace, |entry| {
            if entry != name {
                return false;
            }
            fs::rename(&dir, &moved).unwrap();
            fs::create_dir(&dir).unwrap();
            fs::write(dir.join(name), b"replacement directory's data").unwrap();
            true
        })
        .unwrap();
    assert_eq!(fs::read(moved.join("state.json")).unwrap(), b"new state");
    assert!(!moved.join(name).exists());
    assert_eq!(
        fs::read(dir.join(name)).unwrap(),
        b"replacement directory's data"
    );
    assert!(!path.exists());
}

#[test]
fn removal_and_legacy_selection_share_the_same_directory_after_an_ancestor_moves() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("parent");
    let moved = root.path().join("moved");
    fs::create_dir(&dir).unwrap();
    let path = dir.join("state.json");
    let name = "state.json.tmp";
    fs::write(&path, b"saved").unwrap();
    fs::write(dir.join(name), b"abandoned").unwrap();
    remove_with_legacy_matching(&path, |entry| {
        if entry != name {
            return false;
        }
        fs::rename(&dir, &moved).unwrap();
        fs::create_dir(&dir).unwrap();
        fs::write(&path, b"another saved state").unwrap();
        fs::write(dir.join(name), b"another legacy-looking file").unwrap();
        true
    })
    .unwrap();
    assert!(!moved.join("state.json").exists());
    assert!(!moved.join(name).exists());
    assert_eq!(fs::read(&path).unwrap(), b"another saved state");
    assert_eq!(
        fs::read(dir.join(name)).unwrap(),
        b"another legacy-looking file"
    );
    remove_with_legacy_matching(&root.path().join("absent/state.json"), |_| true).unwrap();
    assert!(!root.path().join("absent").exists());
}

#[test]
fn removal_validates_all_legacy_names_before_removing_the_saved_state() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    fs::write(&path, b"saved").unwrap();
    let legacy = dir.path().join("state.json.tmp");
    fs::write(&legacy, b"abandoned").unwrap();
    assert_eq!(
        remove_with_legacy_matching(&path, |_| true)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidInput
    );
    assert_eq!(fs::read(&path).unwrap(), b"saved");
    assert_eq!(fs::read(&legacy).unwrap(), b"abandoned");
}

#[test]
fn removal_cannot_unlink_a_live_stage_through_a_filesystem_alias() {
    let dir = tempfile::tempdir().unwrap();
    let live = AtomicFile::prepare(&dir.path().join("live.json")).unwrap();
    live.as_file().write_all(b"live").unwrap();
    let stage = fs::read_dir(dir.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let path = dir.path().join("state.json");
    fs::hard_link(&stage, &path).unwrap();
    assert_eq!(
        remove_with_legacy_matching(&path, |_| false)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidInput
    );
    assert_eq!(fs::read(&path).unwrap(), b"live");
    fs::remove_file(&path).unwrap();
    fs::write(&path, b"saved").unwrap();
    let legacy = dir.path().join("state.json.tmp");
    fs::hard_link(&stage, &legacy).unwrap();
    assert_eq!(
        remove_with_legacy_matching(&path, |name| name == "state.json.tmp")
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidInput
    );
    assert_eq!(fs::read(&path).unwrap(), b"saved");
    assert_eq!(fs::read(&legacy).unwrap(), b"live");
    assert_eq!(fs::read(&stage).unwrap(), b"live");
    drop(live);
}

#[test]
fn legacy_cleanup_follows_successful_publication() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    let legacy = dir.path().join("state.json.tmp");
    fs::write(&legacy, b"legacy").unwrap();
    let stage = AtomicFile::prepare(&path).unwrap();
    stage.as_file().write_all(b"new").unwrap();
    assert_eq!(fs::read(&legacy).unwrap(), b"legacy");
    assert!(!path.exists());
    stage
        .finish_with_legacy(Publish::Create, &[OsStr::new("state.json.tmp")])
        .unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"new");
    assert!(!legacy.exists());

    fs::write(&legacy, b"legacy again").unwrap();
    let stage = AtomicFile::prepare(&path).unwrap();
    stage.as_file().write_all(b"rejected").unwrap();
    assert_eq!(
        stage
            .finish_with_legacy(Publish::Create, &[OsStr::new("state.json.tmp")])
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::AlreadyExists
    );
    assert_eq!(fs::read(&path).unwrap(), b"new");
    assert_eq!(fs::read(&legacy).unwrap(), b"legacy again");
}

#[test]
fn invalid_legacy_names_cannot_remove_protected_files() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("parent");
    fs::create_dir(&dir).unwrap();
    fs::create_dir(dir.join("nested")).unwrap();
    let destination = dir.join("state.json");
    let escape = root.path().join("escape");
    let nested = dir.join("nested/file");
    fs::write(&destination, b"destination").unwrap();
    fs::write(&escape, b"escape").unwrap();
    fs::write(&nested, b"nested").unwrap();
    let mut worker = Worker::start(&dir, "live", "stage");
    let live = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .find(|name| name.to_string_lossy().starts_with(".koi-state-"))
        .unwrap();
    let live_path = dir.join(&live);
    let reserved = OsStr::new(".koi-state-0123456789abcdef0123456789abcdef.tmp");
    for name in [
        OsStr::new("state.json"),
        OsStr::new("../escape"),
        OsStr::new("nested/file"),
        OsStr::new("bad\0name"),
        reserved,
        live.as_os_str(),
    ] {
        assert_eq!(
            write_with_legacy(&destination, b"rejected", &[name])
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert_eq!(fs::read(&destination).unwrap(), b"destination");
        assert_eq!(fs::read(&escape).unwrap(), b"escape");
        assert_eq!(fs::read(&nested).unwrap(), b"nested");
        assert_eq!(fs::read(&live_path).unwrap(), b"live");
    }
    worker.finish();
}

#[test]
fn legacy_ascii_case_alias_cannot_remove_a_live_stage() {
    let dir = tempfile::tempdir().unwrap();
    let first_path = dir.path().join("first.json");
    let first = AtomicFile::prepare(&first_path).unwrap();
    first.as_file().write_all(b"first").unwrap();
    let live = fs::read_dir(dir.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let alias = live.file_name().unwrap().to_str().unwrap().to_uppercase();
    let second_path = dir.path().join("second.json");
    let second = AtomicFile::prepare(&second_path).unwrap();
    second.as_file().write_all(b"second").unwrap();
    let result = second.finish_with_legacy(Publish::Replace, &[OsStr::new(&alias)]);
    assert_eq!(fs::read(&live).unwrap(), b"first");
    assert!(!second_path.exists());
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
    first.finish(Publish::Create).unwrap();
    assert_eq!(fs::read(first_path).unwrap(), b"first");
}

#[test]
fn legacy_ascii_case_alias_cannot_remove_the_destination() {
    for publish in [Publish::Replace, Publish::Create] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, b"original").unwrap();
        let stage = AtomicFile::prepare(&path).unwrap();
        stage.as_file().write_all(b"replacement").unwrap();
        let result = stage.finish_with_legacy(publish, &[OsStr::new("STATE.JSON")]);
        assert_eq!(fs::read(&path).unwrap(), b"original");
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(stages(dir.path()), 0);
    }
}

#[test]
fn ascii_case_spellings_of_the_staging_namespace_are_reserved() {
    for name in [
        ".KOI-STATE-0123456789abcdef0123456789abcdef.tmp",
        ".koi-state-0123456789abcdef0123456789abcdef.TMP",
        ".KoI-StAtE-0123456789ABCDEF0123456789ABCDEF.TmP",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        fs::write(&path, b"protected").unwrap();
        let result = AtomicFile::prepare(&path);
        assert_eq!(fs::read(&path).unwrap(), b"protected");
        assert_eq!(
            result.err().unwrap().kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        let destination = dir.path().join("state.json");
        let result = write_with_legacy(&destination, b"new", &[OsStr::new(name)]);
        assert_eq!(fs::read(&path).unwrap(), b"protected");
        assert!(!destination.exists());
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
    }
}

#[test]
fn destination_inode_alias_cannot_replace_a_live_stage() {
    let dir = tempfile::tempdir().unwrap();
    let first_path = dir.path().join("first.json");
    let first = AtomicFile::prepare(&first_path).unwrap();
    first.as_file().write_all(b"first").unwrap();
    let live = fs::read_dir(dir.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let alias = dir.path().join("ordinary.json");
    fs::hard_link(&live, &alias).unwrap();
    let result = AtomicFile::prepare(&alias);
    assert_eq!(fs::read(&live).unwrap(), b"first");
    assert_eq!(fs::read(&alias).unwrap(), b"first");
    assert_eq!(
        result.err().unwrap().kind(),
        std::io::ErrorKind::InvalidInput
    );
    first.finish(Publish::Create).unwrap();
    assert_eq!(fs::read(&first_path).unwrap(), b"first");
    assert_eq!(fs::read(&alias).unwrap(), b"first");
}

#[test]
fn legacy_inode_alias_cannot_remove_a_live_stage() {
    let dir = tempfile::tempdir().unwrap();
    let first_path = dir.path().join("first.json");
    let first = AtomicFile::prepare(&first_path).unwrap();
    first.as_file().write_all(b"first").unwrap();
    let live = fs::read_dir(dir.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let second_path = dir.path().join("second.json");
    let second = AtomicFile::prepare(&second_path).unwrap();
    second.as_file().write_all(b"second").unwrap();
    fs::hard_link(&live, dir.path().join("legacy")).unwrap();
    let result = second.finish_with_legacy(Publish::Replace, &[OsStr::new("legacy")]);
    assert_eq!(fs::read(&live).unwrap(), b"first");
    assert_eq!(fs::read(dir.path().join("legacy")).unwrap(), b"first");
    assert!(!second_path.exists());
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
    first.finish(Publish::Create).unwrap();
    assert_eq!(fs::read(first_path).unwrap(), b"first");
}

#[test]
fn legacy_inode_alias_cannot_remove_its_own_stage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    let stage = AtomicFile::prepare(&path).unwrap();
    stage.as_file().write_all(b"new").unwrap();
    let live = fs::read_dir(dir.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::hard_link(live, dir.path().join("legacy")).unwrap();
    let result = stage.finish_with_legacy(Publish::Replace, &[OsStr::new("legacy")]);
    assert_eq!(fs::read(dir.path().join("legacy")).unwrap(), b"new");
    assert!(!path.exists());
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(stages(dir.path()), 0);
}

#[test]
fn destination_inode_alias_created_after_preparation_cannot_publish() {
    for publish in [Publish::Replace, Publish::Create] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let stage = AtomicFile::prepare(&path).unwrap();
        stage.as_file().write_all(b"staged").unwrap();
        let live = fs::read_dir(dir.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        fs::hard_link(live, &path).unwrap();
        let result = stage.finish(publish);
        assert_eq!(fs::read(&path).unwrap(), b"staged");
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(stages(dir.path()), 0);
    }
}

#[test]
fn interrupted_create_recovers_its_stage_without_removing_the_destination() {
    for publish in [Publish::Create, Publish::Replace] {
        let dir = tempfile::tempdir().unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "subprocess_interrupted_create", "--nocapture"])
            .env("KOI_STATE_INTERRUPTED_CREATE_DIR", dir.path())
            .stdin(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let path = dir.path().join("state.json");
        let abandoned = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|entry| {
                entry
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".koi-state-")
            })
            .unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"published");
        assert_eq!(fs::read(&abandoned).unwrap(), b"published");
        assert_eq!(stages(dir.path()), 1);
        let result = AtomicFile::prepare(&path);
        assert_eq!(fs::read(&path).unwrap(), b"published");
        assert!(!abandoned.exists());
        let retry = result.unwrap_or_else(|error| panic!("recovery rejected publication: {error}"));
        retry.as_file().write_all(b"replacement").unwrap();
        match publish {
            Publish::Create => {
                assert_eq!(
                    retry.finish(publish).unwrap_err().kind(),
                    std::io::ErrorKind::AlreadyExists
                );
                assert_eq!(fs::read(&path).unwrap(), b"published");
            }
            Publish::Replace => {
                retry.finish(publish).unwrap();
                assert_eq!(fs::read(&path).unwrap(), b"replacement");
            }
        }
        assert_eq!(stages(dir.path()), 0);
    }
}

#[test]
fn subprocess_interrupted_create() {
    let Some(dir) = std::env::var_os("KOI_STATE_INTERRUPTED_CREATE_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    let path = dir.join("state.json");
    let stage = AtomicFile::prepare(&path).unwrap();
    stage.as_file().write_all(b"published").unwrap();
    stage.as_file().sync_all().unwrap();
    let live = fs::read_dir(&dir).unwrap().next().unwrap().unwrap().path();
    fs::hard_link(live, path).unwrap();
    std::process::exit(0);
}

#[test]
fn destination_inode_alias_of_an_inaccessible_stage_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let inaccessible = dir
        .path()
        .join(".koi-state-0123456789abcdef0123456789abcdef.tmp");
    let path = dir.path().join("state.json");
    fs::write(&inaccessible, b"inaccessible").unwrap();
    fs::hard_link(&inaccessible, &path).unwrap();
    fs::set_permissions(&inaccessible, fs::Permissions::from_mode(0o000)).unwrap();
    let result = AtomicFile::prepare(&path);
    fs::set_permissions(&inaccessible, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(fs::read(&inaccessible).unwrap(), b"inaccessible");
    assert_eq!(fs::read(&path).unwrap(), b"inaccessible");
    assert_eq!(
        result.err().unwrap().kind(),
        std::io::ErrorKind::InvalidInput
    );
    assert_eq!(stages(dir.path()), 1);
}

#[test]
fn destination_alias_scan_does_not_skip_abandoned_stage_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let abandoned = dir
        .path()
        .join(".koi-state-0123456789abcdef0123456789abcdef.tmp");
    let path = dir.path().join("state.json");
    fs::write(&path, b"original").unwrap();
    fs::write(&abandoned, b"abandoned").unwrap();
    let stage = AtomicFile::prepare(&path).unwrap();
    stage.as_file().write_all(b"published").unwrap();
    assert!(!abandoned.exists());
    assert_eq!(stages(dir.path()), 1);
    assert_eq!(fs::read(&path).unwrap(), b"original");
    stage.finish(Publish::Replace).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"published");
    assert_eq!(stages(dir.path()), 0);
}

#[test]
fn legacy_cleanup_can_remove_an_old_destination_hard_link() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    let legacy = dir.path().join("legacy");
    let retained = dir.path().join("retained");
    fs::write(&path, b"original").unwrap();
    fs::hard_link(&path, &legacy).unwrap();
    fs::hard_link(&path, &retained).unwrap();
    write_with_legacy(&path, b"published", &[OsStr::new("legacy")]).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"published");
    assert_eq!(fs::read(retained).unwrap(), b"original");
    assert!(!legacy.exists());
    assert_eq!(stages(dir.path()), 0);
}

#[cfg(target_os = "macos")]
#[test]
fn unicode_filesystem_aliases_cannot_replace_or_remove_a_live_stage() {
    let dir = tempfile::tempdir().unwrap();
    let first_path = dir.path().join("first.json");
    let first = AtomicFile::prepare(&first_path).unwrap();
    first.as_file().write_all(b"first").unwrap();
    let live = fs::read_dir(dir.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let name = live.file_name().unwrap().to_str().unwrap();
    for alias in [name.replacen('k', "K", 1), name.replacen('s', "ſ", 1)] {
        let alias_path = dir.path().join(&alias);
        let Ok(metadata) = fs::symlink_metadata(&alias_path) else {
            continue;
        };
        assert_eq!(metadata.ino(), first.as_file().metadata().unwrap().ino());
        let result = AtomicFile::prepare(&alias_path);
        assert_eq!(fs::read(&live).unwrap(), b"first");
        assert_eq!(
            result.err().unwrap().kind(),
            std::io::ErrorKind::InvalidInput
        );
        let second_path = dir.path().join("second.json");
        let second = AtomicFile::prepare(&second_path).unwrap();
        second.as_file().write_all(b"second").unwrap();
        let result = second.finish_with_legacy(Publish::Replace, &[OsStr::new(&alias)]);
        assert_eq!(fs::read(&live).unwrap(), b"first");
        assert_eq!(fs::read(&alias_path).unwrap(), b"first");
        assert!(!second_path.exists());
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
    }
    first.finish(Publish::Create).unwrap();
    assert_eq!(fs::read(first_path).unwrap(), b"first");
}

#[cfg(target_os = "macos")]
#[test]
fn unicode_destination_alias_cannot_remove_the_publication() {
    for publish in [Publish::Replace, Publish::Create] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("café-state.json");
        let alias = "cafe\u{301}-STATE.json";
        fs::write(&path, b"original").unwrap();
        let Ok(metadata) = fs::symlink_metadata(dir.path().join(alias)) else {
            continue;
        };
        assert_eq!(metadata.ino(), fs::metadata(&path).unwrap().ino());
        if matches!(publish, Publish::Create) {
            fs::remove_file(&path).unwrap();
        }
        let stage = AtomicFile::prepare(&path).unwrap();
        stage.as_file().write_all(b"published").unwrap();
        let result = stage.finish_with_legacy(publish, &[OsStr::new(alias)]);
        assert_eq!(fs::read(&path).unwrap(), b"published");
        assert_eq!(fs::read(dir.path().join(alias)).unwrap(), b"published");
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(stages(dir.path()), 0);
    }
}

#[test]
fn ordinary_unicode_destinations_replace_inaccessible_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("café-state.json");
    fs::write(&path, b"original").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    write(&path, b"published").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"published");
}

#[test]
fn legacy_symlink_cleanup_preserves_its_target() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    let legacy = dir.path().join("legacy");
    fs::write(&target, b"target").unwrap();
    symlink(&target, &legacy).unwrap();
    write_with_legacy(&dir.path().join("state"), b"new", &[OsStr::new("legacy")]).unwrap();
    assert!(legacy.symlink_metadata().is_err());
    assert_eq!(fs::read(target).unwrap(), b"target");
}

#[test]
fn legacy_symlink_to_the_destination_is_unlinked_without_removing_the_publication() {
    for publish in [Publish::Replace, Publish::Create] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let legacy = dir.path().join("legacy");
        symlink(&path, &legacy).unwrap();
        let stage = AtomicFile::prepare(&path).unwrap();
        stage.as_file().write_all(b"published").unwrap();
        stage
            .finish_with_legacy(publish, &[OsStr::new("legacy")])
            .unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"published");
        assert!(legacy.symlink_metadata().is_err());
    }
}

#[test]
fn legacy_cleanup_follows_the_original_parent_after_rename() {
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("original");
    let moved = dir.path().join("moved");
    fs::create_dir(&original).unwrap();
    fs::write(original.join("legacy"), b"old").unwrap();
    let stage = AtomicFile::prepare(&original.join("state")).unwrap();
    stage.as_file().write_all(b"new").unwrap();
    fs::rename(&original, &moved).unwrap();
    fs::create_dir(&original).unwrap();
    fs::write(original.join("legacy"), b"protected").unwrap();
    stage
        .finish_with_legacy(Publish::Replace, &[OsStr::new("legacy")])
        .unwrap();
    assert_eq!(fs::read(moved.join("state")).unwrap(), b"new");
    assert!(!moved.join("legacy").exists());
    assert_eq!(fs::read(original.join("legacy")).unwrap(), b"protected");
}

#[test]
fn a_destination_cannot_be_mistaken_for_a_recoverable_stage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir
        .path()
        .join(".koi-state-0123456789abcdef0123456789abcdef.tmp");
    fs::write(&path, b"existing file").unwrap();
    assert_eq!(
        AtomicFile::prepare(&path).err().unwrap().kind(),
        std::io::ErrorKind::InvalidInput
    );
    assert_eq!(fs::read(&path).unwrap(), b"existing file");
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn publication_is_whole_and_create_never_clobbers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    let stage = AtomicFile::prepare(&path).unwrap();
    assert_eq!(
        stage.as_file().metadata().unwrap().permissions().mode() & 0o777,
        0o600
    );
    stage.as_file().write_all(b"whole").unwrap();
    assert!(!path.exists());
    stage.finish(Publish::Create).unwrap();
    let stage = AtomicFile::prepare(&path).unwrap();
    stage.as_file().write_all(b"replacement").unwrap();
    assert_eq!(
        stage.finish(Publish::Create).unwrap_err().kind(),
        std::io::ErrorKind::AlreadyExists
    );
    assert_eq!(fs::read(&path).unwrap(), b"whole");
    write(&path, b"replacement").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"replacement");
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn drops_and_failed_publications_remove_the_stage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    let stage = AtomicFile::prepare(&path).unwrap();
    stage.as_file().write_all(b"unfinished").unwrap();
    drop(stage);
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    fs::create_dir(&path).unwrap();
    assert!(write(&path, b"not a directory").is_err());
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn recovery_ignores_other_names_directories_and_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("user.tmp");
    fs::write(&target, b"mine").unwrap();
    fs::create_dir(
        dir.path()
            .join(".koi-state-0123456789abcdef0123456789abcdef.tmp"),
    )
    .unwrap();
    symlink(
        &target,
        dir.path()
            .join(".koi-state-abcdef0123456789abcdef0123456789.tmp"),
    )
    .unwrap();
    fs::write(dir.path().join(".koi-state-wrong-length.tmp"), b"mine too").unwrap();
    recover(dir.path()).unwrap();
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 4);
    assert_eq!(fs::read(target).unwrap(), b"mine");
}

#[test]
fn publication_and_drop_follow_the_original_parent_after_it_moves() {
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("original");
    let moved = dir.path().join("moved");
    fs::create_dir(&original).unwrap();
    let stage = AtomicFile::prepare(&original.join("state.json")).unwrap();
    stage.as_file().write_all(b"complete").unwrap();
    fs::rename(&original, &moved).unwrap();
    fs::create_dir(&original).unwrap();
    write(&original.join("state.json"), b"replacement directory").unwrap();
    stage.finish(Publish::Replace).unwrap();
    assert_eq!(
        fs::read(original.join("state.json")).unwrap(),
        b"replacement directory"
    );
    assert_eq!(fs::read(moved.join("state.json")).unwrap(), b"complete");
    assert_eq!(stages(&moved), 0);
    let stage = AtomicFile::prepare(&original.join("next.json")).unwrap();
    let again = dir.path().join("again");
    fs::rename(&original, &again).unwrap();
    drop(stage);
    assert_eq!(stages(&again), 0);
}

#[test]
fn a_replaced_stage_cannot_publish_bytes_from_another_inode() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    let stage = AtomicFile::prepare(&path).unwrap();
    stage.as_file().write_all(b"original").unwrap();
    let name = fs::read_dir(dir.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::rename(&name, dir.path().join("displaced")).unwrap();
    fs::write(&name, b"other inode").unwrap();
    assert!(stage.finish(Publish::Replace).is_err());
    assert!(!path.exists());
    assert_eq!(fs::read(name).unwrap(), b"other inode");
}

struct Worker {
    child: Child,
    ready: PathBuf,
    release: PathBuf,
}

impl Worker {
    fn start(dir: &Path, name: &str, mode: &str) -> Self {
        let ready = dir.join(format!("{name}.ready"));
        let release = dir.join(format!("{name}.release"));
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "subprocess_worker", "--nocapture"])
            .env("KOI_STATE_WORKER", mode)
            .env("KOI_STATE_DIR", dir)
            .env("KOI_STATE_NAME", name)
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let mut worker = Self {
            child,
            ready,
            release,
        };
        let deadline = Instant::now() + Duration::from_secs(15);
        while !worker.ready.exists() {
            assert!(
                worker.child.try_wait().unwrap().is_none(),
                "worker exited before staging"
            );
            assert!(
                Instant::now() < deadline,
                "worker did not stage within 15 seconds"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        worker
    }

    fn finish(&mut self) {
        fs::write(&self.release, b"go").unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success());
                return;
            }
            assert!(
                Instant::now() < deadline,
                "worker did not finish within 15 seconds"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn killed_stage_is_removed_on_restart_without_publishing() {
    let dir = tempfile::tempdir().unwrap();
    let worker = Worker::start(dir.path(), "crashed", "stage");
    assert!(!dir.path().join("state.json").exists());
    drop(worker);
    assert_eq!(stages(dir.path()), 1);
    recover(dir.path()).unwrap();
    assert_eq!(stages(dir.path()), 0);
    assert!(!dir.path().join("state.json").exists());
    write(&dir.path().join("state.json"), b"restart").unwrap();
    assert_eq!(fs::read(dir.path().join("state.json")).unwrap(), b"restart");
}

#[test]
fn recovery_removes_write_only_abandoned_stage_and_skips_inaccessible_stage() {
    let dir = tempfile::tempdir().unwrap();
    let abandoned = dir
        .path()
        .join(".koi-state-0123456789abcdef0123456789abcdef.tmp");
    let inaccessible = dir
        .path()
        .join(".koi-state-abcdef0123456789abcdef0123456789.tmp");
    fs::write(&abandoned, b"abandoned").unwrap();
    fs::write(&inaccessible, b"inaccessible").unwrap();
    fs::set_permissions(&abandoned, fs::Permissions::from_mode(0o200)).unwrap();
    fs::set_permissions(&inaccessible, fs::Permissions::from_mode(0o000)).unwrap();
    assert_eq!(recover(dir.path()).unwrap(), 1);
    assert!(!abandoned.exists());
    assert!(inaccessible.exists());
    write(&dir.path().join("state.json"), b"published").unwrap();
    assert_eq!(
        fs::read(dir.path().join("state.json")).unwrap(),
        b"published"
    );
    assert!(inaccessible.exists());
    fs::set_permissions(&inaccessible, fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn recovery_preserves_a_write_only_live_stage() {
    let dir = tempfile::tempdir().unwrap();
    let mut worker = Worker::start(dir.path(), "live", "write-only");
    assert_eq!(recover(dir.path()).unwrap(), 0);
    assert_eq!(stages(dir.path()), 1);
    write(&dir.path().join("other.json"), b"other").unwrap();
    assert_eq!(stages(dir.path()), 1);
    worker.finish();
    assert_eq!(fs::read(dir.path().join("state.json")).unwrap(), b"live");
}

#[test]
fn live_writer_survives_another_process_recovery_and_publication() {
    let dir = tempfile::tempdir().unwrap();
    let mut first = Worker::start(dir.path(), "first", "stage");
    let mut second = Worker::start(dir.path(), "second", "stage");
    recover(dir.path()).unwrap();
    assert_eq!(stages(dir.path()), 2);
    first.finish();
    recover(dir.path()).unwrap();
    assert_eq!(stages(dir.path()), 1);
    second.finish();
    assert_eq!(stages(dir.path()), 0);
    assert_eq!(fs::read(dir.path().join("state.json")).unwrap(), b"second");
}

#[test]
fn concurrent_writers_complete_with_one_whole_publication() {
    let dir = tempfile::tempdir().unwrap();
    let mut first = Worker::start(dir.path(), "first", "repeat");
    let mut second = Worker::start(dir.path(), "second", "repeat");
    fs::write(&first.release, b"go").unwrap();
    second.finish();
    first.finish();
    assert_eq!(stages(dir.path()), 0);
    let bytes = fs::read(dir.path().join("state.json")).unwrap();
    assert!(bytes == b"first".repeat(4096) || bytes == b"second".repeat(4096));
}

fn stages(dir: &Path) -> usize {
    fs::read_dir(dir)
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".koi-state-")
        })
        .count()
}

#[test]
fn subprocess_worker() {
    let Ok(mode) = std::env::var("KOI_STATE_WORKER") else {
        return;
    };
    let dir = PathBuf::from(std::env::var_os("KOI_STATE_DIR").unwrap());
    let name = std::env::var("KOI_STATE_NAME").unwrap();
    let stage = AtomicFile::prepare(&dir.join("state.json")).unwrap();
    stage.as_file().write_all(name.as_bytes()).unwrap();
    if mode == "write-only" {
        stage
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o200))
            .unwrap();
    }
    fs::write(dir.join(format!("{name}.ready")), b"ready").unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !dir.join(format!("{name}.release")).exists() {
        assert!(Instant::now() < deadline, "parent did not release worker");
        std::thread::sleep(Duration::from_millis(10));
    }
    if mode == "write-only" {
        stage
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .unwrap();
    }
    stage.finish(Publish::Replace).unwrap();
    if mode == "repeat" {
        for _ in 0..8 {
            write(&dir.join("state.json"), &name.as_bytes().repeat(4096)).unwrap();
        }
    }
}
