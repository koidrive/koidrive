use super::*;

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
        &[
            Arg::from("--display"),
            Arg::from("--verbose=4"),
            passed(&executable).unwrap(),
        ],
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
            &signature_args(&executable, &requirement, deep).unwrap(),
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
            &signature_args(&executable, requirement, deep).unwrap(),
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
            &signature_args(path, claimed, deep).unwrap(),
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
        &signature_args(&helper, SIGNER, false).unwrap(),
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
            &signature_args(path, "anchor apple", deep).unwrap(),
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
        &signature_args(helper, SIGNER, false).unwrap(),
        &control,
    )
    .await
    .unwrap_err();
    assert!(error.message.contains("failed to satisfy"), "{error:?}");
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
