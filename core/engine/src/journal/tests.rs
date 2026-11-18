// SPDX-License-Identifier: Apache-2.0

use super::*;

fn content(size: u64) -> Value {
    Value::Content {
        version_id: None,
        value: ContentValue {
            size,
            manifest_digest: vec![size as u8],
            executable: false,
        },
    }
}

fn place(parent: &str) -> Value {
    Value::Place(Place {
        parent: parent.into(),
        name: "x".into(),
        mark: None,
    })
}

fn whole(_: &RowKey, _: &HeldRow) -> Row {
    unreachable!("a journal alone keeps no compact row")
}

fn id(n: u8) -> IntentId {
    IntentId([n; 16])
}

fn held(n: u8, at: i64) -> FieldIntent {
    FieldIntent {
        id: id(n),
        author: "d1".into(),
        carried: false,
        applied_at_ms: at,
    }
}

fn seen(node: &str, field: Field, n: u8, value: Option<Value>) -> Seen {
    Seen {
        node: node.into(),
        field,
        intent: id(n),
        value,
    }
}

#[test]
fn legacy_ids_match_the_spec_vectors() {
    let vectors: serde_json::Value =
        serde_json::from_str(include_str!("../../../../spec/vectors/legacy-intents.json")).unwrap();
    let vectors = vectors["vectors"].as_array().unwrap();
    assert!(vectors.len() >= 6);
    for vector in vectors {
        let text = |key: &str| vector[key].as_str().unwrap();
        let got = IntentId::legacy(
            text("node_id"),
            Field::parse(text("field")).unwrap(),
            text("key"),
        );
        assert_eq!(got.to_string(), text("id"), "{vector}");
    }
    let id = |n: usize| vectors[n]["id"].as_str().unwrap();
    let node = "0190f5c2-0000-7000-8000-000000000001";
    assert_eq!(
        IntentId::legacy_content(node, "0190f5c2-0000-7000-8000-0000000000aa").to_string(),
        id(0)
    );
    assert_eq!(
        IntentId::legacy_place(node, 7, "0190f5c2-0000-7000-8000-0000000000ff", "a.txt")
            .to_string(),
        id(1)
    );
    assert_eq!(IntentId::legacy_live(node, 3).to_string(), id(4));
    assert_eq!(
        IntentId::legacy_trashed(node, "0190f5c2-0000-7000-8000-0000000000c1:0").to_string(),
        id(5)
    );
}

#[test]
fn minted_ids_are_version_7_and_round_trip() {
    let mut ids = IdGen::new(3);
    let minted = IntentId::mint(&mut ids, 0x0190_f3a0_1234);
    assert!(crate::ids::is_uuid_v7(&minted.to_string()));
    assert_eq!(
        crate::ids::unix_ms(&minted.to_string()),
        Some(0x0190_f3a0_1234)
    );
    assert_eq!(IntentId::parse(&minted.to_string()), Some(minted));
    assert_eq!(IntentId::from_bytes(minted.as_bytes()), Some(minted));
    assert_eq!(IntentId::from_bytes(&[1, 2]), None);
    assert_ne!(IntentId::mint(&mut ids, 1), IntentId::mint(&mut ids, 1));
}

#[test]
fn field_intents_round_trip_through_the_wire_message() {
    let intents = NodeIntents {
        place: Some(held(1, 5)),
        content: None,
        status: Some(FieldIntent {
            carried: true,
            ..held(2, 6)
        }),
    };
    assert_eq!(NodeIntents::from_proto(&intents.to_proto()), intents);
    assert!(intents.to_proto().status.unwrap().replay);
}

#[test]
fn a_change_of_a_local_row_keeps_its_base() {
    let mut journal = Journal::default();
    let first = journal.user_change("n", content(1), WireBase::None, Some(id(9)));
    let second = journal.user_change("n", content(2), WireBase::None, Some(id(8)));
    let row = journal.row("n", Field::Content).unwrap();
    assert_ne!(first, second);
    assert_eq!(row.intent, second);
    assert_eq!(row.base, Some(id(9)));
    assert_eq!(row.state, RowState::Local { flight: None });
}

#[test]
fn a_change_after_a_send_is_made_over_the_sent_intent_and_keeps_the_flight() {
    let mut journal = Journal::default();
    let first = journal.user_change("n", content(1), WireBase::None, Some(id(9)));
    journal.send("n", Field::Content, 50);
    let second = journal.user_change("n", content(2), WireBase::None, None);
    let row = journal.row("n", Field::Content).unwrap();
    assert_eq!(row.intent, second);
    assert_eq!(row.base, Some(first));
    let RowState::Local {
        flight: Some(flight),
    } = &row.state
    else {
        panic!("{row:?}");
    };
    assert_eq!(
        (flight.intent, flight.base, flight.time_ms),
        (first, Some(id(9)), 50)
    );
}

#[test]
fn a_value_returning_to_the_one_in_flight_takes_its_intent_back() {
    let mut journal = Journal::default();
    let first = journal.user_change("n", content(1), WireBase::None, Some(id(9)));
    journal.send("n", Field::Content, 50);
    journal.user_change("n", content(2), WireBase::None, None);
    let back = journal.user_change("n", content(1), WireBase::None, None);
    let row = journal.row("n", Field::Content).unwrap();
    assert_eq!(back, first);
    assert_eq!(
        (&row.state, row.base, row.time_ms),
        (&RowState::Sent, Some(id(9)), 50)
    );
}

#[test]
fn a_change_after_a_held_row_is_made_over_held_as() {
    let mut journal = Journal::default();
    let first = journal.user_change("n", content(1), WireBase::None, None);
    journal.send("n", Field::Content, 0);
    journal.applied("n", Field::Content, first, &held(7, 100), Some("v7".into()));
    journal.user_change("n", content(2), WireBase::None, None);
    assert_eq!(journal.row("n", Field::Content).unwrap().base, Some(id(7)));
}

#[test]
fn an_applied_result_holds_the_row_rebases_and_records_history() {
    let mut journal = Journal::default();
    let first = journal.user_change("n", content(1), WireBase::None, Some(id(9)));
    journal.send("n", Field::Content, 10);
    let second = journal.user_change("n", content(2), WireBase::None, None);
    journal.applied("n", Field::Content, first, &held(7, 100), Some("v7".into()));
    let row = journal.row("n", Field::Content).unwrap();
    assert_eq!((row.intent, row.base), (second, Some(id(7))));
    assert_eq!(row.state, RowState::Local { flight: None });
    let entry = HistoryEntry {
        intent: id(7),
        applied_at_ms: 100,
        in_local: true,
    };
    assert_eq!(journal.history("n", Field::Content), [entry]);
    let third = journal.user_change("m", content(3), WireBase::None, None);
    journal.send("m", Field::Content, 10);
    journal.applied("m", Field::Content, third, &held(6, 120), None);
    let row = journal.row("m", Field::Content).unwrap();
    let state = RowState::Held {
        held_as: id(6),
        version_id: None,
    };
    assert_eq!((&row.state, row.time_ms), (&state, 120));
}

#[test]
fn a_superseded_row_ends_and_rows_made_over_it_take_the_reported_intent() {
    let mut journal = Journal::default();
    let first = journal.user_delete("n", vec![seen("n", Field::Place, 3, None)], None);
    journal.send("n", Field::Status, 0);
    journal.superseded("n", Field::Status, first, Some(id(4)));
    assert_eq!(journal.row("n", Field::Status), None);
    assert!(journal.seen("n").is_empty());
    let a = journal.user_change("a", content(1), WireBase::None, None);
    journal.send("a", Field::Content, 0);
    journal.user_change("a", content(2), WireBase::None, None);
    journal.superseded("a", Field::Content, a, Some(id(4)));
    let row = journal.row("a", Field::Content).unwrap();
    assert_eq!(
        (row.base, &row.state),
        (Some(id(4)), &RowState::Local { flight: None })
    );
}

#[test]
fn a_held_row_ends_once_r_holds_a_later_intent() {
    let mut journal = Journal::default();
    let first = journal.user_change("n", content(1), WireBase::None, None);
    journal.send("n", Field::Content, 0);
    journal.applied("n", Field::Content, first, &held(7, 100), None);
    journal.remote_holds("n", Field::Content, &held(7, 100), true);
    assert!(journal.row("n", Field::Content).is_some());
    journal.remote_holds("n", Field::Content, &held(8, 200), false);
    assert_eq!(journal.row("n", Field::Content), None);
    assert_eq!(journal.history("n", Field::Content).len(), 2);
}

#[test]
fn a_record_reaching_r_does_not_end_a_local_or_sent_row() {
    let mut journal = Journal::default();
    journal.user_change("n", content(1), WireBase::None, Some(id(1)));
    journal.remote_holds("n", Field::Content, &held(1, 1), true);
    journal.remote_holds("n", Field::Content, &held(2, 2), false);
    assert!(journal.row("n", Field::Content).is_some());
    journal.send("n", Field::Content, 0);
    journal.remote_holds("n", Field::Content, &held(3, 3), false);
    assert!(journal.row("n", Field::Content).is_some());
}

#[test]
fn the_history_keeps_first_positions_and_the_newest_entries() {
    let mut journal = Journal::default();
    journal.remote_holds("n", Field::Place, &held(1, 1), false);
    journal.remote_holds("n", Field::Place, &held(2, 2), false);
    journal.remote_holds("n", Field::Place, &held(1, 1), true);
    let order: Vec<_> = (journal.history("n", Field::Place).iter())
        .map(|entry| (entry.intent, entry.in_local))
        .collect();
    assert_eq!(order, [(id(1), true), (id(2), false)]);
    assert!(journal.places_after("n", Field::Place, id(2), id(1)));
    assert!(!journal.places_after("n", Field::Place, id(1), id(2)));
    for n in 0..=HISTORY_KEPT as u8 {
        journal.remote_holds("m", Field::Place, &held(n, 1), false);
    }
    let history = journal.history("m", Field::Place);
    assert_eq!(history.len(), HISTORY_KEPT);
    assert_eq!(history[0].intent, id(1));
}

#[test]
fn local_holds_marks_the_history_entry() {
    let mut journal = Journal::default();
    journal.remote_holds("n", Field::Content, &held(1, 1), false);
    journal.local_holds("n", Field::Content, id(1));
    assert!(journal.history("n", Field::Content)[0].in_local);
}

#[test]
fn a_conflict_copy_moves_the_content_row_to_the_copy() {
    let mut journal = Journal::default();
    let sent = journal.user_change("n", content(1), WireBase::None, Some(id(9)));
    journal.send("n", Field::Content, 0);
    assert_eq!(journal.sent_intent("n", Field::Content), Some(sent));
    journal.held_elsewhere("n", Field::Content, sent, "copy", Some("v".into()), 50);
    assert_eq!(journal.row("n", Field::Content), None);
    let row = journal.row("copy", Field::Content).unwrap();
    let state = RowState::Held {
        held_as: sent,
        version_id: Some("v".into()),
    };
    assert_eq!((row.intent, &row.state, row.time_ms), (sent, &state, 50));
    assert_eq!(journal.history("copy", Field::Content)[0].intent, sent);
}

#[test]
fn a_merge_ends_place_and_status_and_moves_children_to_the_target() {
    let mut journal = Journal::default();
    journal.user_change("dir", place("root"), WireBase::None, None);
    journal.user_change("child", place("dir"), WireBase::None, None);
    journal.merged_into("dir", "target");
    assert_eq!(journal.row("dir", Field::Place), None);
    assert_eq!(
        journal.row("child", Field::Place).unwrap().value,
        place("target")
    );
}

#[test]
fn an_unsent_row_goes_back_to_local() {
    let mut journal = Journal::default();
    journal.user_change("n", place("root"), WireBase::None, None);
    assert!(journal.send("n", Field::Place, 3).unwrap().is_sent());
    journal.unsend("n", Field::Place);
    assert_eq!(
        journal.row("n", Field::Place).unwrap().state,
        RowState::Local { flight: None }
    );
    let unrecorded = journal.send_unrecorded("m", place("root"), WireBase::None, Some(id(1)), 4);
    assert_eq!(
        (
            unrecorded.state.clone(),
            unrecorded.base,
            unrecorded.time_ms
        ),
        (RowState::Sent, Some(id(1)), 4)
    );
}

#[test]
fn a_loss_marks_sent_and_held_rows_as_replays() {
    let mut journal = Journal::default();
    let a = journal.user_change("a", content(1), WireBase::None, None);
    journal.send("a", Field::Content, 0);
    journal.applied("a", Field::Content, a, &held(1, 1), None);
    journal.user_change("b", content(1), WireBase::None, None);
    journal.send("b", Field::Content, 0);
    journal.user_change("c", content(1), WireBase::None, None);
    journal.lose();
    let replay = |node| journal.row(node, Field::Content).unwrap().replay;
    assert_eq!((replay("a"), replay("b"), replay("c")), (true, true, false));
}

#[test]
fn a_delete_lists_what_r_holds_unseen_and_what_r_lost_held() {
    let list = vec![
        seen("a", Field::Content, 1, Some(content(1))),
        seen("b", Field::Content, 2, Some(content(2))),
        seen("b", Field::Content, 2, Some(content(2))),
    ];
    let remote = |node: &str, n: u8, value: Value| RemoteField {
        node: node.into(),
        field: Field::Content,
        intent: id(n),
        value,
    };
    let remote = vec![
        remote("a", 1, content(1)),
        remote("b", 3, content(2)),
        remote("c", 5, content(5)),
    ];
    let view = DeleteView {
        unseen: vec![id(5)],
        held: vec![id(2)],
    };
    assert_eq!(delete_view(&list, &remote), view);
}

#[test]
fn a_delete_keeps_the_newest_seen_entries_and_a_revival_ends_its_row() {
    let mut journal = Journal::default();
    let list: Vec<Seen> = (0..=HISTORY_KEPT as u8)
        .map(|n| seen("a", Field::Place, n, None))
        .collect();
    journal.user_delete("a", list, Some(id(200)));
    assert_eq!(journal.seen("a").len(), HISTORY_KEPT);
    assert_eq!(journal.seen("a")[0].intent, id(1));
    assert_eq!(journal.row("a", Field::Status).unwrap().base, Some(id(200)));
    journal.user_revived("a");
    assert_eq!(journal.row("a", Field::Status), None);
    assert!(journal.seen("a").is_empty());
}

#[test]
fn replacing_computed_ids_reaches_rows_history_and_seen_lists() {
    let mut journal = Journal::default();
    journal.remote_holds("n", Field::Content, &held(1, 1), true);
    let sent = journal.user_change("n", content(1), WireBase::None, Some(id(1)));
    journal.send("n", Field::Content, 0);
    journal.applied("n", Field::Content, sent, &held(2, 2), None);
    journal.user_delete("d", vec![seen("d", Field::Place, 1, None)], Some(id(1)));
    journal.replace_ids(&BTreeMap::from([(id(1), id(11)), (id(2), id(12))]));
    let row = journal.row("n", Field::Content).unwrap();
    let state = RowState::Held {
        held_as: id(12),
        version_id: None,
    };
    assert_eq!((&row.state, row.base), (&state, Some(id(11))));
    let history: Vec<_> = (journal.history("n", Field::Content).iter())
        .map(|entry| entry.intent)
        .collect();
    assert_eq!(history, [id(11), id(12)]);
    assert_eq!(journal.seen("d")[0].intent, id(11));
    assert_eq!(journal.row("d", Field::Status).unwrap().base, Some(id(11)));
}

#[test]
fn the_generator_is_its_own_stream_and_survives_a_restore() {
    let mut engine = IdGen::new(42);
    let mut journal = Journal::default();
    journal.seed(engine.state);
    engine.next_u64();
    let mut earlier: Vec<String> = (0..64).map(|_| engine.uuid_v7(0)).collect();
    let minted: Vec<IntentId> = (0..64).map(|_| journal.mint()).collect();
    earlier.extend(minted.iter().map(IntentId::to_string));
    let mut restored = Journal::restore(journal.state(whole));
    restored.seed(IdGen::new(42).state);
    let next = restored.mint().to_string();
    assert!(!earlier.contains(&next), "{next}");
    assert_eq!(earlier.iter().collect::<BTreeSet<_>>().len(), earlier.len());
    assert_eq!(
        restored.take_changes(whole).ids,
        Some(restored.state(whole).ids)
    );
}

#[test]
fn changes_name_only_what_changed_and_a_state_restores() {
    let mut journal = Journal::default();
    journal.user_change("n", content(1), WireBase::None, None);
    journal.take_changes(whole);
    let before = journal.state(whole);
    journal.remote_holds("m", Field::Place, &held(1, 1), false);
    journal.set_listed(true);
    let changes = journal.take_changes(whole);
    assert!(changes.rows.is_empty());
    assert_eq!(changes.history.len(), 1);
    assert_eq!(changes.listed, Some(true));
    assert_eq!(
        JournalChanges::between(&before, &journal.state(whole)),
        changes
    );
    assert_eq!(Journal::restore(journal.state(whole)), journal);
}

#[test]
fn a_compact_row_that_leaves_and_ends_before_a_take_reaches_the_store_as_ended() {
    for replaced in [false, true] {
        let mut journal = Journal::default();
        let first = journal.user_change("n", content(1), WireBase::None, None);
        journal.send("n", Field::Content, 0);
        journal.applied("n", Field::Content, first, &held(7, 100), None);
        let key = ("n".to_owned(), Field::Content);
        let row = journal.row("n", Field::Content).cloned().unwrap();
        journal.take_changes(whole);
        assert!(journal.compact(key.clone(), true));
        match replaced {
            false => journal.write_out(key.clone(), row),
            true => {
                journal.user_change("n", content(2), WireBase::None, None);
            }
        }
        journal.end("n", Field::Content);
        assert_eq!(
            journal.take_changes(whole).rows,
            [(key, None)],
            "{replaced}"
        );
    }
}
