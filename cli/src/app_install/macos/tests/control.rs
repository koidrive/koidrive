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
    let guard = lock(&root, &control).await.unwrap();
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
