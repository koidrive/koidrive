// SPDX-License-Identifier: Apache-2.0

#[test]
fn the_service_runs_while_a_folder_is_linked_and_goes_with_the_last_one() {
    use State::*;
    assert_eq!(plan(1, NotInstalled, false), Action::Start);
    assert_eq!(plan(2, Stopped, false), Action::Start);
    assert_eq!(plan(1, Running, false), Action::Start);
    assert_eq!(plan(0, Running, false), Action::Remove);
    assert_eq!(plan(0, Stopped, false), Action::Remove);
    assert_eq!(plan(0, NotInstalled, false), Action::Leave);
    assert_eq!(plan(1, Unavailable, false), Action::Leave);
    assert_eq!(plan(0, Off, false), Action::Leave);
}

#[test]
fn an_agent_switched_off_in_login_items_starts_only_when_asked_and_goes_with_the_last_folder() {
    use State::*;
    assert_eq!(plan(1, SwitchedOff, false), Action::Leave);
    assert_eq!(plan(1, SwitchedOff, true), Action::Start);
    assert_eq!(plan(0, SwitchedOff, false), Action::Remove);
}

#[tokio::test]
async fn linking_starts_the_service_once_and_unlinking_the_last_folder_removes_it() {
    let fake = Fake::default();
    let service = service(&[]);
    let first = converge(&fake, &service, 1, false, false, &Daemon::Absent).await;
    assert_eq!((first.state, first.started), (State::Running, true));
    let text = first.text();
    assert!(
        text.contains("Syncing in the background (koidrive.service)"),
        "{text}"
    );
    assert!(!text.contains("log out"), "lingering is unknown: {text}");
    let second = converge(&fake, &service, 2, false, false, &Daemon::Absent).await;
    assert_eq!((second.state, second.started), (State::Running, false));
    assert_eq!(second.text(), "");
    assert_eq!(fake.restarts.get(), 0);
    assert_eq!(
        converge(&fake, &service, 1, false, false, &Daemon::Absent)
            .await
            .state,
        State::Running
    );
    let removed = converge(&fake, &service, 0, false, false, &Daemon::Absent).await;
    assert_eq!(
        (removed.state, removed.removed),
        (State::NotInstalled, true)
    );
    assert_eq!(
        removed.text(),
        "Stopped and removed the background service.\n"
    );
    assert_eq!(*fake.unit.borrow(), None);
    let again = converge(&fake, &service, 0, false, false, &Daemon::Absent).await;
    assert_eq!((again.state, again.removed), (State::NotInstalled, false));
    assert_eq!(again.text(), "");
}

#[tokio::test]
async fn a_service_that_spawns_a_moment_after_it_starts_reads_as_running() {
    let fake = Fake {
        spawning: Cell::new(3),
        ..Fake::default()
    };
    let report = converge(&fake, &service(&[]), 1, false, false, &Daemon::Absent).await;
    assert_eq!((report.state, report.started), (State::Running, true));
    assert!(!report.text().contains("not running"), "{}", report.text());
}

#[tokio::test]
async fn a_service_spawn_wait_can_be_cancelled_without_waiting_for_its_deadline() {
    let fake = Fake {
        spawning: Cell::new(u32::MAX),
        ..Fake::default()
    };
    let service = service(&[]);
    let started = Instant::now();
    let result = tokio::time::timeout(Duration::from_millis(250), async {
        converge(&fake, &service, 1, false, false, &Daemon::Absent).await
    })
    .await;
    assert!(
        result.is_err(),
        "the service must still be waiting to spawn"
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(fake.unit.borrow().is_some());
}

#[tokio::test]
async fn a_link_leaves_an_agent_switched_off_in_login_items_off_and_says_how_to_switch_it_on() {
    let fake = Fake::default();
    let service = service(&[]);
    converge(&fake, &service, 1, false, false, &Daemon::Absent).await;
    fake.running.set(false);
    fake.switched_off.set(true);
    let linked = converge(&fake, &service, 2, false, false, &Daemon::Absent).await;
    assert_eq!(linked.state, State::SwitchedOff);
    assert!(!fake.running.get());
    let text = linked.text();
    assert!(
        text.contains("Login Items > Allow in the Background"),
        "{text}"
    );
    assert!(
        text.contains("switch it on, then run: koi service start"),
        "{text}"
    );
    let line = linked.status_line();
    assert!(
        line.contains("switched off in Login Items (koidrive.service)"),
        "{line}"
    );
    assert!(linked.failed());
    let asked = converge(&fake, &service, 2, false, true, &Daemon::Absent).await;
    assert_eq!((asked.state, asked.started), (State::Running, true));
    assert!(!asked.failed());
}

#[tokio::test]
async fn removing_a_disabled_agent_preserves_the_removal_failure() {
    let service = service(&[]);
    let fake = Fake {
        unit: RefCell::new(Some(service.unit())),
        switched_off: Cell::new(true),
        remove_problem: Some("removal refused"),
        ..Fake::default()
    };
    let report = converge(&fake, &service, 0, false, false, &Daemon::Absent).await;
    assert_eq!(report.state, State::SwitchedOff);
    assert!(report.failed());
    assert_eq!(report.problem.as_deref(), Some("removal refused"));
    assert_eq!(report.failure_message(), "removal refused");
    assert!(!report.removed);
    assert_eq!(*fake.unit.borrow(), Some(service.unit()));
}

#[tokio::test]
async fn the_config_switch_keeps_koi_from_installing_or_removing_the_service() {
    let fake = Fake::default();
    let off = converge(&fake, &service(&[]), 1, true, false, &Daemon::Absent).await;
    assert_eq!(off.state, State::Off);
    assert_eq!(*fake.unit.borrow(), None);
    assert!(off.text().contains("off in config.toml"), "{}", off.text());
    fake.running.set(true);
    *fake.unit.borrow_mut() = Some(String::new());
    converge(&fake, &service(&[]), 0, true, false, &Daemon::Absent).await;
    assert!(fake.running.get());
}

#[tokio::test]
async fn without_a_service_manager_the_link_says_so_and_what_to_do() {
    let fake = Fake {
        unavailable: true,
        ..Fake::default()
    };
    let report = converge(&fake, &service(&[]), 1, false, false, &Daemon::Absent).await;
    assert_eq!(report.state, State::Unavailable);
    assert_eq!(report.problem.as_deref(), Some("no user bus"));
    let text = report.text();
    assert!(
        text.starts_with("Koidrive can't sync in the background: no user bus.\n"),
        "{text}"
    );
    assert!(text.contains("koi service start"), "{text}");
    assert_eq!(
        converge(&fake, &service(&[]), 0, false, false, &Daemon::Absent)
            .await
            .text(),
        ""
    );
}

#[tokio::test]
async fn lingering_is_turned_on_or_the_command_to_turn_it_on_is_named() {
    let allowed = Fake::default();
    allowed.lingers.set(Some(false));
    let report = converge(&allowed, &service(&[]), 1, false, false, &Daemon::Absent).await;
    assert_eq!(report.lingers, Some(true));
    let text = report.text();
    assert!(text.contains("keeps running after you log out"), "{text}");
    let refused = Fake {
        linger_refused: true,
        ..Fake::default()
    };
    refused.lingers.set(Some(false));
    let report = converge(&refused, &service(&[]), 1, false, false, &Daemon::Absent).await;
    assert_eq!(report.lingers, Some(false));
    assert!(
        report.text().contains("sudo loginctl enable-linger"),
        "{}",
        report.text()
    );
    assert!(report.status_line().contains("sudo loginctl enable-linger"));
}

#[tokio::test]
async fn resume_without_a_daemon_starts_the_service_implicitly() {
    let (_dir, paths, _root) = crate::ipc::tests::linked_profile();
    assert_eq!(
        ipc::run(&paths, Request::Pause).await.unwrap(),
        Response::Paused
    );
    assert_eq!(crate::resume(&paths).await.unwrap().text, "Resumed sync.\n");
    assert!(!paths.paused().exists());
    assert!(!paths.socket().exists());
    let fake = Fake::default();
    let caller = service(&[]);
    let mut daemon = None;
    let start_daemon = async {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !fake.running.get() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("implicit reconciliation starts the service");
        let driver = koidrive_driver::Driver::open(paths.clone()).unwrap();
        driver.start();
        daemon = Some(tokio::spawn(ipc::serve(
            koidrive_ipc::bind(&paths.socket()).unwrap(),
            driver,
        )));
    };
    let (report, ()) = tokio::join!(
        reconcile_with(&fake, Ok(caller), &paths, false),
        start_daemon
    );
    assert_eq!(
        (report.state, report.started, report.problem),
        (State::Running, true, None)
    );
    let daemon = daemon.unwrap();
    daemon.abort();
    let _ = daemon.await;
}

#[tokio::test]
async fn resume_without_a_daemon_preserves_disabled_login_items() {
    let (_dir, paths, _root) = crate::ipc::tests::linked_profile();
    assert_eq!(
        ipc::run(&paths, Request::Pause).await.unwrap(),
        Response::Paused
    );
    crate::resume(&paths).await.unwrap();
    let fake = Fake::default();
    let caller = service(&[]);
    *fake.unit.borrow_mut() = Some(caller.unit());
    fake.switched_off.set(true);

    let report = reconcile_with(&fake, Ok(caller), &paths, false).await;

    assert_eq!(
        (report.state, report.started, report.problem),
        (State::SwitchedOff, false, None)
    );
    assert!(!fake.running.get());
    assert!(fake.switched_off.get());
    assert!(!paths.paused().exists());
    assert!(!paths.socket().exists());
}

#[tokio::test]
async fn reconciling_leaves_an_agent_switched_off_in_login_items_off_and_no_reconcile_resumes_a_pause()
 {
    let (_dir, paths, _root) = crate::ipc::tests::linked_profile();
    let paused = ipc::run(&paths, Request::Pause).await.unwrap();
    assert_eq!(paused, Response::Paused);
    let fake = Fake::default();
    let caller = service(&[]);
    *fake.unit.borrow_mut() = Some(caller.unit());
    fake.switched_off.set(true);
    let report = reconcile_with(&fake, Ok(caller.clone()), &paths, false).await;
    assert_eq!(report.state, State::SwitchedOff);
    assert!(!fake.running.get());

    let driver = koidrive_driver::Driver::open(paths.clone()).unwrap();
    driver.start();
    let daemon = tokio::spawn(ipc::serve(
        koidrive_ipc::bind(&paths.socket()).unwrap(),
        driver.clone(),
    ));
    let report = reconcile_with(&fake, Ok(caller), &paths, true).await;
    assert_eq!((report.state, report.problem), (State::Running, None));
    let Response::Status { status } = ipc::run(&paths, Request::Status).await.unwrap() else {
        panic!("the daemon sent no status");
    };
    assert!(status.running && status.paused, "{status:?}");
    assert!(paths.paused().exists());
    daemon.abort();
}

#[tokio::test]
async fn an_older_koi_hands_the_service_to_the_newer_installed_one_without_opening_the_profile() {
    let (dir, paths, _root) = crate::ipc::tests::linked_profile();
    unreadable_stores(&paths);
    let ran = dir.path().join("ran");
    let newer = dir.path().join("newer-koi");
    fs::write(
            &newer,
            format!(
                "#!/bin/sh\n[ \"$1\" = --version ] && echo 'koi 99.0.0' && exit 0\necho \"$@\" > '{}'\necho '{{\"data\":{{\"service\":{{\"manager\":\"launchd\",\"name\":\"dev.koidrive.koi\",\"state\":\"running\"}}}}}}'\n",
                ran.display()
            ),
        )
        .unwrap();
    fs::set_permissions(&newer, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let fake = Fake::default();
    running_on(&fake, &newer);
    fake.running.set(false);
    for (asked, args) in [
        (false, "_reconcile --no-app --json"),
        (true, "service start --json"),
    ] {
        let report = reconcile_with(&fake, Ok(service(&[])), &paths, asked).await;
        assert_eq!(
            (report.state, report.name.as_str(), report.problem),
            (State::Running, "dev.koidrive.koi", None)
        );
        assert_eq!(fs::read_to_string(&ran).unwrap(), format!("{args}\n"));
        assert!(!fake.running.get());
        assert_eq!(runs(&fake), newer.to_str().unwrap());
    }

    let reporting = |name: &str, envelope: &str, status: u8| {
        let path = dir.path().join(name);
        fs::write(
                &path,
                format!("#!/bin/sh\n[ \"$1\" = --version ] && echo 'koi 99.0.0' && exit 0\necho '{envelope}'\nexit {status}\n"),
            )
            .unwrap();
        fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        path
    };
    let unknown = r#"{"data":{"service":{"manager":"launchd","name":"dev.koidrive.koi","state":"degraded"}},"error":null}"#;
    let degraded = reporting("degraded-koi", unknown, 0);
    running_on(&fake, &degraded);
    let report = reconcile_with(&fake, Ok(service(&[])), &paths, true).await;
    assert_eq!(
        (report.state, report.problem.as_deref()),
        (State::Running, None)
    );
    assert_eq!(report.name, fake.name(&service(&[])));
    assert!(!report.failed());
    let refused =
        r#"{"data":{"service":{"state":"degraded"}},"error":{"message":"it is degraded"}}"#;
    running_on(&fake, &reporting("refusing-koi", refused, 1));
    let report = reconcile_with(&fake, Ok(service(&[])), &paths, true).await;
    let problem = report.problem.unwrap();
    assert!(
        problem.contains("refusing-koi service start failed: it is degraded"),
        "{problem}"
    );

    let silent = koi_printing(dir.path(), "silent-koi", "koi 99.0.0\n", 0);
    running_on(&fake, &silent);
    let report = reconcile_with(&fake, Ok(service(&[])), &paths, true).await;
    assert_eq!(report.state, State::Running);
    assert!(!report.text().contains("Log in"), "{}", report.text());
    let problem = report.problem.unwrap();
    assert!(
        problem.contains("silent-koi service start reported no service"),
        "{problem}"
    );
}

#[tokio::test]
async fn with_a_daemon_running_reconciling_reads_the_status_from_its_socket_and_opens_no_store() {
    let (dir, paths, root) = crate::ipc::tests::linked_profile();
    unreadable_stores(&paths);
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
    let this = this_version();
    let same = koi_printing(dir.path(), "same-koi", &format!("koi {this}\n"), 0);
    let fake = Fake::default();
    running_on(&fake, &same);
    let report = reconcile_with(&fake, Ok(service(&[])), &paths, false).await;
    assert_eq!((report.state, report.problem), (State::Running, None));
    assert_eq!(runs(&fake), same.to_str().unwrap());
    assert_eq!(fake.restarts.get(), 0);
    daemon.abort();
}
