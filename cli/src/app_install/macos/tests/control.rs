use super::*;

#[tokio::test]
async fn lock_guard_releases_inherited_descriptors_without_losing_exclusion() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    let root = private_work(&f.config.work, true).unwrap();
    let (_tx, cancel) = watch::channel(false);
    let control = Control {
        deadline: Instant::now() + Duration::from_secs(5),
        cancel,
    };
    let guard = lock(&root, Duration::from_secs(5), &control).await.unwrap();
    let inherited = guard.0.try_clone().unwrap();
    let competing = regular_at(&root, OsStr::new(".koidrive-app-install.lock")).unwrap();
    assert_eq!(
        rustix::fs::flock(&competing, FlockOperation::NonBlockingLockExclusive),
        Err(rustix::io::Errno::WOULDBLOCK)
    );
    drop(guard);
    rustix::fs::flock(&competing, FlockOperation::NonBlockingLockExclusive).unwrap();
    assert_eq!(
        rustix::fs::flock(&inherited, FlockOperation::NonBlockingLockExclusive),
        Err(rustix::io::Errno::WOULDBLOCK)
    );
    rustix::fs::flock(&competing, FlockOperation::Unlock).unwrap();
}

#[test]
fn ci_and_unsupported_platforms_are_hard_skips() {
    for macos in [true, false] {
        for arm64 in [true, false] {
            assert_eq!(skip_reason(true, macos, arm64), Some(AppSkip::Ci));
        }
    }
    assert_eq!(skip_reason(false, false, true), Some(AppSkip::Platform));
    assert_eq!(skip_reason(false, true, false), Some(AppSkip::Architecture));
    assert_eq!(skip_reason(false, true, true), None);
}

#[tokio::test]
async fn deadline_kills_and_reaps_child_then_cleans_mount() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    f.flag("attach-sleep");
    let deadline = Instant::now() + CLEANUP + Duration::from_secs(10);
    let ensure = ensure_with(
        f.config.clone(),
        Version::parse(TARGET).unwrap(),
        AppLaunch::Preserve,
        deadline,
    );
    tokio::pin!(ensure);
    let ready = async {
        loop {
            if let Some(pid) = fs::read_to_string(f.base().join("child.pid"))
                .ok()
                .and_then(|text| text.trim().parse::<i32>().ok())
            {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    let pid = tokio::select! {
        result = &mut ensure => panic!("Installer completed before the attaching child was ready: {result:?}"),
        pid = ready => pid,
    };
    assert_eq!(
        rustix::process::test_kill_process(rustix::process::Pid::from_raw(pid).unwrap()),
        Ok(())
    );
    assert!(Instant::now() < deadline - CLEANUP);
    let result = ensure.await;
    assert!(Instant::now() >= deadline - CLEANUP);
    assert_eq!(result.unwrap_err().kind, AppInstallFailure::Deadline);
    assert_eq!(
        rustix::process::test_kill_process(rustix::process::Pid::from_raw(pid).unwrap()),
        Err(rustix::io::Errno::SRCH)
    );
    f.preserved();
    f.unmounted();
}

#[tokio::test]
async fn cancellation_owns_child_and_mount_cleanup_after_caller_drop() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    f.flag("attach-sleep");
    let config = f.config.clone();
    let task = tokio::spawn(async move {
        ensure_with(
            config,
            Version::parse(TARGET).unwrap(),
            AppLaunch::Preserve,
            Instant::now() + Duration::from_secs(60),
        )
        .await
    });
    let pid = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(pid) = fs::read_to_string(f.base().join("child.pid"))
                .ok()
                .and_then(|text| text.trim().parse::<i32>().ok())
            {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(10), async {
        while f.base().join("mounted").exists()
            || fs::read_dir(&f.config.work).unwrap().count() != 1
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        rustix::process::test_kill_process(rustix::process::Pid::from_raw(pid).unwrap()),
        Err(rustix::io::Errno::SRCH)
    );
    f.preserved();
    f.unmounted();
}

#[tokio::test]
async fn expired_deadline_has_no_effects() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    let error = ensure_with(
        f.config.clone(),
        Version::parse(TARGET).unwrap(),
        AppLaunch::Open,
        Instant::now(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind, AppInstallFailure::Deadline);
    assert!(f.requests.lock().unwrap().is_empty());
    assert!(!f.base().join("codesign.log").exists());
    assert!(!f.config.work.exists());
    f.preserved();
}

#[tokio::test]
async fn cancel_active_waits_for_reap_and_detach_even_after_caller_drop() {
    let dropped = Fixture::new(None, DMG.to_vec(), 200).await;
    let awaited = Fixture::new(None, DMG.to_vec(), 200).await;
    let independent = Fixture::new(None, DMG.to_vec(), 200).await;
    for fixture in [&dropped, &awaited, &independent] {
        fixture.old();
        fixture.flag("attach-sleep");
    }
    dropped.flag("detach-block");
    awaited.flag("detach-block");
    let start = |config: Config, registry: Registry| {
        tokio::spawn(async move {
            ensure_registered(
                config,
                Version::parse(TARGET).unwrap(),
                AppLaunch::Preserve,
                Instant::now() + Duration::from_secs(60),
                &registry,
            )
            .await
        })
    };
    let dropped_task = start(dropped.config.clone(), active().clone());
    let awaited_task = start(awaited.config.clone(), active().clone());
    let private_registry = Registry::default();
    let independent_task = start(independent.config.clone(), private_registry.clone());
    let (dropped_pid, awaited_pid, independent_pid) = tokio::join!(
        pid_ready(dropped.base().join("child.pid")),
        pid_ready(awaited.base().join("child.pid")),
        pid_ready(independent.base().join("child.pid")),
    );
    dropped_task.abort();
    assert!(dropped_task.await.unwrap_err().is_cancelled());
    let cancelling = tokio::spawn(cancel_active());
    tokio::join!(
        pid_ready(dropped.base().join("detach.pid")),
        pid_ready(awaited.base().join("detach.pid"))
    );
    assert!(!cancelling.is_finished());
    for pid in [dropped_pid, awaited_pid] {
        assert_eq!(
            rustix::process::test_kill_process(rustix::process::Pid::from_raw(pid).unwrap()),
            Err(rustix::io::Errno::SRCH)
        );
    }
    assert!(dropped.base().join("mounted").exists());
    assert!(awaited.base().join("mounted").exists());
    dropped.flag("detach-release");
    awaited.flag("detach-release");
    cancelling.await.unwrap();
    assert_eq!(
        awaited_task.await.unwrap().unwrap_err().kind,
        AppInstallFailure::Cancelled
    );
    assert!(active().0.lock().unwrap().is_empty());
    for fixture in [&dropped, &awaited] {
        fixture.preserved();
        fixture.unmounted();
    }
    assert!(!independent_task.is_finished());
    assert_eq!(
        rustix::process::test_kill_process(
            rustix::process::Pid::from_raw(independent_pid).unwrap()
        ),
        Ok(())
    );
    private_registry.cancel().await;
    assert_eq!(
        independent_task.await.unwrap().unwrap_err().kind,
        AppInstallFailure::Cancelled
    );
    assert!(private_registry.0.lock().unwrap().is_empty());
    independent.unmounted();
    independent.preserved();
    cancel_active().await;
}

#[tokio::test]
async fn registry_entries_are_removed_after_success_and_failure() {
    let registry = Registry::default();
    let fixture = Fixture::new(None, DMG.to_vec(), 200).await;
    let expired = ensure_registered(
        fixture.config.clone(),
        Version::parse(TARGET).unwrap(),
        AppLaunch::Preserve,
        Instant::now(),
        &registry,
    )
    .await;
    assert_eq!(expired.unwrap_err().kind, AppInstallFailure::Deadline);
    assert!(registry.0.lock().unwrap().is_empty());
    let result = ensure_registered(
        fixture.config.clone(),
        Version::parse(TARGET).unwrap(),
        AppLaunch::Preserve,
        Instant::now() + Duration::from_secs(60),
        &registry,
    )
    .await;
    result.unwrap();
    assert!(registry.0.lock().unwrap().is_empty());
    fixture.unmounted();
    let failed = Fixture::new(None, DMG.to_vec(), 404).await;
    let result = ensure_registered(
        failed.config.clone(),
        Version::parse(TARGET).unwrap(),
        AppLaunch::Preserve,
        Instant::now() + Duration::from_secs(60),
        &registry,
    )
    .await;
    assert_eq!(result.unwrap_err().kind, AppInstallFailure::Download);
    assert!(registry.0.lock().unwrap().is_empty());
    failed.unmounted();
    registry.cancel().await;
}

#[tokio::test]
async fn a_hung_tool_fails_within_its_own_limit_and_names_the_step() {
    let mut f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    f.flag("attach-sleep");
    f.config.limits.tool = Duration::from_secs(1);
    let journal = f.base().join("journal.log");
    let _journaled = journal_to(&journal).await;
    let started = Instant::now();
    let error = f.run().await.unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(error.kind, AppInstallFailure::Stalled);
    assert_eq!(
        error.message,
        "Opening the downloaded disk image did not finish within 1 seconds"
    );
    let log = fs::read_to_string(&journal).unwrap();
    for line in [
        "Opening the downloaded disk image…".to_owned(),
        format!("  $ {}", f.config.hdiutil.display()),
        ": Opening the downloaded disk image did not finish within 1 seconds".to_owned(),
    ] {
        assert!(log.contains(&line), "{line} is missing from {log}");
    }
    f.preserved();
    f.unmounted();
}

#[tokio::test]
async fn an_app_that_does_not_quit_fails_when_the_install_window_ends_and_keeps_running() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    f.flag("running");
    f.flag("quit-hang");
    let started = Instant::now();
    let error = ensure_with(
        f.config.clone(),
        Version::parse(TARGET).unwrap(),
        AppLaunch::Preserve,
        Instant::now() + CLEANUP + Duration::from_secs(8),
    )
    .await
    .unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(25));
    assert_eq!(error.kind, AppInstallFailure::InUse);
    assert_eq!(
        error.message,
        "Koidrive did not quit before the app update ran out of time"
    );
    let installed = format!("{}\n", f.installed().display());
    assert!(
        fs::read_to_string(f.base().join("open.log"))
            .unwrap()
            .contains(&installed)
    );
    assert_eq!(
        fs::read_to_string(f.base().join("started.log")).unwrap(),
        installed
    );
    f.preserved();
    f.unmounted();
}

#[tokio::test]
async fn an_app_that_quits_after_every_step_limit_is_updated_and_reopened() {
    let mut f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    f.flag("running");
    f.config.limits.tool = Duration::from_secs(1);
    script(
        &f.config.osascript,
        r#"
case "$4" in *terminate*) /bin/sleep 3;; esac
echo running
"#,
    );
    let AppInstall::Present { change, launch, .. } = f.run().await.unwrap() else {
        panic!("the app should be installed");
    };
    assert_eq!(change, AppChange::Replaced);
    assert_eq!(launch, AppLaunchResult::Requested);
    assert!(
        fs::read_to_string(f.base().join("open.log"))
            .unwrap()
            .contains(&f.installed().display().to_string())
    );
    f.unmounted();
}

#[tokio::test]
async fn a_failed_tool_names_the_step_and_keeps_its_output_in_the_log() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    f.flag("attach-fail");
    let journal = f.base().join("journal.log");
    let _journaled = journal_to(&journal).await;
    let error = f.run().await.unwrap_err();
    assert_eq!(error.kind, AppInstallFailure::Command);
    assert_eq!(error.message, "Opening the downloaded disk image failed");
    let log = fs::read_to_string(&journal).unwrap();
    for line in [
        "  exit status: 7",
        "  stderr: hdiutil: WARNING: 'hdiutil attach -readonly -nobrowse -mountpoint ...' is deprecated.",
        "  stderr: hdiutil: attach failed - image not recognized",
    ] {
        assert!(log.contains(line), "{line} is missing from {log}");
    }
    f.preserved();
    f.unmounted();
}

#[tokio::test]
async fn an_old_app_that_does_not_start_again_keeps_the_recovery_pending_for_the_next_attempt() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    f.flag("running");
    f.flag("launch-fail");
    fs::set_permissions(f.installed(), fs::Permissions::from_mode(0o555)).unwrap();
    let result = f.run().await;
    fs::set_permissions(f.installed(), fs::Permissions::from_mode(0o755)).unwrap();
    let error = result.unwrap_err();
    assert_eq!(error.kind, AppInstallFailure::Io);
    assert_eq!(
        error.residue.len(),
        1,
        "the recovery stays pending: {error:?}"
    );
    let started = format!("{}\n", f.installed().display());
    assert_eq!(
        fs::read_to_string(f.base().join("started.log")).unwrap(),
        started
    );
    f.preserved();
    fs::remove_file(f.base().join("launch-fail")).unwrap();
    f.flag("running");
    let AppInstall::Present { change, .. } = f.run().await.unwrap() else {
        panic!("the retry should install");
    };
    assert_eq!(change, AppChange::Replaced);
    assert!(
        fs::read_to_string(f.base().join("started.log"))
            .unwrap()
            .starts_with(&started.repeat(2))
    );
    f.unmounted();
}

#[tokio::test]
async fn a_stalled_download_fails_with_how_far_it_got() {
    let mut f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    f.flag("download-stall");
    f.config.limits.stall = Duration::from_secs(1);
    let started = Instant::now();
    let error = f.run().await.unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(error.kind, AppInstallFailure::Download);
    assert!(
        error.message.starts_with(&format!(
            "The download of Koidrive-{TARGET}-arm64.dmg stopped at 0.0 of 0.0 MB: no data arrived for 1 seconds"
        )),
        "{}",
        error.message
    );
    f.preserved();
    f.unmounted();
}

#[tokio::test]
async fn a_held_install_lock_fails_fast_and_says_another_install_holds_it() {
    let mut f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    f.config.limits.lock = Duration::from_secs(1);
    let root = private_work(&f.config.work, true).unwrap();
    let (_tx, cancel) = watch::channel(false);
    let holder = Control {
        deadline: Instant::now() + Duration::from_secs(60),
        cancel,
    };
    let _held = lock(&root, Duration::from_secs(5), &holder).await.unwrap();
    let started = Instant::now();
    let error = f.run().await.unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(error.kind, AppInstallFailure::InUse);
    assert_eq!(
        error.message,
        "Another Koidrive install held the install lock for 1 seconds"
    );
    f.preserved();
}

#[tokio::test]
async fn a_download_that_keeps_moving_is_not_bounded_by_the_install_window() {
    let mut f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    f.flag("download-trickle");
    f.config.limits.stall = Duration::from_secs(2);
    let started = Instant::now();
    let installed = ensure_with(
        f.config.clone(),
        Version::parse(TARGET).unwrap(),
        AppLaunch::Preserve,
        Instant::now() + CLEANUP + Duration::from_secs(8),
    )
    .await
    .unwrap();
    assert!(started.elapsed() > Duration::from_secs(8));
    let AppInstall::Present { change, .. } = installed else {
        panic!("the app should be installed");
    };
    assert_eq!(change, AppChange::Replaced);
    f.unmounted();
}
