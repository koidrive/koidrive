// SPDX-License-Identifier: Apache-2.0

//! The values that cross the bridge: session state, the events that update it, questions and
//! their answers, problems and errors. Each type is derived from the engine's inputs and outputs
//! in docs/design/sync-engine.md §1, with the section that defines it named in its documentation.
//!
//! Paths inside a linked folder are relative to the folder, use `/` as the separator and never
//! start with one. Folder paths are absolute. Times are Unix milliseconds.

/// What the whole session is doing. The menu bar shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum SyncStatus {
    /// The session is not running: the app has not started it yet, or stopped it.
    Stopped,
    /// The session runs but has nothing to sync, because this device is not signed in or no
    /// folder is linked. The first-run setup fixes it.
    NeedsSetup,
    /// Opening a connection to the server.
    Connecting,
    /// Transferring or applying changes. [`SessionState::progress`] says how far it got.
    Syncing,
    /// Connected and up to date.
    Idle,
    /// The person paused sync. Nothing is uploaded, downloaded or applied to disk until they
    /// resume; local changes made meanwhile are picked up then.
    Paused,
    /// The server cannot be reached. The session keeps retrying on its own.
    Offline,
    /// A linked folder stopped syncing because of a problem the person has to fix.
    /// [`SessionState::problems`] says which folder and why. The other folders keep syncing.
    Halted,
}

/// How far the current sync has got, summed over every linked folder that syncs (docs/design/driver.md,
/// "Progress counts the files left to sync"). The session sends it with [`SyncStatus::Syncing`], at most four
/// times a second, and `None` once no file is left to send or fetch.
///
/// While the session syncs, `bytes_done` never decreases: a folder that finishes, abandons a file, goes offline or
/// stops keeps the bytes it moved in the sum until no folder has a file left. `bytes_total` grows only when new work
/// arrives, so the fraction falls only when a folder finds more to sync.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncProgress {
    /// Files whose content still goes to the server, including the ones being sent.
    pub files_to_upload: u64,
    /// Files whose content still comes from the server, including the ones being fetched.
    pub files_to_download: u64,
    /// Bytes that this device and the server both hold, in both directions. A chunk the server holds already counts
    /// as done without being sent.
    pub bytes_done: u64,
    /// Bytes of the content of every file counted, the synced ones included, in both directions. A file that leaves
    /// without syncing, because it was edited again or removed, takes its bytes not done out of it.
    pub bytes_total: u64,
    /// The file whose content moved last, if any.
    pub current: Option<FolderPath>,
}

/// A path inside a linked folder.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct FolderPath {
    /// The space whose linked folder holds the path.
    pub space_id: String,
    /// The path relative to the folder.
    pub path: String,
}

/// Whether a node is a file or a folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum NodeKind {
    File,
    Folder,
}

/// One change the session synced recently, for the menu bar.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct RecentChange {
    pub space_id: String,
    /// The path after the change; for a deletion, the path it had.
    pub path: String,
    pub kind: NodeKind,
    pub change: ChangeKind,
    pub direction: Direction,
    pub at_ms: i64,
}

/// What happened to a node in a [`RecentChange`].
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ChangeKind {
    Added,
    Edited,
    /// Moved or renamed; `from` is the old path.
    Moved {
        from: String,
    },
    /// Deleted. A local file removed by a remote deletion goes to the local trash (design §6.1).
    Deleted,
    /// Two devices changed the same file, and the server kept both versions: the other one is at
    /// `path` and this one was saved under `original`'s conflict name (design §2.4).
    ConflictCopy {
        original: String,
    },
}

/// Which way a [`RecentChange`] went.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum Direction {
    /// Made on this device and sent to the server.
    Uploaded,
    /// Made on another device and applied here.
    Downloaded,
}

/// A server the app connected to before signing in (`GET /v1/server`).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ServerInfo {
    /// The server's base URL, normalized: no trailing slash.
    pub url: String,
    pub server_version: String,
    /// Whether an owner has claimed the server. An unclaimed server is claimed with the one-time
    /// code from its Worker log; a claimed one accepts a new device with a pairing code from a
    /// device already signed in.
    pub claimed: bool,
}

/// The signed-in device and the server it belongs to.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Account {
    pub server_url: String,
    pub user_id: String,
    /// The name the owner gave when claiming the server. A device that paired does not learn it
    /// from the server's current API, so it is empty there.
    pub user_name: String,
    pub device_id: String,
    pub device_name: String,
}

/// A device signed in to the account.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Device {
    pub device_id: String,
    pub name: String,
    /// The platform the device reported, such as `macos` or `linux`.
    pub platform: String,
    /// True for the device this app runs on.
    pub this_device: bool,
    pub added_at_ms: i64,
}

/// A code that signs in another device of the same account (`POST /v1/pair/start`).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct PairingCode {
    /// The code to type on the new device. Case and separators do not matter.
    pub code: String,
    pub expires_at_ms: i64,
}

/// A space on the server: one synced tree that devices link a folder to.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Space {
    pub space_id: String,
    pub name: String,
}

/// What a space stores, in bytes of chunks (RFC 0010). Each chunk counts once, under the first of
/// live files, the trash and old versions that holds it, so the four byte counts add up to the
/// space's chunk bytes.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SpaceUsage {
    pub live_files: u64,
    pub live_bytes: u64,
    pub trashed_files: u64,
    /// Chunks of trashed files that no live file holds.
    pub trash_bytes: u64,
    /// Replaced versions the space keeps, of live and trashed files.
    pub old_versions: u64,
    /// Chunks that only old versions hold.
    pub old_version_bytes: u64,
    /// Chunks no version holds: uploads not committed yet, and chunks waiting out the grace
    /// period before deletion.
    pub unreferenced_bytes: u64,
}

/// What was deleted in a linked folder, or in a folder inside one, and is still in its space's trash.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Trash {
    /// The linked folder's absolute path.
    pub folder: String,
    /// In path order.
    pub items: Vec<DeletedItem>,
}

/// One deletion in the trash: a file, or a folder with what was deleted with it.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct DeletedItem {
    pub node_id: String,
    /// The path it had.
    pub path: String,
    /// A symbolic link counts as a file.
    pub kind: NodeKind,
    /// 1 for a file; for a folder, the files deleted with it.
    pub files: u64,
    pub bytes: u64,
    /// When its content was saved; none for a folder.
    pub modified_at_ms: Option<i64>,
    /// When it was deleted; none when the server does not say.
    pub deleted_at_ms: Option<i64>,
    /// The device that deleted it; none when the server does not say.
    pub deleted_by_device_id: Option<String>,
    /// That device's name; none for a device of another account or when the server does not say.
    pub deleted_by_device_name: Option<String>,
}

/// The versions the server keeps of a file.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct FileVersions {
    pub path: String,
    pub node_id: String,
    /// Newest first.
    pub versions: Vec<FileVersion>,
}

/// One version of a file the server keeps.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct FileVersion {
    pub version_id: String,
    pub size: u64,
    pub saved_at_ms: i64,
    /// The device that saved it.
    pub device_id: String,
    /// The device's name, or none for a device of another account.
    pub device_name: Option<String>,
    /// True for the file's content now.
    pub current: bool,
}

/// A restore that was asked for. The folder sends it with its next commit; [`RestoreRequest`] says how it went.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct RestoreAsked {
    pub request_id: String,
    /// The linked folder's absolute path.
    pub folder: String,
    pub path: String,
}

/// A restore asked for in a linked folder, with how it went.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct RestoreRequest {
    pub id: String,
    /// Where its node is, or where it was while it is in the trash; none when the folder does not know it.
    pub path: Option<String>,
    pub asked_at_ms: i64,
    pub target: RestoreTarget,
    pub state: RestoreState,
}

/// What a restore brings back.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum RestoreTarget {
    /// A deleted file or folder, from the trash.
    Deleted { node_id: String },
    /// An earlier version of a file, as its content now. `saved_at_ms` is when that version was saved; none when the
    /// folder does not know it.
    Version {
        node_id: String,
        version_id: String,
        saved_at_ms: Option<i64>,
    },
}

/// How a [`RestoreRequest`] went.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum RestoreState {
    /// Not sent yet, or sent and not answered; it goes out once the folder syncs. With `held_by_problem`, a change
    /// this device made to the node was refused, and the restore waits until that problem is cleared.
    Pending {
        held_by_problem: bool,
    },
    Restored,
    /// Restored under `name`, because its name was taken.
    RestoredAs {
        name: String,
    },
    /// Another device changed the file meanwhile, so the version came back as the new file `name`.
    RestoredAsCopy {
        node_id: String,
        name: String,
    },
    Refused {
        reason: RestoreRefusal,
    },
    /// It brings back more than one commit can hold.
    TooLarge,
}

/// Why the server refused a restore.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum RestoreRefusal {
    /// The server no longer has it, or the folder to restore into: it was purged or it expired.
    Gone,
    /// Only a file has versions.
    NotAFile,
    InvalidName,
    /// The place to restore into is a file.
    DestinationNotAFolder,
    /// The folder to restore into is inside what is restored.
    DestinationInsideItself,
    /// It is the root of the space.
    Root,
    /// The server takes no writes from this device.
    ReadOnly,
    /// A reason this app does not know, as the server spells it.
    Other {
        reason: String,
    },
}

/// A share link without its URL, which only [`NewShareLink`] carries.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ShareLink {
    pub id: String,
    /// The absolute path of the linked folder that syncs the link's space on this device; none when no folder here
    /// syncs it.
    pub folder: Option<String>,
    /// Where the shared file or folder is now, relative to `folder`; empty for the whole space; none when it is in
    /// the trash or gone.
    pub path: Option<String>,
    /// A symbolic link counts as a file; none when the item is gone.
    pub kind: Option<NodeKind>,
    pub created_at_ms: i64,
    /// None for a link that never expires.
    pub expires_at_ms: Option<i64>,
    pub has_password: bool,
    pub state: ShareState,
}

/// Whether a [`ShareLink`] opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ShareState {
    Active,
    Expired,
    Revoked,
}

/// How long a new share link works.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ShareExpiry {
    /// One day.
    Day,
    /// Seven days.
    Week,
    /// Thirty days.
    Month,
}

/// A share link just made, with its URL. The URL is in no other result, so the app shows it now or never.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct NewShareLink {
    /// Anyone who has it can open what the link shares.
    pub url: String,
    pub link: ShareLink,
}

/// A folder on this device linked to a space. Each space links at most one folder, and linked
/// folders never contain each other.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct LinkedFolder {
    pub space_id: String,
    pub space_name: String,
    /// The folder's absolute path.
    pub path: String,
    pub state: FolderState,
}

/// What sync is doing for one linked folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum FolderState {
    /// Linked, waiting for its first sync or for the session to start.
    Waiting,
    Syncing,
    UpToDate,
    /// Stopped by a failed check, such as a missing folder or marker (design §6.2) or a store
    /// older than the disk (§2.8). A [`Problem`] for the space says why; nothing syncs until it
    /// is fixed.
    Halted,
    /// Recovering after the space was rewound or this device's store was rolled back (design
    /// §6.4). Certain changes are applied; held files wait for answers to recovery questions.
    Recovering,
    /// The space uses a newer format than this app understands (design §4.2): downloads continue,
    /// but nothing is uploaded until the app is updated.
    ReadOnly,
}

/// A decision the engine needs from the person (`AskUser`, design §1). The engine persists
/// every question with what it holds and asks it again after a restart (§2.8), so an unanswered
/// question is in [`SessionState::questions`] until it is answered or withdrawn.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Question {
    /// Unique per question. A question whose held set changes is replaced by one with a new ID,
    /// so an answer always refers to the set the person saw.
    pub id: String,
    pub space_id: String,
    pub asked_at_ms: i64,
    pub kind: QuestionKind,
    /// Every node the answer applies to, in path order. Nothing else is affected by the answer.
    pub items: Vec<HeldItem>,
    /// The answers this question accepts, in the order the app shows them. Always two.
    pub answers: Vec<Answer>,
}

/// Why a question was asked, with what each kind must show.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum QuestionKind {
    /// The mass-delete guard (design §6.1) held local deletions because the commit would cross a
    /// limit. The items are the held topmost deleted nodes; a deleted folder is one deletion and
    /// carries the files it held. The counts include deletions already made in the rolling
    /// 10-minute window. Answers: [`Answer::Delete`] deletes the items on the server and every
    /// device; [`Answer::Restore`] downloads them here again.
    MassDelete {
        /// Deletions in the window, the held ones included.
        deletions: u64,
        /// Files those deletions remove.
        affected_files: u64,
        /// Files the space had before the window's deletions.
        space_files: u64,
        /// The limits crossed; at least one.
        exceeded: Vec<MassDeleteLimit>,
    },
    /// Files disappeared while changes were not being tracked, and could have been moved into an
    /// ignored folder rather than deleted (design §5.10). They stay on the server and on other
    /// devices until answered. Answers: [`Answer::Delete`] deletes them everywhere;
    /// [`Answer::Restore`] downloads them here again.
    AmbiguousDisappearance,
    /// One group of files held by a recovery (design §6.4). The cause is the same for every
    /// group of one recovery and is also announced by a [`NoticeKind::Recovery`] notice; the
    /// group decides the two answers.
    Recovery {
        cause: RecoveryCause,
        group: RecoveryGroup,
    },
}

/// A limit of the mass-delete guard (design §6.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum MassDeleteLimit {
    /// More than `limit` deletions in 10 minutes (1,000).
    Deletions { limit: u64 },
    /// More than `percent` of the space's files in 10 minutes (50), in a space of at least 20 files.
    ShareOfSpace { percent: u8 },
}

/// What started a recovery and when the space was restored to.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct RecoveryCause {
    pub trigger: RecoveryTrigger,
    /// The point in time the owner asked to rewind the space to, when the server reports it. The server restores
    /// the space's state at about that time (RFC 0003 §2).
    pub restore_point_ms: Option<i64>,
}

/// Why a recovery started (design §6.4, Triggers).
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum RecoveryTrigger {
    /// The owner rewound the space, or its server state was restored (`HISTORY_CHANGED`).
    SpaceRewound,
    /// This device's sync state is older than its history on the server: it was restored from a
    /// backup or copied from another machine.
    StoreRolledBack,
    /// This device's sync state is older than the files on disk.
    StoreOlderThanDisk,
    /// The folder was linked to a space imported from an export.
    SpaceImported,
    /// The folder was linked without earlier sync state (a first link of a folder that already
    /// has files, or lost state), so local files could not be matched with certainty.
    NoSyncState,
    /// The daemon did not record a cause, or this app does not recognize it.
    Unknown,
}

/// A group of held files in a recovery (design §6.4, What the user sees), with its answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum RecoveryGroup {
    /// Unchanged here, but created on the server after the restore point, so the rewind removed
    /// them. [`Answer::Keep`] puts them back on the server; [`Answer::Discard`] moves the local
    /// copies to the local trash.
    CreatedAfterRestorePoint,
    /// Edited or moved on this device since its last sync. [`Answer::Publish`] sends this
    /// device's version; [`Answer::Discard`] moves it to the local trash and takes the
    /// server's.
    ChangedOnThisDevice,
    /// Created on this device and unknown to the server. [`Answer::Publish`] uploads them;
    /// [`Answer::Discard`] moves them to the local trash.
    CreatedOnThisDevice,
    /// Deleted on this device but present on the server. [`Answer::Delete`] deletes them on
    /// the server, through the mass-delete guard; [`Answer::Restore`] downloads them again.
    DeletedOnThisDevice,
    /// A restore from the trash or of an earlier version that was requested before the reset and
    /// that the server does not show. [`Answer::Repeat`] sends it again; [`Answer::Cancel`]
    /// drops it.
    RequestedBeforeReset,
}

/// One node held by a question.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct HeldItem {
    /// The node's ID, or the request's ID for [`RecoveryGroup::RequestedBeforeReset`].
    pub node_id: String,
    pub path: String,
    pub kind: NodeKind,
    /// Files the answer affects through this item: 1 for a file, the files it holds for a folder.
    pub files: u64,
    pub detail: ItemDetail,
}

/// What happened to a held item, where its group does not already say it.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ItemDetail {
    /// Nothing beyond what the group says.
    Plain,
    /// Its content changed on this device.
    Edited,
    /// Moved or renamed on this device; `from` is its last synced path.
    Moved { from: String },
    /// Edited and moved on this device; `from` is its last synced path.
    EditedAndMoved { from: String },
    /// A requested restore from the server's trash.
    RestoreFromTrash,
    /// A requested restore of the version saved at `version_at_ms`.
    RestoreVersion { version_at_ms: i64 },
}

/// An answer to a question. Each [`QuestionKind`] accepts two of them, listed in
/// [`Question::answers`]; the engine refuses any other.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum Answer {
    /// Delete the items on the server and on every device.
    Delete,
    /// Keep the items on the server and download them here again.
    Restore,
    /// Put files the rewind removed back on the server.
    Keep,
    /// Move this device's copies to the local trash and follow the server.
    Discard,
    /// Send this device's version to the server.
    Publish,
    /// Send a request again.
    Repeat,
    /// Drop a request.
    Cancel,
}

impl QuestionKind {
    /// The two answers this kind of question accepts.
    pub fn answers(&self) -> Vec<Answer> {
        match self {
            QuestionKind::MassDelete { .. } | QuestionKind::AmbiguousDisappearance => {
                vec![Answer::Delete, Answer::Restore]
            }
            QuestionKind::Recovery { group, .. } => match group {
                RecoveryGroup::CreatedAfterRestorePoint => vec![Answer::Keep, Answer::Discard],
                RecoveryGroup::ChangedOnThisDevice | RecoveryGroup::CreatedOnThisDevice => {
                    vec![Answer::Publish, Answer::Discard]
                }
                RecoveryGroup::DeletedOnThisDevice => vec![Answer::Delete, Answer::Restore],
                RecoveryGroup::RequestedBeforeReset => vec![Answer::Repeat, Answer::Cancel],
            },
        }
    }
}

/// Something the person should know that needs neither an answer nor a fix. The core keeps a
/// notice, across restarts, until the person dismisses it.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Notice {
    pub id: String,
    pub space_id: String,
    pub at_ms: i64,
    pub kind: NoticeKind,
}

/// What a [`Notice`] reports.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum NoticeKind {
    /// A recovery started for the space (design §6.4, What the user sees): one notice per
    /// recovery, also when nothing is held. Changes the device had not made were applied, and
    /// files it removed went to the local trash; `held_files` are waiting for answers to
    /// [`QuestionKind::Recovery`] questions.
    Recovery {
        cause: RecoveryCause,
        held_files: u64,
    },
    /// Deletions from the server crossed a limit of the mass-delete guard and were applied here
    /// without asking, because the server is authoritative (design §6.1, Remote deletions). The
    /// files are in the local trash. The counts start with the 10-minute window that crossed the
    /// limit and grow with each later removal until the notice is dismissed.
    RemoteMassDelete {
        deletions: u64,
        affected_files: u64,
        space_files: u64,
        exceeded: Vec<MassDeleteLimit>,
    },
}

/// Something only a person can fix (`Problem`, design §1). A problem stays listed until its
/// cause goes away; the engine retries on its own.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Problem {
    pub id: String,
    /// The space it concerns, or none for the whole session.
    pub space_id: Option<String>,
    /// The path it concerns inside the space's folder, if it is about one node.
    pub path: Option<String>,
    pub kind: ProblemKind,
    pub since_ms: i64,
}

/// What a [`Problem`] is. The root checks halt their folder (design §6.2); the others leave
/// the rest of the folder syncing.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ProblemKind {
    /// The linked folder is missing, for example on an unmounted disk.
    FolderMissing,
    /// The linked path is no longer a folder.
    FolderNotAFolder,
    /// The linked folder or a folder inside it cannot be read.
    FolderUnreadable { reason: String },
    /// A different volume is mounted at the linked folder's path.
    FolderOnOtherVolume,
    /// The folder's marker `.koidrive/space.json` is missing.
    MarkerMissing,
    /// The folder's marker cannot be read.
    MarkerUnreadable { reason: String },
    /// The folder's marker names another space.
    MarkerOfOtherSpace { space_id: String },
    /// The folder holds files older than this device's sync state: the disk or its marker was
    /// restored from a backup (design §2.8).
    DiskOlderThanSyncState,
    /// The folder's file system cannot rename without replacing (design §5.2), so files cannot
    /// be applied safely. Some network and FUSE file systems do this.
    UnsupportedFileSystem,
    /// The space uses a newer format than this app understands (design §4.2).
    SpaceFormatTooNew,
    /// A file cannot be read, so it is not uploaded (design §5.7).
    FileUnreadable { reason: String },
    /// A file cannot be written, so a change to it cannot be applied (design §5.7).
    FileNotWritable { reason: String },
    /// A file keeps changing, such as a log or a database being written, so it is not uploaded.
    /// The core retries with backoff and reports it after a limit (design §5.8).
    FileBusy,
    /// A name from the server that this file system cannot hold.
    NameNotAllowed,
    /// A file from the server cannot be placed because an ignored or unsupported entry, such as
    /// a symbolic link, already has its name (design §5.1, §5.6).
    NameTaken,
    /// The server rejected a change to this node, with the reason it gave (design §2.7).
    ChangeRejected { reason: String },
    /// A line in a `.koiignore` file cannot be parsed and is ignored (design §5.10).
    IgnoreRuleInvalid { line: u32 },
    /// The server no longer accepts this device's credentials. Sign in again.
    SignInRequired,
    /// This device cannot save its sync state or credentials. Every change waits for its state
    /// to be saved first (design §1, §2.8), so nothing syncs until saving works again.
    StateNotWritable { reason: String },
    /// The folder stopped syncing for the reason the driver gives, which only a person can fix.
    FolderHalted { reason: String },
    /// Both versions of a file are syncing. `resolve_command` lists the choices in the bundled CLI.
    Conflict { resolve_command: String },
}

/// Whether this profile sends anonymous usage reports (docs/guide/telemetry.md).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, uniffi::Record)]
pub struct TelemetrySetting {
    /// Reports are recorded and sent: the stored choice, unless an environment variable decides.
    pub enabled: bool,
    /// Someone chose already, in the app's setup or settings or with the CLI, which shares the profile.
    pub decided: bool,
    /// `DO_NOT_TRACK` or `KOIDRIVE_TELEMETRY` decides, so the stored choice has no effect until it is gone.
    pub locked: bool,
}

/// The whole state the app shows. [`SessionEvent`]s replace one part of it at a time.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SessionState {
    pub status: SyncStatus,
    /// Set while the status is [`SyncStatus::Syncing`].
    pub progress: Option<SyncProgress>,
    pub account: Option<Account>,
    pub folders: Vec<LinkedFolder>,
    /// The latest synced changes, newest first, at most [`RECENT_CHANGES`].
    pub recent_changes: Vec<RecentChange>,
    /// Unanswered questions, oldest first.
    pub questions: Vec<Question>,
    /// Current problems, oldest first.
    pub problems: Vec<Problem>,
    /// Notices not dismissed yet, oldest first.
    pub notices: Vec<Notice>,
}

/// How many recent changes the session keeps.
pub const RECENT_CHANGES: usize = 20;

/// One change to the session state. Each event replaces one part of [`SessionState`] with its
/// new value, so applying an event twice, or after a state read that already includes it, is
/// harmless.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum SessionEvent {
    Status { status: SyncStatus },
    Progress { progress: Option<SyncProgress> },
    Account { account: Option<Account> },
    Folders { folders: Vec<LinkedFolder> },
    RecentChanges { changes: Vec<RecentChange> },
    Questions { questions: Vec<Question> },
    Problems { problems: Vec<Problem> },
    Notices { notices: Vec<Notice> },
}

/// Why a session call failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, uniffi::Error)]
pub enum SessionError {
    #[error("the server address is not an http or https URL")]
    InvalidServerUrl,
    #[error("the server cannot be reached: {reason}")]
    ServerUnreachable { reason: String },
    #[error("the server runs version {server_version}, which this app cannot talk to")]
    ServerIncompatible { server_version: String },
    #[error("connect to a server first")]
    NoServer,
    #[error("the server already has an owner; sign in with a pairing code")]
    AlreadyClaimed,
    #[error("the server has no owner yet; claim it with the code from its log")]
    NotClaimed,
    #[error("the code is wrong or has expired")]
    InvalidCode,
    #[error("too many wrong codes; try again in a few minutes")]
    TooManyAttempts,
    /// The pairing code was pasted as `<code>@<host>`, as `koi device add` prints it, and the host
    /// is not the server this device connected to.
    #[error("the pairing code is for {host}, not {server}")]
    CodeForOtherServer { host: String, server: String },
    #[error("a name is required")]
    EmptyName,
    #[error("this device is not signed in")]
    NotSignedIn,
    /// No other device of the account is active, so after this one signs out only owner recovery
    /// signs a device in again. The app tells the person and signs out again with confirmation.
    #[error("this is the account's last active device")]
    LastDevice,
    /// The server could not be reached or did not answer, so this device cannot tell whether
    /// another device of the account stays signed in. Nothing changed. The app tells the person
    /// that this may be their last device and signs out again with confirmation.
    #[error("the server did not confirm that it removed this device: {reason}")]
    NotRevoked { reason: String },
    /// The device to revoke is this one, which leaves the account with a sign-out instead.
    #[error("this device cannot revoke itself; sign out instead")]
    ThisDevice,
    #[error("the account has no such device")]
    DeviceNotFound,
    #[error("the space does not exist")]
    SpaceNotFound,
    #[error("the folder does not exist")]
    FolderNotFound,
    #[error("the space is already linked to a folder on this device")]
    SpaceAlreadyLinked,
    #[error("the folder contains or is inside the linked folder {}", koidrive_fs::quoted(.other))]
    FolderOverlaps { other: String },
    #[error("the space is not linked to a folder on this device")]
    FolderNotLinked,
    #[error("the question was answered or replaced")]
    QuestionNotFound,
    #[error("the question does not accept this answer")]
    AnswerNotAllowed,
    #[error("the notice was already dismissed")]
    NoticeNotFound,
    #[error("the change could not be saved on this device, so it was not made: {reason}")]
    NotSaved { reason: String },
    #[error("the path is not a folder")]
    NotAFolder,
    #[error("another Koidrive process is syncing the folder")]
    FolderInUse,
    #[error("the folder is linked by device {device_id}; unlink it there first")]
    FolderOfOtherDevice { device_id: String },
    #[error("the folder belongs to another space; unlink it there first")]
    MarkerOfOtherSpace,
    #[error(
        "the folder's file system cannot rename without replacing, so files cannot be applied safely"
    )]
    UnsupportedFileSystem,
    #[error("{} is not in a linked folder", koidrive_fs::quoted(.path))]
    PathNotInLinkedFolder { path: String },
    #[error("the path is the linked folder itself; name a file or folder in it")]
    PathIsLinkedFolder,
    #[error("the server has nothing at the path")]
    NothingAtPath,
    #[error("the path is a folder; only a file has versions")]
    PathIsFolder,
    #[error("the file or folder at the path is not deleted")]
    NotDeleted,
    #[error("the deleted item is not in the trash")]
    DeletedItemNotFound,
    #[error("the file has no such version")]
    VersionNotFound,
    #[error("the version is already the file's current one")]
    VersionIsCurrent,
    #[error("the file or folder is not on the server yet")]
    NotOnServer,
    #[error("a link's password has 8 to 256 characters")]
    PasswordLength,
    #[error("the account has no such share link")]
    ShareLinkNotFound,
    #[error("{reason}")]
    Failed { reason: String },
}
