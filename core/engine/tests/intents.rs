// SPDX-License-Identifier: Apache-2.0

use koidrive_engine::classify::{Origin, Trees, Waiting};
use koidrive_engine::commit::{Base, Commits, InFlight};
use koidrive_engine::effects::{BatchLimit, Effects};
use koidrive_engine::ids::IdGen;
use koidrive_engine::ignore::{RuleFile, Source};
use koidrive_engine::journal::{
    Author, Field, FieldIntent, HistoryEntry, IntentId, NodeIntents, OpsWithoutIntent, Place, Row,
    RowState, Value, Wire, WireBase,
};
use koidrive_engine::state::{
    Agreed, CommitsState, ContentValue, MoveHistory, MovePosition, MovedHere,
};
use koidrive_engine::tree::{
    ContentKey, FileIdentity, Kind, LocalNode, NodeId, RemoteNode, Shape, Tree,
};
use koidrive_proto::v1::{self as proto, Welcome, op, op_result};

const ROOT: &str = "root";

fn content(digest: u8) -> ContentKey {
    ContentKey::try_from(&proto::Content {
        size: 3,
        manifest_digest: vec![digest; 32],
        ..Default::default()
    })
    .unwrap()
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

fn remote(shape: Shape, rev: u64) -> RemoteNode {
    RemoteNode {
        version_id: match shape.kind {
            Kind::File => format!("v{rev}"),
            Kind::Dir => String::new(),
        },
        shape,
        trashed: false,
        content_rev: rev,
        meta_rev: rev,
        meta_edit_rev: rev,
        trash_group: String::new(),
        made_over: Vec::new(),
        edit_rev: rev,
        pending_for: None,
        pending_by: None,
        chunks: Box::default(),
        intents: None,
    }
}

fn local(shape: Shape) -> LocalNode {
    LocalNode {
        left_out_when_placed: false,
        content_from_server: false,
        shape,
        identity: FileIdentity {
            ino: 7,
            birth_ns: Some(0),
            handle: None,
        },
        parked: false,
        chunks: Box::default(),
    }
}

fn id(n: u8) -> IntentId {
    IntentId::from_bytes(&[n; 16]).unwrap()
}

fn held(trees: &Trees, node: &str, field: Field) -> IntentId {
    let remote = trees.remote().get(&node.into()).unwrap();
    remote.intents.as_ref().unwrap().get(field).unwrap().id
}

fn synced(trees: &mut Trees, node: &str, shape: Shape, rev: u64) {
    trees.set_remote(node.into(), remote(shape.clone(), rev));
    trees.set_synced(node.into(), Some(shape.clone()));
    trees.set_local(node.into(), local(shape));
}

#[test]
fn a_node_sent_without_intents_gets_legacy_ids_and_keeps_its_live_id_while_live() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    trees.set_remote("f".into(), remote(file(ROOT, "Café.txt", 1), 3));
    assert_eq!(
        trees.remote().get(&"f".into()).unwrap().intents,
        Some(NodeIntents {
            place: Some(FieldIntent::legacy(IntentId::legacy_place(
                "f",
                3,
                ROOT,
                "Café.txt"
            ))),
            content: Some(FieldIntent::legacy(IntentId::legacy_content("f", "v3"))),
            status: Some(FieldIntent::legacy(IntentId::legacy_live("f", 3))),
            ..Default::default()
        })
    );
    trees.set_remote("f".into(), remote(file(ROOT, "b.txt", 1), 5));
    assert_eq!(
        held(&trees, "f", Field::Place),
        IntentId::legacy_place("f", 5, ROOT, "b.txt")
    );
    assert_eq!(
        held(&trees, "f", Field::Status),
        IntentId::legacy_live("f", 3)
    );
    trees.set_remote(
        "f".into(),
        RemoteNode {
            trashed: true,
            trash_group: "6:0".into(),
            ..remote(file(ROOT, "b.txt", 1), 6)
        },
    );
    assert_eq!(
        held(&trees, "f", Field::Status),
        IntentId::legacy_trashed("f", "6:0")
    );
    trees.set_listed_remote("g".into(), remote(file(ROOT, "g", 1), 2));
    assert_eq!(
        held(&trees, "g", Field::Place),
        IntentId::legacy_place("g", 2, ROOT, "g")
    );
    trees.set_remote("d".into(), remote(dir(ROOT, "d"), 1));
    assert_eq!(
        trees
            .remote()
            .get(&"d".into())
            .unwrap()
            .intents
            .as_ref()
            .unwrap()
            .content,
        None
    );
}

#[test]
fn r_keeps_the_intents_the_server_sent() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    let sent = NodeIntents {
        place: Some(FieldIntent {
            id: id(1),
            author: "d1".into(),
            replay: true,
            applied_at_ms: 40,
        }),
        content: None,
        status: Some(FieldIntent::legacy(id(2))),
        ..Default::default()
    };
    trees.set_remote(
        "f".into(),
        RemoteNode {
            intents: Some(sent.clone()),
            ..remote(file(ROOT, "a", 1), 1)
        },
    );
    assert_eq!(trees.remote().get(&"f".into()).unwrap().intents, Some(sent));
}

#[test]
fn the_history_follows_r_until_r_changes_and_keeps_a_downloaded_value_marked() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    trees.set_remote("f".into(), remote(file(ROOT, "a", 1), 1));
    let first = held(&trees, "f", Field::Content);
    assert!(
        trees
            .journal()
            .history(&"f".into(), Field::Content)
            .is_empty()
    );
    let entry = |intent, in_local| HistoryEntry {
        intent,
        applied_at_ms: 0,
        in_local,
    };
    assert_eq!(
        trees.remote_history(&"f".into(), Field::Content),
        [entry(first, false)]
    );
    trees.set_local("f".into(), local(file(ROOT, "a", 1)));
    assert_eq!(
        trees.remote_history(&"f".into(), Field::Content),
        [entry(first, true)]
    );
    trees.set_remote("f".into(), remote(file(ROOT, "a", 2), 2));
    let second = held(&trees, "f", Field::Content);
    assert_eq!(
        trees.journal().history(&"f".into(), Field::Content),
        [entry(first, true)]
    );
    assert_eq!(
        trees.remote_history(&"f".into(), Field::Content),
        [entry(first, true), entry(second, false)]
    );
}

#[test]
fn an_edit_writes_a_content_row_over_r_s_intent_and_keeps_r_s_value_in_the_history() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    synced(&mut trees, "f", file(ROOT, "a", 1), 1);
    let seen = held(&trees, "f", Field::Content);
    trees.set_local_changed("f".into(), local(file(ROOT, "a", 2)));
    let row = trees.journal_row(&"f".into(), Field::Content).unwrap();
    assert_eq!(row.base, Some(seen));
    assert_eq!(
        row.value,
        Value::Content {
            version_id: None,
            value: ContentValue::from(&content(2)),
        }
    );
    assert_eq!(row.state, RowState::Local { flight: None });
    assert!(trees.journal_row(&"f".into(), Field::Place).is_none());
    assert!(trees.journal_row(&"f".into(), Field::Status).is_none());
    assert_eq!(
        trees.journal().history(&"f".into(), Field::Content),
        [HistoryEntry {
            intent: seen,
            applied_at_ms: 0,
            in_local: true,
        }]
    );
}

#[test]
fn a_move_writes_a_place_row_that_sends_the_place_it_left() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    synced(&mut trees, "d", dir(ROOT, "d"), 1);
    synced(&mut trees, "f", file(ROOT, "a", 1), 1);
    let seen = held(&trees, "f", Field::Place);
    trees.set_local_changed("f".into(), local(file("d", "b", 1)));
    let row = trees.journal_row(&"f".into(), Field::Place).unwrap();
    assert_eq!(row.base, Some(seen));
    let at = |parent: &str, name: &str| Place {
        parent: parent.into(),
        name: name.into(),
        mark: None,
    };
    assert_eq!(row.value, Value::Place(at("d", "b")));
    assert_eq!(row.wire_base, WireBase::Place(at(ROOT, "a")));
    assert!(trees.journal_row(&"f".into(), Field::Content).is_none());
}

#[test]
fn a_scan_that_finds_what_s_holds_writes_no_row() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    trees.set_remote("f".into(), remote(file(ROOT, "a", 1), 1));
    trees.set_synced("f".into(), Some(file(ROOT, "a", 1)));
    trees.set_local_changed("f".into(), local(file(ROOT, "a", 1)));
    assert_eq!(trees.journal_rows().count(), 0);
}

#[test]
fn a_create_writes_a_row_for_each_of_its_fields_and_a_delete_before_any_send_ends_them() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    trees.set_local_changed("f".into(), local(file(ROOT, "a", 1)));
    let fields: Vec<Field> = (trees.journal_rows())
        .map(|((_, field), _)| field)
        .collect();
    assert_eq!(fields, [Field::Place, Field::Content, Field::Status]);
    assert!(trees.journal_rows().all(|(_, row)| row.base.is_none()));
    trees.remove_local_by_user(&"f".into());
    assert_eq!(trees.journal_rows().count(), 0);
    assert!(trees.local().get(&"f".into()).is_none());
}

#[test]
fn a_child_the_rules_leave_out_gets_no_status_row_when_its_folder_is_deleted() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    synced(&mut trees, "d", dir(ROOT, "d"), 1);
    synced(&mut trees, "f", file("d", "c.txt", 1), 1);
    let rules = RuleFile::parse(b"*.txt\n");
    trees.set_rules(&[] as &[&str; 0], Source::Koiignore, Some(rules));
    trees.settle(&Default::default());
    assert!(trees.unobserved(&"f".into()));
    trees.remove_local_by_user(&"f".into());
    trees.remove_local_by_user(&"d".into());
    assert!(trees.journal_row(&"f".into(), Field::Status).is_none());
    let deleted = trees
        .journal_row(&"d".into(), Field::Status)
        .map(|row| row.value.clone());
    assert_eq!(deleted, Some(Value::Status { live: false }));
}

#[test]
fn a_removal_the_new_rules_leave_out_before_a_settle_records_no_delete() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    synced(&mut trees, "f", file(ROOT, "c.txt", 1), 1);
    trees.rules_scanned();
    trees.settle(&Default::default());
    let rules = RuleFile::parse(b"*.txt\n");
    trees.set_rules(&[] as &[&str; 0], Source::Koiignore, Some(rules));
    assert!(!trees.unobserved(&"f".into()));
    trees.remove_local_by_user(&"f".into());
    assert!(trees.journal_row(&"f".into(), Field::Status).is_none());
    assert!(trees.journal().seen(&"f".into()).is_empty());
}

#[test]
fn a_delete_of_a_folder_whose_create_is_in_flight_sees_the_create() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    trees.set_local_changed("d".into(), local(dir(ROOT, "d")));
    build(&mut trees, &["field_intents"], 1_700);
    let reported = proto::FieldIntents {
        place: Some(wire(sent(&trees, "d", Field::Place), 1_700)),
        content: None,
        status: Some(wire(sent(&trees, "d", Field::Status), 1_700)),
    };
    trees.remove_local_by_user(&"d".into());
    let state = node_state("d", &dir(ROOT, "d"), 1, Some(reported));
    trees.set_remote("d".into(), RemoteNode::from_state(&state).unwrap().1);
    let mut ops = vec![proto::Op {
        op: Some(op::Op::Delete(proto::Delete {
            node_id: "d".into(),
            ..Default::default()
        })),
        ..Default::default()
    }];
    trees.stamp_intents(&mut ops, "me", 0, Wire::WithIntents);
    let Some(op::Op::Delete(delete)) = &ops[0].op else {
        unreachable!();
    };
    assert!(delete.unseen.is_empty(), "{:?}", delete.unseen);
}

#[test]
fn a_delete_sees_its_files_create_that_a_rename_replaced_in_flight() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    trees.set_local_changed("f".into(), local(file(ROOT, "a", 1)));
    build(&mut trees, &["field_intents"], 1_700);
    let created = sent(&trees, "f", Field::Place);
    trees.set_local_changed("f".into(), local(file(ROOT, "b", 1)));
    trees.settle(&Default::default());
    trees.remove_local_by_user(&"f".into());
    let reported = proto::FieldIntents {
        place: Some(wire(created, 1_700)),
        content: Some(wire(sent(&trees, "f", Field::Content), 1_700)),
        status: None,
    };
    let state = node_state("f", &file(ROOT, "a", 1), 1, Some(reported));
    trees.set_remote("f".into(), RemoteNode::from_state(&state).unwrap().1);
    let mut ops = vec![proto::Op {
        op: Some(op::Op::Delete(proto::Delete {
            node_id: "f".into(),
            ..Default::default()
        })),
        ..Default::default()
    }];
    trees.stamp_intents(&mut ops, "me", 0, Wire::WithIntents);
    let Some(op::Op::Delete(delete)) = &ops[0].op else {
        unreachable!();
    };
    assert!(delete.unseen.is_empty(), "{:?}", delete.unseen);
}

#[test]
fn a_delete_sees_its_files_create_sent_into_the_folder_l_holds_that_its_folder_was_merged_into() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    synced(&mut trees, "t", dir(ROOT, "t"), 1);
    trees.set_local_changed("d".into(), local(dir(ROOT, "d")));
    let mut commits = Commits::from_state(CommitsState::default()).unwrap();
    commits.connected(&Welcome {
        features: vec!["field_intents".into()],
        ..Default::default()
    });
    trees.rules_scanned();
    let mut ids = IdGen::new(1);
    let base = |seq| Base {
        seq,
        commit_id: String::new(),
        committed_at_ms: 1_700,
    };
    trees.settle(commits.aliases());
    commits.build(&mut trees, &mut ids, 0, "history", "me", base(0));
    trees.set_local_changed("c".into(), local(file("d", "c", 1)));
    let merged = proto::OpResult {
        node_id: "t".into(),
        ..outcome(op_result::Code::MergedInto)
    };
    (commits.received(proto::CommitResult {
        seq: 1,
        results: vec![merged],
        ..Default::default()
    }))
    .unwrap();
    assert!(commits.acknowledge(&mut trees, 1));
    trees.settle(commits.aliases());
    let commit = (commits
        .build(&mut trees, &mut ids, 0, "history", "me", base(1))
        .cloned())
    .unwrap();
    let [
        proto::Op {
            op: Some(op::Op::CreateFile(create)),
            ..
        },
    ] = &commit.ops[..]
    else {
        panic!("{:?}", commit.ops);
    };
    assert_eq!(create.parent_id, "t");
    trees.remove_local_by_user(&"c".into());
    let reported = proto::FieldIntents {
        place: Some(wire(sent(&trees, "c", Field::Place), 1_700)),
        content: Some(wire(sent(&trees, "c", Field::Content), 1_700)),
        status: Some(wire(sent(&trees, "c", Field::Status), 1_700)),
    };
    let state = node_state("c", &file("t", "c", 1), 1, Some(reported));
    trees.set_remote("c".into(), RemoteNode::from_state(&state).unwrap().1);
    let mut ops = vec![proto::Op {
        op: Some(op::Op::Delete(proto::Delete {
            node_id: "c".into(),
            ..Default::default()
        })),
        ..Default::default()
    }];
    trees.stamp_intents(&mut ops, "me", 0, Wire::WithIntents);
    let Some(op::Op::Delete(delete)) = &ops[0].op else {
        unreachable!();
    };
    assert!(delete.unseen.is_empty(), "{:?}", delete.unseen);
}

#[test]
fn a_folders_delete_sees_a_child_created_in_flight_and_not_a_child_another_user_added() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    synced(&mut trees, "d", dir(ROOT, "d"), 1);
    trees.set_local_changed("f".into(), local(file("d", "a", 1)));
    build(&mut trees, &["field_intents"], 1_700);
    let reported = proto::FieldIntents {
        place: Some(wire(sent(&trees, "f", Field::Place), 1_700)),
        content: Some(wire(sent(&trees, "f", Field::Content), 1_700)),
        status: Some(wire(sent(&trees, "f", Field::Status), 1_700)),
    };
    trees.remove_local_by_user(&"f".into());
    trees.remove_local_by_user(&"d".into());
    let state = node_state("f", &file("d", "a", 1), 2, Some(reported));
    trees.set_remote("f".into(), RemoteNode::from_state(&state).unwrap().1);
    trees.set_remote("g".into(), remote(file("d", "g", 1), 2));
    let mut ops = vec![proto::Op {
        op: Some(op::Op::Delete(proto::Delete {
            node_id: "d".into(),
            ..Default::default()
        })),
        ..Default::default()
    }];
    trees.stamp_intents(&mut ops, "me", 0, Wire::WithIntents);
    let Some(op::Op::Delete(delete)) = &ops[0].op else {
        unreachable!();
    };
    let mut unseen: Vec<IntentId> = (delete.unseen.iter())
        .map(|id| IntentId::from_bytes(id).unwrap())
        .collect();
    unseen.sort();
    let mut expected: Vec<IntentId> = (Field::ALL.into_iter())
        .map(|field| held(&trees, "g", field))
        .collect();
    expected.sort();
    assert_eq!(unseen, expected);
}

#[test]
fn a_folders_delete_sees_a_child_its_user_deleted_before_a_restore_brings_it_back() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    synced(&mut trees, "d", dir(ROOT, "d"), 1);
    synced(&mut trees, "f", file("d", "a", 1), 1);
    trees.remove_local_by_user(&"f".into());
    let live = trees.remote().get(&"f".into()).unwrap().clone();
    trees.set_remote(
        "f".into(),
        RemoteNode {
            trashed: true,
            ..live.clone()
        },
    );
    trees.remove_local_by_user(&"d".into());
    trees.set_remote("f".into(), live);
    let mut ops = vec![proto::Op {
        op: Some(op::Op::Delete(proto::Delete {
            node_id: "d".into(),
            ..Default::default()
        })),
        ..Default::default()
    }];
    trees.stamp_intents(&mut ops, "me", 0, Wire::WithIntents);
    let Some(op::Op::Delete(delete)) = &ops[0].op else {
        unreachable!();
    };
    assert!(delete.unseen.is_empty(), "{:?}", delete.unseen);
}

#[test]
fn a_delete_lists_what_l_held_for_the_node_and_below_it() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    synced(&mut trees, "d", dir(ROOT, "d"), 1);
    synced(&mut trees, "f", file("d", "a", 1), 1);
    trees.set_remote("f".into(), remote(file("d", "a", 2), 2));
    let old = IntentId::legacy_content("f", "v1");
    trees.remove_local_by_user(&"d".into());
    let row = trees.journal_row(&"d".into(), Field::Status).unwrap();
    assert_eq!(row.value, Value::Status { live: false });
    assert_eq!(row.base, Some(held(&trees, "d", Field::Status)));
    let listed: Vec<(NodeId, Field, IntentId, bool)> = (trees.journal().seen(&"d".into()).iter())
        .map(|entry| {
            (
                entry.node.clone(),
                entry.field,
                entry.intent,
                entry.value.is_some(),
            )
        })
        .collect();
    assert_eq!(
        listed,
        [
            (
                NodeId::from("d"),
                Field::Place,
                held(&trees, "d", Field::Place),
                true
            ),
            (
                NodeId::from("d"),
                Field::Status,
                held(&trees, "d", Field::Status),
                true
            ),
            (
                NodeId::from("f"),
                Field::Place,
                IntentId::legacy_place("f", 1, "d", "a"),
                false
            ),
            (
                NodeId::from("f"),
                Field::Place,
                held(&trees, "f", Field::Place),
                true
            ),
            (NodeId::from("f"), Field::Content, old, true),
            (
                NodeId::from("f"),
                Field::Status,
                held(&trees, "f", Field::Status),
                true
            ),
        ]
    );
}

#[test]
fn a_delete_after_r_drops_the_node_still_sees_the_value_l_took_from_r() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    let f = NodeId::from("f");
    synced(&mut trees, "f", file(ROOT, "f", 1), 1);
    let a = held(&trees, "f", Field::Content);
    assert!(trees.journal().history(&f, Field::Content).is_empty());
    trees.remove_remote(&f);
    trees.set_local_changed(f.clone(), local(file(ROOT, "f", 2)));
    trees.remove_local_by_user(&f);
    assert!(
        trees
            .journal()
            .seen(&f)
            .iter()
            .any(|entry| entry.intent == a)
    );
}

#[test]
fn s_keeps_the_intents_of_the_values_it_holds_when_r_moves_on_or_loses_the_node() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    synced(&mut trees, "f", file(ROOT, "a", 1), 1);
    let content = |trees: &Trees| {
        trees
            .synced_intents(&"f".into())
            .unwrap()
            .content
            .map(|intent| intent.id)
    };
    let first = held(&trees, "f", Field::Content);
    assert_eq!(content(&trees), Some(first));
    trees.take_changed();
    trees.set_remote("f".into(), remote(file(ROOT, "a", 2), 2));
    assert!(trees.take_changed().synced.contains(&"f".into()));
    assert_eq!(content(&trees), Some(first));
    trees.set_synced("f".into(), Some(file(ROOT, "a", 2)));
    assert_eq!(content(&trees), Some(held(&trees, "f", Field::Content)));
    let before = trees.synced_intents(&"f".into());
    trees.remove_remote(&"f".into());
    assert_eq!(trees.synced_intents(&"f".into()), before);
}

#[test]
fn s_taking_a_waiting_update_that_r_and_the_journal_lack_drops_its_old_intent() {
    let mut t = Trees::new(ROOT, &IdGen::new(0));
    let n = NodeId::from("f");
    synced(&mut t, "b", dir(ROOT, "b"), 1);
    synced(&mut t, "f", file(ROOT, "a", 1), 1);
    let blocker = NodeId::from("g");
    t.set_synced(blocker.clone(), Some(file("b", "a", 5)));
    let old = held(&t, "f", Field::Content);
    let waiting = file("b", "a", 2);
    t.set_remote(n.clone(), remote(waiting.clone(), 2));
    let mut state = t.state();
    state.waiting.insert(
        n.clone(),
        Waiting {
            shape: Some(waiting.clone()),
            origin: Origin::Commit(1),
        },
    );
    t.restore(state).unwrap();
    t.set_remote(n.clone(), remote(file("b", "a", 3), 3));
    t.set_local_changed(n.clone(), local(file("b", "a", 4)));
    t.set_synced(blocker, None);
    assert!(t.set_synced_if_valid(n.clone(), Some(waiting)));
    assert_ne!(
        t.synced_intents(&n).unwrap().content.map(|i| i.id),
        Some(old)
    );
}

#[test]
fn s_taking_a_marked_remote_place_takes_that_places_intent() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    let f = NodeId::from("f");
    synced(&mut trees, "f", file(ROOT, "a", 1), 1);
    let moved = file(ROOT, "b (pending)", 1);
    trees.set_remote(
        f.clone(),
        RemoteNode {
            pending_for: Some("b".into()),
            ..remote(file(ROOT, "b (pending)", 2), 2)
        },
    );
    let wanted = held(&trees, "f", Field::Place);
    trees.set_synced(f.clone(), Some(moved));
    assert_eq!(trees.synced_mark(&f), Some("b"));
    assert_eq!(trees.synced_intents(&f).unwrap().place.unwrap().id, wanted);
}

#[test]
fn a_remote_change_of_only_the_pending_mark_gives_s_the_new_place_intent() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    let f = NodeId::from("f");
    let shape = file(ROOT, "stand-in", 1);
    synced(&mut trees, "f", shape, 1);
    let mut changed = trees.remote().get(&f).unwrap().clone();
    changed.pending_for = Some("wanted".into());
    changed.intents.as_mut().unwrap().place = Some(FieldIntent::legacy(id(42)));
    trees.set_remote(f.clone(), changed);
    assert_eq!(trees.synced_mark(&f), Some("wanted"));
    assert_eq!(trees.synced_intents(&f).unwrap().place.unwrap().id, id(42));
}

fn build(trees: &mut Trees, features: &[&str], committed_at_ms: i64) -> proto::Commit {
    let mut commits = Commits::from_state(CommitsState::default()).unwrap();
    commits.connected(&Welcome {
        features: features
            .iter()
            .map(|feature| (*feature).to_owned())
            .collect(),
        ..Default::default()
    });
    trees.rules_scanned();
    trees.settle(&Default::default());
    let base = Base {
        seq: 0,
        commit_id: String::new(),
        committed_at_ms,
    };
    let commit = commits.build(trees, &mut IdGen::new(1), 0, "history", "me", base);
    commit.cloned().unwrap()
}

fn intent_of(trees: &Trees, node: &str, field: Field) -> proto::Intent {
    let row = trees.journal_row(&node.into(), field).unwrap();
    proto::Intent {
        id: row.intent.as_bytes().to_vec(),
        author_device_id: "me".into(),
        ..Default::default()
    }
}

#[test]
fn a_commit_names_its_intents_only_when_the_server_announces_them() {
    for (features, named) in [(&[][..], false), (&["field_intents"][..], true)] {
        let mut trees = Trees::new(ROOT, &IdGen::new(0));
        trees.set_local_changed("f".into(), local(file(ROOT, "a", 1)));
        let commit = build(&mut trees, features, 1_700);
        let [create] = &commit.ops[..] else {
            panic!("{commit:?}");
        };
        assert!(matches!(create.op, Some(op::Op::CreateFile(_))));
        for field in Field::ALL {
            let row = trees.journal_row(&"f".into(), field).unwrap();
            assert_eq!(row.state, RowState::Sent);
            assert_eq!(row.time_ms, 1_700);
        }
        match named {
            false => assert_eq!((&create.intent, &create.fields), (&None, &None)),
            true => {
                assert_eq!(create.intent, Some(intent_of(&trees, "f", Field::Place)));
                assert_eq!(
                    create.fields,
                    Some(proto::FieldIntents {
                        place: Some(intent_of(&trees, "f", Field::Place)),
                        content: Some(intent_of(&trees, "f", Field::Content)),
                        status: Some(intent_of(&trees, "f", Field::Status)),
                    })
                );
            }
        }
    }
}

#[test]
fn an_op_no_row_covers_gets_an_own_sent_row_over_r_s_intent() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    synced(&mut trees, "f", file(ROOT, "a", 1), 1);
    let seen = held(&trees, "f", Field::Place);
    trees.set_local("f".into(), local(file(ROOT, "b", 1)));
    assert!(trees.journal_row(&"f".into(), Field::Place).is_none());
    let commit = build(&mut trees, &["field_intents"], 9);
    assert!(matches!(&commit.ops[..], [op] if matches!(op.op, Some(op::Op::Move(_)))));
    let row = trees.journal_row(&"f".into(), Field::Place).unwrap();
    assert_eq!(
        (row.base, &row.state, row.time_ms),
        (Some(seen), &RowState::Sent, 9)
    );
    assert_eq!(
        commit.ops[0].intent,
        Some(intent_of(&trees, "f", Field::Place))
    );
}

#[test]
fn an_op_that_carries_another_devices_value_without_a_row_goes_without_an_intent() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    synced(&mut trees, "f", file(ROOT, "a", 1), 1);
    let mut ops = vec![proto::Op {
        op: Some(op::Op::Move(proto::Move {
            node_id: "f".into(),
            new_parent_id: ROOT.into(),
            new_name: "b".into(),
            from: Some(proto::Place {
                parent_id: ROOT.into(),
                name: "a".into(),
            }),
            ..Default::default()
        })),
        ..Default::default()
    }];
    assert_eq!(
        trees.stamp_intents(&mut ops, "me", 0, Wire::WithIntents),
        OpsWithoutIntent(1)
    );
    assert_eq!(ops[0].intent, None);
    assert_eq!(trees.journal_rows().count(), 0);
}

#[test]
fn a_finish_names_the_marks_intent_and_leaves_a_later_rename_to_send() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    let mark = FieldIntent {
        id: id(5),
        author: "me".into(),
        replay: false,
        applied_at_ms: 0,
    };
    let marked = RemoteNode {
        pending_for: Some("d".into()),
        intents: Some(NodeIntents {
            place: Some(mark),
            content: None,
            status: Some(FieldIntent::legacy(id(6))),
            ..Default::default()
        }),
        ..remote(dir(ROOT, "d (pending)"), 1)
    };
    trees.set_remote("d".into(), marked);
    trees.set_synced("d".into(), Some(dir(ROOT, "d (pending)")));
    trees.set_local("d".into(), local(dir(ROOT, "d (pending)")));
    trees.set_local_changed("d".into(), local(dir(ROOT, "x")));
    let mut ops = vec![proto::Op {
        op: Some(op::Op::Move(proto::Move {
            node_id: "d".into(),
            new_parent_id: ROOT.into(),
            new_name: "d".into(),
            from: Some(proto::Place {
                parent_id: ROOT.into(),
                name: "d (pending)".into(),
            }),
            ..Default::default()
        })),
        ..Default::default()
    }];
    assert_eq!(
        trees.stamp_intents(&mut ops, "me", 0, Wire::WithIntents),
        OpsWithoutIntent(0)
    );
    assert_eq!(
        ops[0].intent.as_ref().unwrap().id,
        id(5).as_bytes().to_vec()
    );
    let row = trees.journal_row(&"d".into(), Field::Place).unwrap();
    assert!(matches!(row.state, RowState::Local { .. }));
}

#[test]
fn a_delete_sends_what_r_holds_that_l_never_held_as_unseen() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    synced(&mut trees, "d", dir(ROOT, "d"), 1);
    synced(&mut trees, "f", file("d", "a", 1), 1);
    trees.remove_local_by_user(&"f".into());
    trees.remove_local_by_user(&"d".into());
    trees.set_remote("g".into(), remote(file("d", "g", 1), 2));
    let mut ops = vec![proto::Op {
        op: Some(op::Op::Delete(proto::Delete {
            node_id: "d".into(),
            ..Default::default()
        })),
        ..Default::default()
    }];
    assert_eq!(
        trees.stamp_intents(&mut ops, "me", 0, Wire::WithIntents),
        OpsWithoutIntent(0)
    );
    let Some(op::Op::Delete(delete)) = &ops[0].op else {
        unreachable!();
    };
    let mut unseen: Vec<IntentId> = (delete.unseen.iter())
        .map(|id| IntentId::from_bytes(id).unwrap())
        .collect();
    unseen.sort();
    let mut expected: Vec<IntentId> = (Field::ALL.into_iter())
        .map(|field| held(&trees, "g", field))
        .collect();
    expected.sort();
    assert_eq!(unseen, expected);
    assert!(delete.held.is_empty());
    assert_eq!(ops[0].intent, Some(intent_of(&trees, "d", Field::Status)));
}

#[test]
fn a_delete_sees_the_place_r_holds_that_l_held_before_a_move_no_user_made() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    synced(&mut trees, "d", dir(ROOT, "d"), 1);
    trees.set_local("d".into(), local(dir(ROOT, "e")));
    trees.remove_local_by_user(&"d".into());
    let mut ops = vec![proto::Op {
        op: Some(op::Op::Delete(proto::Delete {
            node_id: "d".into(),
            ..Default::default()
        })),
        ..Default::default()
    }];
    assert_eq!(
        trees.stamp_intents(&mut ops, "me", 0, Wire::WithIntents),
        OpsWithoutIntent(0)
    );
    let Some(op::Op::Delete(delete)) = &ops[0].op else {
        unreachable!();
    };
    assert!(delete.unseen.is_empty(), "{:?}", delete.unseen);
}

fn wire(id: IntentId, applied_at_ms: i64) -> proto::Intent {
    proto::Intent {
        id: id.as_bytes().to_vec(),
        author_device_id: "me".into(),
        applied_at_ms,
        ..Default::default()
    }
}

fn node_state(
    id: &str,
    shape: &Shape,
    rev: u64,
    intents: Option<proto::FieldIntents>,
) -> proto::NodeState {
    proto::NodeState {
        node_id: id.into(),
        parent_id: shape.parent.to_string(),
        name: shape.name.clone(),
        kind: match shape.kind {
            Kind::Dir => proto::NodeKind::Dir,
            Kind::File => proto::NodeKind::File,
        } as i32,
        status: proto::NodeStatus::Live as i32,
        content_rev: rev,
        meta_rev: rev,
        current: shape.content.map(|content| proto::VersionInfo {
            version_id: format!("v{rev}"),
            content: Some(content.content(&[])),
            ..Default::default()
        }),
        intents,
        ..Default::default()
    }
}

fn outcome(code: op_result::Code) -> proto::OpResult {
    proto::OpResult {
        code: code as i32,
        ..Default::default()
    }
}

fn answered(results: Vec<proto::OpResult>, states: Vec<proto::NodeState>) -> proto::CommitResult {
    proto::CommitResult {
        seq: 1,
        results,
        changes: (states.into_iter())
            .map(|state| proto::ChangeRecord {
                change: Some(proto::change_record::Change::Node(state)),
            })
            .collect(),
        committed_at_ms: 2_000,
    }
}

fn sent(trees: &Trees, node: &str, field: Field) -> IntentId {
    trees.journal_row(&node.into(), field).unwrap().intent
}

fn held_as(trees: &Trees, node: &str, field: Field) -> Option<(IntentId, i64)> {
    let row = trees.journal_row(&node.into(), field)?;
    match row.state {
        RowState::Held { held_as, .. } => Some((held_as, row.time_ms)),
        _ => None,
    }
}

#[test]
fn an_applied_result_holds_each_row_as_the_node_state_reports_it() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    let shape = file(ROOT, "a", 1);
    trees.set_local_changed("f".into(), local(shape.clone()));
    let commit = build(&mut trees, &["field_intents"], 1_700);
    let reported = proto::FieldIntents {
        place: Some(wire(sent(&trees, "f", Field::Place), 1_900)),
        content: Some(wire(sent(&trees, "f", Field::Content), 1_900)),
        status: Some(wire(id(7), 1_000)),
    };
    let state = node_state("f", &shape, 1, Some(reported));
    trees.record_results(
        &commit,
        &answered(vec![outcome(op_result::Code::Applied)], vec![state]),
    );
    for field in [Field::Place, Field::Content] {
        assert_eq!(
            held_as(&trees, "f", field),
            Some((sent(&trees, "f", field), 1_900))
        );
    }
    assert_eq!(held_as(&trees, "f", Field::Status), Some((id(7), 1_000)));
    assert!(
        trees
            .journal()
            .history(&"f".into(), Field::Status)
            .iter()
            .any(|entry| entry.intent == id(7))
    );
}

#[test]
fn a_result_without_intents_holds_rows_as_the_legacy_ids_of_its_node_state() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    let shape = file(ROOT, "a", 1);
    trees.set_local_changed("f".into(), local(shape.clone()));
    let commit = build(&mut trees, &[], 1_700);
    let state = node_state("f", &shape, 3, None);
    trees.record_results(
        &commit,
        &answered(vec![outcome(op_result::Code::Applied)], vec![state]),
    );
    let legacy = [
        (Field::Place, IntentId::legacy_place("f", 3, ROOT, "a")),
        (Field::Content, IntentId::legacy_content("f", "v3")),
        (Field::Status, IntentId::legacy_live("f", 3)),
    ];
    for (field, intent) in legacy {
        assert_eq!(held_as(&trees, "f", field), Some((intent, 2_000)));
    }
}

#[test]
fn an_applied_rename_does_not_mark_the_servers_name_as_held_in_l() {
    let mut t = Trees::new(ROOT, &IdGen::new(0));
    synced(&mut t, "f", file(ROOT, "a", 1), 1);
    t.set_local_changed("f".into(), local(file(ROOT, "b", 1)));
    let commit = build(&mut t, &[], 1_700);
    let renamed = file(ROOT, "b (conflict)", 1);
    t.set_remote_made_here("f".into(), remote(renamed.clone(), 2));
    t.record_results(
        &commit,
        &answered(
            vec![proto::OpResult {
                name: renamed.name.clone(),
                ..outcome(op_result::Code::AppliedRenamed)
            }],
            vec![node_state("f", &renamed, 2, None)],
        ),
    );
    let server_place = held(&t, "f", Field::Place);
    assert_eq!(t.local().get(&"f".into()).unwrap().shape.name, "b");
    t.remove_local_by_user(&"f".into());
    assert!(
        !t.journal()
            .seen(&"f".into())
            .iter()
            .any(|e| e.intent == server_place)
    );
}

#[test]
fn an_applied_result_that_r_already_moved_past_ends_the_row() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    let f = NodeId::from("f");
    synced(&mut trees, "f", file(ROOT, "f", 1), 1);
    let b = file(ROOT, "f", 2);
    trees.set_local_changed(f.clone(), local(b.clone()));
    let (mut commits, _) = sent_with_intents(&mut trees);
    for (shape, rev, intent) in [(b.clone(), 2, id(20)), (file(ROOT, "f", 3), 3, id(30))] {
        let mut node = remote(shape, rev);
        node.intents = Some(NodeIntents {
            content: Some(FieldIntent::legacy(intent)),
            ..Default::default()
        });
        trees.set_remote(f.clone(), node);
    }
    let proof = node_state(
        "f",
        &b,
        2,
        Some(proto::FieldIntents {
            content: Some(wire(id(20), 2_000)),
            ..Default::default()
        }),
    );
    commits
        .received(proto::CommitResult {
            seq: 2,
            ..answered(vec![outcome(op_result::Code::Applied)], vec![proof])
        })
        .unwrap();
    assert!(commits.acknowledge(&mut trees, 3));
    assert!(trees.journal_row(&f, Field::Content).is_none());
}

#[test]
fn a_superseded_move_ends_unless_the_server_applied_another_place_before_it() {
    for (applied_at_ms, stays) in [(2_500, false), (1_000, true)] {
        let mut trees = Trees::new(ROOT, &IdGen::new(0));
        synced(&mut trees, "f", file(ROOT, "a", 1), 1);
        trees.set_local_changed("f".into(), local(file(ROOT, "b", 1)));
        let commit = build(&mut trees, &["field_intents"], 1_700);
        let reported = proto::FieldIntents {
            place: Some(wire(id(5), applied_at_ms)),
            ..Default::default()
        };
        let state = node_state("f", &file(ROOT, "c", 1), 2, Some(reported));
        trees.record_results(
            &commit,
            &answered(vec![outcome(op_result::Code::Superseded)], vec![state]),
        );
        let row = trees.journal_row(&"f".into(), Field::Place);
        match stays {
            true => assert_eq!(
                row.map(|row| row.state.clone()),
                Some(RowState::Local { flight: None })
            ),
            false => assert_eq!(row, None),
        }
    }
}

#[test]
fn a_superseded_row_finds_the_node_holding_its_intent_among_the_results_node_states() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    synced(&mut trees, "f", file(ROOT, "a", 1), 1);
    trees.set_local_changed("f".into(), local(file(ROOT, "b", 1)));
    let commit = build(&mut trees, &["field_intents"], 1_700);
    let place = sent(&trees, "f", Field::Place);
    let reported = proto::FieldIntents {
        place: Some(wire(place, 1_900)),
        ..Default::default()
    };
    let state = node_state("g", &file(ROOT, "b", 1), 2, Some(reported));
    let superseded = outcome(op_result::Code::Superseded);
    trees.record_results(&commit, &answered(vec![superseded], vec![state]));
    assert_eq!(trees.journal_row(&"f".into(), Field::Place), None);
    assert_eq!(held_as(&trees, "g", Field::Place), Some((place, 1_900)));
}

#[test]
fn a_conflict_copy_takes_the_content_row_as_held() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    synced(&mut trees, "f", file(ROOT, "a", 1), 1);
    trees.set_local_changed("f".into(), local(file(ROOT, "a", 2)));
    let commit = build(&mut trees, &["field_intents"], 1_700);
    let edit = sent(&trees, "f", Field::Content);
    let copy = proto::OpResult {
        node_id: "copy".into(),
        ..outcome(op_result::Code::ConflictCopy)
    };
    let state = node_state("copy", &file(ROOT, "a (copy)", 2), 4, None);
    trees.record_results(&commit, &answered(vec![copy], vec![state]));
    assert_eq!(trees.journal_row(&"f".into(), Field::Content), None);
    let row = trees.journal_row(&"copy".into(), Field::Content).unwrap();
    let state = RowState::Held {
        held_as: edit,
        version_id: Some("v4".into()),
    };
    assert_eq!((row.intent, &row.state), (edit, &state));
}

#[test]
fn a_fallback_create_merged_into_a_folder_ends_its_place_and_status_and_moves_its_childrens_rows() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    trees.set_local_changed("d".into(), local(dir(ROOT, "d")));
    trees.set_local_changed("c".into(), local(file("d", "c", 1)));
    let commit = build(&mut trees, &["field_intents"], 1_700);
    let merged = proto::OpResult {
        node_id: "t".into(),
        ..outcome(op_result::Code::MergedInto)
    };
    let results = (commit.ops.iter())
        .map(|op| match &op.op {
            Some(op::Op::CreateDir(_)) => merged.clone(),
            _ => outcome(op_result::Code::Rejected),
        })
        .collect();
    trees.record_results(&commit, &answered(results, Vec::new()));
    assert_eq!(trees.journal_row(&"d".into(), Field::Place), None);
    assert_eq!(trees.journal_row(&"d".into(), Field::Status), None);
    let child = trees.journal_row(&"c".into(), Field::Place).unwrap();
    assert!(matches!(&child.value, Value::Place(place) if place.parent == "t"));
}

#[test]
fn a_create_answered_id_exists_ends_its_live_status_row_and_keeps_the_rest_to_send_again() {
    for (reason, ended) in [("ID_EXISTS", true), ("NAME_TAKEN", false)] {
        let mut trees = Trees::new(ROOT, &IdGen::new(0));
        trees.set_local_changed("f".into(), local(file(ROOT, "a", 1)));
        let commit = build(&mut trees, &["field_intents"], 1_700);
        let rejected = proto::OpResult {
            reject_reason: reason.into(),
            ..outcome(op_result::Code::Rejected)
        };
        trees.record_results(&commit, &answered(vec![rejected], Vec::new()));
        assert_eq!(
            trees.journal_row(&"f".into(), Field::Status).is_none(),
            ended
        );
        for field in [Field::Place, Field::Content] {
            let row = trees.journal_row(&"f".into(), field).unwrap();
            assert_eq!(row.state, RowState::Local { flight: None });
        }
    }
}

#[test]
fn id_exists_on_a_create_ends_its_flight_and_keeps_a_later_delete_with_its_seen_list() {
    let mut t = Trees::new(ROOT, &IdGen::new(0));
    let n = NodeId::from("f");
    let shape = file(ROOT, "a", 1);
    t.set_local_changed(n.clone(), local(shape.clone()));
    let commit = build(&mut t, &["field_intents"], 1_700);
    t.remove_local_by_user(&n);
    let seen = t.journal().seen(&n).to_vec();
    assert!(!seen.is_empty());
    t.set_remote(n.clone(), remote(shape, 2));
    t.record_results(
        &commit,
        &answered(
            vec![proto::OpResult {
                reject_reason: "ID_EXISTS".into(),
                ..outcome(op_result::Code::Rejected)
            }],
            Vec::new(),
        ),
    );
    assert!(
        t.journal_row(&n, Field::Status)
            .is_some_and(|r| r.value == (Value::Status { live: false })
                && r.state == (RowState::Local { flight: None }))
    );
    assert_eq!(t.journal().seen(&n), seen.as_slice());
}

#[test]
fn a_create_names_the_author_of_each_field_it_carries() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    trees.set_local_changed("f".into(), local(file(ROOT, "a", 1)));
    let mut status = trees
        .journal_row(&"f".into(), Field::Status)
        .unwrap()
        .into_owned();
    status.author = Author::Carried(Some("b".into()));
    trees.journal_mut().replace(&"f".into(), status);
    let commit = build(&mut trees, &["field_intents"], 1_700);
    let fields = commit.ops[0].fields.clone().unwrap();
    let author = |intent: Option<proto::Intent>| intent.unwrap().author_device_id;
    assert_eq!(author(fields.place), "me");
    assert_eq!(author(fields.content), "me");
    assert_eq!(author(fields.status), "b");
}

#[test]
fn a_held_row_keeps_its_value_and_time_when_r_takes_an_earlier_intent() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    synced(&mut trees, "f", file(ROOT, "a", 1), 1);
    let first = held(&trees, "f", Field::Content);
    trees.set_local_changed("f".into(), local(file(ROOT, "a", 2)));
    let commit = build(&mut trees, &["field_intents"], 1_700);
    let edit = sent(&trees, "f", Field::Content);
    let reported = proto::FieldIntents {
        content: Some(wire(edit, 1_900)),
        ..Default::default()
    };
    let shape = file(ROOT, "a", 2);
    let state = node_state("f", &shape, 2, Some(reported.clone()));
    let (_, mut node) = RemoteNode::from_state(&state).unwrap();
    trees.set_remote("f".into(), node.clone());
    trees.record_results(
        &commit,
        &answered(vec![outcome(op_result::Code::Applied)], vec![state]),
    );
    assert_eq!(held_as(&trees, "f", Field::Content), Some((edit, 1_900)));
    node.shape = file(ROOT, "a", 1);
    node.version_id = "v1".into();
    node.intents.as_mut().unwrap().content = Some(FieldIntent {
        id: first,
        author: "me".into(),
        replay: false,
        applied_at_ms: 1_000,
    });
    trees.set_remote("f".into(), node);
    let row = trees.journal_row(&"f".into(), Field::Content).unwrap();
    assert_eq!(held_as(&trees, "f", Field::Content), Some((edit, 1_900)));
    let edited = Value::Content {
        version_id: None,
        value: ContentValue::from(&content(2)),
    };
    assert!(row.value.same(&edited));
}

#[test]
fn an_acknowledged_commit_holds_the_rows_its_results_prove_as_r_holds_them() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    let shape = file(ROOT, "a", 1);
    trees.set_local_changed("f".into(), local(shape.clone()));
    let mut commits = Commits::from_state(CommitsState::default()).unwrap();
    trees.rules_scanned();
    trees.settle(&Default::default());
    let base = Base {
        seq: 0,
        commit_id: String::new(),
        committed_at_ms: 1_700,
    };
    (commits.build(&mut trees, &mut IdGen::new(1), 0, "history", "me", base)).unwrap();
    let place = sent(&trees, "f", Field::Place);
    trees.set_remote_made_here("f".into(), remote(shape, 1));
    let result = proto::CommitResult {
        seq: 1,
        results: vec![outcome(op_result::Code::Applied)],
        ..Default::default()
    };
    commits.received(result).unwrap();
    assert!(commits.acknowledge(&mut trees, 1));
    assert_eq!(
        held_as(&trees, "f", Field::Place).map(|(held_as, _)| held_as),
        Some(held(&trees, "f", Field::Place))
    );
    assert_ne!(held(&trees, "f", Field::Place), place);
}

fn compact_edit(trees: &mut Trees) -> (Row, RemoteNode) {
    synced(trees, "f", file(ROOT, "a", 1), 1);
    trees.set_local_changed("f".into(), local(file(ROOT, "a", 2)));
    let commit = build(trees, &["field_intents"], 1_700);
    let reported = proto::FieldIntents {
        place: Some(wire(held(trees, "f", Field::Place), 1_000)),
        content: Some(wire(sent(trees, "f", Field::Content), 1_900)),
        status: Some(wire(held(trees, "f", Field::Status), 1_000)),
    };
    let state = node_state("f", &file(ROOT, "a", 2), 2, Some(reported));
    let (_, node) = RemoteNode::from_state(&state).unwrap();
    trees.set_remote("f".into(), node.clone());
    trees.record_results(
        &commit,
        &answered(vec![outcome(op_result::Code::Applied)], vec![state]),
    );
    assert!(
        trees
            .journal()
            .compact_row(&"f".into(), Field::Content)
            .is_some()
    );
    (
        trees
            .journal_row(&"f".into(), Field::Content)
            .unwrap()
            .into_owned(),
        node,
    )
}

#[test]
fn a_compact_row_is_written_out_unchanged_whenever_r_stops_giving_it() {
    for trigger in ["earlier", "listed", "moved", "lost", "loss"] {
        let mut trees = Trees::new(ROOT, &IdGen::new(0));
        let (original, mut node) = compact_edit(&mut trees);
        match trigger {
            "earlier" | "listed" => {
                node.shape = file(ROOT, "a", 1);
                node.version_id = "v1".into();
                let first = trees.remote_history(&"f".into(), Field::Content)[0].intent;
                node.intents.as_mut().unwrap().content = Some(FieldIntent {
                    id: first,
                    author: "me".into(),
                    replay: false,
                    applied_at_ms: 1_000,
                });
                match trigger {
                    "listed" => trees.set_listed_remote("f".into(), node),
                    _ => trees.set_remote("f".into(), node),
                }
            }
            "moved" => {
                node.shape.name = "b".into();
                node.meta_rev = 3;
                node.intents.as_mut().unwrap().place = Some(FieldIntent::legacy(id(8)));
                trees.set_remote("f".into(), node);
            }
            "lost" => trees.remove_remote(&"f".into()),
            _ => {
                let (none, merged) = (Default::default(), Default::default());
                trees.forget_lost(vec![], vec![], vec![], vec![], vec![], none, &merged, 0, 0);
            }
        }
        let row = trees
            .journal_row(&"f".into(), Field::Content)
            .unwrap()
            .into_owned();
        let expected = Row {
            replay: trigger == "loss",
            ..original
        };
        assert_eq!(row, expected, "{trigger}");
        let compact = trees
            .journal()
            .compact_row(&"f".into(), Field::Content)
            .is_some();
        assert_eq!(compact, trigger == "moved", "{trigger}");
    }
}

#[test]
fn the_store_keeps_compact_rows_whole_and_a_reload_compacts_them_again() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    let (original, _) = compact_edit(&mut trees);
    let changes = trees.take_changes();
    let written = changes
        .journal
        .rows
        .iter()
        .find(|(key, _)| *key == ("f".into(), Field::Content));
    assert_eq!(written.and_then(|(_, row)| row.as_ref()), Some(&original));
    let state = trees.state();
    assert_eq!(
        state.journal.rows.get(&("f".into(), Field::Content)),
        Some(&original)
    );
    let reload = || {
        let mut reloaded = Trees::new(ROOT, &IdGen::new(0));
        for (id, node) in trees.remote().iter() {
            reloaded.set_remote(id.clone(), node.clone());
        }
        for (id, shape) in trees.synced().iter() {
            reloaded.set_synced(id.clone(), Some(shape.clone()));
        }
        for (id, node) in trees.local().iter() {
            reloaded.set_local(id.clone(), node.clone());
        }
        reloaded.restore(state.clone()).unwrap();
        reloaded
    };
    let mut reloaded = reload();
    assert!(
        reloaded
            .journal()
            .compact_row(&"f".into(), Field::Content)
            .is_some()
    );
    assert!(reloaded.take_changes().journal.rows.is_empty());
    reloaded.remove_remote(&"f".into());
    assert!(
        reloaded
            .journal()
            .compact_row(&"f".into(), Field::Content)
            .is_none()
    );
    assert_eq!(
        reloaded
            .journal_row(&"f".into(), Field::Content)
            .unwrap()
            .into_owned(),
        original
    );
    assert!(reloaded.take_changes().journal.rows.is_empty());
    let mut reloaded = reload();
    let mut later = trees.remote().get(&"f".into()).cloned().unwrap();
    later.shape = file(ROOT, "a", 3);
    later.version_id = "v3".into();
    later.intents.as_mut().unwrap().content = Some(FieldIntent::legacy(id(9)));
    reloaded.set_remote("f".into(), later);
    assert_eq!(
        reloaded.take_changes().journal.rows,
        [(("f".into(), Field::Content), None)]
    );
}

#[test]
fn a_history_entry_that_r_s_intent_gives_is_kept_only_once_r_moves_past_it() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    let (row, _) = compact_edit(&mut trees);
    let RowState::Held { held_as, .. } = row.state else {
        panic!("{row:?}");
    };
    let entries = trees
        .journal()
        .history(&"f".into(), Field::Content)
        .to_vec();
    assert!(
        entries.iter().all(|entry| entry.intent != held_as),
        "{entries:?}"
    );
    assert_eq!(
        trees
            .remote_history(&"f".into(), Field::Content)
            .last()
            .map(|entry| entry.intent),
        Some(held_as)
    );
    let first = trees.remote_history(&"f".into(), Field::Content)[0].intent;
    for (moved_to, shape) in [(id(9), file(ROOT, "a", 3)), (first, file(ROOT, "a", 1))] {
        let mut trees = Trees::new(ROOT, &IdGen::new(0));
        let (_, mut node) = compact_edit(&mut trees);
        node.shape = shape;
        node.version_id = "v3".into();
        node.intents.as_mut().unwrap().content = Some(FieldIntent::legacy(moved_to));
        trees.set_remote("f".into(), node);
        let explicit: Vec<IntentId> = (trees.journal().history(&"f".into(), Field::Content).iter())
            .map(|entry| entry.intent)
            .collect();
        assert_eq!(explicit.last(), Some(&held_as), "{explicit:?}");
        assert!(!explicit.contains(&id(9)), "{explicit:?}");
    }
}

#[test]
fn a_create_into_a_purged_folder_takes_the_place_the_server_reports_and_never_sends_the_folder_again()
 {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    synced(&mut trees, "a", dir(ROOT, "a"), 1);
    synced(&mut trees, "x", dir("a", "x"), 1);
    trees.set_local_changed("z".into(), local(file("x", "z", 1)));
    trees.remove_remote(&"x".into());
    let mut commits = Commits::from_state(CommitsState::default()).unwrap();
    commits.connected(&Welcome {
        features: vec!["field_intents".into()],
        ..Default::default()
    });
    trees.rules_scanned();
    trees.settle(&Default::default());
    let base = |seq| Base {
        seq,
        commit_id: String::new(),
        committed_at_ms: 1_700,
    };
    let mut ids = IdGen::new(1);
    let commit = (commits
        .build(&mut trees, &mut ids, 0, "history", "me", base(0))
        .cloned())
    .unwrap();
    let place = sent(&trees, "z", Field::Place);
    let reported = proto::FieldIntents {
        place: Some(wire(place, 1_800)),
        content: Some(wire(sent(&trees, "z", Field::Content), 1_800)),
        status: Some(wire(sent(&trees, "z", Field::Status), 1_800)),
    };
    let state = node_state("z", &file("a", "z", 1), 2, Some(reported));
    let (_, node) = RemoteNode::from_state(&state).unwrap();
    trees.set_remote_made_here("z".into(), node);
    let results = (commit.ops.iter())
        .map(|op| match &op.op {
            Some(op::Op::CreateDir(create)) if create.node_id == "x" => {
                outcome(op_result::Code::Superseded)
            }
            _ => outcome(op_result::Code::Applied),
        })
        .collect();
    commits.received(answered(results, vec![state])).unwrap();
    assert!(commits.acknowledge(&mut trees, 1));
    trees.settle(commits.aliases());
    assert_eq!(held_as(&trees, "z", Field::Place), Some((place, 1_800)));
    let mut effects = Effects::default();
    for ino in 100.. {
        trees.settle(commits.aliases());
        let in_flight = commits.in_flight();
        let Some(record) = (effects
            .plan(&trees, in_flight, &mut ids, 0, BatchLimit::ONE)
            .first())
        .cloned() else {
            break;
        };
        let identity = FileIdentity {
            ino,
            birth_ns: Some(0),
            handle: None,
        };
        effects
            .done(&mut trees, in_flight, &record.effect_id, Some(identity))
            .unwrap();
    }
    let parent = |trees: &Trees, id: &str| {
        trees
            .local()
            .get(&id.into())
            .map(|node| node.shape.parent.clone())
    };
    assert_eq!(parent(&trees, "z"), Some(NodeId::from("a")));
    trees.set_local_changed("w".into(), local(file(ROOT, "w", 1)));
    trees.settle(commits.aliases());
    let next = commits
        .build(&mut trees, &mut ids, 0, "history", "me", base(1))
        .cloned()
        .unwrap();
    let sent: Vec<&str> = (next.ops.iter())
        .filter_map(|op| match &op.op {
            Some(op::Op::CreateDir(create)) => Some(create.node_id.as_str()),
            Some(op::Op::CreateFile(create)) => Some(create.node_id.as_str()),
            Some(op::Op::Move(moved)) => Some(moved.node_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(sent, ["w"]);
}

struct PurgedFolder {
    trees: Trees,
    commits: Commits,
    ids: IdGen,
    seq: u64,
}

impl PurgedFolder {
    fn new(change: impl FnOnce(&mut Trees)) -> Self {
        let mut trees = Trees::new(ROOT, &IdGen::new(0));
        synced(&mut trees, "a", dir(ROOT, "a"), 1);
        synced(&mut trees, "x", dir("a", "x"), 1);
        change(&mut trees);
        trees.remove_remote(&"x".into());
        let mut commits = Commits::from_state(CommitsState::default()).unwrap();
        commits.connected(&Welcome {
            features: vec!["field_intents".into()],
            ..Default::default()
        });
        trees.rules_scanned();
        let ids = IdGen::new(1);
        Self {
            trees,
            commits,
            ids,
            seq: 0,
        }
    }

    fn build(&mut self) -> Option<Vec<(String, String)>> {
        self.trees.settle(self.commits.aliases());
        let base = Base {
            seq: self.seq,
            commit_id: String::new(),
            committed_at_ms: 1_700,
        };
        let commit = (self.commits).build(&mut self.trees, &mut self.ids, 0, "history", "me", base);
        let ops = commit?.ops.iter().map(|op| match &op.op {
            Some(op::Op::CreateDir(create)) => (create.node_id.clone(), create.parent_id.clone()),
            Some(op::Op::CreateFile(create)) => (create.node_id.clone(), create.parent_id.clone()),
            other => (format!("{other:?}"), String::new()),
        });
        Some(ops.collect())
    }

    fn build_one(&mut self) -> Vec<(String, String)> {
        loop {
            let ops = self.build().unwrap();
            if ops.len() == 1 {
                return ops;
            }
            let refusal = proto::Error {
                code: proto::error::Code::LimitExceeded as i32,
                ..Default::default()
            };
            self.commits.failed(&mut self.trees, &refusal).unwrap();
        }
    }

    fn answer(&mut self, result: proto::OpResult, states: Vec<proto::NodeState>) {
        self.seq += 1;
        for state in &states {
            let (id, node) = RemoteNode::from_state(state).unwrap();
            self.trees.set_remote_made_here(id, node);
        }
        let result = proto::CommitResult {
            seq: self.seq,
            ..answered(vec![result], states)
        };
        self.commits.received(result).unwrap();
        assert!(self.commits.acknowledge(&mut self.trees, self.seq));
    }

    fn run_effects(&mut self) -> Vec<String> {
        let (mut effects, mut done) = (Effects::default(), Vec::new());
        for ino in 100.. {
            self.trees.settle(self.commits.aliases());
            let in_flight = self.commits.in_flight();
            let planned = effects.plan(&self.trees, in_flight, &mut self.ids, 0, BatchLimit::ONE);
            let Some(record) = planned.first().cloned() else {
                return done;
            };
            done.push(format!("{:?}", record.effect));
            let identity = FileIdentity {
                ino,
                birth_ns: Some(0),
                handle: None,
            };
            (effects)
                .done(
                    &mut self.trees,
                    in_flight,
                    &record.effect_id,
                    Some(identity),
                )
                .unwrap();
        }
        done
    }

    fn parent(&self, id: &str) -> Option<NodeId> {
        (self.trees.local().get(&id.into())).map(|node| node.shape.parent.clone())
    }

    fn superseded(&mut self) {
        assert_eq!(self.build_one(), [("x".to_owned(), "a".to_owned())]);
        self.answer(outcome(op_result::Code::Superseded), vec![]);
    }

    fn settles_without_the_folder(&mut self) {
        self.run_effects();
        assert_eq!(self.parent("x"), None);
        assert_eq!(self.build(), None);
    }
}

fn applied_at(id: &str, shape: &Shape, trees: &Trees) -> proto::NodeState {
    let reported = |field| Some(wire(sent(trees, id, field), 1_800));
    let intents = proto::FieldIntents {
        place: reported(Field::Place),
        content: reported(Field::Content),
        status: reported(Field::Status),
    };
    node_state(id, shape, 2, Some(intents))
}

#[test]
fn an_empty_renamed_folder_the_server_purged_goes_to_the_local_trash_and_nothing_is_sent_after_the_result()
 {
    let mut device = PurgedFolder::new(|trees| {
        trees.set_local_changed("x".into(), local(dir("a", "y")));
    });
    assert_eq!(device.build_one(), [("x".to_owned(), "a".to_owned())]);
    device.answer(outcome(op_result::Code::Superseded), vec![]);
    let effects = device.run_effects();
    assert!(
        effects.iter().any(|effect| effect.starts_with("RemoveDir")),
        "{effects:?}"
    );
    assert_eq!(device.parent("x"), None);
    assert_eq!(device.build(), None);
}

#[test]
fn a_new_file_in_a_purged_folder_lands_under_the_nearest_live_ancestor() {
    let mut device = PurgedFolder::new(|trees| {
        trees.set_local_changed("z".into(), local(file("x", "z", 1)));
    });
    device.superseded();
    assert_eq!(device.build(), Some(vec![("z".to_owned(), "x".to_owned())]));
    let state = applied_at("z", &file("a", "z", 1), &device.trees);
    device.answer(outcome(op_result::Code::Applied), vec![state]);
    device.settles_without_the_folder();
    assert_eq!(device.parent("z"), Some("a".into()));
}

#[test]
fn an_edited_file_in_a_purged_folder_lands_as_a_conflict_copy() {
    let mut device = PurgedFolder::new(|trees| {
        synced(trees, "f", file("x", "f", 1), 1);
        trees.set_local_changed("f".into(), local(file("x", "f", 2)));
        trees.remove_remote(&"f".into());
    });
    device.superseded();
    assert_eq!(device.build(), Some(vec![("f".to_owned(), "x".to_owned())]));
    let copy = file("a", "f (conflict)", 2);
    let state = node_state("c", &copy, 2, None);
    let result = proto::OpResult {
        node_id: "c".into(),
        name: copy.name.clone(),
        ..outcome(op_result::Code::ConflictCopy)
    };
    device.answer(result, vec![state]);
    device.settles_without_the_folder();
    assert_eq!(device.parent("f"), None);
    assert_eq!(device.parent("c"), Some("a".into()));
}

#[test]
fn a_carried_file_in_a_purged_folder_that_the_tombstone_lists_does_not_come_back() {
    let mut device = PurgedFolder::new(|trees| {
        trees.set_local_changed("g".into(), local(file("x", "g", 1)));
        for field in Field::ALL {
            let mut row = trees.journal_row(&"g".into(), field).unwrap().into_owned();
            row.author = Author::Carried(Some("b".into()));
            trees.journal_mut().replace(&"g".into(), row);
        }
    });
    device.superseded();
    assert_eq!(device.build(), Some(vec![("g".to_owned(), "x".to_owned())]));
    device.answer(outcome(op_result::Code::Superseded), vec![]);
    device.settles_without_the_folder();
    assert_eq!(device.parent("g"), None);
}

#[test]
fn a_create_superseded_because_another_node_holds_its_intents_does_not_mark_its_folder_purged() {
    let mut device = PurgedFolder::new(|trees| {
        trees.set_local_changed("z".into(), local(file("x", "z", 1)));
    });
    assert_eq!(device.build_one(), [("x".to_owned(), "a".to_owned())]);
    let held = |field| Some(wire(sent(&device.trees, "x", field), 1_800));
    let intents = proto::FieldIntents {
        place: held(Field::Place),
        status: held(Field::Status),
        content: None,
    };
    let holder = node_state("y", &dir("a", "x"), 2, Some(intents));
    device.answer(outcome(op_result::Code::Superseded), vec![holder]);
    assert!(!device.trees.purged_folder(&"x".into()));
}

#[test]
fn a_purged_folder_ends_when_r_holds_it_again_or_its_absence_lapses() {
    let mut device = PurgedFolder::new(|trees| {
        trees.set_local_changed("z".into(), local(file("x", "z", 1)));
    });
    device.superseded();
    assert!(device.trees.purged_folder(&"x".into()));
    let mut relisted = device.trees.clone();
    relisted.set_listed_remote("x".into(), remote(dir("a", "x"), 3));
    assert!(!relisted.purged_folder(&"x".into()));
    device.trees.journal_mut().set_clock(30 * DAY_MS);
    assert!(!device.trees.purged_folder(&"x".into()));
}

fn stored_flight(kind: op::Op) -> InFlight {
    InFlight {
        commit: proto::Commit {
            ops: vec![proto::Op {
                op: Some(kind),
                ..Default::default()
            }],
            ..Default::default()
        },
        move_bases: Default::default(),
        deletes: Default::default(),
        restores: Default::default(),
        owners: Default::default(),
        result: None,
    }
}

fn backfilled(table: &str, waiting: Option<Waiting>) -> Trees {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    trees.set_synced("d".into(), Some(dir(ROOT, "d")));
    trees.set_local("d".into(), local(dir(ROOT, "d")));
    let mut state = trees.state();
    match table {
        "lost_creates" => state.lost_creates.insert("d".into()),
        _ => state.unsent_creates.insert("d".into()),
    };
    state
        .waiting
        .extend(waiting.map(|waiting| ("d".into(), waiting)));
    trees.restore(state).unwrap();
    trees.backfill(None, false, 1_000);
    trees
}

fn rows_of(trees: &Trees, node: &str) -> Vec<(Field, Row)> {
    (trees.journal_rows())
        .filter(|((id, _), _)| id == node)
        .map(|((_, field), row)| (field, row.into_owned()))
        .collect()
}

#[test]
fn a_backfill_carries_every_field_of_a_folder_owed_a_create_as_a_replay_only_when_it_was_lost() {
    for (table, replay) in [("lost_creates", true), ("unsent_creates", false)] {
        let trees = backfilled(table, None);
        let place = IntentId::legacy_place("d", 0, ROOT, "d");
        let status = IntentId::legacy_live("d", 0);
        let carried = |field, intent, value| {
            let state = RowState::Local { flight: None };
            (
                field,
                Row {
                    intent,
                    author: Author::Carried(None),
                    base: None,
                    value,
                    wire_base: WireBase::None,
                    state,
                    time_ms: 0,
                    replay,
                },
            )
        };
        let at = Value::Place(Place {
            parent: ROOT.into(),
            name: "d".into(),
            mark: None,
        });
        assert_eq!(
            rows_of(&trees, "d"),
            [
                carried(Field::Place, place, at),
                carried(Field::Status, status, Value::Status { live: true })
            ],
            "{table}"
        );
    }
}

#[test]
fn a_backfilled_lost_folder_whose_queued_removal_finishes_keeps_no_rows() {
    let removal = Waiting {
        shape: None,
        origin: Origin::Other,
    };
    let mut trees = backfilled("lost_creates", Some(removal));
    assert_eq!(rows_of(&trees, "d").len(), 2);
    trees.remove_local(&"d".into());
    trees.settle(&Default::default());
    assert_eq!(rows_of(&trees, "d"), []);
    let mut sending = backfilled("unsent_creates", None);
    build(&mut sending, &["field_intents"], 1_700);
    sending.remove_local(&"d".into());
    sending.settle(&Default::default());
    let states: Vec<RowState> = (rows_of(&sending, "d").into_iter())
        .map(|(_, row)| row.state)
        .collect();
    assert_eq!(states, [RowState::Sent, RowState::Sent]);
}

#[test]
fn a_backfill_keeps_an_unsent_edit_whose_bytes_r_already_holds() {
    let mut t = Trees::new(ROOT, &IdGen::new(0));
    synced(&mut t, "f", file(ROOT, "a", 1), 1);
    let mut old = t.state();
    old.unsent_edits.insert("f".into());
    old.dirty.insert("f".into());
    t.restore(old).unwrap();
    t.backfill(None, false, 1_000);
    assert!(
        t.journal_row(&"f".into(), Field::Content)
            .is_some_and(|r| r.author == Author::Own && matches!(r.state, RowState::Local { .. }))
    );
}

fn position(seed: u64, rev: u64, name: &str) -> MovePosition {
    MovePosition {
        place_id: IdGen::new(seed).uuid_v7_bytes(1),
        record: Agreed {
            rev,
            parent: ROOT.into(),
            name: name.into(),
            trash_group: None,
        },
    }
}

fn place_history(trees: &Trees, node: &str) -> Vec<IntentId> {
    (trees.remote_history(&node.into(), Field::Place).iter())
        .map(|entry| entry.intent)
        .collect()
}

fn base_id(position: &MovePosition) -> IntentId {
    IntentId::from_bytes(&position.place_id).unwrap()
}

#[test]
fn a_backfill_seeds_the_place_history_and_the_held_base_of_a_move_r_holds_exactly() {
    let (result, newest, oldest) = (
        position(1, 5, "c"),
        position(2, 4, "b"),
        position(3, 3, "a"),
    );
    for (rev, place_id, exact) in [
        (5, result.place_id, true),
        (6, result.place_id, false),
        (5, newest.place_id, false),
    ] {
        let mut trees = Trees::new(ROOT, &IdGen::new(0));
        let mut node = remote(file(ROOT, "c", 1), rev);
        node.intents = Some(NodeIntents {
            place: Some(FieldIntent {
                id: IntentId::from_bytes(&place_id).unwrap(),
                author: "me".into(),
                replay: false,
                applied_at_ms: 77,
            }),
            ..Default::default()
        });
        trees.set_remote("f".into(), node);
        trees.set_synced("f".into(), Some(file(ROOT, "c", 1)));
        trees.set_local("f".into(), local(file(ROOT, "c", 1)));
        trees.set_remote_place_id(&"f".into(), Some(place_id));
        let history = MoveHistory {
            result: result.clone(),
            bases: vec![newest.clone(), oldest.clone()],
        };
        let mut state = trees.state();
        let moved = MovedHere {
            seq: 5,
            upgraded: false,
            history: Some(Box::new(history)),
        };
        state.moved_here.insert("f".into(), moved);
        trees.restore(state).unwrap();
        trees.backfill(None, false, 1_000);
        let row = trees.journal_row(&"f".into(), Field::Place).unwrap();
        let seeded = place_history(&trees, "f");
        match exact {
            true => {
                assert_eq!(row.base, Some(base_id(&newest)), "{rev}");
                assert_eq!(
                    seeded,
                    [base_id(&oldest), base_id(&newest), base_id(&result)],
                    "{rev}"
                );
            }
            false => {
                assert_eq!(row.base, None, "{rev}");
                assert!(!seeded.contains(&base_id(&oldest)), "{rev}");
                assert!(!seeded.contains(&base_id(&newest)) || rev == 5, "{rev}");
            }
        }
        assert_eq!(row.time_ms, 77);
    }
}

#[test]
fn a_backfill_gives_the_newest_move_base_to_the_sent_row_or_to_the_flight_a_later_move_replaced() {
    let (newest, oldest) = (position(2, 3, "a"), position(3, 2, "z"));
    for later in [None, Some("c")] {
        let mut trees = Trees::new(ROOT, &IdGen::new(0));
        let mut node = remote(file(ROOT, "a", 1), 3);
        node.intents = Some(NodeIntents {
            place: Some(FieldIntent::legacy(base_id(&newest))),
            ..Default::default()
        });
        trees.set_remote("f".into(), node);
        trees.set_synced("f".into(), Some(file(ROOT, "a", 1)));
        trees.set_local("f".into(), local(file(ROOT, later.unwrap_or("b"), 1)));
        let commit = proto::Commit {
            ops: vec![proto::Op {
                op: Some(op::Op::Move(proto::Move {
                    node_id: "f".into(),
                    new_parent_id: ROOT.into(),
                    new_name: "b".into(),
                    ..Default::default()
                })),
                ..Default::default()
            }],
            ..Default::default()
        };
        let flight = InFlight {
            commit,
            move_bases: [("f".into(), vec![newest.clone(), oldest.clone()])].into(),
            deletes: Default::default(),
            restores: Default::default(),
            owners: Default::default(),
            result: None,
        };
        trees.backfill(Some(&flight), false, 1_000);
        let row = trees.journal_row(&"f".into(), Field::Place).unwrap();
        let base = match &row.state {
            RowState::Sent => row.base,
            RowState::Local {
                flight: Some(flight),
            } => flight.base,
            other => panic!("{other:?}"),
        };
        assert_eq!(base, Some(base_id(&newest)), "{later:?}");
        assert_eq!(
            place_history(&trees, "f"),
            [base_id(&oldest), base_id(&newest)],
            "{later:?}"
        );
        assert_eq!(matches!(row.state, RowState::Sent), later.is_none());
    }
}

#[test]
fn a_backfill_keeps_the_trash_group_of_a_trashed_past_position() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    synced(&mut trees, "f", file(ROOT, "f.txt", 1), 5);
    let trashed = Agreed {
        rev: 3,
        parent: ROOT.into(),
        name: "f.txt".into(),
        trash_group: Some("3:0".into()),
    };
    let mut state = trees.state();
    state
        .past_positions
        .insert("f".into(), vec![trashed.clone()]);
    trees.restore(state).unwrap();
    trees.backfill(None, false, 1_000);
    let history = |field| -> Vec<IntentId> {
        (trees.remote_history(&"f".into(), field).iter())
            .map(|entry| entry.intent)
            .collect()
    };
    assert!(history(Field::Place).contains(&IntentId::legacy_place("f", 3, ROOT, "f.txt")));
    assert!(
        history(Field::Status).contains(&IntentId::legacy_trashed("f", "3:0")),
        "{:?}",
        history(Field::Status)
    );
    assert_eq!(trees.state().past_positions[&"f".into()], [trashed]);
}

#[test]
fn a_backfilled_folder_whose_create_the_server_applies_keeps_its_rows_held() {
    let mut trees = backfilled("unsent_creates", None);
    let mut commits = Commits::from_state(CommitsState::default()).unwrap();
    commits.connected(&Welcome {
        features: vec!["field_intents".into()],
        ..Default::default()
    });
    trees.rules_scanned();
    trees.settle(&Default::default());
    let base = Base {
        seq: 0,
        commit_id: String::new(),
        committed_at_ms: 1_700,
    };
    let commit = (commits
        .build(&mut trees, &mut IdGen::new(1), 0, "history", "me", base)
        .cloned())
    .unwrap();
    let place = sent(&trees, "d", Field::Place);
    let reported = proto::FieldIntents {
        place: Some(wire(place, 1_800)),
        content: None,
        status: Some(wire(sent(&trees, "d", Field::Status), 1_800)),
    };
    let state = node_state("d", &dir(ROOT, "d"), 2, Some(reported));
    let (_, node) = RemoteNode::from_state(&state).unwrap();
    trees.set_remote_made_here("d".into(), node);
    let results = commit
        .ops
        .iter()
        .map(|_| outcome(op_result::Code::Applied))
        .collect();
    commits.received(answered(results, vec![state])).unwrap();
    assert!(commits.acknowledge(&mut trees, 1));
    trees.settle(commits.aliases());
    assert!(trees.state().unsent_creates.is_empty());
    assert_eq!(held_as(&trees, "d", Field::Place), Some((place, 1_800)));
    assert_eq!(rows_of(&trees, "d").len(), 2);
}

#[test]
fn a_backfilled_resend_in_flight_is_held_once_the_server_applies_it() {
    let mut t = Trees::new(ROOT, &IdGen::new(0));
    let n = NodeId::from("f");
    let old = file(ROOT, "a", 1);
    let sent = file(ROOT, "a", 2);
    t.set_remote(n.clone(), remote(old.clone(), 1));
    t.set_synced(n.clone(), Some(old));
    t.set_local(n.clone(), local(sent.clone()));
    let mut state = t.state();
    state.resends.insert(n.clone());
    t.restore(state).unwrap();
    let flight = stored_flight(op::Op::UpdateContent(proto::UpdateContent {
        node_id: "f".into(),
        content: sent.content.map(|key| key.content(&[])),
        resend: true,
        ..Default::default()
    }));
    t.backfill(Some(&flight), true, 1_000);
    let reported = node_state("f", &sent, 2, None);
    t.set_remote_made_here(n.clone(), remote(sent.clone(), 2));
    t.record_results(
        &flight.commit,
        &answered(vec![outcome(op_result::Code::Applied)], vec![reported]),
    );
    t.set_synced(n.clone(), Some(sent));
    t.settle(&Default::default());
    assert!(
        t.journal_row(&n, Field::Content)
            .is_some_and(|r| matches!(r.state, RowState::Held { .. }))
    );
}

#[test]
fn the_first_listing_that_names_intents_replaces_the_ids_computed_for_the_values_it_lists() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    synced(&mut trees, "f", file(ROOT, "a", 1), 1);
    synced(&mut trees, "g", file(ROOT, "b", 2), 1);
    let renamed = RemoteNode {
        meta_rev: 2,
        meta_edit_rev: 2,
        ..remote(file(ROOT, "c", 2), 1)
    };
    trees.set_remote("g".into(), renamed);
    let place_of_g = held(&trees, "g", Field::Place);
    let synced_place_of_g = trees
        .synced_intents_apart_from_remote(&"g".into())
        .unwrap()
        .place
        .clone();
    trees.set_local_changed("f".into(), local(file(ROOT, "a", 3)));
    let server = |n| NodeIntents {
        place: Some(FieldIntent::legacy(id(n))),
        content: Some(FieldIntent::legacy(id(n + 1))),
        status: Some(FieldIntent::legacy(id(n + 2))),
        ..Default::default()
    };
    let mut listed: Tree<RemoteNode> = Tree::new(ROOT);
    for (node, shape, n) in [("f", file(ROOT, "a", 1), 1), ("g", file(ROOT, "d", 2), 4)] {
        let listing = RemoteNode {
            intents: Some(server(n)),
            ..remote(shape, 1)
        };
        listed.insert(node.into(), listing);
    }
    assert!(!trees.journal().listed());
    trees.take_listed_intents(&listed);
    assert_eq!(
        trees.remote().get(&"f".into()).unwrap().intents,
        Some(server(1))
    );
    assert_eq!(trees.synced_intents(&"f".into()), Some(server(1)));
    assert_eq!(
        trees.journal_row(&"f".into(), Field::Content).unwrap().base,
        Some(id(2))
    );
    let history: Vec<IntentId> = (trees.journal().history(&"f".into(), Field::Content).iter())
        .map(|entry| entry.intent)
        .collect();
    assert_eq!(history, [id(2)]);
    assert_eq!(held(&trees, "g", Field::Place), place_of_g);
    assert_eq!(held(&trees, "g", Field::Content), id(5));
    let synced_g = trees.synced_intents_apart_from_remote(&"g".into()).unwrap();
    assert_eq!(synced_g.place, synced_place_of_g);
    assert_eq!(
        synced_g.content.as_ref().map(|intent| intent.id),
        Some(id(5))
    );
    assert!(trees.journal().listed());
}

fn sent_with_intents(trees: &mut Trees) -> (Commits, Vec<op::Op>) {
    let mut commits = Commits::from_state(CommitsState::default()).unwrap();
    commits.connected(&Welcome {
        features: vec!["field_intents".into()],
        ..Default::default()
    });
    trees.rules_scanned();
    trees.settle(&Default::default());
    let base = Base {
        seq: 1,
        commit_id: String::new(),
        committed_at_ms: 1_700,
    };
    let commit = (commits.build(trees, &mut IdGen::new(1), 0, "history", "me", base)).unwrap();
    let ops = commit.ops.iter().filter_map(|op| op.op.clone()).collect();
    (commits, ops)
}

fn acknowledge_superseded_move(
    trees: &mut Trees,
    commits: &mut Commits,
    results: Vec<proto::OpResult>,
    now: Shape,
    place_applied_at_ms: i64,
) {
    trees.set_remote("f".into(), remote(now.clone(), 2));
    let place = proto::FieldIntents {
        place: Some(wire(id(9), place_applied_at_ms)),
        ..Default::default()
    };
    let result = proto::CommitResult {
        seq: 2,
        ..answered(results, vec![node_state("f", &now, 2, Some(place))])
    };
    commits.received(result).unwrap();
    assert!(commits.acknowledge(trees, 2));
    trees.settle(&Default::default());
}

#[test]
fn a_superseded_move_whose_node_a_later_op_of_its_commit_brings_back_ends() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    synced(&mut trees, "f", file(ROOT, "a", 1), 1);
    trees.set_local_changed("f".into(), local(file(ROOT, "b", 2)));
    let (mut commits, ops) = sent_with_intents(&mut trees);
    assert!(
        matches!(&ops[..], [op::Op::Move(_), op::Op::UpdateContent(_)]),
        "{ops:?}"
    );
    let results = vec![
        outcome(op_result::Code::Superseded),
        outcome(op_result::Code::AppliedRenamed),
    ];
    let revived = file(ROOT, "a (conflict)", 2);
    acknowledge_superseded_move(&mut trees, &mut commits, results, revived, 1_900);
    assert!(commits.problems().is_empty(), "{:?}", commits.problems());
}

#[test]
fn a_superseded_move_whose_live_node_holds_an_earlier_place_does_not_end() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    synced(&mut trees, "f", file(ROOT, "a", 1), 1);
    trees.set_local_changed("f".into(), local(file(ROOT, "b", 1)));
    let (mut commits, ops) = sent_with_intents(&mut trees);
    assert!(matches!(&ops[..], [op::Op::Move(_)]), "{ops:?}");
    let results = vec![outcome(op_result::Code::Superseded)];
    acknowledge_superseded_move(&mut trees, &mut commits, results, file(ROOT, "c", 1), 1_000);
    let row = trees.journal_row(&"f".into(), Field::Place).unwrap();
    assert!(matches!(row.state, RowState::Local { .. }), "{row:?}");
    assert_eq!(trees.synced().get(&"f".into()), Some(&file(ROOT, "a", 1)));
}

#[test]
fn a_nested_delete_lists_each_intent_below_it_once() {
    fn grow(trees: &mut Trees, order: &mut Vec<String>, parent: &str, depth: usize) {
        for f in 0..4 {
            let id = format!("{parent}/f{f}");
            synced(trees, &id, file(parent, &format!("f{f}"), f), 1);
            order.push(id);
        }
        if depth == 0 {
            return;
        }
        for c in 0..2 {
            let id = format!("{parent}/d{c}");
            synced(trees, &id, dir(parent, &format!("d{c}")), 1);
            grow(trees, order, &id, depth - 1);
            order.push(id);
        }
    }
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    let mut order = Vec::new();
    synced(&mut trees, "top", dir(ROOT, "top"), 1);
    grow(&mut trees, &mut order, "top", 7);
    order.push("top".into());
    for id in &order {
        trees.remove_local_by_user(&NodeId::from(id));
    }
    let fields: usize = (order.iter())
        .map(
            |id| match trees.remote().get(&NodeId::from(id)).unwrap().shape.kind {
                Kind::File => 3,
                Kind::Dir => 2,
            },
        )
        .sum();
    let seen = trees.journal().seen(&"top".into());
    let distinct: std::collections::BTreeSet<_> = (seen.iter())
        .map(|entry| (&entry.node, entry.field, entry.intent))
        .collect();
    assert_eq!((seen.len(), distinct.len()), (fields, fields));
}

const DAY_MS: u64 = 86_400_000;

fn journal_of(trees: &Trees, node: &str) -> Vec<(Field, String)> {
    (trees.journal_rows())
        .filter(|((id, _), _)| id == node)
        .map(|((_, field), row)| (field, format!("{:?}", row.state)))
        .collect()
}

fn purged(trees: &mut Trees, node: &str, at_ms: u64) {
    trees.journal_mut().set_clock(at_ms);
    trees.remove_remote(&node.into());
}

#[test]
fn a_purged_nodes_rows_and_history_end_30_days_after_the_purge() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    compact_edit(&mut trees);
    trees.remove_local_by_user(&"f".into());
    purged(&mut trees, "f", 1_000);
    trees.journal_mut().set_clock(1_000 + 30 * DAY_MS - 1);
    assert_eq!(journal_of(&trees, "f").len(), 2);
    assert!(!trees.journal().seen(&"f".into()).is_empty());
    trees.journal_mut().set_clock(1_000 + 30 * DAY_MS);
    assert_eq!(journal_of(&trees, "f"), []);
    assert!(
        Field::ALL
            .iter()
            .all(|field| trees.journal().history(&"f".into(), *field).is_empty())
    );
    assert!(trees.journal().seen(&"f".into()).is_empty());
}

#[test]
fn a_local_change_of_a_purged_node_outlasts_the_30_days() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    compact_edit(&mut trees);
    trees.set_local_changed("f".into(), local(file(ROOT, "a", 3)));
    trees.settle(&Default::default());
    purged(&mut trees, "f", 1_000);
    trees.journal_mut().set_clock(1_000 + 31 * DAY_MS);
    let rows = journal_of(&trees, "f");
    assert!(
        matches!(&rows[..], [(Field::Content, state)] if state.starts_with("Local")),
        "{rows:?}"
    );
}

#[test]
fn a_new_nodes_status_row_ends_30_days_after_a_listing_without_it_and_its_other_rows_stay() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    trees.set_local_changed("n".into(), local(file(ROOT, "n", 1)));
    trees.journal_mut().set_clock(1_000);
    trees.listed_without_journal_nodes();
    trees.journal_mut().set_clock(1_000 + 30 * DAY_MS - 1);
    assert_eq!(journal_of(&trees, "n").len(), 3);
    trees.journal_mut().set_clock(1_000 + 30 * DAY_MS);
    let rows = journal_of(&trees, "n");
    assert!(
        matches!(&rows[..], [(Field::Place, place), (Field::Content, content)] if place.starts_with("Local") && content.starts_with("Local")),
        "{rows:?}"
    );
}

#[test]
fn r_holding_a_node_again_restarts_its_30_days() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    let (_, node) = compact_edit(&mut trees);
    purged(&mut trees, "f", 1_000);
    trees.journal_mut().set_clock(1_000 + DAY_MS);
    trees.set_remote("f".into(), node);
    purged(&mut trees, "f", 1_000 + 20 * DAY_MS);
    trees.journal_mut().set_clock(1_000 + 31 * DAY_MS);
    assert_ne!(journal_of(&trees, "f"), []);
}

#[test]
fn a_backfilled_delete_of_a_folder_l_lacks_sees_what_s_held_below_it() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    for (node, shape) in [("d", dir(ROOT, "d")), ("f", file("d", "a", 1))] {
        trees.set_remote(node.into(), remote(shape.clone(), 1));
        trees.set_synced(node.into(), Some(shape));
    }
    let state = trees.state();
    trees.restore(state).unwrap();
    trees.backfill(None, false, 1_000);
    let seen: Vec<(NodeId, Field)> = (trees.journal().seen(&"d".into()).iter())
        .map(|entry| (entry.node.clone(), entry.field))
        .collect();
    assert_eq!(
        seen,
        [
            (NodeId::from("d"), Field::Place),
            (NodeId::from("d"), Field::Status),
            (NodeId::from("f"), Field::Place),
            (NodeId::from("f"), Field::Content),
            (NodeId::from("f"), Field::Status)
        ]
    );
    let (_, ops) = sent_with_intents(&mut trees);
    let unseen: Vec<usize> = (ops.iter())
        .filter_map(|op| match op {
            op::Op::Delete(delete) => Some(delete.unseen.len()),
            _ => None,
        })
        .collect();
    assert_eq!(unseen, [0]);
}

#[test]
fn a_relisted_node_whose_field_intents_did_not_change_keeps_no_separate_s_intents() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    let f = NodeId::from("f");
    synced(&mut trees, "f", file(ROOT, "f", 1), 1);
    let mut listed = trees.remote().get(&f).unwrap().clone();
    listed.meta_rev += 1;
    trees.set_listed_remote(f.clone(), listed);
    assert!(trees.synced_intents_apart_from_remote(&f).is_none());
}

#[test]
fn a_backfilled_delete_already_in_flight_gets_the_seen_list_of_what_s_held() {
    let mut t = Trees::new(ROOT, &IdGen::new(0));
    let shape = file(ROOT, "a", 1);
    t.set_remote("f".into(), remote(shape.clone(), 1));
    t.set_synced("f".into(), Some(shape));
    let content = held(&t, "f", Field::Content);
    let flight = stored_flight(op::Op::Delete(proto::Delete {
        node_id: "f".into(),
        ..Default::default()
    }));
    t.backfill(Some(&flight), false, 1_000);
    assert!(t.journal_row(&"f".into(), Field::Status).unwrap().is_sent());
    assert!(
        t.journal()
            .seen(&"f".into())
            .iter()
            .any(|e| e.intent == content)
    );
}

#[test]
fn a_backfilled_delete_sees_a_child_under_the_folder_its_waiting_update_moved_it_to() {
    let mut t = Trees::new(ROOT, &IdGen::new(0));
    synced(&mut t, "a", dir(ROOT, "a"), 1);
    let b = dir(ROOT, "b");
    t.set_remote("b".into(), remote(b.clone(), 1));
    t.set_synced("b".into(), Some(b));
    t.set_synced("f".into(), Some(file("a", "f", 1)));
    t.set_synced("g".into(), Some(file("b", "f", 2)));
    let moved = file("b", "f", 1);
    t.set_remote("f".into(), remote(moved.clone(), 2));
    let mut old = t.state();
    old.waiting.insert(
        "f".into(),
        Waiting {
            shape: Some(moved),
            origin: Origin::Commit(1),
        },
    );
    t.restore(old).unwrap();
    t.backfill(None, false, 1_000);
    assert!(
        t.journal()
            .seen(&"b".into())
            .iter()
            .any(|e| e.node == "f" && matches!(&e.value, Some(Value::Place(p)) if p.parent == "b"))
    );
}
#[test]
fn a_late_conflict_copy_keeps_the_row_of_an_edit_made_on_the_copy() {
    for features in [&[][..], &["field_intents"][..]] {
        let mut trees = Trees::new(ROOT, &IdGen::new(0));
        synced(&mut trees, "f", file(ROOT, "a", 1), 1);
        trees.set_local_changed("f".into(), local(file(ROOT, "a", 2)));
        let commit = build(&mut trees, features, 1_700);
        let copy = file(ROOT, "a (copy)", 2);
        synced(&mut trees, "copy", copy.clone(), 4);
        trees.set_local_changed("copy".into(), local(file(ROOT, "a (copy)", 3)));
        let edit = sent(&trees, "copy", Field::Content);
        let result = proto::OpResult {
            node_id: "copy".into(),
            ..outcome(op_result::Code::ConflictCopy)
        };
        let state = node_state("copy", &copy, 4, None);
        trees.record_results(&commit, &answered(vec![result], vec![state]));
        let row = trees.journal_row(&"copy".into(), Field::Content).unwrap();
        assert_eq!(
            (row.intent, &row.state),
            (edit, &RowState::Local { flight: None }),
            "{features:?}"
        );
    }
}

#[test]
fn a_superseded_replay_held_on_another_node_keeps_that_nodes_edit() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    synced(&mut trees, "f", file(ROOT, "a", 1), 1);
    trees.set_local_changed("f".into(), local(file(ROOT, "a", 2)));
    let commit = build(&mut trees, &["field_intents"], 1_700);
    let replayed = sent(&trees, "f", Field::Content);
    let copy = file(ROOT, "a (copy)", 2);
    synced(&mut trees, "copy", copy.clone(), 4);
    trees.set_local_changed("copy".into(), local(file(ROOT, "a (copy)", 3)));
    let edit = sent(&trees, "copy", Field::Content);
    let reported = proto::FieldIntents {
        content: Some(wire(replayed, 1_900)),
        ..Default::default()
    };
    let state = node_state("copy", &copy, 4, Some(reported));
    let superseded = proto::OpResult {
        node_id: "copy".into(),
        ..outcome(op_result::Code::Superseded)
    };
    trees.record_results(&commit, &answered(vec![superseded], vec![state]));
    let row = trees.journal_row(&"copy".into(), Field::Content).unwrap();
    assert_eq!(row.intent, edit);
}

// zz_cr 4619856240
#[test]
fn zz_cr_4619856240_a_version_restore_of_a_settled_file_names_its_content_intent() {
    use koidrive_engine::{ids::uuid_v7, restore::RestoreIntent};

    let n = uuid_v7(1, 1);
    let mut t = Trees::new(ROOT, &IdGen::new(0));
    synced(&mut t, &n, file(ROOT, "a", 2), 2);
    let mut c = Commits::default();
    c.connected(&Welcome { features: vec!["field_intents".into()], ..Default::default() });
    let restore = RestoreIntent::version(n.as_str().into(), uuid_v7(1, 2)).unwrap();
    assert!(c.restore(uuid_v7(2, 1), restore));
    t.rules_scanned();
    t.settle(c.aliases());
    let commit = c.build(&mut t, &mut IdGen::new(1), 3, "history", "me", 2).unwrap();
    let restore = commit.ops.iter()
        .find(|op| matches!(&op.op, Some(op::Op::Restore(r)) if !r.version_id.is_empty()))
        .unwrap();
    assert!(restore.intent.is_some());
}

// zz_cr 4624045763
#[test]
fn zz_cr_4624045763_backfilled_delete_names_the_acknowledged_content_version_in_held() {
    let mut t = Trees::new(ROOT, &IdGen::new(0));
    let a = file(ROOT, "a", 1);
    t.set_remote("f".into(), remote(a.clone(), 1));
    t.set_synced("f".into(), Some(a));
    t.set_remote("f".into(), remote(file(ROOT, "a", 2), 2));
    t.backfill(None, false, 1_000);
    let mut ops = [proto::Op {
        op: Some(op::Op::Delete(proto::Delete { node_id: "f".into(), ..Default::default() })),
        ..Default::default()
    }];
    t.stamp_intents(&mut ops, "me", 1_000, Wire::WithIntents);
    let Some(op::Op::Delete(d)) = &ops[0].op else { unreachable!() };
    assert!(d.held.contains(&IntentId::legacy_content("f", "v1").as_bytes().to_vec()));
}

// zz_cr 4624045767
#[test]
fn zz_cr_4624045767_a_folder_delete_includes_a_purged_download_seen_by_its_own_delete() {
    let mut t = Trees::new(ROOT, &IdGen::new(0));
    synced(&mut t, "d", dir(ROOT, "d"), 1);
    synced(&mut t, "f", file("d", "a", 1), 2);
    let f = NodeId::from("f");
    let d = NodeId::from("d");
    let content = held(&t, "f", Field::Content);
    t.remove_remote(&f);
    t.remove_local_by_user(&f);
    assert!(t.journal().seen(&f).iter().any(|e| e.intent == content));
    t.remove_local_by_user(&d);
    assert!(t.journal().seen(&d).iter().any(|e| e.node == f && e.intent == content));
}

// zz_cr 4641230633
#[test]
fn zz_cr_4641230633_relist_does_not_relabel_the_base_of_an_equal_byte_different_version() {
    let mut t = Trees::new(ROOT, &IdGen::new(0));
    let old = file(ROOT, "a", 1);
    synced(&mut t, "f", old.clone(), 1);
    let base = held(&t, "f", Field::Content);
    t.set_local_changed("f".into(), local(file(ROOT, "a", 2)));
    let mut listed = Tree::new(ROOT);
    listed.insert("f".into(), RemoteNode {
        intents: Some(NodeIntents {
            content: Some(FieldIntent::legacy(id(7))),
            ..Default::default()
        }),
        ..remote(old, 2)
    });
    t.take_listed_intents(&listed);
    assert_eq!(t.journal_row(&"f".into(), Field::Content).unwrap().base, Some(base));
}

// zz_cr 4646821320
#[test]
fn zz_cr_4646821320_a_late_node_less_result_does_not_add_a_newer_remote_edit_to_a_delete() {
    let mut t = Trees::new(ROOT, &IdGen::new(0));
    synced(&mut t, "f", file(ROOT, "a", 1), 1);
    t.set_local_changed("f".into(), local(file(ROOT, "a", 2)));
    let commit = build(&mut t, &[], 1_700);
    t.set_remote("f".into(), remote(file(ROOT, "a", 2), 2));
    t.remove_local_by_user(&"f".into());
    t.set_remote("f".into(), remote(file(ROOT, "a", 3), 4));
    let newer = held(&t, "f", Field::Content);
    let mut result = answered(vec![outcome(op_result::Code::Applied)], Vec::new());
    result.seq = 3;
    t.record_results(&commit, &result);
    let mut ops = [proto::Op {
        op: Some(op::Op::Delete(proto::Delete { node_id: "f".into(), ..Default::default() })),
        ..Default::default()
    }];
    t.stamp_intents(&mut ops, "me", 3_000, Wire::WithIntents);
    let Some(op::Op::Delete(d)) = &ops[0].op else { unreachable!() };
    assert!(d.unseen.contains(&newer.as_bytes().to_vec()));
}

// zz_cr 4646821331
#[test]
fn zz_cr_4646821331_an_applied_result_keeps_an_explicit_zero_time() {
    let mut t = Trees::new(ROOT, &IdGen::new(0));
    synced(&mut t, "f", file(ROOT, "a", 1), 1);
    let changed = file(ROOT, "a", 2);
    t.set_local_changed("f".into(), local(changed.clone()));
    let n = NodeId::from("f");
    let mut row = t.journal_row(&n, Field::Content).unwrap().into_owned();
    row.replay = true;
    t.journal_mut().replace(&n, row);
    let commit = build(&mut t, &["field_intents"], 0);
    let edit = sent(&t, "f", Field::Content);
    let field = proto::Intent { replay: true, ..wire(edit, 0) };
    let state = node_state("f", &changed, 2, Some(proto::FieldIntents {
        content: Some(field), ..Default::default()
    }));
    t.record_results(&commit, &answered(vec![outcome(op_result::Code::Applied)], vec![state]));
    assert_eq!(held_as(&t, "f", Field::Content), Some((edit, 0)));
}

// zz_cr 4646821344
#[test]
fn zz_cr_4646821344_a_conflict_copy_keeps_its_reported_content_applied_time() {
    let mut t = Trees::new(ROOT, &IdGen::new(0));
    synced(&mut t, "f", file(ROOT, "a", 1), 1);
    t.set_local_changed("f".into(), local(file(ROOT, "a", 2)));
    let n = NodeId::from("f");
    let mut row = t.journal_row(&n, Field::Content).unwrap().into_owned();
    row.replay = true;
    t.journal_mut().replace(&n, row);
    let commit = build(&mut t, &["field_intents"], 900);
    let edit = sent(&t, "f", Field::Content);
    let copy = node_state("copy", &file(ROOT, "a (copy)", 2), 4, Some(proto::FieldIntents {
        content: Some(proto::Intent { replay: true, ..wire(edit, 900) }),
        ..Default::default()
    }));
    t.record_results(&commit, &answered(
        vec![proto::OpResult { node_id: "copy".into(), ..outcome(op_result::Code::ConflictCopy) }],
        vec![copy],
    ));
    assert_eq!(held_as(&t, "copy", Field::Content), Some((edit, 900)));
    assert!(t.remote_history(&"copy".into(), Field::Content).iter()
        .any(|e| e.intent == edit && e.applied_at_ms == 900));
}

// zz_cr 4646821352
#[test]
fn zz_cr_4646821352_a_fresh_superseded_move_ends_even_when_r_holds_an_older_place() {
    let mut t = Trees::new(ROOT, &IdGen::new(0));
    synced(&mut t, "f", file(ROOT, "a", 1), 1);
    t.set_local_changed("f".into(), local(file(ROOT, "b", 1)));
    let commit = build(&mut t, &["field_intents"], 1_700);
    assert!(!commit.ops[0].intent.as_ref().unwrap().replay);
    let state = node_state("f", &file(ROOT, "c", 1), 2, Some(proto::FieldIntents {
        place: Some(wire(id(5), 1_000)), ..Default::default()
    }));
    let (again, _) = t.record_results(&commit, &answered(
        vec![outcome(op_result::Code::Superseded)], vec![state],
    ));
    assert!(!again.contains(&"f".into()));
    assert!(t.journal_row(&"f".into(), Field::Place).is_none());
}

// zz_cr 4619856293
#[test]
fn zz_cr_4619856293_a_planned_delete_fits_even_with_retained_history() {
    use koidrive_engine::journal::Seen;
    use prost::Message;
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    synced(&mut trees, "d", dir(ROOT, "d"), 1);
    let mut history = Vec::new();
    for i in 0u32..40 {
        let n = format!("f{i}");
        synced(&mut trees, &n, file("d", &n, 1), 1);
        for j in 0u32..60 {
            let mut bytes = [0x55; 16];
            bytes[..4].copy_from_slice(&(60 * i + j).to_be_bytes());
            history.push(Seen {
                node: NodeId::from(n.as_str()), field: Field::Content,
                intent: IntentId::from_bytes(&bytes).unwrap(), value: None,
            });
        }
        trees.remove_local_by_user(&NodeId::from(n.as_str()));
    }
    let d = NodeId::from("d");
    trees.remove_local_by_user(&d);
    history.extend_from_slice(trees.journal().seen(&d));
    trees.journal_mut().set_seen(&d, history);
    let mut commits = Commits::from_state(CommitsState::default()).unwrap();
    commits.connected(&Welcome { features: vec!["field_intents".into()], ..Default::default() });
    trees.rules_scanned();
    trees.settle(&Default::default());
    let base = Base { seq: 1, commit_id: String::new(), committed_at_ms: 1_700 };
    assert!(commits.build(&mut trees, &mut IdGen::new(1), 0, "history", "me", base.clone()).is_none());
    let guard = (commits.questions().iter())
        .find(|q| matches!(q.kind, koidrive_engine::question::QuestionKind::MassDelete { .. }))
        .unwrap().id.clone();
    assert!(commits.decide(&mut trees, &guard, koidrive_engine::question::Answer::Delete));
    let commit = commits.build(&mut trees, &mut IdGen::new(1), 0, "history", "me", base).unwrap();
    let ops: Vec<_> = commit.ops.iter().filter_map(|op| op.op.clone()).collect();
    let [op::Op::Delete(delete)] = ops.as_slice() else { panic!("{ops:?}") };
    assert_eq!(delete.node_id, "d");
    let member = proto::NodeState { trash_held: delete.held.clone(), ..Default::default() };
    assert!(41 * member.encoded_len() <= 1 << 20, "{} bytes per member", member.encoded_len());
}

// zz_cr 4644929393
#[test]
fn zz_cr_4644929393_a_place_retry_does_not_mask_a_terminal_content_result() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    let f = NodeId::from("f");
    synced(&mut trees, "f", file(ROOT, "a", 1), 1);
    let b = file(ROOT, "b", 2);
    trees.set_local_changed(f.clone(), local(b.clone()));
    let (mut commits, _) = sent_with_intents(&mut trees);
    let current = file(ROOT, "c", 3);
    trees.set_remote(f.clone(), remote(current.clone(), 2));
    let proof = node_state("f", &current, 2, Some(proto::FieldIntents {
        place: Some(wire(id(1), 1_000)),
        content: Some(wire(id(2), 1_900)), ..Default::default()
    }));
    let result = outcome(op_result::Code::Superseded);
    commits.received(proto::CommitResult {
        seq: 2, ..answered(vec![result.clone(), result], vec![proof])
    }).unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    assert_eq!(trees.synced().get(&f).unwrap().content, b.content);
}

// zz_cr 4619856262
#[test]
fn zz_cr_4619856262_a_superseded_move_whose_live_node_holds_an_earlier_place_does_not_end() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    synced(&mut trees, "f", file(ROOT, "a", 1), 1);
    trees.set_local_changed("f".into(), local(file(ROOT, "b", 1)));
    let (mut commits, ops) = sent_with_intents(&mut trees);
    assert!(matches!(&ops[..], [op::Op::Move(_)]), "{ops:?}");
    let results = vec![outcome(op_result::Code::Superseded)];
    acknowledge_superseded_move(&mut trees, &mut commits, results, file(ROOT, "c", 1), 1_000);
    let row = trees.journal_row(&"f".into(), Field::Place).unwrap();
    assert!(matches!(row.state, RowState::Local { .. }), "{row:?}");
    assert_eq!(trees.synced().get(&"f".into()), Some(&file(ROOT, "a", 1)));
    assert!(!commits.problems().contains(&NodeId::from("f")));
}

// zz_cr 4641230675
#[test]
fn zz_cr_4641230675_a_delete_of_a_folder_whose_create_is_in_flight_sees_the_create() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    trees.set_local_changed("d".into(), local(dir(ROOT, "d")));
    build(&mut trees, &["field_intents"], 1_700);
    let reported = proto::FieldIntents {
        place: Some(wire(sent(&trees, "d", Field::Place), 1_700)),
        content: None,
        status: Some(wire(sent(&trees, "d", Field::Status), 1_700)),
    };
    trees.remove_local_by_user(&"d".into());
    let state = node_state("d", &dir(ROOT, "d"), 1, Some(reported));
    trees.set_remote("d".into(), RemoteNode::from_state(&state).unwrap().1);
    let mut ops = vec![proto::Op {
        op: Some(op::Op::Delete(proto::Delete {
            node_id: "d".into(),
            ..Default::default()
        })),
        ..Default::default()
    }];
    let wanted = trees.journal_row(&"d".into(), Field::Status).unwrap().intent;
    let accepts = |op: &proto::Op| {
        matches!(&op.op, Some(op::Op::Delete(delete)) if delete.unseen.is_empty())
    };
    assert!(!accepts(&ops[0]), "the assertion accepts an untouched Delete");
    trees.stamp_intents(&mut ops, "me", 0, Wire::WithIntents);
    assert!(accepts(&ops[0]));
    assert_eq!(ops[0].intent.as_ref().unwrap().id, wanted.as_bytes().to_vec());
}

// zz_cr 4641230689
#[test]
fn zz_cr_4641230689_a_finish_names_the_marks_intent_and_leaves_a_later_rename_to_send() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    let mark = FieldIntent {
        id: id(5),
        author: "me".into(),
        replay: false,
        applied_at_ms: 0,
    };
    let marked = RemoteNode {
        pending_for: Some("d".into()),
        intents: Some(NodeIntents {
            place: Some(mark),
            content: None,
            status: Some(FieldIntent::legacy(id(6))),
            ..Default::default()
        }),
        ..remote(dir(ROOT, "d (pending)"), 1)
    };
    trees.set_remote("d".into(), marked);
    trees.set_synced("d".into(), Some(dir(ROOT, "d (pending)")));
    trees.set_local("d".into(), local(dir(ROOT, "d (pending)")));
    trees.set_local_changed("d".into(), local(dir(ROOT, "x")));
    let mut ops = vec![proto::Op {
        op: Some(op::Op::Move(proto::Move {
            node_id: "d".into(),
            new_parent_id: ROOT.into(),
            new_name: "d".into(),
            from: Some(proto::Place {
                parent_id: ROOT.into(),
                name: "d (pending)".into(),
            }),
            ..Default::default()
        })),
        ..Default::default()
    }];
    assert_eq!(
        trees.stamp_intents(&mut ops, "me", 0, Wire::WithIntents),
        OpsWithoutIntent(0)
    );
    assert_eq!(
        ops[0].intent.as_ref().unwrap().id,
        id(5).as_bytes().to_vec()
    );
    let finish = ops[0].intent.as_ref().unwrap();
    assert!(!finish.replay);
    assert_eq!(finish.over, id(5).as_bytes().to_vec());
    let row = trees.journal_row(&"d".into(), Field::Place).unwrap();
    assert!(matches!(row.state, RowState::Local { .. }));
}

#[test]
fn zz_cr_4650066677_a_mark_only_change_keeps_s_s_content_intent() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    let f = NodeId::from("f");
    synced(&mut trees, "f", file(ROOT, "a", 1), 1);
    trees.set_remote(f.clone(), remote(file(ROOT, "a", 1), 2));
    let kept = trees.synced_intents(&f).unwrap().content.unwrap().id;
    assert_eq!(kept, IntentId::legacy_content("f", "v1"));
    let mut marked = trees.remote().get(&f).unwrap().clone();
    marked.pending_for = Some("wanted".into());
    trees.set_remote(f.clone(), marked);
    assert_eq!(trees.synced_mark(&f), Some("wanted"));
    assert_eq!(trees.synced_intents(&f).unwrap().content.unwrap().id, kept);
}

#[test]
fn zz_cr_4650066677_a_mark_only_change_leaves_the_delete_and_edit_evidence_alone() {
    let run = |marked: bool, edit: bool| {
        let mut trees = Trees::new(ROOT, &IdGen::new(0));
        let f = NodeId::from("f");
        synced(&mut trees, "f", file(ROOT, "a", 1), 1);
        trees.set_remote(f.clone(), remote(file(ROOT, "a", 1), 2));
        if marked {
            let mut node = trees.remote().get(&f).unwrap().clone();
            node.pending_for = Some("wanted".into());
            trees.set_remote(f.clone(), node);
        }
        let mut moved_on = remote(file(ROOT, "a", 3), 3);
        moved_on.pending_for = marked.then(|| "wanted".into());
        trees.set_remote(f.clone(), moved_on);
        let mut ids: Vec<IntentId> = match edit {
            true => {
                trees.set_local_changed(f.clone(), local(file(ROOT, "a", 4)));
                let row = trees.journal_row(&f, Field::Content).unwrap();
                row.base.into_iter().collect()
            }
            false => {
                trees.remove_local_by_user(&f);
                (trees.journal().seen(&f).iter())
                    .map(|seen| seen.intent)
                    .collect()
            }
        };
        ids.sort();
        ids.dedup();
        ids
    };
    for edit in [false, true] {
        assert_eq!(run(true, edit), run(false, edit), "edit {edit}");
    }
}

#[test]
fn zz_cr_4650066663_intents_read_before_the_announcement_leave_no_computed_id_on_the_wire() {
    let server = NodeIntents {
        place: Some(FieldIntent::legacy(id(10))),
        content: Some(FieldIntent::legacy(IntentId::legacy_content("f", "v1"))),
        status: Some(FieldIntent::legacy(IntentId::legacy_live("f", 7))),
        ..Default::default()
    };
    let run = |read_before: bool, announced: bool| {
        let mut trees = Trees::new(ROOT, &IdGen::new(0));
        let f = NodeId::from("f");
        synced(&mut trees, "f", file(ROOT, "a", 1), 1);
        let renamed = file(ROOT, "b", 1);
        let node = |intents: Option<NodeIntents>| RemoteNode {
            meta_rev: 2,
            meta_edit_rev: 2,
            intents,
            ..remote(renamed.clone(), 1)
        };
        trees.set_remote(f.clone(), node(read_before.then(|| server.clone())));
        trees.set_synced(f.clone(), Some(renamed.clone()));
        trees.set_local(f.clone(), local(renamed.clone()));
        if announced {
            let mut listed: Tree<RemoteNode> = Tree::new(ROOT);
            listed.insert(f.clone(), node(Some(server.clone())));
            trees.take_listed_intents(&listed);
            trees.set_listed_remote(f.clone(), node(Some(server.clone())));
        }
        trees.remove_local_by_user(&f);
        let mut ops = vec![proto::Op {
            op: Some(op::Op::Delete(proto::Delete {
                node_id: "f".into(),
                ..Default::default()
            })),
            ..Default::default()
        }];
        let wire = match announced {
            true => Wire::WithIntents,
            false => Wire::WithoutIntents,
        };
        trees.stamp_intents(&mut ops, "me", 0, wire);
        ops
    };
    for announced in [false, true] {
        assert_eq!(
            run(true, announced),
            run(false, announced),
            "announced {announced}"
        );
    }
}

#[test]
fn zz_cr_4648946595_a_relist_keeps_a_delete_of_another_trash_group_apart() {
    let mut trees = Trees::new(ROOT, &IdGen::new(0));
    let f = NodeId::from("f");
    synced(&mut trees, "f", file(ROOT, "a", 1), 1);
    let trashed = |group: &str, rev: u64| RemoteNode {
        trashed: true,
        trash_group: group.into(),
        meta_rev: rev,
        meta_edit_rev: rev,
        ..remote(file(ROOT, "a", 1), 1)
    };
    trees.set_remote(f.clone(), trashed("g1", 2));
    let first = IntentId::legacy_trashed("f", "g1");
    let history = |trees: &Trees| -> Vec<IntentId> {
        (trees.journal().history(&f, Field::Status).iter())
            .map(|entry| entry.intent)
            .collect()
    };
    assert!(history(&trees).contains(&first));
    let mut listed: Tree<RemoteNode> = Tree::new(ROOT);
    let relisted = RemoteNode {
        intents: Some(NodeIntents {
            status: Some(FieldIntent::legacy(id(9))),
            ..Default::default()
        }),
        ..trashed("g2", 4)
    };
    listed.insert(f.clone(), relisted);
    trees.take_listed_intents(&listed);
    assert!(history(&trees).contains(&first), "{:?}", history(&trees));
}

// zz_cr 4651326485
#[test]
fn zz_cr_4651326485_a_folder_held_elsewhere_is_not_resent_while_its_child_waits() {
    let mut device = PurgedFolder::new(|trees| {
        trees.set_local_changed("z".into(), local(file("x", "z", 1)));
    });
    let mut rounds = Vec::new();
    for round in 0..6 {
        let ops = match round {
            0 => device.build_one(),
            _ => match device.build() {
                Some(ops) => ops,
                None => {
                    rounds.push(format!("{round}: no commit"));
                    break;
                }
            },
        };
        rounds.push(format!("{round}: {ops:?}"));
        if !ops.iter().any(|(id, _)| id == "x") {
            break;
        }
        let held = |field| Some(wire(sent(&device.trees, "x", field), 1_800 + round));
        let intents = proto::FieldIntents {
            place: held(Field::Place),
            status: held(Field::Status),
            content: None,
        };
        let holder = node_state("y", &dir("a", "x"), 2 + round as u64, Some(intents));
        let outcomes: Vec<_> = ops.iter().map(|(id, _)| match id.as_str() {
            "x" => proto::OpResult { node_id: id.clone(), ..outcome(op_result::Code::Superseded) },
            _ => proto::OpResult { node_id: String::new(), reject_reason: "NOT_FOUND".into(), ..outcome(op_result::Code::Rejected) },
        }).collect();
        device.seq += 1;
        let (id, node) = RemoteNode::from_state(&holder).unwrap();
        device.trees.set_remote_made_here(id, node);
        let result = proto::CommitResult { seq: device.seq, ..answered(outcomes, vec![holder]) };
        device.commits.received(result).unwrap();
        device.commits.acknowledge(&mut device.trees, device.seq);
    }
    assert!(rounds.len() < 4, "{rounds:#?}");
}
