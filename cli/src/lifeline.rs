// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsString;
use std::fs::{self, File};
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use koidrive_driver::Paths;
use rustix::fs::{FlockOperation, flock};
use rustix::io::Errno;
use rustix::process::{Signal, getpid, kill_process};

use crate::service;

pub(crate) const LIFELINE: &str = "KOIDRIVE_TEST_LIFELINE";
const INVENTORY: &str = "KOIDRIVE_TEST_INSTALL_INVENTORY";

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
        .map_err(|_| Refusal::Ended)?;
    let real = fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
    let inside = |path: &Path| path.starts_with(&root) || path.starts_with(&real);
    let var = |name| std::env::var_os(name).map(PathBuf::from);
    if !cfg!(feature = "test-install-inventory")
        || !var(INVENTORY).is_some_and(|path| inside(&path))
    {
        return Err(Refusal::Unsafe(format!(
            "reach this machine's installs: build koi with the test-install-inventory feature and set {INVENTORY} inside {}",
            root.display()
        )));
    }
    if let Some(prefix) = var("HOMEBREW_PREFIX").filter(|prefix| !inside(prefix)) {
        return Err(Refusal::Unsafe(format!(
            "use the Homebrew prefix {}, which is outside {}",
            prefix.display(),
            root.display()
        )));
    }
    if let Some(account) = service::account_home() {
        let profile = Paths::from_env().ok();
        let used = (var("HOME").into_iter()).chain(
            profile
                .into_iter()
                .flat_map(|paths| [paths.state_dir, paths.config_file]),
        );
        if let Some(path) = used.into_iter().find(|path| path.starts_with(&account)) {
            return Err(Refusal::Unsafe(format!(
                "use {}, which is in this account's home {}",
                path.display(),
                account.display()
            )));
        }
    }
    Ok((alive, held))
}
