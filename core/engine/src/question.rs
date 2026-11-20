// SPDX-License-Identifier: Apache-2.0

//! Questions the engine asks the person (`AskUser`, design §1): what each asks, the nodes it holds
//! until it is answered, and the answers. The planner leaves every node an open question holds
//! alone, and the answer is routed by the question's kind. With the `serde` feature the kinds and
//! answers serialize the way the driver stores and shows them.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use crate::tree::{NodeId, Shape};

/// An open question and what it holds.
#[derive(Clone, Debug, PartialEq)]
pub struct Question {
    /// Unique per question. A question whose held nodes change is replaced by one with a new id, so
    /// an answer always refers to what the person saw.
    pub id: String,
    pub asked_at_ms: u64,
    pub kind: QuestionKind,
    /// What the question holds, by the topmost node of each item.
    pub held: BTreeMap<NodeId, Held>,
    /// An answer the engine took but applies only when it next plans, if what the question holds
    /// has not changed by then.
    pub answer: Option<Answer>,
    /// The shape L had for each held node R does not hold, when the question was asked. A change
    /// to any of them replaces the question.
    pub shown: BTreeMap<NodeId, Shape>,
    /// What was below a first-link question's folders when the person answered Discard, apart
    /// from what it shows, and which of it the Discard takes along. `None` until then, and when
    /// the build decides instead.
    pub frozen: Option<BTreeMap<NodeId, Frozen>>,
}

/// A node below a first-link question's folders when the person answered: where L held it then,
/// and whether the Discard takes it along as a server entry.
#[derive(Clone, Debug, PartialEq)]
pub struct Frozen {
    pub parent: NodeId,
    pub name: String,
    pub taken: bool,
}

impl Question {
    /// Every node the question holds.
    pub fn holds(&self) -> impl Iterator<Item = &NodeId> {
        self.held.values().flat_map(|held| held.members.keys())
    }

    /// What started the recovery that asked this question.
    pub fn cause(&self) -> Option<&RecoveryCause> {
        match &self.kind {
            QuestionKind::Recovery { cause, .. } => cause.as_ref(),
            _ => None,
        }
    }
}

/// One item a question holds: its topmost node and the nodes below it that the answer applies to,
/// each with the revisions R held for it when it was asked, and the files among them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Held {
    pub members: BTreeMap<NodeId, Option<Revisions>>,
    pub files: u64,
}

/// A node's revisions in R. A held node R does not hold has none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Revisions {
    pub content_rev: u64,
    pub meta_rev: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "type", rename_all = "snake_case"))]
pub enum QuestionKind {
    /// The mass-delete guard held local deletions (design §6.1).
    MassDelete {
        deletions: u64,
        affected_files: u64,
        space_files: u64,
        /// The limits the deletions crossed, also those of the questions this one replaced.
        #[cfg_attr(feature = "serde", serde(default))]
        exceeded: Vec<Limit>,
    },
    /// Deletions made on other devices that this device applied crossed a limit of the guard
    /// (design §6.1). It holds nothing, and its counts grow until the person dismisses it.
    RemoteMassDelete {
        deletions: u64,
        affected_files: u64,
        space_files: u64,
        exceeded: Vec<Limit>,
    },
    /// Files disappeared while changes were not tracked and may have moved into an ignored folder
    /// (design §5.10).
    AmbiguousDisappearance,
    /// One group of files a recovery holds (design §6.4).
    Recovery {
        group: RecoveryGroup,
        /// The ID of the commit that carries the answer's ops, while it is in flight. A commit
        /// refused or abandoned asks the question again.
        #[cfg_attr(
            feature = "serde",
            serde(default, skip_serializing_if = "Option::is_none")
        )]
        sent_in: Option<String>,
        /// What started the recovery; none for a first link.
        #[cfg_attr(
            feature = "serde",
            serde(default, skip_serializing_if = "Option::is_none")
        )]
        cause: Option<RecoveryCause>,
    },
    /// The server gave a write of this device a node of its own (design §2.7): an edit became the
    /// conflict copy `copy` of `original`, or a file this device created or moved took a
    /// conflict name because `original` held the name. A create that a first link's
    /// [`Answer::Publish`] sent asks nothing, since that answer kept both. It holds nothing, so
    /// both files keep syncing. It is asked again when either file's content changes, and it ends
    /// when either is renamed, moved or deleted, here or on another device.
    Conflict {
        original: ConflictFile,
        copy: ConflictFile,
        /// The ID of the commit that carries the op of the answer, while it is in flight. Its result
        /// is applied to this question alone, whatever R holds by then.
        #[cfg_attr(
            feature = "serde",
            serde(default, skip_serializing_if = "Option::is_none")
        )]
        sent_in: Option<String>,
    },
}

/// One file of a conflict, with the revisions R held for it when it was asked.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ConflictFile {
    pub node_id: NodeId,
    pub revisions: Revisions,
}

/// A limit of the mass-delete guard (design §6.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Limit {
    /// More than [`MAX_DELETIONS`](crate::guard::MAX_DELETIONS) deletions in the window.
    Deletions,
    /// More than half of the files of a space of at least
    /// [`MIN_SPACE_FILES`](crate::guard::MIN_SPACE_FILES) files.
    Share,
}

/// What started a recovery (section 6.4), with the seq a rewind restored and the time the owner
/// asked for when the server names them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct RecoveryCause {
    pub trigger: RecoveryTrigger,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub restore_seq: Option<u64>,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub restore_point_ms: Option<i64>,
}

impl RecoveryCause {
    /// A rewind to `restore_seq` that the owner asked for at `restore_point_ms`, as `Welcome` and
    /// `ResyncRequired` carry them: a time of 0 or less is unknown.
    pub fn rewind(restore_seq: u64, restore_point_ms: i64) -> RecoveryCause {
        RecoveryCause {
            trigger: RecoveryTrigger::SpaceRewound,
            restore_seq: Some(restore_seq),
            restore_point_ms: (restore_point_ms > 0).then_some(restore_point_ms),
        }
    }

    pub fn of(trigger: RecoveryTrigger) -> RecoveryCause {
        RecoveryCause {
            trigger,
            restore_seq: None,
            restore_point_ms: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum RecoveryTrigger {
    /// The owner rewound the space, or its server state was restored (`HISTORY_CHANGED`).
    SpaceRewound,
    /// The server's count of this device's commits does not match the folder's sync state.
    StoreRolledBack,
    /// The folder's sync state is older than the files on disk.
    StoreOlderThanDisk,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum RecoveryGroup {
    CreatedAfterRestorePoint,
    ChangedOnThisDevice,
    CreatedOnThisDevice,
    DeletedOnThisDevice,
    RequestedBeforeReset,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
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
    /// Close a notice.
    Dismiss,
    /// Keep the original of a conflict and move the copy to the trash.
    KeepOriginal,
    /// Give the original of a conflict the copy's content, keeping its old content in its version
    /// history, and move the copy to the trash.
    KeepCopy,
    /// Keep both files of a conflict and close the question.
    KeepBoth,
}

impl QuestionKind {
    /// The answers this kind of question accepts, in the order to offer them.
    pub fn answers(&self) -> &'static [Answer] {
        match self {
            QuestionKind::MassDelete { .. } | QuestionKind::AmbiguousDisappearance => {
                &[Answer::Delete, Answer::Restore]
            }
            QuestionKind::RemoteMassDelete { .. } => &[Answer::Dismiss],
            QuestionKind::Recovery { group, .. } => match group {
                RecoveryGroup::CreatedAfterRestorePoint => &[Answer::Keep, Answer::Discard],
                RecoveryGroup::ChangedOnThisDevice | RecoveryGroup::CreatedOnThisDevice => {
                    &[Answer::Publish, Answer::Discard]
                }
                RecoveryGroup::DeletedOnThisDevice => &[Answer::Delete, Answer::Restore],
                RecoveryGroup::RequestedBeforeReset => &[Answer::Repeat, Answer::Cancel],
            },
            QuestionKind::Conflict { .. } => {
                &[Answer::KeepOriginal, Answer::KeepCopy, Answer::KeepBoth]
            }
        }
    }
}

impl Answer {
    pub const ALL: [Answer; 11] = [
        Answer::Delete,
        Answer::Restore,
        Answer::Keep,
        Answer::Discard,
        Answer::Publish,
        Answer::Repeat,
        Answer::Cancel,
        Answer::Dismiss,
        Answer::KeepOriginal,
        Answer::KeepCopy,
        Answer::KeepBoth,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Answer::Delete => "delete",
            Answer::Restore => "restore",
            Answer::Keep => "keep",
            Answer::Discard => "discard",
            Answer::Publish => "publish",
            Answer::Repeat => "repeat",
            Answer::Cancel => "cancel",
            Answer::Dismiss => "dismiss",
            Answer::KeepOriginal => "keep_original",
            Answer::KeepCopy => "keep_copy",
            Answer::KeepBoth => "keep_both",
        }
    }
}

impl fmt::Display for Answer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Answer {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Answer::ALL
            .into_iter()
            .find(|answer| answer.as_str() == text)
            .ok_or_else(|| {
                let names: Vec<&str> = Answer::ALL.iter().map(|answer| answer.as_str()).collect();
                format!("{text:?} is not an answer; use one of {}", names.join(", "))
            })
    }
}
