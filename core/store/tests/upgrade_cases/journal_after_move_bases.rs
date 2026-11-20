// SPDX-License-Identifier: Apache-2.0

upgrade_case!(journal_after_move_bases, |store| {
    use koidrive_engine::classify::Trees;
    use koidrive_engine::ids::IdGen;
    use koidrive_engine::journal::{Field, IntentId, RowState};
    use koidrive_engine::state::{Agreed, MoveHistory, MovePosition};
    use koidrive_engine::tree::{NodeId, RemoteNode};
    use koidrive_proto::v1::{Commit, Intent, Move, Op, op};
    use prost::Message;
    use std::collections::BTreeMap;

    let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/alpha6.db");
    let dir = tempfile::tempdir().unwrap();
    let mut ids = IdGen::new(9);
    let mut at = |rev, name: &str, trash_group: Option<&str>| MovePosition {
        place_id: ids.uuid_v7_bytes(rev),
        record: Agreed {
            rev,
            parent: "".into(),
            name: name.into(),
            trash_group: trash_group.map(str::to_owned),
        },
    };
    let (result, newest, oldest) = (at(3, "report.txt", None), at(2, "old.txt", None), at(1, "earlier.txt", Some("delete:0")));
    let (flight_newest, flight_oldest, other) = (at(1, "deck.txt", None), at(1, "slides.txt", None), at(3, "report.txt", None));
    let id_of = |position: &MovePosition| IntentId::from_bytes(&position.place_id).unwrap();
    let history_of = |trees: &Trees, node: &NodeId| -> Vec<IntentId> {
        (trees.remote_history(node, Field::Place).iter()).map(|entry| entry.intent).collect()
    };
    for variant in ["exact", "rev", "place", "trash", "null"] {
        let path = dir.path().join(format!("{variant}.db"));
        std::fs::copy(fixture, &path).unwrap();
        let conn = rusqlite::Connection::open(&path).unwrap();
        koidrive_store::migrations::register_functions(&conn).unwrap();
        let applied: Vec<String> = (conn.prepare("SELECT name FROM migrations").unwrap())
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for m in (koidrive_store::migrations::MIGRATIONS.iter()).filter(|m| {
            !applied.iter().any(|name| name == m.name) && m.name != "journal" && !m.requires.contains(&"journal")
        }) {
            conn.execute_batch(m.sql).unwrap();
            conn.execute("INSERT INTO migrations (name) VALUES (?1)", [m.name]).unwrap();
        }
        let node = |name: &str| -> (String, String, u64) {
            conn.query_row("SELECT node_id, parent_id, meta_rev FROM remote_nodes WHERE name = ?1", [name], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap()
        };
        let ((report, folder, rev), (deck, slides, _)) = (node("report.txt"), node("deck.txt"));
        assert_eq!(rev, result.record.rev);
        let placed = |position: &MovePosition, parent: &str| MovePosition {
            record: Agreed { parent: parent.into(), ..position.record.clone() },
            ..position.clone()
        };
        let mut result = placed(&result, &folder);
        match variant {
            "rev" => result.record.rev += 1,
            "trash" => result.record.trash_group = Some("delete:1".into()),
            _ => {}
        }
        let history = MoveHistory { result: result.clone(), bases: vec![placed(&newest, &folder), placed(&oldest, &folder)] };
        let flight_bases = vec![placed(&flight_newest, &slides), placed(&flight_oldest, &slides)];
        let stamped = Intent {
            id: match variant {
                "place" => other.place_id.to_vec(),
                _ => result.place_id.to_vec(),
            },
            author_device_id: "fixture-device".into(),
            replay: true,
            over: Vec::new(),
            applied_at_ms: 1_794_000_000_000,
        };
        let mut commit = conn
            .query_row("SELECT commit_message FROM commit_in_flight WHERE id = 1", [], |row| row.get::<_, Vec<u8>>(0))
            .map(|bytes| Commit::decode(bytes.as_slice()).unwrap())
            .unwrap();
        commit.ops.push(Op {
            op: Some(op::Op::Move(Move { node_id: deck.clone(), new_parent_id: slides.clone(), new_name: "deck2.txt".into(), ..Default::default() })),
            ..Default::default()
        });
        let null = variant == "null";
        conn.execute(
            "UPDATE remote_nodes SET place_intent = ?2 WHERE node_id = ?1",
            rusqlite::params![report, (!null).then(|| stamped.encode_to_vec())],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO moved_here (node_id, seq, upgraded, move_history) VALUES (?1, ?2, 0, ?3)",
            rusqlite::params![report, rev, (!null).then(|| serde_json::to_vec(&history).unwrap())],
        )
        .unwrap();
        let staged_bases = BTreeMap::from([(NodeId::from(deck.as_str()), flight_bases.clone())]);
        conn.execute(
            "UPDATE commit_in_flight SET commit_message = ?1, move_bases = ?2 WHERE id = 1",
            rusqlite::params![commit.encode_to_vec(), (!null).then(|| serde_json::to_vec(&staged_bases).unwrap())],
        )
        .unwrap();
        drop(conn);

        let mut reopened = Store::open(&path).unwrap();
        let (remote, state) = reopened
            .read(|tx| Ok::<_, koidrive_store::Error>((tx.remote_nodes()?, tx.engine_state()?)))
            .unwrap();
        let state_of = |id: &str| remote.iter().find(|state| state.node_id == id).unwrap().clone();
        let place = state_of(&report).intents.and_then(|intents| intents.place);
        assert_eq!(place, (!null).then(|| stamped.clone()), "{variant}");
        let (report, deck) = (NodeId::from(report.as_str()), NodeId::from(deck.as_str()));
        let moved = &state.trees.moved_here[&report];
        assert_eq!(moved.history.as_deref(), (!null).then_some(&history), "{variant}");
        let in_flight = state.commits.in_flight.clone().unwrap();
        assert_eq!(in_flight.move_bases, if null { BTreeMap::new() } else { staged_bases.clone() }, "{variant}");

        let root = remote.iter().find(|state| state.parent_id.is_empty()).unwrap();
        let mut trees = Trees::new(root.node_id.as_str(), &IdGen::new(1));
        for wire in remote.iter().filter(|state| !state.parent_id.is_empty()) {
            let (id, node) = RemoteNode::from_state(wire).unwrap();
            trees.set_remote(id.clone(), node);
            trees.set_remote_place_id(&id, RemoteNode::place_id(wire));
        }
        let mut restored = trees.state();
        restored.moved_here.insert(report.clone(), moved.clone());
        let edit_rev = trees.remote().get(&report).unwrap().edit_rev;
        restored.edited_here.insert(report.clone(), edit_rev);
        trees.restore(restored).unwrap();
        trees.backfill(Some(&in_flight), false, false, 1_800_000_000_000);
        let version = &trees.remote().get(&report).unwrap().version_id;
        assert!(!version.is_empty(), "{variant}");
        let edited = IntentId::legacy_content(&report.to_string(), version);
        let content = trees.journal_row(&report, Field::Content).unwrap();
        assert!(matches!(content.state, RowState::Held { held_as, .. } if held_as == edited), "{variant}");
        let held = trees.journal_row(&report, Field::Place).map(|row| row.into_owned());
        let sent = trees.journal_row(&deck, Field::Place).unwrap().into_owned();
        assert_eq!(sent.state, RowState::Sent, "{variant}");
        let exact = variant == "exact";
        assert_eq!(held.as_ref().and_then(|row| row.base) == Some(id_of(&newest)), exact, "{variant}");
        assert_eq!(history_of(&trees, &report).starts_with(&[id_of(&oldest), id_of(&newest)]), exact, "{variant}");
        let stamped_id = IntentId::from_bytes(&stamped.id).unwrap();
        assert_eq!(held.unwrap().intent == stamped_id, !null, "{variant}");
        assert_eq!(sent.base == Some(id_of(&flight_newest)), !null, "{variant}");
        assert_eq!(history_of(&trees, &deck).contains(&id_of(&flight_oldest)), !null, "{variant}");

        let records = trees.remote_records(|_| true);
        assert_eq!(records[&report].move_bases.is_empty(), !exact, "{variant}");
        if !exact {
            continue;
        }
        let base = &history.bases[0];
        let listed = |rev: u64, name: &str, trashed: bool| RemoteNode {
            meta_rev: rev,
            trashed,
            trash_group: if trashed { "delete:2".into() } else { String::new() },
            shape: koidrive_engine::tree::Shape { name: name.into(), ..records[&report].node.shape.clone() },
            ..records[&report].node.clone()
        };
        for (relisted, clears) in [
            (listed(base.record.rev, &base.record.name, false), true),
            (listed(base.record.rev + 1, &base.record.name, false), false),
            (listed(base.record.rev, "elsewhere.txt", false), false),
            (listed(base.record.rev, &base.record.name, true), false),
        ] {
            let mut lost = trees.clone();
            lost.revert(report.clone());
            lost.set_listed_remote(report.clone(), relisted);
            lost.set_remote_place_id(&report, Some(base.place_id));
            let moved = lost.moved_here();
            let (trashed, edited) = (lost.trashed_here(), lost.edited_here());
            let synced = lost.taken_from_others(|_| true);
            lost.forget_lost(trashed, edited, synced, moved, Vec::new(), records.clone(), &Default::default(), 0, 0);
            assert_eq!(lost.reverted().contains(&report), !clears);
        }
    }
    store.read(|tx| tx.engine_state()).unwrap();
});
