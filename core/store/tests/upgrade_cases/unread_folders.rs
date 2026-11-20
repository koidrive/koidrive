// SPDX-License-Identifier: Apache-2.0

upgrade_case!(unread_folders, |store| {
    use koidrive_engine::question::{Answer, Frozen, Question, QuestionKind, RecoveryGroup};
    store
        .transaction(|tx| {
            assert_eq!(tx.unread_folders()?, None);
            let unread = std::collections::BTreeMap::from([
                (std::path::PathBuf::from("Private"), 1_795_079_591_616_636_478),
                (std::path::PathBuf::from("a/b/Locked"), -1),
            ]);
            tx.set_unread_folders(&unread)?;
            assert_eq!(tx.unread_folders()?, Some(unread));
            let node = tx.local_nodes()?.into_iter().next().unwrap().node_id;
            let mut state = tx.engine_state()?;
            state.trees.outside.insert(node.clone());
            state.trees.found.insert(node, "dist/kept".into());
            state.commits.questions.push(Question {
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
                frozen: Some(std::collections::BTreeMap::from([(
                    "x".into(),
                    Frozen {
                        parent: "f".into(),
                        name: "x".into(),
                        taken: true,
                    },
                )])),
            });
            tx.set_engine_state(&state)?;
            assert_eq!(tx.engine_state()?, state);
            assert!(tx.unread_folders()?.is_some());
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
