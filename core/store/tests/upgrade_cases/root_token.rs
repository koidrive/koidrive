// SPDX-License-Identifier: Apache-2.0

upgrade_case!(root_token, |store| {
    store
        .transaction(|tx| {
            assert_eq!(tx.root_tokens()?, koidrive_fs::RootTokens::default());
            let tokens = koidrive_fs::RootTokens {
                current: Some("a".repeat(32)),
                pending: Some("b".repeat(32)),
                ..Default::default()
            };
            tx.set_root_tokens(&tokens)?;
            let mut link = tx.link()?.unwrap();
            link.root_dev += 1;
            tx.set_link(&link)?;
            assert_eq!(tx.link()?.unwrap(), link);
            assert_eq!(tx.root_tokens()?, tokens);
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
