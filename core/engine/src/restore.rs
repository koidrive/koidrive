// SPDX-License-Identifier: Apache-2.0

//! Restores a person asks for (docs/design/sync-engine.md sections 2.6, 2.7 and 6.3; spec v0 rules
//! R9 and R10): bringing a trashed node back, or making an earlier version of a file current. A
//! restore changes none of the trees, so the planner cannot find it as a difference between L and
//! S. It is an explicit intent that [`Commits`](crate::commit::Commits) keeps from the request
//! until the commit that carries its `Restore` op is acknowledged.

use std::collections::BTreeSet;

use koidrive_proto::v1::{self as proto, op_result};

use crate::commit::MAX_OPS;
use crate::ids::is_uuid_v7;
use crate::tree::NodeId;

/// Names a restore from the request to its outcome. The caller mints it as a UUIDv7, so the order
/// of the IDs is the order of the requests.
pub type RestoreId = String;

/// The outcomes [`Commits`](crate::commit::Commits) keeps, newest last. The outcomes of one full
/// commit of restores always fit.
pub const RESOLVED_KEPT: usize = MAX_OPS;

/// What a person asked the server to bring back, as the `Restore` op that asks for it. Every ID
/// in it is a UUIDv7: the server refuses a whole commit for one malformed ID, and the engine
/// answers that refusal by halting the space.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreIntent(proto::Restore);

/// A restore too large for one commit, sent as restores of its trash group's topmost members (R8)
/// and then its own op: the group and the place its node held in the trash when it began.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SplitRestore {
    pub trash_group: String,
    pub parent: NodeId,
    pub name: String,
    pub folders: BTreeSet<NodeId>,
    pub landed: Option<(NodeId, String)>,
}

/// The field of a restore that is not a lowercase UUIDv7, or that a version restore must leave
/// empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Malformed(pub &'static str);

impl RestoreIntent {
    /// Brings a trashed node back with the nodes under it that were trashed with it (R9): under
    /// `parent` and as `name` when given, and otherwise where it was.
    pub fn from_trash(
        node: NodeId,
        parent: Option<NodeId>,
        name: Option<String>,
    ) -> Result<Self, Malformed> {
        Self::from_op(proto::Restore {
            node_id: node,
            version_id: String::new(),
            target_parent_id: parent.unwrap_or_default(),
            name: name.unwrap_or_default(),
        })
    }

    /// Makes the content of an earlier version the file's current content, as a new version
    /// (R10).
    pub fn version(node: NodeId, version_id: String) -> Result<Self, Malformed> {
        if version_id.is_empty() {
            return Err(Malformed("version_id"));
        }
        Self::from_op(proto::Restore {
            node_id: node,
            version_id,
            ..Default::default()
        })
    }

    /// Reads a `Restore` op, as the store keeps an intent. A version restore names no position.
    pub fn from_op(op: proto::Restore) -> Result<Self, Malformed> {
        let optional = |id: &str| id.is_empty() || is_uuid_v7(id);
        if !is_uuid_v7(&op.node_id) {
            return Err(Malformed("node_id"));
        }
        if !optional(&op.version_id) {
            return Err(Malformed("version_id"));
        }
        if !optional(&op.target_parent_id)
            || (!op.version_id.is_empty() && !op.target_parent_id.is_empty())
        {
            return Err(Malformed("target_parent_id"));
        }
        if !op.version_id.is_empty() && !op.name.is_empty() {
            return Err(Malformed("name"));
        }
        Ok(Self(op))
    }

    /// The op a commit carries for this restore. An op in flight belongs to a queued restore
    /// whose op equals it.
    pub fn op(&self) -> &proto::Restore {
        &self.0
    }

    pub fn node(&self) -> &str {
        &self.0.node_id
    }

    /// The version to make current, or None for a restore from the trash.
    pub fn version_id(&self) -> Option<&str> {
        Some(self.0.version_id.as_str()).filter(|id| !id.is_empty())
    }

    /// The folder a trashed node returns to, when the request named one.
    pub fn parent(&self) -> Option<&str> {
        Some(self.0.target_parent_id.as_str()).filter(|id| !id.is_empty())
    }

    /// The name a trashed node returns under, when the request named one.
    pub fn name(&self) -> Option<&str> {
        Some(self.0.name.as_str()).filter(|name| !name.is_empty())
    }

    /// Whether the restore waits because its node is one of `problems`, the nodes the server
    /// rejected an op for ([`Commits::problems`](crate::commit::Commits::problems)). The
    /// planner sends nothing for a problem node, and a restore follows the changes of its node,
    /// so it is sent only once the problem is cleared.
    pub fn held_by(&self, problems: &BTreeSet<NodeId>) -> bool {
        problems.contains(self.node())
    }
}

/// How a restore ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The server applied it. A node that was already live counts as restored.
    Restored,
    /// Applied under another name, because the node's name was taken where it returned (R1).
    Renamed {
        name: String,
    },
    /// Another device changed the file after this device last saw it, so the version's content
    /// is a new file `node` next to it (R3).
    Copied {
        node: NodeId,
        name: String,
    },
    Rejected(Rejection),
    /// The restore was alone in its commit and the commit still did not fit
    /// (`LIMIT_EXCEEDED`): the nodes it brings back are too many or too deep for one commit.
    DoesNotFit,
}

impl Outcome {
    /// Reads the result of a `Restore` op. A code the spec does not give a restore is a
    /// rejection that names the code, so the restore still ends.
    pub(crate) fn from_result(result: &proto::OpResult) -> Self {
        use op_result::Code;
        match Code::try_from(result.code) {
            Ok(Code::Applied) => Self::Restored,
            Ok(Code::AppliedRenamed) => Self::Renamed {
                name: result.name.clone(),
            },
            Ok(Code::ConflictCopy) => Self::Copied {
                node: result.node_id.clone(),
                name: result.name.clone(),
            },
            Ok(Code::Rejected) => Self::Rejected(Rejection::from_reason(&result.reject_reason)),
            _ => Self::Rejected(Rejection::Other(format!("result code {}", result.code))),
        }
    }
}

/// Why a restore was not applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rejection {
    /// The server no longer has the node, the version, or the folder to restore into: it was
    /// purged or it expired. A version of another node is not found either.
    NotFound,
    /// A version restore of a folder.
    NotFile,
    InvalidName,
    /// The node to restore into is a file.
    ParentNotDir,
    /// The folder to restore into is the node itself or inside it.
    Cycle,
    /// The node is the root of the space.
    Root,
    /// The server takes no writes from this device, so the restore was not sent.
    ReadOnly,
    /// The person cancelled a restore they had asked for before recovery began (design section
    /// 6.4).
    Cancelled,
    /// A reason this build does not know. Build it with [`Rejection::from_reason`].
    Other(String),
}

impl Rejection {
    pub fn from_reason(reason: &str) -> Self {
        match reason {
            "NOT_FOUND" => Self::NotFound,
            "NOT_FILE" => Self::NotFile,
            "INVALID_NAME" => Self::InvalidName,
            "PARENT_NOT_DIR" => Self::ParentNotDir,
            "CYCLE" => Self::Cycle,
            "ROOT" => Self::Root,
            "READ_ONLY" => Self::ReadOnly,
            "CANCELLED" => Self::Cancelled,
            other => Self::Other(other.to_owned()),
        }
    }

    /// The reason as the server spells it, which is also how the store keeps it.
    pub fn reason(&self) -> &str {
        match self {
            Self::NotFound => "NOT_FOUND",
            Self::NotFile => "NOT_FILE",
            Self::InvalidName => "INVALID_NAME",
            Self::ParentNotDir => "PARENT_NOT_DIR",
            Self::Cycle => "CYCLE",
            Self::Root => "ROOT",
            Self::ReadOnly => "READ_ONLY",
            Self::Cancelled => "CANCELLED",
            Self::Other(reason) => reason,
        }
    }
}

/// A restore that ended, with how.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub id: RestoreId,
    pub intent: RestoreIntent,
    pub outcome: Outcome,
}
