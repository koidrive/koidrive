// SPDX-License-Identifier: Apache-2.0

#[tokio::test]
async fn the_service_lock_is_held_while_a_koi_picks_and_starts_the_binary() {
    let (dir, paths, root) = crate::ipc::tests::linked_profile();
    let folder: koidrive_driver::FolderStatus = serde_json::from_value(serde_json::json!({
        "space_id": crate::ipc::tests::SPACE,
        "space_name": "Projects",
        "path": root,
        "state": "up_to_date",
        "history_id": null,
        "last_seq": null,
        "head_seq": null,
        "remote_nodes": 0,
        "local_entries": null,
        "questions": 0,
        "problem": null,
    }))
    .unwrap();
    let status = koidrive_driver::Status {
        running: true,
        version: Some(this_version().to_string()),
        folders: vec![folder],
        ..koidrive_driver::Status::default()
    };
    let listener = koidrive_ipc::bind(&paths.socket()).unwrap();
    let daemon = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            koidrive_ipc::read_line(reader).await.unwrap();
            let response = Response::Status {
                status: status.clone(),
            };
            let _ = koidrive_ipc::write_line(&mut writer, &response).await;
        }
    });
    let older = koi_printing(dir.path(), "older-koi", "koi 0.0.0-0\n", 0);
    let fake = Fake {
        service_lock: Some(paths.service_lock()),
        ..Fake::default()
    };
    running_on(&fake, &older);
    let report = reconcile_with(&fake, Ok(service(&[])), &paths, false).await;
    assert_eq!((report.state, report.problem), (State::Running, None));
    assert_eq!(fake.locked_at_start.get(), Some(true));
    let file = fs::File::open(paths.service_lock()).unwrap();
    assert!(rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_ok());
    daemon.abort();
}

#[tokio::test]
async fn a_service_lock_another_koi_holds_past_the_deadline_is_a_problem() {
    let (_dir, paths, _root) = crate::ipc::tests::linked_profile();
    let held = (fs::File::options().create(true).truncate(false).write(true))
        .open(paths.service_lock())
        .unwrap();
    let held = Flock::take(held).unwrap().unwrap();
    let began = Instant::now();
    let problem = lock_service(&paths, Duration::from_secs(1))
        .await
        .unwrap_err();
    assert!(began.elapsed() >= Duration::from_secs(1));
    assert!(problem.contains("another koi has held"), "{problem}");
    drop(held);
    assert!(
        lock_service(&paths, Duration::from_secs(1))
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn waiting_for_the_service_lock_lets_the_other_tasks_of_the_command_run() {
    let (_dir, paths, _root) = crate::ipc::tests::linked_profile();
    let held = (fs::File::options().create(true).truncate(false).write(true))
        .open(paths.service_lock())
        .unwrap();
    let _held = Flock::take(held).unwrap().unwrap();
    let ticks = Cell::new(0);
    let ticking = async {
        loop {
            tokio::time::sleep(Duration::from_millis(50)).await;
            ticks.set(ticks.get() + 1);
        }
    };
    let waited = tokio::select! {
        waited = lock_service(&paths, Duration::from_secs(1)) => waited,
        () = ticking => unreachable!(),
    };
    assert!(waited.is_err());
    assert!(
        ticks.get() >= 5,
        "{} ticks while the lock was held",
        ticks.get()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn waiting_for_a_delegated_koi_lets_the_interrupt_branch_run() {
    let dir = tempfile::tempdir().unwrap();
    let stalled = dir.path().join("stalled-koi");
    fs::write(&stalled, "#!/bin/sh\nexec sleep 30\n").unwrap();
    fs::set_permissions(
        &stalled,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let fake = Fake::default();
    let service = service(&[]);
    tokio::select! {
        biased;
        _ = delegated(&fake, &service, &stalled, false, Duration::from_millis(200)) => {
            panic!("the delegated child wait blocked the interrupt branch");
        }
        () = tokio::task::yield_now() => {}
    }
}

#[tokio::test(flavor = "current_thread")]
async fn cancelling_delegation_kills_and_reaps_the_child_in_the_callers_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("pid");
    let stalled = dir.path().join("stalled-koi");
    fs::write(
        &stalled,
        format!(
            "#!/bin/sh\necho $$ > '{}'\nexec sleep 30\n",
            pid_file.display()
        ),
    )
    .unwrap();
    fs::set_permissions(
        &stalled,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let fake = Fake::default();
    let service = service(&[]);
    let mut delegated = Box::pin(delegated(&fake, &service, &stalled, false, DELEGATED));
    let pid = tokio::select! {
        biased;
        report = &mut delegated => panic!("delegation finished before cancellation: {report:?}"),
        pid = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(pid) = fs::read_to_string(&pid_file)
                    && let Ok(pid) = pid.trim().parse::<i32>()
                {
                    break rustix::process::Pid::from_raw(pid).unwrap();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }) => pid.expect("the delegated child started"),
    };
    let group = rustix::process::getpgid(Some(pid));
    drop(delegated);
    tokio::time::timeout(Duration::from_secs(5), async {
        while rustix::process::test_kill_process(pid) != Err(rustix::io::Errno::SRCH) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("cancellation killed and reaped the delegated child");
    assert_eq!(group, Ok(rustix::process::getpgrp()));
    assert_eq!(
        rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::NOHANG).unwrap_err(),
        rustix::io::Errno::CHILD
    );
}

#[tokio::test]
async fn a_delegated_koi_that_closes_stdout_still_has_to_exit_before_the_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("pid");
    let stalled = dir.path().join("stalled-koi");
    fs::write(
        &stalled,
        format!(
            "#!/bin/sh\nexec 1>&-\necho $$ > '{}'\nexec sleep 30\n",
            pid_file.display()
        ),
    )
    .unwrap();
    fs::set_permissions(
        &stalled,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let report = delegated(
        &Fake::default(),
        &service(&[]),
        &stalled,
        false,
        Duration::from_secs(1),
    )
    .await;
    let pid = rustix::process::Pid::from_raw(
        fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        rustix::process::test_kill_process(pid),
        Err(rustix::io::Errno::SRCH)
    );
    assert!(
        report
            .problem
            .unwrap()
            .contains("did not finish within 1 seconds")
    );
}

#[tokio::test]
async fn a_delegated_koi_that_exits_cannot_leave_its_stdout_wait_unbounded() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("pid");
    let holder_file = dir.path().join("holder");
    let stalled = dir.path().join("stalled-koi");
    fs::write(
        &stalled,
        format!(
            "#!/bin/sh\necho $$ > '{}'\nsleep 30 &\necho $! > '{}'\n",
            pid_file.display(),
            holder_file.display()
        ),
    )
    .unwrap();
    fs::set_permissions(
        &stalled,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let report = tokio::time::timeout(
        Duration::from_secs(5),
        delegated(
            &Fake::default(),
            &service(&[]),
            &stalled,
            false,
            Duration::from_secs(1),
        ),
    )
    .await;
    let holder = rustix::process::Pid::from_raw(
        fs::read_to_string(holder_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap(),
    )
    .unwrap();
    let _ = rustix::process::kill_process(holder, rustix::process::Signal::KILL);
    let pid = rustix::process::Pid::from_raw(
        fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::NOHANG).unwrap_err(),
        rustix::io::Errno::CHILD
    );
    assert!(
        report
            .unwrap()
            .problem
            .unwrap()
            .contains("did not finish within 1 seconds")
    );
}

#[tokio::test]
async fn a_delegated_report_keeps_the_folders_the_newer_koi_counted_and_this_profiles_log() {
    let dir = tempfile::tempdir().unwrap();
    let newer = dir.path().join("newer-koi");
    fs::write(
            &newer,
            r#"#!/bin/sh
echo '{"data":{"service":{"manager":"systemd","name":"koidrive.service","state":"stopped","folders":1}}}'
"#,
        )
        .unwrap();
    fs::set_permissions(&newer, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let fake = Fake::default();
    let service = service(&[]);
    let report = delegated(&fake, &service, &newer, false, PROBE).await;
    let text = report.text();
    assert!(text.contains("is installed but not running"), "{text}");
    assert!(
        text.contains(&service.log(ServiceManager::Systemd)),
        "{text}"
    );
}

#[tokio::test]
async fn a_newer_koi_that_does_not_finish_is_killed_and_reported_after_the_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let pid = dir.path().join("pid");
    let stalled = dir.path().join("stalled-koi");
    fs::write(
        &stalled,
        format!("#!/bin/sh\necho $$ > '{}'\nexec sleep 30\n", pid.display()),
    )
    .unwrap();
    fs::set_permissions(
        &stalled,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let began = Instant::now();
    let report = delegated(
        &Fake::default(),
        &service(&[]),
        &stalled,
        true,
        Duration::from_secs(1),
    )
    .await;
    assert!(began.elapsed() < Duration::from_secs(3));
    let problem = report.problem.unwrap();
    assert!(
        problem.contains("stalled-koi service start did not finish within 1 seconds"),
        "{problem}"
    );
    let pid = fs::read_to_string(&pid).unwrap();
    let alive = Command::new("kill")
        .args(["-0", pid.trim()])
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(!alive.success(), "the child was killed and reaped");
}
