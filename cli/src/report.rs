// SPDX-License-Identifier: Apache-2.0

//! What `--json` prints: one versioned envelope per command, and the CLI's own types for the data in it. The driver's,
//! engine's and server's types never reach the output directly, so they can change without changing the schema.
//! Schema 1 may gain optional fields; anything else needs a new schema version.

use koidrive_driver::{
    Account, FolderState, FolderStatus, Limit, Question, QuestionKind, RecoveryGroup,
    RecoveryTrigger, RestoreRejection, RestoreRequest, RestoreState, RestoreStatus, RestoreWait,
    Status, Update, Wait, WaitKind,
};
use koidrive_ipc::{Code, Next, Reason};
use koidrive_proto::v1::{DeviceInfo, SpaceInfo, SpaceUsage, device_info::Kind};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;

use crate::error::CliError;
use crate::output::{Presentation, Tone};

/// The only schema `--json` speaks.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The command did what it was asked.
    Ok,
    /// The work is recorded and happens later; it has not finished.
    Accepted,
    /// The state the command asks for already held, or the command repeated an accepted answer, so nothing changed.
    Unchanged,
    Error,
}

/// What a finished command produced: the text it prints without `--json`, and the data, decisions and next commands
/// it returns with it.
#[derive(Debug)]
pub struct Done {
    pub outcome: Outcome,
    /// The exit status, which only `status --check` sets for a successful command.
    pub exit: u8,
    pub data: Value,
    /// What it prints on stdout. A command whose result is one value, such as a new ID, prints only that value, so that
    /// `id=$(koi space create Notes)` works.
    pub text: String,
    pub presentation: Presentation,
    pub tone: Tone,
    /// What it prints on stderr after the text without `--json`: what the value is and what to do with it.
    pub note: String,
    pub decisions: Vec<Decision>,
    pub next: Vec<Next>,
}

impl Done {
    pub fn ok(data: impl Serialize, text: impl Into<String>) -> Self {
        Done {
            outcome: Outcome::Ok,
            exit: 0,
            data: serde_json::to_value(data).expect("the CLI's own types serialize"),
            text: text.into(),
            presentation: Presentation::Text,
            tone: Tone::Neutral,
            note: String::new(),
            decisions: Vec::new(),
            next: Vec::new(),
        }
    }

    /// A result that is one value: `value` alone on stdout, and `note` on stderr.
    pub fn value(data: impl Serialize, value: &str, note: impl Into<String>) -> Self {
        Done {
            note: note.into(),
            presentation: Presentation::Value,
            ..Done::ok(data, format!("{value}\n"))
        }
    }

    pub fn accepted(data: impl Serialize, text: impl Into<String>) -> Self {
        Done {
            outcome: Outcome::Accepted,
            ..Done::ok(data, text)
        }
    }

    /// The same result, with the outcome `unchanged`.
    pub fn unchanged(self) -> Self {
        Done {
            outcome: Outcome::Unchanged,
            ..self
        }
    }

    /// A command whose output was already printed while it ran, such as a server deploy.
    pub fn printed() -> Self {
        Done::ok(Value::Null, "")
    }

    pub fn then(mut self, argv: &[&str]) -> Self {
        self.next.push(Next::of(argv));
        self
    }
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct ErrorBody {
    pub code: Code,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<Reason>,
    pub message: String,
    pub retryable: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct Envelope {
    pub schema_version: u32,
    /// The subcommand path, such as `folder link`; empty for `koi` itself.
    pub command: String,
    pub outcome: Outcome,
    pub data: Value,
    pub error: Option<ErrorBody>,
    pub decisions: Vec<Decision>,
    pub next_commands: Vec<Next>,
}

impl Envelope {
    pub fn of(command: &str, done: Done) -> Self {
        Envelope {
            schema_version: SCHEMA_VERSION,
            command: command.to_owned(),
            outcome: done.outcome,
            data: done.data,
            error: None,
            decisions: done.decisions,
            next_commands: done.next,
        }
    }

    /// The envelope of a failed command, keeping the data it read before it failed.
    pub fn failed(command: &str, error: CliError, partial: Option<Done>) -> Self {
        let (data, decisions) = partial.map_or((Value::Null, Vec::new()), |done| {
            (done.data, done.decisions)
        });
        Envelope {
            schema_version: SCHEMA_VERSION,
            command: command.to_owned(),
            outcome: Outcome::Error,
            data,
            error: Some(ErrorBody {
                code: error.code,
                reason: error.reason,
                retryable: error.code.retryable(),
                message: error.message,
            }),
            decisions,
            next_commands: error.next,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    MassDelete,
    RemoteMassDelete,
    AmbiguousDisappearance,
    Conflict,
    Recovery,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DecisionGroup {
    CreatedAfterRestorePoint,
    ChangedOnThisDevice,
    CreatedOnThisDevice,
    DeletedOnThisDevice,
    RequestedBeforeReset,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LimitKind {
    Deletions,
    Share,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SyncState {
    Stopped,
    Connecting,
    Bootstrapping,
    CatchingUp,
    UpToDate,
    Offline,
    Halted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WaitReason {
    Quota,
    Retry,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub enum Signal {
    #[serde(rename = "SIGTERM")]
    Term,
    #[serde(rename = "SIGINT")]
    Int,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    Device,
    Agent,
    Unspecified,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NodeType {
    File,
    Folder,
    Symlink,
}

impl std::fmt::Display for NodeType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            NodeType::File => "file",
            NodeType::Folder => "folder",
            NodeType::Symlink => "symlink",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PendingOn {
    NodeProblem,
}

/// A decision sync waits on, with every item it holds and the command for each answer.
#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct Decision {
    pub id: String,
    pub space_id: String,
    pub kind: DecisionKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<DecisionGroup>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_cause: Option<RecoveryCauseData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflict: Option<ConflictData>,
    pub asked_at_ms: i64,
    /// False for a notice that only takes `dismiss`: it holds nothing and blocks nothing.
    pub requires_choice: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counts: Option<Counts>,
    pub items: Vec<Item>,
    pub answers: Vec<AnswerChoice>,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConflictData {
    Conflict {
        original: ConflictFileData,
        copy: ConflictFileData,
        #[serde(skip_serializing_if = "Option::is_none")]
        sent_in: Option<String>,
    },
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct ConflictFileData {
    pub node_id: String,
    pub revisions: RevisionsData,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct RevisionsData {
    pub content_rev: u64,
    pub meta_rev: u64,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct RecoveryCauseData {
    pub trigger: RecoveryTriggerData,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restore_point_ms: Option<i64>,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryTriggerData {
    SpaceRewound,
    StoreRolledBack,
    StoreOlderThanDisk,
    SpaceImported,
    NoSyncState,
    Unknown,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct Counts {
    pub deletions: u64,
    pub affected_files: u64,
    pub space_files: u64,
    pub exceeded: Vec<LimitKind>,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct Item {
    pub node_id: String,
    pub path: String,
    pub files: u64,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct AnswerChoice {
    pub token: String,
    pub consequence: String,
    pub argv: Vec<String>,
}

impl Decision {
    pub fn of(question: &Question) -> Self {
        let counts = |deletions: &u64, affected: &u64, space: &u64, exceeded: &[Limit]| {
            Some(Counts {
                deletions: *deletions,
                affected_files: *affected,
                space_files: *space,
                exceeded: exceeded
                    .iter()
                    .map(|limit| match limit {
                        Limit::Deletions => LimitKind::Deletions,
                        Limit::Share => LimitKind::Share,
                    })
                    .collect(),
            })
        };
        let (kind, group, counts) = match &question.kind {
            QuestionKind::MassDelete {
                deletions,
                affected_files,
                space_files,
                exceeded,
            } => (
                DecisionKind::MassDelete,
                None,
                counts(deletions, affected_files, space_files, exceeded),
            ),
            QuestionKind::RemoteMassDelete {
                deletions,
                affected_files,
                space_files,
                exceeded,
            } => (
                DecisionKind::RemoteMassDelete,
                None,
                counts(deletions, affected_files, space_files, exceeded),
            ),
            QuestionKind::AmbiguousDisappearance => {
                (DecisionKind::AmbiguousDisappearance, None, None)
            }
            QuestionKind::Conflict { .. } => (DecisionKind::Conflict, None, None),
            QuestionKind::Recovery { group, .. } => (
                DecisionKind::Recovery,
                Some(match group {
                    RecoveryGroup::CreatedAfterRestorePoint => {
                        DecisionGroup::CreatedAfterRestorePoint
                    }
                    RecoveryGroup::ChangedOnThisDevice => DecisionGroup::ChangedOnThisDevice,
                    RecoveryGroup::CreatedOnThisDevice => DecisionGroup::CreatedOnThisDevice,
                    RecoveryGroup::DeletedOnThisDevice => DecisionGroup::DeletedOnThisDevice,
                    RecoveryGroup::RequestedBeforeReset => DecisionGroup::RequestedBeforeReset,
                }),
                None,
            ),
        };
        let conflict = match &question.kind {
            QuestionKind::Conflict {
                original,
                copy,
                sent_in,
            } => {
                let file = |file: &koidrive_engine::question::ConflictFile| ConflictFileData {
                    node_id: file.node_id.clone(),
                    revisions: RevisionsData {
                        content_rev: file.revisions.content_rev,
                        meta_rev: file.revisions.meta_rev,
                    },
                };
                Some(ConflictData::Conflict {
                    original: file(original),
                    copy: file(copy),
                    sent_in: sent_in.clone(),
                })
            }
            _ => None,
        };
        Decision {
            id: question.id.clone(),
            space_id: question.space_id.clone(),
            kind,
            group,
            conflict,
            recovery_cause: question
                .recovery_cause
                .as_ref()
                .map(|cause| RecoveryCauseData {
                    trigger: match cause.trigger {
                        RecoveryTrigger::SpaceRewound => RecoveryTriggerData::SpaceRewound,
                        RecoveryTrigger::StoreRolledBack => RecoveryTriggerData::StoreRolledBack,
                        RecoveryTrigger::StoreOlderThanDisk => {
                            RecoveryTriggerData::StoreOlderThanDisk
                        }
                        RecoveryTrigger::SpaceImported => RecoveryTriggerData::SpaceImported,
                        RecoveryTrigger::NoSyncState => RecoveryTriggerData::NoSyncState,
                        RecoveryTrigger::Unknown => RecoveryTriggerData::Unknown,
                    },
                    restore_point_ms: cause.restore_point_ms,
                }),
            asked_at_ms: question.asked_at_ms,
            requires_choice: requires_choice(question),
            counts,
            items: (question.items.iter())
                .map(|item| Item {
                    node_id: item.node_id.clone(),
                    path: item.path.clone(),
                    files: item.files,
                })
                .collect(),
            answers: (crate::choices(question).into_iter())
                .map(|choice| AnswerChoice {
                    argv: Next::of(&["koi", "resolve", &question.id, &choice.word]).argv,
                    token: choice.word,
                    consequence: choice.effect,
                })
                .collect(),
        }
    }
}

/// Whether a decision needs a choice. A notice that only takes `dismiss` holds nothing back.
pub fn requires_choice(question: &Question) -> bool {
    !matches!(question.kind, QuestionKind::RemoteMassDelete { .. })
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct AccountData {
    pub server: String,
    pub user_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_name: Option<String>,
    pub device_id: String,
    pub device_name: String,
}

impl AccountData {
    pub fn of(account: &Account) -> Self {
        AccountData {
            server: account.server.clone(),
            user_id: account.user_id.clone(),
            user_name: account.user_name.clone(),
            device_id: account.device_id.clone(),
            device_name: account.device_name.clone(),
        }
    }
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct SetupData {
    /// The device this computer is connected as.
    pub account: AccountData,
    /// The linked folder, with the ID and name of the space it syncs with.
    pub folder: FolderData,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct FolderData {
    pub path: String,
    pub space_id: String,
    pub space_name: String,
    pub state: SyncState,
    pub history_id: Option<String>,
    pub last_seq: Option<u64>,
    pub head_seq: Option<u64>,
    pub remote_nodes: u64,
    pub local_entries: Option<u64>,
    pub questions: u64,
    pub problem: Option<String>,
    pub wait: Option<WaitData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<ProgressData>,
}

/// The files the folder still sends to the server or fetches from it, while it syncs. The bytes are those of every
/// file counted since the folder last had none left, the synced ones included, so `bytes_done` never decreases.
/// `current` is the file whose content moved last, relative to the folder.
#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct ProgressData {
    pub files_to_upload: u64,
    pub files_to_download: u64,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub current: Option<String>,
}

/// A call the folder puts off: `quota` when the server refused it for a used-up limit or full storage, `retry` when
/// it kept failing for another reason. It is sent again at `until_ms`, in Unix milliseconds.
#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct WaitData {
    pub kind: WaitReason,
    pub until_ms: i64,
}

impl WaitData {
    fn of(wait: Wait) -> Self {
        WaitData {
            kind: match wait.kind {
                WaitKind::Quota => WaitReason::Quota,
                WaitKind::Retry => WaitReason::Retry,
            },
            until_ms: wait.until_ms,
        }
    }
}

impl FolderData {
    pub fn of(folder: &FolderStatus) -> Self {
        FolderData {
            path: folder.path.clone(),
            space_id: folder.space_id.clone(),
            space_name: folder.space_name.clone(),
            state: match folder.state {
                FolderState::Stopped => SyncState::Stopped,
                FolderState::Connecting => SyncState::Connecting,
                FolderState::Bootstrapping => SyncState::Bootstrapping,
                FolderState::CatchingUp => SyncState::CatchingUp,
                FolderState::UpToDate => SyncState::UpToDate,
                FolderState::Offline => SyncState::Offline,
                FolderState::Halted => SyncState::Halted,
            },
            history_id: folder.history_id.clone(),
            last_seq: folder.last_seq,
            head_seq: folder.head_seq,
            remote_nodes: folder.remote_nodes,
            local_entries: folder.local_entries,
            questions: folder.questions,
            problem: folder.problem.clone(),
            wait: folder.wait.map(WaitData::of),
            progress: (folder.progress.as_ref()).map(|progress| ProgressData {
                files_to_upload: progress.files_to_upload,
                files_to_download: progress.files_to_download,
                bytes_done: progress.bytes_done,
                bytes_total: progress.bytes_total,
                current: progress.current.clone(),
            }),
        }
    }
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct StatusData {
    pub account: Option<AccountData>,
    pub running: bool,
    /// Sync is paused until `koi resume`, so the folders are stopped.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub paused: bool,
    pub folders: Vec<FolderData>,
    /// Every condition `status --check` weighs, whether or not `--check` was given.
    pub conditions: Vec<Condition>,
    pub service: crate::service::Report,
    #[schemars(
        with = "Option<UpdateNoticeData>",
        skip_serializing_if = "Option::is_none"
    )]
    pub update: UpdateNoticeData,
}

/// Whether a newer Koidrive release is out than the version this device runs, from the daemon's last check.
#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct UpdateNoticeData {
    pub state: UpdateState,
    /// The version this device runs: the daemon's, or this koi's when no daemon runs.
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest: Option<String>,
    /// The release's page on GitHub.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// When GitHub last answered the check, in UTC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<String>,
}

/// `unknown` until a check has had an answer, and `off` when config.toml or `KOIDRIVE_UPDATE_CHECK` turns it off.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UpdateState {
    Available,
    UpToDate,
    Unknown,
    Off,
}

impl UpdateNoticeData {
    pub fn of(update: &Update, version: &str, command: Option<&str>) -> Self {
        let notice = |state| UpdateNoticeData {
            state,
            version: version.to_owned(),
            latest: None,
            url: None,
            command: None,
            checked_at: None,
        };
        match update {
            Update::Available {
                latest,
                url,
                checked_at_ms,
            } => UpdateNoticeData {
                latest: Some(latest.clone()),
                url: Some(url.clone()),
                command: command.map(str::to_owned),
                checked_at: Some(utc(*checked_at_ms)),
                ..notice(UpdateState::Available)
            },
            Update::UpToDate { checked_at_ms } => UpdateNoticeData {
                checked_at: Some(utc(*checked_at_ms)),
                ..notice(UpdateState::UpToDate)
            },
            Update::Unknown => notice(UpdateState::Unknown),
            Update::Off => notice(UpdateState::Off),
        }
    }
}

fn utc(at_ms: i64) -> String {
    let time = std::time::UNIX_EPOCH + std::time::Duration::from_millis(at_ms.max(0) as u64);
    let seconds = at_ms.max(0) / 1000 % 86_400;
    format!(
        "{}T{:02}:{:02}:{:02}Z",
        koidrive_fs::utc_date(time),
        seconds / 3600,
        seconds % 3600 / 60,
        seconds % 60
    )
}

/// What `koi update` did: `updated` from `from` to `to` through `via`, or `unchanged` when this device runs the newest
/// release. `service` is the background service once the new koi took it.
#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct UpdatedData {
    pub from: String,
    pub to: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<Via>,
    pub result: UpdateResult,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<crate::service::Report>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<crate::app::Report>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Via {
    Homebrew,
    Installer,
    App,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UpdateResult {
    Updated,
    Unchanged,
}

/// Something `status --check` reports. Connecting and catching up are not conditions: they pass on their own.
#[derive(Debug, PartialEq, Serialize, JsonSchema)]
#[serde(tag = "condition", rename_all = "snake_case")]
pub enum Condition {
    NotSetUp,
    Halted {
        folder: String,
        problem: Option<String>,
    },
    Stopped {
        folder: String,
    },
    DecisionRequired {
        decision_id: String,
        space_id: String,
    },
    Offline {
        folder: String,
    },
    /// The folder puts a call off until `until_ms`, because the server refused it or it kept failing.
    Waiting {
        folder: String,
        #[serde(flatten)]
        wait: WaitData,
    },
}

impl Condition {
    /// The exit status of `status --check` for this condition alone.
    pub fn exit(&self) -> u8 {
        match self {
            Condition::NotSetUp => Code::AuthRequired.exit(),
            Condition::Halted { .. } | Condition::Stopped { .. } => Code::Halted.exit(),
            Condition::DecisionRequired { .. } => Code::DecisionRequired.exit(),
            Condition::Offline { .. } | Condition::Waiting { .. } => Code::Unavailable.exit(),
        }
    }
}

/// The conditions of a status and the decisions pending in it. A notice that only takes `dismiss` is not one.
pub fn conditions(status: &Status, questions: &[Question]) -> Vec<Condition> {
    let mut found = Vec::new();
    if status.account.is_none() {
        found.push(Condition::NotSetUp);
    }
    for folder in &status.folders {
        let path = folder.path.clone();
        match folder.state {
            FolderState::Halted => found.push(Condition::Halted {
                folder: path,
                problem: folder.problem.clone(),
            }),
            FolderState::Stopped if status.paused => {}
            FolderState::Stopped => found.push(Condition::Stopped { folder: path }),
            FolderState::Offline => found.push(Condition::Offline { folder: path }),
            FolderState::Connecting
            | FolderState::Bootstrapping
            | FolderState::CatchingUp
            | FolderState::UpToDate => {}
        }
        if let Some(wait) = folder.wait {
            found.push(Condition::Waiting {
                folder: folder.path.clone(),
                wait: WaitData::of(wait),
            });
        }
    }
    for question in questions
        .iter()
        .filter(|question| requires_choice(question))
    {
        found.push(Condition::DecisionRequired {
            decision_id: question.id.clone(),
            space_id: question.space_id.clone(),
        });
    }
    found
}

/// The exit status of `status --check`: missing credentials first, then halted or stopped sync, then a decision
/// that needs a choice, then offline or waiting folders. 0 when there is none of them.
pub fn check_exit(conditions: &[Condition]) -> u8 {
    let precedence = [
        Code::AuthRequired,
        Code::Halted,
        Code::DecisionRequired,
        Code::Unavailable,
    ];
    precedence
        .iter()
        .map(|code| code.exit())
        .find(|exit| conditions.iter().any(|condition| condition.exit() == *exit))
        .unwrap_or(0)
}

#[derive(Serialize, JsonSchema)]
pub struct DaemonLine<'a> {
    schema_version: u32,
    #[serde(flatten)]
    event: &'a Lifecycle,
}

/// One line of `koi _daemon --json`, printed as it happens.
#[derive(Debug, PartialEq, Serialize, JsonSchema)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Lifecycle {
    Started {
        version: &'static str,
        socket: String,
        folders: usize,
    },
    /// A folder's state changed, with the state named as in `status`.
    FolderState {
        folder: String,
        space_id: String,
        state: SyncState,
    },
    Stopping {
        signal: Signal,
    },
    Stopped,
}

impl Lifecycle {
    /// The event as one line of JSON, with the schema version.
    pub fn line(&self) -> String {
        let line = DaemonLine {
            schema_version: SCHEMA_VERSION,
            event: self,
        };
        serde_json::to_string(&line).expect("an event serializes")
    }
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct UnlinkedData {
    /// The space the folder was linked to. Absent when it was not linked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub space_id: Option<String>,
    pub path: String,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct AnsweredData {
    pub decision_id: String,
    pub answer: String,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct DeviceData {
    pub device_id: String,
    pub name: String,
    pub platform: String,
    pub kind: DeviceKind,
    pub created_at_ms: i64,
    pub last_seen_ms: i64,
    pub revoked_at_ms: i64,
    pub credential_expires_at_ms: i64,
    pub issued_by: String,
    pub current: bool,
}

impl DeviceData {
    pub fn of(device: &DeviceInfo) -> Self {
        DeviceData {
            device_id: device.device_id.clone(),
            name: device.name.clone(),
            platform: device.platform.clone(),
            kind: match device.kind() {
                Kind::Device => DeviceKind::Device,
                Kind::Agent => DeviceKind::Agent,
                Kind::Unspecified => DeviceKind::Unspecified,
            },
            created_at_ms: device.created_at_ms,
            last_seen_ms: device.last_seen_ms,
            revoked_at_ms: device.revoked_at_ms,
            credential_expires_at_ms: device.credential_expires_at_ms,
            issued_by: device.issued_by.clone(),
            current: device.current,
        }
    }
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct DevicesData {
    pub devices: Vec<DeviceData>,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct RevokedData {
    pub device_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ShareState {
    Active,
    Expired,
    Revoked,
}

/// A share link without its URL, which only `koi share create` shows.
#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct ShareLinkData {
    /// What `koi share revoke` takes. It does not reveal the link.
    pub id: String,
    pub space_id: String,
    pub space_name: String,
    /// The linked folder that syncs the space on this device.
    pub folder: Option<String>,
    pub node_id: String,
    /// The path of the shared file or folder in its space, empty for the whole space, or null if it is in the trash,
    /// gone, or unavailable after revocation.
    pub path: Option<String>,
    pub kind: Option<NodeType>,
    pub created_at_ms: i64,
    /// Null for a link that never expires.
    pub expires_at_ms: Option<i64>,
    pub revoked_at_ms: Option<i64>,
    pub has_password: bool,
    pub state: ShareState,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct SharedData {
    /// The link. Anyone who has it can open what it shares.
    pub url: String,
    #[serde(flatten)]
    pub link: ShareLinkData,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct ShareLinksData {
    /// Newest first.
    pub links: Vec<ShareLinkData>,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct InvitationData {
    /// The arguments of `koi join` on the new device.
    pub join_argv: Vec<String>,
    pub expires_at_ms: i64,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct SpaceData {
    pub space_id: String,
    pub name: String,
    pub created_at_ms: i64,
}

impl SpaceData {
    pub fn of(space: &SpaceInfo) -> Self {
        SpaceData {
            space_id: space.space_id.clone(),
            name: space.name.clone(),
            created_at_ms: space.created_at_ms,
        }
    }
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct SpacesData {
    pub spaces: Vec<SpaceData>,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct UsageData {
    pub space_id: String,
    pub name: String,
    pub live_files: u64,
    pub live_bytes: u64,
    pub trashed_files: u64,
    pub trash_bytes: u64,
    pub old_versions: u64,
    pub old_version_bytes: u64,
    pub unreferenced_bytes: u64,
    pub total_bytes: u64,
}

impl UsageData {
    pub fn of(space: &SpaceInfo, usage: &SpaceUsage) -> Self {
        UsageData {
            space_id: space.space_id.clone(),
            name: space.name.clone(),
            live_files: usage.live_files,
            live_bytes: usage.live_bytes,
            trashed_files: usage.trashed_files,
            trash_bytes: usage.trash_bytes,
            old_versions: usage.old_versions,
            old_version_bytes: usage.old_version_bytes,
            unreferenced_bytes: usage.unreferenced_bytes,
            total_bytes: crate::total_bytes(usage),
        }
    }
}

/// The spaces that answered, and the ones the server refused with why.
#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct UsagesData {
    pub spaces: Vec<UsageData>,
    pub refused: Vec<RefusedSpace>,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct RefusedSpace {
    pub space_id: String,
    pub name: String,
    pub message: String,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct TrashEntry {
    pub node_id: String,
    pub path: String,
    pub kind: NodeType,
    pub trash_group: String,
    pub files: u64,
    pub bytes: u64,
    /// When the content it has was saved; null for a folder.
    pub modified_at_ms: Option<i64>,
    /// When it was deleted, null when the server does not say (RFC 0023).
    pub deleted_at_ms: Option<i64>,
    /// The device that deleted it, null when the server does not say.
    pub deleted_by_device_id: Option<String>,
    /// That device's name, null for a device of another account or when the server does not say.
    pub deleted_by_device_name: Option<String>,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct TrashData {
    pub folder: String,
    pub entries: Vec<TrashEntry>,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct VersionData {
    pub version_id: String,
    pub seq: u64,
    pub size: u64,
    pub device_id: String,
    /// The name of the device that saved it, or null for a device of another account.
    pub device_name: Option<String>,
    pub created_at_ms: i64,
    pub current: bool,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct VersionsData {
    pub path: String,
    pub node_id: String,
    pub versions: Vec<VersionData>,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct RestoreAskedData {
    pub request_id: String,
    pub folder: String,
    pub path: String,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct RequestsData {
    pub folder: String,
    pub requests: Vec<RequestData>,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct RequestData {
    pub id: String,
    /// The path of its node relative to the linked folder, or where it was while it is in the trash; null when the
    /// folder does not know it.
    pub path: Option<String>,
    pub asked_at_ms: i64,
    pub request: RequestedData,
    pub state: RequestState,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RequestedData {
    FromTrash {
        node_id: String,
        parent_id: Option<String>,
        name: Option<String>,
    },
    Version {
        node_id: String,
        version_id: String,
        /// When the version was saved, in Unix milliseconds; null when the folder does not know it.
        saved_at_ms: Option<i64>,
    },
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RequestState {
    Pending { waits_for: Option<PendingOn> },
    Restored,
    Renamed { name: String },
    Copied { node_id: String, name: String },
    Rejected { reason: String },
    DoesNotFit,
}

impl RequestData {
    pub fn of(restore: &RestoreStatus) -> Self {
        RequestData {
            id: restore.id.clone(),
            path: restore.path.clone(),
            asked_at_ms: restore.asked_at_ms,
            request: match &restore.request {
                RestoreRequest::FromTrash {
                    node_id,
                    parent_id,
                    name,
                } => RequestedData::FromTrash {
                    node_id: node_id.clone(),
                    parent_id: parent_id.clone(),
                    name: name.clone(),
                },
                RestoreRequest::Version {
                    node_id,
                    version_id,
                    saved_at_ms,
                } => RequestedData::Version {
                    node_id: node_id.clone(),
                    version_id: version_id.clone(),
                    saved_at_ms: *saved_at_ms,
                },
            },
            state: match &restore.state {
                RestoreState::Pending { waits_for } => RequestState::Pending {
                    waits_for: waits_for.map(|wait| match wait {
                        RestoreWait::NodeProblem => PendingOn::NodeProblem,
                    }),
                },
                RestoreState::Restored => RequestState::Restored,
                RestoreState::Renamed { name } => RequestState::Renamed { name: name.clone() },
                RestoreState::Copied { node_id, name } => RequestState::Copied {
                    node_id: node_id.clone(),
                    name: name.clone(),
                },
                RestoreState::Rejected { reason } => RequestState::Rejected {
                    reason: match reason {
                        RestoreRejection::NotFound => "not_found".into(),
                        RestoreRejection::NotFile => "not_file".into(),
                        RestoreRejection::InvalidName => "invalid_name".into(),
                        RestoreRejection::ParentNotDir => "parent_not_dir".into(),
                        RestoreRejection::Cycle => "cycle".into(),
                        RestoreRejection::Root => "root".into(),
                        RestoreRejection::ReadOnly => "read_only".into(),
                        RestoreRejection::Cancelled => "cancelled".into(),
                        RestoreRejection::Other(reason) => reason.clone(),
                    },
                },
                RestoreState::DoesNotFit => RequestState::DoesNotFit,
            },
        }
    }
}

/// One way to set up this machine.
#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct Choice {
    pub argv: Vec<String>,
    pub description: &'static str,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct ChoicesData {
    pub choices: Vec<Choice>,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct HelpData {
    pub help: String,
    /// The command asked about and every command under it.
    pub commands: Vec<CommandInfo>,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct CommandInfo {
    /// The subcommand path, as in the envelope's `command`.
    pub command: String,
    pub about: String,
    /// Clap's usage line, as `--help` shows it.
    pub usage: String,
    pub aliases: Vec<String>,
    pub arguments: Vec<ArgumentInfo>,
    pub options: Vec<OptionInfo>,
    /// Arguments that are given together or apart, named as in `requires`.
    pub groups: Vec<GroupInfo>,
    pub subcommands: Vec<String>,
    pub auth: Auth,
    pub output: Output,
    /// The file of the schema of its envelope in docs/json-schema/v1.
    pub schema: String,
}

/// What the command signs in with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Auth {
    None,
    /// This device's credential, from `koi setup`. Without one it fails with `auth_required`.
    Device,
    /// A Cloudflare login, which only `--interactive` starts. Without one it fails with `auth_required`.
    Cloudflare,
}

/// What it prints on stdout with `--json`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Output {
    /// One envelope.
    Envelope,
    /// One JSON object per line while it runs.
    Lines,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct ArgumentInfo {
    pub name: String,
    pub required: bool,
    pub repeated: bool,
    /// What it needs beside it: argument names, and options as `--long`.
    pub requires: Vec<String>,
    /// What it can't be given with, named as in `requires`.
    pub conflicts: Vec<String>,
    pub about: String,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct GroupInfo {
    pub members: Vec<String>,
    /// One of the members must be given.
    pub required: bool,
    /// More than one member may be given.
    pub multiple: bool,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct OptionInfo {
    pub long: String,
    pub short: Option<char>,
    /// The name of the value it takes, or null for a flag.
    pub value: Option<String>,
    /// The value may be left out, as in `--json`.
    pub value_optional: bool,
    /// A value follows an equals sign, as in `--json=1`, never as the next argument.
    pub require_equals: bool,
    /// The values it accepts, when it accepts only some.
    pub values: Vec<String>,
    pub required: bool,
    /// What it needs beside it, named as in the arguments, and options as `--long`.
    pub requires: Vec<String>,
    /// What it can't be given with, named as in `requires`.
    pub conflicts: Vec<String>,
    /// Whether every subcommand takes it too.
    pub global: bool,
    pub about: String,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct VersionInfoData {
    pub version: &'static str,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct Empty {}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct LeftData {
    /// The files and directories that could not be removed after the server revoked this device, such as a folder's
    /// `.koidrive/device.json` owner record. Running `koi device leave` again removes them.
    pub not_removed: Vec<NotRemovedData>,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct NotRemovedData {
    pub path: String,
    pub error: String,
}

/// The data of a command that brings the background service in line with the profile, with the service's report.
#[derive(Debug, Serialize, JsonSchema)]
#[schemars(rename = "WithService_{T}")]
pub struct WithService<T> {
    #[serde(flatten)]
    pub data: T,
    pub service: crate::service::Report,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct WithApp<T> {
    #[serde(flatten)]
    pub data: T,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<crate::app::Report>,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct DiagnosticsData {
    pub path: std::path::PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;
    use koidrive_driver::HeldItem;

    #[test]
    fn an_unchanged_result_keeps_its_data_and_its_value() {
        let done = Done::value(
            RevokedData {
                device_id: "d1".into(),
            },
            "d1",
            "Device d1 is already revoked.\n",
        )
        .unchanged();
        assert_eq!((done.exit, done.text.as_str()), (0, "d1\n"));
        let envelope = serde_json::to_value(Envelope::of("device revoke", done)).unwrap();
        assert_eq!(envelope["outcome"], "unchanged");
        assert_eq!(envelope["data"], serde_json::json!({ "device_id": "d1" }));
        assert_eq!(envelope["error"], Value::Null);
    }

    fn folder(path: &str, state: FolderState) -> FolderStatus {
        FolderStatus {
            recovery: None,
            space_id: format!("space-{path}"),
            space_name: path.to_owned(),
            path: format!("/home/ada/{path}"),
            state,
            history_id: None,
            last_seq: None,
            head_seq: None,
            remote_nodes: 0,
            local_entries: None,
            questions: 0,
            problem: None,
            progress: None,
            wait: None,
        }
    }

    #[test]
    fn the_update_notice_carries_latest_url_and_command_only_while_a_release_is_out() {
        let json = |update: &Update| {
            serde_json::to_value(UpdateNoticeData::of(update, "0.1.0", Some("koi update"))).unwrap()
        };
        let checked_at_ms = 1_763_006_400_000;
        assert_eq!(
            json(&Update::Available {
                latest: "0.1.2".into(),
                url: "https://github.com/koidrive/koidrive/releases/tag/v0.1.2".into(),
                checked_at_ms,
            }),
            serde_json::json!({
                "state": "available",
                "version": "0.1.0",
                "latest": "0.1.2",
                "url": "https://github.com/koidrive/koidrive/releases/tag/v0.1.2",
                "command": "koi update",
                "checked_at": "2025-11-13T04:00:00Z",
            })
        );
        assert_eq!(
            json(&Update::UpToDate { checked_at_ms }),
            serde_json::json!({ "state": "up_to_date", "version": "0.1.0", "checked_at": "2025-11-13T04:00:00Z" })
        );
        assert_eq!(
            json(&Update::Unknown),
            serde_json::json!({ "state": "unknown", "version": "0.1.0" })
        );
        assert_eq!(
            json(&Update::Off),
            serde_json::json!({ "state": "off", "version": "0.1.0" })
        );
        assert_eq!(utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc(951_825_599_999), "2000-02-29T11:59:59Z");
    }

    fn status(signed_in: bool, folders: Vec<FolderStatus>) -> Status {
        Status {
            account: signed_in.then(|| Account {
                server: "https://files.example.com".into(),
                user_id: "u1".into(),
                user_name: None,
                device_id: "d1".into(),
                device_name: "desk".into(),
            }),
            running: true,
            folders,
            ..Status::default()
        }
    }

    fn question(id: &str, kind: QuestionKind) -> Question {
        Question {
            id: id.into(),
            lineage: id.into(),
            space_id: "space-Work".into(),
            asked_at_ms: 5,
            recovery_cause: None,
            kind,
            items: vec![
                HeldItem {
                    node_id: "n1".into(),
                    path: "archive/a".into(),
                    files: 3,
                    device_id: None,
                },
                HeldItem {
                    node_id: "n2".into(),
                    path: "archive/b".into(),
                    files: 1,
                    device_id: None,
                },
            ],
        }
    }

    fn held() -> QuestionKind {
        QuestionKind::MassDelete {
            deletions: 24,
            affected_files: 1_240,
            space_files: 2_000,
            exceeded: vec![Limit::Deletions, Limit::Share],
        }
    }

    fn notice() -> QuestionKind {
        QuestionKind::RemoteMassDelete {
            deletions: 9,
            affected_files: 900,
            space_files: 1_000,
            exceeded: vec![Limit::Share],
        }
    }

    fn exit(status: &Status, questions: &[Question]) -> u8 {
        check_exit(&conditions(status, questions))
    }

    #[test]
    fn a_success_envelope_carries_every_field_and_a_null_error() {
        let done = Done::accepted(
            AnsweredData {
                decision_id: "q1".into(),
                answer: "restore".into(),
            },
            "Resolved.\n",
        )
        .then(&["koi", "status", "--json"]);
        let envelope = serde_json::to_value(Envelope::of("resolve", done)).unwrap();
        assert_eq!(
            envelope,
            serde_json::json!({
                "schema_version": 1,
                "command": "resolve",
                "outcome": "accepted",
                "data": { "decision_id": "q1", "answer": "restore" },
                "error": null,
                "decisions": [],
                "next_commands": [{ "argv": ["koi", "status", "--json"] }],
            })
        );
    }

    #[test]
    fn an_error_envelope_carries_the_code_whether_to_retry_and_the_next_command() {
        let error =
            CliError::new(Code::StaleDecision, "q1 changed").then(&["koi", "resolve", "--json"]);
        let envelope = serde_json::to_value(Envelope::failed("resolve", error, None)).unwrap();
        assert_eq!(
            envelope,
            serde_json::json!({
                "schema_version": 1,
                "command": "resolve",
                "outcome": "error",
                "data": null,
                "error": { "code": "stale_decision", "message": "q1 changed", "retryable": false },
                "decisions": [],
                "next_commands": [{ "argv": ["koi", "resolve", "--json"] }],
            })
        );
        let offline = CliError::new(Code::Unavailable, "could not reach the server");
        let envelope = Envelope::failed("space list", offline, None);
        assert!(envelope.error.unwrap().retryable);
    }

    #[test]
    fn a_decision_carries_its_counts_every_item_and_each_answer_as_argv() {
        let decision = serde_json::to_value(Decision::of(&question("q1", held()))).unwrap();
        assert_eq!(
            decision,
            serde_json::json!({
                "id": "q1",
                "space_id": "space-Work",
                "kind": "mass_delete",
                "asked_at_ms": 5,
                "requires_choice": true,
                "counts": {
                    "deletions": 24,
                    "affected_files": 1240,
                    "space_files": 2000,
                    "exceeded": ["deletions", "share"],
                },
                "items": [
                    { "node_id": "n1", "path": "archive/a", "files": 3 },
                    { "node_id": "n2", "path": "archive/b", "files": 1 },
                ],
                "answers": [
                    {
                        "token": "delete",
                        "consequence": "Delete these files on the server and other devices",
                        "argv": ["koi", "resolve", "q1", "delete"],
                    },
                    {
                        "token": "restore",
                        "consequence": "Download these files to this device again",
                        "argv": ["koi", "resolve", "q1", "restore"],
                    },
                ],
            })
        );
        let recovery = Decision::of(&question(
            "q2",
            QuestionKind::Recovery {
                group: RecoveryGroup::RequestedBeforeReset,
                sent_in: None,
                cause: None,
            },
        ));
        assert_eq!(
            (recovery.kind, recovery.group, recovery.counts),
            (
                DecisionKind::Recovery,
                Some(DecisionGroup::RequestedBeforeReset),
                None
            )
        );
        assert!(!Decision::of(&question("q3", notice())).requires_choice);
    }

    #[test]
    fn recovery_causes_reach_decision_json_without_changing_schema_version() {
        let validator =
            jsonschema::validator_for(&crate::schema::envelope("resolve").unwrap()).unwrap();
        for (trigger, expected) in [
            (RecoveryTrigger::SpaceRewound, "space_rewound"),
            (RecoveryTrigger::StoreRolledBack, "store_rolled_back"),
            (RecoveryTrigger::StoreOlderThanDisk, "store_older_than_disk"),
            (RecoveryTrigger::SpaceImported, "space_imported"),
            (RecoveryTrigger::NoSyncState, "no_sync_state"),
            (RecoveryTrigger::Unknown, "unknown"),
        ] {
            for restore_point_ms in [Some(1_700_000_000_123), None] {
                let mut question = question(
                    "q1",
                    QuestionKind::Recovery {
                        group: RecoveryGroup::CreatedAfterRestorePoint,
                        sent_in: None,
                        cause: None,
                    },
                );
                question.recovery_cause = Some(koidrive_driver::RecoveryCause {
                    trigger,
                    restore_point_ms,
                });
                let mut done = Done::ok(
                    AnsweredData {
                        decision_id: "q1".into(),
                        answer: "keep".into(),
                    },
                    "",
                );
                done.decisions.push(Decision::of(&question));
                let envelope = serde_json::to_value(Envelope::of("resolve", done)).unwrap();
                let cause = &envelope["decisions"][0]["recovery_cause"];
                assert_eq!(cause["trigger"], expected);
                match restore_point_ms {
                    Some(expected) => assert_eq!(cause["restore_point_ms"], expected),
                    None => assert!(cause.get("restore_point_ms").is_none()),
                }
                assert_eq!(envelope["schema_version"], 1);
                assert!(validator.is_valid(&envelope), "{envelope:#}");
            }
        }
    }

    #[test]
    fn older_and_future_recovery_causes_remain_compatible() {
        let question = question(
            "q1",
            QuestionKind::Recovery {
                group: RecoveryGroup::CreatedAfterRestorePoint,
                sent_in: None,
                cause: None,
            },
        );
        let mut stored = serde_json::to_value(&question).unwrap();
        assert!(stored.get("recovery_cause").is_none());
        let older: Question = serde_json::from_value(stored.clone()).unwrap();
        assert!(older.recovery_cause.is_none());
        let decision = serde_json::to_value(Decision::of(&older)).unwrap();
        assert!(decision.get("recovery_cause").is_none());

        stored["recovery_cause"] = serde_json::json!({
            "trigger": "a_future_trigger",
            "restore_point_ms": 123,
        });
        let future: Question = serde_json::from_value(stored).unwrap();
        let decision = serde_json::to_value(Decision::of(&future)).unwrap();
        assert_eq!(
            decision["recovery_cause"],
            serde_json::json!({
                "trigger": "unknown",
                "restore_point_ms": 123,
            })
        );
    }

    #[test]
    fn conflict_payload_preserves_both_files_revisions_and_the_pending_commit() {
        use koidrive_engine::question::{ConflictFile, Revisions};

        let validator =
            jsonschema::validator_for(&crate::schema::envelope("resolve").unwrap()).unwrap();
        for sent_in in [Some("commit-42".to_owned()), None] {
            let kind = QuestionKind::Conflict {
                original: ConflictFile {
                    node_id: "original-node".into(),
                    revisions: Revisions {
                        content_rev: 43,
                        meta_rev: 17,
                    },
                },
                copy: ConflictFile {
                    node_id: "copy-node".into(),
                    revisions: Revisions {
                        content_rev: u64::MAX,
                        meta_rev: 29,
                    },
                },
                sent_in,
            };
            let decision = Decision::of(&question("q1", kind.clone()));
            let mut done = Done::ok(
                AnsweredData {
                    decision_id: "q1".into(),
                    answer: "keep-both".into(),
                },
                "",
            );
            done.decisions.push(decision);
            let envelope = serde_json::to_value(Envelope::of("resolve", done)).unwrap();
            let conflict = envelope["decisions"][0]["conflict"].clone();
            assert_eq!(conflict["type"], "conflict");
            assert_eq!(
                serde_json::from_value::<QuestionKind>(conflict).unwrap(),
                kind
            );
            assert_eq!(envelope["schema_version"], 1);
            assert!(validator.is_valid(&envelope), "{envelope:#}");
        }
        let decision = serde_json::to_value(Decision::of(&question("q2", notice()))).unwrap();
        assert!(decision.get("conflict").is_none());
    }

    #[test]
    fn a_conflict_answers_with_the_node_to_keep_or_keep_both() {
        use koidrive_engine::question::{ConflictFile, Revisions};

        let file = |node_id: &str| ConflictFile {
            node_id: node_id.into(),
            revisions: Revisions {
                content_rev: 2,
                meta_rev: 1,
            },
        };
        let conflict = QuestionKind::Conflict {
            original: file("n1"),
            copy: file("n2"),
            sent_in: None,
        };
        let decision = Decision::of(&question("q1", conflict));
        assert_eq!(
            (decision.kind, decision.group, decision.counts),
            (DecisionKind::Conflict, None, None)
        );
        let answers: Vec<(&str, Vec<&str>)> = (decision.answers.iter())
            .map(|answer| {
                let argv = answer.argv.iter().map(String::as_str).collect();
                (answer.token.as_str(), argv)
            })
            .collect();
        assert_eq!(
            answers,
            [
                ("keep-node:n1", vec!["koi", "resolve", "q1", "keep-node:n1"]),
                ("keep-node:n2", vec!["koi", "resolve", "q1", "keep-node:n2"]),
                ("keep-both", vec!["koi", "resolve", "q1", "keep-both"]),
            ]
        );
        assert_eq!(
            decision.answers[0].consequence,
            "Keep archive/a and move archive/b to the trash"
        );
    }

    #[test]
    fn check_exits_with_the_most_urgent_condition_first() {
        let halted = folder("Work", FolderState::Halted);
        let offline = folder("Photos", FolderState::Offline);
        let choice = question("q1", held());

        assert_eq!(
            exit(
                &status(false, vec![halted.clone(), offline.clone()]),
                std::slice::from_ref(&choice)
            ),
            4
        );
        assert_eq!(
            exit(
                &status(true, vec![halted.clone(), offline.clone()]),
                std::slice::from_ref(&choice)
            ),
            5
        );
        assert_eq!(
            exit(
                &status(true, vec![folder("Work", FolderState::Stopped)]),
                std::slice::from_ref(&choice)
            ),
            5,
            "a stopped folder counts as halted"
        );
        assert_eq!(
            exit(
                &status(true, vec![offline.clone()]),
                std::slice::from_ref(&choice)
            ),
            3
        );
        assert_eq!(exit(&status(true, vec![offline]), &[]), 6);
        assert_eq!(exit(&status(true, Vec::new()), &[]), 0);
    }

    #[test]
    fn a_pause_adds_no_condition_and_the_folders_it_stopped_are_none() {
        let paused = |folders| Status {
            paused: true,
            ..status(true, folders)
        };
        let stopped = paused(vec![folder("Work", FolderState::Stopped)]);
        assert!(conditions(&stopped, &[]).is_empty());
        assert_eq!(exit(&stopped, &[]), 0);
        let choice = question("q1", held());
        assert_eq!(exit(&stopped, std::slice::from_ref(&choice)), 3);
        let halted = paused(vec![folder("Work", FolderState::Halted)]);
        assert_eq!(exit(&halted, &[]), 5);
        assert_eq!(
            exit(
                &status(true, vec![folder("Work", FolderState::Stopped)]),
                &[]
            ),
            5
        );
    }

    #[test]
    fn a_quota_or_retry_wait_exits_unavailable_from_the_typed_field_alone() {
        let waiting = |kind| FolderStatus {
            recovery: None,
            problem: Some("no words here decide the exit".into()),
            wait: Some(Wait {
                kind,
                until_ms: 1_700_000_000_000,
            }),
            ..folder("Work", FolderState::CatchingUp)
        };
        assert_eq!(exit(&status(true, vec![waiting(WaitKind::Quota)]), &[]), 6);
        assert_eq!(exit(&status(true, vec![waiting(WaitKind::Retry)]), &[]), 6);
        assert_eq!(
            exit(
                &status(true, vec![waiting(WaitKind::Quota)]),
                &[question("q1", held())]
            ),
            3
        );
        let mut quota_text = folder("Work", FolderState::UpToDate);
        quota_text.problem = Some("QUOTA_EXCEEDED: the server's storage is full".into());
        assert_eq!(exit(&status(true, vec![quota_text]), &[]), 0);

        let found = conditions(&status(true, vec![waiting(WaitKind::Quota)]), &[]);
        assert_eq!(
            serde_json::to_value(&found).unwrap(),
            serde_json::json!([{
                "condition": "waiting", "folder": "/home/ada/Work", "kind": "quota", "until_ms": 1_700_000_000_000_i64
            }])
        );
        let data = FolderData::of(&waiting(WaitKind::Retry));
        assert_eq!(
            serde_json::to_value(&data).unwrap()["wait"],
            serde_json::json!({ "kind": "retry", "until_ms": 1_700_000_000_000_i64 })
        );
    }

    #[test]
    fn connecting_catching_up_and_a_dismiss_only_notice_never_fail_the_check() {
        let busy = status(
            true,
            vec![
                folder("A", FolderState::Connecting),
                folder("B", FolderState::Bootstrapping),
                folder("C", FolderState::CatchingUp),
                folder("D", FolderState::UpToDate),
            ],
        );
        let notice = question("q9", notice());
        assert_eq!(exit(&busy, std::slice::from_ref(&notice)), 0);
        assert!(conditions(&busy, &[notice]).is_empty());
    }

    #[test]
    fn the_status_data_reports_every_condition_with_its_folder() {
        let mut halted = folder("Work", FolderState::Halted);
        halted.problem = Some("the folder is gone".into());
        let found = conditions(
            &status(false, vec![halted, folder("Photos", FolderState::Offline)]),
            &[question("q1", held())],
        );
        assert_eq!(
            serde_json::to_value(&found).unwrap(),
            serde_json::json!([
                { "condition": "not_set_up" },
                { "condition": "halted", "folder": "/home/ada/Work", "problem": "the folder is gone" },
                { "condition": "offline", "folder": "/home/ada/Photos" },
                { "condition": "decision_required", "decision_id": "q1", "space_id": "space-Work" },
            ])
        );
        assert_eq!(check_exit(&found), 4);
    }
}
