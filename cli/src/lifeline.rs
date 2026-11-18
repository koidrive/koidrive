// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, ErrorKind};
use std::os::unix::ffi::OsStringExt;
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;
use std::sync::OnceLock;

use koidrive_driver::Paths;
use rustix::fs::{FlockOperation, flock};
use rustix::io::Errno;
use rustix::process::{Signal, getpid, kill_process};

pub(crate) const LIFELINE: &str = "KOIDRIVE_TEST_LIFELINE";
pub(crate) const INVENTORY: &str = "KOIDRIVE_TEST_INSTALL_INVENTORY";

static SANDBOX: OnceLock<PathBuf> = OnceLock::new();
static ITSELF: OnceLock<PathBuf> = OnceLock::new();

enum Refusal {
    Ended,
    Unsafe(String),
}

pub(crate) fn hold() -> Option<ExitCode> {
    let dir = PathBuf::from(std::env::var_os(LIFELINE)?);
    match tie(&dir) {
        Ok((alive, held)) => {
            std::thread::spawn(move || {
                let _alive = alive;
                while flock(&held, FlockOperation::LockShared) == Err(Errno::INTR) {}
                let _ = kill_process(getpid(), Signal::KILL);
            });
            None
        }
        Err(Refusal::Ended) => Some(ExitCode::SUCCESS),
        Err(Refusal::Unsafe(reason)) => {
            eprintln!("koi: a test must not {reason}");
            Some(ExitCode::from(78))
        }
    }
}

fn tie(dir: &Path) -> Result<(File, File), Refusal> {
    let open = |name: &str| File::open(dir.join(name)).map_err(|_| Refusal::Ended);
    let alive = open("alive")?;
    flock(&alive, FlockOperation::NonBlockingLockShared).map_err(|_| Refusal::Ended)?;
    let held = open("held")?;
    if flock(&held, FlockOperation::NonBlockingLockShared).is_ok() {
        return Err(Refusal::Ended);
    }
    let root = fs::read(dir.join("root"))
        .map(|root| PathBuf::from(OsString::from_vec(root)))
        .and_then(fs::canonicalize)
        .map_err(|_| Refusal::Ended)?;
    if !cfg!(feature = "test-install-inventory") || std::env::var_os(INVENTORY).is_none() {
        return Err(Refusal::Unsafe(format!(
            "reach this machine's installs: build koi with the test-install-inventory feature and set {INVENTORY}"
        )));
    }
    let profile = Paths::from_env().ok();
    let used = (["HOME", INVENTORY, "HOMEBREW_PREFIX"].into_iter())
        .filter_map(|name| std::env::var_os(name).map(PathBuf::from))
        .chain(profile.into_iter().flat_map(|paths| {
            let config_dir =
                (paths.config_file.parent()).map_or_else(PathBuf::new, Path::to_path_buf);
            [
                paths.state_dir,
                config_dir,
                paths.config_file,
                paths.runtime_dir,
            ]
        }));
    if let Some(path) = used.into_iter().find(|path| !inside(&root, path)) {
        return Err(Refusal::Unsafe(format!(
            "use {}, which is outside its sandbox {}",
            path.display(),
            root.display()
        )));
    }
    if let Ok(itself) = std::env::current_exe().and_then(fs::canonicalize) {
        let _ = ITSELF.set(itself);
    }
    let _ = SANDBOX.set(root);
    Ok((alive, held))
}

pub(crate) fn confine(path: &Path) -> io::Result<()> {
    refuse_outside("change", path, |root| {
        (path.parent().zip(path.file_name()))
            .and_then(|(parent, name)| Some(resolved(parent)?.join(name)))
            .is_some_and(|entry| entry.starts_with(root))
    })
}

pub(crate) fn confine_program(path: &Path) -> io::Result<()> {
    confine_resolved("run", path)
}

pub(crate) fn confine_passed(path: &Path) -> io::Result<()> {
    confine_resolved("pass", path)
}

fn confine_resolved(act: &str, path: &Path) -> io::Result<()> {
    refuse_outside(act, path, |root| {
        resolved(path).is_some_and(|real| real.starts_with(root) || ITSELF.get() == Some(&real))
    })
}

fn refuse_outside(act: &str, path: &Path, allowed: impl FnOnce(&Path) -> bool) -> io::Result<()> {
    let Some(root) = SANDBOX.get() else {
        return Ok(());
    };
    match path.is_absolute() && allowed(root) {
        true => Ok(()),
        false => Err(io::Error::new(
            ErrorKind::PermissionDenied,
            format!(
                "a test must not {act} {}, which is outside its sandbox {}",
                path.display(),
                root.display()
            ),
        )),
    }
}

fn inside(root: &Path, path: &Path) -> bool {
    path.is_absolute() && resolved(path).is_some_and(|real| real.starts_with(root))
}

fn resolved(path: &Path) -> Option<PathBuf> {
    let existing = (path.ancestors()).find(|ancestor| ancestor.symlink_metadata().is_ok())?;
    let rest = path.strip_prefix(existing).ok()?;
    if rest.components().any(|part| part == Component::ParentDir) {
        return None;
    }
    match fs::canonicalize(existing) {
        Ok(real) => Some(real.join(rest)),
        Err(error) if error.kind() == ErrorKind::NotFound => resolved(
            &existing
                .parent()?
                .join(fs::read_link(existing).ok()?)
                .join(rest),
        ),
        Err(_) => None,
    }
}
