// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use koidrive_state_file::{AtomicFile, Publish, recover, write};

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
    fs::write(dir.join(format!("{name}.ready")), b"ready").unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !dir.join(format!("{name}.release")).exists() {
        assert!(Instant::now() < deadline, "parent did not release worker");
        std::thread::sleep(Duration::from_millis(10));
    }
    stage.finish(Publish::Replace).unwrap();
    if mode == "repeat" {
        for _ in 0..8 {
            write(&dir.join("state.json"), &name.as_bytes().repeat(4096)).unwrap();
        }
    }
}
