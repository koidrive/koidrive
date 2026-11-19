// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use koidrive_net::{Api, ApiError, CredentialStore, Credentials, Staged, Stored};
use koidrive_state_file::Published;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::status::Account;

/// How to use a device again once the server refuses its credential: pairing refuses a profile that holds one.
pub const SET_UP_AGAIN: &str = "delete this device's Koidrive state as https://github.com/koidrive/koidrive/blob/main/docs/guide/uninstall.md#remove-koidrive-from-one-device describes, then set it up again and link its folders again";

/// A device's long-lived credential, as `POST /v1/claim` or `POST /v1/pair/complete` returned it, with the server it
/// belongs to. It lives
/// in a file only its owner can read, in the profile's state directory and never inside a linked folder. One
/// credential is one writer: it is never copied to another machine or shared with another profile (H6 of
/// reports/BOX-71), so a second client on the same machine pairs on its own.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    pub server: String,
    pub user_id: String,
    /// The name given when this device claimed the server; none on a paired device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_name: Option<String>,
    pub device_id: String,
    pub device_name: String,
    pub refresh_credential: String,
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credential")
            .field("server", &self.server)
            .field("user_id", &self.user_id)
            .field("user_name", &self.user_name)
            .field("device_id", &self.device_id)
            .field("device_name", &self.device_name)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[error("cannot use the credential file {}: {source}", koidrive_fs::quoted(.path))]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("the credential file {} is not valid: {reason}", koidrive_fs::quoted(.path))]
    Invalid { path: PathBuf, reason: String },
    #[error(
        "the credential file {} can be read by other users (mode {mode:o}); run chmod 600 on it",
        koidrive_fs::quoted(.path)
    )]
    Exposed { path: PathBuf, mode: u32 },
    #[error("the credential file {} belongs to another user", koidrive_fs::quoted(.path))]
    NotOwned { path: PathBuf },
    #[error("this profile already has a credential at {}", koidrive_fs::quoted(.path))]
    Exists { path: PathBuf },
    #[error(
        "a previous sign-out is awaiting confirmation from the server; run koi device leave again"
    )]
    SignOutPending,
    #[error(
        "cannot keep the credential file {} out of Time Machine backups ({source}); Koidrive does not use a credential that backups would copy. Restore write access to the file (it may be locked), keep the profile on storage that supports extended attributes, then restart Koidrive",
        koidrive_fs::quoted(.path)
    )]
    NotExcludedFromBackups {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

fn signed_out(path: &Path) -> Result<bool, CredentialError> {
    let state = path.parent().unwrap_or(Path::new("."));
    let phase = crate::leave::phase(state).map_err(|source| CredentialError::Io {
        path: state.join("leaving"),
        source,
    })?;
    match phase {
        Some(crate::leave::Phase::Revoking) => Err(CredentialError::SignOutPending),
        Some(crate::leave::Phase::Committed) => Ok(true),
        _ => Ok(false),
    }
}

impl Credential {
    pub fn account(&self) -> Account {
        Account {
            server: self.server.clone(),
            user_id: self.user_id.clone(),
            user_name: self.user_name.clone(),
            device_id: self.device_id.clone(),
            device_name: self.device_name.clone(),
        }
    }

    pub fn net(&self) -> Credentials {
        Credentials {
            user_id: self.user_id.clone(),
            device_id: self.device_id.clone(),
            refresh_credential: self.refresh_credential.clone(),
        }
    }

    pub fn api(&self) -> Result<Api, ApiError> {
        api_for(&self.server)
    }

    /// Reads the credential, or returns `None` when there is none or the profile has signed out. A file that another
    /// user owns or may read is refused rather than used. An unresolved revocation returns [`CredentialError::SignOutPending`].
    pub fn load(path: &Path) -> Result<Option<Self>, CredentialError> {
        if signed_out(path)? {
            return Ok(None);
        }
        read_private(path)?
            .map(|(text, _)| parse(path, &text))
            .transpose()
    }

    /// Reads the credential and its staged slot as one state that the process owning them stored at some moment,
    /// even while it changes them (RFC 0011 section 2): it notes the identity of `credential.next`, reads it, reads
    /// `credential.json`, and starts over if either file was replaced or removed in between. Every change renames a
    /// new file into place or removes one, so unchanged identities mean the two files held what was read. Returns
    /// `None` when there is no credential.
    pub fn load_stored(path: &Path) -> Result<Option<StoredCredential>, CredentialError> {
        if signed_out(path)? {
            return Ok(None);
        }
        Self::load_stored_for_revocation(path)
    }

    pub(crate) fn load_stored_for_revocation(
        path: &Path,
    ) -> Result<Option<StoredCredential>, CredentialError> {
        let next_path = next_path(path);
        for _ in 0..READ_ATTEMPTS {
            let next = read_private(&next_path)?;
            #[cfg(test)]
            tests::between_reads();
            let Some((text, meta)) = read_private(path)? else {
                return Ok(None);
            };
            if identity_at(&next_path)? != next.as_ref().map(|(_, meta)| identity(meta))
                || identity_at(path)? != Some(identity(&meta))
            {
                continue;
            }
            let staged = next
                .map(|(text, _)| {
                    serde_json::from_str::<NextFile>(&text)
                        .map(|next| Staged {
                            refresh_credential: next.refresh_credential,
                            rotate_before_ms: next.rotate_before_ms,
                        })
                        .map_err(|err| CredentialError::Invalid {
                            path: next_path.clone(),
                            reason: err.to_string(),
                        })
                })
                .transpose()?;
            return Ok(Some(StoredCredential {
                credential: parse(path, &text)?,
                staged,
                changed_at: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            }));
        }
        Err(CredentialError::Io {
            path: path.to_path_buf(),
            source: io::Error::other("the credential files kept changing while they were read"),
        })
    }

    /// Writes the credential to a new file readable only by its owner, creating the directory with mode 0700. Fails
    /// with [`CredentialError::Exists`] instead of replacing a credential. The file appears whole or not at all, and on
    /// macOS it is excluded from Time Machine before the credential is written to it.
    pub fn save_new(&self, path: &Path) -> Result<(), CredentialError> {
        NewCredentialFile::create(path)?
            .write(self)?
            .warn_unless_durable(path);
        Ok(())
    }

    pub fn keep_out_of_backups(path: &Path) -> Result<(), CredentialError> {
        let file = File::open(path).map_err(|source| CredentialError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        #[cfg(target_os = "macos")]
        {
            let mut value = [0; 64];
            if let Ok(len) = rustix::fs::fgetxattr(&file, BACKUP_EXCLUSION.0, &mut value)
                && value[..len] == *BACKUP_EXCLUSION.1
            {
                return Ok(());
            }
            tracing::info!(path = %koidrive_fs::quoted(path), "excluding the credential from backups again");
        }
        exclude_from_backups(&file).map_err(|source| CredentialError::NotExcludedFromBackups {
            path: path.to_path_buf(),
            source,
        })
    }
}

pub(crate) struct NewCredentialFile {
    path: PathBuf,
    file: koidrive_state_file::AtomicFile,
}

impl NewCredentialFile {
    pub fn create(path: &Path) -> Result<Self, CredentialError> {
        let dir = path.parent().unwrap_or(Path::new("."));
        koidrive_state_file::create_dir(dir, dir).map_err(|source| CredentialError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let file = koidrive_state_file::AtomicFile::prepare(path).map_err(|source| {
            CredentialError::Io {
                path: path.to_path_buf(),
                source,
            }
        })?;
        exclude_from_backups(file.as_file()).map_err(|source| {
            CredentialError::NotExcludedFromBackups {
                path: path.to_path_buf(),
                source,
            }
        })?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
        })
    }

    pub fn write(self, credential: &Credential) -> Result<Published, CredentialError> {
        let bytes = serde_json::to_vec_pretty(credential).map_err(io::Error::from);
        self.finish(bytes, false)
    }

    fn replace(self, bytes: Vec<u8>) -> Result<Published, CredentialError> {
        self.finish(Ok(bytes), true)
    }

    fn finish(
        self,
        bytes: io::Result<Vec<u8>>,
        replace: bool,
    ) -> Result<Published, CredentialError> {
        let io = |source| CredentialError::Io {
            path: self.path.clone(),
            source,
        };
        let written = (|| {
            self.file.as_file().write_all(&bytes?)?;
            let publish = if replace {
                koidrive_state_file::Publish::Replace
            } else {
                koidrive_state_file::Publish::Create
            };
            self.file.finish(publish)
        })();
        match written {
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                Err(CredentialError::Exists {
                    path: self.path.clone(),
                })
            }
            other => other
                .map(|published| Published {
                    value: (),
                    durable: published.durable,
                })
                .map_err(io),
        }
    }
}

const READ_ATTEMPTS: usize = 10;

pub fn next_path(path: &Path) -> PathBuf {
    path.with_file_name("credential.next")
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NextFile {
    refresh_credential: String,
    rotate_before_ms: i64,
}

/// The credential with its staged slot, read as one state. `changed_at` is when `credential.json` was last written.
#[derive(Clone, Debug)]
pub struct StoredCredential {
    pub credential: Credential,
    pub staged: Option<Staged>,
    pub changed_at: SystemTime,
}

impl StoredCredential {
    pub fn net(&self) -> Stored {
        Stored {
            current: self.credential.net(),
            staged: self.staged.clone(),
            changed_at: self.changed_at,
        }
    }
}

fn read_private(path: &Path) -> Result<Option<(String, fs::Metadata)>, CredentialError> {
    let io = |source| CredentialError::Io {
        path: path.to_path_buf(),
        source,
    };
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path)
    {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(io(err)),
    };
    let meta = file.metadata().map_err(io)?;
    if meta.uid() != rustix::process::getuid().as_raw() {
        return Err(CredentialError::NotOwned {
            path: path.to_path_buf(),
        });
    }
    if meta.mode() & 0o077 != 0 {
        return Err(CredentialError::Exposed {
            path: path.to_path_buf(),
            mode: meta.mode() & 0o777,
        });
    }
    let mut text = String::new();
    file.read_to_string(&mut text).map_err(io)?;
    Ok(Some((text, meta)))
}

fn parse(path: &Path, text: &str) -> Result<Credential, CredentialError> {
    serde_json::from_str(text).map_err(|err| CredentialError::Invalid {
        path: path.to_path_buf(),
        reason: err.to_string(),
    })
}

type Identity = (u64, u64, i64, i64);

fn identity(meta: &fs::Metadata) -> Identity {
    (meta.dev(), meta.ino(), meta.mtime(), meta.mtime_nsec())
}

fn identity_at(path: &Path) -> Result<Option<Identity>, CredentialError> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(identity(&meta))),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(CredentialError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

pub struct CredentialFiles {
    path: PathBuf,
    credential: Mutex<Credential>,
    writes: Arc<Mutex<()>>,
}

impl CredentialFiles {
    pub fn new(path: &Path, credential: Credential, writes: Arc<Mutex<()>>) -> Self {
        CredentialFiles {
            path: path.to_path_buf(),
            credential: Mutex::new(credential),
            writes,
        }
    }

    fn still_signed_in(&self, credential: &Credential) -> io::Result<()> {
        let stored = Credential::load(&self.path).map_err(io::Error::other)?;
        if stored.is_some_and(|stored| {
            (&stored.server, &stored.user_id, &stored.device_id)
                == (
                    &credential.server,
                    &credential.user_id,
                    &credential.device_id,
                )
        }) {
            return Ok(());
        }
        Err(io::Error::other(
            "the profile no longer holds this device's credential",
        ))
    }

    fn write(path: &Path, bytes: Vec<u8>) -> io::Result<()> {
        NewCredentialFile::create(path)
            .and_then(|file| file.replace(bytes))
            .map_err(io::Error::other)?
            .durable
    }

    fn remove_next(&self) -> io::Result<()> {
        match fs::remove_file(next_path(&self.path)) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        File::open(self.path.parent().unwrap_or(Path::new(".")))?.sync_all()
    }
}

impl CredentialStore for CredentialFiles {
    fn stage(&self, staged: &Staged) -> io::Result<()> {
        let _writes = self.writes.lock().unwrap();
        self.still_signed_in(&self.credential.lock().unwrap())?;
        let next = NextFile {
            refresh_credential: staged.refresh_credential.clone(),
            rotate_before_ms: staged.rotate_before_ms,
        };
        Self::write(&next_path(&self.path), serde_json::to_vec_pretty(&next)?)
    }

    fn promote(&self, staged: &Staged) -> io::Result<()> {
        let _writes = self.writes.lock().unwrap();
        let mut credential = self.credential.lock().unwrap();
        self.still_signed_in(&credential)?;
        let promoted = Credential {
            refresh_credential: staged.refresh_credential.clone(),
            ..credential.clone()
        };
        Self::write(&self.path, serde_json::to_vec_pretty(&promoted)?)?;
        *credential = promoted;
        self.remove_next()
    }

    fn clear_staged(&self) -> io::Result<()> {
        let _writes = self.writes.lock().unwrap();
        self.still_signed_in(&self.credential.lock().unwrap())?;
        self.remove_next()
    }
}

/// The extended attribute that keeps a file out of Time Machine backups, and the value `tmutil addexclusion` writes.
#[cfg(target_os = "macos")]
pub(crate) const BACKUP_EXCLUSION: (&str, &[u8]) = (
    "com.apple.metadata:com_apple_backup_excludeItem",
    b"bplist00_\x10\x11com.apple.backupd\x08\x00\x00\x00\x00\x00\x00\x01\x01\x00\x00\x00\x00\x00\x00\x00\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x1c",
);

fn exclude_from_backups(file: &File) -> io::Result<()> {
    #[cfg(test)]
    if tests::REFUSE_EXCLUSION.get() {
        return Err(io::Error::from(io::ErrorKind::Unsupported));
    }
    #[cfg(target_os = "macos")]
    rustix::fs::fsetxattr(
        file,
        BACKUP_EXCLUSION.0,
        BACKUP_EXCLUSION.1,
        rustix::fs::XattrFlags::empty(),
    )?;
    #[cfg(not(target_os = "macos"))]
    let _ = file;
    Ok(())
}

/// The API of the server at `server`. Plain `http` is accepted only on a loopback address, for a server under
/// `wrangler dev`.
pub fn api_for(server: &str) -> Result<Api, ApiError> {
    let url = Url::parse(server).map_err(|err| ApiError::Url(format!("{server}: {err}")))?;
    if url.scheme() == "http" {
        Api::local_development(url)
    } else {
        Api::new(url)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    thread_local! {
        pub(crate) static REFUSE_EXCLUSION: Cell<bool> = const { Cell::new(false) };
        static BETWEEN_READS: RefCell<VecDeque<Box<dyn FnOnce()>>> = RefCell::default();
    }

    pub(super) fn between_reads() {
        if let Some(change) = BETWEEN_READS.with_borrow_mut(VecDeque::pop_front) {
            change();
        }
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn time_machine_excludes(path: &Path) -> bool {
        let out = std::process::Command::new("tmutil")
            .arg("isexcluded")
            .arg(path)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let said = String::from_utf8(out.stdout).unwrap();
        assert!(
            said.starts_with("[Excluded]") || said.starts_with("[Included]"),
            "{said}"
        );
        said.starts_with("[Excluded]")
    }

    fn credential() -> Credential {
        Credential {
            server: "https://files.example.com/".into(),
            user_id: "u1".into(),
            user_name: Some("Ada".into()),
            device_id: "d1".into(),
            device_name: "vm".into(),
            refresh_credential: "secret-refresh".into(),
        }
    }

    #[test]
    fn a_saved_credential_is_private_and_loads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/credential.json");
        assert!(Credential::load(&path).unwrap().is_none());
        credential().save_new(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dir_mode = fs::metadata(path.parent().unwrap()).unwrap().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(Credential::load(&path).unwrap(), Some(credential()));
        let mut names = (fs::read_dir(path.parent().unwrap()).unwrap())
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, [koidrive_state_file::MADE, "credential.json"]);
    }

    #[test]
    fn a_credential_without_a_user_name_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credential.json");
        let paired = Credential {
            user_name: None,
            ..credential()
        };
        paired.save_new(&path).unwrap();
        assert!(!fs::read_to_string(&path).unwrap().contains("user_name"));
        assert_eq!(Credential::load(&path).unwrap(), Some(paired));
    }

    #[test]
    fn a_credential_is_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credential.json");
        credential().save_new(&path).unwrap();
        let other = Credential {
            device_id: "d2".into(),
            ..credential()
        };
        assert!(matches!(
            other.save_new(&path),
            Err(CredentialError::Exists { .. })
        ));
        assert_eq!(Credential::load(&path).unwrap(), Some(credential()));
    }

    #[test]
    fn a_credential_others_can_read_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credential.json");
        credential().save_new(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            Credential::load(&path),
            Err(CredentialError::Exposed { mode: 0o644, .. })
        ));
    }

    #[test]
    fn debug_output_leaves_out_the_refresh_credential() {
        let text = format!("{:?}", credential());
        assert!(text.contains("d1"), "{text}");
        assert!(!text.contains("secret"), "{text}");
    }

    #[test]
    fn plain_http_is_only_for_loopback() {
        assert!(api_for("http://127.0.0.1:8787").is_ok());
        assert!(api_for("https://files.example.com").is_ok());
        assert!(api_for("http://files.example.com").is_err());
        assert!(api_for("files.example.com").is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_credential_file_is_excluded_before_its_first_byte() {
        let home = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
        let staged = NewCredentialFile::create(&home.path().join("credential.json")).unwrap();
        let file = staged.file.as_file();
        assert_eq!(file.metadata().unwrap().len(), 0);
        let mut value = [0; 64];
        let len = rustix::fs::fgetxattr(file, BACKUP_EXCLUSION.0, &mut value).unwrap();
        assert_eq!(&value[..len], BACKUP_EXCLUSION.1);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn time_machine_leaves_out_a_new_credential() {
        let home = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
        let path = home.path().join("state").join("credential.json");
        credential().save_new(&path).unwrap();
        assert!(time_machine_excludes(&path));

        Credential::keep_out_of_backups(&path).unwrap();
        assert!(time_machine_excludes(&path));
    }

    fn files(path: &Path) -> CredentialFiles {
        CredentialFiles::new(path, credential(), Arc::default())
    }

    fn staged(refresh_credential: &str) -> Staged {
        Staged {
            refresh_credential: refresh_credential.into(),
            rotate_before_ms: 7,
        }
    }

    fn slots(path: &Path) -> (String, Option<String>) {
        let stored = Credential::load_stored(path).unwrap().unwrap();
        (
            stored.credential.refresh_credential,
            stored.staged.map(|staged| staged.refresh_credential),
        )
    }

    #[test]
    fn the_age_of_a_credential_is_when_its_file_was_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credential.json");
        credential().save_new(&path).unwrap();
        let written = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(written)
            .unwrap();
        assert_eq!(
            Credential::load_stored(&path).unwrap().unwrap().changed_at,
            written
        );
    }

    #[test]
    fn staging_and_promoting_keep_both_files_private_and_end_with_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credential.json");
        credential().save_new(&path).unwrap();
        let files = files(&path);

        files.stage(&staged("next")).unwrap();
        assert_eq!(slots(&path), ("secret-refresh".into(), Some("next".into())));
        for entry in fs::read_dir(dir.path()).unwrap() {
            let name = entry.unwrap().file_name().into_string().unwrap();
            assert!(
                crate::Paths::state_entry(Path::new(&name)) == Some(crate::StateEntry::File),
                "{name}"
            );
        }
        let mode = fs::metadata(next_path(&path)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        files.promote(&staged("next")).unwrap();
        assert_eq!(slots(&path), ("next".into(), None));
        assert!(!next_path(&path).exists());
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["credential.json"]);

        files.stage(&staged("later")).unwrap();
        files.clear_staged().unwrap();
        assert_eq!(slots(&path), ("next".into(), None));

        let _failing = koidrive_state_file::fail_syncs_after_publication();
        assert!(files.stage(&staged("unsynced")).is_err());
        assert!(files.promote(&staged("unsynced")).is_err());
    }

    #[test]
    fn reading_starts_over_when_a_promotion_and_a_staging_happen_between_its_reads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credential.json");
        credential().save_new(&path).unwrap();
        files(&path).stage(&staged("first")).unwrap();
        let changing = path.clone();
        BETWEEN_READS.with_borrow_mut(|changes| {
            changes.push_back(Box::new(move || {
                let files = files(&changing);
                files.promote(&staged("first")).unwrap();
                files.stage(&staged("second")).unwrap();
            }))
        });
        assert_eq!(slots(&path), ("first".into(), Some("second".into())));
    }

    #[test]
    fn reading_gives_up_on_files_that_keep_changing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credential.json");
        credential().save_new(&path).unwrap();
        for round in 0..READ_ATTEMPTS {
            let changing = path.clone();
            BETWEEN_READS.with_borrow_mut(|changes| {
                changes.push_back(Box::new(move || {
                    files(&changing).stage(&staged(&round.to_string())).unwrap()
                }))
            });
        }
        assert!(matches!(
            Credential::load_stored(&path),
            Err(CredentialError::Io { .. })
        ));
        BETWEEN_READS.with_borrow_mut(VecDeque::clear);
    }
}
