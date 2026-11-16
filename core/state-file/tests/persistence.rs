// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use koidrive_state_file::{AtomicFile, Publish, recover, write, write_with_legacy};

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
