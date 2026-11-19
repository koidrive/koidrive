use super::*;

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
        "Creating the fixture disk image",
        Duration::from_secs(60),
        Path::new("/usr/bin/hdiutil"),
        &[
            Arg::from("create"),
            Arg::from("-srcfolder"),
            passed(&payload).unwrap(),
            Arg::from("-volname"),
            Arg::from("Koidrive installer test"),
            Arg::from("-format"),
            Arg::from("UDZO"),
            passed(&image).unwrap(),
        ],
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
        "Listing open disk images",
        Duration::from_secs(60),
        Path::new("/usr/bin/hdiutil"),
        &[Arg::from("info"), Arg::from("-plist")],
        &control,
    )
    .await
    .unwrap();
    let plist = plist::Value::from_reader(std::io::Cursor::new(inventory.stdout)).unwrap();
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
