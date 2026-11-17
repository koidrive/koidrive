// SPDX-License-Identifier: Apache-2.0

upgrade_case!(schema_open_existing, |store| {
    store
        .transaction(|tx| {
            assert_eq!(tx.link()?.unwrap().device_id, "fixture-device");
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let release = dir.path().join("state.db");
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/alpha6.db"),
        &release,
    )
    .unwrap();
    Store::open_existing(&release)
        .unwrap()
        .transaction(|tx| {
            assert_eq!(tx.link()?.unwrap().device_id, "fixture-device");
            assert!(tx.cursor()?.unwrap().last_seq > 0);
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
    let emptied = dir.path().join("emptied.db");
    std::fs::write(&emptied, b"").unwrap();
    assert!(matches!(
        Store::open_existing(&emptied),
        Err(koidrive_store::Error::NotAStore)
    ));
    assert_eq!(std::fs::metadata(&emptied).unwrap().len(), 0);
});
