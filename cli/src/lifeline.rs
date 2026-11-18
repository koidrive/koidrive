// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsString;
use std::fs::{self, File};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use koidrive_driver::Paths;
use rustix::fs::{FlockOperation, flock};
use rustix::io::Errno;
use rustix::process::{Signal, getpid, kill_process};

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
    if let Some(account) = account_home() {
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

fn account_home() -> Option<PathBuf> {
    let mut buffer = vec![0_u8; 1024];
    loop {
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut found = std::ptr::null_mut();
        // SAFETY: every pointer is valid for the call, and `buffer` holds the strings `entry` points into.
        let status = unsafe {
            libc::getpwuid_r(
                libc::getuid(),
                entry.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut found,
            )
        };
        match status {
            libc::ERANGE => buffer.resize(buffer.len() * 2, 0),
            0 if !found.is_null() => {
                // SAFETY: getpwuid_r succeeded, so `found` points to `entry` and `pw_dir` to a string in `buffer`.
                let dir = unsafe { std::ffi::CStr::from_ptr((*found).pw_dir) };
                let dir = PathBuf::from(std::ffi::OsStr::from_bytes(dir.to_bytes()));
                return dir.is_absolute().then_some(dir);
            }
            _ => return None,
        }
    }
}
