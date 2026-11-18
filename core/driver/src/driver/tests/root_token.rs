// SPDX-License-Identifier: Apache-2.0

use super::*;

fn tokens(store: &Path) -> koidrive_fs::RootTokens {
    Store::open(store)
        .unwrap()
        .transaction(|tx| tx.root_tokens())
        .unwrap()
}

#[tokio::test]
async fn a_second_profile_neither_rotates_the_token_of_a_folder_another_holds_nor_halts_it() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let (a, b) = (Paths::under(&base.join("a")), Paths::under(&base.join("b")));
    linked(&a, &root, "d1");
    linked(&b, &root, "d2");
    let (store_a, store_b) = (a.store(SPACE).unwrap(), b.store(SPACE).unwrap());
    let stale = koidrive_fs::RootTokens {
        current: Some("0".repeat(32)),
        pending: None,
    };
    Store::open(&store_b)
        .unwrap()
        .transaction(|tx| tx.set_root_tokens(&stale))
        .unwrap();
    fs::write(root.join("one"), b"1").unwrap();
    let first = Driver::open(a).unwrap();
    let second = Driver::open(b).unwrap();
    let entries = |driver: &Driver| driver.status().folders[0].local_entries;

    first.start();
    eventually("the first profile observes the folder", || {
        entries(&first) == Some(1)
    })
    .await;
    second.start();
    let problem = halted(&second).await;
    assert!(problem.contains("another Koidrive process"), "{problem}");
    let refused = second.relink(&root).await;
    assert!(
        matches!(refused, Err(DriverError::FolderInUse(_))),
        "{refused:?}"
    );
    second.stop().await;
    assert_ne!(first.status().folders[0].state, FolderState::Halted);
    first.stop().await;
    assert_eq!(tokens(&store_b), stale);
    let held = read_marker(&root).unwrap().unwrap().token;
    assert!(held.is_some());
    assert_eq!(
        tokens(&store_a),
        koidrive_fs::RootTokens {
            current: held,
            pending: None,
        }
    );

    fs::write(root.join("two"), b"2").unwrap();
    first.start();
    eventually("the restarted profile observes the folder", || {
        entries(&first) == Some(2)
    })
    .await;
    assert_ne!(first.status().folders[0].state, FolderState::Halted);
    first.stop().await;
}

#[tokio::test]
async fn relink_refuses_a_folder_that_syncs_and_takes_one_halted_as_an_older_copy() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("a"));
    linked(&paths, &root, "d1");
    fs::write(root.join("one"), b"1").unwrap();
    let driver = Driver::open(paths).unwrap();
    let entries = |driver: &Driver| driver.status().folders[0].local_entries;
    driver.start();
    eventually("the folder is observed", || entries(&driver) == Some(1)).await;

    let refused = driver.relink(&root).await;
    assert!(
        matches!(refused, Err(DriverError::NotACopy(_))),
        "{refused:?}"
    );
    fs::write(root.join("two"), b"2").unwrap();
    eventually("the folder is observed again", || {
        entries(&driver) == Some(2)
    })
    .await;
    assert_ne!(driver.status().folders[0].state, FolderState::Halted);
    driver.stop().await;

    let mut marker = read_marker(&root).unwrap().unwrap();
    marker.token = Some("0".repeat(32));
    write_marker(&root, &marker).unwrap();
    driver.start();
    eventually("the copy halts", || driver.status().folders[0].relinkable).await;
    driver.stop().await;
    for folder in driver.inner.folders.lock().unwrap().values() {
        let mut status = folder.status.lock().unwrap();
        (status.last_seq, status.remote_nodes, status.questions) = (Some(7), 3, 2);
    }
    let relinked = driver.relink(&root).await.unwrap();
    assert!(!relinked.relinkable, "{relinked:?}");
    let reset = (relinked.last_seq, relinked.remote_nodes, relinked.questions);
    assert_eq!(reset, (None, 0, 0));
    driver.start();
    eventually("the relinked folder is observed", || {
        entries(&driver) == Some(2)
    })
    .await;
    assert_ne!(read_marker(&root).unwrap().unwrap().token, marker.token);
    driver.stop().await;
}
