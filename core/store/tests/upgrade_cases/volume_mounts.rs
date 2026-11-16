// SPDX-License-Identifier: Apache-2.0

upgrade_case!(volume_mounts, |store| {
    store
        .transaction(|tx| {
            assert!(tx.volume_mounts()?.is_empty());
            let mounts = std::collections::BTreeMap::from([
                (
                    std::path::PathBuf::from("Disk"),
                    koidrive_fs::Mount::Volume(koidrive_fs::VolumeId::Uuid([5; 16])),
                ),
                (
                    std::path::PathBuf::from("a/b/Deep"),
                    koidrive_fs::Mount::Volume(koidrive_fs::VolumeId::FileSystemId([5; 8])),
                ),
                (
                    std::path::PathBuf::from("Stick"),
                    koidrive_fs::Mount::Unidentified,
                ),
            ]);
            tx.set_volume_mounts(&mounts)?;
            assert_eq!(tx.volume_mounts()?, mounts);
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
