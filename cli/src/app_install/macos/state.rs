use semver::Version;
use std::ffi::OsString;
use std::fs::File;
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Identity {
    pub(super) device: u64,
    pub(super) inode: u64,
}
#[derive(Debug, PartialEq, Eq)]
pub(in crate::app_install) struct RetiredOwnership {
    pub(super) root: PathBuf,
    pub(super) parent: PathBuf,
    pub(super) root_handle: PinnedDirectory,
    pub(super) parent_handle: PinnedDirectory,
    pub(super) bundle_handle: PinnedDirectory,
    pub(super) current: OwnedPath,
    pub(super) current_root: OwnedPath,
    pub(super) current_handle: PinnedDirectory,
    pub(super) receipt: Receipt,
}
#[derive(Debug)]
pub(super) struct PinnedDirectory {
    pub(super) file: File,
    pub(super) identity: Identity,
}
impl PartialEq for PinnedDirectory {
    fn eq(&self, other: &Self) -> bool {
        self.identity == other.identity
    }
}
impl Eq for PinnedDirectory {}
#[derive(Clone)]
pub(super) struct VerifiedBundle {
    pub(super) path: PathBuf,
    pub(super) version: Option<Version>,
    pub(super) identity: Identity,
}
pub(super) struct VerifiedDmg(pub(super) PathBuf);
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OwnedPath {
    pub(super) path: PathBuf,
    pub(super) identity: Identity,
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PreviousBundle {
    pub(super) identity: Identity,
    pub(super) version: Option<String>,
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Transfer {
    pub(super) incoming: Identity,
    pub(super) previous: Option<PreviousBundle>,
}
impl Transfer {
    pub(super) fn previous(&self) -> Option<&PreviousBundle> {
        self.previous.as_ref()
    }
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RunningApp {
    pub(super) bundle: OwnedPath,
    pub(super) root: OwnedPath,
    pub(super) version: Option<String>,
}
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UiIntent {
    pub(super) requested: bool,
    pub(super) running: Vec<RunningApp>,
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) enum Lifecycle {
    Preparing,
    Quiescing(UiIntent),
    Swapping(UiIntent),
    Installed(UiIntent),
    Reopening(UiIntent),
    AwaitingHandoff,
    Retiring,
    Aborting(UiIntent),
}
impl Lifecycle {
    pub(super) fn ui(&self) -> UiIntent {
        match self {
            Self::Quiescing(ui)
            | Self::Swapping(ui)
            | Self::Installed(ui)
            | Self::Reopening(ui)
            | Self::Aborting(ui) => ui.clone(),
            _ => UiIntent::default(),
        }
    }
}
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) enum WorkState {
    Downloading,
    AttachPending,
    Cleaning { attached: bool },
    Cleaned,
}
impl WorkState {
    pub(super) fn attached(self) -> bool {
        matches!(
            self,
            Self::AttachPending | Self::Cleaning { attached: true }
        )
    }
    pub(super) fn cleaning(self) -> bool {
        matches!(self, Self::Cleaning { .. } | Self::Cleaned)
    }
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Stage {
    pub(super) root: OwnedPath,
    pub(super) parent: OwnedPath,
    pub(super) bundle: Identity,
    pub(super) transfer: Option<Transfer>,
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReceiptData {
    pub(super) schema: u8,
    pub(super) work_root: Identity,
    pub(super) work: OwnedPath,
    pub(super) image: OwnedPath,
    pub(super) point: OwnedPath,
    pub(super) target: String,
    pub(super) work_state: WorkState,
    pub(super) lifecycle: Lifecycle,
    pub(super) stage: Option<Stage>,
}
#[derive(Debug)]
pub(super) struct Receipt {
    pub(super) root: PathBuf,
    pub(super) name: OsString,
    pub(super) root_handle: PinnedDirectory,
    pub(super) state: Mutex<(ReceiptData, Option<File>)>,
}
impl PartialEq for Receipt {
    fn eq(&self, other: &Self) -> bool {
        self.root == other.root && self.name == other.name && self.root_handle == other.root_handle
    }
}
impl Eq for Receipt {}
pub(super) enum Replay {
    Empty,
    Aborted,
    Committed,
}
pub(super) struct OwnedMount {
    pub(super) point: PathBuf,
    pub(super) image: PathBuf,
}
pub(super) enum MountState {
    Absent,
    Owned,
    Foreign,
}
pub(super) struct Staged {
    pub(super) bundle: VerifiedBundle,
    pub(super) parent: PathBuf,
    pub(super) parent_fd: File,
}
