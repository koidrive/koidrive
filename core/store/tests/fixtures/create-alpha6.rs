// SPDX-License-Identifier: Apache-2.0

#[test]
fn create_alpha6_upgrade_fixture() {
    let output = std::env::var("KOIDRIVE_FIXTURE_PATH").unwrap();
    assert!(!Path::new(&output).exists());
    let mut remote = Remote::new();
    let (site, mut device) = linked(&mut remote, "fixture-device");
    fs::create_dir_all(site.root.join("Disk/Slides")).unwrap();
    fs::write(site.root.join("Disk/Slides/deck.txt"), b"presentation").unwrap();
    fs::write(site.root.join("Disk/original.txt"), b"first revision").unwrap();
    fs::write(site.root.join("pending.txt"), b"synced contents").unwrap();
    device.rescan();
    device.rest(&mut remote);
    device.create(
        &mut remote,
        &site.root.join("Disk/original.txt"),
        b"second revision",
    );
    fs::rename(
        site.root.join("Disk/original.txt"),
        site.root.join("Disk/report.txt"),
    )
    .unwrap();
    device.rescan();
    device.rest(&mut remote);
    assert!(remote.tree().contains_key("Disk/report.txt"));
    assert!(!remote.tree().contains_key("Disk/original.txt"));
    fs::write(site.root.join("pending.txt"), b"unsent local edit").unwrap();
    device.rescan();
    device.scan_until(|device| !device.work.is_empty());
    assert!(!device.folder.engine.trees.state().local_changes.is_empty());
    drop(device);
    let conn = rusqlite::Connection::open(&site.db).unwrap();
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .unwrap();
    conn.execute("VACUUM INTO ?1", [&output]).unwrap();
}
