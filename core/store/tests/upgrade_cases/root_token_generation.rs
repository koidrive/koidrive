// SPDX-License-Identifier: Apache-2.0

upgrade_case!(root_token_generation, |store| {
    store
        .transaction(|tx| {
            assert_eq!(tx.root_tokens()?.generation, 0);
            let tokens = koidrive_fs::RootTokens {
                current: Some("a".repeat(32)),
                pending: Some("b".repeat(32)),
                generation: 7,
            };
            tx.set_root_tokens(&tokens)?;
            assert_eq!(tx.root_tokens()?, tokens);
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
