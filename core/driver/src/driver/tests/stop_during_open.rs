// SPDX-License-Identifier: Apache-2.0

use std::sync::atomic::AtomicBool;
use std::sync::mpsc;

use koidrive_engine::restore::RestoreIntent;

use super::*;

fn stored(store: &Path) -> (usize, koidrive_fs::RootTokens) {
    Store::open(store)
        .unwrap()
        .read(|tx| {
            Ok::<_, koidrive_store::Error>((tx.restore_requests()?.len(), tx.root_tokens()?))
        })
        .unwrap()
}

#[tokio::test]
async fn a_pause_while_a_folder_loads_its_store_stops_it_at_once_and_leaves_the_store_as_a_crash_would()
 {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &root, "d1");
    let store = paths.store(SPACE).unwrap();
    let node = "0190f0a0-0000-7000-8000-0000000000aa".to_owned();
    let intent = RestoreIntent::from_trash(node.into(), None, None).unwrap();
    Store::open(&store)
        .unwrap()
        .transaction(|tx| tx.request_restore("0190f0a0-0000-7000-8000-0000000000bb", &intent))
        .unwrap();
    let before = stored(&store);
    let (entered, loading) = mpsc::channel();
    let waited_out = Arc::new(AtomicBool::new(false));
    crate::folder::WHILE_LOADING.lock().unwrap().insert(
        store.clone(),
        Box::new({
            let waited_out = waited_out.clone();
            move |abandoned: &dyn Fn() -> bool| {
                entered.send(()).unwrap();
                let parked = std::time::Instant::now();
                while !abandoned() {
                    if parked.elapsed() > Duration::from_secs(10) {
                        waited_out.store(true, Ordering::SeqCst);
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }),
    );
    let driver = Driver::open(paths).unwrap();
    driver.start();
    eventually("the folder loads its store", || loading.try_recv().is_ok()).await;

    driver.pause().await.unwrap();
    assert!(
        !waited_out.load(Ordering::SeqCst),
        "the pause waited for the store to load"
    );
    assert!(
        claim::lock(&root).unwrap().is_some(),
        "the paused folder is still claimed"
    );
    assert_eq!(stored(&store), before, "the stopped open changed the store");

    driver.resume().await.unwrap();
    eventually("the next open takes the restore request", || {
        stored(&store).0 == 0
    })
    .await;
    assert_ne!(driver.status().folders[0].state, FolderState::Halted);
    driver.stop().await;
}

#[tokio::test]
async fn pauses_and_resumes_while_a_folder_loads_its_store_never_load_it_twice_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &root, "d1");
    let store = paths.store(SPACE).unwrap();
    let parked = Arc::new(AtomicBool::new(true));
    let (entered, loading) = mpsc::channel();
    crate::folder::WHILE_LOADING.lock().unwrap().insert(
        store.clone(),
        Box::new({
            let parked = parked.clone();
            move |_: &dyn Fn() -> bool| {
                entered.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(500));
                parked.store(false, Ordering::SeqCst);
            }
        }),
    );
    let driver = Driver::open(paths).unwrap();
    driver.start();
    eventually("the folder loads its store", || loading.try_recv().is_ok()).await;
    let (again, loading_again) = mpsc::channel();
    crate::folder::WHILE_LOADING.lock().unwrap().insert(
        store.clone(),
        Box::new({
            let parked = parked.clone();
            move |_: &dyn Fn() -> bool| again.send(parked.load(Ordering::SeqCst)).unwrap()
        }),
    );

    for _ in 0..3 {
        driver.pause().await.unwrap();
        driver.resume().await.unwrap();
    }
    let beside = std::cell::Cell::new(None);
    eventually("the folder loads its store again", || {
        beside.set(loading_again.try_recv().ok());
        beside.get().is_some()
    })
    .await;
    assert_eq!(
        beside.get(),
        Some(false),
        "a second load ran beside the first"
    );
    driver.stop().await;
}
