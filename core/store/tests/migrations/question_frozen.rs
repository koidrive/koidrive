// SPDX-License-Identifier: Apache-2.0

use super::*;

fn frozen(id: &str, parent: &str, taken: bool) -> (String, Frozen) {
    let (parent, name) = (parent.into(), id.into());
    (id.into(), Frozen { parent, name, taken })
}

fn first_link_discard(id: &str, frozen: Option<BTreeMap<String, Frozen>>) -> Question {
    Question {
        id: id.into(),
        asked_at_ms: 11,
        kind: QuestionKind::Recovery {
            group: RecoveryGroup::CreatedOnThisDevice,
            sent_in: None,
            cause: None,
        },
        held: BTreeMap::from([(
            "f".into(),
            Held {
                members: BTreeMap::from([("f".into(), None)]),
                files: 0,
            },
        )]),
        answer: Some(Answer::Discard),
        shown: BTreeMap::from([("f".into(), engine_shape("root", "f"))]),
        frozen,
    }
}

#[test]
fn a_store_from_before_discards_froze_what_was_below_leaves_the_build_to_decide() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut before = engine_state();
    before.commits.questions = vec![first_link_discard("older", None)];
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&before))
        .unwrap();
    store_before(&path, "question_frozen");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), before);
    let mut after = before.clone();
    after.commits.questions = vec![
        first_link_discard("older", None),
        first_link_discard("frozen", Some(BTreeMap::from([frozen("x", "f", true), frozen("o", "f", false)]))),
        first_link_discard("nothing", Some(BTreeMap::new())),
    ];
    store
        .transaction(|tx| tx.update_engine_state(&before, &after))
        .unwrap();
    drop(store);
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), after);

    let mut asked_again = after.clone();
    asked_again.commits.questions = vec![first_link_discard("older", None)];
    store
        .transaction(|tx| tx.update_engine_state(&after, &asked_again))
        .unwrap();
    drop(store);
    let rows: u64 = Connection::open(&path)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM open_question_frozen", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 0);
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), asked_again);
}
