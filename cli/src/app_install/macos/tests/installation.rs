use super::*;

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
async fn a_warning_from_hdiutil_does_not_break_reading_its_disk_image_list() {
    let f = Fixture::new(None, DMG.to_vec(), 200).await;
    f.old();
    f.flag("info-warn");
    let AppInstall::Present { change, .. } = f.run().await.unwrap() else {
        panic!("the app should be installed");
    };
    assert_eq!(change, AppChange::Replaced);
    f.unmounted();
}
