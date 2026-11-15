// SPDX-License-Identifier: Apache-2.0

//! The first link of a folder to a space, by a device with no store (design section 6.4): there is
//! no S to compare L with, so L is matched against R by path and content, and what matches nothing
//! is held under a question until the person publishes or discards it.

use std::collections::{BTreeMap, BTreeSet};

use crate::classify::Trees;
use crate::commit::rekey;
use crate::ids::IdGen;
use crate::names::{name_key, nfc, same_name};
use crate::question::{Held, Question, QuestionKind, RecoveryCause, RecoveryGroup};
use crate::tree::{
    Content, FileIdentity, Kind, Node, NodeId, RemoteNode, Shape, Tree, same_content,
};

/// Each shape the server gave a node from the store's cursor on, so that a recovery finds a node at
/// a place it left, where a download made before the store lost track of it may still hold it on
/// disk (design section 6.4).
pub type History = BTreeMap<NodeId, Vec<Shape>>;

/// The node each local entry's mark names (design section 6.4), for the entries S does not hold,
/// keyed by their ID in L. The caller keeps only marks written on the entry as L holds it.
pub type Marks = BTreeMap<NodeId, NodeId>;

/// The history of a recovery whose log reaches back to the store's cursor: the shape `remote`, R
/// as the store held it, gives each live node, and then each shape in `log`.
pub fn since(remote: &Tree<RemoteNode>, log: History) -> History {
    let mut history: History = (remote.iter())
        .filter(|(_, node)| !node.trashed)
        .map(|(id, node)| (id.clone(), vec![node.shape.clone()]))
        .collect();
    for (id, shapes) in log {
        history.entry(id).or_default().extend(shapes);
    }
    history
}

/// Whether the folder synced the space before the store it links with: the server accepted commits
/// from this device in the current history, or the folder held the space's root marker before the
/// link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PriorSync {
    /// Neither the server nor the folder shows an earlier sync.
    Fresh,
    /// The server accepted commits from this device that no store remembers, or the folder held the
    /// space's marker before the link.
    Shown,
}

/// Matches L against R from the root down, and returns the topmost local nodes that match
/// nothing. A local entry matches the live node of R at the same parent with
/// exactly its name and kind, and for a file the same content. A folder also matches a folder R
/// marks as pending for its name (RFC 0017), which a commit gave it while another node held it. A matching entry is adopted: L's
/// node takes R's ID and S takes R's shape, and a matching folder's children are matched in turn,
/// so a folder whose contents differ is still adopted. Every other entry is returned with its
/// subtree, for the user to publish or discard: a name R does not hold, a name that only has the
/// same name key, another kind, or a file with other bytes. An entry the rules exclude here is
/// neither adopted nor returned, and neither is an entry S holds, though a folder S holds has its
/// children matched, so a recovery adopts what a store that lost track of it finds on disk. An
/// entry whose name S gives another node is returned, since S keeps its shapes during a recovery.
/// With `history`, as in a recovery, the disk may hold what the server sent before the store lost
/// track of it: an entry at a place no live node of R holds also matches the one node R holds in the
/// trash there that it matches, and then the one node of R that `history` shows at that place
/// with the entry's content, when R still gives the node that content. S then holds it there. An entry whose match S cannot hold there, as when it would make S a
/// cycle, is returned.
pub fn adopt(trees: &mut Trees, history: Option<&History>) -> Vec<NodeId> {
    adopt_around(trees, history, &BTreeMap::new())
}

pub(crate) fn adopt_around(
    trees: &mut Trees,
    history: Option<&History>,
    claimed: &BTreeMap<NodeId, NodeId>,
) -> Vec<NodeId> {
    let claimants: BTreeSet<&NodeId> = claimed.values().collect();
    let remote: BTreeMap<(NodeId, String), NodeId> = (trees.remote().iter())
        .filter(|(_, node)| !node.trashed)
        .map(|(id, node)| {
            (
                (node.shape.parent.clone(), nfc(&node.shape.name)),
                id.clone(),
            )
        })
        .collect();
    let mut pending_dirs: BTreeMap<(NodeId, String), Vec<NodeId>> = BTreeMap::new();
    for (id, node) in trees.remote().iter() {
        if node.trashed || node.shape.kind != Kind::Dir {
            continue;
        }
        if let Some(stem) = &node.pending_for {
            let key = (node.shape.parent.clone(), nfc(stem));
            pending_dirs.entry(key).or_default().push(id.clone());
        }
    }
    let mut trashed: BTreeMap<(NodeId, String), Vec<NodeId>> = BTreeMap::new();
    for (id, node) in trees.remote().iter().filter(|_| history.is_some()) {
        if node.trashed {
            let key = (node.shape.parent.clone(), nfc(&node.shape.name));
            trashed.entry(key).or_default().push(id.clone());
        }
    }
    let mut left: BTreeMap<(NodeId, String), Vec<(NodeId, Shape)>> = BTreeMap::new();
    for (id, shapes) in history.into_iter().flatten() {
        for shape in shapes {
            let key = (shape.parent.clone(), nfc(&shape.name));
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
            let key = (parent.clone(), nfc(&name));
            let matches = |remote_id: &NodeId, shape: &Shape| {
                (*remote_id == id || trees.local().get(remote_id).is_none())
                    && (claimed.get(remote_id))
                        .map_or(!claimants.contains(&id), |claimant| *claimant == id)
                    && (trees.synced_holders(&parent, &shape.name)).all(|held| held == remote_id)
                    && shape.kind == local.kind
                    && same_bytes(&shape.content, &local.content)
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
                let unique = matching.all(|(other, _)| *other == first.0);
                let held_now = (trees.remote().get(&first.0))
                    .is_some_and(|node| same_bytes(&node.shape.content, &first.1.content));
                (unique && held_now).then(|| first.clone())
            };
            let Some((remote_id, shape)) = found.or_else(earlier) else {
                held.push(id);
                continue;
            };
            let renamed = !same_name(&shape.name, &local.name);
            if !trees.set_synced_if_valid(remote_id.clone(), Some(shape)) {
                held.push(id);
                continue;
            }
            if renamed {
                trees.adopt_pending(remote_id.clone());
            }
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

#[derive(Clone, Copy, PartialEq)]
enum Pass {
    Latest,
    Older,
    InNewFolder,
}

pub(crate) fn adopt_by_marks_first(
    trees: &mut Trees,
    history: &History,
    marks: &Marks,
) -> BTreeSet<NodeId> {
    let folders = folder_places(trees, history);
    let (_, mut waiting) = follow_marks(trees, history, marks, &folders, Pass::Latest);
    'adopt: loop {
        adopt_around(trees, Some(history), &waiting);
        for pass in [Pass::Latest, Pass::Older, Pass::InNewFolder] {
            let (followed, left) = follow_marks(trees, history, marks, &folders, pass);
            waiting = left;
            if followed {
                continue 'adopt;
            }
        }
        return (waiting.into_iter())
            .filter(|(node, local)| node != local)
            .map(|(_, local)| local)
            .collect();
    }
}

fn follow_marks(
    trees: &mut Trees,
    history: &History,
    marks: &Marks,
    folders: &BTreeSet<(NodeId, String)>,
    pass: Pass,
) -> (bool, BTreeMap<NodeId, NodeId>) {
    let mut followed = false;
    loop {
        let claims = claimed_once(trees, marks);
        let mut waiting = BTreeMap::new();
        let mut found = Vec::new();
        for (local, node) in claims.iter().copied() {
            let Some(entry) = (trees.local().get(local))
                .filter(|_| !trees.excluded(local))
                .map(|entry| entry.shape.clone())
            else {
                continue;
            };
            let shapes = shapes_to_follow(trees, history, node, &entry);
            if shapes.is_empty() {
                continue;
            }
            let in_place = (shapes.iter())
                .any(|shape| shape.parent == entry.parent && same_name(&shape.name, &entry.name));
            let current = (trees.remote().get(node))
                .is_some_and(|remote| same_bytes(&remote.shape.content, &entry.content));
            if !in_place && !current {
                waiting.insert(node.clone(), local.clone());
                continue;
            }
            let new_folder = match settled_above(trees, &entry.parent) {
                None => false,
                Some(chain) => {
                    let top = (chain.last()).and_then(|top| trees.local().get(top));
                    let at_a_folder_place = top.is_some_and(|top| {
                        folders.contains(&(top.shape.parent.clone(), name_key(&top.shape.name)))
                    });
                    if pass != Pass::InNewFolder
                        || at_a_folder_place
                        || chain.iter().any(|id| marks.contains_key(id))
                    {
                        waiting.insert(node.clone(), local.clone());
                        continue;
                    }
                    true
                }
            };
            let tried = if pass == Pass::Latest && !new_folder {
                1
            } else {
                shapes.len()
            };
            match (shapes.into_iter().take(tried))
                .find(|shape| trees.fits_synced(node, Some(shape)))
            {
                Some(shape) => found.push((local.clone(), node.clone(), shape)),
                None => {
                    waiting.insert(node.clone(), local.clone());
                }
            }
        }
        let mut places: BTreeMap<(NodeId, String), usize> = BTreeMap::new();
        for (_, _, shape) in &found {
            *places
                .entry((shape.parent.clone(), name_key(&shape.name)))
                .or_default() += 1;
        }
        let (alone, shared): (Vec<_>, Vec<_>) = (found.into_iter())
            .partition(|(_, _, shape)| places[&(shape.parent.clone(), name_key(&shape.name))] == 1);
        let updates: Vec<(NodeId, Option<Shape>)> = (alone.iter())
            .map(|(_, node, shape)| (node.clone(), Some(shape.clone())))
            .collect();
        let together = !alone.is_empty() && trees.fits_synced_together(&updates);
        let held = shared
            .into_iter()
            .chain(if together { Vec::new() } else { alone.clone() });
        waiting.extend(held.map(|(local, node, _)| (node, local)));
        if !together {
            return (followed, waiting);
        }
        for (local, node, shape) in alone {
            let entry = &trees.local().get(&local).expect("a claim in L").shape;
            let pending = (trees.remote().get(&node)).is_some_and(|remote| {
                entry.kind == Kind::Dir
                    && remote.shape.parent == entry.parent
                    && remote.pending_for.as_deref() == Some(entry.name.as_str())
            });
            if pending {
                trees.adopt_pending(node.clone());
            }
            trees.set_synced(node.clone(), Some(shape));
            if local != node {
                rekey(trees, &local, &node);
            }
        }
        followed = true;
    }
}

fn folder_places(trees: &Trees, history: &History) -> BTreeSet<(NodeId, String)> {
    let shapes =
        (trees.remote().iter().map(|(_, node)| &node.shape)).chain(history.values().flatten());
    let pending = (trees.remote().iter()).filter_map(|(_, node)| {
        let stem = node
            .pending_for
            .as_deref()
            .filter(|_| node.shape.kind == Kind::Dir)?;
        Some((node.shape.parent.clone(), name_key(stem)))
    });
    (shapes.filter(|shape| shape.kind == Kind::Dir))
        .map(|shape| (shape.parent.clone(), name_key(&shape.name)))
        .chain(pending)
        .collect()
}

/// Whether the entry of L `id` is, or is inside, an entry `is_entry` names. A store rollback's
/// recovery does not find an entry by its node mark there, since the place a brought-back entry
/// takes is not one the user gave it (design §6.4).
pub fn at_or_under(trees: &Trees, id: &str, is_entry: impl Fn(&FileIdentity) -> bool) -> bool {
    let mut at = id;
    for _ in 0..=trees.local().len() {
        let Some(node) = (at != trees.local().root)
            .then(|| trees.local().get(at))
            .flatten()
        else {
            return false;
        };
        if is_entry(&node.identity) {
            return true;
        }
        at = &node.shape.parent;
    }
    false
}

fn settled_above(trees: &Trees, parent: &NodeId) -> Option<Vec<NodeId>> {
    let mut chain = Vec::new();
    let mut at = parent.clone();
    for _ in 0..=trees.local().len() {
        if at == trees.local().root || trees.synced().get(&at).is_some() {
            return (!chain.is_empty()).then_some(chain);
        }
        let above = trees.local().get(&at)?.shape.parent.clone();
        chain.push(at);
        at = above;
    }
    Some(chain)
}

fn claimed_once<'a>(trees: &Trees, marks: &'a Marks) -> Vec<(&'a NodeId, &'a NodeId)> {
    let mut claims: BTreeMap<&NodeId, usize> = BTreeMap::new();
    for node in marks.values() {
        *claims.entry(node).or_default() += 1;
    }
    (marks.iter())
        .filter(|(local, node)| {
            claims[node] == 1
                && trees.synced().get(local).is_none()
                && (local == node || trees.local().get(node).is_none())
        })
        .collect()
}

fn shapes_to_follow(trees: &Trees, history: &History, node: &str, entry: &Shape) -> Vec<Shape> {
    let Some(remote) = (trees.remote().get(node))
        .filter(|remote| !remote.trashed && remote.shape.kind == entry.kind)
    else {
        return Vec::new();
    };
    let logged = history.get(node).filter(|logged| !logged.is_empty());
    let with_content: Vec<&Shape> = (logged.into_iter().flatten().chain([&remote.shape]))
        .filter(|shape| shape.kind == entry.kind && same_bytes(&shape.content, &entry.content))
        .collect();
    let in_place = (with_content.iter().rev())
        .find(|shape| shape.parent == entry.parent && same_name(&shape.name, &entry.name));
    if let Some(shape) = in_place {
        return vec![(*shape).clone()];
    }
    let since = logged
        .map(|_| with_content.iter().rev())
        .into_iter()
        .flatten();
    since.map(|shape| (*shape).clone()).collect()
}

pub(crate) fn adopt_equal_files(trees: &mut Trees, held: &BTreeSet<NodeId>) {
    let created: Vec<(NodeId, NodeId, Shape)> = (trees.dirty_others().iter())
        .filter_map(|remote_id| {
            let remote = trees.remote().get(remote_id)?;
            let free =
                trees.local().get(remote_id).is_none() && trees.synced().get(remote_id).is_none();
            if !free || remote.trashed || remote.shape.kind != Kind::File {
                return None;
            }
            let name = nfc(&remote.shape.name);
            let id =
                (trees.local_holders(&remote.shape.parent, &remote.shape.name)).find(|id| {
                    trees.local().get(id).is_some_and(|node| {
                        node.shape.kind == Kind::File
                            && nfc(&node.shape.name) == name
                            && same_bytes(&remote.shape.content, &node.shape.content)
                    }) && !held.contains(*id)
                        && !trees.excluded(id)
                        && trees.synced().get(id).is_none()
                        && trees.remote().get(id).is_none()
                })?;
            Some((id.clone(), remote_id.clone(), remote.shape.clone()))
        })
        .collect();
    for (id, remote_id, shape) in created {
        if trees.set_synced_if_valid(remote_id.clone(), Some(shape)) {
            rekey(trees, &id, &remote_id);
        }
    }
}

fn same_bytes(a: &Option<Content>, b: &Option<Content>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => same_content(a, b),
        (a, b) => a.is_none() && b.is_none(),
    }
}

fn only<T>(items: impl IntoIterator<Item = T>) -> Option<T> {
    let mut items = items.into_iter();
    items.next().filter(|_| items.next().is_none())
}

pub(crate) fn below(trees: &Trees, tops: &[NodeId]) -> Vec<NodeId> {
    (tops.iter())
        .flat_map(|top| unsynced(trees, top).into_keys())
        .collect()
}

pub(crate) fn trashed_there(trees: &Trees) -> impl Fn(&NodeId) -> bool + '_ {
    let trashed: BTreeSet<Vec<String>> = (trees.remote().iter())
        .filter(|(_, node)| node.trashed)
        .filter_map(|(id, _)| key_path(trees.remote(), id))
        .collect();
    move |top| {
        (unsynced(trees, top).keys())
            .any(|id| key_path(trees.local(), id).is_some_and(|path| trashed.contains(&path)))
    }
}

fn key_path<N: Node>(tree: &Tree<N>, id: &str) -> Option<Vec<String>> {
    let mut keys = Vec::new();
    let mut current = id;
    for _ in 0..=tree.len() {
        if current == tree.root {
            keys.reverse();
            return Some(keys);
        }
        let shape = tree.get(current)?.shape();
        keys.push(name_key(&shape.name));
        current = &shape.parent;
    }
    None
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
        let known = trees.acknowledged(&id).is_some() || trees.remote().get(&id).is_some();
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
                same_name(&mine.name, &node.shape.name)
                    && mine.kind == node.shape.kind
                    && alike
                    && (mine.kind != Kind::Dir || holds_no_more(trees, remote_id, id))
            })
        })
}
