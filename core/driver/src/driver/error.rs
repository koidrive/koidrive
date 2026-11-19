// SPDX-License-Identifier: Apache-2.0

use super::*;

/// A file or directory that a sign-out could not remove after the server had revoked the device. Signing out again
/// retries it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupFailure {
    #[serde(serialize_with = "cleanup_path")]
    pub path: PathBuf,
    pub error: String,
}

fn cleanup_path<S: serde::Serializer>(path: &Path, serializer: S) -> Result<S::Ok, S::Error> {
    path.to_string_lossy().serialize(serializer)
}

#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    #[error("another Koidrive process is using this profile ({})", koidrive_fs::quoted(.0))]
    Busy(PathBuf),
    #[error("this device is not set up; run koi setup first")]
    NotPaired,
    #[error(
        "this profile is already paired as device {device_id}; a profile holds one device credential"
    )]
    AlreadyPaired { device_id: String },
    #[error("a device name is required")]
    EmptyName,
    #[error("a user name is required")]
    EmptyUserName,
    #[error(
        "this is the last active device of its user; after it signs out, only owner recovery adds a device again"
    )]
    LastDevice,
    #[error(
        "the server did not confirm that it revoked this device ({0}); if this is the last active device of its user, only owner recovery adds a device again after it signs out"
    )]
    NotRevoked(String),
    #[error(
        "the saved credential now belongs to device {device_id}, not to the one the server refused, so it was kept; run the command again"
    )]
    CredentialReplaced { device_id: String },
    #[error(transparent)]
    Paths(#[from] PathsError),
    #[error(transparent)]
    Credential(#[from] CredentialError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("the server refused or failed: {0}")]
    Api(#[from] ApiError),
    #[error("no space is called {0:?}")]
    SpaceNotFound(String),
    #[error("several spaces are called {name:?}; name one by id: {}", ids.join(", "))]
    SpaceAmbiguous { name: String, ids: Vec<String> },
    #[error("{} does not exist", koidrive_fs::quoted(.0))]
    FolderNotFound(PathBuf),
    #[error("{} is not a folder", koidrive_fs::quoted(.0))]
    NotAFolder(PathBuf),
    #[error("{} changed after its folder was approved; run koi setup again to review its files", koidrive_fs::quoted(.0))]
    FolderApprovalChanged(PathBuf),
    #[error(
        "the folder overlaps Koidrive's own files at {}; link a folder that neither holds nor sits inside it",
        koidrive_fs::quoted(.0)
    )]
    FolderHoldsProfile(PathBuf),
    #[error("the folder overlaps the linked folder {}", koidrive_fs::quoted(.0))]
    FolderOverlaps(PathBuf),
    #[error("another Koidrive process is syncing {}; stop it, or unlink the folder there", koidrive_fs::quoted(.0))]
    FolderInUse(PathBuf),
    #[error(
        "the folder is linked by device {device_id} of {server}; unlink it there first, or remove its .koidrive/device.json"
    )]
    FolderOfOtherDevice { device_id: String, server: String },
    #[error(
        "the folder was linked by device {device_id} of {server}, not by this profile's device; unlink it and link it again"
    )]
    LinkedByOtherDevice { device_id: String, server: String },
    #[error("the space is already linked to {} on this device", koidrive_fs::quoted(.0))]
    SpaceAlreadyLinked(PathBuf),
    #[error(
        "the space is already linked on this device, but its folder's path is unknown; run koi folder unlink --space {0}, then link the folder again"
    )]
    SpaceUnplaced(String),
    #[error(
        "the folder is marked for space {0}; unlink it there first, or remove its .koidrive folder"
    )]
    MarkerOfOtherSpace(String),
    #[error(
        "the folder's file system cannot rename without replacing, so files cannot be applied safely"
    )]
    UnsupportedFileSystem,
    #[error("{} is not a linked folder", koidrive_fs::quoted(.0))]
    NotLinked(PathBuf),
    #[error(
        "unlinking {} did not finish; unlink it again to finish",
        koidrive_fs::quoted(.0)
    )]
    UnlinkUnfinished(PathBuf),
    #[error(
        "{error}; the versions syncing had set aside are back in the folder as {}",
        brought_back.iter().map(|path| koidrive_fs::quoted(path).to_string()).collect::<Vec<_>>().join(", ")
    )]
    UnlinkFailedAfterBringingBack {
        error: Box<DriverError>,
        brought_back: Vec<PathBuf>,
    },
    #[error(
        "this device was not signed out, because {} cannot be unlinked: {error}",
        koidrive_fs::quoted(.path)
    )]
    UnlinkBlocked { path: PathBuf, error: io::Error },
    #[error("no unanswered question has the id {0:?}")]
    QuestionNotFound(String),
    #[error("the question was answered with {given}")]
    AnsweredOtherwise { given: Answer },
    #[error("the question accepts only {}", allowed.iter().map(|a| a.as_str()).collect::<Vec<_>>().join(" or "))]
    AnswerNotAllowed { allowed: &'static [Answer] },
    #[error("no linked folder syncs the space {0:?}")]
    SpaceNotLinked(String),
    #[error("the restore's {0} is not an id the server gave")]
    InvalidRestore(&'static str),
    #[error("the folder holds no changes the server refused")]
    NothingToLift,
    #[error(
        "{} is not halted as an older copy of the linked folder, so it syncs as it is",
        koidrive_fs::quoted(.0)
    )]
    NotACopy(PathBuf),
    #[error("{} was not relinked: {why}", koidrive_fs::quoted(.path))]
    NotRelinked { path: PathBuf, why: String },
    #[error("{} is not an absolute path", koidrive_fs::quoted(.0))]
    RelativePath(PathBuf),
    #[error("{} is not in a linked folder; koi status lists them", koidrive_fs::quoted(.0))]
    OutsideLinkedFolders(PathBuf),
    #[error("{} does not exist", koidrive_fs::quoted(.0))]
    PathNotFound(PathBuf),
    #[error(
        "{} is not on the server: it has not synced yet, or the folder's rules leave it out",
        koidrive_fs::quoted(.0)
    )]
    NotOnServer(PathBuf),
    #[error("a link's password has 8 to 256 characters")]
    PasswordLength,
    #[error("that is not a share link's ID or URL")]
    NotAShareLink,
    #[error("no share link of this account matches that ID or URL")]
    ShareLinkNotFound,
    #[error("the space has as many active share links as the server allows; revoke one first")]
    ShareLinkLimit,
    #[error("several share links have the ID {0}; name the one to revoke by its URL")]
    ShareLinkAmbiguous(String),
    #[error("the folder changed while its server tree was being read; try again")]
    ShareTreeChanged,
    #[error("the server refused: {}", .0.message())]
    Refused(ServerError),
    #[error(
        "the server waits for the space's owner to confirm the folder's changes, and Koidrive cannot ask for that yet"
    )]
    NeedsConfirmation,
    #[error("the sync state at {} cannot be used: {error}", koidrive_fs::quoted(.path))]
    Store {
        path: PathBuf,
        error: koidrive_store::Error,
    },
    #[error(
        "sync is paused, but {} could not be saved completely: {error}",
        koidrive_fs::quoted(.path)
    )]
    PausedUnsaved { path: PathBuf, error: io::Error },
    #[error("{}: {error}", koidrive_fs::quoted(.path))]
    Io { path: PathBuf, error: io::Error },
}

pub(crate) fn io_error(path: &Path) -> impl FnOnce(io::Error) -> DriverError + '_ {
    move |error| DriverError::Io {
        path: path.to_path_buf(),
        error,
    }
}

pub(crate) fn store_error(path: &Path) -> impl FnOnce(koidrive_store::Error) -> DriverError + '_ {
    move |error| DriverError::Store {
        path: path.to_path_buf(),
        error,
    }
}

/// Lifts the policy halt the store at `path` holds (RFC 0006 section 1, point 5). No task may hold the store.
pub(crate) fn lift_halt(path: &Path, lift: Lift) -> Result<(), DriverError> {
    let lifted = Store::open_existing(path)
        .and_then(|mut store| {
            store.transaction(|tx| {
                let before = tx.engine_state()?;
                let mut commits = Commits::from_state(before.commits.clone()).map_err(|error| {
                    koidrive_store::Error::InvalidEngineState(format!("{error:?}"))
                })?;
                let lifted = commits.lift(lift);
                if lifted.is_ok() {
                    let after = EngineState {
                        commits: commits.state(),
                        ..before.clone()
                    };
                    tx.update_engine_state(&before, &after)?;
                }
                Ok(lifted)
            })
        })
        .map_err(store_error(path))?;
    lifted.map_err(|error| match error {
        LiftError::NotHalted => DriverError::NothingToLift,
        LiftError::NeedsConfirmation => DriverError::NeedsConfirmation,
    })
}
