// SPDX-License-Identifier: Apache-2.0

//! What the driver reports about itself: the signed-in device and each linked folder.

use std::time::{Duration, UNIX_EPOCH};

use koidrive_engine::question::{RecoveryCause, RecoveryTrigger};
use serde::{Deserialize, Serialize};

use crate::observe::Observing;
use crate::update::Update;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    /// The paired device, or none before `pair`.
    pub account: Option<Account>,
    /// Whether the driver was started: the daemon is running, or the app started it. A paused driver syncs nothing all
    /// the same.
    pub running: bool,
    /// Whether sync is paused until a resume, also across restarts.
    #[serde(default)]
    pub paused: bool,
    /// The Koidrive version of the process that reports the status. None from a daemon too old to send it.
    #[serde(default)]
    pub version: Option<String>,
    /// Whether a newer release is out than `version`. Unknown from a daemon too old to send it.
    #[serde(default)]
    pub update: Update,
    pub folders: Vec<FolderStatus>,
    /// The folders' progress summed while any folder syncs and has a file left to sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<Totals>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub server: String,
    pub user_id: String,
    /// The user's name, known on the device that claimed the server. A paired device does not learn it: no server
    /// response carries it yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_name: Option<String>,
    pub device_id: String,
    pub device_name: String,
}

mod unknown_path {
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(
        path: &Option<String>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(path.as_deref().unwrap_or_default())
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<String>, D::Error> {
        Ok(Some(String::deserialize(deserializer)?).filter(|path| !path.is_empty()))
    }
}

/// What lifts a halt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Remedy {
    /// Fixing the problem, then restarting the folder.
    #[serde(alias = "Restart")]
    Restart,
    /// Putting the folder back where it was. Koidrive checks it again on its own.
    #[serde(alias = "Root")]
    Root,
    /// Relinking the folder, which keeps this copy.
    #[serde(alias = "Relink")]
    Relink,
    /// Unlinking the folder again, which finishes the unlink that stopped.
    #[serde(alias = "Unlink")]
    Unlink,
    /// Fixing the cause, then sending the refused changes again or planning them again.
    Retry,
    /// The space's owner confirming the refused changes.
    Owner,
    /// Setting this device up again.
    SetUpAgain,
    /// The space's owner adding this device back, or unlinking the folder when the space is gone.
    Membership,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderStatus {
    pub space_id: String,
    pub space_name: String,
    /// The folder's absolute path, or none when neither its sync database nor the copy of its link can be read. The
    /// wire carries an unknown path as an empty string.
    #[serde(with = "unknown_path")]
    pub path: Option<String>,
    pub state: FolderState,
    /// The server history and the last seq whose changes are in the store.
    pub history_id: Option<String>,
    pub last_seq: Option<u64>,
    /// The server's head, as of the last `Welcome` or `CaughtUp`.
    pub head_seq: Option<u64>,
    /// Nodes of the remote tree in the store, live and trashed.
    pub remote_nodes: u64,
    /// Files and folders the last observation of the disk found, while running.
    pub local_entries: Option<u64>,
    /// Questions waiting for an answer.
    pub questions: u64,
    /// Why the folder is halted, or what the last transient failure was.
    pub problem: Option<String>,
    /// Whether `Driver::relink` resolves the halt.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub relinkable: bool,
    /// What lifts the halt, while the folder is halted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remedy: Option<Remedy>,
    /// The call the folder puts off, as soon as it waits: why and until when. `problem` shows a wait only once it is
    /// long or keeps failing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<Wait>,
    /// Why the folder is recovering (design §6.4), while it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<RecoveryCause>,
    /// How far the folder's transfers have got, while it has files to send or fetch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<Progress>,
    /// What the folder does while it syncs besides the transfers `progress` counts. None when those transfers are all
    /// it has left, and while it does not sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<Activity>,
    /// Bytes the folder's transfers moved over the network since this process started: each chunk the server stored
    /// from this device and each chunk fetched from the server, once. A chunk the server refused and that is sent
    /// again counts once, so the count is bytes delivered. Chunks the server already held and content found locally
    /// add nothing, and the count never decreases while the process runs. It starts from zero again
    /// when the process restarts, so a reader treats a decrease as a new baseline.
    #[serde(default)]
    pub bytes_moved: u64,
    /// The disks inside the folder whose files the folder keeps as they were, while it is running.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub held_disks: Vec<HeldDisk>,
    /// Entries, relative to the folder, that another device's delete left here and that stay out of sync under the
    /// rules of the deleted folder they were in, until the person deletes or makes them again (design §5.10).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kept_entries: Vec<koidrive_fs::ItemPath>,
}

/// A disk inside a folder whose files stay on the server as they were and do not sync until it is back.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldDisk {
    /// The mount point, relative to the folder.
    pub path: koidrive_fs::ItemPath,
    pub kind: HeldDiskKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeldDiskKind {
    /// The disk is ejected, or another disk is mounted in its place.
    NotConnected,
    /// A disk is mounted over a folder whose files sync, and hides them.
    MountedOver,
}

impl HeldDisk {
    /// What a person reads about the disk in the folder at `folder`.
    pub fn describe(&self, folder: &str) -> String {
        let at = std::path::Path::new(folder).join(&self.path);
        match self.kind {
            HeldDiskKind::NotConnected => format!(
                "Disk not connected: {}. Sync resumes when it's back.",
                koidrive_fs::quoted(&at)
            ),
            HeldDiskKind::MountedOver => format!(
                "Disk mounted over a synced folder: {}. Sync resumes when it's ejected.",
                koidrive_fs::quoted(&at)
            ),
        }
    }
}

/// What a person reads about the entries of a folder at `folder` that stay excluded under the rules of a deleted
/// folder they were kept in ([`FolderStatus::kept_entries`]).
pub fn describe_kept(folder: &str, entries: &[koidrive_fs::ItemPath]) -> String {
    let shown: Vec<String> = (entries.iter())
        .map(|entry| koidrive_fs::quoted(&std::path::Path::new(folder).join(entry)).to_string())
        .collect();
    let (count, stay, release) = match entries.len() {
        1 => (
            "1 entry".to_owned(),
            "stays",
            "Once it's deleted, a new entry at that path",
        ),
        count => (
            format!("{count} entries"),
            "stay",
            "Once one is deleted, a new entry at its path",
        ),
    };
    format!(
        "{count} kept after another device deleted their folder {stay} out of sync under that folder's rules: {}. {release} follows the rules that apply there.",
        shown.join(", ")
    )
}

/// What a person reads about `cause`.
pub fn describe(cause: &RecoveryCause) -> String {
    match (cause.trigger, cause.restore_seq, cause.restore_point_ms) {
        (RecoveryTrigger::SpaceRewound, Some(seq), Some(requested)) => {
            let at = UNIX_EPOCH + Duration::from_millis(requested.unsigned_abs());
            let seconds = requested.unsigned_abs() / 1_000 % 86_400;
            format!(
                "the space was rewound on the server to seq {seq} (requested {} {:02}:{:02} UTC)",
                koidrive_fs::utc_date(at),
                seconds / 3_600,
                seconds / 60 % 60
            )
        }
        (RecoveryTrigger::SpaceRewound, ..) => {
            "the space was rewound or restored on the server".into()
        }
        (RecoveryTrigger::StoreRolledBack, ..) => {
            "the folder's sync state is older than its history on the server".into()
        }
        (RecoveryTrigger::StoreOlderThanDisk, ..) => {
            "the folder's sync state is older than the files on disk".into()
        }
    }
}

/// The files a folder still sends to the server or fetches from it, and how far their content has got
/// (docs/design/driver.md, "Progress counts the files left to sync").
///
/// A file counts once its content differs between the folder and the server and no rule, question or problem keeps
/// it out of sync, until both hold the same content. The progress ends when no file is left, and the next starts from
/// zero. Until then `bytes_done` never decreases, and `bytes_total` grows only when new work arrives, so the fraction
/// falls only when the folder finds more to sync.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    /// Files whose content still goes to the server, the ones being sent included.
    pub files_to_upload: u64,
    /// Files whose content still comes from the server, the ones being fetched included.
    pub files_to_download: u64,
    /// Bytes of the content that the folder and the server both hold: each chunk once it is stored or fetched, or
    /// found on the server already, and the whole content of a file once it is in sync. It never decreases.
    pub bytes_done: u64,
    /// Bytes of the content of every file counted, the synced ones included. A file found later adds its size at
    /// once. A file that leaves without syncing, because it was edited again or removed, takes its bytes not done
    /// out of it.
    pub bytes_total: u64,
    /// The file whose content moved last, relative to the folder.
    pub current: Option<String>,
}

/// What a syncing folder is busy with besides moving file content (docs/design/driver.md, "A syncing folder says
/// what it is doing"). A folder that bootstraps or catches up has an activity, its transfers, or both.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Activity {
    /// The first scan since the folder started walks the disk. `files` counts the files it has found so far.
    Scanning { files: u64 },
    /// The folder reads the space's tree from the server.
    Listing,
    /// The walk is done, and `files` files of `bytes` bytes found on the disk are still to be read, to learn what
    /// changed. Both fall as the folder reads, chunk by chunk.
    Checking { files: u64, bytes: u64 },
    /// Changes that move no file content, such as new folders, renames, moves and removals.
    Applying { changes: u64 },
    /// Nothing is left to sync but what `questions` unanswered questions hold until the person answers.
    AwaitingAnswers { questions: u64 },
    /// The folder waits for the rest of the server's changes or for the server to take its own.
    CatchingUp,
    /// A kind a newer Koidrive reports.
    #[serde(other)]
    Unknown,
}

impl Activity {
    pub(crate) fn live(
        activity: Option<Activity>,
        observing: Option<Observing>,
    ) -> Option<Activity> {
        match (activity, observing) {
            (Some(Activity::Scanning { .. }), Some(Observing::Walking { files })) => {
                Some(Activity::Scanning { files })
            }
            (
                Some(Activity::Scanning { .. } | Activity::Checking { .. }),
                Some(Observing::Reading { files, bytes }),
            ) => Some(Activity::Checking { files, bytes }),
            (activity, _) => activity,
        }
    }
}

/// The files every folder still sends or fetches and the bytes they moved, summed while any folder has a file left
/// (docs/design/driver.md, "Progress counts the files left to sync"). A folder that finishes, abandons a file, goes
/// offline or stops keeps the bytes it moved in the sum until no folder has a file left, so `bytes_done` never
/// decreases until then, and `bytes_total` grows only when new work arrives.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Totals {
    pub files_to_upload: u64,
    pub files_to_download: u64,
    pub bytes_done: u64,
    pub bytes_total: u64,
}

impl FolderStatus {
    pub(crate) fn end_transfers(&mut self) -> bool {
        let ended = self.progress.take().is_some();
        if ended
            && matches!(
                self.state,
                FolderState::Bootstrapping | FolderState::CatchingUp
            )
        {
            self.activity = Some(Activity::CatchingUp);
        }
        ended
    }

    /// Why the folder cannot reach the server, and when it tries again in Unix milliseconds, while it is offline. No
    /// time means it is trying now.
    pub fn unreachable(&self) -> Option<(&Unreachable, Option<i64>)> {
        let wait = (self.wait.as_ref()).filter(|_| self.state == FolderState::Offline)?;
        Some((wait.unreachable.as_ref()?, wait.until_ms))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unreachable {
    /// In plain words, such as "the server's address could not be found".
    pub why: String,
    /// This device has no network connection at all.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_internet: bool,
}

/// A call a folder sends again later, after the server put it off or it kept failing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wait {
    pub kind: WaitKind,
    /// When the call is sent again, in Unix milliseconds, or `None` while it is being sent again.
    pub until_ms: Option<i64>,
    /// Why the server cannot be reached, when that is what the call waits on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unreachable: Option<Unreachable>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitKind {
    /// The server refused with `QUOTA_EXCEEDED`: a daily limit is used up, or the space's storage is full.
    Quota,
    /// The call failed for another reason, such as the network or `RETRY_LATER`, and is tried again.
    Retry,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FolderState {
    /// Linked, but the driver is not running.
    Stopped,
    /// Opening a session with the server.
    Connecting,
    /// Reading the whole remote tree, for a first link or after the server's log no longer reaches the store.
    Bootstrapping,
    /// Applying the server's log up to its head.
    CatchingUp,
    /// The store holds the server's head, and changes arrive as they are committed.
    UpToDate,
    /// The server cannot be reached; the session keeps retrying.
    Offline,
    /// Stopped by a problem; `problem` says which. A folder that was moved away or fails its root check, including one
    /// that `relinkable` flags, syncs again on its own once it passes the check. Any other problem needs a person, and
    /// the folder waits for a restart.
    Halted,
}
