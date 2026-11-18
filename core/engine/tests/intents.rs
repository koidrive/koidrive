// SPDX-License-Identifier: Apache-2.0

use koidrive_engine::classify::Trees;
use koidrive_engine::journal::{
    Field, FieldIntent, HistoryEntry, IntentId, NodeIntents, Place, RowState, Value, WireBase,
};
use koidrive_engine::state::ContentValue;
use koidrive_engine::tree::{Content, FileIdentity, Kind, LocalNode, RemoteNode, Shape};

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
        [entry(first, true), entry(second, false)]
    );
}

#[test]
fn an_edit_writes_a_content_row_over_r_s_intent_and_keeps_r_s_value_in_the_history() {
    let mut trees = Trees::new(ROOT);
    synced(&mut trees, "f", file(ROOT, "a", 1), 1);
    let seen = held(&trees, "f", Field::Content);
    trees.set_local_changed("f".into(), local(file(ROOT, "a", 2)));
    let row = trees.journal().row("f", Field::Content).unwrap();
    assert_eq!(row.base, Some(seen));
    assert_eq!(
        row.value,
        Value::Content {
            version_id: None,
            value: ContentValue::from(&content(2)),
        }
    );
    assert_eq!(row.state, RowState::Local { flight: None });
    assert!(trees.journal().row("f", Field::Place).is_none());
    assert!(trees.journal().row("f", Field::Status).is_none());
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
    let row = trees.journal().row("f", Field::Place).unwrap();
    assert_eq!(row.base, Some(seen));
    let at = |parent: &str, name: &str| Place {
        parent: parent.into(),
        name: name.into(),
        mark: None,
    };
    assert_eq!(row.value, Value::Place(at("d", "b")));
    assert_eq!(row.wire_base, WireBase::Place(at(ROOT, "a")));
    assert!(trees.journal().row("f", Field::Content).is_none());
}

#[test]
fn a_scan_that_finds_what_s_holds_writes_no_row() {
    let mut trees = Trees::new(ROOT);
    trees.set_remote("f".into(), remote(file(ROOT, "a", 1), 1));
    trees.set_synced("f".into(), Some(file(ROOT, "a", 1)));
    trees.set_local_changed("f".into(), local(file(ROOT, "a", 1)));
    assert_eq!(trees.journal().rows().count(), 0);
}

#[test]
fn a_create_writes_a_row_for_each_of_its_fields_and_a_delete_before_any_send_ends_them() {
    let mut trees = Trees::new(ROOT);
    trees.set_local_changed("f".into(), local(file(ROOT, "a", 1)));
    let fields: Vec<Field> = trees
        .journal()
        .rows()
        .map(|((_, field), _)| *field)
        .collect();
    assert_eq!(fields, [Field::Place, Field::Content, Field::Status]);
    assert!(trees.journal().rows().all(|(_, row)| row.base.is_none()));
    trees.remove_local_by_user("f");
    assert_eq!(trees.journal().rows().count(), 0);
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
    let row = trees.journal().row("d", Field::Status).unwrap();
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
