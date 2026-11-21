// SPDX-License-Identifier: Apache-2.0

upgrade_case!(question_frozen, |store| {
    use koidrive_engine::question::{
        Answer, Frozen, FrozenAnswer, Question, QuestionKind, RecoveryGroup,
    };
    use std::collections::BTreeMap;
    let before = store.transaction(|tx| tx.engine_state()).unwrap();
    let mut after = before.clone();
    after.commits.questions.push(Question {
        id: "0190a6b2-0000-7000-8000-000000000007".into(),
        asked_at_ms: 11,
        kind: QuestionKind::Recovery {
            group: RecoveryGroup::CreatedOnThisDevice,
            sent_in: None,
            cause: None,
        },
        held: Default::default(),
        answer: Some(Answer::Discard),
        shown: Default::default(),
        frozen: Some(FrozenAnswer {
            below: BTreeMap::from([("x".into(), Frozen { parent: "f".into(), name: "x".into(), taken: true, member_of: Some("x".into()) })]),
            whole: true,
        }),
    });
    store
        .transaction(|tx| tx.update_engine_state(&before, &after))
        .unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), after);
});
