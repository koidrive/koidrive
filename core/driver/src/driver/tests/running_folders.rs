// SPDX-License-Identifier: Apache-2.0

#[tokio::test]
async fn a_folder_that_holds_a_reopened_profile_halts_before_it_is_observed() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap().join("Koidrive");
    let paths = Paths::under(&root.join("moved-profile"));
    linked(&paths, &root, "d1");
    fs::write(root.join("file"), b"synced").unwrap();

    let driver = Driver::open(paths).unwrap();
    driver.start();
    let problem = halted(&driver).await;
    assert!(
        problem.contains("overlaps Koidrive's own files"),
        "{problem}"
    );
    assert_eq!(driver.status().folders[0].local_entries, None);
    assert_eq!(claim::owner(&root).unwrap(), None);
    driver.stop().await;
}

#[test]
fn an_overlap_is_found_through_a_symbolic_link() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let state = root.join("profile/state");
    fs::create_dir_all(&state).unwrap();
    let alias = base.join("alias");
    std::os::unix::fs::symlink(&root, &alias).unwrap();
    assert!(matches!(
        check_placement(&alias, &[state], &[]),
        Err(DriverError::FolderHoldsProfile(_))
    ));
    assert!(matches!(
        check_placement(&root.join("profile"), &[], &[alias]),
        Err(DriverError::FolderOverlaps(_))
    ));
}

#[tokio::test]
async fn two_profiles_never_sync_one_folder_at_once_or_in_turn() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let (a, b) = (Paths::under(&base.join("a")), Paths::under(&base.join("b")));
    linked(&a, &root, "d1");
    linked(&b, &root, "d2");
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

    first.stop().await;
    drop(
        claim::lock(&root)
            .unwrap()
            .expect("stopping releases the folder's lock"),
    );
    fs::write(root.join("two"), b"2").unwrap();
    first.start();
    eventually("the restarted profile observes the folder", || {
        entries(&first) == Some(2)
    })
    .await;
    assert_ne!(first.status().folders[0].state, FolderState::Halted);
    second.start();
    let problem = halted(&second).await;
    assert!(problem.contains("another Koidrive process"), "{problem}");
    second.stop().await;
    first.stop().await;

    second.start();
    assert!(halted(&second).await.contains("another Koidrive process"));
    second.restart(&root).await.unwrap();
    let problem = halted(&second).await;
    assert!(problem.contains("linked by device d1"), "{problem}");
    second.stop().await;
    assert!(claim::lock(&root).unwrap().is_some());
}

#[tokio::test]
async fn deleting_the_private_directory_does_not_let_another_profile_in() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let (a, b) = (Paths::under(&base.join("a")), Paths::under(&base.join("b")));
    linked(&a, &root, "d1");
    linked(&b, &root, "d2");
    fs::write(root.join("one"), b"1").unwrap();
    let first = Driver::open(a).unwrap();
    first.start();
    eventually("the first profile observes the folder", || {
        first.status().folders[0].local_entries == Some(1)
    })
    .await;

    fs::remove_dir_all(root.join(koidrive_fs::PRIVATE_DIR)).unwrap();
    let third = Paths::under(&base.join("c"));
    paired(&third, "d3");
    assert!(matches!(
        Driver::open(third).unwrap().link(&root, "Projects").await,
        Err(DriverError::FolderInUse(_))
    ));
    write_marker(
        &root,
        &Marker {
            space_id: SPACE.into(),
            effects: 0,
            token: None,
        },
    )
    .unwrap();
    let second = Driver::open(b).unwrap();
    second.start();
    let problem = halted(&second).await;
    assert!(problem.contains("another Koidrive process"), "{problem}");
    assert_ne!(first.status().folders[0].state, FolderState::Halted);
    second.stop().await;
    first.stop().await;
}

#[tokio::test]
async fn a_folder_replaced_under_its_task_halts_it() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &root, "d1");
    let driver = Driver::open(paths).unwrap();
    driver.start();
    eventually("the folder is claimed", || {
        claim::owner(&root).unwrap().is_some()
    })
    .await;

    fs::rename(&root, base.join("Moved")).unwrap();
    fs::create_dir(&root).unwrap();
    let problem = halted(&driver).await;
    assert!(problem.contains("no longer the folder"), "{problem}");
    driver.stop().await;
    assert!(claim::lock(&base.join("Moved")).unwrap().is_some());
}

#[tokio::test]
async fn a_folder_replaced_before_its_task_opens_it_halts_the_task_as_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &root, "d1");
    let (at, moved) = (root.clone(), base.join("Moved"));
    crate::folder::BEFORE_OPEN.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            fs::rename(&at, &moved).unwrap();
            fs::create_dir(&at).unwrap();
        }));
    });
    let driver = Driver::open(paths).unwrap();
    driver.start();

    let problem = halted(&driver).await;
    assert!(problem.contains("no longer the folder"), "{problem}");
    driver.stop().await;
}

#[tokio::test]
async fn a_batch_scanned_before_its_folder_was_replaced_halts_the_task_as_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &root, "d1");
    let mut driver = Driver::open(paths).unwrap();
    let (at, moved) = (root.clone(), base.join("Moved"));
    Arc::get_mut(&mut driver.inner).unwrap().observe_fault =
        Some(Arc::new(crate::folder::ObserveFault {
            paused: Mutex::new(None),
            before_batch: Mutex::new(Some(Box::new(move || {
                fs::rename(&at, &moved).unwrap();
                fs::create_dir(&at).unwrap();
            }))),
        }));
    driver.start();

    let problem = halted(&driver).await;
    assert!(problem.contains("no longer the folder"), "{problem}");
    driver.stop().await;
}

#[tokio::test]
async fn stopping_does_not_wait_for_the_first_scan_to_read_every_file() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &root, "d1");
    for n in 0..16 {
        let file = fs::File::create(root.join(format!("large-{n}"))).unwrap();
        file.set_len(1 << 40).unwrap();
    }
    let mut driver = Driver::open(paths).unwrap();
    let (entered, scanning) = tokio::sync::oneshot::channel();
    Arc::get_mut(&mut driver.inner)
        .unwrap()
        .rules
        .get_mut()
        .unwrap()
        .matcher = Arc::new(Paused {
        entered: Mutex::new(Some(entered)),
        gate: Arc::new((Mutex::new(true), std::sync::Condvar::new())),
    });
    driver.start();
    scanning.await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    driver.stop().await;
}

#[tokio::test]
async fn stopping_does_not_wait_out_the_observers_pause_after_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &root, "d1");
    let mut driver = Driver::open(paths).unwrap();
    let (paused, pausing) = tokio::sync::oneshot::channel();
    Arc::get_mut(&mut driver.inner).unwrap().observe_fault =
        Some(Arc::new(crate::folder::ObserveFault {
            paused: Mutex::new(Some(paused)),
            before_batch: Mutex::new(None),
        }));
    driver.start();
    pausing.await.unwrap();

    driver.stop().await;
    assert!(lockable(&root), "the observer's thread released the folder");
}

#[tokio::test]
async fn a_folder_moved_away_and_back_syncs_again() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &root, "d1");
    let driver = Driver::open(paths).unwrap();
    driver.start();
    eventually("the folder is claimed", || {
        claim::owner(&root).unwrap().is_some()
    })
    .await;

    fs::rename(&root, base.join("Moved")).unwrap();
    let problem = halted(&driver).await;
    assert!(problem.contains("no longer"), "{problem}");
    fs::rename(base.join("Moved"), &root).unwrap();
    eventually("the folder syncs again", || {
        driver.status().folders[0].state != FolderState::Halted
            && claim::lock(&root).unwrap().is_none()
    })
    .await;
    driver.stop().await;
    assert!(claim::lock(&root).unwrap().is_some());
}

#[tokio::test]
async fn a_halted_folder_shows_in_the_next_hourly_report_and_its_recovery_after() {
    use crate::health::tests::{named, posthog, with_prefix};

    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &root, "d1");
    let recorder = koidrive_telemetry::Recorder::on();
    let driver = Driver::open_with_telemetry(paths, recorder.clone()).unwrap();
    driver.start();
    eventually("the folder is claimed", || {
        claim::owner(&root).unwrap().is_some()
    })
    .await;
    driver.record_health();
    let healthy = posthog(recorder.take());
    assert_eq!(named(&healthy, "hourly_report")[0]["l.sync.halted"], 0);

    fs::rename(&root, base.join("Moved")).unwrap();
    halted(&driver).await;
    let now = koidrive_telemetry::now();
    driver.record_health_at(now);
    let events = posthog(recorder.take());
    let report = named(&events, "hourly_report")[0];
    assert_eq!(report["l.sync.halted"], 1);
    assert_eq!(report["l.sync.oldest_halt_minutes"], 0);
    let reasons = with_prefix(report, "l.halted.");
    let [(reason, 1)] = reasons[..] else {
        panic!("{reasons:?}")
    };
    let reason = reason.trim_start_matches("l.halted.").to_owned();
    assert!(named(&events, "problem_cleared").is_empty());

    fs::rename(base.join("Moved"), &root).unwrap();
    eventually("the folder syncs again", || {
        driver.status().folders[0].state != FolderState::Halted
    })
    .await;
    driver.record_health_at(now + 3_600);
    let events = posthog(recorder.take());
    let report = named(&events, "hourly_report")[0];
    assert_eq!(report["l.sync.halted"], 0);
    assert!(with_prefix(report, "l.halted.").is_empty(), "{report}");
    let cleared = named(&events, "problem_cleared");
    assert_eq!(cleared.len(), 1, "{events:?}");
    assert_eq!(cleared[0]["problem"], format!("halted.{reason}"));
    assert_eq!(cleared[0]["lasted"], "under_1h");
    driver.stop().await;
}

#[tokio::test]
async fn another_folder_with_the_spaces_marker_waits_for_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &root, "d1");
    let original = fs::metadata(&root).unwrap().ino();
    let driver = Driver::open(paths).unwrap();
    driver.start();
    eventually("the folder is claimed", || {
        claim::owner(&root).unwrap().is_some()
    })
    .await;

    fs::rename(&root, base.join("Moved")).unwrap();
    fs::create_dir(&root).unwrap();
    write_marker(
        &root,
        &Marker {
            space_id: SPACE.into(),
            effects: 0,
            token: None,
        },
    )
    .unwrap();
    fs::write(root.join("replacement-only.txt"), b"not synced on its own").unwrap();
    assert_ne!(fs::metadata(&root).unwrap().ino(), original);
    eventually("the folder waits for a restart", || {
        driver.status().folders[0]
            .problem
            .as_ref()
            .is_some_and(|problem| problem.contains("koi folder restart"))
    })
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let status = driver.status();
    assert_eq!(status.folders[0].state, FolderState::Halted);
    assert_eq!(claim::owner(&root).unwrap(), None);
    assert!(claim::lock(&root).unwrap().is_some());

    driver.restart(&root).await.unwrap();
    eventually("the restart claims the folder at the path", || {
        claim::owner(&root).unwrap().is_some()
    })
    .await;
    driver.stop().await;
}

#[tokio::test]
async fn a_restart_syncs_a_folder_again_once_its_problem_is_fixed() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &root, "d1");
    let other = Owner {
        server: nowhere().into(),
        device_id: "d2".into(),
    };
    claim::set_owner(&root, &other).unwrap();
    let driver = Driver::open(paths).unwrap();
    driver.start();
    let problem = halted(&driver).await;
    assert!(problem.contains("device d2"), "{problem}");

    claim::release(&root, &other).unwrap().durable.unwrap();
    let restarted = driver.restart(&root).await.unwrap();
    assert_ne!(restarted.state, FolderState::Halted);
    eventually("the folder is claimed by this device", || {
        claim::owner(&root)
            .unwrap()
            .is_some_and(|owner| owner.device_id == "d1")
    })
    .await;
    driver.stop().await;
}
