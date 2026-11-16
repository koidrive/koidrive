// SPDX-License-Identifier: Apache-2.0

upgrade_case!(root_identity, |store| {
    store
        .transaction(|tx| {
            let mut link = tx.link()?.unwrap();
            assert_eq!(link.root_dev, 16_777_233);
            assert_eq!(link.root_volume, None);
            let local = tx.local_nodes()?;
            assert!(!local.is_empty());
            assert!(local.iter().all(|node| node.identity.birth_ns.is_some()));
            for volume in [
                koidrive_fs::VolumeId::Uuid([5; 16]),
                koidrive_fs::VolumeId::FileSystemId([5; 8]),
            ] {
                link.root_volume = Some(volume);
                tx.set_link(&link)?;
                assert_eq!(tx.link()?.unwrap().root_volume, Some(volume));
            }
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
});
