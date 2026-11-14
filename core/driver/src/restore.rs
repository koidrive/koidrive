// SPDX-License-Identifier: Apache-2.0

//! Restores a person asks for (design §6.3): bringing a trashed node back, or making an earlier version of a file
//! current again. The server does both with a `Restore` op, and only the engine may send it, because the engine owns
//! the device's commit slot. So [`Driver::restore`](crate::Driver::restore) only records the request in the folder's
//! store. The task that runs the engine takes it from there, and [`Driver::restores`](crate::Driver::restores) reads
//! how each request ended.

use std::hash::{BuildHasher, Hasher, RandomState};

use koidrive_engine::ids::{unix_ms, uuid_v7};
use koidrive_engine::restore::{Malformed, Outcome, Rejection, RestoreIntent};
use koidrive_engine::tree::{RemoteNode, Tree};
use koidrive_store::Tx;
use serde::{Deserialize, Serialize};

/// What to restore. The IDs come from the server's `ListTrash` and `ListVersions`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RestoreRequest {
    /// Brings a trashed node back with the nodes under it that were trashed with it. It returns under `parent_id`
    /// and as `name` when given, and otherwise where it was. It keeps its node ID and its versions.
    FromTrash {
        node_id: String,
        parent_id: Option<String>,
        name: Option<String>,
    },
    /// Makes an earlier version of a file its current content, as a new version of the same node. `saved_at_ms` is
    /// when that version was saved, from `ListVersions`; none when the store does not know it.
    Version {
        node_id: String,
        version_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        saved_at_ms: Option<i64>,
    },
}

impl RestoreRequest {
    pub(crate) fn intent(self) -> Result<RestoreIntent, Malformed> {
        match self {
            Self::FromTrash {
                node_id,
                parent_id,
                name,
            } => RestoreIntent::from_trash(node_id, parent_id, name),
            Self::Version {
                node_id,
                version_id,
                ..
            } => RestoreIntent::version(node_id, version_id),
        }
    }

    pub(crate) fn saved_at_ms(&self) -> Option<i64> {
        match self {
            Self::Version { saved_at_ms, .. } => *saved_at_ms,
            Self::FromTrash { .. } => None,
        }
    }

    fn of(intent: &RestoreIntent, saved_at_ms: Option<i64>) -> Self {
        let node_id = intent.node().to_owned();
        match intent.version_id() {
            Some(version_id) => Self::Version {
                node_id,
                version_id: version_id.to_owned(),
                saved_at_ms,
            },
            None => Self::FromTrash {
                node_id,
                parent_id: intent.parent().map(str::to_owned),
                name: intent.name().map(str::to_owned),
            },
        }
    }
}

/// A restore that was asked for, with where it stands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreStatus {
    /// The ID [`Driver::restore`](crate::Driver::restore) returned.
    pub id: String,
    /// The node's path in the space: where it is, or where it was while it is in the trash. When the folder's copy of
    /// the server's tree lacks the node or a folder above it, the path the person saw when they asked for the restore,
    /// and `None` only when that is unknown too.
    #[serde(default)]
    pub path: Option<String>,
    /// When the restore was asked for, from its ID.
    #[serde(default)]
    pub asked_at_ms: i64,
    pub request: RestoreRequest,
    pub state: RestoreState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RestoreState {
    /// Not sent yet, or sent and not answered. It waits while the folder is not syncing, offline or halted, and
    /// while the folder has not synced the other changes of its node. `waits_for` names what else holds it back
    /// when the store shows it. Without a reason the restore can still wait for a change that another node's
    /// problem holds back, so a surface says that it waits for the folder to sync and shows the folder's
    /// problems. It must not promise that the restore goes through by itself.
    Pending {
        waits_for: Option<RestoreWait>,
    },
    /// The server applied it, and the folder follows as for any change another device made. A node that was not in
    /// the trash counts as restored.
    Restored,
    /// Restored under another name, because its name was taken where it returned.
    Renamed {
        name: String,
    },
    /// Another device changed the file in the meantime, so the version came back as a new file next to it.
    Copied {
        node_id: String,
        name: String,
    },
    Rejected {
        reason: RestoreRejection,
    },
    /// The nodes the restore brings back are too many or too deep for one commit.
    DoesNotFit,
}

/// What holds a pending restore back besides the next commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreWait {
    /// The node is a problem: the server refused a change this device made to it, and the engine sends nothing
    /// for the node until the problem is cleared. A restore is sent after the unsent changes of its node, so that
    /// an edit made before the request cannot replace what the restore brings back. It waits with them.
    NodeProblem,
}

/// Why the server did not apply a restore, or why it was not sent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreRejection {
    /// The server no longer has the node, the version or the folder to restore into: it was purged or it expired.
    NotFound,
    /// Only a file has versions.
    NotFile,
    InvalidName,
    /// The node to restore into is a file.
    ParentNotDir,
    /// The folder to restore into is the node itself or inside it.
    Cycle,
    /// The node is the root of the space.
    Root,
    /// The server takes no writes from this device.
    ReadOnly,
    /// The person cancelled it during recovery (design §6.4).
    Cancelled,
    /// A reason this build does not know, as the server spells it.
    Other(String),
}

impl From<&Outcome> for RestoreState {
    fn from(outcome: &Outcome) -> Self {
        match outcome {
            Outcome::Restored => Self::Restored,
            Outcome::Renamed { name } => Self::Renamed { name: name.clone() },
            Outcome::Copied { node, name } => Self::Copied {
                node_id: node.clone(),
                name: name.clone(),
            },
            Outcome::Rejected(rejection) => Self::Rejected {
                reason: match rejection {
                    Rejection::NotFound => RestoreRejection::NotFound,
                    Rejection::NotFile => RestoreRejection::NotFile,
                    Rejection::InvalidName => RestoreRejection::InvalidName,
                    Rejection::ParentNotDir => RestoreRejection::ParentNotDir,
                    Rejection::Cycle => RestoreRejection::Cycle,
                    Rejection::Root => RestoreRejection::Root,
                    Rejection::ReadOnly => RestoreRejection::ReadOnly,
                    Rejection::Cancelled => RestoreRejection::Cancelled,
                    Rejection::Other(reason) => RestoreRejection::Other(reason.clone()),
                },
            },
            Outcome::DoesNotFit => Self::DoesNotFit,
        }
    }
}

/// A new restore ID: a UUIDv7, so the IDs of a store sort in the order of their requests, to the millisecond.
pub(crate) fn new_id(now_ms: u64) -> String {
    let random = || u128::from(RandomState::new().build_hasher().finish());
    uuid_v7(now_ms, random() << 64 | random())
}

/// Every restore the store knows in the space whose root node is `root`: first those that ended, in the order they
/// ended, then those that wait, in request order. A restore the engine has taken and one it has not are both pending,
/// and either says so when its node is a problem.
pub(crate) fn list(tx: &Tx<'_>, root: &str) -> Result<Vec<RestoreStatus>, koidrive_store::Error> {
    let (queued, resolved) = tx.restores()?;
    let problems = tx.problem_nodes()?;
    let asked = tx.asked_restores()?;
    let mut remote = Tree::new(root);
    for state in tx.remote_nodes()? {
        if let Ok((id, node)) = RemoteNode::from_state(&state) {
            remote.insert(id, node);
        }
    }
    let status = |id: &str, intent: &RestoreIntent, state| {
        let asked = asked.get(id).cloned().unwrap_or_default();
        RestoreStatus {
            id: id.to_owned(),
            path: (remote.path(intent.node()))
                .and_then(|path| path.to_str().map(str::to_owned))
                .or(asked.path),
            asked_at_ms: unix_ms(id).map_or(0, |ms| ms as i64),
            request: RestoreRequest::of(intent, asked.version_saved_at_ms),
            state,
        }
    };
    let ended = (resolved.iter())
        .map(|ended| status(&ended.id, &ended.intent, RestoreState::from(&ended.outcome)));
    let mut waiting: Vec<_> = queued.into_iter().chain(tx.restore_requests()?).collect();
    waiting.sort_by(|(a, _), (b, _)| a.cmp(b));
    let waiting = waiting.iter().map(|(id, intent)| {
        let waits_for = (intent.held_by(&problems)).then_some(RestoreWait::NodeProblem);
        status(id, intent, RestoreState::Pending { waits_for })
    });
    Ok(ended.chain(waiting).collect())
}
