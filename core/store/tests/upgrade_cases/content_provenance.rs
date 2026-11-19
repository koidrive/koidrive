// SPDX-License-Identifier: Apache-2.0

upgrade_case!(content_provenance, |store| {
    store
        .transaction(|tx| {
            let mut state = tx.engine_state()?;
            assert!(state.trees.content_provenance.is_empty());
            let report = tx
                .remote_nodes()?
                .into_iter()
                .find(|node| node.name == "report.txt")
                .unwrap();
            let version = report.current.unwrap();
            assert!(!version.version_id.is_empty());
            let source = koidrive_engine::state::ContentVersion {
                version_id: version.version_id,
                made_over: version.made_over,
                own: false,
            };
            let provenance = koidrive_engine::state::ContentProvenance {
                synced: Some(source.clone()),
                local: Some(source),
                edit_bases: Vec::new(),
                unversioned_bases: Vec::new(),
            };
            state
                .trees
                .content_provenance
                .insert(report.node_id.clone().into(), provenance.clone());
            tx.set_engine_state(&state)?;
            assert_eq!(
                tx.engine_state()?.trees.content_provenance,
                std::collections::BTreeMap::from([(report.node_id.into(), provenance)])
            );
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
