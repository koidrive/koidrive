// SPDX-License-Identifier: Apache-2.0

use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, ErrorKind};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::Duration;

use rustix::fs::{FlockOperation, flock};

/// The variable koi reads its lifeline from; `koidrive-cli` holds the other end of this protocol.
pub const LIFELINE: &str = "KOIDRIVE_TEST_LIFELINE";

const INVENTORY: &str = "KOIDRIVE_TEST_INSTALL_INVENTORY";

const LIFELINE_PREFIX: &str = "koi-lifeline-";
const ENDING: Duration = Duration::from_secs(30);

struct Open {
    roots: [PathBuf; 2],
    lifeline: PathBuf,
}

static OPEN: Mutex<Vec<Open>> = Mutex::new(Vec::new());
static SWEPT: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// A temporary tree for one test's koi profiles and folders. Every koi started by a [`sandboxed`] command stops when
/// the sandbox drops or this process dies. The drop waits until each of them has exited, removes the tree, and fails
/// the test if one is still running or the tree remains.
pub struct Sandbox {
    root: PathBuf,
    lifeline: PathBuf,
    held: Option<File>,
    _owner: File,
}

impl Sandbox {
    pub fn new() -> Self {
        Self::in_dir(&std::env::temp_dir())
    }

    /// For a test whose socket paths must stay short, such as one in `/tmp`.
    pub fn in_dir(parent: &Path) -> Self {
        sweep(parent);
        let made = |prefix| {
            (tempfile::Builder::new().prefix(prefix))
                .tempdir_in(parent)
                .unwrap()
                .keep()
        };
        let lifeline = made(LIFELINE_PREFIX);
        let owner = locked(&lifeline, "owner");
        let root = made("koi-");
        koidrive_state_file::write(&lifeline.join("root"), root.as_os_str().as_bytes()).unwrap();
        File::create(lifeline.join("alive")).unwrap();
        let held = locked(&lifeline, "held");
        OPEN.lock().unwrap().push(Open {
            roots: [root.clone(), fs::canonicalize(&root).unwrap()],
            lifeline: lifeline.clone(),
        });
        Sandbox {
            root,
            lifeline,
            held: Some(held),
            _owner: owner,
        }
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    pub fn lifeline(&self) -> &Path {
        &self.lifeline
    }

    /// The variables [`sandboxed`] sets, for a koi started with an environment the test writes out, such as a
    /// service definition's.
    pub fn vars(&self) -> [(&'static str, &Path); 2] {
        [(LIFELINE, &self.lifeline), (INVENTORY, &self.root)]
    }
}

impl Default for Sandbox {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        OPEN.lock()
            .unwrap()
            .retain(|open| open.lifeline != self.lifeline);
        drop(self.held.take());
        let problem = match ended(&self.lifeline.join("alive")) {
            false => Some(format!(
                "a koi this test started was still running {ENDING:?} after the test ended, so {} stays for a later sweep",
                self.root.display()
            )),
            true => (remove(&self.root, &self.lifeline).err())
                .map(|error| format!("{} was not removed: {error}", self.root.display())),
        };
        let Some(problem) = problem else {
            return;
        };
        match std::thread::panicking() {
            true => eprintln!("{problem}"),
            false => panic!("{problem}"),
        }
    }
}

fn remove(root: &Path, lifeline: &Path) -> io::Result<()> {
    match fs::remove_dir_all(root) {
        Err(error) if error.kind() != ErrorKind::NotFound => Err(error),
        _ => {
            let _ = fs::remove_dir_all(lifeline);
            Ok(())
        }
    }
}

fn locked(lifeline: &Path, name: &str) -> File {
    let file = File::create(lifeline.join(name)).unwrap();
    flock(&file, FlockOperation::LockExclusive).unwrap();
    file
}

fn ended(alive: &Path) -> bool {
    let Ok(alive) = File::open(alive) else {
        return false;
    };
    let (done, wait) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(flock(&alive, FlockOperation::LockExclusive));
    });
    wait.recv_timeout(ENDING).is_ok_and(|locked| locked.is_ok())
}

/// A command that runs `program` with the lifeline of the sandbox `within` lies in, so that any koi it starts, with
/// whatever environment it is given, stops with that sandbox. The command keeps koi's install inventory in the sandbox
/// and leaves out this account's XDG directories and Homebrew prefix.
pub fn sandboxed(program: impl AsRef<OsStr>, within: &Path) -> Command {
    let found = (OPEN.lock().unwrap().iter())
        .find(|open| open.roots.iter().any(|root| within.starts_with(root)))
        .map(|open| (open.lifeline.clone(), open.roots[0].clone()));
    let (lifeline, root) = found.unwrap_or_else(|| {
        panic!(
            "{} is not in a Sandbox, so a koi started there could outlive the test",
            within.display()
        )
    });
    let mut command = Command::new(program);
    command
        .env(LIFELINE, lifeline)
        .env(INVENTORY, root)
        .env_remove("HOMEBREW_PREFIX")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("XDG_RUNTIME_DIR");
    command
}

fn sweep(parent: &Path) {
    let mut swept = SWEPT.lock().unwrap();
    if swept.iter().any(|done| done == parent) {
        return;
    }
    swept.push(parent.to_path_buf());
    let Ok(real) = fs::canonicalize(parent) else {
        return;
    };
    let me = rustix::process::getuid().as_raw();
    let foreign = |path: &Path| {
        fs::symlink_metadata(path)
            .is_ok_and(|meta| !meta.is_dir() || meta.uid() != me || meta.mode() & 0o022 != 0)
    };
    for entry in fs::read_dir(parent).into_iter().flatten().flatten() {
        let lifeline = entry.path();
        if !(entry.file_name().as_bytes()).starts_with(LIFELINE_PREFIX.as_bytes())
            || foreign(&lifeline)
        {
            continue;
        }
        let Ok(owner) = File::open(lifeline.join("owner")) else {
            continue;
        };
        if flock(&owner, FlockOperation::NonBlockingLockExclusive).is_err()
            || File::open(lifeline.join("alive"))
                .is_ok_and(|alive| flock(&alive, FlockOperation::NonBlockingLockExclusive).is_err())
        {
            continue;
        }
        let Ok(root) = fs::read(lifeline.join("root")) else {
            continue;
        };
        let root = PathBuf::from(OsString::from_vec(root));
        let made = root
            .file_name()
            .is_some_and(|name| name.as_bytes().starts_with(b"koi-"))
            && root.parent().and_then(|dir| fs::canonicalize(dir).ok()) == Some(real.clone());
        if made && !foreign(&root) {
            let _ = remove(&root, &lifeline);
        }
    }
}
