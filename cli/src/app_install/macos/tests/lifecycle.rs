use super::*;

struct Probe {
    dir: tempfile::TempDir,
    app: PathBuf,
    config: Config,
    control: Control,
    _cancel: watch::Sender<bool>,
}

impl Probe {
    async fn launch(script: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join(BUNDLE);
        let run = |program: &str, arguments: &[&OsStr]| {
            let output = std::process::Command::new(program)
                .args(arguments)
                .output()
                .unwrap();
            assert!(output.status.success(), "{program}: {output:?}");
        };
        let mut compile = vec![OsStr::new("-s"), OsStr::new("-o"), app.as_os_str()];
        for line in script {
            compile.extend([OsStr::new("-e"), OsStr::new(line)]);
        }
        run("/usr/bin/osacompile", &compile);
        let plist = app.join("Contents/Info.plist");
        for entry in [
            format!("Add :CFBundleIdentifier string {IDENTIFIER}"),
            "Add :LSUIElement bool true".to_owned(),
        ] {
            run(
                "/usr/libexec/PlistBuddy",
                &[OsStr::new("-c"), OsStr::new(&entry), plist.as_os_str()],
            );
        }
        run(
            "/usr/bin/codesign",
            &[
                OsStr::new("-s"),
                OsStr::new("-"),
                OsStr::new("--force"),
                app.as_os_str(),
            ],
        );
        let tools = dir.path().join("tools");
        let config = Config {
            system: dir.path().join("system"),
            user: dir.path().join("user"),
            work: dir.path().join("work"),
            release: "http://127.0.0.1:9/releases/download".into(),
            codesign: tools.join("codesign"),
            spctl: tools.join("spctl"),
            hdiutil: tools.join("hdiutil"),
            ditto: "/usr/bin/ditto".into(),
            osascript: "/usr/bin/osascript".into(),
            open: "/usr/bin/open".into(),
            updater: dir.path().join("updater"),
            max_download: 1024,
            limits: Limits::default(),
        };
        let (cancel_tx, cancel) = watch::channel(false);
        let control = Control {
            deadline: Instant::now() + Duration::from_secs(60),
            cancel,
        };
        let probe = Self {
            dir,
            app,
            config,
            control,
            _cancel: cancel_tx,
        };
        run("/usr/bin/open", &[OsStr::new("-n"), probe.app.as_os_str()]);
        let launched = Instant::now() + Duration::from_secs(20);
        while probe.inspect().await != b"running" {
            assert!(Instant::now() < launched, "the probe app never ran");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        probe
    }

    async fn inspect(&self) -> Vec<u8> {
        process(
            "Checking whether the probe app is running",
            Duration::from_secs(20),
            &self.config.osascript,
            &[
                Arg::from("-l"),
                Arg::from("JavaScript"),
                Arg::from("-e"),
                Arg::from(INSPECT_RUNNING),
                passed(&self.app).unwrap(),
            ],
            &self.control,
        )
        .await
        .unwrap()
        .stdout
        .trim_ascii()
        .to_vec()
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        let _ = std::process::Command::new("/usr/bin/pkill")
            .args(["-9", "-f"])
            .arg(self.dir.path().join(BUNDLE))
            .status();
    }
}

#[tokio::test]
async fn quiescing_quits_the_app_running_from_the_bundle_and_says_it_was_running() {
    let probe = Probe::launch(&["on idle", "return 30", "end idle"]).await;

    assert!(
        quiesce(&probe.config, &[&probe.app], &probe.control)
            .await
            .unwrap()
    );
    assert_eq!(probe.inspect().await, b"stopped");
    assert!(
        !quiesce(&probe.config, &[&probe.app], &probe.control)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn an_app_that_ignores_the_quit_request_fails_quiescing_within_the_quit_limit() {
    let mut probe = Probe::launch(&[
        "on idle",
        "return 30",
        "end idle",
        "on quit",
        "return",
        "end quit",
    ])
    .await;
    probe.config.limits.quit = Duration::from_secs(2);
    let started = Instant::now();

    let error = quiesce(&probe.config, &[&probe.app], &probe.control)
        .await
        .unwrap_err();

    assert!(started.elapsed() < Duration::from_secs(15));
    assert_eq!(error.kind, AppInstallFailure::InUse);
    assert_eq!(
        error.message,
        "Koidrive did not quit within 2 seconds of being asked to"
    );
    assert_eq!(probe.inspect().await, b"running");
}

#[tokio::test]
async fn lifecycle_failed_swap_restores_only_previously_running_ui() {
    for running in [true, false] {
        let f = Fixture::new(None, DMG.to_vec(), 200).await;
        f.old();
        if running {
            f.flag("running");
        }
        fs::set_permissions(f.installed(), fs::Permissions::from_mode(0o555)).unwrap();
        let result = f.run().await;
        fs::set_permissions(f.installed(), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(result.unwrap_err().kind, AppInstallFailure::Io);
        f.preserved();
        f.unmounted();
        assert_eq!(f.base().join("open.log").exists(), running);
    }
}

#[tokio::test]
async fn failed_swap_restores_running_ui_even_when_detach_also_fails() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    f.flag("running");
    f.flag("detach-fail");
    fs::set_permissions(f.installed(), fs::Permissions::from_mode(0o555)).unwrap();
    let result = f.run().await;
    fs::set_permissions(f.installed(), fs::Permissions::from_mode(0o755)).unwrap();
    let error = result.unwrap_err();
    assert_eq!(error.kind, AppInstallFailure::Io);
    assert!(!error.residue.is_empty());
    assert!(error.committed.is_empty());
    f.preserved();
    assert!(f.base().join("mounted").exists());
    assert!(
        f.base().join("open.log").exists(),
        "The running UI must be restored independently of mount cleanup"
    );
    fs::remove_file(f.base().join("detach-fail")).unwrap();
    let AppInstall::Present {
        retired, residue, ..
    } = f.run().await.unwrap()
    else {
        panic!()
    };
    assert!(residue.is_empty());
    assert_eq!(retired.len(), 1);
    retired[0].discard().unwrap();
    f.unmounted();
}

#[tokio::test]
async fn lifecycle_post_swap_foreign_bundle_is_never_opened_or_returned() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    let foreign = f.base().join("foreign.app");
    bundle(&foreign, Some(TARGET));
    fs::write(foreign.join("Signer"), "OTHERTEAM").unwrap();
    script(
        &foreign.join("Contents/Helpers/koi"),
        &format!("echo ran > '{}/foreign-helper-ran'", f.base().display()),
    );
    script(
        &f.config.osascript,
        &format!(
            r#"
base='{}'
printf '%s\n' invoked >> "$base/quit.log"
count=$(/usr/bin/wc -l < "$base/quit.log")
if [ "$count" -eq 2 ]; then
    /bin/mv "$base/system/Koidrive.app" "$base/verified-displaced.app"
    /usr/bin/ditto "$base/foreign.app" "$base/system/Koidrive.app"
fi
echo stopped
"#,
            f.base().display()
        ),
    );
    let result = ensure_with(
        f.config.clone(),
        Version::parse(TARGET).unwrap(),
        AppLaunch::Open,
        Instant::now() + Duration::from_secs(60),
    )
    .await;
    let error = result.unwrap_err();
    assert!(matches!(
        error.kind,
        AppInstallFailure::ConcurrentChange | AppInstallFailure::Verification
    ));
    assert!(!error.residue.is_empty());
    assert!(!f.base().join("open.log").exists());
    assert!(!f.base().join("foreign-helper-ran").exists());
    assert_eq!(
        error.committed,
        vec![CommitFact {
            path: f.installed(),
            version: Version::parse(TARGET).unwrap(),
            change: CommitChange::Replaced
        }]
    );
    assert_eq!(
        fs::read(f.installed().join("Signer")).unwrap(),
        b"OTHERTEAM"
    );
}

#[tokio::test]
async fn reopening_failure_replays_before_a_clean_unchanged_result() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.flag("open-fail");
    let AppInstall::Present {
        launch, residue, ..
    } = ensure_with(
        f.config.clone(),
        Version::parse(TARGET).unwrap(),
        AppLaunch::Open,
        Instant::now() + Duration::from_secs(60),
    )
    .await
    .unwrap()
    else {
        panic!()
    };
    assert!(matches!(launch, AppLaunchResult::Failed(_)));
    assert!(!residue.is_empty());
    fs::remove_file(f.base().join("open-fail")).unwrap();
    let before = fs::read_to_string(f.base().join("open.log"))
        .unwrap()
        .lines()
        .count();
    let AppInstall::Present {
        change,
        retired,
        residue,
        ..
    } = f.run().await.unwrap()
    else {
        panic!()
    };
    assert_eq!(change, AppChange::Unchanged);
    assert!(retired.is_empty());
    assert!(residue.is_empty());
    assert!(
        fs::read_to_string(f.base().join("open.log"))
            .unwrap()
            .lines()
            .count()
            > before
    );
    f.unmounted();
    assert_eq!(fs::read_dir(&f.config.work).unwrap().count(), 1);
}

#[tokio::test]
async fn replacement_retains_old_helper_and_preserve_restarts_running_ui() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    f.flag("running");
    let result = f.run().await.unwrap();
    let AppInstall::Present {
        change,
        retired,
        launch,
        residue,
        ..
    } = result
    else {
        panic!("{result:?}");
    };
    let old = retired.into_iter().next().unwrap();
    assert_eq!(change, AppChange::Replaced);
    assert_eq!(launch, AppLaunchResult::Requested);
    assert!(residue.is_empty());
    assert_eq!(old.version, Some(Version::parse("0.1.0").unwrap()));
    assert_eq!(
        fs::read(old.bundle.join("Contents/Helpers/koi")).unwrap(),
        b"0.1.0"
    );
    assert_eq!(
        fs::read(f.installed().join("Contents/Helpers/koi")).unwrap(),
        TARGET.as_bytes()
    );
    f.unmounted();
}

#[tokio::test]
async fn open_failure_is_not_an_install_failure() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.flag("open-fail");
    let result = ensure_with(
        f.config.clone(),
        Version::parse(TARGET).unwrap(),
        AppLaunch::Open,
        Instant::now() + Duration::from_secs(60),
    )
    .await
    .unwrap();
    assert!(matches!(
        result,
        AppInstall::Present {
            change: AppChange::Installed,
            launch: AppLaunchResult::Failed(_),
            ..
        }
    ));
    f.unmounted();
}

#[tokio::test]
async fn refused_quit_preserves_old_app_and_cleans_mount() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    f.flag("quit-fail");
    assert_eq!(f.run().await.unwrap_err().kind, AppInstallFailure::InUse);
    f.preserved();
    f.unmounted();
}

#[tokio::test]
async fn launch_racing_swap_keeps_old_helper_and_requests_preserving_restart() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    script(
        &f.config.osascript,
        &format!(
            r#"
base='{}'
printf '%s\n' invoked >> "$base/quit.log"
count=$(/usr/bin/wc -l < "$base/quit.log")
if [ "$count" -eq 2 ]; then echo running; else echo stopped; fi
"#,
            f.base().display()
        ),
    );
    let AppInstall::Present {
        retired, launch, ..
    } = f.run().await.unwrap()
    else {
        panic!()
    };
    let old = retired.into_iter().next().unwrap();
    assert_eq!(launch, AppLaunchResult::Requested);
    assert_eq!(
        fs::read(old.bundle.join("Contents/Helpers/koi")).unwrap(),
        b"0.1.0"
    );
    f.unmounted();
}

#[tokio::test]
async fn failed_postcommit_quit_reports_installation_and_retains_old_helper() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    script(
        &f.config.osascript,
        &format!(
            r#"
base='{}'
printf '%s\n' invoked >> "$base/quit.log"
count=$(/usr/bin/wc -l < "$base/quit.log")
[ "$count" -ne 2 ] || exit 8
echo stopped
"#,
            f.base().display()
        ),
    );
    let AppInstall::Present {
        change,
        retired,
        launch,
        ..
    } = f.run().await.unwrap()
    else {
        panic!()
    };
    let old = retired.into_iter().next().unwrap();
    assert_eq!(change, AppChange::Replaced);
    assert!(matches!(launch, AppLaunchResult::Failed(_)));
    assert_eq!(
        fs::read(old.bundle.join("Contents/Helpers/koi")).unwrap(),
        b"0.1.0"
    );
    assert_eq!(
        fs::read(f.installed().join("Contents/Helpers/koi")).unwrap(),
        TARGET.as_bytes()
    );
    assert!(!f.base().join("open.log").exists());
    f.unmounted();
}
