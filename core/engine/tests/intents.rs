// SPDX-License-Identifier: Apache-2.0

use koidrive_engine::classify::{Origin, Trees, Waiting};
use koidrive_engine::commit::{Base, Commits};
use koidrive_engine::effects::{BatchLimit, Effects};
use koidrive_engine::ids::IdGen;
use koidrive_engine::journal::{
    Author, Field, FieldIntent, HistoryEntry, IntentId, NodeIntents, Place, Row, RowState, Value,
    WireBase,
};
use koidrive_engine::state::{CommitsState, ContentValue};
use koidrive_engine::tree::{Content, FileIdentity, Kind, LocalNode, RemoteNode, Shape, Tree};
use koidrive_proto::v1::{self as proto, Welcome, op, op_result};

const ROOT: &str = "root";

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
    }
}

fn id(n: u8) -> IntentId {
    IntentId::from_bytes(&[n; 16]).unwrap()
}

fn held(trees: &Trees, node: &str, field: Field) -> IntentId {
    let remote = trees.remote().get(node).unwrap();
    remote.intents.as_ref().unwrap().get(field).unwrap().id
}

fn synced(trees: &mut Trees, node: &str, shape: Shape, rev: u64) {
    trees.set_remote(node.into(), remote(shape.clone(), rev));
    trees.set_synced(node.into(), Some(shape.clone()));
    trees.set_local(node.into(), local(shape));
}

#[test]
fn a_node_sent_without_intents_gets_legacy_ids_and_keeps_its_live_id_while_live() {
    let mut trees = Trees::new(ROOT);
    trees.set_remote("f".into(), remote(file(ROOT, "Café.txt", 1), 3));
    assert_eq!(
        trees.remote().get("f").unwrap().intents,
        Some(NodeIntents {
            place: Some(FieldIntent::legacy(IntentId::legacy_place(
                "f",
                3,
                ROOT,
                "Café.txt"
            ))),
            content: Some(FieldIntent::legacy(IntentId::legacy_content("f", "v3"))),
            status: Some(FieldIntent::legacy(IntentId::legacy_live("f", 3))),
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
            .get("d")
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
    let mut trees = Trees::new(ROOT);
    let sent = NodeIntents {
        place: Some(FieldIntent {
            id: id(1),
            author: "d1".into(),
            carried: true,
            applied_at_ms: 40,
        }),
        content: None,
        status: Some(FieldIntent::legacy(id(2))),
    };
    trees.set_remote(
        "f".into(),
        RemoteNode {
            intents: Some(sent.clone()),
            ..remote(file(ROOT, "a", 1), 1)
        },
    );
    assert_eq!(trees.remote().get("f").unwrap().intents, Some(sent));
}

#[test]
fn the_history_follows_r_until_r_changes_and_keeps_a_downloaded_value_marked() {
    let mut trees = Trees::new(ROOT);
    trees.set_remote("f".into(), remote(file(ROOT, "a", 1), 1));
    let first = held(&trees, "f", Field::Content);
    assert!(trees.journal().history("f", Field::Content).is_empty());
    let entry = |intent, in_local| HistoryEntry {
        intent,
        applied_at_ms: 0,
        in_local,
    };
    assert_eq!(
        trees.remote_history("f", Field::Content),
        [entry(first, false)]
    );
    trees.set_local("f".into(), local(file(ROOT, "a", 1)));
    assert_eq!(
        trees.remote_history("f", Field::Content),
        [entry(first, true)]
    );
    trees.set_remote("f".into(), remote(file(ROOT, "a", 2), 2));
    let second = held(&trees, "f", Field::Content);
    assert_eq!(
        trees.journal().history("f", Field::Content),
        [entry(first, true)]
    );
    assert_eq!(
        trees.remote_history("f", Field::Content),
        [entry(first, true), entry(second, false)]
    );
}

#[test]
fn an_edit_writes_a_content_row_over_r_s_intent_and_keeps_r_s_value_in_the_history() {
    let mut trees = Trees::new(ROOT);
    synced(&mut trees, "f", file(ROOT, "a", 1), 1);
    let seen = held(&trees, "f", Field::Content);
    trees.set_local_changed("f".into(), local(file(ROOT, "a", 2)));
    let row = trees.journal_row("f", Field::Content).unwrap();
    assert_eq!(row.base, Some(seen));
    assert_eq!(
        row.value,
        Value::Content {
            version_id: None,
            value: ContentValue::from(&content(2)),
        }
    );
    assert_eq!(row.state, RowState::Local { flight: None });
    assert!(trees.journal_row("f", Field::Place).is_none());
    assert!(trees.journal_row("f", Field::Status).is_none());
    assert_eq!(
        trees.journal().history("f", Field::Content),
        [HistoryEntry {
            intent: seen,
            applied_at_ms: 0,
            in_local: true,
        }]
    );
}

#[test]
fn a_move_writes_a_place_row_that_sends_the_place_it_left() {
    let mut trees = Trees::new(ROOT);
    synced(&mut trees, "d", dir(ROOT, "d"), 1);
    synced(&mut trees, "f", file(ROOT, "a", 1), 1);
    let seen = held(&trees, "f", Field::Place);
    trees.set_local_changed("f".into(), local(file("d", "b", 1)));
    let row = trees.journal_row("f", Field::Place).unwrap();
    assert_eq!(row.base, Some(seen));
    let at = |parent: &str, name: &str| Place {
        parent: parent.into(),
        name: name.into(),
        mark: None,
    };
    assert_eq!(row.value, Value::Place(at("d", "b")));
    assert_eq!(row.wire_base, WireBase::Place(at(ROOT, "a")));
    assert!(trees.journal_row("f", Field::Content).is_none());
}

#[test]
fn a_scan_that_finds_what_s_holds_writes_no_row() {
    let mut trees = Trees::new(ROOT);
    trees.set_remote("f".into(), remote(file(ROOT, "a", 1), 1));
    trees.set_synced("f".into(), Some(file(ROOT, "a", 1)));
    trees.set_local_changed("f".into(), local(file(ROOT, "a", 1)));
    assert_eq!(trees.journal_rows().count(), 0);
}

#[test]
fn a_create_writes_a_row_for_each_of_its_fields_and_a_delete_before_any_send_ends_them() {
    let mut trees = Trees::new(ROOT);
    trees.set_local_changed("f".into(), local(file(ROOT, "a", 1)));
    let fields: Vec<Field> = (trees.journal_rows())
        .map(|((_, field), _)| *field)
        .collect();
    assert_eq!(fields, [Field::Place, Field::Content, Field::Status]);
    assert!(trees.journal_rows().all(|(_, row)| row.base.is_none()));
    trees.remove_local_by_user("f");
    assert_eq!(trees.journal_rows().count(), 0);
    assert!(trees.local().get("f").is_none());
}

#[test]
fn a_delete_lists_what_l_held_for_the_node_and_below_it() {
    let mut trees = Trees::new(ROOT);
    synced(&mut trees, "d", dir(ROOT, "d"), 1);
    synced(&mut trees, "f", file("d", "a", 1), 1);
    trees.set_remote("f".into(), remote(file("d", "a", 2), 2));
    let old = IntentId::legacy_content("f", "v1");
    trees.remove_local_by_user("d");
    let row = trees.journal_row("d", Field::Status).unwrap();
    assert_eq!(row.value, Value::Status { live: false });
    assert_eq!(row.base, Some(held(&trees, "d", Field::Status)));
    let listed: Vec<(&str, Field, IntentId, bool)> = (trees.journal().seen("d").iter())
        .map(|entry| {
            (
                entry.node.as_str(),
                entry.field,
                entry.intent,
                entry.value.is_some(),
            )
        })
        .collect();
    assert_eq!(
        listed,
        [
            ("d", Field::Place, held(&trees, "d", Field::Place), true),
            ("d", Field::Status, held(&trees, "d", Field::Status), true),
            (
                "f",
                Field::Place,
                IntentId::legacy_place("f", 1, "d", "a"),
                false
            ),
            ("f", Field::Place, held(&trees, "f", Field::Place), true),
            ("f", Field::Content, old, true),
            ("f", Field::Status, held(&trees, "f", Field::Status), true),
        ]
    );
}

#[test]
fn s_keeps_the_intent_of_the_value_it_holds_when_r_moves_on() {
    let mut trees = Trees::new(ROOT);
    synced(&mut trees, "f", file(ROOT, "a", 1), 1);
    let first = held(&trees, "f", Field::Content);
    assert_eq!(
        trees
            .synced_intents("f")
            .unwrap()
            .content
            .map(|intent| intent.id),
        Some(first)
    );
    trees.set_remote("f".into(), remote(file(ROOT, "a", 2), 2));
    assert_eq!(
        trees
            .synced_intents("f")
            .unwrap()
            .content
            .map(|intent| intent.id),
        Some(first)
    );
    trees.set_synced("f".into(), Some(file(ROOT, "a", 2)));
    assert_eq!(
        trees
            .synced_intents("f")
            .unwrap()
            .content
            .map(|intent| intent.id),
        Some(held(&trees, "f", Field::Content))
    );
}

#[test]
fn s_keeps_the_intents_of_what_it_holds_when_r_loses_the_node() {
    let mut trees = Trees::new(ROOT);
    synced(&mut trees, "f", file(ROOT, "a", 1), 1);
    let before = trees.synced_intents("f");
    assert!(
        before
            .as_ref()
            .is_some_and(|intents| intents.content.is_some())
    );
    trees.remove_remote("f");
    assert_eq!(trees.synced_intents("f"), before);
}

#[test]
fn the_s_row_is_written_again_when_r_moves_on_from_the_value_s_holds() {
    let mut trees = Trees::new(ROOT);
    synced(&mut trees, "f", file(ROOT, "a", 1), 1);
    trees.take_changed();
    trees.set_remote("f".into(), remote(file(ROOT, "a", 2), 2));
    assert!(trees.take_changed().synced.contains("f"));
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
    let row = trees.journal_row(node, field).unwrap();
    proto::Intent {
        id: row.intent.as_bytes().to_vec(),
        author_device_id: "me".into(),
        ..Default::default()
    }
}

#[test]
fn a_commit_names_its_intents_only_when_the_server_announces_them() {
    for (features, named) in [(&[][..], false), (&["field_intents"][..], true)] {
        let mut trees = Trees::new(ROOT);
        trees.set_local_changed("f".into(), local(file(ROOT, "a", 1)));
        let commit = build(&mut trees, features, 1_700);
        let [create] = &commit.ops[..] else {
            panic!("{commit:?}");
        };
        assert!(matches!(create.op, Some(op::Op::CreateFile(_))));
        for field in Field::ALL {
            let row = trees.journal_row("f", field).unwrap();
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
    let mut trees = Trees::new(ROOT);
    synced(&mut trees, "f", file(ROOT, "a", 1), 1);
    let seen = held(&trees, "f", Field::Place);
    trees.set_local("f".into(), local(file(ROOT, "b", 1)));
    assert!(trees.journal_row("f", Field::Place).is_none());
    let commit = build(&mut trees, &["field_intents"], 9);
    assert!(matches!(&commit.ops[..], [op] if matches!(op.op, Some(op::Op::Move(_)))));
    let row = trees.journal_row("f", Field::Place).unwrap();
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
    let mut trees = Trees::new(ROOT);
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
    assert_eq!(trees.stamp_intents(&mut ops, "me", 0, true), 1);
    assert_eq!(ops[0].intent, None);
    assert_eq!(trees.journal_rows().count(), 0);
}

#[test]
fn a_delete_sends_what_r_holds_that_l_never_held_as_unseen() {
    let mut trees = Trees::new(ROOT);
    synced(&mut trees, "d", dir(ROOT, "d"), 1);
    synced(&mut trees, "f", file("d", "a", 1), 1);
    trees.remove_local_by_user("f");
    trees.remove_local_by_user("d");
    trees.set_remote("g".into(), remote(file("d", "g", 1), 2));
    let mut ops = vec![proto::Op {
        op: Some(op::Op::Delete(proto::Delete {
            node_id: "d".into(),
            ..Default::default()
        })),
        ..Default::default()
    }];
    assert_eq!(trees.stamp_intents(&mut ops, "me", 0, true), 0);
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
    let mut trees = Trees::new(ROOT);
    synced(&mut trees, "d", dir(ROOT, "d"), 1);
    trees.set_local("d".into(), local(dir(ROOT, "e")));
    trees.remove_local_by_user("d");
    let mut ops = vec![proto::Op {
        op: Some(op::Op::Delete(proto::Delete {
            node_id: "d".into(),
            ..Default::default()
        })),
        ..Default::default()
    }];
    assert_eq!(trees.stamp_intents(&mut ops, "me", 0, true), 0);
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
        parent_id: shape.parent.clone(),
        name: shape.name.clone(),
        kind: match shape.kind {
            Kind::Dir => proto::NodeKind::Dir,
            Kind::File => proto::NodeKind::File,
        } as i32,
        status: proto::NodeStatus::Live as i32,
        content_rev: rev,
        meta_rev: rev,
        current: shape.content.clone().map(|content| proto::VersionInfo {
            version_id: format!("v{rev}"),
            content: Some(content),
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
    trees.journal_row(node, field).unwrap().intent
}

fn held_as(trees: &Trees, node: &str, field: Field) -> Option<(IntentId, i64)> {
    let row = trees.journal_row(node, field)?;
    match row.state {
        RowState::Held { held_as, .. } => Some((held_as, row.time_ms)),
        _ => None,
    }
}

#[test]
fn an_applied_result_holds_each_row_as_the_node_state_reports_it() {
    let mut trees = Trees::new(ROOT);
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
            .history("f", Field::Status)
            .iter()
            .any(|entry| entry.intent == id(7))
    );
}

#[test]
fn a_result_without_intents_holds_rows_as_the_legacy_ids_of_its_node_state() {
    let mut trees = Trees::new(ROOT);
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
fn a_superseded_move_ends_unless_the_server_applied_another_place_before_it() {
    for (applied_at_ms, stays) in [(2_500, false), (1_000, true)] {
        let mut trees = Trees::new(ROOT);
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
        let row = trees.journal_row("f", Field::Place);
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
fn a_superseded_row_whose_intent_another_node_holds_moves_there_as_held() {
    let mut trees = Trees::new(ROOT);
    synced(&mut trees, "f", file(ROOT, "a", 1), 1);
    trees.set_local_changed("f".into(), local(file(ROOT, "b", 1)));
    let commit = build(&mut trees, &["field_intents"], 1_700);
    let place = sent(&trees, "f", Field::Place);
    let reported = proto::FieldIntents {
        place: Some(wire(place, 1_900)),
        ..Default::default()
    };
    let state = node_state("g", &file(ROOT, "b", 1), 2, Some(reported));
    let superseded = proto::OpResult {
        node_id: "g".into(),
        ..outcome(op_result::Code::Superseded)
    };
    trees.record_results(&commit, &answered(vec![superseded], vec![state]));
    assert_eq!(trees.journal_row("f", Field::Place), None);
    assert_eq!(held_as(&trees, "g", Field::Place), Some((place, 1_900)));
}

#[test]
fn a_conflict_copy_takes_the_content_row_as_held() {
    let mut trees = Trees::new(ROOT);
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
    assert_eq!(trees.journal_row("f", Field::Content), None);
    let row = trees.journal_row("copy", Field::Content).unwrap();
    let state = RowState::Held {
        held_as: edit,
        version_id: Some("v4".into()),
    };
    assert_eq!((row.intent, &row.state), (edit, &state));
}

#[test]
fn a_fallback_create_merged_into_a_folder_ends_its_place_and_status_and_moves_its_childrens_rows() {
    let mut trees = Trees::new(ROOT);
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
    assert_eq!(trees.journal_row("d", Field::Place), None);
    assert_eq!(trees.journal_row("d", Field::Status), None);
    let child = trees.journal_row("c", Field::Place).unwrap();
    assert!(matches!(&child.value, Value::Place(place) if place.parent == "t"));
}

#[test]
fn a_create_answered_id_exists_ends_its_live_status_row_and_keeps_the_rest_to_send_again() {
    for (reason, ended) in [("ID_EXISTS", true), ("NAME_TAKEN", false)] {
        let mut trees = Trees::new(ROOT);
        trees.set_local_changed("f".into(), local(file(ROOT, "a", 1)));
        let commit = build(&mut trees, &["field_intents"], 1_700);
        let rejected = proto::OpResult {
            reject_reason: reason.into(),
            ..outcome(op_result::Code::Rejected)
        };
        trees.record_results(&commit, &answered(vec![rejected], Vec::new()));
        assert_eq!(trees.journal_row("f", Field::Status).is_none(), ended);
        for field in [Field::Place, Field::Content] {
            let row = trees.journal_row("f", field).unwrap();
            assert_eq!(row.state, RowState::Local { flight: None });
        }
    }
}

#[test]
fn a_create_names_the_author_of_each_field_it_carries() {
    let mut trees = Trees::new(ROOT);
    trees.set_local_changed("f".into(), local(file(ROOT, "a", 1)));
    let mut status = trees.journal_row("f", Field::Status).unwrap().into_owned();
    status.author = Author::Carried("b".into());
    trees.journal_mut().replace("f", status);
    let commit = build(&mut trees, &["field_intents"], 1_700);
    let fields = commit.ops[0].fields.clone().unwrap();
    let author = |intent: Option<proto::Intent>| intent.unwrap().author_device_id;
    assert_eq!(author(fields.place), "me");
    assert_eq!(author(fields.content), "me");
    assert_eq!(author(fields.status), "b");
}

#[test]
fn a_held_row_keeps_its_value_and_time_when_r_takes_an_earlier_intent() {
    let mut trees = Trees::new(ROOT);
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
        carried: false,
        applied_at_ms: 1_000,
    });
    trees.set_remote("f".into(), node);
    let row = trees.journal_row("f", Field::Content).unwrap();
    assert_eq!(held_as(&trees, "f", Field::Content), Some((edit, 1_900)));
    let edited = Value::Content {
        version_id: None,
        value: ContentValue::from(&content(2)),
    };
    assert!(row.value.same(&edited));
}

#[test]
fn an_acknowledged_commit_holds_the_rows_its_results_prove_as_r_holds_them() {
    let mut trees = Trees::new(ROOT);
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
    assert!(trees.journal().compact_row("f", Field::Content).is_some());
    (
        trees.journal_row("f", Field::Content).unwrap().into_owned(),
        node,
    )
}

#[test]
fn a_compact_row_is_written_out_unchanged_whenever_r_stops_giving_it() {
    for trigger in ["earlier", "listed", "moved", "lost", "loss"] {
        let mut trees = Trees::new(ROOT);
        let (original, mut node) = compact_edit(&mut trees);
        match trigger {
            "earlier" | "listed" => {
                node.shape = file(ROOT, "a", 1);
                node.version_id = "v1".into();
                let first = trees.remote_history("f", Field::Content)[0].intent;
                node.intents.as_mut().unwrap().content = Some(FieldIntent {
                    id: first,
                    author: "me".into(),
                    carried: false,
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
            "lost" => trees.remove_remote("f"),
            _ => {
                let (none, merged) = (Default::default(), Default::default());
                trees.forget_lost(vec![], vec![], vec![], vec![], vec![], none, &merged, 0, 0);
            }
        }
        let row = trees.journal_row("f", Field::Content).unwrap().into_owned();
        let expected = Row {
            replay: trigger == "loss",
            ..original
        };
        assert_eq!(row, expected, "{trigger}");
        let compact = trees.journal().compact_row("f", Field::Content).is_some();
        assert_eq!(compact, trigger == "moved", "{trigger}");
    }
}

#[test]
fn the_store_keeps_compact_rows_whole_and_a_reload_compacts_them_again() {
    let mut trees = Trees::new(ROOT);
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
        let mut reloaded = Trees::new(ROOT);
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
            .compact_row("f", Field::Content)
            .is_some()
    );
    assert!(reloaded.take_changes().journal.rows.is_empty());
    reloaded.remove_remote("f");
    assert!(
        reloaded
            .journal()
            .compact_row("f", Field::Content)
            .is_none()
    );
    assert_eq!(
        reloaded
            .journal_row("f", Field::Content)
            .unwrap()
            .into_owned(),
        original
    );
    assert!(reloaded.take_changes().journal.rows.is_empty());
    let mut reloaded = reload();
    let mut later = trees.remote().get("f").cloned().unwrap();
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
    let mut trees = Trees::new(ROOT);
    let (row, _) = compact_edit(&mut trees);
    let RowState::Held { held_as, .. } = row.state else {
        panic!("{row:?}");
    };
    let entries = trees.journal().history("f", Field::Content).to_vec();
    assert!(
        entries.iter().all(|entry| entry.intent != held_as),
        "{entries:?}"
    );
    assert_eq!(
        trees
            .remote_history("f", Field::Content)
            .last()
            .map(|entry| entry.intent),
        Some(held_as)
    );
    let first = trees.remote_history("f", Field::Content)[0].intent;
    for (moved_to, shape) in [(id(9), file(ROOT, "a", 3)), (first, file(ROOT, "a", 1))] {
        let mut trees = Trees::new(ROOT);
        let (_, mut node) = compact_edit(&mut trees);
        node.shape = shape;
        node.version_id = "v3".into();
        node.intents.as_mut().unwrap().content = Some(FieldIntent::legacy(moved_to));
        trees.set_remote("f".into(), node);
        let explicit: Vec<IntentId> = (trees.journal().history("f", Field::Content).iter())
            .map(|entry| entry.intent)
            .collect();
        assert_eq!(explicit.last(), Some(&held_as), "{explicit:?}");
        assert!(!explicit.contains(&id(9)), "{explicit:?}");
    }
}

#[test]
fn a_create_into_a_purged_folder_takes_the_place_the_server_reports_and_never_sends_the_folder_again()
 {
    let mut trees = Trees::new(ROOT);
    synced(&mut trees, "a", dir(ROOT, "a"), 1);
    synced(&mut trees, "x", dir("a", "x"), 1);
    trees.set_local_changed("z".into(), local(file("x", "z", 1)));
    trees.remove_remote("x");
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
    let parent = |trees: &Trees, id| trees.local().get(id).map(|node| node.shape.parent.clone());
    assert_eq!(parent(&trees, "z").as_deref(), Some("a"));
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

fn backfilled(table: &str, waiting: Option<Waiting>) -> Trees {
    let mut trees = Trees::new(ROOT);
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
    trees.backfill(None, false);
    trees
}

fn rows_of(trees: &Trees, node: &str) -> Vec<(Field, Row)> {
    (trees.journal_rows())
        .filter(|((id, _), _)| id == node)
        .map(|((_, field), row)| (*field, row.into_owned()))
        .collect()
}

#[test]
fn a_backfill_carries_every_field_of_a_lost_folder_as_a_replay() {
    let trees = backfilled("lost_creates", None);
    let place = IntentId::legacy_place("d", 0, ROOT, "d");
    let status = IntentId::legacy_live("d", 0);
    let carried = |field, intent, value| {
        let state = RowState::Local { flight: None };
        (
            field,
            Row {
                intent,
                author: Author::Carried(String::new()),
                base: None,
                value,
                wire_base: WireBase::None,
                state,
                time_ms: 0,
                replay: true,
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
        ]
    );
}

#[test]
fn a_backfill_carries_every_field_of_a_folder_owed_a_create_without_a_replay() {
    let trees = backfilled("unsent_creates", None);
    let rows = rows_of(&trees, "d");
    let carried: Vec<(Field, &Author, bool)> = (rows.iter())
        .map(|(field, row)| (*field, &row.author, row.replay))
        .collect();
    let empty = Author::Carried(String::new());
    assert_eq!(
        carried,
        [
            (Field::Place, &empty, false),
            (Field::Status, &empty, false)
        ]
    );
}

#[test]
fn a_backfilled_lost_folder_whose_queued_removal_finishes_keeps_no_rows() {
    let removal = Waiting {
        shape: None,
        origin: Origin::Other,
    };
    let mut trees = backfilled("lost_creates", Some(removal));
    assert_eq!(rows_of(&trees, "d").len(), 2);
    trees.remove_local("d");
    trees.settle(&Default::default());
    assert_eq!(rows_of(&trees, "d"), []);
    let mut sending = backfilled("unsent_creates", None);
    build(&mut sending, &["field_intents"], 1_700);
    sending.remove_local("d");
    sending.settle(&Default::default());
    let states: Vec<RowState> = (rows_of(&sending, "d").into_iter())
        .map(|(_, row)| row.state)
        .collect();
    assert_eq!(states, [RowState::Sent, RowState::Sent]);
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
fn the_first_listing_that_names_intents_replaces_the_ids_computed_for_the_values_it_lists() {
    let mut trees = Trees::new(ROOT);
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
        .synced_intents_apart_from_remote("g")
        .unwrap()
        .place
        .clone();
    trees.set_local_changed("f".into(), local(file(ROOT, "a", 3)));
    let server = |n| NodeIntents {
        place: Some(FieldIntent::legacy(id(n))),
        content: Some(FieldIntent::legacy(id(n + 1))),
        status: Some(FieldIntent::legacy(id(n + 2))),
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
    assert_eq!(trees.remote().get("f").unwrap().intents, Some(server(1)));
    assert_eq!(trees.synced_intents("f"), Some(server(1)));
    assert_eq!(
        trees.journal_row("f", Field::Content).unwrap().base,
        Some(id(2))
    );
    let history: Vec<IntentId> = (trees.journal().history("f", Field::Content).iter())
        .map(|entry| entry.intent)
        .collect();
    assert_eq!(history, [id(2)]);
    assert_eq!(held(&trees, "g", Field::Place), place_of_g);
    assert_eq!(held(&trees, "g", Field::Content), id(5));
    let synced_g = trees.synced_intents_apart_from_remote("g").unwrap();
    assert_eq!(synced_g.place, synced_place_of_g);
    assert_eq!(
        synced_g.content.as_ref().map(|intent| intent.id),
        Some(id(5))
    );
    assert!(trees.journal().listed());
}

#[test]
fn an_edit_superseded_on_a_trashed_file_ends_and_leaves_the_delete_to_reach_l() {
    let mut trees = Trees::new(ROOT);
    synced(&mut trees, "f", file(ROOT, "a", 1), 1);
    trees.set_local_changed("f".into(), local(file(ROOT, "a", 2)));
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
    (commits.build(&mut trees, &mut IdGen::new(1), 0, "history", "me", base)).unwrap();
    let trashed = RemoteNode {
        trashed: true,
        meta_rev: 2,
        ..remote(file(ROOT, "a", 1), 1)
    };
    trees.set_remote("f".into(), trashed);
    let result = proto::CommitResult {
        seq: 2,
        results: vec![outcome(op_result::Code::Superseded)],
        ..Default::default()
    };
    commits.received(result).unwrap();
    assert!(commits.acknowledge(&mut trees, 2));
    assert!(commits.problems().is_empty(), "{:?}", commits.problems());
    assert_eq!(trees.synced().get("f"), Some(&file(ROOT, "a", 2)));
}
