// SPDX-License-Identifier: Apache-2.0

#[test]
fn concise_starting_service_does_not_claim_sync_is_running() {
    let report = Report::new(&Fake::default(), "fixture".into(), State::Starting);
    assert_eq!(report.summary(), "Background sync is starting.\n");
}

#[test]
fn concise_service_keeps_the_specific_problem() {
    for state in ["unavailable", "running"] {
        let report: Report = serde_json::from_value(serde_json::json!({
            "manager":"launchd", "name":"dev.koidrive.internal", "state":state,
            "problem":"the daemon did not answer within 5 seconds"
        }))
        .unwrap();
        let summary = report.summary();
        assert!(
            summary.contains("the daemon did not answer within 5 seconds"),
            "{summary}"
        );
        assert!(!summary.contains("dev.koidrive.internal"));
    }
}

#[test]
fn concise_startup_problem_keeps_log_details_in_the_legacy_report() {
    let mut report: Report = serde_json::from_value(serde_json::json!({
            "manager":"launchd", "name":"dev.koidrive.internal", "state":"running",
            "problem":"it started, but its daemon did not answer within 5 seconds; its log: /internal/daemon.log"
        })).unwrap();
    report.summary_problem =
        Some("it started, but its daemon did not answer within 5 seconds".into());
    assert!(report.summary().contains("did not answer within 5 seconds"));
    assert!(!report.summary().contains("/internal/daemon.log"));
    assert!(report.text().contains("/internal/daemon.log"));
    let data = serde_json::to_value(report).unwrap();
    assert!(
        data["problem"]
            .as_str()
            .unwrap()
            .contains("/internal/daemon.log")
    );
    assert!(data.get("summary_problem").is_none());
}

#[test]
fn a_disabled_launch_agent_is_distinct_from_an_unloaded_one() {
    let disabled = r#"disabled services = {
            "dev.koidrive.koi.other" => disabled
            "dev.koidrive.koi.enabled" => enabled
            "dev.koidrive.koi.disabled" => disabled
        }"#;
    assert!(launchd_disabled(disabled, "dev.koidrive.koi.disabled"));
    assert!(!launchd_disabled(disabled, "dev.koidrive.koi.enabled"));
    assert!(!launchd_disabled(disabled, "dev.koidrive.koi.absent"));
    assert!(launchd_disabled(
        "\"dev.koidrive.koi.disabled\" => true",
        "dev.koidrive.koi.disabled"
    ));
}

#[test]
fn only_a_disabled_agent_is_switched_off_and_an_unloaded_one_starts_again() {
    assert_eq!(unloaded(true, true), State::SwitchedOff);
    assert_eq!(unloaded(true, false), State::Stopped);
    assert_eq!(unloaded(false, false), State::NotInstalled);
    assert_eq!(unloaded(false, true), State::NotInstalled);
    assert_eq!(plan(1, unloaded(true, false), false), Action::Start);
    assert_eq!(plan(1, unloaded(true, true), false), Action::Leave);
}

#[test]
fn a_failed_start_of_a_disabled_agent_has_one_login_items_message() {
    let mut report = Report::new(&Fake::default(), "koi".into(), State::SwitchedOff);
    assert_eq!(report.failure_message(), SWITCHED_OFF.trim_end());
    report.problem = Some(format!("bootstrap failed. {SWITCHED_OFF}"));
    report.folders = Some(1);
    let message = report.failure_message();
    assert_eq!(
        message,
        format!("bootstrap failed. {}", SWITCHED_OFF.trim_end())
    );
    assert!(!message.contains('\n'));
    assert!(message.contains("bootstrap"));
    assert!(!crate::printable(&message).contains("\\u{a}"));
}

#[test]
fn a_failed_status_probe_is_reported_without_a_folder_count() {
    let report = Report::unavailable(
        &Fake::default(),
        "the daemon did not answer within 5 seconds".into(),
    );
    assert_eq!(report.folders, None);
    let text = report.text();
    assert!(
        text.contains("the daemon did not answer within 5 seconds"),
        "{text}"
    );
    assert!(text.contains("can't sync in the background"), "{text}");
    let unavailable = Report::new(&Fake::default(), String::new(), State::Unavailable);
    assert!(unavailable.text().is_empty());
}

#[tokio::test]
async fn a_daemon_that_takes_the_connection_and_never_answers_does_not_hold_the_command() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    fs::create_dir_all(&paths.runtime_dir).unwrap();
    let _held = std::os::unix::net::UnixListener::bind(paths.socket()).unwrap();
    let answered = tokio::time::timeout(
        Duration::from_secs(5),
        answers(&paths, Duration::from_millis(300)),
    )
    .await;
    assert_eq!(answered, Ok(false));
}

#[tokio::test(start_paused = true)]
async fn a_daemon_that_takes_the_status_request_and_never_answers_is_a_problem_after_the_probe() {
    let (_dir, paths, _root) = crate::ipc::tests::linked_profile();
    let listener = koidrive_ipc::bind(&paths.socket()).unwrap();
    let daemon = tokio::spawn(async move {
        let _held = listener.accept().await.unwrap();
        std::future::pending::<()>().await
    });
    let fake = Fake::default();
    let began = tokio::time::Instant::now();
    let report = reconcile_with(&fake, Ok(service(&[])), &paths, false).await;
    assert!(began.elapsed() < PROBE + Duration::from_secs(2));
    assert_eq!(
        report.problem.as_deref(),
        Some("the daemon did not answer within 5 seconds")
    );
    assert_ne!(report.state, State::Unavailable);
    assert!(!report.text().contains("Log in"), "{}", report.text());
    daemon.abort();
}

#[tokio::test(start_paused = true)]
async fn a_daemon_that_starts_and_then_never_replies_is_a_problem_after_the_probe() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let _held = koidrive_driver::Driver::open(paths.clone()).unwrap();
    let socket = paths.socket();
    let daemon = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let listener = koidrive_ipc::bind(&socket).unwrap();
        let _accepted = listener.accept().await.unwrap();
        std::future::pending::<()>().await;
    });
    let started = tokio::time::Instant::now();
    let result = daemon_status(&paths).await;
    daemon.abort();
    assert!(started.elapsed() < Duration::from_secs(1) + PROBE + Duration::from_secs(2));
    assert_eq!(
        result.unwrap_err(),
        "the daemon did not answer within 5 seconds"
    );
}

#[tokio::test]
async fn a_service_that_never_leaves_starting_is_a_retryable_problem() {
    let (_dir, paths, _root) = crate::ipc::tests::linked_profile();
    let fake = Fake {
        script: RefCell::new(VecDeque::from(vec![State::Starting; 1000])),
        ..Fake::default()
    };
    let report = reconcile_with(&fake, Ok(service(&[])), &paths, false).await;
    assert_eq!(report.state, State::Starting);
    let problem = report.problem.clone().unwrap();
    assert!(
        problem.starts_with("it was still starting after 15 seconds; its log: "),
        "{problem}"
    );
    assert!(report.failed());
    assert!(report.retryable());
}

#[tokio::test]
async fn a_service_that_finishes_restarting_is_planned_on_its_settled_state() {
    let service = service(&[]);
    let fake = Fake {
        unit: RefCell::new(Some("an older definition".to_owned())),
        running: Cell::new(true),
        script: RefCell::new(VecDeque::from(vec![State::Starting; 3])),
        ..Fake::default()
    };
    let older = Daemon::Running(Some(version("0.0.0-0")));
    let report = converge(&fake, &service, 1, false, false, &older).await;
    assert_eq!(report.state, State::Running);
    assert_eq!(*fake.unit.borrow(), Some(service.unit()));
    assert_eq!(fake.restarts.get(), 1);
    fake.script
        .replace(VecDeque::from(vec![State::Starting; 3]));
    converge(&fake, &service, 1, false, false, &older).await;
    assert_eq!(fake.restarts.get(), 1);
}

#[tokio::test]
async fn a_service_that_starts_after_a_late_spawn_gets_the_startup_wait() {
    let service = service(&[]);
    let script = [State::NotInstalled; 1]
        .into_iter()
        .chain([State::Stopped; 10])
        .chain([State::Starting; 60]);
    let fake = Fake {
        script: RefCell::new(script.collect()),
        ..Fake::default()
    };
    let report = converge(&fake, &service, 1, false, false, &Daemon::Absent).await;
    assert_eq!((report.state, report.started), (State::Running, true));
    assert_eq!(report.problem, None);
}

#[tokio::test]
async fn a_service_that_stops_after_starting_names_its_daemon_exit() {
    let (_dir, paths, _root) = crate::ipc::tests::linked_profile();
    let fake = Fake {
        script: RefCell::new(VecDeque::from(vec![State::Stopped; 1000])),
        crashed: Some("exit code 1"),
        ..Fake::default()
    };
    let report = reconcile_with(&fake, Ok(service(&[])), &paths, false).await;
    assert_eq!(report.state, State::Stopped);
    let problem = report.problem.clone().unwrap();
    assert!(
        problem.starts_with("its daemon keeps exiting (exit code 1); its log: "),
        "{problem}"
    );
}

#[test]
fn launchd_reports_a_restarting_service_as_starting_and_a_crashing_one_as_stopped() {
    let job = |state: &str, exit: &str| {
        format!(
            "gui/501/dev.koidrive.koi = {{\n\tstate = {state}\n\truns = 2\n\t{exit}\n\tendpoints = {{\n\t\tstate = active\n\t}}\n}}\n"
        )
    };
    assert_eq!(
        launchd_state(&job("running", "last exit code = 1")),
        State::Running
    );
    assert_eq!(
        launchd_state(&job("SIGTERMed", "last exit code = (never exited)")),
        State::Starting
    );
    assert_eq!(
        launchd_state(&job("spawn scheduled", "last exit code = 0")),
        State::Starting
    );
    assert_eq!(
        launchd_state(&job(
            "spawn scheduled",
            "last terminating signal = Terminated: 15"
        )),
        State::Starting
    );
    assert_eq!(
        launchd_state(&job("xpcproxy", "last exit code = 0")),
        State::Starting
    );
    assert_eq!(
        launchd_state(&job("spawn scheduled", "last exit code = 1")),
        State::Stopped
    );
    assert_eq!(
        launchd_state(&job("xpcproxy", "last exit code = 1")),
        State::Stopped
    );
    assert_eq!(launchd_state(&job("not running", "")), State::Stopped);
    assert_eq!(launchd_exit(&job("running", "last exit code = 78")), None);
    assert_eq!(
        launchd_exit(&job("spawn scheduled", "last exit code = 78")).as_deref(),
        Some("exit code 78")
    );
    assert!(plan(1, State::Starting, false) == Action::Leave);
}

#[tokio::test]
async fn a_daemon_that_keeps_exiting_is_a_stopped_service_that_names_its_exit_and_log() {
    let (_dir, paths, _root) = crate::ipc::tests::linked_profile();
    let fake = Fake {
        crashed: Some("exit code 1"),
        ..Fake::default()
    };
    let report = reconcile_with(&fake, Ok(service(&[])), &paths, false).await;
    assert_eq!(report.state, State::Stopped);
    let problem = report.problem.clone().unwrap();
    assert!(
        problem.starts_with("its daemon keeps exiting (exit code 1); its log: "),
        "{problem}"
    );
    let summary = report.summary();
    assert!(
        summary.contains("its daemon keeps exiting (exit code 1)"),
        "{summary}"
    );
    assert!(!summary.contains("its log:"), "{summary}");
    assert!(report.failed());
    assert!(!report.retryable());
}

#[test]
fn only_a_running_service_whose_daemon_has_a_problem_is_worth_retrying() {
    let fake = Fake::default();
    let problem = |state| Report {
        problem: Some("the daemon did not answer within 5 seconds".into()),
        ..Report::new(&fake, String::new(), state)
    };
    assert!(problem(State::Running).retryable());
    assert!(!problem(State::Stopped).retryable());
    assert!(!problem(State::Unavailable).retryable());
    assert!(!Report::new(&fake, String::new(), State::Running).retryable());
}

#[tokio::test]
async fn a_daemon_still_starting_after_a_restart_is_waited_for_and_not_a_problem() {
    let (_dir, paths, _root) = crate::ipc::tests::linked_profile();
    let driver = koidrive_driver::Driver::open(paths.clone()).unwrap();
    let socket = paths.socket();
    let daemon = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        ipc::serve(koidrive_ipc::bind(&socket).unwrap(), driver).await;
    });
    let fake = Fake::default();
    let report = reconcile_with(&fake, Ok(service(&[])), &paths, false).await;
    assert_eq!(
        (report.state, report.problem.as_deref()),
        (State::Running, None)
    );
    assert!(!report.failed());
    daemon.abort();
}

#[test]
fn a_koi_that_leaves_a_child_holding_its_output_reads_as_unversioned_within_the_probe() {
    let dir = tempfile::tempdir().unwrap();
    let koi = dir.path().join("lingering-koi");
    fs::write(&koi, "#!/bin/sh\necho 'koi 99.0.0'\nsleep 30 &\n").unwrap();
    fs::set_permissions(&koi, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let began = Instant::now();
    assert_eq!(version_of(&koi), None);
    assert!(began.elapsed() < Duration::from_secs(30));
}

#[test]
fn a_folder_macos_keeps_from_the_daemon_names_the_settings_and_the_restart() {
    let text = blocked_text(&[
        Blocked {
            folder: "/Users/ada/Documents/Work".into(),
            waiting: true,
        },
        Blocked {
            folder: "/Users/ada/Desktop/Notes".into(),
            waiting: false,
        },
    ]);
    assert!(
            text.contains(
                "koi is still waiting to read /Users/ada/Documents/Work. If macOS asks whether koi may access it, click Allow"
            ),
            "{text}"
        );
    assert!(
        text.contains("Privacy & Security > Files & Folders"),
        "{text}"
    );
    assert!(
        text.contains("koi folder restart /Users/ada/Desktop/Notes"),
        "{text}"
    );
}

#[test]
fn the_restart_command_for_a_folder_with_a_space_pastes_as_one_argument() {
    let text = blocked_text(&[Blocked {
        folder: "/Users/ada/Desktop/Work Notes".into(),
        waiting: false,
    }]);
    assert!(
        text.contains("Then run: koi folder restart '/Users/ada/Desktop/Work Notes'\n"),
        "{text}"
    );
}
