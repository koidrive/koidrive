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
