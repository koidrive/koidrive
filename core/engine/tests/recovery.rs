// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;

use koidrive_engine::classify::{Class, Origin, Trees, Waiting};
use koidrive_engine::commit::{Base, Commits, Failure, Halt, Reconnect};
use koidrive_engine::effects::{BatchLimit, Effect, EffectRecord, Effects};
use koidrive_engine::first_link::{History, Marks, PriorSync};
use koidrive_engine::ids::IdGen;
use koidrive_engine::ignore::RuleFile;
use koidrive_engine::names::{nfc, same_name};
use koidrive_engine::question::{Answer, Question, QuestionKind, RecoveryCause, RecoveryGroup};
use koidrive_engine::restore::{Outcome, Rejection, RestoreIntent};
use koidrive_engine::tree::{Content, FileIdentity, Kind, LocalNode, RemoteNode, Shape};
use koidrive_proto::v1::{CommitResult, Error, OpResult, Welcome, error, op, op_result};

const ROOT: &str = "root";
const NOW: u64 = 1_794_268_740_000;
const OLD: &str = "old-history";
const NEW: &str = "new-history";
const NODE: &str = "0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b";

fn content(digest: u8) -> Content {
    Content {
        size: 3,
        manifest_digest: vec![digest; 32],
        ..Default::default()
    }
}

fn file(parent: &str, name: &str, digest: u8) -> Shape {
    Shape {
        parent: parent.into(),
        name: name.into(),
        kind: Kind::File,
        content: Some(content(digest)),
    }
}

fn dir(parent: &str, name: &str) -> Shape {
    Shape {
        parent: parent.into(),
        name: name.into(),
        kind: Kind::Dir,
        content: None,
    }
}

struct Remote<'a>(&'a str, Shape, bool, u64);

fn trees(local: &[(&str, Shape)], synced: &[(&str, Shape)], remote: &[Remote]) -> Trees {
    let mut trees = Trees::new(ROOT);
    for (ino, (id, shape)) in local.iter().enumerate() {
        let node = LocalNode {
            left_out_when_placed: false,
            content_from_server: false,
            shape: shape.clone(),
            identity: FileIdentity {
                ino: ino as u128 + 1,
                birth_ns: Some(0),
            },
            parked: false,
        };
        trees.set_local((*id).into(), node);
    }
    for (id, shape) in synced {
        trees.set_synced((*id).into(), Some(shape.clone()));
    }
    for Remote(id, shape, trashed, rev) in remote {
        let node = RemoteNode {
            made_over: Vec::new(),
            shape: shape.clone(),
            trashed: *trashed,
            content_rev: *rev,
            edit_rev: *rev,
            meta_rev: *rev,
            meta_edit_rev: *rev,
            trash_group: String::new(),
            version_id: String::new(),
            pending_for: None,
            pending_by: None,
        };
        trees.set_remote((*id).into(), node);
    }
    trees.rules_scanned();
    trees.settle();
    trees
}

fn live<'a>(id: &'a str, shape: Shape) -> Remote<'a> {
    Remote(id, shape, false, 1)
}

fn mark(trees: &mut Trees, id: &str, stem: &str) {
    let mut node = trees.remote().get(id).expect("a node of R").clone();
    node.pending_for = Some(stem.into());
    trees.set_remote(id.into(), node);
    trees.settle();
}

fn rewind() -> RecoveryCause {
    RecoveryCause::rewind(4, NOW as i64)
}

fn recovered(trees: &mut Trees, device_seq: u64) -> (Commits, IdGen) {
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    commits.enter_recovery(rewind());
    assert_eq!(commits.build(trees, &mut ids, NOW, OLD, "me", 9), None);
    commits.recover(
        trees,
        device_seq,
        &rewind(),
        &History::new(),
        &Marks::new(),
        &mut ids,
        NOW,
    );
    trees.settle();
    (commits, ids)
}

fn ops(commits: &mut Commits, trees: &mut Trees, ids: &mut IdGen) -> Vec<(&'static str, String)> {
    let Some(commit) = commits.build(trees, ids, NOW, NEW, "me", 1) else {
        return Vec::new();
    };
    (commit.ops.iter())
        .map(|op| match &op.op {
            Some(op::Op::CreateFile(create)) => ("create file", create.node_id.clone()),
            Some(op::Op::CreateDir(create)) => ("create dir", create.node_id.clone()),
            Some(op::Op::Move(moved)) => ("move", moved.node_id.clone()),
            Some(op::Op::UpdateContent(update)) => ("update", update.node_id.clone()),
            Some(op::Op::Delete(delete)) => ("delete", delete.node_id.clone()),
            Some(op::Op::Restore(restore)) => ("restore", restore.node_id.clone()),
            None => panic!("an empty op"),
        })
        .collect()
}

fn effects(trees: &mut Trees, ids: &mut IdGen) -> Vec<(String, Effect)> {
    let mut effects = Effects::default();
    let mut done = Vec::new();
    for ino in 100.. {
        trees.settle();
        let Some(record) = (effects.plan(trees, None, ids, NOW, BatchLimit::ONE).first()).cloned()
        else {
            break;
        };
        let identity = FileIdentity {
            ino,
            birth_ns: Some(0),
        };
        effects
            .done(trees, None, &record.effect_id, Some(identity))
            .unwrap();
        done.push((record.node, record.effect));
    }
    trees.settle();
    done
}

fn group(question: &Question) -> RecoveryGroup {
    match question.kind {
        QuestionKind::Recovery { group, .. } => group,
        ref other => panic!("not a recovery question: {other:?}"),
    }
}

fn asked(commits: &Commits, group: RecoveryGroup, held: &[&str]) -> Question {
    let question = (commits.questions().iter())
        .find(|question| self::group(question) == group)
        .unwrap_or_else(|| panic!("no {group:?} question: {:?}", commits.questions()));
    let mut holds: Vec<&str> = question.holds().map(String::as_str).collect();
    holds.sort();
    assert_eq!(holds, held, "{question:?}");
    question.clone()
}

fn groups(commits: &Commits) -> Vec<RecoveryGroup> {
    commits.questions().iter().map(group).collect()
}

#[test]
fn a_rename_after_the_restore_point_is_reverted_as_one_move_and_a_new_file_is_held() {
    let mut trees = trees(
        &[("n", file(ROOT, "b", 1)), ("new", file(ROOT, "new", 2))],
        &[("n", file(ROOT, "b", 1)), ("new", file(ROOT, "new", 2))],
        &[live("n", file(ROOT, "a", 1))],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    assert_eq!(groups(&commits), [RecoveryGroup::CreatedAfterRestorePoint]);
    asked(&commits, RecoveryGroup::CreatedAfterRestorePoint, &["new"]);
    assert!(trees.held("new") && !trees.held("n"));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let done = effects(&mut trees, &mut ids);
    assert!(
        matches!(&done[..], [(node, Effect::Move { .. })] if node == "n"),
        "{done:?}"
    );
    assert_eq!(trees.local().get("n").unwrap().shape.name, "a");
    assert!(trees.local().get("new").is_some());
}

#[test]
fn a_node_the_device_and_the_server_agree_on_is_certain() {
    let mut trees = trees(
        &[("n", file(ROOT, "a", 2))],
        &[("n", file(ROOT, "a", 1))],
        &[live("n", file(ROOT, "a", 2))],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    assert!(commits.questions().is_empty(), "{:?}", commits.questions());
    assert_eq!(trees.class("n"), Class::InSync);
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
}

#[test]
fn a_synced_node_the_rewind_trashed_goes_to_the_local_trash() {
    let mut trees = trees(
        &[("n", file(ROOT, "a", 1))],
        &[("n", file(ROOT, "a", 1))],
        &[Remote("n", file(ROOT, "a", 1), true, 1)],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    assert!(commits.questions().is_empty(), "{:?}", commits.questions());
    let done = effects(&mut trees, &mut ids);
    assert!(
        matches!(&done[..], [(node, Effect::RemoveFile { .. })] if node == "n"),
        "{done:?}"
    );
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
}

#[test]
fn a_node_changed_on_this_device_is_held_until_it_is_published() {
    let mut trees = trees(
        &[("n", file(ROOT, "a", 2))],
        &[("n", file(ROOT, "a", 1))],
        &[live("n", file(ROOT, "a", 3))],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    let question = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["n"]);
    assert!(trees.held("n"));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    assert_eq!(effects(&mut trees, &mut ids), []);
    assert!(commits.decide(&question.id, Answer::Publish));
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("update", "n".into())]
    );
    let flight = commits.in_flight().unwrap();
    assert!(flight.commit.require_head);
    assert_eq!(flight.commit.history_id, NEW);
    assert!(!trees.held("n"));
}

#[test]
fn an_answer_whose_commit_finds_the_head_moved_is_asked_again() {
    let mut trees = trees(
        &[("n", file(ROOT, "a", 2))],
        &[("n", file(ROOT, "a", 1))],
        &[live("n", file(ROOT, "a", 3))],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    let question = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["n"]);
    assert!(commits.decide(&question.id, Answer::Publish));
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("update", "n".into())]
    );
    let moved = Error {
        code: error::Code::HeadMoved as i32,
        ..Default::default()
    };
    assert_eq!(commits.failed(&mut trees, &moved), Ok(Failure::Abandoned));
    trees.set_remote(
        "n".into(),
        RemoteNode {
            made_over: Vec::new(),
            shape: file(ROOT, "a", 5),
            trashed: false,
            content_rev: 2,
            edit_rev: 2,
            meta_rev: 2,
            meta_edit_rev: 2,
            trash_group: String::new(),
            version_id: String::new(),
            pending_for: None,
            pending_by: None,
        },
    );
    trees.settle();
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let again = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["n"]);
    assert_ne!(again.id, question.id);
    assert_eq!(again.answer, None);
    assert!(trees.held("n"));
    assert!(commits.decide(&again.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let done = effects(&mut trees, &mut ids);
    assert!(
        matches!(&done[..], [(node, Effect::WriteFile { .. })] if node == "n"),
        "{done:?}"
    );
    assert_eq!(
        trees.local().get("n").unwrap().shape.content,
        Some(content(5))
    );
}

#[test]
fn a_question_asked_again_names_the_cause_of_its_recovery() {
    let mut trees = trees(
        &[("n", file(ROOT, "a", 2))],
        &[("n", file(ROOT, "a", 1))],
        &[live("n", file(ROOT, "a", 3))],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    let question = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["n"]);
    assert_eq!(question.cause(), Some(&rewind()));
    assert!(commits.decide(&question.id, Answer::Publish));
    ops(&mut commits, &mut trees, &mut ids);
    let moved = Error {
        code: error::Code::HeadMoved as i32,
        ..Default::default()
    };
    assert_eq!(commits.failed(&mut trees, &moved), Ok(Failure::Abandoned));
    trees.set_remote(
        "n".into(),
        RemoteNode {
            made_over: Vec::new(),
            shape: file(ROOT, "a", 5),
            trashed: false,
            content_rev: 2,
            edit_rev: 2,
            meta_rev: 2,
            meta_edit_rev: 2,
            trash_group: String::new(),
            version_id: String::new(),
            pending_for: None,
            pending_by: None,
        },
    );
    trees.settle();
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let again = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["n"]);
    assert_ne!(again.id, question.id);
    assert_eq!(again.cause(), Some(&rewind()));
}

#[test]
fn discarding_a_changed_node_downloads_the_server_version() {
    let mut trees = trees(
        &[("n", file(ROOT, "a", 2))],
        &[("n", file(ROOT, "a", 1))],
        &[live("n", file(ROOT, "a", 3))],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    let question = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["n"]);
    assert!(commits.decide(&question.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let done = effects(&mut trees, &mut ids);
    assert!(
        matches!(&done[..], [(node, Effect::WriteFile { .. })] if node == "n"),
        "{done:?}"
    );
    assert_eq!(
        trees.local().get("n").unwrap().shape.content,
        Some(content(3))
    );
}

#[test]
fn keeping_a_file_the_rewind_removed_creates_it_under_its_own_id() {
    let mut trees = trees(
        &[("new", file(ROOT, "new", 2))],
        &[("new", file(ROOT, "new", 2))],
        &[],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    let question = asked(&commits, RecoveryGroup::CreatedAfterRestorePoint, &["new"]);
    assert!(commits.decide(&question.id, Answer::Keep));
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("create file", "new".into())]
    );
}

fn a_folder_the_rewind_removed(
    folder_on_disk: Shape,
    child: Remote,
    child_on_disk: Shape,
) -> (Trees, Commits, IdGen) {
    let moved_in = file("a", "A", 1);
    let mut trees = trees(
        &[("a", folder_on_disk), ("f", moved_in.clone())],
        &[("a", dir(ROOT, "A")), ("f", moved_in)],
        &[child],
    );
    let (commits, ids) = recovered(&mut trees, 0);
    let mut renamed = trees.local().get("f").unwrap().clone();
    renamed.shape = child_on_disk;
    trees.set_local("f".into(), renamed);
    trees.settle();
    (trees, commits, ids)
}

#[test]
fn keeping_a_folder_sends_a_file_renamed_in_it_that_the_rewind_moved_out() {
    let (mut trees, mut commits, mut ids) = a_folder_the_rewind_removed(
        dir(ROOT, "A"),
        live("f", file(ROOT, "b", 1)),
        file("a", "c.txt", 1),
    );
    let question = asked(&commits, RecoveryGroup::CreatedAfterRestorePoint, &["a"]);
    assert!(commits.decide(&question.id, Answer::Keep));
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("create dir", "a".into()), ("move", "f".into())]
    );
}

#[test]
fn keeping_a_folder_sends_a_file_renamed_in_it_that_the_rewind_trashed() {
    let (mut trees, mut commits, mut ids) = a_folder_the_rewind_removed(
        dir(ROOT, "A"),
        Remote("f", file(ROOT, "b", 1), true, 1),
        file("a", "c.txt", 1),
    );
    let question = asked(&commits, RecoveryGroup::CreatedAfterRestorePoint, &["a"]);
    assert!(commits.decide(&question.id, Answer::Keep));
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("create dir", "a".into()), ("move", "f".into())]
    );
}

#[test]
fn publishing_a_folder_the_rewind_removed_sends_a_file_renamed_in_it() {
    let (mut trees, mut commits, mut ids) = a_folder_the_rewind_removed(
        dir(ROOT, "A2"),
        live("f", file(ROOT, "b", 1)),
        file("a", "c.txt", 1),
    );
    let question = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["a"]);
    assert!(commits.decide(&question.id, Answer::Publish));
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("create dir", "a".into()), ("move", "f".into())]
    );
}

#[test]
fn keeping_a_folder_sends_an_edit_in_it_and_still_moves_the_file_where_the_rewind_put_it() {
    let (mut trees, mut commits, mut ids) = a_folder_the_rewind_removed(
        dir(ROOT, "A"),
        live("f", file(ROOT, "b", 1)),
        file("a", "A", 2),
    );
    let question = asked(&commits, RecoveryGroup::CreatedAfterRestorePoint, &["a"]);
    assert!(commits.decide(&question.id, Answer::Keep));
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("update", "f".into())]
    );
    let applied = OpResult {
        code: op_result::Code::Applied as i32,
        ..Default::default()
    };
    let result = CommitResult {
        seq: 2,
        results: vec![applied],
        ..Default::default()
    };
    commits.received(result).unwrap();
    trees.set_remote(
        "f".into(),
        RemoteNode {
            made_over: Vec::new(),
            shape: file(ROOT, "b", 2),
            trashed: false,
            content_rev: 2,
            edit_rev: 2,
            meta_rev: 1,
            meta_edit_rev: 1,
            trash_group: String::new(),
            version_id: String::new(),
            pending_for: None,
            pending_by: None,
        },
    );
    assert!(commits.acknowledge(&mut trees, 2));
    let done = effects(&mut trees, &mut ids);
    assert!(
        matches!(&done[..], [(node, Effect::Move { .. })] if node == "f"),
        "{done:?}"
    );
    assert_eq!(trees.local().get("f").unwrap().shape, file(ROOT, "b", 2));
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("create dir", "a".into())]
    );
}

#[test]
fn keeping_a_folder_still_moves_out_an_unchanged_file_the_rewind_moved_out() {
    let (mut trees, mut commits, mut ids) = a_folder_the_rewind_removed(
        dir(ROOT, "A"),
        live("f", file(ROOT, "b", 1)),
        file("a", "A", 1),
    );
    let question = asked(&commits, RecoveryGroup::CreatedAfterRestorePoint, &["a"]);
    assert!(commits.decide(&question.id, Answer::Keep));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let done = effects(&mut trees, &mut ids);
    assert!(
        matches!(&done[..], [(node, Effect::Move { .. })] if node == "f"),
        "{done:?}"
    );
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("create dir", "a".into())]
    );
}

#[test]
fn keeping_a_folder_leaves_a_child_another_question_holds_held() {
    let mut trees = trees(
        &[("a", dir(ROOT, "A")), ("f", file(ROOT, "b", 1))],
        &[("a", dir(ROOT, "A")), ("f", file("a", "x", 1))],
        &[live("f", file(ROOT, "b", 2))],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    let parent = asked(&commits, RecoveryGroup::CreatedAfterRestorePoint, &["a"]);
    assert!(commits.decide(&parent.id, Answer::Keep));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    assert!(trees.held("f"));
    let child = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["f"]);
    assert!(commits.decide(&child.id, Answer::Publish));
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("create dir", "a".into()), ("update", "f".into())]
    );
}

#[test]
fn discarding_a_file_moved_out_of_a_folder_restored_in_the_same_pass_sends_nothing() {
    let mut trees = trees(
        &[("c", file(ROOT, "x", 1))],
        &[("d", dir(ROOT, "D")), ("c", file("d", "c", 1))],
        &[live("d", dir(ROOT, "D")), live("c", file("d", "c", 1))],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    let deleted = asked(&commits, RecoveryGroup::DeletedOnThisDevice, &["d"]);
    let changed = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["c"]);
    assert!(commits.decide(&deleted.id, Answer::Restore));
    assert!(commits.decide(&changed.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
}

#[test]
fn publishing_a_file_moved_out_of_a_folder_restored_in_the_same_pass_sends_its_content() {
    let mut trees = trees(
        &[("c", file(ROOT, "x", 2))],
        &[("d", dir(ROOT, "D")), ("c", file("d", "c", 2))],
        &[live("d", dir(ROOT, "D")), live("c", file("d", "c", 1))],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    let deleted = asked(&commits, RecoveryGroup::DeletedOnThisDevice, &["d"]);
    let changed = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["c"]);
    assert!(commits.decide(&deleted.id, Answer::Restore));
    assert!(commits.decide(&changed.id, Answer::Publish));
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("move", "c".into()), ("update", "c".into())]
    );
}

#[test]
fn a_file_the_rewind_removed_that_the_server_holds_at_its_place_under_another_id_is_linked() {
    let mut written = file(ROOT, "new", 2);
    written.content.as_mut().expect("a file").mtime_ns = 1_794_268_740_123_456_789;
    let mut trees = trees(
        &[("new", written.clone())],
        &[("new", written.clone())],
        &[live("theirs", written)],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    assert!(commits.questions().is_empty(), "{:?}", commits.questions());
    assert!(trees.local().get("new").is_none() && trees.synced().get("new").is_none());
    assert_eq!(trees.class("theirs"), Class::InSync);
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    assert_eq!(effects(&mut trees, &mut ids), []);
}

#[test]
fn a_file_the_rewind_removed_beside_other_bytes_at_its_place_is_asked_about() {
    let mut trees = trees(
        &[("new", file(ROOT, "new", 2))],
        &[("new", file(ROOT, "new", 2))],
        &[live("theirs", file(ROOT, "new", 3))],
    );
    let (commits, _) = recovered(&mut trees, 0);
    asked(&commits, RecoveryGroup::CreatedAfterRestorePoint, &["new"]);
}

#[test]
fn discarding_a_file_the_rewind_removed_moves_it_to_the_local_trash() {
    let mut trees = trees(
        &[("new", file(ROOT, "new", 2))],
        &[("new", file(ROOT, "new", 2))],
        &[],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    let question = asked(&commits, RecoveryGroup::CreatedAfterRestorePoint, &["new"]);
    assert!(commits.decide(&question.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let done = effects(&mut trees, &mut ids);
    assert!(
        matches!(&done[..], [(node, Effect::RemoveFile { .. })] if node == "new"),
        "{done:?}"
    );
}

#[test]
fn a_node_deleted_on_this_device_is_held_until_the_delete_is_confirmed() {
    let synced = [("d", dir(ROOT, "d")), ("f", file("d", "f", 1))];
    for (answer, expected) in [
        (Answer::Delete, vec![("delete", "f".to_owned())]),
        (Answer::Restore, vec![]),
    ] {
        let mut trees = trees(
            &[("d", dir(ROOT, "d"))],
            &synced,
            &[live("d", dir(ROOT, "d")), live("f", file("d", "f", 1))],
        );
        let (mut commits, mut ids) = recovered(&mut trees, 0);
        let question = asked(&commits, RecoveryGroup::DeletedOnThisDevice, &["f"]);
        assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
        assert_eq!(effects(&mut trees, &mut ids), [], "{answer:?}");
        assert!(commits.decide(&question.id, answer));
        assert_eq!(ops(&mut commits, &mut trees, &mut ids), expected);
        if answer == Answer::Restore {
            let done = effects(&mut trees, &mut ids);
            assert!(
                matches!(&done[..], [(node, Effect::WriteFile { .. })] if node == "f"),
                "{done:?}"
            );
        }
    }
}

#[test]
fn an_entry_new_on_this_device_is_held_as_created_on_this_device() {
    let mut trees = trees(
        &[
            ("mine", dir(ROOT, "mine")),
            ("inside", file("mine", "inside", 4)),
        ],
        &[],
        &[live("theirs", file(ROOT, "theirs", 5))],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    let question = asked(
        &commits,
        RecoveryGroup::CreatedOnThisDevice,
        &["inside", "mine"],
    );
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    assert!(commits.decide(&question.id, Answer::Publish));
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [
            ("create dir", "mine".into()),
            ("create file", "inside".into())
        ]
    );
}

#[test]
fn an_entry_new_on_this_device_stays_held_when_another_device_sends_the_same_node() {
    let mut trees = trees(&[("n", file(ROOT, "n", 1))], &[], &[]);
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    asked(&commits, RecoveryGroup::CreatedOnThisDevice, &["n"]);
    trees.set_remote(
        "n".into(),
        RemoteNode {
            made_over: Vec::new(),
            shape: file(ROOT, "n", 2),
            trashed: false,
            content_rev: 1,
            edit_rev: 1,
            meta_rev: 1,
            meta_edit_rev: 1,
            trash_group: String::new(),
            version_id: String::new(),
            pending_for: None,
            pending_by: None,
        },
    );
    trees.settle();
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    assert_eq!(effects(&mut trees, &mut ids), []);
    let question = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["n"]);
    assert_eq!(question.cause(), Some(&rewind()));
}

#[test]
fn an_entry_like_the_servers_at_a_name_s_gives_another_node_is_held_and_s_stays_a_tree() {
    let mut trees = trees(
        &[("moved", file(ROOT, "A", 1))],
        &[("moved", file(ROOT, "A", 1))],
        &[live("moved", file(ROOT, "A", 1))],
    );
    let identity = trees.local().get("moved").unwrap().identity;
    let moved = LocalNode {
        left_out_when_placed: false,
        content_from_server: false,
        shape: file(ROOT, "c.txt", 1),
        identity,
        parked: false,
    };
    trees.set_local("moved".into(), moved);
    let mine = LocalNode {
        left_out_when_placed: false,
        content_from_server: false,
        shape: dir(ROOT, "a"),
        identity: FileIdentity { ino: 9, ..identity },
        parked: false,
    };
    trees.set_local("mine".into(), mine);
    for (id, shape) in [
        ("moved", file(ROOT, "c.txt", 1)),
        ("theirs", dir(ROOT, "a")),
    ] {
        let node = RemoteNode {
            made_over: Vec::new(),
            shape,
            trashed: false,
            content_rev: 2,
            edit_rev: 2,
            meta_rev: 2,
            meta_edit_rev: 2,
            trash_group: String::new(),
            version_id: String::new(),
            pending_for: None,
            pending_by: None,
        };
        trees.set_remote(id.into(), node);
    }
    let mut commits = Commits::default();
    commits.enter_recovery(rewind());
    commits.recover(
        &mut trees,
        0,
        &rewind(),
        &History::new(),
        &Marks::new(),
        &mut IdGen::new(7),
        NOW,
    );
    let mut synced = Trees::new(ROOT);
    for (id, shape) in trees.synced().iter() {
        synced.set_synced(id.clone(), Some(shape.clone()));
    }
    assert_eq!(synced.synced(), trees.synced(), "S is a tree");
    asked(&commits, RecoveryGroup::CreatedOnThisDevice, &["mine"]);
    assert_eq!(trees.synced().get("theirs"), None);
}

#[test]
fn a_held_node_another_device_publishes_stays_held_unless_it_ends_equal() {
    for (published, held) in [
        (file(ROOT, "a", 3), true),
        (file(ROOT, "elsewhere", 2), true),
        (file(ROOT, "a", 2), false),
    ] {
        let mut trees = trees(
            &[("n", file(ROOT, "a", 2))],
            &[("n", file(ROOT, "a", 1))],
            &[Remote("n", file(ROOT, "a", 1), true, 1)],
        );
        let (mut commits, mut ids) = recovered(&mut trees, 0);
        let before = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["n"]);
        trees.set_remote(
            "n".into(),
            RemoteNode {
                made_over: Vec::new(),
                shape: published.clone(),
                trashed: false,
                content_rev: 2,
                edit_rev: 2,
                meta_rev: 2,
                meta_edit_rev: 2,
                trash_group: String::new(),
                version_id: String::new(),
                pending_for: None,
                pending_by: None,
            },
        );
        trees.settle();
        let sent = ops(&mut commits, &mut trees, &mut ids);
        if held {
            assert_eq!(sent, [], "{published:?}");
            let after = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["n"]);
            assert_ne!(after.id, before.id, "{published:?}");
            assert!(trees.held("n"));
        } else {
            assert!(commits.questions().is_empty(), "{:?}", commits.questions());
            assert!(!trees.held("n"));
            assert_eq!(sent, [("update", "n".into())]);
        }
    }
}

#[test]
fn an_answer_to_a_node_that_changed_since_is_not_taken() {
    let mut trees = trees(
        &[("n", file(ROOT, "a", 2))],
        &[("n", file(ROOT, "a", 1))],
        &[live("n", file(ROOT, "a", 3))],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    let question = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["n"]);
    assert!(commits.decide(&question.id, Answer::Publish));
    let mut edited = trees.local().get("n").unwrap().clone();
    edited.shape.content = Some(content(4));
    trees.set_local("n".into(), edited);
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let again = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["n"]);
    assert_ne!(again.id, question.id);
    assert_eq!(again.answer, None);
}

#[test]
fn the_commit_in_flight_before_the_reset_is_never_sent_again() {
    let mut trees = trees(
        &[("n", file(ROOT, "a", 2))],
        &[("n", file(ROOT, "a", 1))],
        &[live("n", file(ROOT, "a", 1))],
    );
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    let sent = commits
        .build(&mut trees, &mut ids, NOW, OLD, "me", 1)
        .cloned();
    assert_eq!(sent.as_ref().map(|commit| commit.device_seq), Some(1));
    let welcome = Welcome {
        history_id: OLD.into(),
        device_seq: 4,
        ..Default::default()
    };
    assert_eq!(commits.connected(&welcome), Reconnect::Recovery);
    commits.recover(
        &mut trees,
        4,
        &rewind(),
        &History::new(),
        &Marks::new(),
        &mut ids,
        NOW,
    );
    trees.settle();
    assert_eq!(commits.in_flight(), None);
    assert_eq!(commits.accepted(), 4);
    let question = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["n"]);
    assert!(commits.decide(&question.id, Answer::Publish));
    let commit = commits.build(&mut trees, &mut ids, NOW, NEW, "me", Base::default());
    let commit = commit.expect("the published edit");
    assert_eq!(commit.device_seq, 5);
    assert_ne!(Some(&commit.commit_id), sent.as_ref().map(|c| &c.commit_id));
}

#[test]
fn a_result_that_arrives_after_the_recovery_starts_leaves_the_change_held() {
    let mut trees = trees(
        &[("n", file(ROOT, "a", 2))],
        &[("n", file(ROOT, "a", 1))],
        &[live("n", file(ROOT, "a", 1))],
    );
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    assert!(
        commits
            .build(&mut trees, &mut ids, NOW, OLD, "me", 1)
            .is_some()
    );
    commits.enter_recovery(rewind());
    let applied = OpResult {
        code: op_result::Code::Applied as i32,
        ..Default::default()
    };
    let result = CommitResult {
        seq: 2,
        results: vec![applied],
        ..Default::default()
    };
    commits.received(result).unwrap();
    commits.acknowledge(&mut trees, 2);
    commits.recover(
        &mut trees,
        0,
        &rewind(),
        &History::new(),
        &Marks::new(),
        &mut ids,
        NOW,
    );
    trees.settle();
    asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["n"]);
}

#[test]
fn a_restore_asked_for_before_the_reset_is_held_until_it_is_answered() {
    for answer in [Answer::Repeat, Answer::Cancel] {
        let mut trees = trees(
            &[(NODE, file(ROOT, "a", 1))],
            &[(NODE, file(ROOT, "a", 1))],
            &[live(NODE, file(ROOT, "a", 1))],
        );
        let mut commits = Commits::default();
        let mut ids = IdGen::new(7);
        let intent = RestoreIntent::version(NODE.into(), NODE.into()).unwrap();
        assert!(commits.restore("request".into(), intent));
        let sent = ops(&mut commits, &mut trees, &mut ids);
        assert_eq!(sent, [("restore", NODE.to_owned())]);
        commits.enter_recovery(rewind());
        commits.recover(
            &mut trees,
            0,
            &rewind(),
            &History::new(),
            &Marks::new(),
            &mut ids,
            NOW,
        );
        trees.settle();
        let question = asked(&commits, RecoveryGroup::RequestedBeforeReset, &["request"]);
        assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
        assert_eq!(
            commits.restores().len(),
            1,
            "the request is kept until answered"
        );
        assert!(commits.decide(&question.id, answer));
        let ops = ops(&mut commits, &mut trees, &mut ids);
        if answer == Answer::Repeat {
            assert_eq!(ops, [("restore", NODE.to_owned())]);
        } else {
            assert_eq!(ops, []);
            let [ended] = commits.resolved() else {
                panic!("{:?}", commits.resolved());
            };
            assert_eq!(ended.outcome, Outcome::Rejected(Rejection::Cancelled));
            assert!(commits.questions().is_empty());
        }
    }
}

#[test]
fn a_restore_asked_for_again_after_the_reset_waits_for_the_answer_about_the_first() {
    let mut trees = trees(
        &[(NODE, file(ROOT, "a", 1))],
        &[(NODE, file(ROOT, "a", 1))],
        &[live(NODE, file(ROOT, "a", 1))],
    );
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    let intent = RestoreIntent::version(NODE.into(), NODE.into()).unwrap();
    assert!(commits.restore("before".into(), intent.clone()));
    commits.enter_recovery(rewind());
    commits.recover(
        &mut trees,
        0,
        &rewind(),
        &History::new(),
        &Marks::new(),
        &mut ids,
        NOW,
    );
    trees.settle();
    let question = asked(&commits, RecoveryGroup::RequestedBeforeReset, &["before"]);
    assert!(commits.restore("later".into(), intent));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    assert!(commits.decide(&question.id, Answer::Repeat));
    let sent = ops(&mut commits, &mut trees, &mut ids);
    assert_eq!(
        sent,
        [("restore", NODE.to_owned()), ("restore", NODE.to_owned())]
    );
    let applied = OpResult {
        code: op_result::Code::Applied as i32,
        ..Default::default()
    };
    let result = CommitResult {
        seq: 2,
        results: vec![applied; sent.len()],
        ..Default::default()
    };
    commits.received(result).unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    assert!(commits.restores().is_empty());
    let ended: Vec<(&str, &Outcome)> = (commits.resolved().iter())
        .map(|ended| (ended.id.as_str(), &ended.outcome))
        .collect();
    assert_eq!(
        ended,
        [
            ("before", &Outcome::Restored),
            ("later", &Outcome::Restored)
        ]
    );
}

#[test]
fn a_restore_the_rewound_tree_already_shows_is_done() {
    let mut trees = trees(&[], &[], &[live(NODE, file(ROOT, "a", 1))]);
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    let intent = RestoreIntent::from_trash(NODE.into(), None, None).unwrap();
    assert!(commits.restore("request".into(), intent));
    commits.enter_recovery(rewind());
    commits.recover(
        &mut trees,
        0,
        &rewind(),
        &History::new(),
        &Marks::new(),
        &mut ids,
        NOW,
    );
    assert!(commits.questions().is_empty(), "{:?}", commits.questions());
    let [ended] = commits.resolved() else {
        panic!("{:?}", commits.resolved());
    };
    assert_eq!(ended.outcome, Outcome::Restored);
}

#[test]
fn a_folder_the_server_renamed_before_the_rewind_is_created_again_where_l_holds_it() {
    let mut trees = trees(
        &[("d", dir(ROOT, "a")), ("c", file("d", "c", 1))],
        &[("d", dir(ROOT, "a (conflict)"))],
        &[],
    );
    trees.revert("d".into());
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    for question in commits.questions().to_vec() {
        assert!(commits.decide(&question.id, question.kind.answers()[0]));
    }
    let mut sent = ops(&mut commits, &mut trees, &mut ids);
    sent.sort();
    assert_eq!(
        sent,
        [("create dir", "d".into()), ("create file", "c".into())]
    );
}

#[test]
fn a_reverted_folder_published_over_the_servers_name_is_moved_to_the_name_l_holds() {
    let mut trees = trees(
        &[("d", dir(ROOT, "a")), ("c", file("d", "c", 1))],
        &[("d", dir(ROOT, "a (conflict)"))],
        &[live("d", dir(ROOT, "old"))],
    );
    trees.revert("d".into());
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    let changed = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["d"]);
    assert!(commits.decide(&changed.id, Answer::Publish));
    for question in commits.questions().to_vec() {
        if question.answer.is_none() {
            assert!(commits.decide(&question.id, question.kind.answers()[0]));
        }
    }
    let mut sent = ops(&mut commits, &mut trees, &mut ids);
    sent.sort();
    assert_eq!(sent, [("create file", "c".into()), ("move", "d".into())]);
}

#[test]
fn an_open_recovery_question_keeps_s_for_what_it_holds() {
    let mut trees = trees(
        &[("d", dir(ROOT, "a")), ("f", file("d", "f", 1))],
        &[("d", dir(ROOT, "a")), ("f", file(ROOT, "f", 1))],
        &[],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    trees.settle();
    assert_eq!(trees.synced().live("d"), Some(&dir(ROOT, "a")));
    assert_eq!(trees.synced().live("f"), Some(&file(ROOT, "f", 1)));
    asked(&commits, RecoveryGroup::CreatedAfterRestorePoint, &["d"]);
    asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["f"]);
}

#[test]
fn a_discarded_file_in_a_kept_folder_the_server_merged_stays_discarded() {
    let mut trees = trees(
        &[("d", dir(ROOT, "a")), ("f", file("d", "f", 1))],
        &[("d", dir(ROOT, "a")), ("f", file(ROOT, "f", 1))],
        &[],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    let kept = asked(&commits, RecoveryGroup::CreatedAfterRestorePoint, &["d"]);
    let discarded = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["f"]);
    assert!(commits.decide(&kept.id, Answer::Keep));
    assert!(commits.decide(&discarded.id, Answer::Discard));
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("create dir", "d".into())]
    );
    let merged = OpResult {
        node_id: "other".into(),
        code: op_result::Code::MergedInto as i32,
        ..Default::default()
    };
    let result = CommitResult {
        seq: 1,
        results: vec![merged],
        ..Default::default()
    };
    commits.received(result).unwrap();
    assert!(commits.acknowledge(&mut trees, 1));
    trees.set_remote(
        "other".into(),
        RemoteNode {
            made_over: Vec::new(),
            shape: dir(ROOT, "a"),
            trashed: false,
            content_rev: 1,
            edit_rev: 1,
            meta_rev: 1,
            meta_edit_rev: 1,
            trash_group: String::new(),
            version_id: String::new(),
            pending_for: None,
            pending_by: None,
        },
    );
    trees.settle();
    assert!(!trees.reclaimed("f"));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
}

#[test]
fn a_discarded_file_in_a_kept_folder_the_server_merged_is_not_moved_back_over_another_devices_rename()
 {
    let mut trees = trees(
        &[("d", dir(ROOT, "a")), ("f", file("d", "f", 1))],
        &[("d", dir(ROOT, "a")), ("f", file(ROOT, "f", 1))],
        &[live("f", file(ROOT, "g", 1))],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    let kept = asked(&commits, RecoveryGroup::CreatedAfterRestorePoint, &["d"]);
    let discarded = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["f"]);
    assert!(commits.decide(&kept.id, Answer::Keep));
    assert!(commits.decide(&discarded.id, Answer::Discard));
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("create dir", "d".into())]
    );
    let merged = OpResult {
        node_id: "other".into(),
        code: op_result::Code::MergedInto as i32,
        ..Default::default()
    };
    let result = CommitResult {
        seq: 1,
        results: vec![merged],
        ..Default::default()
    };
    commits.received(result).unwrap();
    assert!(commits.acknowledge(&mut trees, 1));
    trees.set_remote(
        "other".into(),
        RemoteNode {
            made_over: Vec::new(),
            shape: dir(ROOT, "a"),
            trashed: false,
            content_rev: 1,
            edit_rev: 1,
            meta_rev: 1,
            meta_edit_rev: 1,
            trash_group: String::new(),
            version_id: String::new(),
            pending_for: None,
            pending_by: None,
        },
    );
    trees.settle();
    assert!(!trees.reclaimed("f"));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
}

#[test]
fn a_folder_the_server_holds_under_its_pending_name_with_nothing_more_gets_its_name() {
    let mut trees = trees(
        &[("d", dir(ROOT, "x")), ("f", file("d", "kept.txt", 3))],
        &[],
        &[
            live("p", dir(ROOT, "x (pending)")),
            live("c", file("p", "kept.txt", 3)),
        ],
    );
    mark(&mut trees, "p", "x");
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    assert_eq!(commits.questions(), []);
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("move", "p".into())]
    );
}

#[test]
fn a_folder_a_user_named_like_a_pending_name_is_not_taken_for_this_devices_folder() {
    let mut trees = trees(
        &[("d", dir(ROOT, "x")), ("f", file("d", "kept.txt", 3))],
        &[],
        &[
            live("p", dir(ROOT, "x (pending)")),
            live("c", file("p", "kept.txt", 3)),
        ],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    asked(&commits, RecoveryGroup::CreatedOnThisDevice, &["d", "f"]);
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
}

#[test]
fn a_folder_the_server_holds_under_its_pending_name_with_more_than_this_device_is_not_adopted() {
    let mut trees = trees(
        &[("d", dir(ROOT, "x"))],
        &[],
        &[
            live("p", dir(ROOT, "x (pending)")),
            live("c", file("p", "other-device.txt", 3)),
        ],
    );
    mark(&mut trees, "p", "x");
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    asked(&commits, RecoveryGroup::CreatedOnThisDevice, &["d"]);
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
}

#[test]
fn a_discarded_file_the_user_moves_before_it_leaves_the_disk_is_asked_about_again() {
    let mut trees = trees(
        &[("f", file(ROOT, "c.txt", 1)), ("d", dir(ROOT, "C.TXT"))],
        &[],
        &[],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    let question = asked(&commits, RecoveryGroup::CreatedOnThisDevice, &["d", "f"]);
    assert!(commits.decide(&question.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let mut moved = trees.local().get("f").unwrap().clone();
    moved.shape = file("d", "a", 1);
    trees.set_local("f".into(), moved);
    trees.settle();
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let again = asked(&commits, RecoveryGroup::CreatedOnThisDevice, &["d", "f"]);
    assert_eq!(again.answer, None);
    assert_eq!(again.shown.get("f"), Some(&file("d", "a", 1)));
    assert_eq!(effects(&mut trees, &mut ids), []);
}

#[test]
fn discarding_an_edit_of_a_file_the_rewind_trashed_sends_nothing() {
    let mut trees = trees(
        &[("n", file(ROOT, "a", 2))],
        &[("n", file(ROOT, "a", 1))],
        &[Remote("n", file(ROOT, "a", 1), true, 1)],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    let question = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["n"]);
    assert!(commits.decide(&question.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let done = effects(&mut trees, &mut ids);
    assert!(
        matches!(&done[..], [(node, Effect::RemoveFile { .. })] if node == "n"),
        "{done:?}"
    );
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
}

#[test]
fn an_edit_of_a_trashed_file_whose_sent_revival_the_rewind_took_is_held_and_not_sent() {
    let mut trees = trees(
        &[("n", file(ROOT, "a", 2))],
        &[("n", file(ROOT, "a", 1))],
        &[Remote("n", file(ROOT, "a", 1), true, 1)],
    );
    assert!(trees.unsent_edits().contains("n"));
    trees.set_synced("n".into(), Some(file(ROOT, "a", 2)));
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    let question = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["n"]);
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    assert!(commits.decide(&question.id, Answer::Publish));
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("update", "n".to_owned())]
    );
}

#[test]
fn a_name_the_disk_and_the_server_agree_on_at_recovery_is_not_sent_after_the_server_moves_on() {
    let mut trees = trees(
        &[("n", file(ROOT, "a", 2))],
        &[("n", file(ROOT, "ss", 2)), ("m", file(ROOT, "A", 4))],
        &[live("n", file(ROOT, "a", 2)), live("m", file(ROOT, "b", 4))],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    asked(&commits, RecoveryGroup::DeletedOnThisDevice, &["m"]);
    assert!(!trees.held("n"));
    let renamed = RemoteNode {
        made_over: Vec::new(),
        shape: file(ROOT, "A", 2),
        trashed: false,
        content_rev: 1,
        edit_rev: 1,
        meta_rev: 2,
        meta_edit_rev: 2,
        trash_group: String::new(),
        version_id: String::new(),
        pending_for: None,
        pending_by: None,
    };
    trees.set_remote("n".into(), renamed);
    trees.settle();
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
}

#[test]
fn a_discard_answer_does_not_send_another_discarded_file_the_user_changed() {
    let mut trees = trees(
        &[("f", file(ROOT, "f", 1)), ("g", file(ROOT, "g", 2))],
        &[],
        &[live("r", file(ROOT, "remote", 9))],
    );
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    let first = asked(&commits, RecoveryGroup::CreatedOnThisDevice, &["f", "g"]);
    assert!(commits.decide(&first.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let mut moved = trees.local().get("f").unwrap().clone();
    moved.shape.name = "f-new".into();
    trees.set_local("f".into(), moved);
    trees.settle();
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let next = asked(&commits, RecoveryGroup::CreatedOnThisDevice, &["f"]);
    let mut moved = trees.local().get("g").unwrap().clone();
    moved.shape.name = "g-new".into();
    trees.set_local("g".into(), moved);
    trees.settle();
    assert!(commits.decide(&next.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    asked(&commits, RecoveryGroup::CreatedOnThisDevice, &["g"]);
}

#[test]
fn an_answer_to_a_later_first_link_question_is_taken_while_an_earlier_one_waits() {
    let mut trees = trees(
        &[("f", file(ROOT, "f", 1)), ("g", file(ROOT, "g", 2))],
        &[],
        &[live("r", file(ROOT, "remote", 9))],
    );
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    commits.first_link(&mut trees, &mut ids, NOW, PriorSync::Shown);
    let first = asked(&commits, RecoveryGroup::CreatedOnThisDevice, &["f", "g"]);
    assert!(commits.decide(&first.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    for (id, name) in [("f", "f-new"), ("g", "g-new")] {
        let mut moved = trees.local().get(id).unwrap().clone();
        moved.shape.name = name.into();
        trees.set_local(id.into(), moved);
        trees.settle();
        assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    }
    assert_eq!(commits.questions().len(), 2, "{:?}", commits.questions());
    let later = (commits.questions().iter())
        .find(|question| question.holds().any(|id| id == "g"))
        .unwrap()
        .clone();
    assert!(commits.decide(&later.id, Answer::Publish));
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("create file", "g".to_owned())]
    );
}

#[test]
fn a_rename_of_a_synced_folder_from_its_pending_name_is_held() {
    let mut trees = trees(
        &[("d", dir(ROOT, "work"))],
        &[("d", dir(ROOT, "work (pending)"))],
        &[live("d", dir(ROOT, "work (pending)"))],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["d"]);
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
}

#[test]
fn a_repeated_restore_is_sent_only_against_the_head_it_was_answered_at() {
    let mut trees = trees(
        &[(NODE, file(ROOT, "a", 1))],
        &[(NODE, file(ROOT, "a", 1))],
        &[live(NODE, file(ROOT, "a", 1))],
    );
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    let intent = RestoreIntent::version(NODE.into(), NODE.into()).unwrap();
    assert!(commits.restore("request".into(), intent));
    commits.enter_recovery(rewind());
    commits.recover(
        &mut trees,
        0,
        &rewind(),
        &History::new(),
        &Marks::new(),
        &mut ids,
        NOW,
    );
    let question = asked(&commits, RecoveryGroup::RequestedBeforeReset, &["request"]);
    assert!(commits.decide(&question.id, Answer::Repeat));
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("restore", NODE.to_owned())]
    );
    assert!(commits.in_flight().unwrap().commit.require_head);
    let moved = Error {
        code: error::Code::HeadMoved as i32,
        ..Default::default()
    };
    assert_eq!(commits.failed(&mut trees, &moved), Ok(Failure::Abandoned));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let again = asked(&commits, RecoveryGroup::RequestedBeforeReset, &["request"]);
    assert_eq!(again.answer, None);
    assert_eq!(commits.restores().len(), 1);
}

#[test]
fn publishing_a_folder_keeps_the_child_the_rewind_trashed_with_it_trashed() {
    let mut trees = trees(
        &[("d", dir(ROOT, "new"))],
        &[("d", dir(ROOT, "old")), ("f", file("d", "child", 1))],
        &[
            Remote("d", dir(ROOT, "old"), true, 1),
            Remote("f", file("d", "child", 1), true, 1),
        ],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    let question = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["d"]);
    assert!(commits.decide(&question.id, Answer::Publish));
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [
            ("move", "d".into()),
            ("move", "f".into()),
            ("delete", "f".into())
        ]
    );
    assert!(commits.in_flight().unwrap().commit.require_head);
}

#[test]
fn an_answer_and_what_trashes_again_its_revival_are_never_split() {
    let mut trees = trees(
        &[("d", dir(ROOT, "new"))],
        &[("d", dir(ROOT, "old")), ("f", file("d", "child", 1))],
        &[
            Remote("d", dir(ROOT, "old"), true, 1),
            Remote("f", file("d", "child", 1), true, 1),
        ],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    let question = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["d"]);
    assert!(commits.decide(&question.id, Answer::Publish));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids).len(), 3);
    let limit = Error {
        code: error::Code::LimitExceeded as i32,
        ..Default::default()
    };
    assert_eq!(commits.failed(&mut trees, &limit), Ok(Failure::Split));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let question = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["d"]);
    assert!(commits.decide(&question.id, Answer::Publish));
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [
            ("move", "d".into()),
            ("move", "f".into()),
            ("delete", "f".into())
        ]
    );
}

#[test]
fn a_child_an_answer_trashes_again_leaves_the_disk_and_is_not_sent_again() {
    let mut trees = trees(
        &[("d", dir(ROOT, "new")), ("f", file("d", "child", 1))],
        &[("d", dir(ROOT, "old")), ("f", file("d", "child", 1))],
        &[
            Remote("d", dir(ROOT, "old"), true, 1),
            Remote("f", file("d", "child", 1), true, 1),
        ],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    let question = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["d"]);
    assert!(commits.decide(&question.id, Answer::Publish));
    let sent = ops(&mut commits, &mut trees, &mut ids);
    assert_eq!(
        sent,
        [
            ("move", "d".into()),
            ("move", "f".into()),
            ("delete", "f".into())
        ]
    );
    for (id, shape, trashed) in [
        ("d", dir(ROOT, "new"), false),
        ("f", file("d", "child", 1), true),
    ] {
        let node = RemoteNode {
            made_over: Vec::new(),
            shape,
            trashed,
            content_rev: 1,
            edit_rev: 1,
            meta_rev: 2,
            meta_edit_rev: 2,
            trash_group: String::new(),
            version_id: String::new(),
            pending_for: None,
            pending_by: None,
        };
        trees.set_remote(id.into(), node);
    }
    let applied = OpResult {
        code: op_result::Code::Applied as i32,
        ..Default::default()
    };
    let result = CommitResult {
        seq: 2,
        results: vec![applied; sent.len()],
        ..Default::default()
    };
    commits.received(result).unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    trees.settle();
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let done = effects(&mut trees, &mut ids);
    assert!(
        matches!(&done[..], [(node, Effect::RemoveFile { .. })] if node == "f"),
        "{done:?}"
    );
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
}

#[test]
fn an_edit_sent_before_its_folder_is_published_is_not_trashed_again() {
    let mut trees = trees(
        &[("d", dir(ROOT, "new")), ("c", file("d", "child", 1))],
        &[("d", dir(ROOT, "old")), ("c", file("d", "child", 1))],
        &[
            Remote("d", dir(ROOT, "old"), true, 1),
            Remote("c", file("d", "child", 1), true, 1),
        ],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    let question = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["d"]);
    assert!(commits.decide(&question.id, Answer::Publish));
    let mut edited = trees.local().get("c").unwrap().clone();
    edited.shape = file("d", "child", 2);
    trees.set_local("c".into(), edited);
    trees.settle();
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("update", "c".into()), ("move", "d".into())]
    );
}

#[test]
fn a_restore_of_a_child_an_answer_trashes_again_waits_while_the_child_is_unsettled() {
    let mut trees = trees(
        &[("d", dir(ROOT, "new")), (NODE, file("d", "child", 1))],
        &[("d", dir(ROOT, "old")), (NODE, file("d", "child", 1))],
        &[
            Remote("d", dir(ROOT, "old"), true, 1),
            Remote(NODE, file("d", "child", 1), true, 1),
        ],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    let mut state = trees.state();
    let waiting = Waiting {
        shape: Some(file("elsewhere", "child", 1)),
        origin: Origin::Other,
    };
    state.waiting.insert(NODE.into(), waiting);
    trees.restore(state).unwrap();
    trees.rules_scanned();
    trees.settle();
    let intent = RestoreIntent::version(NODE.into(), NODE.into()).unwrap();
    assert!(commits.restore("request".into(), intent));
    let question = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["d"]);
    assert!(commits.decide(&question.id, Answer::Publish));
    let sent = ops(&mut commits, &mut trees, &mut ids);
    assert!(!sent.iter().any(|(kind, _)| *kind == "restore"), "{sent:?}");
}

#[test]
fn a_folder_whose_removal_from_s_waits_is_held_with_its_new_children_after_a_rewind() {
    let mut trees = trees(
        &[("d", dir(ROOT, "d")), (NODE, file("d", "new", 2))],
        &[("d", dir(ROOT, "d")), ("k", file("d", "kept", 1))],
        &[],
    );
    let mut state = trees.state();
    let waiting = Waiting {
        shape: None,
        origin: Origin::Other,
    };
    state.waiting.insert("d".into(), waiting);
    trees.restore(state).unwrap();
    trees.rules_scanned();
    trees.settle();
    assert!(trees.waiting().contains_key("d"));
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.enter_recovery(rewind());
    commits.recover(
        &mut trees,
        0,
        &rewind(),
        &History::new(),
        &Marks::new(),
        &mut ids,
        NOW,
    );
    trees.settle();
    asked(&commits, RecoveryGroup::CreatedOnThisDevice, &[NODE, "d"]);
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
}

#[test]
fn a_commit_counts_the_nodes_a_published_folder_brings_back_from_the_trash() {
    let folders: Vec<(String, String)> = (0..900)
        .map(|n| (format!("s{n}"), format!("g{n}")))
        .collect();
    let long = |n: &str| format!("{n}{}", "x".repeat(200));
    let mut local = vec![("d".to_owned(), dir(ROOT, "new"))];
    let mut synced = vec![("d".to_owned(), dir(ROOT, "old"))];
    for (folder, edited) in &folders {
        let shape = dir("d", &long(folder));
        local.push((folder.clone(), shape.clone()));
        synced.push((folder.clone(), shape));
        local.push((edited.clone(), file(folder, "g", 2)));
        synced.push((edited.clone(), file(folder, "g", 1)));
    }
    let remote: Vec<Remote> = (synced.iter())
        .map(|(id, shape)| Remote(id, shape.clone(), true, 1))
        .collect();
    let local: Vec<(&str, Shape)> = local
        .iter()
        .map(|(id, s)| (id.as_str(), s.clone()))
        .collect();
    let synced: Vec<(&str, Shape)> = synced
        .iter()
        .map(|(id, s)| (id.as_str(), s.clone()))
        .collect();
    let mut trees = trees(&local, &synced, &remote);
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    let [question] = commits.questions() else {
        panic!("{:?}", commits.questions());
    };
    assert_eq!(question.holds().count(), 1 + folders.len());
    assert!(commits.decide(&question.id.clone(), Answer::Publish));
    let sent = ops(&mut commits, &mut trees, &mut ids);
    assert_eq!(sent, [("move", "d".to_owned())]);
}

#[test]
fn publishing_a_folder_and_a_file_in_its_unchanged_subfolder_keeps_the_subfolder() {
    let mut trees = trees(
        &[
            ("a", dir(ROOT, "new")),
            ("b", dir("a", "b")),
            ("c", file("b", "c", 2)),
        ],
        &[
            ("a", dir(ROOT, "old")),
            ("b", dir("a", "b")),
            ("c", file("b", "c", 1)),
        ],
        &[
            Remote("a", dir(ROOT, "old"), true, 1),
            Remote("b", dir("a", "b"), true, 1),
            Remote("c", file("b", "c", 1), true, 1),
        ],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    let question = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["a", "c"]);
    assert!(commits.decide(&question.id, Answer::Publish));
    let mut sent = ops(&mut commits, &mut trees, &mut ids);
    sent.sort();
    assert_eq!(sent, [("move", "a".into()), ("update", "c".into())]);
    assert!(commits.in_flight().unwrap().commit.require_head);
}

#[test]
fn publishing_a_folder_and_a_file_moved_out_of_its_deleted_subfolder_leaves_the_subfolder_trashed()
{
    let mut trees = trees(
        &[("a", dir(ROOT, "new")), ("c", file(ROOT, "c", 1))],
        &[
            ("a", dir(ROOT, "old")),
            ("b", dir("a", "b")),
            ("c", file("b", "c", 1)),
        ],
        &[
            Remote("a", dir(ROOT, "old"), true, 1),
            Remote("b", dir("a", "b"), true, 1),
            Remote("c", file("b", "c", 1), true, 1),
        ],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    let question = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["a", "c"]);
    assert!(commits.decide(&question.id, Answer::Publish));
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [
            ("move", "c".into()),
            ("move", "a".into()),
            ("move", "b".into()),
            ("delete", "b".into())
        ]
    );
}

#[test]
fn a_held_node_excluded_and_included_again_keeps_its_question() {
    let mut trees = trees(
        &[("a", file(ROOT, "a", 1))],
        &[("a", file(ROOT, "a", 1))],
        &[],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    asked(&commits, RecoveryGroup::CreatedAfterRestorePoint, &["a"]);
    trees.set_rules::<&str>(&[], Some(RuleFile::parse(b"a\n")));
    trees.settle();
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    trees.set_rules::<&str>(&[], None);
    trees.settle();
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    asked(&commits, RecoveryGroup::CreatedAfterRestorePoint, &["a"]);
    assert_eq!(effects(&mut trees, &mut ids), []);
    assert!(trees.local().get("a").is_some());
}

#[test]
fn a_node_excluded_when_recovery_starts_is_asked_about_once_included() {
    let mut trees = trees(
        &[("a", file(ROOT, "a", 1))],
        &[("a", file(ROOT, "a", 1))],
        &[],
    );
    trees.set_rules::<&str>(&[], Some(RuleFile::parse(b"a\n")));
    trees.settle();
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    trees.set_rules::<&str>(&[], None);
    trees.settle();
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    asked(&commits, RecoveryGroup::CreatedAfterRestorePoint, &["a"]);
    assert_eq!(effects(&mut trees, &mut ids), []);
    assert!(trees.local().get("a").is_some());
}

#[test]
fn a_file_the_store_lost_that_another_device_deleted_unchanged_follows_the_delete() {
    let mut trees = trees(
        &[("a", file(ROOT, "a", 1))],
        &[],
        &[Remote("a", file(ROOT, "a", 1), true, 1)],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    assert!(commits.questions().is_empty(), "{:?}", commits.questions());
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let removed = effects(&mut trees, &mut ids);
    assert!(
        matches!(&removed[..], [(id, Effect::RemoveFile { .. })] if id == "a"),
        "{removed:?}"
    );
}

#[test]
fn entries_the_store_lost_in_a_folder_another_device_deleted_follow_the_delete() {
    let mut trees = trees(
        &[
            ("a", dir(ROOT, "a")),
            ("l-x", dir("a", "x")),
            ("l-f", file("a", "f", 1)),
        ],
        &[("a", dir(ROOT, "a"))],
        &[
            Remote("a", dir(ROOT, "a"), true, 1),
            Remote("r-x", dir("a", "x"), true, 1),
            Remote("r-f", file("a", "f", 1), true, 1),
        ],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    assert!(commits.questions().is_empty(), "{:?}", commits.questions());
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let mut removed: Vec<String> = (effects(&mut trees, &mut ids).into_iter())
        .map(|(id, _)| id)
        .collect();
    removed.sort();
    assert_eq!(removed, ["a", "r-f", "r-x"]);
}

#[test]
fn an_entry_the_store_lost_with_other_bytes_than_the_servers_trash_is_asked_about() {
    let mut trees = trees(
        &[("a", dir(ROOT, "a")), ("l-f", file("a", "f", 2))],
        &[("a", dir(ROOT, "a"))],
        &[
            Remote("a", dir(ROOT, "a"), true, 1),
            Remote("r-f", file("a", "f", 1), true, 1),
        ],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    asked(&commits, RecoveryGroup::CreatedOnThisDevice, &["l-f"]);
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    assert_eq!(effects(&mut trees, &mut ids), []);
    assert!(trees.local().get("l-f").is_some());
}

#[test]
fn an_answer_split_across_commits_keeps_its_approval_only_for_what_it_showed() {
    let mut trees = trees(
        &[("a", file(ROOT, "a", 2)), ("b", file(ROOT, "b", 2))],
        &[("a", file(ROOT, "a", 1)), ("b", file(ROOT, "b", 1))],
        &[live("a", file(ROOT, "a", 1)), live("b", file(ROOT, "b", 1))],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    let question = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["a", "b"]);
    assert!(commits.decide(&question.id, Answer::Publish));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids).len(), 2);
    let limit = Error {
        code: error::Code::LimitExceeded as i32,
        ..Default::default()
    };
    assert_eq!(commits.failed(&mut trees, &limit), Ok(Failure::Split));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let question = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["a", "b"]);
    assert!(commits.decide(&question.id, Answer::Publish));
    let sent = ops(&mut commits, &mut trees, &mut ids);
    let [(_, first)] = &sent[..] else {
        panic!("{sent:?}");
    };
    let rest = if first == "a" { "b" } else { "a" };
    let mut sent_node = trees.remote().get(first).unwrap().clone();
    sent_node.shape = trees.local().get(first).unwrap().shape.clone();
    sent_node.content_rev = 2;
    trees.set_remote(first.clone(), sent_node);
    let applied = OpResult {
        node_id: first.clone(),
        code: op_result::Code::Applied as i32,
        ..Default::default()
    };
    let result = CommitResult {
        seq: 2,
        results: vec![applied],
        ..Default::default()
    };
    commits.received(result).unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    trees.settle();
    let kept = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &[rest]);
    assert_eq!(kept.answer, Some(Answer::Publish));
    let mut changed = trees.local().get(rest).unwrap().clone();
    changed.shape.content = Some(content(9));
    trees.set_local(rest.into(), changed);
    trees.settle();
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let again = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &[rest]);
    assert_eq!(again.answer, None);
    assert!(commits.decide(&again.id, Answer::Publish));
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("update", rest.to_owned())]
    );
    assert!(commits.in_flight().unwrap().commit.require_head);
}

#[test]
fn a_rewind_a_refused_commit_found_takes_its_restore_point_from_the_next_welcome() {
    let mut trees = trees(
        &[("n", file(ROOT, "a", 2))],
        &[("n", file(ROOT, "a", 1))],
        &[live("n", file(ROOT, "a", 1))],
    );
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    assert!(
        commits
            .build(&mut trees, &mut ids, NOW, OLD, "me", 1)
            .is_some()
    );
    let mismatch = Error {
        code: error::Code::HistoryMismatch as i32,
        ..Default::default()
    };
    assert!(matches!(
        commits.failed(&mut trees, &mismatch),
        Ok(Failure::Halted(Halt::Recovery(_)))
    ));
    let welcome = Welcome {
        history_id: NEW.into(),
        restore_seq: 4,
        restore_point_ms: NOW as i64,
        ..Default::default()
    };
    assert_eq!(commits.connected(&welcome), Reconnect::Recovery);
    assert_eq!(
        commits.halted(),
        Some(Halt::Recovery(RecoveryCause::rewind(4, NOW as i64)))
    );
}

fn rewound_folder(stem: Option<&str>) -> Trees {
    let shape = dir(ROOT, "A (pending)");
    let mut trees = trees(&[("p", shape.clone())], &[], &[live("p", shape.clone())]);
    if let Some(stem) = stem {
        mark(&mut trees, "p", stem);
    }
    trees.set_synced("p".into(), Some(shape));
    trees.remove_remote("p");
    trees.settle();
    trees
}

fn dropped_from_s() -> Trees {
    let mut trees = rewound_folder(Some("A"));
    trees.set_synced("p".into(), None);
    trees.settle();
    assert_eq!(
        trees.state().unsent_marks,
        [("p".into(), "A".into())].into()
    );
    trees
}

fn created_dirs(
    commits: &mut Commits,
    trees: &mut Trees,
    ids: &mut IdGen,
) -> Vec<(String, String)> {
    let Some(commit) = commits.build(trees, ids, NOW, NEW, "me", 1) else {
        return Vec::new();
    };
    (commit.ops.iter())
        .filter_map(|op| match &op.op {
            Some(op::Op::CreateDir(create)) => {
                Some((create.name.clone(), create.pending_for.clone()))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn a_folder_kept_after_a_rewind_is_created_again_pending_for_the_name_s_marked() {
    let mut trees = rewound_folder(Some("A"));
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    let question = asked(&commits, RecoveryGroup::CreatedAfterRestorePoint, &["p"]);
    assert!(commits.decide(&question.id, Answer::Keep));
    assert_eq!(
        created_dirs(&mut commits, &mut trees, &mut ids),
        [("A (pending)".into(), "A".into())]
    );
}

#[test]
fn a_folder_a_user_named_like_a_pending_name_is_created_again_unmarked_after_a_rewind() {
    let mut trees = rewound_folder(None);
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    let question = asked(&commits, RecoveryGroup::CreatedAfterRestorePoint, &["p"]);
    assert!(commits.decide(&question.id, Answer::Keep));
    assert_eq!(
        created_dirs(&mut commits, &mut trees, &mut ids),
        [("A (pending)".into(), String::new())]
    );
}

#[test]
fn a_mark_s_dropped_is_sent_with_the_create_after_a_restart() {
    let before = dropped_from_s();
    let mut trees = Trees::new(ROOT);
    for (id, node) in before.local().iter() {
        trees.set_local(id.clone(), node.clone());
    }
    trees.restore(before.state()).unwrap();
    trees.rules_scanned();
    trees.settle();
    assert_eq!(
        created_dirs(&mut Commits::default(), &mut trees, &mut IdGen::new(7)),
        [("A (pending)".into(), "A".into())]
    );
}

struct Stale {
    trees: Trees,
    ids: IdGen,
    effects: Effects,
    record: EffectRecord,
    commits: Commits,
}

fn a_removal_planned_before_a_rewind() -> Stale {
    let mut trees = trees(
        &[("x", file(ROOT, "x", 1))],
        &[("x", file(ROOT, "x", 1))],
        &[Remote("x", file(ROOT, "x", 1), true, 2)],
    );
    let mut ids = IdGen::new(3);
    let mut effects = Effects::default();
    let record = (effects
        .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
        .first())
    .cloned()
    .expect("a planned removal");
    assert!(
        matches!(record.effect, Effect::RemoveFile { .. }),
        "{record:?}"
    );
    trees.remove_remote("x");
    trees.settle();
    let mut commits = Commits::default();
    commits.enter_recovery(rewind());
    commits.recover(
        &mut trees,
        0,
        &rewind(),
        &History::new(),
        &Marks::new(),
        &mut ids,
        NOW,
    );
    trees.settle();
    Stale {
        trees,
        ids,
        effects,
        record,
        commits,
    }
}

fn refused(stale: &mut Stale) {
    let Stale {
        trees,
        effects,
        record,
        commits,
        ..
    } = stale;
    assert!(!record.applies(trees, commits.in_flight()));
    let done = effects.done(trees, commits.in_flight(), &record.effect_id, None);
    assert!(done.is_err(), "{done:?}");
    trees.settle();
    assert_eq!(
        trees.local().get("x").map(|node| &node.shape),
        Some(&file(ROOT, "x", 1))
    );
}

#[test]
fn a_removal_planned_before_a_rewind_leaves_a_held_file_on_the_disk() {
    let mut stale = a_removal_planned_before_a_rewind();
    asked(
        &stale.commits,
        RecoveryGroup::CreatedAfterRestorePoint,
        &["x"],
    );
    refused(&mut stale);
    let Stale {
        trees,
        ids,
        commits,
        ..
    } = &mut stale;
    assert_eq!(ops(commits, trees, ids), []);
    asked(commits, RecoveryGroup::CreatedAfterRestorePoint, &["x"]);
}

#[test]
fn a_removal_planned_before_a_rewind_leaves_a_kept_file_while_its_create_is_in_flight() {
    let mut stale = a_removal_planned_before_a_rewind();
    let question = asked(
        &stale.commits,
        RecoveryGroup::CreatedAfterRestorePoint,
        &["x"],
    );
    assert!(stale.commits.decide(&question.id, Answer::Keep));
    let Stale {
        trees,
        ids,
        commits,
        ..
    } = &mut stale;
    assert_eq!(ops(commits, trees, ids), [("create file", "x".into())]);
    refused(&mut stale);
}

#[test]
fn a_removal_planned_before_a_rewind_leaves_a_kept_file_once_the_server_has_it() {
    let mut stale = a_removal_planned_before_a_rewind();
    let question = asked(
        &stale.commits,
        RecoveryGroup::CreatedAfterRestorePoint,
        &["x"],
    );
    assert!(stale.commits.decide(&question.id, Answer::Keep));
    let Stale {
        trees,
        ids,
        commits,
        ..
    } = &mut stale;
    assert_eq!(ops(commits, trees, ids), [("create file", "x".into())]);
    let sent = RemoteNode {
        made_over: Vec::new(),
        shape: file(ROOT, "x", 1),
        trashed: false,
        content_rev: 2,
        edit_rev: 2,
        meta_rev: 2,
        meta_edit_rev: 2,
        trash_group: String::new(),
        version_id: String::new(),
        pending_for: None,
        pending_by: None,
    };
    trees.set_remote("x".into(), sent);
    let applied = OpResult {
        node_id: "x".into(),
        code: op_result::Code::Applied as i32,
        ..Default::default()
    };
    let result = CommitResult {
        seq: 2,
        results: vec![applied],
        ..Default::default()
    };
    commits.received(result).unwrap();
    assert!(commits.acknowledge(trees, 2));
    trees.settle();
    refused(&mut stale);
}

#[test]
fn a_removal_planned_before_a_rewind_stays_refused_after_a_second_rewind() {
    let mut stale = a_removal_planned_before_a_rewind();
    asked(
        &stale.commits,
        RecoveryGroup::CreatedAfterRestorePoint,
        &["x"],
    );
    stale.commits.enter_recovery(rewind());
    stale.trees.settle();
    refused(&mut stale);
    let Stale {
        trees,
        ids,
        commits,
        ..
    } = &mut stale;
    commits.recover(
        trees,
        0,
        &rewind(),
        &History::new(),
        &Marks::new(),
        ids,
        NOW,
    );
    trees.settle();
    refused(&mut stale);
}

#[test]
fn a_download_planned_after_a_rewind_is_still_taken_after_a_reload() {
    let rows = [live("y", file(ROOT, "y", 1))];
    let mut trees = trees(&[], &[], &rows);
    let (mut commits, mut ids, mut effects) =
        (Commits::default(), IdGen::new(3), Effects::default());
    commits.enter_recovery(rewind());
    commits.recover(
        &mut trees,
        0,
        &rewind(),
        &History::new(),
        &Marks::new(),
        &mut ids,
        NOW,
    );
    trees.settle();
    let record = (effects
        .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
        .first())
    .cloned()
    .expect("a planned download");
    assert!(
        matches!(record.effect, Effect::WriteFile { .. }),
        "{record:?}"
    );
    assert!(record.applies(&trees, None));
    let mut reloaded = self::trees(&[], &[], &rows);
    let mut effects = Effects::from_state(effects.state()).unwrap();
    assert!(effects.in_flight()[0].applies(&reloaded, None));
    let identity = FileIdentity {
        ino: 9,
        birth_ns: Some(0),
    };
    effects
        .done(&mut reloaded, None, &record.effect_id, Some(identity))
        .unwrap();
    reloaded.settle();
    assert_eq!(reloaded.synced().get("y"), Some(&file(ROOT, "y", 1)));
}

#[test]
fn a_mark_s_dropped_goes_once_r_holds_the_folder() {
    let mut trees = dropped_from_s();
    let node = RemoteNode {
        made_over: Vec::new(),
        meta_edit_rev: 5,
        shape: dir(ROOT, "A (pending)"),
        trashed: false,
        content_rev: 5,
        meta_rev: 5,
        trash_group: String::new(),
        version_id: String::new(),
        pending_for: Some("A".into()),
        pending_by: Some("me".into()),
        edit_rev: 5,
    };
    trees.set_remote("p".into(), node);
    trees.settle();
    assert!(trees.state().unsent_marks.is_empty());
}

#[test]
fn a_mark_s_dropped_goes_when_the_user_renames_the_folder() {
    let mut trees = dropped_from_s();
    let mut node = trees.local().get("p").unwrap().clone();
    node.shape.name = "B".into();
    trees.set_local("p".into(), node);
    trees.settle();
    assert!(trees.state().unsent_marks.is_empty());
    assert_eq!(
        created_dirs(&mut Commits::default(), &mut trees, &mut IdGen::new(7)),
        [("B".into(), String::new())]
    );
}

#[test]
fn a_mark_s_dropped_goes_when_the_user_deletes_the_folder() {
    let mut trees = dropped_from_s();
    trees.remove_local("p");
    trees.settle();
    assert!(trees.state().unsent_marks.is_empty());
    assert_eq!(
        created_dirs(&mut Commits::default(), &mut trees, &mut IdGen::new(7)),
        []
    );
}

fn followed(
    local: &[(&str, Shape)],
    synced: &[(&str, Shape)],
    remote: &[Remote],
    history: &[(&str, Shape)],
    marks: &[(&str, &str)],
) -> (Trees, Commits, IdGen) {
    let recovered = recovered_by_marks(local, synced, remote, history, marks);
    let remote_ids: BTreeSet<&str> = remote.iter().map(|Remote(id, ..)| *id).collect();
    let own: Vec<&str> = (local.iter().map(|(id, _)| *id))
        .chain(synced.iter().map(|(id, _)| *id))
        .filter(|id| !remote_ids.contains(id))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let relabel: BTreeMap<&str, String> = (own.iter().rev().enumerate())
        .map(|(i, id)| (*id, format!("~{i}")))
        .collect();
    let swap = |id: &str| relabel.get(id).cloned().unwrap_or_else(|| id.to_owned());
    let moved = |shape: &Shape| Shape {
        parent: swap(&shape.parent),
        ..shape.clone()
    };
    let other_local: Vec<(String, Shape)> = (local.iter())
        .map(|(id, shape)| (swap(id), moved(shape)))
        .collect();
    let other_synced: Vec<(String, Shape)> = (synced.iter())
        .map(|(id, shape)| (swap(id), shape.clone()))
        .collect();
    let other_marks: Vec<(String, &str)> = (marks.iter())
        .map(|(local, node)| (swap(local), *node))
        .collect();
    let (relabeled, _, _) = recovered_by_marks(
        &other_local
            .iter()
            .map(|(id, shape)| (id.as_str(), shape.clone()))
            .collect::<Vec<_>>(),
        &other_synced
            .iter()
            .map(|(id, shape)| (id.as_str(), shape.clone()))
            .collect::<Vec<_>>(),
        remote,
        history,
        &other_marks
            .iter()
            .map(|(local, node)| (local.as_str(), *node))
            .collect::<Vec<_>>(),
    );
    keeps_the_mark_invariants(
        local, synced, remote, history, marks, &recovered, &relabeled,
    );
    recovered
}

fn nodes_by_entry(trees: &Trees, entries: usize, remote: &[Remote]) -> Vec<Option<String>> {
    let by_ino: BTreeMap<u128, &String> = (trees.local().iter())
        .map(|(id, node)| (node.identity.ino, id))
        .collect();
    let in_r: BTreeSet<&str> = remote.iter().map(|Remote(id, ..)| *id).collect();
    (0..entries)
        .map(|i| {
            let id = by_ino.get(&(i as u128 + 1))?;
            let adopted = trees.synced().live(id).is_some() && in_r.contains(id.as_str());
            adopted.then(|| (*id).clone())
        })
        .collect()
}

fn keeps_the_mark_invariants(
    local: &[(&str, Shape)],
    synced: &[(&str, Shape)],
    remote: &[Remote],
    history: &[(&str, Shape)],
    marks: &[(&str, &str)],
    (after, asked, _): &(Trees, Commits, IdGen),
    relabeled: &Trees,
) {
    let before = trees(local, synced, remote);
    let case = || {
        let in_r: Vec<(&str, &Shape, bool)> = (remote.iter())
            .map(|Remote(id, shape, trashed, _)| (*id, shape, *trashed))
            .collect();
        format!(
            "local {local:?}\nsynced {synced:?}\nremote {in_r:?}\nhistory {history:?}\nmarks {marks:?}"
        )
    };
    let found = nodes_by_entry(after, local.len(), remote);
    assert!(
        found == nodes_by_entry(relabeled, local.len(), remote),
        "the outcome depends on the order of the local IDs\n{}",
        case()
    );
    let mut known: BTreeMap<&str, Vec<&Shape>> = BTreeMap::new();
    for (id, shape) in history {
        known.entry(id).or_default().push(shape);
    }
    for Remote(id, shape, ..) in remote {
        known.entry(id).or_default().push(shape);
    }
    let in_r: BTreeMap<&str, (&Shape, bool)> = (remote.iter())
        .map(|Remote(id, shape, trashed, _)| (*id, (shape, *trashed)))
        .collect();
    let logged: BTreeSet<&str> = history.iter().map(|(id, _)| *id).collect();
    let local_ids: BTreeSet<&str> = local.iter().map(|(id, _)| *id).collect();
    let mark_of: BTreeMap<&str, &str> = marks.iter().copied().collect();
    let mut claims: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, node) in marks {
        *claims.entry(node).or_default() += 1;
    }
    let holder: BTreeMap<&str, usize> = (found.iter().enumerate())
        .filter_map(|(i, node)| Some((node.as_deref()?, i)))
        .collect();
    let claimable = |shape: &Shape, node: &str| {
        let live =
            (in_r.get(node)).is_some_and(|(live, trashed)| !trashed && live.kind == shape.kind);
        let content = (known.get(node).into_iter().flatten())
            .any(|known| known.kind == shape.kind && known.content == shape.content);
        claims[node] == 1 && live && logged.contains(node) && content
    };
    let mut given_up: BTreeSet<&str> = BTreeSet::new();
    loop {
        let more: Vec<&str> = (local.iter())
            .filter(|(id, shape)| {
                !given_up.contains(id)
                    && before.synced().get(id).is_none()
                    && mark_of.get(id).is_some_and(|node| {
                        node != id
                            && (!local_ids.contains(node) || given_up.contains(node))
                            && claimable(shape, node)
                    })
            })
            .map(|(id, _)| *id)
            .collect();
        if more.is_empty() {
            break;
        }
        given_up.extend(more);
    }
    for (i, (id, shape)) in local.iter().enumerate() {
        if before.synced().get(id).is_some() {
            continue;
        }
        let Some(node) = mark_of.get(id).copied() else {
            continue;
        };
        let free = !local_ids.contains(node) || node == *id || given_up.contains(node);
        if !(free && claimable(shape, node)) {
            continue;
        }
        assert!(
            found[i].as_deref().is_none_or(|found| found == node),
            "entry {id} with the mark of {node} became {:?}\n{}",
            found[i],
            case()
        );
        assert!(
            holder.get(node).is_none_or(|j| *j == i),
            "entry {} took {node}, which the mark of {id} claims\n{}",
            local[holder[node]].0,
            case()
        );
    }
    for (i, (id, shape)) in local.iter().enumerate() {
        let Some(node) = mark_of.get(id).copied().filter(|node| node != id) else {
            continue;
        };
        if before.synced().get(id).is_some() || found[i].as_deref() != Some(*id) {
            continue;
        }
        assert!(
            !claimable(shape, node) || after.local().get(node).is_some(),
            "entry {id} kept its ID while the mark of {node} claims\n{}",
            case()
        );
    }
    for (i, node) in found.iter().enumerate() {
        let Some(node) = node else {
            continue;
        };
        if before.synced().get(local[i].0).is_some() {
            continue;
        }
        if mark_of.contains_key(local[i].0) {
            let mut above = after
                .local()
                .get(node)
                .map(|entry| entry.shape.parent.clone());
            while let Some(at) = (above.filter(|at| *at != after.local().root))
                .filter(|at| after.synced().live(at).is_none())
            {
                let folder = after.local().get(&at).expect("an ancestor in L");
                assert!(
                    !mark_of.contains_key(local[folder.identity.ino as usize - 1].0),
                    "{node} followed its mark inside a marked folder S does not hold\n{}",
                    case()
                );
                above = Some(folder.shape.parent.clone());
            }
        }
        let entry = &after.local().get(node).expect("an adopted entry").shape;
        let held = after.synced().live(node).expect("S holds an adopted entry");
        assert!(
            held.content == entry.content,
            "S gives {node} other content than L\n{}",
            case()
        );
        let in_place = (known.get(node.as_str()).into_iter().flatten()).any(|known| {
            known.kind == entry.kind
                && known.content == entry.content
                && known.parent == entry.parent
                && same_name(&known.name, &entry.name)
        });
        assert!(
            !in_place || (held.parent == entry.parent && same_name(&held.name, &entry.name)),
            "{node} sits at a place the server gave it, but S holds it at {held:?}\n{}",
            case()
        );
        let moved = held.parent != entry.parent || !same_name(&held.name, &entry.name);
        let current =
            (in_r.get(node.as_str())).is_some_and(|(live, _)| live.content == entry.content);
        assert!(
            !moved
                || (current
                    && asked
                        .questions()
                        .iter()
                        .any(|question| question.holds().any(|held| held == node))),
            "{node} was adopted away from every place the server gave it without a question, or with content R does not give it\n{}",
            case()
        );
    }
    for (id, entry) in after.local().iter() {
        let Some(live) = after.remote().live(id) else {
            continue;
        };
        if (marks.iter()).any(|(local, node)| local == id && node != id) {
            continue;
        }
        let agrees = live.kind == entry.shape.kind
            && live.content == entry.shape.content
            && live.parent == entry.shape.parent
            && same_name(&live.name, &entry.shape.name);
        assert!(
            !agrees || after.synced().live(id).is_some() || after.waiting().get(id).is_some(),
            "{id} sits where R′ has it, but S holds nothing for it\n{}",
            case()
        );
    }
}

fn recovered_by_marks(
    local: &[(&str, Shape)],
    synced: &[(&str, Shape)],
    remote: &[Remote],
    history: &[(&str, Shape)],
    marks: &[(&str, &str)],
) -> (Trees, Commits, IdGen) {
    let mut trees = trees(local, synced, remote);
    let mut logged = History::new();
    for (id, shape) in history {
        logged.entry((*id).into()).or_default().push(shape.clone());
    }
    let marks: Marks = (marks.iter())
        .map(|(local, node)| ((*local).into(), (*node).into()))
        .collect();
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    commits.enter_recovery(rewind());
    commits.recover(&mut trees, 0, &rewind(), &logged, &marks, &mut ids, NOW);
    trees.settle();
    (trees, commits, ids)
}

fn local_shape<'a>(trees: &'a Trees, id: &str) -> Option<&'a Shape> {
    trees.local().get(id).map(|node| &node.shape)
}

#[test]
fn a_marked_entry_the_user_renamed_is_its_node_and_the_rename_is_committed() {
    let (mut trees, mut commits, mut ids) = followed(
        &[("entry", file(ROOT, "b", 1))],
        &[],
        &[live("n", file(ROOT, "a", 1))],
        &[("n", file(ROOT, "a", 1))],
        &[("entry", "n")],
    );
    assert_eq!(local_shape(&trees, "n"), Some(&file(ROOT, "b", 1)));
    assert_eq!(trees.local().get("entry"), None);
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "a", 1)));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let question = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["n"]);
    assert!(commits.decide(&question.id, Answer::Publish));
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("move", "n".to_owned())]
    );
}

#[test]
fn a_marked_entry_at_a_place_the_server_moved_it_from_follows_the_server() {
    let (mut trees, mut commits, mut ids) = followed(
        &[("entry", file(ROOT, "a", 1))],
        &[],
        &[live("n", file(ROOT, "b", 1))],
        &[("n", file(ROOT, "a", 1)), ("n", file(ROOT, "b", 1))],
        &[("entry", "n")],
    );
    assert_eq!(groups(&commits), []);
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "a", 1)));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let done = effects(&mut trees, &mut ids);
    assert!(
        matches!(&done[..], [(node, Effect::Move { .. })] if node == "n"),
        "{done:?}"
    );
    assert_eq!(local_shape(&trees, "n"), Some(&file(ROOT, "b", 1)));
}

#[test]
fn a_marked_entry_the_user_renamed_whose_content_the_server_replaced_is_asked_about_as_new() {
    let (mut trees, mut commits, mut ids) = followed(
        &[("entry", file(ROOT, "x", 1))],
        &[],
        &[live("n", file(ROOT, "c", 2))],
        &[
            ("n", file(ROOT, "a", 1)),
            ("n", file(ROOT, "b", 1)),
            ("n", file(ROOT, "b", 2)),
            ("n", file(ROOT, "c", 2)),
        ],
        &[("entry", "n")],
    );
    asked(&commits, RecoveryGroup::CreatedOnThisDevice, &["entry"]);
    assert_eq!(trees.synced().get("n"), None);
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
}

#[test]
fn a_marked_entry_whose_content_the_server_never_gave_its_node_is_held() {
    let (trees, commits, _) = followed(
        &[("entry", file(ROOT, "b", 2))],
        &[],
        &[live("n", file(ROOT, "a", 1))],
        &[("n", file(ROOT, "a", 1))],
        &[("entry", "n")],
    );
    asked(&commits, RecoveryGroup::CreatedOnThisDevice, &["entry"]);
    assert_eq!(trees.synced().get("n"), None);
}

#[test]
fn two_entries_with_one_mark_are_both_held() {
    let (trees, commits, _) = followed(
        &[("one", file(ROOT, "b", 1)), ("two", file(ROOT, "c", 1))],
        &[],
        &[live("n", file(ROOT, "a", 1))],
        &[("n", file(ROOT, "a", 1))],
        &[("one", "n"), ("two", "n")],
    );
    asked(
        &commits,
        RecoveryGroup::CreatedOnThisDevice,
        &["one", "two"],
    );
    assert_eq!(trees.synced().get("n"), None);
}

#[test]
fn a_mark_of_a_node_l_holds_leaves_the_node_to_its_own_entry() {
    let (trees, commits, _) = followed(
        &[("n", file(ROOT, "a", 1)), ("copy", file(ROOT, "b", 1))],
        &[],
        &[live("n", file(ROOT, "a", 1))],
        &[("n", file(ROOT, "a", 1))],
        &[("copy", "n")],
    );
    asked(&commits, RecoveryGroup::CreatedOnThisDevice, &["copy"]);
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "a", 1)));
}

#[test]
fn a_mark_of_a_node_r_does_not_hold_live_or_of_another_kind_is_not_followed() {
    for remote in [
        Remote("n", file(ROOT, "a", 1), true, 1),
        live("n", dir(ROOT, "a")),
        live("other", file(ROOT, "a", 1)),
    ] {
        let (trees, commits, _) = followed(
            &[("entry", file(ROOT, "b", 1))],
            &[],
            &[remote],
            &[("n", file(ROOT, "a", 1))],
            &[("entry", "n")],
        );
        asked(&commits, RecoveryGroup::CreatedOnThisDevice, &["entry"]);
        assert_eq!(trees.synced().get("n"), None);
    }
}

#[test]
fn without_the_nodes_history_a_marked_entry_only_follows_at_the_servers_place() {
    let (trees, commits, _) = followed(
        &[("entry", file(ROOT, "b", 1))],
        &[],
        &[live("n", file(ROOT, "a", 1))],
        &[],
        &[("entry", "n")],
    );
    asked(&commits, RecoveryGroup::CreatedOnThisDevice, &["entry"]);
    assert_eq!(trees.synced().get("n"), None);
    let (trees, commits, _) = followed(
        &[("entry", file(ROOT, "a", 1))],
        &[],
        &[live("n", file(ROOT, "a", 1))],
        &[],
        &[("entry", "n")],
    );
    assert_eq!(groups(&commits), []);
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "a", 1)));
}

#[test]
fn a_marked_entry_whose_shape_s_cannot_hold_is_held() {
    let (trees, commits, _) = followed(
        &[("entry", file(ROOT, "z", 1)), ("m", file(ROOT, "a", 3))],
        &[("m", file(ROOT, "a", 3))],
        &[live("n", file(ROOT, "b", 2)), live("m", file(ROOT, "a", 3))],
        &[("n", file(ROOT, "a", 1)), ("n", file(ROOT, "b", 2))],
        &[("entry", "n")],
    );
    asked(&commits, RecoveryGroup::CreatedOnThisDevice, &["entry"]);
    assert_eq!(trees.synced().get("n"), None);
}

#[test]
fn a_marked_file_the_user_moved_out_of_its_marked_folder_follows_once_the_folder_does() {
    let (mut trees, mut commits, mut ids) = followed(
        &[("a-file", file(ROOT, "f", 1)), ("b-folder", dir(ROOT, "d"))],
        &[],
        &[live("d", dir(ROOT, "d")), live("n", file("d", "f", 1))],
        &[("d", dir(ROOT, "d")), ("n", file("d", "f", 1))],
        &[("a-file", "n"), ("b-folder", "d")],
    );
    assert_eq!(trees.synced().live("d"), Some(&dir(ROOT, "d")));
    assert_eq!(trees.synced().live("n"), Some(&file("d", "f", 1)));
    assert_eq!(local_shape(&trees, "n"), Some(&file(ROOT, "f", 1)));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let question = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["n"]);
    assert!(commits.decide(&question.id, Answer::Publish));
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("move", "n".to_owned())]
    );
}

#[test]
fn a_marked_file_the_user_renamed_in_an_unmarked_folder_follows_once_the_folder_is_adopted() {
    let (mut trees, mut commits, mut ids) = followed(
        &[
            ("a-file", file("b-folder", "g", 1)),
            ("b-folder", dir(ROOT, "d")),
        ],
        &[],
        &[live("d", dir(ROOT, "d")), live("n", file("d", "f", 1))],
        &[("d", dir(ROOT, "d")), ("n", file("d", "f", 1))],
        &[("a-file", "n")],
    );
    assert_eq!(trees.synced().live("d"), Some(&dir(ROOT, "d")));
    assert_eq!(trees.synced().live("n"), Some(&file("d", "f", 1)));
    assert_eq!(local_shape(&trees, "n"), Some(&file("d", "g", 1)));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let question = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["n"]);
    assert!(commits.decide(&question.id, Answer::Publish));
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("move", "n".to_owned())]
    );
}

#[test]
fn a_copy_at_the_old_name_in_an_unmarked_folder_leaves_the_node_to_the_renamed_original() {
    let (mut trees, mut commits, mut ids) = followed(
        &[
            ("a-file", file("b-folder", "g", 1)),
            ("b-folder", dir(ROOT, "d")),
            ("c-copy", file("b-folder", "f", 1)),
        ],
        &[],
        &[live("d", dir(ROOT, "d")), live("n", file("d", "f", 1))],
        &[("d", dir(ROOT, "d")), ("n", file("d", "f", 1))],
        &[("a-file", "n")],
    );
    asked(&commits, RecoveryGroup::CreatedOnThisDevice, &["c-copy"]);
    assert_eq!(trees.synced().live("n"), Some(&file("d", "f", 1)));
    assert_eq!(local_shape(&trees, "n"), Some(&file("d", "g", 1)));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let question = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["n"]);
    assert!(commits.decide(&question.id, Answer::Publish));
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("move", "n".to_owned())]
    );
}

#[test]
fn a_copy_at_the_old_name_takes_the_node_when_the_renamed_original_cannot_follow_its_mark() {
    let (trees, commits, _) = followed(
        &[
            ("a-file", file("b-folder", "g", 2)),
            ("b-folder", dir(ROOT, "d")),
            ("c-copy", file("b-folder", "f", 1)),
        ],
        &[],
        &[live("d", dir(ROOT, "d")), live("n", file("d", "f", 1))],
        &[("d", dir(ROOT, "d")), ("n", file("d", "f", 1))],
        &[("a-file", "n")],
    );
    asked(&commits, RecoveryGroup::CreatedOnThisDevice, &["a-file"]);
    assert_eq!(trees.synced().live("n"), Some(&file("d", "f", 1)));
    assert_eq!(local_shape(&trees, "n"), Some(&file("d", "f", 1)));
}

#[test]
fn a_marked_entry_whose_name_is_the_same_in_nfc_is_in_place_and_follows_the_servers_rename() {
    let (mut trees, mut commits, mut ids) = followed(
        &[("entry", file(ROOT, "cafe\u{301}", 1))],
        &[],
        &[live("n", file(ROOT, "b", 1))],
        &[("n", file(ROOT, "caf\u{e9}", 1)), ("n", file(ROOT, "b", 1))],
        &[("entry", "n")],
    );
    assert_eq!(groups(&commits), []);
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "caf\u{e9}", 1)));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let done = effects(&mut trees, &mut ids);
    assert!(
        matches!(&done[..], [(node, Effect::Move { .. })] if node == "n"),
        "{done:?}"
    );
    assert_eq!(local_shape(&trees, "n"), Some(&file(ROOT, "b", 1)));
}

#[test]
fn a_marked_entry_the_user_renamed_by_case_only_commits_the_rename() {
    let (mut trees, mut commits, mut ids) = followed(
        &[("entry", file(ROOT, "Cafe", 1))],
        &[],
        &[live("n", file(ROOT, "cafe", 1))],
        &[("n", file(ROOT, "cafe", 1))],
        &[("entry", "n")],
    );
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "cafe", 1)));
    assert_eq!(local_shape(&trees, "n"), Some(&file(ROOT, "Cafe", 1)));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let question = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["n"]);
    assert!(commits.decide(&question.id, Answer::Publish));
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("move", "n".to_owned())]
    );
}

#[test]
fn a_mark_whose_node_another_mark_frees_follows_before_place_adoption() {
    let (mut trees, mut commits, mut ids) = followed(
        &[
            ("n", file(ROOT, "x", 1)),
            ("renamed", file(ROOT, "b", 2)),
            ("copy", file(ROOT, "a", 2)),
        ],
        &[],
        &[live("m", file(ROOT, "x", 1)), live("n", file(ROOT, "a", 2))],
        &[("m", file(ROOT, "x", 1)), ("n", file(ROOT, "a", 2))],
        &[("n", "m"), ("renamed", "n")],
    );
    asked(&commits, RecoveryGroup::CreatedOnThisDevice, &["copy"]);
    assert_eq!(local_shape(&trees, "m"), Some(&file(ROOT, "x", 1)));
    assert_eq!(local_shape(&trees, "n"), Some(&file(ROOT, "b", 2)));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let question = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["n"]);
    assert!(commits.decide(&question.id, Answer::Publish));
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("move", "n".to_owned())]
    );
}

#[test]
fn a_marked_folder_takes_an_older_place_and_its_file_whose_content_the_server_replaced_waits() {
    let (trees, commits, _) = followed(
        &[
            ("a-file", file(ROOT, "g", 1)),
            ("a-folder", dir(ROOT, "q")),
            ("b-folder", dir(ROOT, "p")),
            ("s-folder", dir(ROOT, "s")),
            ("there", file("s-folder", "f", 2)),
        ],
        &[],
        &[
            live("r", dir(ROOT, "r")),
            live("p", dir("r", "p")),
            live("s", dir(ROOT, "s")),
            live("n", file("s", "f", 2)),
        ],
        &[
            ("r", dir(ROOT, "r")),
            ("p", dir(ROOT, "p")),
            ("p", dir("r", "p")),
            ("s", dir(ROOT, "s")),
            ("n", file("p", "f", 1)),
            ("n", file("s", "f", 2)),
        ],
        &[("a-file", "n"), ("a-folder", "p")],
    );
    assert_eq!(trees.synced().live("p"), Some(&dir(ROOT, "p")));
    assert_eq!(local_shape(&trees, "p"), Some(&dir(ROOT, "q")));
    assert_eq!(trees.synced().get("n"), None);
    asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["p"]);
    asked(
        &commits,
        RecoveryGroup::CreatedOnThisDevice,
        &["a-file", "b-folder", "there"],
    );
}

#[test]
fn a_node_a_mark_claims_goes_to_no_other_entry_while_the_mark_cannot_follow() {
    let (trees, commits, _) = followed(
        &[
            ("a-file", file(ROOT, "g", 1)),
            ("a-folder", dir(ROOT, "q")),
            ("b-folder", dir(ROOT, "b")),
            ("i-folder", dir("b-folder", "i")),
            ("s-folder", dir(ROOT, "s")),
            ("there", file("s-folder", "f", 2)),
        ],
        &[],
        &[
            live("r", dir(ROOT, "r")),
            live("b", dir("r", "b")),
            live("u", dir(ROOT, "u")),
            live("i", dir("u", "i")),
            live("s", dir(ROOT, "s")),
            live("n", file("s", "f", 2)),
        ],
        &[
            ("b", dir(ROOT, "b")),
            ("b", dir("r", "b")),
            ("i", dir("b", "i")),
            ("i", dir("u", "i")),
            ("n", file("i", "f", 1)),
            ("n", file("s", "f", 2)),
        ],
        &[("a-file", "n"), ("a-folder", "b")],
    );
    assert_eq!(trees.synced().get("n"), None);
    assert_eq!(trees.local().get("n"), None);
    assert_eq!(local_shape(&trees, "b"), Some(&dir(ROOT, "q")));
    asked(
        &commits,
        RecoveryGroup::CreatedOnThisDevice,
        &["a-file", "b-folder", "i-folder", "there"],
    );
}

#[test]
fn a_marked_entry_at_its_logged_place_waits_when_s_cannot_hold_it_there() {
    let (mut trees, mut commits, mut ids) = followed(
        &[("entry", file(ROOT, "a", 1)), ("m", file(ROOT, "z", 3))],
        &[("m", file(ROOT, "a", 3))],
        &[live("n", file(ROOT, "b", 1)), live("m", file(ROOT, "w", 3))],
        &[("n", file(ROOT, "a", 1)), ("n", file(ROOT, "b", 1))],
        &[("entry", "n")],
    );
    assert_eq!(trees.synced().get("n"), None);
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
}

#[test]
fn a_marked_file_the_user_moved_into_a_new_folder_follows_and_the_folder_is_asked_about() {
    let (trees, commits, _) = followed(
        &[("entry", file("new", "f", 1)), ("new", dir(ROOT, "fresh"))],
        &[],
        &[live("n", file(ROOT, "f", 1))],
        &[("n", file(ROOT, "f", 1))],
        &[("entry", "n")],
    );
    asked(&commits, RecoveryGroup::CreatedOnThisDevice, &["new"]);
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "f", 1)));
    assert_eq!(local_shape(&trees, "n"), Some(&file("new", "f", 1)));
}

#[test]
fn a_marked_file_moved_into_a_new_folder_named_like_a_files_pending_name_follows() {
    let mut trees = trees(
        &[("entry", file("new", "f", 1)), ("new", dir(ROOT, "fresh"))],
        &[],
        &[
            live("n", file(ROOT, "f", 1)),
            live("p", file(ROOT, "fresh (pending)", 2)),
        ],
    );
    mark(&mut trees, "p", "fresh");
    let mut logged = History::new();
    logged
        .entry("n".into())
        .or_default()
        .push(file(ROOT, "f", 1));
    let marks: Marks = [("entry".to_owned(), "n".to_owned())].into_iter().collect();
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    commits.enter_recovery(rewind());
    commits.recover(&mut trees, 0, &rewind(), &logged, &marks, &mut ids, NOW);
    trees.settle();
    assert_eq!(trees.synced().live("n"), Some(&file(ROOT, "f", 1)));
    assert_eq!(local_shape(&trees, "n"), Some(&file("new", "f", 1)));
}

#[test]
fn a_marked_file_moved_into_a_new_folder_named_like_a_server_folder_elsewhere_follows() {
    let (trees, _, _) = followed(
        &[("entry", file("new", "f", 1)), ("new", dir(ROOT, "fresh"))],
        &[],
        &[
            live("n", file(ROOT, "f", 1)),
            live("o", dir(ROOT, "other")),
            live("o2", dir("o", "fresh")),
        ],
        &[("n", file(ROOT, "f", 1))],
        &[("entry", "n")],
    );
    assert_eq!(local_shape(&trees, "n"), Some(&file("new", "f", 1)));
}

#[test]
fn a_marked_file_in_a_marked_folder_moved_into_a_new_folder_keeps_its_place() {
    let (trees, _, _) = followed(
        &[
            ("z", dir(ROOT, "new")),
            ("y", dir("z", "d")),
            ("f", file("y", "a", 1)),
        ],
        &[],
        &[live("n", dir(ROOT, "d")), live("m", file("n", "b", 1))],
        &[
            ("n", dir(ROOT, "d")),
            ("m", file("n", "a", 1)),
            ("m", file("n", "b", 1)),
        ],
        &[("y", "n"), ("f", "m")],
    );
    assert_eq!(trees.synced().live("n"), Some(&dir(ROOT, "d")));
    assert_eq!(trees.synced().live("m"), Some(&file("n", "a", 1)));
}

#[test]
fn a_file_in_a_marked_folder_that_cannot_follow_waits_with_it() {
    let (mut trees, mut commits, mut ids) = followed(
        &[
            ("folder", dir(ROOT, "fresh")),
            ("entry", file("folder", "a", 1)),
            ("m", file(ROOT, "z", 3)),
        ],
        &[("m", file(ROOT, "p", 3))],
        &[
            live("r", dir(ROOT, "r")),
            live("p", dir("r", "p")),
            live("n", file(ROOT, "b", 1)),
            live("m", file(ROOT, "w", 3)),
        ],
        &[
            ("p", dir(ROOT, "p")),
            ("p", dir("r", "p")),
            ("n", file("p", "a", 1)),
            ("n", file(ROOT, "b", 1)),
        ],
        &[("folder", "p"), ("entry", "n")],
    );
    assert_eq!(trees.synced().get("p"), None);
    assert_eq!(trees.synced().get("n"), None);
    let question = (commits.questions().iter())
        .find(|question| question.holds().any(|id| id == "folder"))
        .expect("a question about the folder")
        .clone();
    assert!(question.holds().any(|id| id == "entry"));
    assert!(commits.decide(&question.id, Answer::Publish));
    let sent = ops(&mut commits, &mut trees, &mut ids);
    assert!(!sent.iter().any(|(kind, id)| *kind == "move" && id == "n"));
}

#[test]
fn a_marked_folder_the_server_holds_under_its_pending_name_is_adopted_and_moved_to_its_name() {
    let mut trees = trees(
        &[("f", dir(ROOT, "x"))],
        &[],
        &[live("p", dir(ROOT, "x (pending)"))],
    );
    mark(&mut trees, "p", "x");
    let mut logged = History::new();
    logged
        .entry("p".into())
        .or_default()
        .push(dir(ROOT, "x (pending)"));
    let marks: Marks = [("f".to_owned(), "p".to_owned())].into_iter().collect();
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    commits.enter_recovery(rewind());
    commits.recover(&mut trees, 0, &rewind(), &logged, &marks, &mut ids, NOW);
    trees.settle();
    assert_eq!(groups(&commits), []);
    assert_eq!(local_shape(&trees, "p"), Some(&dir(ROOT, "x")));
    assert_eq!(trees.synced().live("p"), Some(&dir(ROOT, "x (pending)")));
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("move", "p".to_owned())]
    );
}

#[test]
fn a_waiting_marked_entry_takes_no_other_node_at_its_place() {
    let (trees, _, _) = followed(
        &[
            ("d-folder", dir(ROOT, "d")),
            ("entry", file(ROOT, "renamed", 1)),
        ],
        &[],
        &[
            live("d", dir(ROOT, "d")),
            live("n", file("d", "orig", 1)),
            live("m", file(ROOT, "renamed", 1)),
        ],
        &[("n", file("d", "orig", 1))],
        &[("entry", "n")],
    );
    assert_eq!(local_shape(&trees, "n"), Some(&file(ROOT, "renamed", 1)));
    assert_eq!(trees.synced().live("n"), Some(&file("d", "orig", 1)));
    assert_eq!(trees.local().get("m"), None);
}

#[test]
fn a_result_gives_a_node_to_applied_creates_merged_folders_and_conflict_copies() {
    use koidrive_engine::commit::given_nodes;
    use koidrive_proto::v1::{Commit, CreateDir, CreateFile, Op, UpdateContent};
    let sent = |op| Op { op: Some(op) };
    let commit = Commit {
        ops: vec![
            sent(op::Op::CreateFile(CreateFile {
                node_id: "file".into(),
                ..Default::default()
            })),
            sent(op::Op::CreateDir(CreateDir {
                node_id: "dir".into(),
                ..Default::default()
            })),
            sent(op::Op::CreateDir(CreateDir {
                node_id: "merged".into(),
                ..Default::default()
            })),
            sent(op::Op::UpdateContent(UpdateContent {
                node_id: "edited".into(),
                ..Default::default()
            })),
            sent(op::Op::CreateFile(CreateFile {
                node_id: "refused".into(),
                ..Default::default()
            })),
        ],
        ..Default::default()
    };
    let outcome = |code: op_result::Code, node: &str| OpResult {
        code: code as i32,
        node_id: node.into(),
        ..Default::default()
    };
    let result = CommitResult {
        results: vec![
            outcome(op_result::Code::Applied, ""),
            outcome(op_result::Code::AppliedRenamed, ""),
            outcome(op_result::Code::MergedInto, "theirs"),
            outcome(op_result::Code::ConflictCopy, "copy"),
            outcome(op_result::Code::Rejected, ""),
        ],
        ..Default::default()
    };
    let pair = |sent: &str, node: &str| (sent.to_owned(), node.to_owned());
    assert_eq!(
        given_nodes(&commit, &result),
        [
            pair("file", "file"),
            pair("dir", "dir"),
            pair("merged", "theirs"),
            pair("edited", "copy")
        ]
    );
}

#[test]
fn a_marked_folder_the_servers_place_cannot_hold_follows_an_older_place_and_its_marked_child_follows()
 {
    let (trees, commits, _) = followed(
        &[
            ("a-file", file(ROOT, "g", 1)),
            ("a-folder", dir(ROOT, "q")),
            ("b-folder", dir(ROOT, "p")),
            ("copy", file("b-folder", "f", 1)),
        ],
        &[],
        &[
            live("r", dir(ROOT, "r")),
            live("p", dir("r", "p")),
            live("n", file("p", "f", 1)),
        ],
        &[
            ("r", dir(ROOT, "r")),
            ("p", dir(ROOT, "p")),
            ("p", dir("r", "p")),
            ("n", file("p", "f", 1)),
        ],
        &[("a-file", "n"), ("a-folder", "p")],
    );
    assert_eq!(local_shape(&trees, "n"), Some(&file(ROOT, "g", 1)));
    assert_eq!(trees.synced().live("p"), Some(&dir(ROOT, "p")));
    assert_eq!(local_shape(&trees, "p"), Some(&dir(ROOT, "q")));
    asked(
        &commits,
        RecoveryGroup::CreatedOnThisDevice,
        &["b-folder", "copy"],
    );
}

const DIR_NAMES: [&str; 2] = ["p", "q"];
const FILE_NAMES: [&str; 3] = ["a", "b", "caf\u{e9}"];
const LOCAL_NAMES: [&str; 6] = ["a", "b", "caf\u{e9}", "cafe\u{301}", "p", "q"];

struct Drawn {
    local: Vec<(String, Shape)>,
    synced: Vec<(String, Shape)>,
    remote: Vec<(String, Shape, bool)>,
    history: Vec<(String, Shape)>,
    marks: Vec<(String, String)>,
}

fn drawn(genes: &[u8]) -> Drawn {
    let mut genes = genes.iter().copied().chain(std::iter::repeat(0));
    let mut next = move |n: usize| genes.next().expect("endless") as usize % n;
    let mut remote: Vec<(String, Shape, bool)> = Vec::new();
    let mut taken = BTreeSet::new();
    for d in 0..next(3) {
        let name = DIR_NAMES[next(2)];
        if taken.insert((ROOT.to_owned(), name.to_owned())) {
            remote.push((format!("d{d}"), dir(ROOT, name), false));
        }
    }
    let parents: Vec<String> = std::iter::once(ROOT.to_owned())
        .chain(remote.iter().map(|(id, ..)| id.clone()))
        .collect();
    for f in 0..1 + next(3) {
        let parent = &parents[next(parents.len())];
        let name = FILE_NAMES[next(3)];
        let trashed = next(6) == 0;
        if trashed || taken.insert((parent.clone(), name.to_owned())) {
            let shape = file(parent, name, 1 + next(2) as u8);
            remote.push((format!("n{f}"), shape, trashed));
        }
    }
    let mut history = Vec::new();
    for (id, shape, _) in &remote {
        for _ in 0..next(3) {
            let parent = &parents[next(parents.len())];
            let logged = match shape.kind {
                Kind::Dir => dir(parent, DIR_NAMES[next(2)]),
                Kind::File => file(parent, FILE_NAMES[next(3)], 1 + next(2) as u8),
            };
            history.push((id.clone(), logged));
        }
        if next(2) == 0 {
            history.push((id.clone(), shape.clone()));
        }
    }
    let mut local: Vec<(String, Shape)> = Vec::new();
    let mut folders = vec![ROOT.to_owned()];
    let mut places = BTreeSet::new();
    for i in 0..1 + next(4) {
        let is_dir = next(4) == 0;
        let parent = folders[next(folders.len())].clone();
        let name = LOCAL_NAMES[next(6)];
        if !places.insert((parent.clone(), nfc(name))) {
            continue;
        }
        let kind = if is_dir { Kind::Dir } else { Kind::File };
        let reused: Vec<&String> = (remote.iter())
            .filter(|(id, shape, _)| {
                shape.kind == kind && local.iter().all(|(other, _)| other != id)
            })
            .map(|(id, ..)| id)
            .collect();
        let id = match next(4) {
            0 if !reused.is_empty() => reused[next(reused.len())].clone(),
            _ => format!("e{i}"),
        };
        let shape = match kind {
            Kind::Dir => dir(&parent, name),
            Kind::File => file(&parent, name, 1 + next(2) as u8),
        };
        if is_dir {
            folders.push(id.clone());
        }
        local.push((id, shape));
    }
    let mut synced = Vec::new();
    let mut held = BTreeSet::new();
    for (id, shape) in &local {
        if remote.iter().all(|(node, ..)| node != id) || next(3) != 0 {
            continue;
        }
        let at = match shape.kind {
            Kind::Dir => dir(ROOT, DIR_NAMES[next(2)]),
            Kind::File => file(ROOT, FILE_NAMES[next(3)], 1 + next(2) as u8),
        };
        if held.insert(nfc(&at.name)) {
            synced.push((id.clone(), at));
        }
    }
    let mut marks = Vec::new();
    for (id, _) in &local {
        if next(3) != 0 {
            let (node, ..) = &remote[next(remote.len())];
            marks.push((id.clone(), node.clone()));
        }
    }
    Drawn {
        local,
        synced,
        remote,
        history,
        marks,
    }
}

fn as_str(rows: &[(String, Shape)]) -> Vec<(&str, Shape)> {
    (rows.iter())
        .map(|(id, shape)| (id.as_str(), shape.clone()))
        .collect()
}

proptest! {
    #![proptest_config(koidrive_test_support::battery_config(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/recovery.proptest-regressions"),
        4096,
    ))]

    #[test]
    fn a_recovery_by_mark_keeps_its_rules_for_any_marks_places_and_history(
        genes in proptest::collection::vec(any::<u8>(), 64),
    ) {
        let drawn = drawn(&genes);
        let remote: Vec<Remote> = (drawn.remote.iter())
            .map(|(id, shape, trashed)| Remote(id, shape.clone(), *trashed, 1))
            .collect();
        let marks: Vec<(&str, &str)> = (drawn.marks.iter())
            .map(|(local, node)| (local.as_str(), node.as_str()))
            .collect();
        followed(
            &as_str(&drawn.local),
            &as_str(&drawn.synced),
            &remote,
            &as_str(&drawn.history),
            &marks,
        );
    }
}

#[test]
fn a_marked_file_at_its_place_in_a_moved_folder_whose_mark_cannot_follow_is_settled_to_r() {
    let (trees, _, _) = followed(
        &[
            ("c", dir(ROOT, "c")),
            ("p", dir("c", "a")),
            ("n", file("p", "a", 1)),
        ],
        &[("c", dir(ROOT, "c"))],
        &[
            live("c", dir(ROOT, "c")),
            live("p", dir(ROOT, "A")),
            live("n", file("p", "a", 1)),
        ],
        &[],
        &[("p", "p"), ("n", "n")],
    );
    let settled = (trees.synced().live("n")).or_else(|| trees.waiting().get("n")?.shape.as_ref());
    assert_eq!(settled, Some(&file("p", "a", 1)));
}

#[test]
fn a_file_whose_id_names_another_node_than_its_waiting_mark_is_asked_about_as_new() {
    let (trees, commits, _) = followed(
        &[("d0", dir(ROOT, "p")), ("n1", file("d0", "b", 2))],
        &[],
        &[
            live("d0", dir(ROOT, "q")),
            live("n1", file("d0", "b", 2)),
            live("n2", file(ROOT, "a", 1)),
        ],
        &[
            ("d0", dir(ROOT, "p")),
            ("d0", dir(ROOT, "q")),
            ("n2", file(ROOT, "a", 2)),
        ],
        &[("d0", "d0"), ("n1", "n2")],
    );
    assert_eq!(trees.synced().live("n1"), None);
    let (entry, _) = (trees.local().iter())
        .find(|(_, node)| node.shape == file("d0", "b", 2))
        .expect("the entry stays on disk");
    assert_ne!(entry, "n1");
    asked(&commits, RecoveryGroup::CreatedOnThisDevice, &[entry]);
}

#[test]
fn an_entry_marked_with_the_id_another_marked_entry_gives_up_is_adopted_as_that_node() {
    let (trees, _, _) = followed(
        &[
            ("d0", dir(ROOT, "p")),
            ("n1", file("d0", "b", 2)),
            ("e0", file("d0", "c", 2)),
        ],
        &[],
        &[
            live("d0", dir(ROOT, "q")),
            live("n1", file("d0", "b", 2)),
            live("n2", file(ROOT, "a", 1)),
        ],
        &[
            ("d0", dir(ROOT, "p")),
            ("d0", dir(ROOT, "q")),
            ("n2", file(ROOT, "a", 2)),
            ("n1", file("d0", "c", 2)),
            ("n1", file("d0", "b", 2)),
        ],
        &[("d0", "d0"), ("n1", "n2"), ("e0", "n1")],
    );
    assert_eq!(local_shape(&trees, "n1"), Some(&file("d0", "c", 2)));
    assert_eq!(trees.local().get("e0"), None);
}

#[test]
fn a_file_whose_mark_names_the_id_another_entry_gives_up_is_not_settled_as_its_own_id() {
    let (trees, _, _) = followed(
        &[
            ("d0", dir(ROOT, "p")),
            ("n1", file("d0", "b", 2)),
            ("n2", file("d0", "z", 3)),
        ],
        &[],
        &[
            live("d0", dir(ROOT, "q")),
            live("n1", file("d0", "b", 2)),
            live("n2", file(ROOT, "a", 1)),
            live("n3", file(ROOT, "c", 3)),
        ],
        &[
            ("d0", dir(ROOT, "p")),
            ("d0", dir(ROOT, "q")),
            ("n2", file(ROOT, "a", 2)),
            ("n3", file(ROOT, "c", 3)),
        ],
        &[("d0", "d0"), ("n1", "n2"), ("n2", "n3")],
    );
    assert_eq!(trees.synced().live("n1"), None);
    assert_eq!(trees.local().get("n1"), None);
}

#[test]
fn entries_whose_marks_name_the_ids_a_chain_of_entries_gives_up_are_not_settled_as_their_own_ids() {
    let (trees, _, _) = followed(
        &[
            ("d0", dir(ROOT, "p")),
            ("n0", file("d0", "c", 4)),
            ("n1", file("d0", "b", 2)),
            ("n2", file("d0", "z", 3)),
        ],
        &[],
        &[
            live("d0", dir(ROOT, "q")),
            live("n0", file("d0", "c", 4)),
            live("n1", file("d0", "b", 2)),
            live("n2", file(ROOT, "a", 1)),
            live("n3", file(ROOT, "c", 3)),
        ],
        &[
            ("d0", dir(ROOT, "p")),
            ("d0", dir(ROOT, "q")),
            ("n1", file(ROOT, "y", 4)),
            ("n2", file(ROOT, "a", 2)),
            ("n3", file(ROOT, "c", 3)),
        ],
        &[("d0", "d0"), ("n0", "n1"), ("n1", "n2"), ("n2", "n3")],
    );
    assert_eq!(trees.synced().live("n0"), None);
    assert_eq!(trees.synced().live("n1"), None);
}

#[test]
fn a_file_whose_mark_claims_once_its_folder_is_adopted_gives_up_another_nodes_id() {
    let (mut trees, mut commits, mut ids) = recovered_by_marks(
        &[("e0", dir(ROOT, "p")), ("n0", file("e0", "c", 1))],
        &[("d0", dir(ROOT, "p")), ("held", file("d0", "c", 9))],
        &[
            live("e0", dir(ROOT, "old")),
            live("d0", dir(ROOT, "p")),
            live("n0", file("d0", "b", 1)),
            live("n1", file("d0", "c", 1)),
            live("held", file("d0", "z", 9)),
        ],
        &[],
        &[("e0", "d0"), ("n0", "n1")],
    );
    assert_eq!(trees.local().get("n0"), None);
    let asked: Vec<String> = (commits.questions().iter())
        .map(|question| question.id.clone())
        .collect();
    for id in asked {
        commits.decide(&id, Answer::Publish);
    }
    let sent = ops(&mut commits, &mut trees, &mut ids);
    assert!(!sent.contains(&("move", "n0".to_owned())), "{sent:?}");
}

#[test]
fn a_file_whose_mark_names_an_id_a_place_match_frees_is_not_settled_as_its_own_id() {
    let (trees, _, _) = followed(
        &[
            ("d0", dir(ROOT, "p")),
            ("n2", file("d0", "a", 7)),
            ("n1", file("d0", "b", 2)),
        ],
        &[],
        &[
            live("d0", dir(ROOT, "q")),
            live("n5", file("d0", "a", 7)),
            live("n1", file("d0", "b", 2)),
            live("n2", file(ROOT, "z", 1)),
        ],
        &[
            ("d0", dir(ROOT, "p")),
            ("d0", dir(ROOT, "q")),
            ("n2", file(ROOT, "z", 2)),
        ],
        &[("d0", "d0"), ("n1", "n2")],
    );
    assert_eq!(trees.synced().live("n1"), None);
    assert_eq!(trees.local().get("n1"), None);
}

#[test]
fn a_file_whose_mark_names_a_trashed_node_keeps_its_id_under_a_folder_renamed_here() {
    let (trees, commits, _) = followed(
        &[("f", dir(ROOT, "p")), ("n1", file("f", "b", 2))],
        &[],
        &[
            live("f", dir(ROOT, "q")),
            live("n1", file("f", "b", 2)),
            Remote("n2", file(ROOT, "a", 2), true, 1),
        ],
        &[("n2", file(ROOT, "a", 2))],
        &[("n1", "n2")],
    );
    assert!(trees.local().get("n1").is_some());
    assert!(
        commits
            .questions()
            .iter()
            .all(|question| question.holds().all(|id| id != "n1"))
    );
}

#[test]
fn publish_moves_no_server_file_out_of_a_folder_whose_mark_names_a_trashed_node() {
    let (mut trees, mut commits, mut ids) = followed(
        &[
            ("f", dir(ROOT, "p")),
            ("d1", dir("f", "q")),
            ("n1", file("d1", "b", 1)),
        ],
        &[],
        &[
            live("f", dir(ROOT, "q")),
            live("d1", dir("f", "q")),
            live("n1", file("d1", "b", 1)),
            Remote("d9", dir(ROOT, "r"), true, 1),
        ],
        &[("d9", dir(ROOT, "r"))],
        &[("d1", "d9")],
    );
    let asked: Vec<String> = (commits.questions().iter())
        .map(|question| question.id.clone())
        .collect();
    for id in asked {
        commits.decide(&id, Answer::Publish);
    }
    let sent = ops(&mut commits, &mut trees, &mut ids);
    assert!(!sent.contains(&("move", "n1".to_owned())), "{sent:?}");
}

#[test]
fn a_file_whose_mark_names_another_node_is_not_settled_to_its_own_id_while_the_mark_waits() {
    let (trees, _, _) = followed(
        &[("d0", dir(ROOT, "q")), ("n0", file("d0", "a", 1))],
        &[],
        &[
            live("d0", dir(ROOT, "p")),
            live("n0", file("d0", "a", 1)),
            live("n1", file(ROOT, "b", 1)),
        ],
        &[("n1", file(ROOT, "b", 1))],
        &[("d0", "d0"), ("n0", "n1")],
    );
    assert_eq!(trees.synced().live("n0"), None);
    assert!(trees.waiting().get("n0").is_none());
}

#[test]
fn a_recovery_discard_takes_along_a_server_file_the_rules_let_in_while_the_folder_was_asked_about()
{
    let mut trees = trees(
        &[("f", dir(ROOT, "new")), ("x", file(ROOT, "server", 9))],
        &[("x", file(ROOT, "server", 9))],
        &[live("x", file(ROOT, "server", 9))],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    asked(&commits, RecoveryGroup::CreatedOnThisDevice, &["f"]);
    trees.set_rules(&[] as &[&str], Some(RuleFile::parse(b"ignored\n")));
    let mut moved = trees.local().get("x").unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle();
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    trees.set_rules(&[] as &[&str], None);
    trees.settle();
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let question = asked(&commits, RecoveryGroup::CreatedOnThisDevice, &["f"]);
    assert!(commits.decide(&question.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let done = effects(&mut trees, &mut ids);
    assert!(trees.local().get("f").is_none(), "{done:?}");
    assert_eq!(local_shape(&trees, "x"), Some(&file(ROOT, "server", 9)));
}

#[test]
fn a_file_recovered_by_its_mark_and_moved_in_while_left_out_is_taken_along_with_the_server_s_edit()
{
    let (mut trees, mut commits, mut ids) = recovered_by_marks(
        &[
            ("new", dir(ROOT, "new")),
            ("e", file("new", "e", 4)),
            ("entry", file(ROOT, "server", 9)),
        ],
        &[],
        &[live("n", file(ROOT, "server", 9))],
        &[("n", file(ROOT, "server", 9))],
        &[("entry", "n")],
    );
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    assert_eq!(local_shape(&trees, "n"), Some(&file(ROOT, "server", 9)));
    let question = asked(&commits, RecoveryGroup::CreatedOnThisDevice, &["e", "new"]);
    trees.set_rules(&[] as &[&str], Some(RuleFile::parse(b"ignored\n")));
    trees.settle();
    let mut moved = trees.local().get("n").unwrap().clone();
    moved.shape.parent = "new".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("n".into(), moved);
    trees.settle();
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let mut edited = trees.remote().get("n").unwrap().clone();
    edited.shape = file(ROOT, "server", 10);
    edited.content_rev = 2;
    edited.edit_rev = 2;
    trees.set_remote("n".into(), edited);
    trees.settle();
    trees.set_rules(&[] as &[&str], None);
    trees.settle();
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let question = (commits.questions().iter())
        .find(|asked| asked.holds().any(|id| id == "new"))
        .unwrap_or_else(|| panic!("{question:?} {:?}", commits.questions()))
        .clone();
    assert!(commits.decide(&question.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    effects(&mut trees, &mut ids);
    trees.settle();
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    assert!(trees.local().get("new").is_none());
    assert_eq!(local_shape(&trees, "n"), Some(&file(ROOT, "server", 10)));
}

#[test]
fn a_file_a_rewind_finds_with_the_server_s_bytes_and_moved_in_while_left_out_is_taken_along_with_the_server_s_edit()
 {
    let (mut trees, mut commits, mut ids) = recovered_by_marks(
        &[
            ("new", dir(ROOT, "new")),
            ("e", file("new", "e", 4)),
            ("n", file(ROOT, "server", 9)),
        ],
        &[("n", file(ROOT, "server", 8))],
        &[live("n", file(ROOT, "server", 9))],
        &[],
        &[],
    );
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    assert_eq!(local_shape(&trees, "n"), Some(&file(ROOT, "server", 9)));
    let question = asked(&commits, RecoveryGroup::CreatedOnThisDevice, &["e", "new"]);
    trees.set_rules(&[] as &[&str], Some(RuleFile::parse(b"ignored\n")));
    trees.settle();
    let mut moved = trees.local().get("n").unwrap().clone();
    moved.shape.parent = "new".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("n".into(), moved);
    trees.settle();
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let mut edited = trees.remote().get("n").unwrap().clone();
    edited.shape = file(ROOT, "server", 10);
    edited.content_rev = 2;
    edited.edit_rev = 2;
    trees.set_remote("n".into(), edited);
    trees.settle();
    trees.set_rules(&[] as &[&str], None);
    trees.settle();
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    let question = (commits.questions().iter())
        .find(|asked| asked.holds().any(|id| id == "new"))
        .unwrap_or_else(|| panic!("{question:?} {:?}", commits.questions()))
        .clone();
    assert!(commits.decide(&question.id, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    effects(&mut trees, &mut ids);
    trees.settle();
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    assert!(trees.local().get("new").is_none());
    assert_eq!(local_shape(&trees, "n"), Some(&file(ROOT, "server", 10)));
}
