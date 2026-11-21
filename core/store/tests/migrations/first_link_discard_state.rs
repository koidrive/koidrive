// SPDX-License-Identifier: Apache-2.0

use super::*;
use koidrive_engine::question::{Frozen, FrozenAnswer};

fn answered_discard(whole: bool) -> Question {
    let below = BTreeMap::from([(
        "x".into(),
        Frozen {
            parent: "f".into(),
            name: "x".into(),
            taken: true,
            member_of: whole.then(|| "x".into()),
        },
    )]);
    Question {
        id: "answered".into(),
        asked_at_ms: 11,
        kind: QuestionKind::Recovery {
            group: RecoveryGroup::CreatedOnThisDevice,
            sent_in: None,
            cause: None,
        },
        held: BTreeMap::new(),
        answer: Some(Answer::Discard),
        shown: BTreeMap::from([("f".into(), engine_shape("root", "f"))]),
        frozen: Some(FrozenAnswer { below, whole }),
    }
}

#[test]
fn a_store_from_before_first_link_discard_state_was_recorded_opens_with_none_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut before = engine_state();
    before.commits.questions = vec![answered_discard(true)];
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&before))
        .unwrap();
    store_before(&path, "first_link_discard_state");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    let after = before.clone();
    before.trees.discard_stays.clear();
    before.trees.discard_answered.clear();
    before.trees.first_link_held.clear();
    before.trees.first_link_open.clear();
    before.commits.questions = vec![answered_discard(false)];
    assert!(!after.trees.discard_stays.is_empty());
    assert!(!after.trees.discard_answered.is_empty());
    assert!(!after.trees.first_link_held.is_empty());
    assert!(!after.trees.first_link_open.is_empty());
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), before);
    store
        .transaction(|tx| tx.update_engine_state(&before, &after))
        .unwrap();
    drop(store);
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), after);
}

#[test]
fn a_frozen_member_without_its_folder_reads_as_an_answer_frozen_before_members_were_listed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut state = engine_state();
    state.commits.questions = vec![answered_discard(true)];
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&state))
        .unwrap();
    (Connection::open(&path).unwrap())
        .execute("UPDATE open_question_frozen SET member_of = NULL", [])
        .unwrap();
    state.commits.questions = vec![answered_discard(false)];
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), state);
}
