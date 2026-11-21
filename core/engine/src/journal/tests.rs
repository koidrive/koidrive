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
        replay: false,
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
            &[text("key")],
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
            replay: true,
            ..held(2, 6)
        }),
        ..Default::default()
    };
    assert_eq!(NodeIntents::from_proto(&intents.to_proto()), intents);
    assert!(intents.to_proto().status.unwrap().replay);
}

#[test]
fn a_change_of_a_local_row_keeps_its_base() {
    let mut journal = Journal::new(&IdGen::new(0));
    let first = journal.user_change(&"n".into(), content(1), WireBase::None, Some(id(9)));
    let second = journal.user_change(&"n".into(), content(2), WireBase::None, Some(id(8)));
    let row = journal.row(&"n".into(), Field::Content).unwrap();
    assert_ne!(first, second);
    assert_eq!(row.intent, second);
    assert_eq!(row.base, Some(id(9)));
    assert_eq!(row.state, RowState::Local { flight: None });
}

#[test]
fn a_change_after_a_send_is_made_over_the_sent_intent_and_keeps_the_flight() {
    let mut journal = Journal::new(&IdGen::new(0));
    let first = journal.user_change(&"n".into(), content(1), WireBase::None, Some(id(9)));
    journal.send(&"n".into(), Field::Content, 50);
    let second = journal.user_change(&"n".into(), content(2), WireBase::None, None);
    let row = journal.row(&"n".into(), Field::Content).unwrap();
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
    let mut journal = Journal::new(&IdGen::new(0));
    let first = journal.user_change(&"n".into(), content(1), WireBase::None, Some(id(9)));
    journal.send(&"n".into(), Field::Content, 50);
    journal.user_change(&"n".into(), content(2), WireBase::None, None);
    let back = journal.user_change(&"n".into(), content(1), WireBase::None, None);
    let row = journal.row(&"n".into(), Field::Content).unwrap();
    assert_eq!(back, first);
    assert_eq!(
        (&row.state, row.base, row.time_ms),
        (&RowState::Sent, Some(id(9)), 50)
    );
}

#[test]
fn a_value_returning_to_a_carried_flight_keeps_its_author_replay_and_time() {
    let mut journal = Journal::new(&IdGen::new(0));
    let f = NodeId::from("f");
    let author = Author::Carried(None);
    journal.replace(
        &f,
        Row {
            intent: id(1),
            author: author.clone(),
            base: None,
            value: content(1),
            wire_base: WireBase::None,
            state: RowState::Sent,
            time_ms: 12,
            replay: true,
        },
    );
    journal.user_change(&f, content(2), WireBase::None, None);
    assert_eq!(
        journal.user_change(&f, content(1), WireBase::None, None),
        id(1)
    );
    let row = journal.row(&f, Field::Content).unwrap();
    assert_eq!((&row.author, row.replay, row.time_ms), (&author, true, 12));
}

#[test]
fn a_change_after_a_held_row_is_made_over_held_as() {
    let mut journal = Journal::new(&IdGen::new(0));
    let first = journal.user_change(&"n".into(), content(1), WireBase::None, None);
    journal.send(&"n".into(), Field::Content, 0);
    journal.applied(
        &"n".into(),
        Field::Content,
        first,
        &held(7, 100),
        Some("v7".into()),
        true,
    );
    journal.user_change(&"n".into(), content(2), WireBase::None, None);
    assert_eq!(
        journal.row(&"n".into(), Field::Content).unwrap().base,
        Some(id(7))
    );
}

#[test]
fn an_applied_result_holds_the_row_rebases_and_records_history() {
    let mut journal = Journal::new(&IdGen::new(0));
    let first = journal.user_change(&"n".into(), content(1), WireBase::None, Some(id(9)));
    journal.send(&"n".into(), Field::Content, 10);
    let second = journal.user_change(&"n".into(), content(2), WireBase::None, None);
    journal.applied(
        &"n".into(),
        Field::Content,
        first,
        &held(7, 100),
        Some("v7".into()),
        true,
    );
    let row = journal.row(&"n".into(), Field::Content).unwrap();
    assert_eq!((row.intent, row.base), (second, Some(id(7))));
    assert_eq!(row.state, RowState::Local { flight: None });
    let entry = HistoryEntry {
        intent: id(7),
        applied_at_ms: 100,
        in_local: true,
    };
    assert_eq!(journal.history(&"n".into(), Field::Content), [entry]);
    let third = journal.user_change(&"m".into(), content(3), WireBase::None, None);
    journal.send(&"m".into(), Field::Content, 10);
    journal.applied(
        &"m".into(),
        Field::Content,
        third,
        &held(6, 120),
        None,
        true,
    );
    let row = journal.row(&"m".into(), Field::Content).unwrap();
    let state = RowState::Held {
        held_as: id(6),
        version_id: None,
    };
    assert_eq!((&row.state, row.time_ms), (&state, 120));
}

#[test]
fn a_superseded_row_ends_and_rows_made_over_it_take_the_reported_intent() {
    let mut journal = Journal::new(&IdGen::new(0));
    let first = journal.user_delete(&"n".into(), vec![seen("n", Field::Place, 3, None)], None);
    journal.send(&"n".into(), Field::Status, 0);
    journal.superseded(&"n".into(), Field::Status, first, Some(id(4)));
    assert_eq!(journal.row(&"n".into(), Field::Status), None);
    assert!(journal.seen(&"n".into()).is_empty());
    let a = journal.user_change(&"a".into(), content(1), WireBase::None, None);
    journal.send(&"a".into(), Field::Content, 0);
    journal.user_change(&"a".into(), content(2), WireBase::None, None);
    journal.superseded(&"a".into(), Field::Content, a, Some(id(4)));
    let row = journal.row(&"a".into(), Field::Content).unwrap();
    assert_eq!(
        (row.base, &row.state),
        (Some(id(4)), &RowState::Local { flight: None })
    );
}

#[test]
fn results_visit_the_rows_they_answer_however_many_the_journal_holds() {
    let answer = |nodes: usize| {
        let mut journal = Journal::new(&IdGen::new(0));
        let mut sent = Vec::new();
        for n in 0..nodes {
            let file = NodeId::from(format!("f{n}"));
            sent.push(journal.user_change(&file, content(1), WireBase::None, None));
            journal.send(&file, Field::Content, 0);
            journal.user_change(&file, content(2), WireBase::None, None);
            let place = Place {
                parent: format!("d{n}").into(),
                name: file.to_string(),
                mark: None,
            };
            journal.user_change(&file, Value::Place(place), WireBase::None, None);
        }
        rows::VISITED.with(|visited| visited.set(0));
        for (n, sent) in sent.into_iter().enumerate() {
            let file = NodeId::from(format!("f{n}"));
            match n % 2 {
                0 => journal.applied(&file, Field::Content, sent, &held(7, 100), None, true),
                _ => journal.superseded(&file, Field::Content, sent, Some(id(8))),
            }
            journal.merged_into(&format!("d{n}").into(), &"target".into(), true);
        }
        let visited = rows::VISITED.with(std::cell::Cell::get);
        for n in 0..nodes {
            let file = NodeId::from(format!("f{n}"));
            let base = journal.row(&file, Field::Content).unwrap().base;
            assert_eq!(base, Some(id(7 + (n % 2) as u8)));
            let place = &journal.row(&file, Field::Place).unwrap().value;
            assert!(matches!(place, Value::Place(place) if place.parent == "target"));
        }
        visited
    };
    assert_eq!(answer(1000), 100 * answer(10));
}

#[test]
fn a_held_row_ends_once_r_holds_a_later_intent() {
    let mut journal = Journal::new(&IdGen::new(0));
    let first = journal.user_change(&"n".into(), content(1), WireBase::None, None);
    journal.send(&"n".into(), Field::Content, 0);
    journal.applied(
        &"n".into(),
        Field::Content,
        first,
        &held(7, 100),
        None,
        true,
    );
    journal.remote_holds(&"n".into(), Field::Content, &held(7, 100), true);
    assert!(journal.row(&"n".into(), Field::Content).is_some());
    journal.remote_holds(&"n".into(), Field::Content, &held(8, 200), false);
    assert_eq!(journal.row(&"n".into(), Field::Content), None);
    assert_eq!(journal.history(&"n".into(), Field::Content).len(), 2);
}

#[test]
fn a_record_reaching_r_does_not_end_a_local_or_sent_row() {
    let mut journal = Journal::new(&IdGen::new(0));
    journal.user_change(&"n".into(), content(1), WireBase::None, Some(id(1)));
    journal.remote_holds(&"n".into(), Field::Content, &held(1, 1), true);
    journal.remote_holds(&"n".into(), Field::Content, &held(2, 2), false);
    assert!(journal.row(&"n".into(), Field::Content).is_some());
    journal.send(&"n".into(), Field::Content, 0);
    journal.remote_holds(&"n".into(), Field::Content, &held(3, 3), false);
    assert!(journal.row(&"n".into(), Field::Content).is_some());
}

#[test]
fn the_history_keeps_first_positions_and_the_newest_entries() {
    let mut journal = Journal::new(&IdGen::new(0));
    journal.remote_holds(&"n".into(), Field::Place, &held(1, 1), false);
    journal.remote_holds(&"n".into(), Field::Place, &held(2, 2), false);
    journal.remote_holds(&"n".into(), Field::Place, &held(1, 1), true);
    let order: Vec<_> = (journal.history(&"n".into(), Field::Place).iter())
        .map(|entry| (entry.intent, entry.in_local))
        .collect();
    assert_eq!(order, [(id(1), true), (id(2), false)]);
    assert!(journal.places_after(&"n".into(), Field::Place, id(2), id(1)));
    assert!(!journal.places_after(&"n".into(), Field::Place, id(1), id(2)));
    for n in 0..=HISTORY_KEPT as u8 {
        journal.remote_holds(&"m".into(), Field::Place, &held(n, 1), false);
    }
    let history = journal.history(&"m".into(), Field::Place);
    assert_eq!(history.len(), HISTORY_KEPT);
    assert_eq!(history[0].intent, id(1));
}

#[test]
fn local_holds_marks_the_history_entry() {
    let mut journal = Journal::new(&IdGen::new(0));
    journal.remote_holds(&"n".into(), Field::Content, &held(1, 1), false);
    journal.local_holds(&"n".into(), Field::Content, id(1));
    assert!(journal.history(&"n".into(), Field::Content)[0].in_local);
}

#[test]
fn a_loss_marks_sent_and_held_rows_as_replays() {
    let mut journal = Journal::new(&IdGen::new(0));
    let a = journal.user_change(&"a".into(), content(1), WireBase::None, None);
    journal.send(&"a".into(), Field::Content, 0);
    journal.applied(&"a".into(), Field::Content, a, &held(1, 1), None, true);
    journal.user_change(&"b".into(), content(1), WireBase::None, None);
    journal.send(&"b".into(), Field::Content, 0);
    journal.user_change(&"c".into(), content(1), WireBase::None, None);
    journal.lose();
    let replay = |node: &str| journal.row(&node.into(), Field::Content).unwrap().replay;
    assert_eq!((replay("a"), replay("b"), replay("c")), (true, true, false));
}

#[test]
fn sending_a_replay_again_keeps_its_original_time() {
    let mut journal = Journal::new(&IdGen::new(0));
    let f = NodeId::from("f");
    journal.replace(
        &f,
        Row {
            intent: id(1),
            author: Author::Own,
            base: None,
            value: content(1),
            wire_base: WireBase::None,
            state: RowState::Sent,
            time_ms: 12,
            replay: true,
        },
    );
    journal.unsend(&f, Field::Content);
    let row = journal.send(&f, Field::Content, 34).unwrap();
    assert_eq!(row.time_ms, 12);
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
    let mut journal = Journal::new(&IdGen::new(0));
    let list: Vec<Seen> = (0..=HISTORY_KEPT as u8)
        .map(|n| seen("a", Field::Place, n, None))
        .collect();
    journal.user_delete(&"a".into(), list, Some(id(200)));
    assert_eq!(journal.seen(&"a".into()).len(), HISTORY_KEPT);
    assert_eq!(journal.seen(&"a".into())[0].intent, id(1));
    assert_eq!(
        journal.row(&"a".into(), Field::Status).unwrap().base,
        Some(id(200))
    );
    journal.user_revived(&"a".into());
    assert_eq!(journal.row(&"a".into(), Field::Status), None);
    assert!(journal.seen(&"a".into()).is_empty());
}

#[test]
fn replacing_computed_ids_reaches_rows_history_and_seen_lists() {
    let mut journal = Journal::new(&IdGen::new(0));
    journal.remote_holds(&"n".into(), Field::Content, &held(1, 1), true);
    let sent = journal.user_change(&"n".into(), content(1), WireBase::None, Some(id(1)));
    journal.send(&"n".into(), Field::Content, 0);
    journal.applied(&"n".into(), Field::Content, sent, &held(2, 2), None, true);
    journal.user_delete(
        &"d".into(),
        vec![seen("d", Field::Place, 1, None)],
        Some(id(1)),
    );
    journal.replace_ids(&BTreeMap::from([(id(1), id(11)), (id(2), id(12))]));
    let row = journal.row(&"n".into(), Field::Content).unwrap();
    let state = RowState::Held {
        held_as: id(12),
        version_id: None,
    };
    assert_eq!((&row.state, row.base), (&state, Some(id(11))));
    let history: Vec<_> = (journal.history(&"n".into(), Field::Content).iter())
        .map(|entry| entry.intent)
        .collect();
    assert_eq!(history, [id(11), id(12)]);
    assert_eq!(journal.seen(&"d".into())[0].intent, id(11));
    assert_eq!(
        journal.row(&"d".into(), Field::Status).unwrap().base,
        Some(id(11))
    );
}

fn deleted_after_a_send(journal: &mut Journal) -> IntentId {
    let sent = journal.user_change(&"n".into(), content(1), WireBase::None, None);
    journal.send(&"n".into(), Field::Content, 0);
    let entry = Seen {
        node: "n".into(),
        field: Field::Content,
        intent: sent,
        value: Some(content(1)),
    };
    journal.user_delete(&"d".into(), vec![entry], None);
    sent
}

fn seen_ids(journal: &Journal) -> Vec<IntentId> {
    journal
        .seen(&"d".into())
        .iter()
        .map(|entry| entry.intent)
        .collect()
}

#[test]
fn an_applied_result_adds_its_held_as_next_to_the_sent_intent_in_a_seen_list() {
    let mut journal = Journal::new(&IdGen::new(0));
    let sent = deleted_after_a_send(&mut journal);
    journal.applied(&"n".into(), Field::Content, sent, &held(2, 2), None, true);
    assert_eq!(seen_ids(&journal), [sent, id(2)]);
}

#[test]
fn a_superseded_result_leaves_the_reported_intent_unseen() {
    let mut journal = Journal::new(&IdGen::new(0));
    let sent = deleted_after_a_send(&mut journal);
    journal.superseded(&"n".into(), Field::Content, sent, Some(id(2)));
    assert_eq!(seen_ids(&journal), [sent]);
    let remote = [RemoteField {
        node: "n".into(),
        field: Field::Content,
        intent: id(2),
        value: content(2),
    }];
    assert_eq!(
        delete_view(journal.seen(&"d".into()), &remote).unseen,
        [id(2)]
    );
}

#[test]
fn the_generator_is_its_own_stream_and_a_restore_mints_from_the_one_it_was_loaded_with() {
    let mut engine = IdGen::new(42);
    let mut journal = Journal::new(&engine);
    let rolled_back = journal.state(whole);
    engine.next_u64();
    let mut earlier: Vec<String> = (0..64).map(|_| engine.uuid_v7(0)).collect();
    earlier.extend((0..64).map(|_| journal.mint().to_string()));
    assert_eq!(earlier.iter().collect::<BTreeSet<_>>().len(), earlier.len());
    let mut loaded = Journal::new(&IdGen::new(7));
    loaded.restore(rolled_back);
    let next = loaded.mint();
    assert!(!earlier.contains(&next.to_string()), "{next}");
    assert_eq!(next, Journal::new(&IdGen::new(7)).mint());
}

#[test]
fn changes_name_only_what_changed_and_a_state_restores() {
    let mut journal = Journal::new(&IdGen::new(0));
    journal.user_change(&"n".into(), content(1), WireBase::None, None);
    journal.take_changes(whole);
    let before = journal.state(whole);
    journal.remote_holds(&"m".into(), Field::Place, &held(1, 1), false);
    journal.set_listed(true);
    let changes = journal.take_changes(whole);
    assert!(changes.rows.is_empty());
    assert_eq!(changes.history.len(), 1);
    assert_eq!(changes.listed, Some(true));
    assert_eq!(
        JournalChanges::between(&before, &journal.state(whole)),
        changes
    );
    let mut restored = Journal::new(&IdGen::new(1));
    restored.restore(journal.state(whole));
    assert_eq!(restored, journal);
}

#[test]
fn a_compact_row_that_leaves_and_ends_before_a_take_reaches_the_store_as_ended() {
    for replaced in [false, true] {
        let mut journal = Journal::new(&IdGen::new(0));
        let first = journal.user_change(&"n".into(), content(1), WireBase::None, None);
        journal.send(&"n".into(), Field::Content, 0);
        journal.applied(
            &"n".into(),
            Field::Content,
            first,
            &held(7, 100),
            None,
            true,
        );
        let key = ("n".into(), Field::Content);
        let row = journal.row(&"n".into(), Field::Content).cloned().unwrap();
        journal.take_changes(whole);
        assert!(journal.compact(key.clone()));
        match replaced {
            false => journal.write_out(key.clone(), row),
            true => {
                journal.user_change(&"n".into(), content(2), WireBase::None, None);
            }
        }
        journal.end(&"n".into(), Field::Content);
        assert_eq!(
            journal.take_changes(whole).rows,
            [(key, None)],
            "{replaced}"
        );
    }
}

#[test]
fn a_result_that_keeps_the_intent_it_was_sent_leaves_compact_rows_based_on_it_unchanged() {
    let mut journal = Journal::new(&IdGen::new(0));
    let sent = journal.user_change(&"n".into(), content(1), WireBase::None, Some(id(5)));
    journal.send(&"n".into(), Field::Content, 0);
    let kept = FieldIntent {
        id: sent,
        ..held(0, 100)
    };
    journal.applied(&"n".into(), Field::Content, sent, &kept, None, true);
    assert!(journal.compact(("n".into(), Field::Content)));
    let full = |_: &RowKey, held: &HeldRow| held.full(content(1), 100, None);
    journal.take_changes(full);
    let before = journal.state(full);
    journal.applied(&"m".into(), Field::Place, id(5), &held(5, 200), None, true);
    let changes = journal.take_changes(full);
    assert!(changes.rows.is_empty(), "{:?}", changes.rows);
    assert_eq!(
        JournalChanges::between(&before, &journal.state(full)),
        changes
    );
}

#[test]
fn a_delete_that_saw_nothing_keeps_no_delete_view() {
    let f = NodeId::from("f");
    let mut journal = Journal::new(&IdGen::new(0));
    journal.user_delete(&f, Vec::new(), None);
    assert!(!journal.state(whole).seen.contains_key(&f));
    let mut state = journal.state(whole);
    state.seen.insert(f.clone(), Vec::new());
    let mut restored = Journal::new(&IdGen::new(0));
    restored.restore(state);
    assert!(!restored.state(whole).seen.contains_key(&f));
}

#[test]
fn an_absent_node_lapses_at_its_deadline_and_not_before() {
    let mut journal = Journal::new(&IdGen::new(0));
    for i in 0..128 {
        journal.r_lacks(&NodeId::from(format!("gone{i}")));
    }
    let mut restored = Journal::new(&IdGen::new(0));
    restored.restore(journal.state(whole));
    restored.set_clock(ABSENT_FOR_MS as u64 - 1);
    assert_eq!(restored.state(whole).absent.len(), 128);
    restored.set_clock(ABSENT_FOR_MS as u64);
    assert!(restored.state(whole).absent.is_empty());
}
