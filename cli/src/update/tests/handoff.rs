// SPDX-License-Identifier: Apache-2.0

#[tokio::test]
async fn the_installer_runs_install_sh_into_the_kois_directory_for_the_release() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("my bin");
    fs::create_dir_all(&bin).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!(
        "http://{}/install.sh",
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let served = tokio::spawn(serve_once(
        listener,
        "printf '%s|%s\\n' \"$KOIDRIVE_INSTALL_DIR\" \"$KOIDRIVE_VERSION\" > \"$KOIDRIVE_INSTALL_DIR/installed\"\necho 'Installed koi'\n",
    ));
    let version = Version::parse("0.1.2").unwrap();
    installer((url, false), &bin, &version, &mut echo(dir.path()))
        .await
        .unwrap();
    served.await.unwrap();
    assert_eq!(
        fs::read_to_string(bin.join("installed")).unwrap(),
        format!("{}|v0.1.2\n", bin.display())
    );
    let log = fs::read_to_string(dir.path().join("logs/update.log")).unwrap();
    assert!(log.contains("Installed koi\n"), "{log}");

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!(
        "http://{}/install.sh",
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let served = tokio::spawn(serve_once(
        listener,
        "echo 'checksum mismatch' >&2\nexit 3\n",
    ));
    let error = installer((url, false), &bin, &version, &mut echo(dir.path()))
        .await
        .unwrap_err();
    served.await.unwrap();
    assert_eq!(error.message, "install.sh failed (exit status 3)");
}

#[test]
fn development_daemons_do_not_hand_the_service_to_installed_releases() {
    let dir = tempfile::tempdir().unwrap();
    let koi = dir.path().join("koi");
    executable(
        &koi,
        "#!/bin/sh\necho probe >> \"$0.calls\"\necho 'koi 99.0.0'\n",
    );
    for version in ["0.0.0-dev", "0.0.0-dev+abc123"] {
        let mut seen = HashMap::new();
        assert_eq!(
            scan(
                &mut seen,
                std::slice::from_ref(&koi),
                &Version::parse(version).unwrap()
            ),
            None
        );
        assert!(seen.is_empty());
        assert!(!koi.with_extension("calls").exists());
    }
}

#[test]
fn the_daemon_hands_the_service_to_the_newest_koi_once_per_change() {
    let dir = tempfile::tempdir().unwrap();
    let koi = |name: &str, version: &str| {
        let path = dir.path().join(name).join("koi");
        executable(&path, &format!("#!/bin/sh\necho 'koi {version}'\n"));
        path
    };
    let brew = koi("brew", "0.1.2");
    let app = koi("app", "0.1.3");
    let script = koi("script", "0.1.0");
    let missing = dir.path().join("gone/koi");
    let candidates = [brew.clone(), app.clone(), script.clone(), missing];
    let runs = Version::parse("0.1.0").unwrap();
    let mut seen = HashMap::new();

    assert_eq!(scan(&mut seen, &candidates, &runs), Some(app.clone()));
    assert_eq!(
        scan(&mut seen, &candidates, &runs),
        Some(app.clone()),
        "retry the handoff until the daemon changes"
    );

    let later = |seconds| SystemTime::now() + Duration::from_secs(seconds);
    executable(&brew, "#!/bin/sh\necho 'koi 0.2.0'\n");
    File::open(&brew).unwrap().set_modified(later(60)).unwrap();
    assert_eq!(scan(&mut seen, &candidates, &runs), Some(brew.clone()));

    executable(&script, "#!/bin/sh\necho 'koi 0.0.9'\n");
    File::open(&script)
        .unwrap()
        .set_modified(later(120))
        .unwrap();
    assert_eq!(
        scan(&mut seen, &candidates, &runs),
        Some(brew.clone()),
        "an older koi does not hide the cached newer install"
    );
    assert_eq!(
        scan(
            &mut HashMap::new(),
            &candidates,
            &Version::parse("0.3.0").unwrap()
        ),
        None
    );
}

#[test]
fn handoff_retries_cached_versions_without_running_unchanged_binaries_again() {
    let dir = tempfile::tempdir().unwrap();
    let koi = dir.path().join("koi");
    executable(
        &koi,
        "#!/bin/sh\necho probe >> \"$0.calls\"\necho 'koi 0.2.0'\n",
    );
    let mut seen = HashMap::new();
    let candidates = [koi.clone()];
    for _ in 0..3 {
        assert_eq!(
            scan(&mut seen, &candidates, &Version::parse("0.1.0").unwrap()),
            Some(koi.clone())
        );
    }
    assert_eq!(
        scan(&mut seen, &candidates, &Version::parse("0.2.0").unwrap()),
        None
    );
    assert_eq!(
        fs::read_to_string(koi.with_extension("calls")).unwrap(),
        "probe\n"
    );
}
