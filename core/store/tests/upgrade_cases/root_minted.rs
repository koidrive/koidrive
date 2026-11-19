// SPDX-License-Identifier: Apache-2.0

upgrade_case!(root_minted, |store| {
    store
        .transaction(|tx| {
            assert_eq!(tx.root_minted()?, None);
            let minted = koidrive_fs::Minted::mint();
            tx.commit_root_token(minted)?;
            let committed = koidrive_fs::RootTokens {
                current: Some(minted.to_string()),
                pending: None,
            };
            assert_eq!(tx.root_tokens()?, committed);
            tx.set_root_tokens(&koidrive_fs::RootTokens::default())?;
            assert_eq!(tx.root_minted()?, Some(minted.to_string()));
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
