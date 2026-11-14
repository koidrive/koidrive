// SPDX-License-Identifier: Apache-2.0

//! The first link of a folder to a space, by a device with no store (design section 6.4): there is
//! no S to compare L with, so L is matched against R by path and content, and what matches nothing
//! is held under a question until the person publishes or discards it.

use std::collections::{BTreeMap, BTreeSet};

use crate::classify::Trees;
use crate::commit::rekey;
use crate::ids::IdGen;
use crate::question::{Held, Question, QuestionKind, RecoveryCause, RecoveryGroup};
use crate::tree::{Kind, NodeId, Shape, pending_stem, same_content};

/// Each shape the log shows the server gave a node after the store's cursor, so that a recovery
/// finds a node at a place it left, where a download made before the store lost track of it may
/// still hold it on disk (design section 6.4).
pub type History = BTreeMap<NodeId, Vec<Shape>>;

/// Matches L against R from the root down, and returns the topmost local nodes that match
/// nothing. A local entry matches the live node of R at the same parent with
/// exactly its name and kind, and for a file the same content. A folder also matches a folder R
/// holds under its pending name, which a commit gave it while another node held its name. A matching entry is adopted: L's
/// node takes R's ID and S takes R's shape, and a matching folder's children are matched in turn,
/// so a folder whose contents differ is still adopted. Every other entry is returned with its
/// subtree, for the user to publish or discard: a name R does not hold, a name that only has the
/// same name key, another kind, or a file with other bytes. An entry the rules exclude here is
/// neither adopted nor returned, and neither is an entry S holds, though a folder S holds has its
/// children matched, so a recovery adopts what a store that lost track of it finds on disk. An
/// entry whose name S gives another node is returned, since S keeps its shapes during a recovery.
/// With `history`, as in a recovery, the disk may hold what the server sent before the store lost
/// track of it: an entry at a place no live node of R holds also matches the one node R holds in the
/// trash there that it matches, and then the one node of R that `history` shows at that place,
/// which S then holds there.
pub fn adopt(trees: &mut Trees, history: Option<&History>) -> Vec<NodeId> {
    let remote: BTreeMap<(NodeId, String), NodeId> = (trees.remote().iter())
        .filter(|(_, node)| !node.trashed)
        .map(|(id, node)| {
            (
                (node.shape.parent.clone(), node.shape.name.clone()),
                id.clone(),
            )
        })
        .collect();
    let mut pending_dirs: BTreeMap<(NodeId, String), Vec<NodeId>> = BTreeMap::new();
    for (id, node) in trees.remote().iter() {
        if node.trashed || node.shape.kind != Kind::Dir {
            continue;
        }
        if let Some(stem) = pending_stem(&node.shape.name) {
            let key = (node.shape.parent.clone(), stem.to_owned());
            pending_dirs.entry(key).or_default().push(id.clone());
        }
    }
    let mut trashed: BTreeMap<(NodeId, String), Vec<NodeId>> = BTreeMap::new();
    for (id, node) in trees.remote().iter().filter(|_| history.is_some()) {
        if node.trashed {
            let key = (node.shape.parent.clone(), node.shape.name.clone());
            trashed.entry(key).or_default().push(id.clone());
        }
    }
    let mut left: BTreeMap<(NodeId, String), Vec<(NodeId, Shape)>> = BTreeMap::new();
    for (id, shapes) in history.into_iter().flatten() {
        if trees.remote().get(id).is_none() {
            continue;
        }
        for shape in shapes {
            let key = (shape.parent.clone(), shape.name.clone());
            left.entry(key)
                .or_default()
                .push((id.clone(), shape.clone()));
        }
    }
    let mut held = Vec::new();
    let mut pending = vec![trees.local().root.clone()];
    while let Some(parent) = pending.pop() {
        let mut children: Vec<(String, NodeId)> = (trees.local_children(&parent))
            .filter_map(|id| Some((trees.local().get(id)?.shape.name.clone(), id.clone())))
            .collect();
        children.sort();
        for (name, id) in children {
            if trees.excluded(&id) {
                continue;
            }
            let local = trees.local().get(&id).expect("a child in L").shape.clone();
            if trees.synced().get(&id).is_some() {
                if local.kind == Kind::Dir {
                    pending.push(id);
                }
                continue;
            }
            let key = (parent.clone(), name);
            let matches = |remote_id: &NodeId, shape: &Shape| {
                (*remote_id == id || trees.local().get(remote_id).is_none())
                    && (trees.synced_holders(&parent, &shape.name)).all(|held| held == remote_id)
                    && shape.kind == local.kind
                    && match (&shape.content, &local.content) {
                        (Some(remote), Some(local)) => same_content(remote, local),
                        (remote, local) => remote.is_none() && local.is_none(),
                    }
            };
            let pending_match = match pending_dirs.get(&key).map(Vec::as_slice) {
                Some([only]) if local.kind == Kind::Dir && holds_no_more(trees, only, &id) => {
                    Some(only)
                }
                _ => None,
            };
            let now = |remote_id: &&NodeId| {
                matches(
                    remote_id,
                    &trees.remote().get(remote_id).expect("indexed").shape,
                )
            };
            let in_trash = (trashed.get(&key)).filter(|_| !remote.contains_key(&key));
            let in_trash = in_trash.and_then(|ids| only(ids.iter().filter(now)));
            let found = (remote.get(&key).filter(now))
                .or_else(|| pending_match.filter(now))
                .or(in_trash)
                .map(|remote_id| {
                    let shape = &trees.remote().get(remote_id).expect("indexed").shape;
                    (remote_id.clone(), shape.clone())
                });
            let earlier = || {
                let mut matching =
                    (left.get(&key)?.iter()).filter(|(remote_id, shape)| matches(remote_id, shape));
                let first = matching.next()?;
                matching
                    .all(|(other, _)| *other == first.0)
                    .then(|| first.clone())
            };
            let Some((remote_id, shape)) = found.or_else(earlier) else {
                held.push(id);
                continue;
            };
            if shape.name != local.name {
                trees.adopt_pending(remote_id.clone());
            }
            trees.set_synced(remote_id.clone(), Some(shape));
            if id != remote_id {
                rekey(trees, &id, &remote_id);
            }
            if local.kind == Kind::Dir {
                pending.push(remote_id);
            }
        }
    }
    held
}

fn only<T>(items: impl IntoIterator<Item = T>) -> Option<T> {
    let mut items = items.into_iter();
    items.next().filter(|_| items.next().is_none())
}

/// Whether the question holds what a first link found on this device and nothing on the server.
pub(crate) fn asks(question: &Question) -> bool {
    matches!(
        question.kind,
        QuestionKind::Recovery {
            group: RecoveryGroup::CreatedOnThisDevice,
            ..
        }
    )
}

/// The question about the local entries `tops` and what is below them, as L holds them now, or none
/// when L holds none of them. An entry inside another's subtree is one of its members, and one the
/// user moved out of its folder is an item of its own. The question is
/// `asked`, with its answer, when it would hold the same nodes with the same shapes, and a new one
/// otherwise, so that an answer acts on what the person saw.
pub(crate) fn ask(
    trees: &Trees,
    tops: impl IntoIterator<Item = NodeId>,
    asked: Option<&Question>,
    cause: Option<&RecoveryCause>,
    ids: &mut IdGen,
    now_ms: u64,
) -> Option<Question> {
    let mut held = BTreeMap::new();
    let mut shown = BTreeMap::new();
    for top in tops {
        let members = unsynced(trees, &top);
        if members.is_empty() {
            continue;
        }
        let files = (members.values())
            .filter(|shape| shape.kind == Kind::File)
            .count() as u64;
        let held_members = members.keys().map(|id| (id.clone(), None)).collect();
        held.insert(
            top,
            Held {
                members: held_members,
                files,
            },
        );
        shown.extend(members);
    }
    let inner: BTreeSet<NodeId> = (held.iter())
        .flat_map(|(top, item)| item.members.keys().filter(move |id| *id != top))
        .cloned()
        .collect();
    held.retain(|top, _| !inner.contains(top));
    if held.is_empty() {
        return None;
    }
    if let Some(asked) = asked.filter(|asked| asked.held == held && asked.shown == shown) {
        return Some(asked.clone());
    }
    Some(Question {
        id: ids.uuid_v7(now_ms),
        asked_at_ms: now_ms,
        kind: QuestionKind::Recovery {
            group: RecoveryGroup::CreatedOnThisDevice,
            sent_in: None,
            cause: cause.cloned(),
        },
        held,
        shown,
        answer: None,
    })
}

fn unsynced(trees: &Trees, top: &str) -> BTreeMap<NodeId, Shape> {
    let mut members = BTreeMap::new();
    let mut pending = vec![top.to_owned()];
    while let Some(id) = pending.pop() {
        let Some(node) = trees.local().get(&id) else {
            continue;
        };
        let known = trees.synced().get(&id).is_some() || trees.remote().get(&id).is_some();
        if known || (id != top && trees.excluded(&id)) {
            continue;
        }
        pending.extend(trees.local_children(&id).cloned());
        members.insert(id, node.shape.clone());
    }
    members
}

fn holds_no_more(trees: &Trees, remote: &str, local: &str) -> bool {
    (trees.remote().iter())
        .filter(|(_, node)| !node.trashed && node.shape.parent == remote)
        .all(|(remote_id, node)| {
            trees.local_children(local).any(|id| {
                let Some(mine) = trees.local().get(id).map(|node| &node.shape) else {
                    return false;
                };
                let alike = match (&node.shape.content, &mine.content) {
                    (Some(remote), Some(local)) => same_content(remote, local),
                    (remote, local) => remote.is_none() && local.is_none(),
                };
                mine.name == node.shape.name
                    && mine.kind == node.shape.kind
                    && alike
                    && (mine.kind != Kind::Dir || holds_no_more(trees, remote_id, id))
            })
        })
}
