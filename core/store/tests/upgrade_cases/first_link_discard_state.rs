// SPDX-License-Identifier: Apache-2.0

upgrade_case!(first_link_discard_state, |store| {
    store
        .transaction(|tx| {
            let mut state = tx.engine_state()?;
            assert!(state.trees.discard_stays.is_empty());
            assert!(state.trees.discard_answered.is_empty());
            assert!(state.trees.first_link_held.is_empty());
            assert!(state.trees.first_link_open.is_empty());
            let report = tx
                .local_nodes()?
                .into_iter()
                .find(|node| node.shape.name == "report.txt")
                .unwrap();
            let folder = report.shape.parent_id.clone();
            (state.trees.discard_stays).insert((report.node_id.clone(), folder.clone().into()));
            (state.trees.discard_answered).insert(folder.clone().into());
            (state.trees.first_link_held).insert(folder.clone().into(), report.node_id.clone());
            (state.trees.first_link_open).insert(folder.clone().into(), true);
            tx.set_engine_state(&state)?;
            let trees = tx.engine_state()?.trees;
            let stays: Vec<_> = (trees.discard_stays.into_iter())
                .map(|(node, folder)| (node, folder.to_string()))
                .collect();
            assert_eq!(stays, [(report.node_id.clone(), folder.clone())]);
            let answered: Vec<_> = (trees.discard_answered.into_iter())
                .map(|folder| folder.to_string())
                .collect();
            assert_eq!(answered, std::slice::from_ref(&folder));
            let held: Vec<_> = (trees.first_link_held.into_iter())
                .map(|(node, parent)| (node.to_string(), parent))
                .collect();
            assert_eq!(held, [(folder.clone(), report.node_id.clone())]);
            let open: Vec<_> = (trees.first_link_open.into_iter())
                .map(|(folder, asked)| (folder.to_string(), asked))
                .collect();
            assert_eq!(open, [(folder.clone(), true)]);
            use koidrive_engine::question::{
                Answer, Frozen, FrozenAnswer, Question, QuestionKind, RecoveryGroup,
            };
            let mut state = tx.engine_state()?;
            let member = Frozen {
                parent: folder.clone().into(),
                name: "report.txt".into(),
                taken: true,
                member_of: Some(folder.clone().into()),
            };
            state.commits.questions.push(Question {
                id: "0190a6b2-0000-7000-8000-000000000009".into(),
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
                    below: std::collections::BTreeMap::from([(report.node_id.clone(), member)]),
                    whole: true,
                }),
            });
            tx.set_engine_state(&state)?;
            assert_eq!(tx.engine_state()?.commits.questions, state.commits.questions);
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
