// SPDX-License-Identifier: Apache-2.0

#[tokio::test]
async fn a_pause_and_a_resume_during_a_restart_leave_the_folder_one_task() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &root, "d1");
    let driver = Driver::open(paths).unwrap();
    driver.inner.running.store(true, Ordering::SeqCst);
    let (stop, mut stopped) = watch::channel(false);
    let (finished, done) = watch::channel(());
    driver
        .inner
        .folders
        .lock()
        .unwrap()
        .values_mut()
        .next()
        .unwrap()
        .run = Some(Running {
        stop,
        requests: Arc::new(Notify::new()),
        done,
    });
    let (reached, stopped_before_lift) = tokio::sync::oneshot::channel();
    let (proceed, released) = tokio::sync::oneshot::channel();
    *driver.inner.restart_stopped.lock().unwrap() = Some(RestartStopped {
        reached,
        proceed: released,
    });
    let restarting = tokio::spawn({
        let driver = driver.clone();
        let root = root.clone();
        async move {
            driver.restart(&root).await.unwrap();
            driver
                .inner
                .folders
                .lock()
                .unwrap()
                .values()
                .next()
                .unwrap()
                .run
                .as_ref()
                .unwrap()
                .done
                .clone()
        }
    });
    stopped.changed().await.unwrap();
    assert!(*stopped.borrow());
    drop(finished);
    stopped_before_lift.await.unwrap();
    let changing = tokio::spawn({
        let driver = driver.clone();
        async move {
            assert!(driver.pause().await.unwrap());
            assert!(driver.resume().await.unwrap());
            driver
                .inner
                .folders
                .lock()
                .unwrap()
                .values()
                .next()
                .unwrap()
                .run
                .as_ref()
                .map(|run| run.done.clone())
        }
    });
    let blocked = still_waiting(&changing).await;
    proceed.send(()).unwrap();
    let restarted = restarting.await.unwrap();
    let resumed = changing.await.unwrap();
    assert!(
        blocked,
        "pause/resume bypassed the stop/lift/start transition"
    );
    let resumed = resumed.unwrap();
    assert!(
        restarted.has_changed().is_err(),
        "pause left the task from restart running"
    );
    assert!(
        resumed.has_changed().is_ok(),
        "resume lost its replacement task"
    );
    eventually("the folder is claimed", || {
        claim::owner(&root).unwrap().is_some()
    })
    .await;
    let folder = &driver.status().folders[0];
    assert_ne!(folder.state, FolderState::Halted, "{:?}", folder.problem);
    driver.stop().await;
    assert!(resumed.has_changed().is_err());
}

#[tokio::test]
async fn a_resume_after_a_pause_cancelled_before_its_task_ran_stays_resumed() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let driver = Driver::open(paths.clone()).unwrap();
    let mut pausing = Box::pin(driver.pause());
    let first = std::future::poll_fn(|cx| {
        std::task::Poll::Ready(std::future::Future::poll(pausing.as_mut(), cx))
    })
    .await;
    assert!(first.is_pending());
    drop(pausing);
    driver.resume().await.unwrap();
    tokio::task::yield_now().await;
    assert!(!driver.status().paused);
    assert!(!paths.paused().exists());
}

#[tokio::test]
async fn a_pause_answers_before_its_folders_end_and_a_resume_starts_them_again_once_they_have() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &base.join("Koidrive"), "d1");
    let driver = Driver::open(paths.clone()).unwrap();
    driver.inner.running.store(true, Ordering::SeqCst);
    let (stop, stopped) = watch::channel(false);
    let (finished, done) = watch::channel(());
    let stopping = Running {
        stop,
        requests: Arc::new(Notify::new()),
        done,
    };
    driver
        .inner
        .folders
        .lock()
        .unwrap()
        .values_mut()
        .next()
        .unwrap()
        .run = Some(stopping);

    let paused = tokio::time::timeout(Duration::from_secs(5), driver.pause()).await;
    assert!(
        paused
            .expect("the pause answers while the folder's task runs")
            .unwrap()
    );
    assert!(*stopped.borrow());
    assert!(paths.paused().is_file());
    let status = driver.status();
    assert!(status.paused);
    assert_eq!(status.folders[0].state, FolderState::Stopped);
    let resuming = tokio::spawn({
        let driver = driver.clone();
        async move { driver.resume().await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !resuming.is_finished(),
        "a resume waits for the paused folders to end"
    );
    drop(finished);
    assert!(resuming.await.unwrap().unwrap());
    {
        let folders = driver.inner.folders.lock().unwrap();
        let run = folders.values().next().unwrap().run.as_ref();
        assert!(
            run.is_some_and(|run| !run.ended()),
            "a new task syncs the folder"
        );
    }
    driver.stop().await;
}

#[tokio::test]
async fn a_pause_ends_the_wait_of_each_folder_it_stops() {
    let (_dir, paths, _root) = profile(&[]);
    let driver = Driver::open(paths).unwrap();
    let wait = crate::status::Wait {
        kind: crate::status::WaitKind::Retry,
        until_ms: Some(i64::MAX),
        unreachable: None,
    };
    for folder in driver.inner.folders.lock().unwrap().values() {
        folder.status.lock().unwrap().wait = Some(wait.clone());
    }
    assert_eq!(driver.status().folders[0].wait, Some(wait));
    assert!(driver.pause().await.unwrap());
    let folder = &driver.status().folders[0];
    assert_eq!(
        (folder.state, folder.wait.clone()),
        (FolderState::Stopped, None)
    );
}

#[tokio::test]
async fn the_status_reports_the_saved_update_check_and_off_once_config_toml_turns_it_off() {
    let (_dir, paths, _root) = profile(&[]);
    let release = serde_json::json!({
        "asset": crate::update::this_asset(),
        "checked_at": 7,
        "release": { "version": "999.0.0", "url": "https://example.com/v999.0.0" },
    });
    fs::write(paths.update_check(), release.to_string()).unwrap();
    let driver = release_driver(paths.clone());
    assert_eq!(
        driver.status().update,
        crate::Update::Available {
            latest: "999.0.0".into(),
            url: "https://example.com/v999.0.0".into(),
            checked_at_ms: 7,
        }
    );
    drop(driver);

    fs::write(&paths.config_file, "[updates]\ncheck = false\n").unwrap();
    let driver = release_driver(paths.clone());
    assert_eq!(driver.status().update, crate::Update::Off);
    driver.check_for_updates();
    assert!(!paths.update_check().exists());
}

#[tokio::test]
async fn only_a_file_marks_a_profile_paused() {
    let (_dir, paths, _root) = profile(&[]);
    fs::create_dir_all(paths.paused()).unwrap();
    let driver = Driver::open(paths.clone()).unwrap();
    assert!(!driver.status().paused);
    assert!(!driver.resume().await.unwrap());
    assert!(matches!(driver.pause().await, Err(DriverError::Io { .. })));
    assert!(!driver.status().paused);
}

#[tokio::test]
async fn a_pause_whose_write_fails_with_the_paused_file_in_place_still_pauses() {
    use std::os::unix::fs::PermissionsExt;

    if rustix::process::geteuid().is_root() {
        eprintln!("SKIPPED: running as root, so permissions do not block writes");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &base.join("Koidrive"), "d1");
    let driver = Driver::open(paths.clone()).unwrap();
    driver.start();
    assert_ne!(driver.status().folders[0].state, FolderState::Stopped);
    fs::write(paths.paused(), b"paused\n").unwrap();
    fs::set_permissions(&paths.state_dir, fs::Permissions::from_mode(0o500)).unwrap();
    let paused = driver.pause().await;
    fs::set_permissions(&paths.state_dir, fs::Permissions::from_mode(0o700)).unwrap();

    let error = paused.unwrap_err();
    assert!(
        matches!(error, DriverError::PausedUnsaved { .. }),
        "{error:?}"
    );
    assert!(
        error.to_string().starts_with("sync is paused, but "),
        "{error}"
    );
    assert!(driver.status().paused);
    assert_eq!(driver.status().folders[0].state, FolderState::Stopped);
}

#[tokio::test]
async fn a_pause_of_a_driver_that_was_not_started_only_keeps_the_profile_paused() {
    let (_dir, paths, _root) = profile(&[]);
    let driver = Driver::open(paths.clone()).unwrap();
    assert!(driver.pause().await.unwrap());
    assert!(paths.paused().exists());
    assert!(!driver.status().running);
    assert!(driver.resume().await.unwrap());
    assert_eq!(driver.status().folders[0].state, FolderState::Stopped);
}

#[tokio::test]
async fn watchers_wake_when_status_or_questions_change() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &root, "d1");
    fs::write(root.join("one"), b"1").unwrap();
    let store = paths.store(SPACE).unwrap();
    Store::open(&store)
        .unwrap()
        .transaction(|tx| {
            tx.put_question(&StoredQuestion {
                question_id: "q1".into(),
                asked_at_ms: 10,
                body: serde_json::to_vec(&mass_delete("q1")).unwrap(),
                answer: None,
            })
        })
        .unwrap();
    let driver = Driver::open(paths).unwrap();
    let mut watch = driver.watch();
    assert!(!watch.has_changed().unwrap());

    driver.answer("q1", Answer::Restore).unwrap();
    assert!(watch.has_changed().unwrap());
    watch.borrow_and_update();

    driver.start();
    assert!(watch.has_changed().unwrap());
    watch.borrow_and_update();
    while driver.status().folders[0].local_entries != Some(1) {
        watch.changed().await.unwrap();
    }

    watch.borrow_and_update();
    driver.stop().await;
    assert!(watch.has_changed().unwrap());
}

#[tokio::test]
async fn progress_alone_wakes_a_watcher_at_most_once_an_interval_and_a_state_change_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let driver = Driver::open(Paths::under(dir.path())).unwrap();
    let mut changes = driver.changes();
    let idle = tokio::time::timeout(Duration::from_secs(1), changes.next()).await;
    assert!(idle.is_err(), "an idle driver wakes no watcher");

    let progressed = driver.inner.progressed.clone();
    let marking = tokio::spawn(async move {
        let started = Instant::now();
        let mut marks = 0;
        while started.elapsed() < Duration::from_secs(1) {
            progressed.send_replace(());
            marks += 1;
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        marks
    });
    let started = Instant::now();
    let mut wakes: u128 = 0;
    while started.elapsed() < Duration::from_secs(1) {
        assert_eq!(changes.next().await.unwrap(), Change::Progress);
        wakes += 1;
    }
    let elapsed = started.elapsed();
    let marks = marking.await.unwrap();
    let most = 1 + elapsed.as_millis() / PROGRESS_INTERVAL.as_millis();
    assert!(marks > 100, "{marks} marks");
    assert!(
        (3..=most).contains(&wakes),
        "{wakes} wakes for {marks} marks in {elapsed:?}"
    );

    driver.inner.progressed.send_replace(());
    let changed = driver.inner.changed.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        changed.send_replace(());
    });
    let asked = Instant::now();
    assert_eq!(changes.next().await.unwrap(), Change::State);
    assert!(asked.elapsed() < PROGRESS_INTERVAL, "{:?}", asked.elapsed());
    let after = tokio::time::timeout(Duration::from_secs(1), changes.next()).await;
    assert!(after.is_err(), "the state change took the progress with it");
}

#[tokio::test]
async fn a_progress_change_waiting_out_the_interval_survives_a_dropped_wait() {
    let dir = tempfile::tempdir().unwrap();
    let driver = Driver::open(Paths::under(dir.path())).unwrap();
    let mut changes = driver.changes();
    driver.inner.progressed.send_replace(());
    assert_eq!(changes.next().await.unwrap(), Change::Progress);

    driver.inner.progressed.send_replace(());
    let dropped = tokio::time::timeout(PROGRESS_INTERVAL / 5, changes.next()).await;
    assert!(dropped.is_err(), "the interval holds the change back");
    let kept = tokio::time::timeout(PROGRESS_INTERVAL * 2, changes.next()).await;
    assert_eq!(kept.ok().map(Result::unwrap), Some(Change::Progress));
}

#[tokio::test(flavor = "multi_thread")]
async fn no_reader_opens_the_store_of_a_folder_while_it_is_unlinked() {
    for _ in 0..25 {
        let (_dir, paths, root) = profile(&[]);
        let driver = Driver::open(paths.clone()).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let reads = Arc::new(AtomicUsize::new(0));
        let reader = {
            let (driver, stop, reads) = (driver.clone(), stop.clone(), reads.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let _ = driver.open_questions();
                    let _ = driver.answered_question("q1");
                    let _ = driver.answer("q1", Answer::Delete);
                    let _ = driver.restores(SPACE);
                    reads.fetch_add(1, Ordering::SeqCst);
                }
            })
        };
        while reads.load(Ordering::SeqCst) == 0 {
            std::thread::yield_now();
        }
        let unlinked = driver.unlink(&root).await;
        stop.store(true, Ordering::SeqCst);
        reader.join().unwrap();
        assert!(unlinked.is_ok(), "{unlinked:?}");
        let store_dir = paths.store(SPACE).unwrap().parent().unwrap().to_path_buf();
        assert!(
            !store_dir.exists(),
            "a reader left {} behind",
            store_dir.display()
        );
    }
}

#[tokio::test]
async fn a_paused_driver_reports_no_progress() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    let (driver, gate) = paused_driver(&paths, &root).await;
    let status = uploading(&driver, 100);
    status.lock().unwrap().state = FolderState::CatchingUp;
    assert_eq!(
        driver.status().progress.map(|totals| totals.bytes_total),
        Some(100)
    );

    let pause = tokio::spawn({
        let driver = driver.clone();
        async move { driver.pause().await }
    });
    gate.open();
    assert!(pause.await.unwrap().unwrap());
    let paused = driver.status();
    assert_eq!(paused.folders[0].progress, None);
    assert_eq!(paused.progress, None);
}

#[tokio::test]
async fn the_status_shows_the_files_the_first_scan_has_found_so_far() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    let (driver, gate) = paused_driver(&paths, &root).await;
    let (status, found) = {
        let folders = driver.inner.folders.lock().unwrap();
        let folder = folders.values().next().unwrap();
        (folder.status.clone(), folder.observing.clone())
    };
    {
        let mut shown = status.lock().unwrap();
        shown.state = FolderState::CatchingUp;
        shown.activity = Some(Activity::Scanning { files: 0 });
    }
    *found.lock().unwrap() = Some(crate::observe::Observing::Walking { files: 12_345 });
    assert_eq!(
        driver.status().folders[0].activity,
        Some(Activity::Scanning { files: 12_345 })
    );
    *found.lock().unwrap() = Some(crate::observe::Observing::Reading {
        files: 7,
        bytes: 9_000,
    });
    assert_eq!(
        driver.status().folders[0].activity,
        Some(Activity::Checking {
            files: 7,
            bytes: 9_000
        })
    );
    {
        let mut shown = status.lock().unwrap();
        shown.progress = Some(crate::status::Progress::default());
        shown.activity = None;
        assert!(shown.end_transfers());
        assert!(!shown.end_transfers());
    }
    *found.lock().unwrap() = None;
    assert_eq!(
        driver.status().folders[0].activity,
        Some(Activity::CatchingUp)
    );
    status.lock().unwrap().state = FolderState::Offline;
    assert_eq!(driver.status().folders[0].activity, None);

    let tracker = driver
        .inner
        .folders
        .lock()
        .unwrap()
        .values()
        .next()
        .unwrap()
        .progress
        .clone();
    crate::progress::Credit::download(&tracker, []).moved(4_096);
    assert_eq!(driver.status().folders[0].bytes_moved, 4_096);
    gate.open();
    driver.stop().await;
}

#[tokio::test]
async fn signing_out_takes_its_folders_out_of_the_progress() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    let (driver, gate) = paused_driver(&paths, &root).await;
    uploading(&driver, 100);
    assert!(driver.inner.totals.lock().unwrap().totals().is_some());

    let sign_out = tokio::spawn({
        let driver = driver.clone();
        async move { driver.sign_out(true).await }
    });
    gate.open();
    sign_out.await.unwrap().unwrap();
    signed_out(&driver, &paths, &root);
    assert_eq!(driver.inner.totals.lock().unwrap().totals(), None);
}

#[tokio::test]
async fn unlinking_takes_the_folder_out_of_the_progress() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    let (driver, gate) = paused_driver(&paths, &root).await;
    uploading(&driver, 100);

    let unlink = tokio::spawn({
        let driver = driver.clone();
        let root = root.clone();
        async move { driver.unlink(&root).await }
    });
    gate.open();
    unlink.await.unwrap().unwrap();
    assert_eq!(driver.inner.totals.lock().unwrap().totals(), None);
}

#[tokio::test]
async fn signing_out_finishes_a_cancelled_unlink() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    let (driver, gate) = paused_driver(&paths, &root).await;
    let unlink = tokio::spawn({
        let driver = driver.clone();
        let root = root.clone();
        async move { driver.unlink(&root).await }
    });
    assert!(still_waiting(&unlink).await);
    unlink.abort();
    assert!(unlink.await.unwrap_err().is_cancelled());
    assert_eq!(driver.status().folders.len(), 1);
    let sign_out = tokio::spawn({
        let driver = driver.clone();
        async move { driver.sign_out(true).await }
    });
    assert!(still_waiting(&sign_out).await);

    gate.open();
    sign_out.await.unwrap().unwrap();
    signed_out(&driver, &paths, &root);
}

#[tokio::test]
async fn linking_or_relinking_a_folder_whose_unlink_is_unfinished_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    let (driver, gate) = paused_driver(&paths, &root).await;
    let unlink = tokio::spawn({
        let driver = driver.clone();
        let root = root.clone();
        async move { driver.unlink(&root).await }
    });
    assert!(still_waiting(&unlink).await);
    unlink.abort();
    assert!(unlink.await.unwrap_err().is_cancelled());
    gate.open();
    let space_id = driver.status().folders[0].space_id.clone();
    let linked = driver.link(&root, &space_id).await;
    assert!(
        matches!(linked, Err(DriverError::UnlinkUnfinished(ref folder)) if *folder == root),
        "{linked:?}"
    );
    let relinked = driver.relink(&root).await;
    assert!(
        matches!(relinked, Err(DriverError::UnlinkUnfinished(ref folder)) if *folder == root),
        "{relinked:?}"
    );
    driver.sign_out(true).await.unwrap();
    signed_out(&driver, &paths, &root);
}
