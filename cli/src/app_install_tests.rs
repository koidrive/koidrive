// SPDX-License-Identifier: Apache-2.0

#[tokio::test]
async fn quiescing_quits_the_app_running_from_the_bundle_and_says_it_was_running() {
    let dir = tempfile::tempdir().unwrap();
    let app = dir.path().join(BUNDLE);
    let run = |program: &str, arguments: &[&OsStr]| {
        let output = std::process::Command::new(program)
            .args(arguments)
            .output()
            .unwrap();
        assert!(output.status.success(), "{program}: {output:?}");
    };
    run(
        "/usr/bin/osacompile",
        &[
            OsStr::new("-s"),
            OsStr::new("-o"),
            app.as_os_str(),
            OsStr::new("-e"),
            OsStr::new("on idle"),
            OsStr::new("-e"),
            OsStr::new("return 30"),
            OsStr::new("-e"),
            OsStr::new("end idle"),
        ],
    );
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
    };
    let (_tx, cancel) = watch::channel(false);
    let control = Control {
        deadline: Instant::now() + Duration::from_secs(60),
        cancel,
    };
    let inspect = async || {
        process(
            &config.osascript,
            &args(&[
                OsStr::new("-l"),
                OsStr::new("JavaScript"),
                OsStr::new("-e"),
                OsStr::new(INSPECT_RUNNING),
                app.as_os_str(),
            ]),
            &control,
        )
        .await
        .unwrap()
    };
    run("/usr/bin/open", &[OsStr::new("-n"), app.as_os_str()]);
    let launched = Instant::now() + Duration::from_secs(20);
    while inspect().await.trim_ascii() != b"running" {
        assert!(Instant::now() < launched, "the probe app never ran");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert!(quiesce(&config, &[&app], &control).await.unwrap());
    assert_eq!(inspect().await.trim_ascii(), b"stopped");
    assert!(!quiesce(&config, &[&app], &control).await.unwrap());
}

#[tokio::test]
async fn malformed_receipt_version_is_rejected_before_namespace_replacement() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    let work = private_work(&f.config.work, true).unwrap();
    let root = directory(&f.config.system, false).unwrap();
    let receipt = begin_receipt(&f.config, &work, &Version::parse(TARGET).unwrap()).unwrap();
    let (_tx, cancel) = watch::channel(false);
    let control = Control {
        deadline: Instant::now() + Duration::from_secs(30),
        cancel,
    };
    let staged = stage_copy(
        &f.config,
        &receipt,
        &f.config.system,
        &root,
        &f.incoming(),
        &control,
    )
    .await
    .unwrap();
    let installed = inventory(&f.config, &control).await.unwrap();
    let mut data = receipt.data();
    data.target = "not-a-version".into();
    receipt.save(&data).unwrap();
    let mut attempt = Attempt::default();
    let error = exchange(
        &f.config,
        &receipt,
        staged,
        &installed,
        false,
        &control,
        &mut attempt,
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind, AppInstallFailure::Io);
    assert!(attempt.committed.is_empty());
    f.preserved();
    assert!(!f.base().join("quit.log").exists());
    assert!(!f.base().join("open.log").exists());
}

#[tokio::test]
async fn primary_commit_facts_survive_a_failed_alternative_upgrade() {
    let mut f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    bundle(&f.config.user.join(BUNDLE), Some("0.1.0"));
    f.config.ditto = f.base().join("fail-alternative-copy");
    script(
        &f.config.ditto,
        &format!(
            "[ \"$1\" != '{}' ] || exit 73\nexec /usr/bin/ditto \"$@\"",
            f.installed().display()
        ),
    );
    let error = f.run().await.unwrap_err();
    assert_eq!(error.kind, AppInstallFailure::Command);
    assert_eq!(
        fs::read(f.installed().join("Contents/Helpers/koi")).unwrap(),
        TARGET.as_bytes()
    );
    assert_eq!(
        error.committed,
        vec![CommitFact {
            path: f.installed(),
            version: Version::parse(TARGET).unwrap(),
            change: CommitChange::Replaced,
        }]
    );
    assert_eq!(error.retired.len(), 1);
    assert_eq!(
        fs::read(error.retired[0].join("Contents/Helpers/koi")).unwrap(),
        b"0.1.0"
    );
    assert!(
        error
            .residue
            .iter()
            .any(|path| path.extension().is_some_and(|ext| ext == "json") && path.exists())
    );
    let prepared = crate::app::Prepared::from_install(Err(error));
    assert!(prepared.helper().is_none());
    let retry = {
        f.config.ditto = "/usr/bin/ditto".into();
        f.run().await.unwrap()
    };
    assert!(
        matches!(retry, AppInstall::Present { committed, .. } if committed.len() == 1 && committed[0].path == f.config.user.join(BUNDLE))
    );
}

#[tokio::test]
async fn alternative_commit_survives_a_later_verification_failure() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    let user = f.config.user.join(BUNDLE);
    bundle(&user, Some("0.4.0"));
    let accepting = f.base().join("accepting-codesign");
    fs::copy(&f.config.codesign, &accepting).unwrap();
    script(
        &f.config.codesign,
        &format!(
            "'{}' \"$@\" || exit $?\nfor last do :; done\nif [ \"$last\" = '{}' ] && [ \"$(/bin/cat '{}/Contents/Helpers/koi')\" = '0.4.0' ]; then printf OTHERTEAM > '{}/Signer'; fi",
            accepting.display(),
            user.display(),
            f.installed().display(),
            user.display()
        ),
    );
    let error = f.run().await.unwrap_err();
    assert_eq!(
        error.committed,
        vec![CommitFact {
            path: f.installed(),
            version: Version::parse("0.4.0").unwrap(),
            change: CommitChange::Replaced
        }]
    );
    assert!(!error.residue.is_empty());
    assert!(!f.base().join("open.log").exists());
    let prepared = crate::app::Prepared::from_install(Err(error));
    assert!(prepared.helper().is_none());
}

#[tokio::test]
async fn refresh_and_final_verification_errors_preserve_attempt_effects() {
    for refresh in [true, false] {
        let f = Fixture::new(None, DMG.to_vec(), 200).await;
        f.old();
        let AppInstall::Present {
            committed, retired, ..
        } = f.run().await.unwrap()
        else {
            panic!()
        };
        let expected = committed.clone();
        let mut attempt = Attempt {
            committed,
            ..Default::default()
        };
        for token in &retired {
            attempt.retain(token);
        }
        let (_tx, cancel) = watch::channel(false);
        let control = Control {
            deadline: Instant::now() + Duration::from_secs(30),
            cancel,
        };
        let installed = inventory(&f.config, &control).await.unwrap();
        let current = current(&installed, &Version::parse(TARGET).unwrap()).unwrap();
        fs::write(current.path.join("Signer"), "OTHERTEAM").unwrap();
        let error = if refresh {
            refresh_tokens(retired, &f.config, &control)
                .await
                .unwrap_err()
        } else {
            unchanged_bundle(&f.config, current, &control)
                .await
                .unwrap_err()
        };
        let error = attempt.fail(error);
        assert_eq!(error.committed, expected);
        assert_eq!(error.retired.len(), 1);
        assert_eq!(
            fs::read(error.retired[0].join("Contents/Helpers/koi")).unwrap(),
            b"0.1.0"
        );
        assert!(
            error
                .residue
                .iter()
                .any(|path| path.extension().is_some_and(|ext| ext == "json") && path.exists())
        );
        assert!(
            crate::app::Prepared::from_install(Err(error))
                .helper()
                .is_none()
        );
    }
}

#[tokio::test]
async fn verifying_an_existing_helper_is_read_only_even_with_pending_work() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    fs::create_dir(&f.config.work).unwrap();
    let receipt = f.config.work.join("receipt-pending.json");
    fs::write(&receipt, "must not recover or remove this receipt").unwrap();
    let helper = f.installed().join("Contents/Helpers/koi");
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
    let verified = verify_helper(
        f.config.clone(),
        &helper,
        Instant::now() + Duration::from_secs(30),
    )
    .await
    .unwrap();
    assert_eq!(verified.version().unwrap().to_string(), "0.1.0");
    verified.require_version("0.1.0").unwrap();
    assert_eq!(
        verified.require_version(TARGET).unwrap_err().kind,
        AppInstallFailure::ConcurrentChange
    );
    assert_eq!(
        fs::read(&receipt).unwrap(),
        b"must not recover or remove this receipt"
    );
    assert_eq!(fs::read_dir(&f.config.work).unwrap().count(), 1);
    for log in ["open.log", "quit.log", "hdi.log"] {
        assert!(!f.base().join(log).exists());
    }
    assert!(f.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn review_retry_after_detach_failure_must_finish_cleanup() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.flag("detach-fail");
    let first = f.run().await.unwrap();
    assert!(matches!(&first, AppInstall::Present { residue, .. } if !residue.is_empty()));
    assert!(f.base().join("mounted").exists());
    fs::remove_file(f.base().join("detach-fail")).unwrap();
    let retry = f.run().await.unwrap();
    assert!(
        matches!(retry, AppInstall::Present { change: AppChange::Unchanged, retired, residue, .. } if retired.is_empty() && residue.is_empty())
    );
    assert!(
        !f.base().join("mounted").exists(),
        "The retry reported a clean unchanged app but left its previous DMG mounted"
    );
}

#[tokio::test]
async fn review_retry_after_handoff_interruption_must_recover_retired() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    let first = f.run().await.unwrap();
    let AppInstall::Present { retired, .. } = first else {
        panic!("replacement required")
    };
    let old = retired.into_iter().next().unwrap();
    let old_path = old.bundle.clone();
    drop(old);
    let retry = f.run().await.unwrap();
    assert_eq!(
        fs::read(old_path.join("Contents/Helpers/koi")).unwrap(),
        b"0.1.0"
    );
    let AppInstall::Present {
        retired,
        residue,
        committed,
        ..
    } = retry
    else {
        panic!()
    };
    assert!(residue.is_empty());
    assert!(committed.is_empty());
    assert_eq!(retired.len(), 1);
    assert_eq!(retired[0].bundle, old_path);
    retired[0].discard().unwrap();
    assert!(!old_path.exists());
    f.unmounted();
}

#[tokio::test]
async fn review_killed_transaction_child() {
    let Some(base) = std::env::var_os("KOI_REVIEW_KILL_BASE").map(PathBuf::from) else {
        return;
    };
    let config = Config {
        system: base.join("system"),
        user: base.join("user"),
        work: base.join("work"),
        release: std::env::var("KOI_REVIEW_KILL_RELEASE").unwrap(),
        codesign: base.join("tools/codesign"),
        spctl: base.join("tools/spctl"),
        hdiutil: base.join("tools/hdiutil"),
        ditto: "/usr/bin/ditto".into(),
        osascript: base.join("tools/osascript"),
        open: base.join("tools/open"),
        updater: base.join("updater"),
        max_download: 1024,
    };
    ensure_with(
        config,
        Version::parse(TARGET).unwrap(),
        AppLaunch::Preserve,
        Instant::now() + Duration::from_secs(120),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn review_retry_after_sigkill_must_report_pending_transaction() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    script(
        &f.config.osascript,
        &format!(
            r#"
base='{}'
printf '%s\n' invoked >> "$base/quit.log"
count=$(/usr/bin/wc -l < "$base/quit.log")
if [ "$count" -eq 2 ]; then echo $$ > "$base/crash-ready"; exec /bin/sleep 120; fi
echo stopped
"#,
            f.base().display()
        ),
    );
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "app_install::macos::tests::review_killed_transaction_child",
            "--nocapture",
        ])
        .env("KOI_REVIEW_KILL_BASE", f.base())
        .env("KOI_REVIEW_KILL_RELEASE", &f.config.release)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let script_pid = pid_ready(f.base().join("crash-ready")).await;
    child.kill().await.unwrap();
    let _ = rustix::process::kill_process(
        rustix::process::Pid::from_raw(script_pid).unwrap(),
        rustix::process::Signal::KILL,
    );
    assert!(f.base().join("mounted").exists());
    assert_eq!(
        fs::read(f.installed().join("Contents/Helpers/koi")).unwrap(),
        TARGET.as_bytes()
    );
    let retry = f.run().await.unwrap();
    let AppInstall::Present {
        retired,
        residue,
        change,
        ..
    } = retry
    else {
        panic!()
    };
    assert_eq!(change, AppChange::Unchanged);
    assert!(residue.is_empty());
    assert_eq!(retired.len(), 1);
    assert_eq!(
        fs::read(retired[0].bundle.join("Contents/Helpers/koi")).unwrap(),
        b"0.1.0"
    );
    f.unmounted();
    retired[0].discard().unwrap();
    f.unmounted();
}

#[tokio::test]
async fn review_signed_development_copy_without_release_version_is_upgraded() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    bundle(&f.installed(), None);
    let AppInstall::Present {
        change,
        version,
        retired,
        residue,
        ..
    } = f.run().await.unwrap()
    else {
        panic!()
    };
    assert_eq!(change, AppChange::Replaced);
    assert_eq!(version, Version::parse(TARGET).unwrap());
    assert!(residue.is_empty());
    assert_eq!(retired.len(), 1);
    assert_eq!(retired[0].version, None);
    assert_eq!(
        fs::read(retired[0].bundle.join("Contents/Helpers/koi")).unwrap(),
        b"development"
    );
    let verification = fs::read_to_string(f.base().join("codesign.log")).unwrap();
    assert!(verification.contains(REQUIREMENT));
    assert_eq!(
        fs::read(f.installed().join("Contents/Helpers/koi")).unwrap(),
        TARGET.as_bytes()
    );
    retired[0].discard().unwrap();
    f.unmounted();
}

use super::*;

#[tokio::test]
async fn native_codesign_accepts_inline_pins_in_bundle_and_updater_modes() {
    let executable = std::env::current_exe().unwrap();
    let mut magic = [0; 4];
    File::open(&executable)
        .unwrap()
        .read_exact(&mut magic)
        .unwrap();
    assert_eq!(magic, [0xcf, 0xfa, 0xed, 0xfe]);
    let (_tx, cancel) = watch::channel(false);
    let control = Control {
        deadline: Instant::now() + Duration::from_secs(20),
        cancel,
    };
    let codesign = Path::new("/usr/bin/codesign");
    let display = process(
        codesign,
        &args(&[
            OsStr::new("--display"),
            OsStr::new("--verbose=4"),
            executable.as_os_str(),
        ]),
        &control,
    )
    .await
    .unwrap();
    let display = String::from_utf8(display).unwrap();
    let hashes: Vec<_> = display
        .lines()
        .filter_map(|line| line.strip_prefix("CDHash="))
        .collect();
    assert_eq!(hashes.len(), 1, "{display}");
    let requirement = format!("cdhash H\"{}\"", hashes[0]);
    for deep in [true, false] {
        process(
            codesign,
            &signature_args(&executable, &requirement, deep),
            &control,
        )
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn native_codesign_parses_production_requirements_and_rejects_the_wrong_signer() {
    let executable = std::env::current_exe().unwrap();
    let (_tx, cancel) = watch::channel(false);
    let control = Control {
        deadline: Instant::now() + Duration::from_secs(20),
        cancel,
    };
    for (requirement, deep) in [(REQUIREMENT, true), (SIGNER, false)] {
        let error = process(
            Path::new("/usr/bin/codesign"),
            &signature_args(&executable, requirement, deep),
            &control,
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind, AppInstallFailure::Command);
        assert!(error.message.contains("failed to satisfy"), "{error:?}");
        let error = verification_error(error, "signature rejected");
        assert_eq!(error.kind, AppInstallFailure::Verification);
    }
}

struct ForgedDeveloperId {
    temp: tempfile::TempDir,
    search_list: Option<Vec<String>>,
}

impl ForgedDeveloperId {
    const IDENTITY: &str = "Developer ID Application: Gunwoo Kim (656SPP5XR8)";
    const PASSWORD: &str = "forged";

    fn new() -> Self {
        let temp = tempfile::Builder::new()
            .prefix("koi-forged-developer-id-")
            .tempdir()
            .unwrap();
        let dir = temp.path().to_path_buf();
        fs::write(
            dir.join("leaf.cnf"),
            format!(
                "[req]\ndistinguished_name=dn\nprompt=no\n[dn]\nCN={}\nOU=656SPP5XR8\nO=Gunwoo Kim\nC=US\n[leaf]\nbasicConstraints=critical,CA:false\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=critical,codeSigning\n1.2.840.113635.100.6.1.13=critical,DER:0500\n",
                Self::IDENTITY
            ),
        )
        .unwrap();
        let mut forged = Self {
            temp,
            search_list: None,
        };
        let keychain = forged.keychain();
        succeed(
            std::process::Command::new("/usr/bin/openssl")
                .current_dir(&dir)
                .args([
                    "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1", "-config",
                    "leaf.cnf", "-extensions", "leaf", "-keyout", "key.pem", "-out", "cert.pem",
                ]),
        );
        succeed(
            std::process::Command::new("/usr/bin/openssl")
                .current_dir(&dir)
                .args([
                    "pkcs12", "-export", "-inkey", "key.pem", "-in", "cert.pem", "-out",
                    "identity.p12", "-passout",
                ])
                .arg(format!("pass:{}", Self::PASSWORD)),
        );
        for args in [
            vec!["create-keychain", "-p", Self::PASSWORD],
            vec!["unlock-keychain", "-p", Self::PASSWORD],
        ] {
            succeed(
                std::process::Command::new("/usr/bin/security")
                    .args(args)
                    .arg(&keychain),
            );
        }
        succeed(
            std::process::Command::new("/usr/bin/security")
                .arg("import")
                .arg(dir.join("identity.p12"))
                .arg("-k")
                .arg(&keychain)
                .args(["-f", "pkcs12", "-P", Self::PASSWORD, "-T", "/usr/bin/codesign"]),
        );
        succeed(
            std::process::Command::new("/usr/bin/security")
                .args([
                    "set-key-partition-list",
                    "-S",
                    "apple-tool:,apple:,codesign:",
                    "-s",
                    "-k",
                    Self::PASSWORD,
                ])
                .arg(&keychain),
        );
        if std::env::var_os("RUNNER_ENVIRONMENT").as_deref() == Some(OsStr::new("github-hosted")) {
            let listed = std::process::Command::new("/usr/bin/security")
                .args(["list-keychains", "-d", "user"])
                .output()
                .unwrap();
            assert!(listed.status.success());
            let listed: Vec<String> = String::from_utf8(listed.stdout)
                .unwrap()
                .lines()
                .map(|line| line.trim().trim_matches('"').to_owned())
                .filter(|line| !line.is_empty())
                .collect();
            succeed(
                std::process::Command::new("/usr/bin/security")
                    .args(["list-keychains", "-d", "user", "-s"])
                    .arg(&keychain)
                    .args(&listed),
            );
            forged.search_list = Some(listed);
        }
        forged
    }

    fn keychain(&self) -> PathBuf {
        self.temp.path().join("forged.keychain-db")
    }

    fn sign(&self, path: &Path) {
        succeed(
            std::process::Command::new("/usr/bin/codesign")
                .args(["--force", "--timestamp=none", "--keychain"])
                .arg(self.keychain())
                .args(["--sign", Self::IDENTITY])
                .arg(path),
        );
    }

    fn app(&self) -> PathBuf {
        let app = self.temp.path().join(BUNDLE);
        for (directory, file) in [("MacOS", "Koidrive"), ("Helpers", "koi")] {
            fs::create_dir_all(app.join("Contents").join(directory)).unwrap();
            fs::copy("/usr/bin/true", app.join("Contents").join(directory).join(file)).unwrap();
        }
        let mut info = plist::Dictionary::new();
        info.insert(
            "CFBundleIdentifier".into(),
            plist::Value::String(IDENTIFIER.into()),
        );
        info.insert(
            "CFBundleExecutable".into(),
            plist::Value::String("Koidrive".into()),
        );
        plist::Value::Dictionary(info)
            .to_file_xml(app.join("Contents/Info.plist"))
            .unwrap();
        self.sign(&app.join("Contents/Helpers/koi"));
        self.sign(&app);
        app
    }
}

impl Drop for ForgedDeveloperId {
    fn drop(&mut self) {
        if let Some(listed) = &self.search_list {
            let _ = std::process::Command::new("/usr/bin/security")
                .args(["list-keychains", "-d", "user", "-s"])
                .args(listed)
                .output();
        }
        let _ = std::process::Command::new("/usr/bin/security")
            .arg("delete-keychain")
            .arg(self.keychain())
            .output();
    }
}

fn succeed(command: &mut std::process::Command) {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{command:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
#[ignore = "creates a throwaway signing keychain; CI runs it on GitHub-hosted macOS"]
async fn native_codesign_refuses_an_app_and_updater_signed_by_a_forged_developer_id() {
    let forged = ForgedDeveloperId::new();
    let app = forged.app();
    let helper = app.join("Contents/Helpers/koi");
    let config = Config::production().unwrap();
    let (_tx, cancel) = watch::channel(false);
    let control = Control {
        deadline: Instant::now() + Duration::from_secs(30),
        cancel,
    };
    let claimed = "certificate leaf[subject.OU] = \"656SPP5XR8\" and certificate leaf[field.1.2.840.113635.100.6.1.13] exists";
    for (path, deep) in [(&app, true), (&helper, false)] {
        process(
            &config.codesign,
            &signature_args(path, claimed, deep),
            &control,
        )
        .await
        .unwrap();
    }
    let Err(error) = verify(&config, &app, None, &control).await else {
        panic!("a forged Developer ID app was accepted");
    };
    assert_eq!(error.kind, AppInstallFailure::Verification);
    assert!(error.message.contains("failed to satisfy"), "{error:?}");
    let error = process(
        &config.codesign,
        &signature_args(&helper, SIGNER, false),
        &control,
    )
    .await
    .unwrap_err();
    assert!(error.message.contains("failed to satisfy"), "{error:?}");
}


#[tokio::test]
async fn native_codesign_refuses_an_app_and_updater_signed_by_another_identity() {
    let app = Path::new("/System/Applications/Calculator.app");
    let helper = Path::new("/usr/bin/true");
    let config = Config::production().unwrap();
    let (_tx, cancel) = watch::channel(false);
    let control = Control {
        deadline: Instant::now() + Duration::from_secs(30),
        cancel,
    };
    for (path, deep) in [(app, true), (helper, false)] {
        process(
            &config.codesign,
            &signature_args(path, "anchor apple", deep),
            &control,
        )
        .await
        .unwrap();
    }
    let Err(error) = verify(&config, app, None, &control).await else {
        panic!("an app signed by another identity was accepted");
    };
    assert_eq!(error.kind, AppInstallFailure::Verification);
    assert!(error.message.contains("failed to satisfy"), "{error:?}");
    let error = process(
        &config.codesign,
        &signature_args(helper, SIGNER, false),
        &control,
    )
    .await
    .unwrap_err();
    assert!(error.message.contains("failed to satisfy"), "{error:?}");
}

#[tokio::test]
async fn review_readonly_old_system_copy_allows_user_fallback() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    fs::set_permissions(&f.config.system, fs::Permissions::from_mode(0o500)).unwrap();
    let result = f.run().await;
    fs::set_permissions(&f.config.system, fs::Permissions::from_mode(0o700)).unwrap();
    f.preserved();
    assert_eq!(
        fs::read(f.config.user.join(BUNDLE).join("Contents/Helpers/koi")).unwrap(),
        TARGET.as_bytes()
    );
    assert!(
        matches!(result, Ok(AppInstall::Present { .. })),
        "The user copy installed, but the result was {result:?}"
    );
}

#[tokio::test]
async fn readonly_verified_system_alternatives_are_advisory_for_installs_and_reuse() {
    for reuse in [false, true] {
        let f = Fixture::new(None, DMG.to_vec(), 200).await;
        f.old();
        let old_identity = observed(&f.installed()).unwrap();
        let user = f.config.user.join(BUNDLE);
        if reuse {
            bundle(&user, Some(TARGET));
        }
        let profile_helper = f.base().join("another-profile-helper");
        std::os::unix::fs::symlink(f.installed().join("Contents/Helpers/koi"), &profile_helper)
            .unwrap();
        fs::set_permissions(&f.config.system, fs::Permissions::from_mode(0o500)).unwrap();
        let result = f.run().await;
        fs::set_permissions(&f.config.system, fs::Permissions::from_mode(0o700)).unwrap();
        let AppInstall::Present {
            change,
            path,
            version,
            preserved,
            retired,
            residue,
            ..
        } = result.unwrap()
        else {
            panic!()
        };
        assert_eq!(
            change,
            if reuse {
                AppChange::Unchanged
            } else {
                AppChange::Installed
            }
        );
        assert_eq!(path, user);
        assert_eq!(version, Version::parse(TARGET).unwrap());
        assert_eq!(preserved, vec![f.installed()]);
        assert!(retired.is_empty());
        assert!(residue.is_empty());
        assert_eq!(observed(&f.installed()).unwrap(), old_identity);
        assert_eq!(fs::read(profile_helper).unwrap(), b"0.1.0");
        assert_eq!(
            fs::read(user.join("Contents/Helpers/koi")).unwrap(),
            TARGET.as_bytes()
        );
        assert!(!f.config.system.join(".koidrive-app-install.lock").exists());
        assert_eq!(f.requests.lock().unwrap().is_empty(), reuse);
        f.unmounted();
    }
}

#[tokio::test]
async fn preserving_a_readonly_alternative_revalidates_the_chosen_app() {
    for reuse in [false, true] {
        for launch in [AppLaunch::Open, AppLaunch::Preserve] {
            let f = Fixture::new(None, DMG.to_vec(), 200).await;
            f.old();
            let user = f.config.user.join(BUNDLE);
            if reuse {
                bundle(&user, Some(TARGET));
            }
            let accepting = f.base().join("accepting-codesign");
            fs::copy(&f.config.codesign, &accepting).unwrap();
            script(
                &f.config.codesign,
                &format!(
                    r#"'{}' "$@" || exit $?
for last do :; done
if [ "$last" = '{}' ] && [ -d '{}' ]; then
    n=0
    [ ! -f '{}' ] || n=$(/bin/cat '{}')
    n=$((n + 1))
    printf '%s' "$n" > '{}'
    if [ "$n" = 4 ]; then printf OTHERTEAM > '{}/Signer'; fi
fi"#,
                    accepting.display(),
                    f.installed().display(),
                    user.display(),
                    f.base().join("checks").display(),
                    f.base().join("checks").display(),
                    f.base().join("checks").display(),
                    user.display()
                ),
            );
            script(
                &f.config.open,
                &format!(
                    "for last do :; done\n/bin/cat \"$last/Signer\" >> '{}'",
                    f.base().join("opened-signers").display()
                ),
            );
            fs::set_permissions(&f.config.system, fs::Permissions::from_mode(0o500)).unwrap();
            let result = ensure_with(
                f.config.clone(),
                Version::parse(TARGET).unwrap(),
                launch,
                Instant::now() + Duration::from_secs(60),
            )
            .await;
            fs::set_permissions(&f.config.system, fs::Permissions::from_mode(0o700)).unwrap();
            assert_eq!(
                fs::read(user.join("Signer")).unwrap(),
                b"OTHERTEAM",
                "The source-change probe did not run: reuse={reuse}, launch={launch:?}"
            );
            assert_eq!(result.unwrap_err().kind, AppInstallFailure::Verification);
            let opened = fs::read_to_string(f.base().join("opened-signers")).unwrap_or_default();
            assert!(
                !opened.contains("OTHERTEAM"),
                "Opened a changed app: {opened}"
            );
            f.preserved();
        }
    }
}

#[tokio::test]
async fn readonly_alternatives_with_unknown_identity_still_fail() {
    for invalid in ["signer", "identifier"] {
        let f = Fixture::new(None, DMG.to_vec(), 200).await;
        f.old();
        let user = f.config.user.join(BUNDLE);
        bundle(&user, Some(TARGET));
        if invalid == "signer" {
            fs::write(f.installed().join("Signer"), "OTHERTEAM").unwrap();
        } else {
            let path = f.installed().join("Contents/Info.plist");
            let mut info = plist::Value::from_file(&path).unwrap();
            info.as_dictionary_mut().unwrap().insert(
                "CFBundleIdentifier".into(),
                plist::Value::String("unrelated.app".into()),
            );
            info.to_file_xml(path).unwrap();
        }
        fs::set_permissions(&f.config.system, fs::Permissions::from_mode(0o500)).unwrap();
        let result = f.run().await;
        fs::set_permissions(&f.config.system, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(result.unwrap_err().kind, AppInstallFailure::Verification);
        assert_eq!(
            fs::read(user.join("Contents/Helpers/koi")).unwrap(),
            TARGET.as_bytes()
        );
        f.preserved();
        assert!(f.requests.lock().unwrap().is_empty());
        f.unmounted();
    }
}

#[tokio::test]
async fn a_changed_copy_source_is_not_opened_after_upgrading_an_alternative() {
    let mut f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    bundle(&f.config.user.join(BUNDLE), Some("0.4.0"));
    f.config.ditto = f.base().join("changing-source-copy");
    script(
        &f.config.ditto,
        "/usr/bin/ditto \"$@\"\nprintf OTHERTEAM > \"$1/Signer\"",
    );
    let error = ensure_with(
        f.config.clone(),
        Version::parse(TARGET).unwrap(),
        AppLaunch::Open,
        Instant::now() + Duration::from_secs(60),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind, AppInstallFailure::Verification);
    assert!(
        !f.base().join("open.log").exists(),
        "The source signer changed during a copy, but that source was opened before verification"
    );
}

#[tokio::test]
async fn an_alternative_upgrade_preserves_another_profiles_helper_path() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    let old_helper = f.installed().join("Contents/Helpers/koi");
    script(&old_helper, "echo 'koi 0.1.0'");
    let user = f.config.user.join(BUNDLE);
    bundle(&user, Some("0.4.0"));
    script(&user.join("Contents/Helpers/koi"), "echo 'koi 0.4.0'");
    let current = koidrive_driver::Paths::under(&f.base().join("current-profile"));
    let other = koidrive_driver::Paths::under(&f.base().join("other-profile"));
    let current_service = crate::service::Service::new(&current, &f.base(), &old_helper).unwrap();
    let other_service = crate::service::Service::new(&other, &f.base(), &old_helper).unwrap();
    assert_ne!(current_service.label(), other_service.label());
    assert!(
        current_service
            .plist()
            .contains(old_helper.to_str().unwrap())
    );
    assert!(other_service.plist().contains(old_helper.to_str().unwrap()));
    assert_eq!(
        crate::service::version_of(&old_helper),
        Some(Version::parse("0.1.0").unwrap())
    );
    let result = f.run().await.unwrap();
    assert!(matches!(result, AppInstall::Present { .. }));
    assert!(
        old_helper.is_file(),
        "Updating one profile removed the helper path still recorded by another profile"
    );
}

#[tokio::test]
async fn an_older_alternative_is_upgraded_in_place_from_the_newer_installed_copy() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    let user = f.config.user.join(BUNDLE);
    bundle(&user, Some("0.4.0"));
    script(
        &f.config.spctl,
        &format!(
            r#"
printf '%s\n' "$*" >> '{}/spctl.log'
for last do :; done
printf 'source=%s\n' "$(cat "$last/Notarization")"
"#,
            f.base().display()
        ),
    );
    let AppInstall::Present {
        path,
        version,
        change,
        retired,
        residue,
        ..
    } = f.run().await.unwrap()
    else {
        panic!()
    };
    assert_eq!(path, user);
    assert_eq!(version, Version::parse("0.4.0").unwrap());
    assert_eq!(change, AppChange::Unchanged);
    assert!(residue.is_empty());
    assert_eq!(
        fs::read(f.installed().join("Contents/Helpers/koi")).unwrap(),
        b"0.4.0"
    );
    assert_eq!(
        fs::read(user.join("Contents/Helpers/koi")).unwrap(),
        b"0.4.0"
    );
    assert_eq!(retired.len(), 1);
    assert_eq!(retired[0].version, Some(Version::parse("0.1.0").unwrap()));
    assert_eq!(
        fs::read(retired[0].bundle.join("Contents/Helpers/koi")).unwrap(),
        b"0.1.0"
    );
    assert!(f.requests.lock().unwrap().is_empty());
    assert!(!f.base().join("hdi.log").exists());
    let assessed = fs::read_to_string(f.base().join("spctl.log")).unwrap();
    assert!(
        assessed
            .lines()
            .next()
            .unwrap()
            .contains("/.koidrive-retired-")
    );
    assert!(!assessed.contains(user.to_str().unwrap()));
    retired[0].discard().unwrap();
    assert_eq!(
        fs::read(f.installed().join("Contents/Helpers/koi")).unwrap(),
        b"0.4.0"
    );
    assert_eq!(fs::read_dir(&f.config.system).unwrap().count(), 2);
    f.unmounted();
}

#[tokio::test]
async fn a_running_older_alternative_is_reopened_once_at_its_upgraded_path() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    bundle(&f.config.user.join(BUNDLE), Some("0.4.0"));
    f.flag("running");
    let AppInstall::Present {
        launch, retired, ..
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
    assert_eq!(launch, AppLaunchResult::Requested);
    assert_eq!(
        fs::read_to_string(f.base().join("open.log")).unwrap(),
        format!("-a {}\n", f.installed().display())
    );
    assert_eq!(
        fs::read(f.installed().join("Contents/Helpers/koi")).unwrap(),
        b"0.4.0"
    );
    assert_eq!(retired.len(), 1);
    retired[0].discard().unwrap();
    f.unmounted();
}

#[tokio::test]
async fn a_failed_alternative_upgrade_preserves_the_root_and_reports_it() {
    for variant in ["copy", "notarization", "identity"] {
        let mut f = Fixture::new(None, DMG.to_vec(), 200).await;
        f.old();
        bundle(&f.config.user.join(BUNDLE), Some("0.4.0"));
        let ditto = f.config.ditto.clone();
        let spctl = f.base().join("spctl-accepting");
        fs::copy(&f.config.spctl, &spctl).unwrap();
        let displaced = f.base().join("displaced.app");
        match variant {
            "copy" => {
                f.config.ditto = f.base().join("partial-copy");
                script(&f.config.ditto, "/usr/bin/ditto \"$@\"\nexit 73");
            }
            "notarization" => script(&f.config.spctl, "exit 91"),
            "identity" => {
                f.config.ditto = f.base().join("substituting-copy");
                script(
                    &f.config.ditto,
                    &format!(
                        r#"/usr/bin/ditto "$@"
/bin/mv '{installed}' '{displaced}'
/usr/bin/ditto '{displaced}' '{installed}'
printf substituted > '{installed}/sentinel'"#,
                        installed = f.installed().display(),
                        displaced = displaced.display()
                    ),
                );
            }
            _ => unreachable!(),
        }
        let error = f.run().await.unwrap_err();
        assert_eq!(
            error.kind,
            match variant {
                "copy" => AppInstallFailure::Command,
                "notarization" => AppInstallFailure::Verification,
                _ => AppInstallFailure::ConcurrentChange,
            },
            "{variant}: {error:?}"
        );
        assert!(error.residue.contains(&f.installed()), "{variant}");
        f.preserved();
        assert!(f.requests.lock().unwrap().is_empty(), "{variant}");
        if variant == "identity" {
            assert_eq!(
                fs::read(f.installed().join("sentinel")).unwrap(),
                b"substituted"
            );
            assert_eq!(
                fs::read(displaced.join("Contents/Helpers/koi")).unwrap(),
                b"0.1.0"
            );
            continue;
        }
        assert_eq!(
            fs::read_dir(&f.config.system).unwrap().count(),
            2,
            "{variant}"
        );
        f.unmounted();
        f.config.ditto = ditto;
        fs::rename(&spctl, &f.config.spctl).unwrap();
        let AppInstall::Present {
            retired, residue, ..
        } = f.run().await.unwrap()
        else {
            panic!()
        };
        assert!(residue.is_empty());
        assert_eq!(retired.len(), 1);
        assert_eq!(
            fs::read(f.installed().join("Contents/Helpers/koi")).unwrap(),
            b"0.4.0"
        );
        retired[0].discard().unwrap();
        f.unmounted();
    }
}

#[tokio::test]
async fn cancelling_an_alternative_upgrade_reaps_the_copy_and_preserves_the_root() {
    let mut f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    bundle(&f.config.user.join(BUNDLE), Some("0.4.0"));
    f.config.ditto = f.base().join("blocking-copy");
    script(
        &f.config.ditto,
        &format!(
            "/usr/bin/ditto \"$@\"\necho $$ > '{}/copy.pid'\nexec /bin/sleep 30",
            f.base().display()
        ),
    );
    let registry = Registry::default();
    let task = {
        let config = f.config.clone();
        let registry = registry.clone();
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
    let pid = pid_ready(f.base().join("copy.pid")).await;
    registry.cancel().await;
    let error = task.await.unwrap().unwrap_err();
    assert_eq!(error.kind, AppInstallFailure::Cancelled);
    assert!(error.residue.contains(&f.installed()));
    assert_eq!(
        rustix::process::test_kill_process(rustix::process::Pid::from_raw(pid).unwrap()),
        Err(rustix::io::Errno::SRCH)
    );
    f.preserved();
    assert_eq!(fs::read_dir(&f.config.system).unwrap().count(), 2);
    f.unmounted();
}

#[tokio::test]
async fn sigkill_during_an_alternative_upgrade_recovers_on_retry() {
    for (kill_at, swapped) in [(1, false), (2, true)] {
        let f = Fixture::new(None, DMG.to_vec(), 200).await;
        f.old();
        bundle(&f.config.user.join(BUNDLE), Some("0.4.0"));
        script(
            &f.config.osascript,
            &format!(
                r#"
base='{}'
printf '%s\n' invoked >> "$base/quit.log"
count=$(/usr/bin/wc -l < "$base/quit.log")
if [ "$count" -eq {kill_at} ]; then echo $$ > "$base/crash-ready"; exec /bin/sleep 120; fi
echo stopped
"#,
                f.base().display()
            ),
        );
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "app_install::macos::tests::review_killed_transaction_child",
                "--nocapture",
            ])
            .env("KOI_REVIEW_KILL_BASE", f.base())
            .env("KOI_REVIEW_KILL_RELEASE", &f.config.release)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = pid_ready(f.base().join("crash-ready")).await;
        child.kill().await.unwrap();
        let _ = rustix::process::kill_process(
            rustix::process::Pid::from_raw(pid).unwrap(),
            rustix::process::Signal::KILL,
        );
        assert_eq!(
            fs::read(f.installed().join("Contents/Helpers/koi")).unwrap(),
            if swapped {
                b"0.4.0".as_slice()
            } else {
                b"0.1.0"
            }
        );
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
        assert!(residue.is_empty());
        assert_eq!(retired.len(), 1, "{kill_at}");
        assert_eq!(
            fs::read(retired[0].bundle.join("Contents/Helpers/koi")).unwrap(),
            b"0.1.0"
        );
        assert_eq!(
            fs::read(f.installed().join("Contents/Helpers/koi")).unwrap(),
            b"0.4.0"
        );
        assert!(f.requests.lock().unwrap().is_empty());
        assert!(!f.base().join("open.log").exists());
        retired[0].discard().unwrap();
        assert_eq!(fs::read_dir(&f.config.system).unwrap().count(), 2);
        f.unmounted();
    }
}

#[tokio::test]
async fn alternative_upgrade_tokens_survive_a_later_upgrade_of_both_roots() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    let user = f.config.user.join(BUNDLE);
    bundle(&user, Some(TARGET));
    drop(f.run().await.unwrap());
    assert_eq!(
        fs::read(f.installed().join("Contents/Helpers/koi")).unwrap(),
        TARGET.as_bytes()
    );
    let mut newer = Fixture::versioned("0.3.0", None, DMG.to_vec(), 200).await;
    newer.config.system = f.config.system.clone();
    newer.config.user = f.config.user.clone();
    newer.config.work = f.config.work.clone();
    let AppInstall::Present {
        path,
        version,
        retired,
        residue,
        ..
    } = ensure_with(
        newer.config.clone(),
        Version::parse("0.3.0").unwrap(),
        AppLaunch::Preserve,
        Instant::now() + Duration::from_secs(60),
    )
    .await
    .unwrap()
    else {
        panic!()
    };
    assert_eq!(path, f.installed());
    assert_eq!(version, Version::parse("0.3.0").unwrap());
    assert!(residue.is_empty());
    for helper in [f.installed(), user.clone()] {
        assert_eq!(
            fs::read(helper.join("Contents/Helpers/koi")).unwrap(),
            b"0.3.0"
        );
    }
    let mut previous: Vec<_> = retired
        .iter()
        .map(|token| fs::read(token.bundle.join("Contents/Helpers/koi")).unwrap())
        .collect();
    previous.sort();
    assert_eq!(
        previous,
        [
            b"0.1.0".to_vec(),
            TARGET.as_bytes().to_vec(),
            TARGET.as_bytes().to_vec()
        ]
    );
    for token in retired {
        token.discard().unwrap();
    }
    for root in [&f.config.system, &f.config.user] {
        assert_eq!(fs::read_dir(root).unwrap().count(), 2);
        assert_eq!(
            fs::read(root.join(BUNDLE).join("Contents/Helpers/koi")).unwrap(),
            b"0.3.0"
        );
    }
    f.unmounted();
    newer.unmounted();
}

#[tokio::test]
async fn review_partial_symlink_copy_does_not_wedge_recovery() {
    let mut f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    fs::create_dir_all(f.incoming().join("Contents/Versions/A")).unwrap();
    symlink("A", f.incoming().join("Contents/Versions/Current")).unwrap();
    let ditto = f.config.ditto.clone();
    f.config.ditto = f.base().join("partial-symlink-copy");
    script(
        &f.config.ditto,
        r#"
for last do :; done
/bin/mkdir -p "$last/Contents/Versions"
/bin/ln -s A "$last/Contents/Versions/Current"
exit 73
"#,
    );
    let first = f.run().await.unwrap_err();
    assert_eq!(first.kind, AppInstallFailure::Command);
    f.preserved();
    f.config.ditto = ditto;
    let retry = f.run().await;
    assert!(
        matches!(
            retry,
            Ok(AppInstall::Present {
                change: AppChange::Replaced,
                ..
            })
        ),
        "{retry:?}"
    );
}

#[tokio::test]
async fn review_interrupted_retirement_with_dangling_symlink_can_finish() {
    let (f, token) = retired_fixture().await;
    let versions = token.bundle.join("Contents/Versions");
    fs::create_dir_all(versions.join("A")).unwrap();
    fs::write(versions.join("A/data"), "owned").unwrap();
    symlink("A", versions.join("Current")).unwrap();
    let (_tx, cancel) = watch::channel(false);
    let control = Control {
        deadline: Instant::now() + Duration::from_secs(10),
        cancel,
    };
    safe_tree(&token.bundle, &control).unwrap();
    let mut data = token.ownership.receipt.data();
    data.lifecycle = Lifecycle::Retiring;
    token.ownership.receipt.save(&data).unwrap();
    fs::remove_dir_all(versions.join("A")).unwrap();
    drop(token);
    let AppInstall::Present { retired, .. } = f.run().await.unwrap() else {
        panic!()
    };
    assert_eq!(retired.len(), 1);
    retired[0].discard().unwrap();
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

#[tokio::test]
async fn post_swap_sigkill_reopens_ui_even_while_detach_is_stuck() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    f.flag("running");
    script(
        &f.config.osascript,
        &format!(
            r#"
base='{}'
printf '%s\n' invoked >> "$base/quit.log"
count=$(/usr/bin/wc -l < "$base/quit.log")
if [ "$count" -eq 2 ]; then echo $$ > "$base/crash-ready"; exec /bin/sleep 120; fi
echo stopped
"#,
            f.base().display()
        ),
    );
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "app_install::macos::tests::review_killed_transaction_child",
            "--nocapture",
        ])
        .env("KOI_REVIEW_KILL_BASE", f.base())
        .env("KOI_REVIEW_KILL_RELEASE", &f.config.release)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let pid = pid_ready(f.base().join("crash-ready")).await;
    child.kill().await.unwrap();
    let _ = rustix::process::kill_process(
        rustix::process::Pid::from_raw(pid).unwrap(),
        rustix::process::Signal::KILL,
    );
    assert!(!f.base().join("open.log").exists());
    f.flag("detach-fail");
    let error = f.run().await.unwrap_err();
    assert!(!error.residue.is_empty());
    assert!(f.base().join("mounted").exists());
    assert!(
        fs::read_to_string(f.base().join("open.log"))
            .unwrap()
            .contains(f.installed().to_str().unwrap())
    );
    fs::remove_file(f.base().join("detach-fail")).unwrap();
    let AppInstall::Present {
        retired,
        residue,
        change,
        ..
    } = f.run().await.unwrap()
    else {
        panic!()
    };
    assert_eq!(change, AppChange::Unchanged);
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
async fn lifecycle_retires_older_alternative_without_download_when_current_exists() {
    for current_exists in [false, true] {
        let f = Fixture::new(None, DMG.to_vec(), 200).await;
        if current_exists {
            bundle(&f.installed(), Some(TARGET));
        }
        let user = f.config.user.join(BUNDLE);
        bundle(&user, Some("0.1.0"));
        let AppInstall::Present {
            retired, residue, ..
        } = f.run().await.unwrap()
        else {
            panic!()
        };
        assert!(residue.is_empty());
        assert_eq!(
            fs::read(user.join("Contents/Helpers/koi")).unwrap(),
            TARGET.as_bytes()
        );
        assert_eq!(retired.len(), 1);
        assert_eq!(
            fs::read(retired[0].bundle.join("Contents/Helpers/koi")).unwrap(),
            b"0.1.0"
        );
        assert_eq!(f.requests.lock().unwrap().is_empty(), current_exists);
        retired[0].discard().unwrap();
        f.unmounted();
    }
}
use std::os::unix::fs::{PermissionsExt, symlink};
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;

const TARGET: &str = "0.2.0-beta.3";

#[tokio::test]
async fn recovery_refuses_substituted_receipts_roots_and_retired_bundles() {
    for variant in [
        "receipt-symlink",
        "receipt-hardlink",
        "malformed",
        "oversize",
        "public-mode",
        "forged-parent",
        "work-root",
        "app-root",
        "parent",
        "bundle",
        "current",
    ] {
        let (f, retired) = retired_fixture().await;
        let receipt = retired.ownership.receipt.path();
        let outside = f.base().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("sentinel"), "unrelated").unwrap();
        match variant {
            "receipt-symlink" => {
                let original = f.base().join("original-receipt");
                fs::rename(&receipt, &original).unwrap();
                symlink(original, &receipt).unwrap();
            }
            "receipt-hardlink" => fs::hard_link(&receipt, outside.join("linked")).unwrap(),
            "malformed" => fs::write(&receipt, b"{not a receipt}").unwrap(),
            "oversize" => fs::write(&receipt, vec![b' '; 32 * 1024 + 1]).unwrap(),
            "public-mode" => {
                fs::set_permissions(&receipt, fs::Permissions::from_mode(0o644)).unwrap()
            }
            "forged-parent" => {
                let mut data = retired.ownership.receipt.data();
                data.stage.as_mut().unwrap().parent = OwnedPath::capture(outside.clone()).unwrap();
                fs::write(&receipt, serde_json::to_vec(&data).unwrap()).unwrap();
            }
            "work-root" => {
                let moved = f.base().join("original-work-root");
                fs::rename(&f.config.work, &moved).unwrap();
                fs::create_dir(&f.config.work).unwrap();
                fs::set_permissions(&f.config.work, fs::Permissions::from_mode(0o700)).unwrap();
                fs::copy(moved.join(receipt.file_name().unwrap()), &receipt).unwrap();
            }
            "app-root" => {
                fs::rename(&f.config.system, f.base().join("original-app-root")).unwrap();
                fs::create_dir(&f.config.system).unwrap();
                bundle(&f.installed(), Some(TARGET));
                fs::write(f.config.system.join("sentinel"), "unrelated").unwrap();
            }
            "parent" => {
                let parent = retired.bundle.parent().unwrap();
                fs::rename(parent, f.base().join("original-parent")).unwrap();
                fs::create_dir(parent).unwrap();
                bundle(&retired.bundle, Some("8.0.0"));
                fs::write(parent.join("sentinel"), "unrelated").unwrap();
            }
            "bundle" => {
                fs::rename(&retired.bundle, f.base().join("original-retired")).unwrap();
                bundle(&retired.bundle, Some("8.0.0"));
                fs::write(retired.bundle.join("sentinel"), "unrelated").unwrap();
            }
            "current" => {
                fs::rename(f.installed(), f.base().join("original-current")).unwrap();
                bundle(&f.installed(), Some(TARGET));
                fs::write(f.installed().join("Signer"), "OTHERTEAM").unwrap();
            }
            _ => unreachable!(),
        }
        let error = f.run().await.unwrap_err();
        assert!(!error.residue.is_empty(), "{variant}: {error:?}");
        assert!(fs::symlink_metadata(&receipt).is_ok(), "{variant}");
        assert_eq!(
            fs::read(outside.join("sentinel")).unwrap(),
            b"unrelated",
            "{variant}"
        );
        if variant == "parent" {
            assert_eq!(
                fs::read(retired.bundle.parent().unwrap().join("sentinel")).unwrap(),
                b"unrelated"
            );
        }
        if variant == "bundle" {
            assert_eq!(
                fs::read(retired.bundle.join("sentinel")).unwrap(),
                b"unrelated"
            );
        }
        if variant == "app-root" {
            assert_eq!(
                fs::read(f.config.system.join("sentinel")).unwrap(),
                b"unrelated"
            );
        }
    }
}

#[tokio::test]
async fn recovery_refuses_foreign_mount_or_substituted_image_work_and_point() {
    for variant in ["foreign-mount", "image", "work", "point"] {
        let f = Fixture::new(None, DMG.to_vec(), 200).await;
        f.flag("detach-fail");
        let AppInstall::Present { residue, .. } = f.run().await.unwrap() else {
            panic!()
        };
        assert_eq!(residue.len(), 1);
        fs::remove_file(f.base().join("detach-fail")).unwrap();
        let mounted = fs::read_to_string(f.base().join("mounted")).unwrap();
        let mut lines = mounted.lines();
        let point = PathBuf::from(lines.next().unwrap());
        let image = PathBuf::from(lines.next().unwrap());
        let sentinel = match variant {
            "foreign-mount" => {
                fs::write(
                    f.base().join("mounted"),
                    format!(
                        "{}\n{}\n",
                        point.display(),
                        f.base().join("foreign.dmg").display()
                    ),
                )
                .unwrap();
                point.join("sentinel")
            }
            "image" => {
                fs::rename(&image, f.base().join("original-image")).unwrap();
                fs::write(&image, "unrelated image").unwrap();
                point.join("sentinel")
            }
            "work" => {
                let work = point.parent().unwrap();
                fs::rename(work, f.base().join("original-work")).unwrap();
                fs::create_dir(work).unwrap();
                work.join("sentinel")
            }
            "point" => {
                fs::rename(&point, f.base().join("original-point")).unwrap();
                fs::create_dir(&point).unwrap();
                point.join("sentinel")
            }
            _ => unreachable!(),
        };
        fs::write(&sentinel, "unrelated").unwrap();
        let before = fs::read_to_string(f.base().join("hdi.log"))
            .unwrap()
            .lines()
            .filter(|line| line.starts_with("detach "))
            .count();
        let error = f.run().await.unwrap_err();
        assert!(!error.residue.is_empty(), "{variant}");
        assert_eq!(fs::read(&sentinel).unwrap(), b"unrelated", "{variant}");
        assert!(f.base().join("mounted").exists(), "{variant}");
        let after = fs::read_to_string(f.base().join("hdi.log"))
            .unwrap()
            .lines()
            .filter(|line| line.starts_with("detach "))
            .count();
        assert_eq!(before, after, "{variant}");
    }
}

#[tokio::test]
async fn recovery_after_owned_removal_before_receipt_removal_is_idempotent() {
    for remove_parent in [false, true] {
        let (f, retired) = retired_fixture().await;
        let mut data = retired.ownership.receipt.data();
        data.lifecycle = Lifecycle::Retiring;
        retired.ownership.receipt.save(&data).unwrap();
        fs::remove_dir_all(&retired.bundle).unwrap();
        if remove_parent {
            fs::remove_dir(retired.bundle.parent().unwrap()).unwrap();
        }
        let AppInstall::Present {
            retired,
            residue,
            change,
            ..
        } = f.run().await.unwrap()
        else {
            panic!()
        };
        assert_eq!(change, AppChange::Unchanged);
        assert!(retired.is_empty());
        assert!(residue.is_empty());
        f.unmounted();
        assert_eq!(fs::read_dir(&f.config.work).unwrap().count(), 1);
    }
}

#[tokio::test]
async fn recovery_reports_unknown_orphan_work_without_touching_it() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    bundle(&f.installed(), Some(TARGET));
    private_work(&f.config.work, true).unwrap();
    let orphan = f.config.work.join("transaction-unowned");
    fs::create_dir(&orphan).unwrap();
    fs::write(orphan.join("sentinel"), "unrelated").unwrap();
    let error = f.run().await.unwrap_err();
    assert_eq!(error.kind, AppInstallFailure::UnsafePath);
    assert_eq!(error.residue.as_slice(), std::slice::from_ref(&orphan));
    assert_eq!(fs::read(orphan.join("sentinel")).unwrap(), b"unrelated");
    assert!(f.requests.lock().unwrap().is_empty());
    assert!(!f.base().join("hdi.log").exists());
}

#[tokio::test]
async fn recovered_retirements_survive_a_later_upgrade() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    drop(f.run().await.unwrap());
    let mut newer = Fixture::versioned("0.3.0", None, DMG.to_vec(), 200).await;
    newer.config.system = f.config.system.clone();
    newer.config.user = f.config.user.clone();
    newer.config.work = f.config.work.clone();
    let AppInstall::Present {
        version,
        retired,
        residue,
        ..
    } = ensure_with(
        newer.config.clone(),
        Version::parse("0.3.0").unwrap(),
        AppLaunch::Preserve,
        Instant::now() + Duration::from_secs(60),
    )
    .await
    .unwrap()
    else {
        panic!()
    };
    assert_eq!(version, Version::parse("0.3.0").unwrap());
    assert!(residue.is_empty());
    assert_eq!(retired.len(), 2);
    let mut versions: Vec<_> = retired
        .iter()
        .map(|token| token.version.clone().unwrap())
        .collect();
    versions.sort();
    assert_eq!(
        versions,
        [
            Version::parse("0.1.0").unwrap(),
            Version::parse(TARGET).unwrap()
        ]
    );
    for token in retired {
        token.discard().unwrap();
    }
    f.unmounted();
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
async fn sigkill_before_swap_restores_recorded_running_ui_and_retries() {
    for running in [false, true] {
        let f = Fixture::new(None, DMG.to_vec(), 200).await;
        f.old();
        if running {
            f.flag("running");
        }
        script(
            &f.config.osascript,
            &format!(
                r#"
base='{}'
printf '%s\n' invoked >> "$base/quit.log"
count=$(/usr/bin/wc -l < "$base/quit.log")
if [ "$count" -eq 1 ]; then /bin/rm -f "$base/running"; echo $$ > "$base/crash-ready"; exec /bin/sleep 120; fi
echo stopped
"#,
                f.base().display()
            ),
        );
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "app_install::macos::tests::review_killed_transaction_child",
                "--nocapture",
            ])
            .env("KOI_REVIEW_KILL_BASE", f.base())
            .env("KOI_REVIEW_KILL_RELEASE", &f.config.release)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = pid_ready(f.base().join("crash-ready")).await;
        child.kill().await.unwrap();
        let _ = rustix::process::kill_process(
            rustix::process::Pid::from_raw(pid).unwrap(),
            rustix::process::Signal::KILL,
        );
        f.preserved();
        assert!(f.base().join("mounted").exists());
        let AppInstall::Present {
            change,
            retired,
            residue,
            ..
        } = f.run().await.unwrap()
        else {
            panic!()
        };
        assert_eq!(change, AppChange::Replaced);
        assert!(residue.is_empty());
        assert_eq!(retired.len(), 1);
        assert_eq!(f.base().join("open.log").exists(), running);
        retired[0].discard().unwrap();
        f.unmounted();
    }
}

#[tokio::test]
async fn sigkill_during_attach_recovers_the_exact_owned_mount() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    f.flag("attach-sleep");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "app_install::macos::tests::review_killed_transaction_child",
            "--nocapture",
        ])
        .env("KOI_REVIEW_KILL_BASE", f.base())
        .env("KOI_REVIEW_KILL_RELEASE", &f.config.release)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let pid = pid_ready(f.base().join("child.pid")).await;
    child.kill().await.unwrap();
    let _ = rustix::process::kill_process(
        rustix::process::Pid::from_raw(pid).unwrap(),
        rustix::process::Signal::KILL,
    );
    fs::remove_file(f.base().join("attach-sleep")).unwrap();
    let AppInstall::Present {
        retired, residue, ..
    } = f.run().await.unwrap()
    else {
        panic!()
    };
    assert!(residue.is_empty());
    assert_eq!(retired.len(), 1);
    f.unmounted();
    retired[0].discard().unwrap();
}

#[tokio::test]
async fn pending_retirements_follow_a_new_current_app_across_roots() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    bundle(&f.config.user.join(BUNDLE), Some("0.1.0"));
    fs::set_permissions(&f.config.system, fs::Permissions::from_mode(0o500)).unwrap();
    let first = f.run().await;
    fs::set_permissions(&f.config.system, fs::Permissions::from_mode(0o700)).unwrap();
    let AppInstall::Present { path, .. } = first.unwrap() else {
        panic!()
    };
    assert_eq!(path, f.config.user.join(BUNDLE));
    bundle(&f.installed(), Some("0.1.0"));
    let mut newer = Fixture::versioned("0.3.0", None, DMG.to_vec(), 200).await;
    newer.config.system = f.config.system.clone();
    newer.config.user = f.config.user.clone();
    newer.config.work = f.config.work.clone();
    let AppInstall::Present {
        path,
        retired,
        residue,
        ..
    } = ensure_with(
        newer.config.clone(),
        Version::parse("0.3.0").unwrap(),
        AppLaunch::Preserve,
        Instant::now() + Duration::from_secs(60),
    )
    .await
    .unwrap()
    else {
        panic!()
    };
    assert_eq!(path, f.installed());
    assert_eq!(
        fs::read(f.config.user.join(BUNDLE).join("Contents/Helpers/koi")).unwrap(),
        b"0.3.0"
    );
    assert_eq!(retired.len(), 3);
    assert!(residue.is_empty());
    for token in retired {
        token.discard().unwrap();
    }
    f.unmounted();
}

#[tokio::test]
async fn an_immovable_older_alternative_is_reported_and_preserved() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    bundle(&f.installed(), Some(TARGET));
    let user = f.config.user.join(BUNDLE);
    bundle(&user, Some("0.1.0"));
    fs::set_permissions(&f.config.user, fs::Permissions::from_mode(0o500)).unwrap();
    let result = f.run().await;
    fs::set_permissions(&f.config.user, fs::Permissions::from_mode(0o700)).unwrap();
    let AppInstall::Present {
        path,
        preserved,
        residue,
        retired,
        ..
    } = result.unwrap()
    else {
        panic!()
    };
    assert_eq!(path, f.installed());
    assert_eq!(preserved, vec![user.clone()]);
    assert!(residue.is_empty());
    assert!(retired.is_empty());
    assert_eq!(
        fs::read(user.join("Contents/Helpers/koi")).unwrap(),
        b"0.1.0"
    );
    assert_eq!(
        fs::read(f.installed().join("Contents/Helpers/koi")).unwrap(),
        TARGET.as_bytes()
    );
    assert!(f.requests.lock().unwrap().is_empty());
    f.unmounted();
}

#[tokio::test]
async fn partial_copy_failure_then_retry_preserves_and_replaces_old_app() {
    let mut f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    let ditto = f.config.ditto.clone();
    f.config.ditto = f.base().join("partial-copy");
    script(&f.config.ditto, "/usr/bin/ditto \"$@\"\nexit 73");
    assert_eq!(f.run().await.unwrap_err().kind, AppInstallFailure::Command);
    f.preserved();
    f.unmounted();
    f.config.ditto = ditto;
    let AppInstall::Present {
        retired, change, ..
    } = f.run().await.unwrap()
    else {
        panic!()
    };
    assert_eq!(change, AppChange::Replaced);
    assert_eq!(retired.len(), 1);
    retired[0].discard().unwrap();
    f.unmounted();
}

#[tokio::test]
async fn versionless_installed_bundle_still_requires_the_expected_identity_and_signer() {
    for variant in ["signer", "identifier"] {
        let f = Fixture::new(None, DMG.to_vec(), 200).await;
        bundle(&f.installed(), None);
        if variant == "signer" {
            fs::write(f.installed().join("Signer"), "OTHERTEAM").unwrap();
        } else {
            let path = f.installed().join("Contents/Info.plist");
            let mut info = plist::Value::from_file(&path).unwrap();
            info.as_dictionary_mut().unwrap().insert(
                "CFBundleIdentifier".into(),
                plist::Value::String("unrelated.app".into()),
            );
            info.to_file_xml(path).unwrap();
        }
        assert_eq!(
            f.run().await.unwrap_err().kind,
            AppInstallFailure::Verification
        );
        assert!(f.requests.lock().unwrap().is_empty());
        assert_eq!(
            fs::read(f.installed().join("Contents/Helpers/koi")).unwrap(),
            b"development"
        );
    }
}

#[tokio::test]
async fn staged_bundle_without_full_release_version_is_rejected() {
    let mut f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    f.config.ditto = f.base().join("versionless-copy");
    script(
        &f.config.ditto,
        r#"/usr/bin/ditto "$@"
for last do :; done
/usr/libexec/PlistBuddy -c 'Delete :KoidriveVersion' "$last/Contents/Info.plist""#,
    );
    assert_eq!(
        f.run().await.unwrap_err().kind,
        AppInstallFailure::Verification
    );
    f.preserved();
    f.unmounted();
}

#[tokio::test]
async fn native_dmg_attach_copy_detach_and_receipt_replay() {
    let source = tempfile::tempdir().unwrap();
    let source_path = fs::canonicalize(source.path()).unwrap();
    let payload = source_path.join("payload");
    bundle(&payload.join(BUNDLE), Some(TARGET));
    let image = source_path.join("fixture.dmg");
    let (_tx, cancel) = watch::channel(false);
    let control = Control {
        deadline: Instant::now() + Duration::from_secs(60),
        cancel,
    };
    process(
        Path::new("/usr/bin/hdiutil"),
        &args(&[
            OsStr::new("create"),
            OsStr::new("-srcfolder"),
            payload.as_os_str(),
            OsStr::new("-volname"),
            OsStr::new("Koidrive installer test"),
            OsStr::new("-format"),
            OsStr::new("UDZO"),
            image.as_os_str(),
        ]),
        &control,
    )
    .await
    .unwrap();
    let bytes = fs::read(&image).unwrap();
    let sum: String = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let mut f = Fixture::new(
        Some(format!("{sum}  Koidrive-{TARGET}-arm64.dmg\n")),
        bytes.clone(),
        200,
    )
    .await;
    f.config.max_download = bytes.len() as u64;
    f.config.hdiutil = "/usr/bin/hdiutil".into();
    f.old();
    let AppInstall::Present {
        change,
        retired,
        residue,
        ..
    } = f.run().await.unwrap()
    else {
        panic!()
    };
    assert_eq!(change, AppChange::Replaced);
    assert!(residue.is_empty());
    assert_eq!(retired.len(), 1);
    let old_path = retired[0].bundle.clone();
    drop(retired);
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
    assert!(residue.is_empty());
    assert_eq!(retired.len(), 1);
    assert_eq!(retired[0].bundle, old_path);
    retired[0].discard().unwrap();
    let inventory = process(
        Path::new("/usr/bin/hdiutil"),
        &args(&[OsStr::new("info"), OsStr::new("-plist")]),
        &control,
    )
    .await
    .unwrap();
    let plist = plist::Value::from_reader(std::io::Cursor::new(inventory)).unwrap();
    for image in plist.as_dictionary().unwrap()["images"].as_array().unwrap() {
        for entity in image.as_dictionary().unwrap()["system-entities"]
            .as_array()
            .unwrap()
        {
            if let Some(point) = entity
                .as_dictionary()
                .unwrap()
                .get("mount-point")
                .and_then(plist::Value::as_string)
            {
                assert!(!Path::new(point).starts_with(&f.config.work));
            }
        }
    }
    f.unmounted();
}

#[tokio::test]
async fn recovery_reports_unowned_application_staging_without_deletion() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    bundle(&f.installed(), Some(TARGET));
    private_work(&f.config.work, true).unwrap();
    let orphan = f.config.system.join(".koidrive-retired-unowned");
    fs::create_dir(&orphan).unwrap();
    fs::write(orphan.join("sentinel"), "unrelated").unwrap();
    let error = f.run().await.unwrap_err();
    assert_eq!(error.kind, AppInstallFailure::UnsafePath);
    assert_eq!(error.residue.as_slice(), std::slice::from_ref(&orphan));
    assert_eq!(fs::read(orphan.join("sentinel")).unwrap(), b"unrelated");
    assert!(f.requests.lock().unwrap().is_empty());
    assert!(!f.base().join("hdi.log").exists());
}
const DMG: &[u8] = b"A real byte fixture, not a fake digest.\x00\x01\xff";

pub(crate) async fn verified_helper_fixture(version: &str) -> (impl Drop, VerifiedHelper) {
    verified_helper_fixture_with_metadata(version, Some(version)).await
}

pub(crate) async fn verified_helper_fixture_with_metadata(
    version: &str,
    metadata: Option<&str>,
) -> (impl Drop, VerifiedHelper) {
    let f = Fixture::versioned(version, None, DMG.to_vec(), 200).await;
    bundle(&f.installed(), metadata);
    let binary = f.installed().join("Contents/Helpers/koi");
    script(
        &binary,
        &format!(
            "printf '%s\\n' \"$*\" >> \"$0.calls\"\ncase \"$1\" in\n--version) echo 'koi {version}' ;;\n_reconcile) echo '{{\"data\":{{\"service\":{{\"manager\":\"launchd\",\"name\":\"fixture\",\"state\":\"running\"}}}}}}' ;;\nesac"
        ),
    );
    let verified = verify_helper(
        f.config.clone(),
        &binary,
        Instant::now() + Duration::from_secs(30),
    )
    .await
    .unwrap();
    (f, verified)
}

fn digest() -> String {
    Sha256::digest(DMG)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
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

struct Fixture {
    temp: tempfile::TempDir,
    config: Config,
    requests: Arc<Mutex<Vec<String>>>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn script(path: &Path, text: &str) {
    let inspect = if path.file_name() == Some(OsStr::new("osascript")) {
        format!(
            r#"case "$*" in *inspectedBundlePath*) if [ -e '{}/running' ]; then echo running; else echo stopped; fi; exit 0;; esac"#,
            path.parent().unwrap().parent().unwrap().display()
        )
    } else {
        String::new()
    };
    fs::write(path, format!("#!/bin/sh\nset -eu\n{inspect}\n{text}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn bundle(path: &Path, version: Option<&str>) {
    fs::create_dir_all(path.join("Contents/Helpers")).unwrap();
    let mut dict = plist::Dictionary::new();
    dict.insert(
        "CFBundleIdentifier".into(),
        plist::Value::String(IDENTIFIER.into()),
    );
    dict.insert(
        "CFBundleShortVersionString".into(),
        plist::Value::String("0.2.0".into()),
    );
    if let Some(version) = version {
        dict.insert(
            "KoidriveVersion".into(),
            plist::Value::String(version.into()),
        );
    }
    plist::Value::Dictionary(dict)
        .to_file_xml(path.join("Contents/Info.plist"))
        .unwrap();
    fs::write(
        path.join("Contents/Helpers/koi"),
        version.unwrap_or("development"),
    )
    .unwrap();
    fs::write(path.join("Signer"), "656SPP5XR8").unwrap();
    fs::write(path.join("Notarization"), "Notarized Developer ID").unwrap();
}

impl Fixture {
    async fn new(sums: Option<String>, dmg: Vec<u8>, status: u16) -> Self {
        Self::versioned(TARGET, sums, dmg, status).await
    }

    async fn versioned(target: &str, sums: Option<String>, dmg: Vec<u8>, status: u16) -> Self {
        let temp = tempfile::Builder::new()
            .prefix("koi-installer-test-")
            .tempdir()
            .unwrap();
        let base = fs::canonicalize(temp.path()).unwrap();
        let tools = base.join("tools");
        fs::create_dir(&tools).unwrap();
        let incoming = base.join("incoming").join(BUNDLE);
        bundle(&incoming, Some(target));
        let sums = sums.unwrap_or_else(|| format!("{}  Koidrive-{target}-arm64.dmg\n", digest()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let observed = requests.clone();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let observed = observed.clone();
                let sums = sums.clone();
                let dmg = dmg.clone();
                tokio::spawn(async move {
                    let mut request = vec![0; 16 * 1024];
                    let len = socket.read(&mut request).await.unwrap();
                    let request = String::from_utf8_lossy(&request[..len]);
                    observed.lock().unwrap().push(request.to_string());
                    let path = request
                        .lines()
                        .next()
                        .unwrap()
                        .split_whitespace()
                        .nth(1)
                        .unwrap();
                    let body = if path.ends_with("/SHA256SUMS") {
                        sums.as_bytes()
                    } else {
                        &dmg
                    };
                    let response = format!(
                        "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    socket.write_all(response.as_bytes()).await.unwrap();
                    socket.write_all(body).await.unwrap();
                });
            }
        });
        let config = Config {
            system: base.join("system"),
            user: base.join("user"),
            work: base.join("work"),
            release: format!("http://{address}/releases/download"),
            codesign: tools.join("codesign"),
            spctl: tools.join("spctl"),
            hdiutil: tools.join("hdiutil"),
            ditto: "/usr/bin/ditto".into(),
            osascript: tools.join("osascript"),
            open: tools.join("open"),
            updater: base.join("updater"),
            max_download: 1024,
        };
        fs::create_dir(&config.system).unwrap();
        fs::write(&config.updater, "signed updater").unwrap();
        script(
            &config.codesign,
            &format!(
                r#"
printf '%s\n' "$*" >> '{}/codesign.log'
requirement=
while [ $# -gt 1 ]; do
    [ "$1" != -R ] || requirement=$2
    shift
done
if [ "$1" = '{}' ]; then
    [ "$requirement" = '={SIGNER}' ]
else
    [ "$requirement" = '={REQUIREMENT}' ] && [ "$(cat "$1/Signer")" = '656SPP5XR8' ]
fi
"#,
                base.display(),
                config.updater.display()
            ),
        );
        script(
            &config.spctl,
            r#"
for last do :; done
printf '%s: accepted\nsource=%s\n' "$last" "$(cat "$last/Notarization")" >&2
"#,
        );
        script(
            &config.hdiutil,
            &format!(
                r#"
base='{}'
printf '%s\n' "$*" >> "$base/hdi.log"
case "$1" in
attach)
    while [ "$1" != '-mountpoint' ]; do shift; done
    point="$2"
    shift 2
    for image do :; done
    /usr/bin/ditto "$base/incoming/Koidrive.app" "$point/Koidrive.app"
    printf '%s\n%s\n' "$point" "$image" > "$base/mounted"
    [ ! -e "$base/attach-fail" ] || exit 7
    if [ -e "$base/attach-sleep" ]; then echo $$ > "$base/child.pid"; exec /bin/sleep 30; fi
    printf '<plist version="1.0"><dict/></plist>\n'
    ;;
info)
    if [ -e "$base/info-fail" ]; then exit 9; fi
    if [ -e "$base/mounted" ]; then
        point=$(/usr/bin/head -n 1 "$base/mounted")
        image=$(/usr/bin/tail -n 1 "$base/mounted")
        printf '<plist version="1.0"><dict><key>images</key><array><dict><key>image-path</key><string>%s</string><key>system-entities</key><array><dict><key>mount-point</key><string>%s</string></dict></array></dict></array></dict></plist>\n' "$image" "$point"
    else
        printf '<plist version="1.0"><dict><key>images</key><array/></dict></plist>\n'
    fi
    ;;
detach)
    [ ! -e "$base/detach-fail" ] || exit 8
    if [ -e "$base/detach-block" ]; then
        echo $$ > "$base/detach.pid"
        while [ ! -e "$base/detach-release" ]; do /bin/sleep 0.02; done
    fi
    point=$(/usr/bin/head -n 1 "$base/mounted")
    /bin/rm -rf "$point/Koidrive.app"
    /bin/rm "$base/mounted"
    ;;
*) exit 11;;
esac
"#,
                base.display()
            ),
        );
        script(
            &config.osascript,
            &format!(
                r#"
base='{}'
printf '%s\n' invoked >> "$base/quit.log"
if [ -e "$base/quit-fail" ]; then exit 5; fi
if [ -e "$base/running" ]; then /bin/rm "$base/running"; echo running; else echo stopped; fi
"#,
                base.display()
            ),
        );
        script(
            &config.open,
            &format!(
                r#"
printf '%s\n' "$*" >> '{}/open.log'
[ ! -e '{}/open-fail' ]
"#,
                base.display(),
                base.display()
            ),
        );
        Self {
            temp,
            config,
            requests,
            server,
        }
    }

    fn base(&self) -> PathBuf {
        fs::canonicalize(self.temp.path()).unwrap()
    }
    fn installed(&self) -> PathBuf {
        self.config.system.join(BUNDLE)
    }
    fn incoming(&self) -> PathBuf {
        self.base().join("incoming").join(BUNDLE)
    }
    fn flag(&self, name: &str) {
        fs::write(self.base().join(name), "yes").unwrap();
    }
    fn old(&self) {
        bundle(&self.installed(), Some("0.1.0"));
    }
    fn preserved(&self) {
        assert_eq!(
            fs::read(self.installed().join("Contents/Helpers/koi")).unwrap(),
            b"0.1.0"
        );
    }
    async fn run(&self) -> Result<AppInstall, AppInstallError> {
        ensure_with(
            self.config.clone(),
            Version::parse(TARGET).unwrap(),
            AppLaunch::Preserve,
            Instant::now() + Duration::from_secs(60),
        )
        .await
    }
    fn unmounted(&self) {
        assert!(!self.base().join("mounted").exists());
        if self.config.work.exists() {
            let entries: Vec<_> = fs::read_dir(&self.config.work)
                .unwrap()
                .map(Result::unwrap)
                .collect();
            let mut receipts = 0;
            for entry in &entries {
                if entry.file_name() == ".koidrive-app-install.lock" {
                    continue;
                }
                assert!(entry.file_name().to_str().unwrap().starts_with("receipt-"));
                let data: ReceiptData =
                    serde_json::from_slice(&fs::read(entry.path()).unwrap()).unwrap();
                assert!(!data.work.path.exists());
                let stage = data.stage.unwrap();
                if let Some(previous) = stage.transfer.as_ref().unwrap().previous() {
                    assert_eq!(
                        observed(&stage.parent.path.join(BUNDLE)).unwrap(),
                        Some(previous.identity)
                    );
                } else {
                    assert!(matches!(data.lifecycle, Lifecycle::Reopening(_)));
                    assert_eq!(observed(&stage.parent.path.join(BUNDLE)).unwrap(), None);
                    assert_eq!(
                        fs::read(self.installed().join("Contents/Helpers/koi")).unwrap(),
                        data.target.as_bytes()
                    );
                }
                receipts += 1;
            }
            assert_eq!(entries.len(), receipts + 1);
        }
    }
}

#[tokio::test]
async fn equal_and_newer_are_verified_without_network() {
    for version in [TARGET, "0.3.0", "0.2.0-beta.4"] {
        let f = Fixture::new(None, DMG.to_vec(), 200).await;
        bundle(&f.installed(), Some(version));
        let before = fs::read(f.installed().join("Contents/Info.plist")).unwrap();
        let result = f.run().await.unwrap();
        assert!(matches!(
            result,
            AppInstall::Present {
                change: AppChange::Unchanged,
                launch: AppLaunchResult::NotRequested,
                ..
            }
        ));
        assert_eq!(
            fs::read(f.installed().join("Contents/Info.plist")).unwrap(),
            before
        );
        assert!(f.requests.lock().unwrap().is_empty());
        assert!(!f.config.work.exists());
        assert!(!f.base().join("hdi.log").exists());
    }
}

#[tokio::test]
async fn equal_and_newer_signed_apps_never_invoke_gatekeeper() {
    for version in [TARGET, "0.3.0", "0.2.0-beta.4"] {
        let fixture = Fixture::new(None, DMG.to_vec(), 200).await;
        bundle(&fixture.installed(), Some(version));
        script(
            &fixture.config.spctl,
            &format!(
                "echo invoked >> '{}/spctl.log'\nexit 91",
                fixture.base().display()
            ),
        );
        let result = fixture.run().await.unwrap();
        assert!(matches!(
            result,
            AppInstall::Present {
                change: AppChange::Unchanged,
                ..
            }
        ));
        assert!(fixture.requests.lock().unwrap().is_empty());
        assert!(!fixture.base().join("spctl.log").exists());
        assert!(!fixture.config.work.exists());
        assert_eq!(
            fs::read(fixture.installed().join("Contents/Helpers/koi")).unwrap(),
            version.as_bytes()
        );
        let verification = fs::read_to_string(fixture.base().join("codesign.log")).unwrap();
        assert!(verification.contains("--verify --deep --strict -R"));
        assert!(verification.contains(REQUIREMENT));
    }
}

#[tokio::test]
async fn incoming_and_staged_apps_both_require_gatekeeper_acceptance() {
    for fail_at in [1, 2] {
        let fixture = Fixture::new(None, DMG.to_vec(), 200).await;
        fixture.old();
        script(
            &fixture.config.spctl,
            &format!(
                r#"
printf '%s\n' "$*" >> '{base}/spctl.log'
count=$(/usr/bin/wc -l < '{base}/spctl.log')
[ "$count" -ne {fail_at} ] || exit 91
printf 'source=Notarized Developer ID\n'
"#,
                base = fixture.base().display()
            ),
        );
        assert_eq!(
            fixture.run().await.unwrap_err().kind,
            AppInstallFailure::Verification
        );
        let calls = fs::read_to_string(fixture.base().join("spctl.log")).unwrap();
        assert_eq!(calls.lines().count(), fail_at);
        assert!(
            calls
                .lines()
                .next()
                .unwrap()
                .contains("/mount/Koidrive.app")
        );
        if fail_at == 2 {
            assert!(
                calls
                    .lines()
                    .nth(1)
                    .unwrap()
                    .contains("/.koidrive-retired-")
            );
        }
        fixture.preserved();
        fixture.unmounted();
    }
}

#[tokio::test]
async fn exact_prerelease_install_is_twice_idempotent() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    let result = f.run().await.unwrap();
    assert!(
        matches!(result, AppInstall::Present { change: AppChange::Installed, retired, residue, .. } if retired.is_empty() && residue.is_empty())
    );
    let requests = f.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].starts_with(&format!("GET /releases/download/v{TARGET}/SHA256SUMS ")));
    assert!(requests[1].starts_with(&format!(
        "GET /releases/download/v{TARGET}/Koidrive-{TARGET}-arm64.dmg "
    )));
    assert!(
        requests
            .iter()
            .all(|request| !request.to_lowercase().contains("authorization:"))
    );
    assert_eq!(
        fs::read(f.installed().join("Contents/Helpers/koi")).unwrap(),
        TARGET.as_bytes()
    );
    assert!(matches!(
        f.run().await.unwrap(),
        AppInstall::Present {
            change: AppChange::Unchanged,
            ..
        }
    ));
    assert_eq!(f.requests.lock().unwrap().len(), 2);
    f.unmounted();
    let hdi = fs::read_to_string(f.base().join("hdi.log")).unwrap();
    assert!(hdi.contains("attach -readonly -nobrowse -noautoopen -mountpoint"));
    assert!(hdi.contains("detach "));
}

#[tokio::test]
async fn invalid_checksums_and_corruption_preserve_old_app() {
    let line = format!("{}  Koidrive-{TARGET}-arm64.dmg\n", digest());
    let cases = [
        ("".into(), DMG.to_vec()),
        (format!("{line}{line}"), DMG.to_vec()),
        (format!("bad  Koidrive-{TARGET}-arm64.dmg\n"), DMG.to_vec()),
        (
            format!("{}  Koidrive-{TARGET}-arm64.dmg\n", "é".repeat(32)),
            DMG.to_vec(),
        ),
        (line, b"corrupted".to_vec()),
    ];
    for (sums, dmg) in cases {
        let f = Fixture::new(Some(sums), dmg, 200).await;
        f.old();
        assert_eq!(
            f.run().await.unwrap_err().kind,
            AppInstallFailure::Verification
        );
        f.preserved();
        assert!(!f.base().join("hdi.log").exists());
        f.unmounted();
    }
}

#[tokio::test]
async fn unavailable_or_oversize_release_preserves_old_app() {
    for (status, bytes) in [(404, DMG.to_vec()), (200, vec![0; 2048])] {
        let f = Fixture::new(None, bytes, status).await;
        f.old();
        assert_eq!(f.run().await.unwrap_err().kind, AppInstallFailure::Download);
        f.preserved();
        assert!(!f.base().join("hdi.log").exists());
        f.unmounted();
    }
}

#[tokio::test]
async fn wrong_version_signer_identifier_and_notarization_are_rejected() {
    for variant in [
        "version",
        "signer",
        "identifier",
        "notarization",
        "missing-version",
    ] {
        let f = Fixture::new(None, DMG.to_vec(), 200).await;
        f.old();
        match variant {
            "version" => bundle(&f.incoming(), Some("0.2.0")),
            "missing-version" => bundle(&f.incoming(), None),
            "signer" => fs::write(f.incoming().join("Signer"), "OTHERTEAM").unwrap(),
            "notarization" => fs::write(f.incoming().join("Notarization"), "Developer ID").unwrap(),
            "identifier" => {
                let path = f.incoming().join("Contents/Info.plist");
                let mut plist = plist::Value::from_file(&path).unwrap();
                plist.as_dictionary_mut().unwrap().insert(
                    "CFBundleIdentifier".into(),
                    plist::Value::String("unrelated.app".into()),
                );
                plist.to_file_xml(path).unwrap();
            }
            _ => unreachable!(),
        }
        assert_eq!(
            f.run().await.unwrap_err().kind,
            AppInstallFailure::Verification,
            "{variant}"
        );
        f.preserved();
        f.unmounted();
    }
}

#[tokio::test]
async fn symlinked_destination_root_and_escaping_bundle_are_rejected() {
    for variant in ["destination", "root", "incoming", "installed"] {
        let f = Fixture::new(None, DMG.to_vec(), 200).await;
        let outside = f.base().join("outside");
        bundle(&outside, Some("0.1.0"));
        match variant {
            "destination" => symlink(&outside, f.installed()).unwrap(),
            "root" => {
                fs::remove_dir(&f.config.system).unwrap();
                symlink(&outside, &f.config.system).unwrap();
            }
            "incoming" => {
                f.old();
                symlink(&outside, f.incoming().join("escape")).unwrap();
            }
            "installed" => {
                f.old();
                symlink(&outside, f.installed().join("escape")).unwrap();
            }
            _ => unreachable!(),
        }
        assert_eq!(
            f.run().await.unwrap_err().kind,
            AppInstallFailure::UnsafePath
        );
        assert_eq!(
            fs::read(outside.join("Contents/Helpers/koi")).unwrap(),
            b"0.1.0"
        );
        f.unmounted();
    }
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
async fn attach_failure_after_mount_still_detaches_exact_owned_image() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    f.flag("attach-fail");
    assert!(f.run().await.is_err());
    f.preserved();
    f.unmounted();
    assert!(
        fs::read_to_string(f.base().join("hdi.log"))
            .unwrap()
            .contains("detach ")
    );
}

#[tokio::test]
async fn detach_failure_retains_private_mount_directory_and_reports_residue() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.flag("detach-fail");
    let AppInstall::Present {
        residue, change, ..
    } = f.run().await.unwrap()
    else {
        panic!()
    };
    assert_eq!(change, AppChange::Installed);
    assert_eq!(residue.len(), 1);
    assert!(
        residue[0]
            .join("mount/Koidrive.app/Contents/Info.plist")
            .exists()
    );
    assert!(f.base().join("mounted").exists());
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
async fn refused_quit_preserves_old_app_and_cleans_mount() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    f.flag("quit-fail");
    assert_eq!(f.run().await.unwrap_err().kind, AppInstallFailure::InUse);
    f.preserved();
    f.unmounted();
}

#[tokio::test]
async fn newer_user_copy_is_never_shadowed() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    let user = f.config.user.join(BUNDLE);
    bundle(&user, Some("1.0.0"));
    let AppInstall::Present {
        path,
        change,
        retired,
        ..
    } = f.run().await.unwrap()
    else {
        panic!()
    };
    assert_eq!(path, user);
    assert_eq!(change, AppChange::Unchanged);
    assert!(f.requests.lock().unwrap().is_empty());
    assert_eq!(
        fs::read(f.installed().join("Contents/Helpers/koi")).unwrap(),
        b"1.0.0"
    );
    assert_eq!(retired.len(), 1);
    assert_eq!(
        fs::read(retired[0].bundle.join("Contents/Helpers/koi")).unwrap(),
        b"0.1.0"
    );
    assert_eq!(
        fs::read(user.join("Contents/Helpers/koi")).unwrap(),
        b"1.0.0"
    );
}

#[tokio::test]
async fn unwritable_system_root_uses_user_applications() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    fs::set_permissions(&f.config.system, fs::Permissions::from_mode(0o500)).unwrap();
    let result = f.run().await;
    fs::set_permissions(&f.config.system, fs::Permissions::from_mode(0o700)).unwrap();
    let AppInstall::Present { path, change, .. } = result.unwrap() else {
        panic!()
    };
    assert_eq!(change, AppChange::Installed);
    assert_eq!(path, f.config.user.join(BUNDLE));
    assert!(!f.installed().exists());
    f.unmounted();
}

#[tokio::test]
async fn an_older_app_only_in_user_applications_is_upgraded_where_it_is() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    let user = f.config.user.join(BUNDLE);
    bundle(&user, Some("0.1.0"));
    let AppInstall::Present {
        path,
        version,
        retired,
        ..
    } = f.run().await.unwrap()
    else {
        panic!()
    };
    assert_eq!(path, user);
    assert_eq!(version.to_string(), TARGET);
    assert!(!f.installed().exists());
    assert!(retired.iter().all(|app| app.bundle != f.installed()));
    f.unmounted();
}

#[tokio::test]
async fn an_older_app_in_an_unwritable_user_applications_is_preserved_beside_a_new_system_app() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    let user = f.config.user.join(BUNDLE);
    bundle(&user, Some("0.1.0"));
    fs::set_permissions(&f.config.user, fs::Permissions::from_mode(0o500)).unwrap();
    let result = f.run().await;
    fs::set_permissions(&f.config.user, fs::Permissions::from_mode(0o700)).unwrap();
    let AppInstall::Present {
        path, preserved, ..
    } = result.unwrap()
    else {
        panic!()
    };
    assert_eq!(path, f.installed());
    assert_eq!(preserved, vec![user.clone()]);
    assert_eq!(
        fs::read(user.join("Contents/Helpers/koi")).unwrap(),
        b"0.1.0"
    );
    f.unmounted();
}

#[tokio::test]
async fn equal_copies_prefer_system_applications() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    bundle(&f.installed(), Some(TARGET));
    bundle(&f.config.user.join(BUNDLE), Some(TARGET));
    let AppInstall::Present { path, .. } = f.run().await.unwrap() else {
        panic!()
    };
    assert_eq!(path, f.installed());
    assert!(f.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn unchanged_app_needs_no_mount_cleanup_reserve() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    bundle(&f.installed(), Some(TARGET));
    let result = ensure_with(
        f.config.clone(),
        Version::parse(TARGET).unwrap(),
        AppLaunch::Preserve,
        Instant::now() + Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert!(matches!(
        result,
        AppInstall::Present {
            change: AppChange::Unchanged,
            ..
        }
    ));
    assert!(f.requests.lock().unwrap().is_empty());
    assert!(!f.config.work.exists());
}

#[tokio::test]
async fn staged_copy_is_verified_independently() {
    let mut f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    f.config.ditto = f.base().join("copy-tamper");
    script(
        &f.config.ditto,
        r#"
/usr/bin/ditto "$@"
for last do :; done
printf OTHERTEAM > "$last/Signer"
"#,
    );
    assert_eq!(
        f.run().await.unwrap_err().kind,
        AppInstallFailure::Verification
    );
    f.preserved();
    f.unmounted();
    assert_eq!(fs::read_dir(&f.config.system).unwrap().count(), 2);
}

#[tokio::test]
async fn unrelated_entries_are_never_overwritten() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    fs::write(f.installed(), "unrelated user data").unwrap();
    assert_eq!(
        f.run().await.unwrap_err().kind,
        AppInstallFailure::UnsafePath
    );
    assert_eq!(fs::read(f.installed()).unwrap(), b"unrelated user data");
    assert!(f.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn malformed_installed_version_is_a_conflict_without_network() {
    for value in [
        Some(plist::Value::String("newer-but-not-semver".into())),
        Some(plist::Value::String(String::new())),
        Some(plist::Value::Integer(3.into())),
    ] {
        let f = Fixture::new(None, DMG.to_vec(), 200).await;
        bundle(&f.installed(), None);
        let path = f.installed().join("Contents/Info.plist");
        if let Some(value) = value {
            let mut info = plist::Value::from_file(&path).unwrap();
            info.as_dictionary_mut()
                .unwrap()
                .insert("KoidriveVersion".into(), value);
            info.to_file_xml(&path).unwrap();
        }
        let before = fs::read(&path).unwrap();
        assert_eq!(f.run().await.unwrap_err().kind, AppInstallFailure::Conflict);
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(
            fs::read(f.installed().join("Contents/Helpers/koi")).unwrap(),
            b"development"
        );
        assert!(f.requests.lock().unwrap().is_empty());
        assert!(!f.config.work.exists());
        assert!(!f.base().join("hdi.log").exists());
        assert!(!f.base().join("quit.log").exists());
    }
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
async fn concurrent_installs_never_downgrade_the_app() {
    let older = Fixture::new(None, DMG.to_vec(), 200).await;
    let mut newer = Fixture::versioned("0.3.0", None, DMG.to_vec(), 200).await;
    newer.config.system = older.config.system.clone();
    newer.config.user = older.config.user.clone();
    newer.config.work = older.config.work.clone();
    let new = ensure_with(
        newer.config.clone(),
        Version::parse("0.3.0").unwrap(),
        AppLaunch::Preserve,
        Instant::now() + Duration::from_secs(60),
    );
    let (old, new) = tokio::join!(older.run(), new);
    old.unwrap();
    new.unwrap();
    assert_eq!(
        fs::read(older.installed().join("Contents/Helpers/koi")).unwrap(),
        b"0.3.0"
    );
    older.unmounted();
    newer.unmounted();
}

#[tokio::test]
async fn root_replacement_does_not_redirect_copy_cleanup_or_commit() {
    let mut f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    let outside = f.base().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("keep"), "unrelated data").unwrap();
    let moved = f.base().join("displaced");
    f.config.ditto = f.base().join("copy-redirect");
    script(
        &f.config.ditto,
        &format!(
            r#"
/usr/bin/ditto "$@"
/bin/mv '{}' '{}'
/bin/ln -s '{}' '{}'
"#,
            f.config.system.display(),
            moved.display(),
            outside.display(),
            f.config.system.display()
        ),
    );
    let error = f.run().await.unwrap_err();
    assert!(!error.residue.is_empty());
    assert_eq!(fs::read(outside.join("keep")).unwrap(), b"unrelated data");
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
    assert_eq!(
        fs::read(moved.join("Koidrive.app/Contents/Helpers/koi")).unwrap(),
        b"0.1.0"
    );
    assert!(!f.base().join("mounted").exists());
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

async fn pid_ready(path: PathBuf) -> i32 {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(pid) = fs::read_to_string(&path)
                .ok()
                .and_then(|text| text.trim().parse().ok())
            {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap()
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

async fn retired_fixture() -> (Fixture, RetiredApp) {
    let fixture = Fixture::new(None, DMG.to_vec(), 200).await;
    fixture.old();
    let AppInstall::Present { retired, .. } = fixture.run().await.unwrap() else {
        panic!()
    };
    (fixture, retired.into_iter().next().unwrap())
}

pub(crate) async fn pending_retirements_fixture() -> (impl Drop, PathBuf, Vec<RetiredApp>) {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    bundle(&f.config.user.join(BUNDLE), Some("0.1.0"));
    let path = f.installed();
    let AppInstall::Present {
        retired, residue, ..
    } = f.run().await.unwrap()
    else {
        panic!()
    };
    assert!(residue.is_empty());
    assert_eq!(retired.len(), 2);
    let mut paths: Vec<_> = retired.iter().map(|token| token.bundle.clone()).collect();
    drop(retired);
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
    assert!(residue.is_empty());
    assert_eq!(retired.len(), 2);
    let mut replayed: Vec<_> = retired.iter().map(|token| token.bundle.clone()).collect();
    paths.sort();
    replayed.sort();
    assert_eq!(paths, replayed);
    (f, path, retired)
}

#[tokio::test]
async fn owned_retired_completion_removes_only_the_retired_tree() {
    let (fixture, retired) = retired_fixture().await;
    let parent = retired.bundle.parent().unwrap().to_path_buf();
    let nested = retired.bundle.join("Contents/Versions/A");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("data"), "owned data").unwrap();
    symlink("A", nested.parent().unwrap().join("Current")).unwrap();
    retired.discard().unwrap();
    assert!(!parent.exists());
    assert_eq!(
        fs::read(fixture.installed().join("Contents/Helpers/koi")).unwrap(),
        TARGET.as_bytes()
    );
    fixture.unmounted();
}

#[tokio::test]
async fn owned_cleanup_unlinks_partial_and_cyclic_links_without_following_them() {
    let (fixture, retired) = retired_fixture().await;
    let contents = retired.bundle.join("Contents");
    symlink("missing", contents.join("dangling")).unwrap();
    symlink("second", contents.join("first")).unwrap();
    symlink("first", contents.join("second")).unwrap();
    symlink("../missing", contents.join("parent-relative")).unwrap();
    let parent = retired.bundle.parent().unwrap().to_path_buf();
    retired.discard().unwrap();
    assert!(!parent.exists());
    assert_eq!(
        fs::read(fixture.installed().join("Contents/Helpers/koi")).unwrap(),
        TARGET.as_bytes()
    );
    fixture.unmounted();
}

#[tokio::test]
async fn retired_completion_refuses_changed_or_symlinked_ownership() {
    for variant in [
        "root",
        "root-symlink",
        "parent",
        "parent-symlink",
        "bundle",
        "bundle-symlink",
        "field",
        "escape",
        "relative-escape",
        "extra-entry",
        "receipt-contents",
    ] {
        let (fixture, mut retired) = retired_fixture().await;
        let parent = retired.bundle.parent().unwrap().to_path_buf();
        let displaced = fixture.base().join("displaced");
        let outside = fixture.base().join("outside");
        bundle(&outside, Some("9.0.0"));
        match variant {
            "root" | "root-symlink" => {
                fs::rename(&fixture.config.system, &displaced).unwrap();
                if variant == "root" {
                    fs::create_dir(&fixture.config.system).unwrap();
                    fs::write(fixture.config.system.join("keep"), "unrelated").unwrap();
                } else {
                    symlink(&displaced, &fixture.config.system).unwrap();
                }
            }
            "parent" | "parent-symlink" => {
                fs::rename(&parent, &displaced).unwrap();
                if variant == "parent" {
                    fs::create_dir(&parent).unwrap();
                    bundle(&parent.join(BUNDLE), Some("9.0.0"));
                } else {
                    symlink(&displaced, &parent).unwrap();
                }
            }
            "bundle" | "bundle-symlink" => {
                fs::rename(&retired.bundle, &displaced).unwrap();
                if variant == "bundle" {
                    bundle(&retired.bundle, Some("9.0.0"));
                } else {
                    symlink(&outside, &retired.bundle).unwrap();
                }
            }
            "field" => retired.bundle = outside.clone(),
            "escape" => symlink(&outside, retired.bundle.join("escape")).unwrap(),
            "relative-escape" => {
                symlink("../../../outside", retired.bundle.join("escape")).unwrap();
            }
            "extra-entry" => {
                fs::write(parent.join("unowned"), "keep").unwrap();
            }
            "receipt-contents" => fs::write(retired.ownership.receipt.path(), b"{}").unwrap(),
            _ => unreachable!(),
        }
        let error = retired.discard().unwrap_err();
        assert!(
            matches!(
                error.kind,
                AppInstallFailure::UnsafePath | AppInstallFailure::ConcurrentChange
            ),
            "{variant}: {error:?}"
        );
        assert!(!error.residue.is_empty());
        assert_eq!(
            fs::read(outside.join("Contents/Helpers/koi")).unwrap(),
            b"9.0.0",
            "{variant}"
        );
        if matches!(variant, "parent" | "parent-symlink") {
            assert_eq!(
                fs::read(displaced.join("Koidrive.app/Contents/Helpers/koi")).unwrap(),
                b"0.1.0"
            );
        } else if matches!(variant, "bundle" | "bundle-symlink") {
            assert_eq!(
                fs::read(displaced.join("Contents/Helpers/koi")).unwrap(),
                b"0.1.0"
            );
        }
        if !matches!(variant, "root" | "root-symlink") {
            assert_eq!(
                fs::read(fixture.installed().join("Contents/Helpers/koi")).unwrap(),
                TARGET.as_bytes()
            );
        }
    }
}
