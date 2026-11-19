// SPDX-License-Identifier: Apache-2.0

upgrade_case!(schema_reads_without_write_lock, |store| {
    let link = store.read(|tx| tx.link()).unwrap().unwrap();
    assert_eq!(link.device_id, "fixture-device");
    let dir = tempfile::tempdir().unwrap();
    let release = dir.path().join("state.db");
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/alpha6.db"),
        &release,
    )
    .unwrap();
    let mut writer = Store::open(&release).unwrap();
    let (writing, written) = std::sync::mpsc::channel();
    let (finish, finished) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        writer.transaction(|tx| {
            tx.forget_sync_state()?;
            writing.send(()).unwrap();
            finished.recv().unwrap();
            Ok::<_, koidrive_store::Error>(())
        })
    });
    written.recv().unwrap();
    let read = Store::open_existing(&release).and_then(|mut reader| {
        reader.read(|tx| Ok::<_, koidrive_store::Error>((tx.link()?, tx.cursor()?)))
    });
    finish.send(()).unwrap();
    holder.join().unwrap().unwrap();
    let (link, cursor) = read.unwrap();
    assert_eq!(link.unwrap().device_id, "fixture-device");
    assert!(cursor.unwrap().last_seq > 0);
});
