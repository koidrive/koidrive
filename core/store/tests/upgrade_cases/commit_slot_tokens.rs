// SPDX-License-Identifier: Apache-2.0

upgrade_case!(commit_slot_tokens, |store| {
    store
        .transaction(|tx| {
            let mut state = tx.engine_state()?;
            let kept: Vec<(u64, koidrive_engine::commit::Base)> =
                (1..=3).map(|slot| (slot, slot.into())).collect();
            assert_eq!(state.commits.slots, kept.into_iter().collect());
            let mut commits =
                koidrive_engine::commit::Commits::from_state(state.commits.clone()).unwrap();
            let welcome = koidrive_proto::v1::Welcome {
                history_id: tx.cursor()?.unwrap().history_id,
                device_seq: 3,
                device_commit_id: tx.cursor()?.unwrap().commit_id,
                device_committed_at_ms: 3,
                ..Default::default()
            };
            commits.connected(&welcome);
            assert_eq!(commits.held_through(), 3);
            let token = koidrive_engine::commit::Base {
                seq: 3,
                commit_id: "commit-3".into(),
                committed_at_ms: 1_700_000_000_003,
            };
            state.commits.slots.insert(3, token.clone());
            tx.set_engine_state(&state)?;
            assert_eq!(tx.engine_state()?.commits.slots.get(&3), Some(&token));
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
