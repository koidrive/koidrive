// SPDX-License-Identifier: Apache-2.0

//! Recovery after a rewind or a rolled-back store (design section 6.4). Once R holds the server's
//! tree in the history the device recovers into, every node is classified by whether this device
//! changed it since S, never by path or content. What is certain goes to the planner as it is, and
//! the rest is held under one question per group until the person answers.

use std::collections::{BTreeMap, BTreeSet};

use crate::classify::Trees;
use crate::ids::IdGen;
use crate::question::{
    Answer, Held, Question, QuestionKind, RecoveryCause, RecoveryGroup, Revisions,
};
use crate::restore::RestoreIntent;
use crate::tree::{Kind, NodeId, Shape, same};

/// Whether `question` holds nodes a recovery classified, so that neither an effect nor a commit is
/// planned for them. The entries created on this device go to a first link's question.
pub(crate) fn asks(question: &Question) -> bool {
    matches!(
        question.kind,
        QuestionKind::Recovery {
            group: RecoveryGroup::CreatedAfterRestorePoint
                | RecoveryGroup::ChangedOnThisDevice
                | RecoveryGroup::DeletedOnThisDevice,
            sent_in: None,
            ..
        }
    )
}

/// The group that holds `id`, or `None` when its outcome is certain.
pub(crate) fn group(trees: &Trees, id: &str) -> Option<RecoveryGroup> {
    if id == trees.local().root {
        return None;
    }
    let local = trees.local().get(id).map(|node| &node.shape);
    let synced = match trees.waiting().get(id) {
        Some(waiting) => waiting.shape.as_ref(),
        None => trees.synced().live(id),
    };
    let remote = trees.remote().get(id);
    let live = trees.remote().live(id);
    if live.is_some() && same(local, live) {
        return None;
    }
    let pending = |local: &Shape| {
        remote.is_some_and(|r| {
            !r.trashed
                && local.kind == Kind::Dir
                && local.parent == r.shape.parent
                && r.pending_for.as_deref() == Some(local.name.as_str())
        })
    };
    match (local, synced) {
        (Some(l), _) if trees.adopted(id) && pending(l) => None,
        (Some(_), Some(_)) if same(local, synced) && remote.is_none() => {
            Some(RecoveryGroup::CreatedAfterRestorePoint)
        }
        (Some(_), Some(_)) if same(local, synced) && !trees.unsent_edits().contains(id) => None,
        (Some(_), Some(_)) => Some(RecoveryGroup::ChangedOnThisDevice),
        (Some(_), None) if remote.is_none() => Some(RecoveryGroup::CreatedOnThisDevice),
        (Some(_), None) => Some(RecoveryGroup::ChangedOnThisDevice),
        (None, Some(_)) => live.is_some().then_some(RecoveryGroup::DeletedOnThisDevice),
        (None, None) => None,
    }
}

/// Every node of R, S and L that a group holds, by group.
pub(crate) fn classify(trees: &Trees) -> BTreeMap<RecoveryGroup, BTreeSet<NodeId>> {
    let ids: BTreeSet<&NodeId> = (trees.remote().iter().map(|(id, _)| id))
        .chain(trees.synced().iter().map(|(id, _)| id))
        .chain(trees.local().iter().map(|(id, _)| id))
        .collect();
    regroup(trees, ids.into_iter().cloned())
}

/// The groups that hold each of `ids` now.
pub(crate) fn regroup(
    trees: &Trees,
    ids: impl IntoIterator<Item = NodeId>,
) -> BTreeMap<RecoveryGroup, BTreeSet<NodeId>> {
    let mut groups: BTreeMap<RecoveryGroup, BTreeSet<NodeId>> = BTreeMap::new();
    for id in ids {
        if let Some(group) = group(trees, &id) {
            groups.entry(group).or_default().insert(id);
        }
    }
    groups
}

fn parent<'a>(trees: &'a Trees, id: &str) -> Option<&'a str> {
    let shape = (trees.local().get(id).map(|node| &node.shape))
        .or_else(|| trees.synced().live(id))
        .or_else(|| trees.remote().get(id).map(|node| &node.shape))?;
    Some(&shape.parent)
}

/// The question that holds `nodes` for `group`, each item under its topmost node, or none
/// when `nodes` is empty. The question `asked` is kept, with its answer, when it holds the
/// same nodes at the same revisions and shapes, so an answer acts on what the person saw.
pub(crate) fn ask(
    trees: &Trees,
    group: RecoveryGroup,
    nodes: &BTreeSet<NodeId>,
    asked: Option<&Question>,
    cause: Option<&RecoveryCause>,
    ids: &mut IdGen,
    now_ms: u64,
) -> Option<Question> {
    let revisions = (nodes.iter()).map(|id| {
        let revisions = (trees.remote().get(id)).map(|remote| Revisions {
            content_rev: remote.content_rev,
            meta_rev: remote.meta_rev,
        });
        (id.clone(), revisions)
    });
    let held = items(trees, revisions.collect());
    if held.is_empty() {
        return None;
    }
    let shown: BTreeMap<NodeId, Shape> = (nodes.iter())
        .filter_map(|id| Some((id.clone(), trees.local().get(id)?.shape.clone())))
        .collect();
    let kind = QuestionKind::Recovery {
        group,
        sent_in: None,
        cause: cause.cloned(),
    };
    if let Some(asked) =
        asked.filter(|asked| asked.kind == kind && asked.held == held && asked.shown == shown)
    {
        return Some(asked.clone());
    }
    Some(Question {
        id: ids.uuid_v7(now_ms),
        asked_at_ms: now_ms,
        kind,
        held,
        shown,
        answer: None,
    })
}

/// The items that hold `members`, each under its topmost member, with the revisions R held each
/// member at when it was asked.
fn items(trees: &Trees, members: BTreeMap<NodeId, Option<Revisions>>) -> BTreeMap<NodeId, Held> {
    let mut held: BTreeMap<NodeId, Held> = BTreeMap::new();
    let top = |id: &str| {
        let mut top = id;
        while let Some(above) = parent(trees, top).filter(|above| members.contains_key(*above)) {
            top = above;
        }
        top.to_owned()
    };
    for (id, revisions) in &members {
        let item = held.entry(top(id)).or_insert(Held {
            members: BTreeMap::new(),
            files: 0,
        });
        item.members.insert(id.clone(), *revisions);
        let kind = (trees.local().get(id).map(|node| node.shape.kind))
            .or_else(|| trees.synced().live(id).map(|shape| shape.kind));
        if kind == Some(Kind::File) {
            item.files += 1;
        }
    }
    held
}

/// The answered question `question` cut down to the members `kept` keeps, with the revisions
/// and shapes the person answered, or none when it keeps none. Its answer stays bound to them
/// until a commit carries them out.
pub(crate) fn narrow(
    trees: &Trees,
    mut question: Question,
    kept: impl Fn(&str) -> bool,
) -> Option<Question> {
    let members = (question.held.values())
        .flat_map(|item| item.members.iter())
        .filter(|(id, _)| kept(id))
        .map(|(id, revisions)| (id.clone(), *revisions))
        .collect();
    question.held = items(trees, members);
    if question.held.is_empty() {
        return None;
    }
    question.shown.retain(|id, _| kept(id));
    if let QuestionKind::Recovery { sent_in, .. } = &mut question.kind {
        *sent_in = None;
    }
    Some(question)
}

/// The question about the restores `restores` a person asked for before the reset, each an item
/// of its own, or none when there is none.
pub(crate) fn ask_intents<'a>(
    restores: impl IntoIterator<Item = &'a String>,
    cause: Option<&RecoveryCause>,
    ids: &mut IdGen,
    now_ms: u64,
) -> Option<Question> {
    let held: BTreeMap<NodeId, Held> = (restores.into_iter())
        .map(|id| {
            let members = BTreeMap::from([(id.clone(), None)]);
            (id.clone(), Held { members, files: 0 })
        })
        .collect();
    if held.is_empty() {
        return None;
    }
    Some(Question {
        id: ids.uuid_v7(now_ms),
        asked_at_ms: now_ms,
        kind: QuestionKind::Recovery {
            group: RecoveryGroup::RequestedBeforeReset,
            sent_in: None,
            cause: cause.cloned(),
        },
        held,
        shown: BTreeMap::new(),
        answer: None,
    })
}

/// Whether R shows a trash restore done: the node is live, at the position it asked for if any.
/// A version restore is never shown done, since R does not say which version a file holds.
pub(crate) fn done(trees: &Trees, intent: &RestoreIntent) -> bool {
    let Some(live) = trees.remote().live(intent.node()) else {
        return false;
    };
    intent.version_id().is_none()
        && intent.parent().is_none_or(|parent| parent == live.parent)
        && intent.name().is_none_or(|name| name == live.name)
}

/// The S updates that carry out `answer` for the nodes `question` holds, so the planner
/// does the rest: a node published or kept is committed against R, under its own ID when R does
/// not hold it, a node discarded follows R with its local version going to the local trash, a
/// deletion is committed through the mass-delete guard, and a restored one is downloaded.
pub(crate) fn release(
    trees: &Trees,
    question: &Question,
    answer: Answer,
) -> Vec<(NodeId, Option<Shape>)> {
    let QuestionKind::Recovery { group, .. } = question.kind else {
        return Vec::new();
    };
    let remote = |id: &str| trees.remote().get(id).map(|node| node.shape.clone());
    let local = |id: &str| trees.local().get(id).map(|node| node.shape.clone());
    (question.holds())
        .filter_map(|id| {
            let shape = match (group, answer) {
                (RecoveryGroup::CreatedAfterRestorePoint, Answer::Keep) => None,
                (RecoveryGroup::ChangedOnThisDevice, Answer::Publish) => remote(id),
                (RecoveryGroup::DeletedOnThisDevice, Answer::Delete) => remote(id),
                (RecoveryGroup::DeletedOnThisDevice, Answer::Restore) => None,
                (_, Answer::Discard) => local(id),
                _ => return None,
            };
            Some((id.clone(), shape))
        })
        .collect()
}
