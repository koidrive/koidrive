use super::*;

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
