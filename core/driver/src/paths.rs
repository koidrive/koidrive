// SPDX-License-Identifier: Apache-2.0

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// Where one profile keeps its files. A profile is one device credential and the stores it writes; the Linux CLI
/// uses the XDG base directories, and the macOS app can pass its own container to [`Paths::under`].
///
/// - The configuration file is edited by the person and may be shared between machines. It holds no secrets and no
///   sync state, and it is never synced.
/// - The state directory belongs to this machine and is never copied: the credential, the profile lock, one store
///   per linked space, and the records of changes the CLI made once (see [`Paths::requests`]). A copied credential
///   or store breaks the rule that one credential writes one store (H6 of reports/BOX-71, RFC 0002 section 6).
/// - The runtime directory holds the daemon's socket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    pub config_file: PathBuf,
    pub state_dir: PathBuf,
    pub runtime_dir: PathBuf,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PathsError {
    #[error("HOME is not set to an absolute path, and no XDG directory replaces it")]
    NoHome,
    #[error("{0:?} is not a space id")]
    BadSpaceId(String),
}

/// What Koidrive writes at one path of a profile's state directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateEntry {
    File,
    Folder,
}

impl Paths {
    /// What Koidrive writes at `relative`, a path inside a profile's state directory, or none for a path it never
    /// writes. A folder holds only the entries this names inside it. Older atomic writes left a temporary
    /// `.<name>.<pid>.tmp` or `.tmpXXXXXX` beside a file.
    pub fn state_entry(relative: &Path) -> Option<StateEntry> {
        use StateEntry::{File, Folder};
        let names: Vec<&str> = (relative.iter())
            .map(OsStr::to_str)
            .collect::<Option<_>>()?;
        let entry = match names.as_slice() {
            [
                "credential.json"
                | "pending-claim.json"
                | "credential.next"
                | "lock"
                | "service.lock"
                | "paused"
                | "update-check.json"
                | "update.log"
                | "update.log.1"
                | "setup-state.json"
                | "uninstall.json"
                | "leaving"
                | "leaving.next"
                | "update-check.json.new"
                | "unconfirmed-revocation.json"
                | "unreported-set-aside.json"
                | "daemon.sock",
            ] => File,
            [made] if *made == koidrive_state_file::MADE => File,
            ["spaces" | "requests" | "creates" | "telemetry" | "systemd"]
            | ["telemetry", "held"] => Folder,
            ["creates", name] if is_create(name) => File,
            ["spaces", id] if is_store_dir(id) => Folder,
            [
                "spaces",
                id,
                "store.sqlite"
                | "store.sqlite-wal"
                | "store.sqlite-shm"
                | "store.sqlite-journal"
                | "lifecycle.json"
                | "link.json"
                | "unlinking"
                | "unlink-brought-back.json"
                | "first-sync",
            ] if is_store_dir(id) => File,
            ["requests", name] if hex_named(name, ".json", 64) => File,
            [
                "telemetry",
                "settings.json" | "prompt.json" | "queue-v2.json" | "queue.json" | "lock"
                | "send.lock" | "settings.json.tmp" | "prompt.json.tmp" | "queue.json.tmp"
                | "queue-v2.json.tmp",
            ] => File,
            ["telemetry", "held", name]
                if [".json", ".lock", ".json.tmp"]
                    .iter()
                    .any(|end| hex_named(name, end, 32)) =>
            {
                File
            }
            ["systemd", "koidrive.service"] => File,
            ["systemd", name]
                if name
                    .strip_prefix("koidrive-")
                    .is_some_and(|name| hex_named(name, ".service", 8)) =>
            {
                File
            }
            [folder @ .., name] if is_temporary(folder, name) => File,
            _ => return None,
        };
        Some(entry)
    }

    /// The XDG base directories (`XDG_CONFIG_HOME`, `XDG_STATE_HOME`, `XDG_RUNTIME_DIR`) with their defaults under
    /// `HOME`. Relative values are ignored, as the XDG specification requires. Without `XDG_RUNTIME_DIR` the socket
    /// lives in the state directory.
    pub fn from_env() -> Result<Self, PathsError> {
        Self::from_vars(|name| std::env::var_os(name))
    }

    pub fn from_vars(var: impl Fn(&str) -> Option<OsString>) -> Result<Self, PathsError> {
        let absolute = |name: &str| {
            var(name)
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
        };
        let home = absolute("HOME");
        let base = |name: &str, default: &str| {
            absolute(name)
                .or_else(|| home.as_ref().map(|home| home.join(default)))
                .ok_or(PathsError::NoHome)
        };
        let config_dir = base("XDG_CONFIG_HOME", ".config")?.join("koidrive");
        let state_dir = base("XDG_STATE_HOME", ".local/state")?.join("koidrive");
        let runtime_dir = absolute("XDG_RUNTIME_DIR")
            .map(|dir| dir.join("koidrive"))
            .unwrap_or_else(|| state_dir.clone());
        Ok(Paths {
            config_file: config_dir.join("config.toml"),
            state_dir,
            runtime_dir,
        })
    }

    /// Everything under one directory, for an app container or a test.
    pub fn under(dir: &Path) -> Self {
        Paths {
            config_file: dir.join("config.toml"),
            state_dir: dir.join("state"),
            runtime_dir: dir.join("state"),
        }
    }

    pub fn credential(&self) -> PathBuf {
        self.state_dir.join("credential.json")
    }

    pub fn unconfirmed_revocation(&self) -> PathBuf {
        self.state_dir.join("unconfirmed-revocation.json")
    }

    /// What the folders of a lost credential had set aside, kept until a new credential is stored and reports it.
    pub fn unreported_set_aside(&self) -> PathBuf {
        self.state_dir.join("unreported-set-aside.json")
    }

    /// The creates this profile sent with a request ID and has not seen answered for good, one file each.
    pub fn creates(&self) -> PathBuf {
        self.state_dir.join("creates")
    }

    pub fn pending_claim(&self) -> PathBuf {
        self.state_dir.join("pending-claim.json")
    }

    pub fn lock(&self) -> PathBuf {
        self.state_dir.join("lock")
    }

    /// Present while sync is paused, so that the pause outlives the process that holds the profile.
    pub fn paused(&self) -> PathBuf {
        self.state_dir.join("paused")
    }

    pub fn update_check(&self) -> PathBuf {
        self.state_dir.join("update-check.json")
    }

    /// Locked while a koi picks the binary that runs the profile's service and installs it.
    pub fn service_lock(&self) -> PathBuf {
        self.state_dir.join("service.lock")
    }

    pub fn socket(&self) -> PathBuf {
        match self.runtime_dir == self.state_dir {
            true => self.runtime_dir.join("daemon.sock"),
            false => (self.runtime_dir).join(format!("daemon-{}.sock", self.state_key())),
        }
    }

    /// A short hash of the state directory. It names the profile's socket in a shared runtime directory and its
    /// background service.
    pub fn state_key(&self) -> String {
        let hash = (self.state_dir.as_os_str().as_encoded_bytes().iter())
            .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
                (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
            });
        format!("{:08x}", hash >> 32)
    }

    pub fn spaces(&self) -> PathBuf {
        self.state_dir.join("spaces")
    }

    /// The records of the changes asked for with a request ID, one file per ID, so that running a change again
    /// returns its first outcome.
    pub fn requests(&self) -> PathBuf {
        self.state_dir.join("requests")
    }

    /// The store of one space. Space ids come from the server, so only ids made of hex digits and dashes name a
    /// directory.
    pub fn store(&self, space_id: &str) -> Result<PathBuf, PathsError> {
        if !is_space_id(space_id) {
            return Err(PathsError::BadSpaceId(space_id.to_owned()));
        }
        Ok(self.spaces().join(space_id).join("store.sqlite"))
    }

    pub(crate) fn removing(&self, space_id: &str, attempt: u32) -> PathBuf {
        self.spaces().join(format!("{space_id}{REMOVING}{attempt}"))
    }

    pub(crate) fn is_removing(name: &OsStr) -> bool {
        name.to_str().is_some_and(is_removing)
    }

    /// Creates `dir`, the state folder or a folder inside it. When this call creates the state folder itself, it records
    /// that Koidrive made it; see [`koidrive_state_file::made`].
    pub fn create_state_dir(&self, dir: &Path) -> std::io::Result<()> {
        koidrive_state_file::create_dir(&self.state_dir, dir)
    }

    pub fn private_dirs(&self) -> Vec<PathBuf> {
        let mut dirs = vec![self.state_dir.clone(), self.runtime_dir.clone()];
        dirs.extend(self.config_file.parent().map(Path::to_path_buf));
        dirs
    }
}

fn is_space_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

const REMOVING: &str = ".removing-";

fn is_removing(name: &str) -> bool {
    name.rsplit_once(REMOVING).is_some_and(|(id, attempt)| {
        is_space_id(id) && !attempt.is_empty() && attempt.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn is_store_dir(name: &str) -> bool {
    is_space_id(name) || is_removing(name)
}

fn hex_named(name: &str, end: &str, len: usize) -> bool {
    name.strip_suffix(end)
        .is_some_and(|stem| stem.len() == len && stem.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn is_create(name: &str) -> bool {
    ["claim-", "pair-", "space-", "share-link-"]
        .iter()
        .any(|kind| {
            name.strip_prefix(kind)
                .is_some_and(|name| hex_named(name, ".json", 64))
        })
}

fn is_tempfile_random(text: &str) -> bool {
    text.len() == 6 && text.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn is_temporary(folder: &[&str], name: &str) -> bool {
    let in_folder = folder.is_empty()
        || Paths::state_entry(&folder.iter().collect::<PathBuf>()) == Some(StateEntry::Folder);
    let beside_a_file = || {
        name.strip_prefix('.')
            .and_then(|name| name.strip_suffix(".tmp"))
            .and_then(|name| name.rsplit_once('.'))
            .is_some_and(|(file, pid)| {
                !pid.is_empty()
                    && pid.bytes().all(|byte| byte.is_ascii_digit())
                    && Paths::state_entry(&folder.iter().chain([&file]).collect::<PathBuf>())
                        == Some(StateEntry::File)
            })
    };
    let tempfile = || name.strip_prefix(".tmp").is_some_and(is_tempfile_random);
    in_folder && (beside_a_file() || tempfile())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        }
    }

    #[test]
    fn defaults_live_under_home() {
        let paths = Paths::from_vars(vars(&[("HOME", "/home/ada")])).unwrap();
        assert_eq!(
            paths.config_file,
            Path::new("/home/ada/.config/koidrive/config.toml")
        );
        assert_eq!(
            paths.state_dir,
            Path::new("/home/ada/.local/state/koidrive")
        );
        assert_eq!(
            paths.socket(),
            Path::new("/home/ada/.local/state/koidrive/daemon.sock")
        );
        assert_eq!(
            paths.credential(),
            Path::new("/home/ada/.local/state/koidrive/credential.json")
        );
    }

    #[test]
    fn xdg_directories_win_and_relative_ones_are_ignored() {
        let paths = Paths::from_vars(vars(&[
            ("HOME", "/home/ada"),
            ("XDG_CONFIG_HOME", "/cfg"),
            ("XDG_STATE_HOME", "relative/state"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
        ]))
        .unwrap();
        assert_eq!(paths.config_file, Path::new("/cfg/koidrive/config.toml"));
        assert_eq!(
            paths.state_dir,
            Path::new("/home/ada/.local/state/koidrive")
        );
        assert_eq!(
            paths.socket(),
            Path::new("/run/user/1000/koidrive").join(format!("daemon-{}.sock", paths.state_key()))
        );
    }

    #[test]
    fn profiles_that_share_a_runtime_directory_get_sockets_of_their_own() {
        let profile = |state: &'static str| {
            Paths::from_vars(vars(&[
                ("HOME", "/home/ada"),
                ("XDG_STATE_HOME", state),
                ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ]))
            .unwrap()
        };
        let (work, personal) = (profile("/srv/work"), profile("/srv/personal"));
        assert_ne!(work.socket(), personal.socket());
        assert_eq!(work.socket(), profile("/srv/work").socket());
        assert_eq!(
            work.socket().parent(),
            Some(Path::new("/run/user/1000/koidrive"))
        );
    }

    #[test]
    fn no_home_and_no_xdg_is_an_error() {
        assert_eq!(
            Paths::from_vars(vars(&[("HOME", "relative")])),
            Err(PathsError::NoHome)
        );
    }

    #[test]
    fn only_space_ids_name_store_directories() {
        let paths = Paths::under(Path::new("/p"));
        assert_eq!(
            paths.store("0190f0a0-0000-7000-8000-00000000000a").unwrap(),
            Path::new("/p/state/spaces/0190f0a0-0000-7000-8000-00000000000a/store.sqlite")
        );
        for bad in ["", "..", "a/b", "../etc", "space name"] {
            assert!(paths.store(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_state_layout_names_each_entry_koidrive_writes_by_its_shape() {
        use StateEntry::{File, Folder};
        let space = "0190f0a0-0000-7000-8000-00000000000a";
        let request = format!("requests/{}.json", "ab".repeat(32));
        let run = "cd".repeat(16);
        for (path, entry) in [
            ("credential.json", Some(File)),
            (".credential.json.7.tmp", Some(File)),
            (".tmpA1b2C3", Some(File)),
            ("spaces", Some(Folder)),
            (&format!("spaces/{space}"), Some(Folder)),
            (&format!("spaces/{space}/store.sqlite-wal"), Some(File)),
            (&format!("spaces/{space}/.store.sqlite.12.tmp"), Some(File)),
            (&request, Some(File)),
            (".koidrive-state", Some(File)),
            ("update-check.json.new", Some(File)),
            ("unconfirmed-revocation.json", Some(File)),
            ("unreported-set-aside.json", Some(File)),
            (&format!("spaces/{space}/lifecycle.json"), Some(File)),
            (&format!("spaces/{space}/link.json"), Some(File)),
            (&format!("spaces/{space}/unlinking"), Some(File)),
            (&format!("spaces/{space}/first-sync"), Some(File)),
            (&format!("spaces/{space}.removing-2"), Some(Folder)),
            (&format!("spaces/{space}.removing-2/link.json"), Some(File)),
            (
                &format!("spaces/{space}.removing-2/.store.sqlite.12.tmp"),
                Some(File),
            ),
            (&format!("spaces/{space}.removing-"), None),
            ("spaces/notes.removing-1", None),
            ("creates", Some(Folder)),
            (
                &format!("creates/space-{}.json", "ef".repeat(32)),
                Some(File),
            ),
            ("telemetry/send.lock", Some(File)),
            ("telemetry/held", Some(Folder)),
            (&format!("telemetry/held/{run}.json.tmp"), Some(File)),
            ("systemd/koidrive.service", Some(File)),
            ("systemd/koidrive-0a1b2c3d.service", Some(File)),
            ("plan.pdf", None),
            (".notes.1.tmp", None),
            (".credential.json.tmp", None),
            ("spaces/plan.pdf", None),
            ("spaces/Photos", None),
            ("spaces/Photos/.credential.json.7.tmp", None),
            (&format!("spaces/{space}/notes.txt"), None),
            (&format!("{request}/draft.txt"), None),
            ("requests/letter.docx", None),
            ("requests/.DS_Store", None),
            ("requests/.A1b2C3", None),
            ("requests/.bashrc", None),
            ("creates/notes.json", None),
            (&format!("creates/backup-{}.json", "ef".repeat(32)), None),
            ("telemetry/held/photo.jpg", None),
            ("systemd/backup.service", None),
            ("systemd/koidrive-backup.service", None),
            ("credential.json/keys.txt", None),
        ] {
            assert_eq!(Paths::state_entry(Path::new(path)), entry, "{path}");
        }
    }

    #[test]
    fn every_file_a_linked_folder_writes_is_in_the_state_layout() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let space = "0190f0a0-0000-7000-8000-00000000000a";
        let store = paths.store(space).unwrap();
        koidrive_state_file::create_dir(&paths.state_dir, store.parent().unwrap()).unwrap();
        let link = koidrive_store::Link {
            space_id: space.into(),
            space_name: "Work".into(),
            root: dir.path().join("Work"),
            root_dev: 7,
            root_volume: None,
            server_url: "https://files.example.com/".into(),
            device_id: "d1".into(),
            linked_at_ms: 5,
            found_marker: false,
        };
        let mut opened = koidrive_store::Store::open(&store).unwrap();
        opened.transaction(|tx| tx.set_link(&link)).unwrap();
        crate::link_copy::write(&store, &link).unwrap();
        crate::timing::Timing::initialize(&store, 5, &koidrive_telemetry::Recorder::on());
        let status = serde_json::from_value(serde_json::json!({
            "space_id": space, "space_name": "Work", "path": "/Work",
            "state": "stopped", "history_id": null, "last_seq": null,
            "head_seq": null, "remote_nodes": 0, "local_entries": null,
            "questions": 0, "problem": null,
        }))
        .unwrap();
        let lifecycle =
            crate::lifecycle::Lifecycle::open(&store, status, koidrive_telemetry::Recorder::off());
        lifecycle.persist().unwrap();
        lifecycle.begin_unlink().unwrap();
        let create = crate::pending_create::PendingCreate::new(
            &paths,
            &"https://files.example.com/".parse().unwrap(),
            crate::pending_create::Intent::Space {
                user_id: "u1",
                name: "Work",
            },
        );
        let held = create.hold().unwrap();

        let written = written(&paths.state_dir, Path::new(""));

        drop((opened, held));
        assert!(koidrive_state_file::made(&paths.state_dir));
        for name in [
            "lifecycle.json",
            "link.json",
            "unlinking",
            "first-sync",
            "store.sqlite",
        ] {
            let rel = Path::new("spaces").join(space).join(name);
            assert!(written.contains(&rel), "{name}: {written:?}");
        }
        assert!(
            written
                .iter()
                .any(|rel| rel.starts_with("creates") && rel != Path::new("creates"))
        );
        for rel in written {
            let entry = match paths.state_dir.join(&rel).is_dir() {
                true => StateEntry::Folder,
                false => StateEntry::File,
            };
            assert_eq!(Paths::state_entry(&rel), Some(entry), "{}", rel.display());
        }
    }

    fn written(state: &Path, rel: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for entry in std::fs::read_dir(state.join(rel)).unwrap() {
            let entry = entry.unwrap();
            let rel = rel.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                found.extend(written(state, &rel));
            }
            found.push(rel);
        }
        found
    }
}
