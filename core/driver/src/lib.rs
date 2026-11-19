// SPDX-License-Identifier: Apache-2.0

//! The sync driver: everything around the engine that a client runs on one device (docs/design/driver.md).
//!
//! - [`Paths`], [`Credential`] and [`Config`]: where a profile keeps its files, the device credential (one file only
//!   its owner can read, outside every linked folder) and the device-local configuration with its `[sync]` rules.
//! - [`Driver`]: the profile's API, shared by the Linux daemon and, through `core/ffi`, the macOS app. It pairs the
//!   device, links and unlinks folders, reports [`Status`], lists [`Question`]s and records answers, records the
//!   restores a person asks for ([`RestoreRequest`]), and runs one task per linked folder.
//! - Each folder task checks the root (design §6.2) and claims the folder, observes the disk through an [`Observer`],
//!   and keeps a `core/net` session with the space. It runs the sync engine between them: the server's log goes
//!   into the remote tree in `core/store`, observations go into the local tree, the engine's effects are applied to
//!   the folder, and its commits are sent to the server.
//! - [`policy`]: what to do after the server refuses a call (H1 of reports/BOX-71).
//! - [`update`]: whether a newer Koidrive release is out, asked of GitHub at most once a day.
//! - [`ignore`]: the ignore rules of a folder, in the forms the scans and the engine take them.
//!
//! New files and folders sync in both directions. Edits, renames, deletes and conflicts go through the same loop and
//! are not proved end to end yet. docs/design/driver.md describes the loop and lists what is still to wire.

mod claim;
mod clock;
mod config;
#[cfg(test)]
mod counting;
mod credential;
mod disk;
mod driver;
mod engine;
mod failures;
mod folder;
mod health;
pub mod ignore;
mod leave;
mod lifecycle;
mod link_copy;
mod observe;
mod paths;
mod pending_create;
pub mod policy;
mod progress;
mod question;
pub mod raw_path;
mod restore;
#[cfg(test)]
mod scripted;
mod share;
mod status;
mod timing;
pub mod update;
mod volume;

pub use claim::Identity as DirectoryIdentity;
pub use config::{BackgroundService, Config, ConfigError, LocalTrash, SyncRules, UpdateCheck};
pub use credential::{Credential, CredentialError, SET_UP_AGAIN, api_for};
pub use driver::{
    Answered, Change, Changes, CleanupFailure, Driver, DriverError, LinkApproval, Linked,
    PROGRESS_INTERVAL, SetAside, SignedOut, Unconfirmed, lossy_paths,
};
pub use failures::{Failure, failures};
pub use folder::MAX_FORMAT;
pub use ignore::GitignoreOnly;
pub use koidrive_engine::guard::MAX_DELETIONS;
pub use koidrive_state_file::Flock;
pub use observe::{FsObserver, Observation, Observed, Observer, Observing, RulesChange};
pub use paths::{Paths, PathsError, StateEntry};
pub use pending_create::{CreatedSpace, claim_pending, create_space, space_create_pending};
pub use question::{
    Answer, HeldItem, Limit, Question, QuestionKind, RecoveryCause, RecoveryGroup, RecoveryTrigger,
};
pub use restore::{
    RestoreRejection, RestoreRequest, RestoreState, RestoreStatus, RestoreWait, Seen,
};
pub use share::{Expiry, ItemKind, LinkState, Revoked, Secret, SharedLink, Target};
pub use status::{
    Account, Activity, FolderState, FolderStatus, HeldDisk, HeldDiskKind, Progress, Remedy, Status,
    Totals, Unreachable, Wait, WaitKind, describe, describe_kept,
};
pub use update::Update;
pub use volume::{Volume, volume};
