// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn volume_mounts_keep_their_exact_names_and_start_empty_in_a_store_from_before_them() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    drop(Store::open(&path).unwrap());
    store_before(&path, "volume_mounts");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    assert_eq!(
        store.transaction(|tx| tx.volume_mounts()).unwrap(),
        BTreeMap::new()
    );
    use koidrive_fs::{Mount, VolumeId};
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;
    let mounts = BTreeMap::from([
        (
            PathBuf::from("Disk"),
            Mount::Volume(VolumeId::Uuid([7; 16])),
        ),
        (
            PathBuf::from("a/b/Deep"),
            Mount::Volume(VolumeId::FileSystemId([9; 8])),
        ),
        (
            PathBuf::from(std::ffi::OsStr::from_bytes(b"caf\xe9")),
            Mount::Unidentified,
        ),
    ]);
    store
        .transaction(|tx| tx.set_volume_mounts(&mounts))
        .unwrap();
    drop(store);
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.transaction(|tx| tx.volume_mounts()).unwrap(), mounts);
    let one = BTreeMap::from([(PathBuf::from("Disk"), Mount::Unidentified)]);
    store.transaction(|tx| tx.set_volume_mounts(&one)).unwrap();
    assert_eq!(store.transaction(|tx| tx.volume_mounts()).unwrap(), one);
}
