// SPDX-License-Identifier: Apache-2.0

upgrade_case!(schema_opens_only_regular_files, |store| {
    assert!(store.read(|tx| tx.link()).unwrap().is_some());
    let dir = tempfile::tempdir().unwrap();
    let release = dir.path().join("state.db");
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/alpha6.db"),
        &release,
    )
    .unwrap();
    let writer = Store::open(&release).unwrap();
    assert!(dir.path().join("state.db-wal").is_file());
    assert!(dir.path().join("state.db-shm").is_file());
    Store::open_existing(&release).unwrap();
    drop(writer);
    let linked = dir.path().join("linked.db");
    std::os::unix::fs::symlink(&release, &linked).unwrap();
    assert!(matches!(
        Store::open_existing(&linked),
        Err(koidrive_store::Error::Unopenable { .. })
    ));
    assert!(matches!(
        Store::open_read_only(&linked),
        Err(koidrive_store::Error::Unopenable { .. })
    ));
});
