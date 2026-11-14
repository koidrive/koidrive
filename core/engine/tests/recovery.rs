// SPDX-License-Identifier: Apache-2.0

use koidrive_engine::classify::{Class, Origin, Trees, Waiting};
use koidrive_engine::commit::{Base, Commits, Failure, Halt, Reconnect};
use koidrive_engine::effects::{BatchLimit, Effect, Effects};
use koidrive_engine::first_link::History;
use koidrive_engine::ids::IdGen;
use koidrive_engine::ignore::RuleFile;
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
            shape: shape.clone(),
            identity: FileIdentity {
                dev: 1,
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
            shape: shape.clone(),
            trashed: *trashed,
            content_rev: *rev,
            edit_rev: *rev,
            meta_rev: *rev,
            meta_edit_rev: *rev,
            trash_group: String::new(),
            version_id: String::new(),
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

fn rewind() -> RecoveryCause {
    RecoveryCause::rewind(4, NOW as i64)
}

fn recovered(trees: &mut Trees, device_seq: u64) -> (Commits, IdGen) {
    let mut commits = Commits::default();
    let mut ids = IdGen::new(7);
    commits.enter_recovery(rewind());
    assert_eq!(commits.build(trees, &mut ids, NOW, OLD, 9), None);
    commits.recover(trees, device_seq, &rewind(), &History::new(), &mut ids, NOW);
    trees.settle();
    (commits, ids)
}

fn ops(commits: &mut Commits, trees: &mut Trees, ids: &mut IdGen) -> Vec<(&'static str, String)> {
    let Some(commit) = commits.build(trees, ids, NOW, NEW, 1) else {
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
            dev: 1,
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
    assert_eq!(commits.failed(&moved), Ok(Failure::Abandoned));
    trees.set_remote(
        "n".into(),
        RemoteNode {
            shape: file(ROOT, "a", 5),
            trashed: false,
            content_rev: 2,
            edit_rev: 2,
            meta_rev: 2,
            meta_edit_rev: 2,
            trash_group: String::new(),
            version_id: String::new(),
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
    assert_eq!(commits.failed(&moved), Ok(Failure::Abandoned));
    trees.set_remote(
        "n".into(),
        RemoteNode {
            shape: file(ROOT, "a", 5),
            trashed: false,
            content_rev: 2,
            edit_rev: 2,
            meta_rev: 2,
            meta_edit_rev: 2,
            trash_group: String::new(),
            version_id: String::new(),
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
            shape: file(ROOT, "n", 2),
            trashed: false,
            content_rev: 1,
            edit_rev: 1,
            meta_rev: 1,
            meta_edit_rev: 1,
            trash_group: String::new(),
            version_id: String::new(),
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
        shape: file(ROOT, "c.txt", 1),
        identity,
        parked: false,
    };
    trees.set_local("moved".into(), moved);
    let mine = LocalNode {
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
            shape,
            trashed: false,
            content_rev: 2,
            edit_rev: 2,
            meta_rev: 2,
            meta_edit_rev: 2,
            trash_group: String::new(),
            version_id: String::new(),
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
                shape: published.clone(),
                trashed: false,
                content_rev: 2,
                edit_rev: 2,
                meta_rev: 2,
                meta_edit_rev: 2,
                trash_group: String::new(),
                version_id: String::new(),
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
    let sent = commits.build(&mut trees, &mut ids, NOW, OLD, 1).cloned();
    assert_eq!(sent.as_ref().map(|commit| commit.device_seq), Some(1));
    let welcome = Welcome {
        history_id: OLD.into(),
        device_seq: 4,
        ..Default::default()
    };
    assert_eq!(commits.connected(&welcome), Reconnect::Recovery);
    commits.recover(&mut trees, 4, &rewind(), &History::new(), &mut ids, NOW);
    trees.settle();
    assert_eq!(commits.in_flight(), None);
    assert_eq!(commits.accepted(), 4);
    let question = asked(&commits, RecoveryGroup::ChangedOnThisDevice, &["n"]);
    assert!(commits.decide(&question.id, Answer::Publish));
    let commit = commits.build(&mut trees, &mut ids, NOW, NEW, Base::default());
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
    assert!(commits.build(&mut trees, &mut ids, NOW, OLD, 1).is_some());
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
    commits.recover(&mut trees, 0, &rewind(), &History::new(), &mut ids, NOW);
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
        commits.recover(&mut trees, 0, &rewind(), &History::new(), &mut ids, NOW);
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
    commits.recover(&mut trees, 0, &rewind(), &History::new(), &mut ids, NOW);
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
    commits.recover(&mut trees, 0, &rewind(), &History::new(), &mut ids, NOW);
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
            shape: dir(ROOT, "a"),
            trashed: false,
            content_rev: 1,
            edit_rev: 1,
            meta_rev: 1,
            meta_edit_rev: 1,
            trash_group: String::new(),
            version_id: String::new(),
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
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    assert_eq!(commits.questions(), []);
    assert_eq!(
        ops(&mut commits, &mut trees, &mut ids),
        [("move", "p".into())]
    );
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
        shape: file(ROOT, "A", 2),
        trashed: false,
        content_rev: 1,
        edit_rev: 1,
        meta_rev: 2,
        meta_edit_rev: 2,
        trash_group: String::new(),
        version_id: String::new(),
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
    commits.first_link(&mut trees, &mut ids, NOW);
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
    commits.first_link(&mut trees, &mut ids, NOW);
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
    commits.recover(&mut trees, 0, &rewind(), &History::new(), &mut ids, NOW);
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
    assert_eq!(commits.failed(&moved), Ok(Failure::Abandoned));
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
    assert_eq!(commits.failed(&limit), Ok(Failure::Split));
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
            shape,
            trashed,
            content_rev: 1,
            edit_rev: 1,
            meta_rev: 2,
            meta_edit_rev: 2,
            trash_group: String::new(),
            version_id: String::new(),
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
    assert_eq!(commits.failed(&limit), Ok(Failure::Split));
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
    assert!(commits.build(&mut trees, &mut ids, NOW, OLD, 1).is_some());
    let mismatch = Error {
        code: error::Code::HistoryMismatch as i32,
        ..Default::default()
    };
    assert!(matches!(
        commits.failed(&mismatch),
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
