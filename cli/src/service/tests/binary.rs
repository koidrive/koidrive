// SPDX-License-Identifier: Apache-2.0

#[tokio::test]
async fn a_moved_binary_restarts_the_running_service_with_the_new_unit() {
    let fake = Fake::default();
    converge(&fake, &service(&[]), 1, false, false, &Daemon::Absent).await;
    let home = "/home/ada";
    let binary = Path::new("/usr/local/bin/koi");
    let moved = Service::new(&paths(home, &[]), Path::new(home), binary).unwrap();
    converge(&fake, &moved, 1, false, false, &Daemon::Absent).await;
    assert_eq!(fake.restarts.get(), 1);
    assert!(
        fake.unit
            .borrow()
            .as_ref()
            .unwrap()
            .contains("/usr/local/bin/koi")
    );
}

#[test]
fn a_homebrew_koi_runs_the_service_from_the_link_that_outlives_brew_upgrade() {
    let cases = [
        ("/Users/ada", "/opt/homebrew"),
        ("/Users/ada", "/usr/local"),
        ("/home/ada", "/home/linuxbrew/.linuxbrew"),
    ];
    for (home, prefix) in cases {
        let keg = format!("{prefix}/Cellar/koidrive/0.1.0/bin/koi");
        let service = Service::new(&paths(home, &[]), Path::new(home), Path::new(&keg)).unwrap();
        let link = format!("{prefix}/opt/koidrive/bin/koi");
        assert!(
            service
                .unit()
                .contains(&format!("ExecStart=\"{link}\" _daemon\n")),
            "{}",
            service.unit()
        );
        assert!(service.plist().contains(&format!(
            "<string>{link}</string>\n\t\t<string>_daemon</string>"
        )));
        assert_eq!(unit_binary(&service.unit()).as_deref(), Some(link.as_str()));
    }
    let own = Service::new(
        &paths("/home/ada", &[]),
        Path::new("/home/ada"),
        Path::new("/srv/Cellar/koi"),
    );
    assert!(
        own.unwrap()
            .unit()
            .contains("ExecStart=\"/srv/Cellar/koi\" _daemon\n")
    );
}

#[test]
fn the_newest_binary_runs_the_service_by_semver_precedence() {
    let order = [
        "0.1.0-alpha.2",
        "0.1.0-alpha.10",
        "0.1.0-rc.1",
        "0.1.0",
        "0.1.1",
    ]
    .map(version);
    for (older, newer) in order.iter().zip(&order[1..]) {
        let keeps = |installed: &Version, caller: &Version| {
            target(Some(installed), caller, &Daemon::Absent).keep
        };
        assert!(keeps(newer, older), "{newer} over {older}");
        assert!(!keeps(older, newer), "{newer} over {older}");
        assert!(
            keeps(newer, newer),
            "the same version keeps the service as it is"
        );
    }
    let caller = version("0.1.0-alpha.2");
    assert!(
        target(
            Some(&version("0.1.0-alpha.2+build.7")),
            &caller,
            &Daemon::Absent
        )
        .keep
    );
    assert!(!target(None, &caller, &Daemon::Absent).keep);
}

#[test]
fn a_running_daemon_older_than_the_binary_the_service_runs_or_without_a_version_restarts() {
    let restarts = |installed: Option<&str>, caller: &str, daemon: Daemon| {
        target(installed.map(version).as_ref(), &version(caller), &daemon).restart
    };
    let running = |text: &str| Daemon::Running(Some(version(text)));
    assert!(restarts(None, "0.1.0", running("0.1.0-rc.1")));
    assert!(!restarts(None, "0.1.0", running("0.1.0")));
    assert!(!restarts(None, "0.1.0", running("0.1.1")));
    assert!(restarts(None, "0.1.0", Daemon::Running(None)));
    assert!(!restarts(None, "0.1.0", Daemon::Absent));
    assert!(restarts(Some("0.1.1"), "0.1.0", running("0.1.0")));
    assert!(!restarts(Some("0.1.1"), "0.1.0", running("0.1.1")));
    assert!(!restarts(
        Some("0.1.0-alpha.2"),
        "0.1.0-alpha.10",
        running("0.1.0-alpha.10")
    ));
    assert!(restarts(
        Some("0.1.0-alpha.2"),
        "0.1.0-alpha.10",
        running("0.1.0-alpha.2")
    ));
}

#[test]
fn the_binary_a_unit_or_a_launch_agent_runs_reads_back_as_written() {
    let home = "/home/ada";
    for binary in [
        "/home/ada/.local/bin/koi",
        "/opt/my $bin/100% \"a\\b\"/<k&i>",
    ] {
        let service = Service::new(&paths(home, &[]), Path::new(home), Path::new(binary)).unwrap();
        assert_eq!(unit_binary(&service.unit()).as_deref(), Some(binary));
        assert_eq!(plist_binary(&service.plist()).as_deref(), Some(binary));
    }
    assert_eq!(unit_binary("[Service]\n"), None);
}

#[test]
fn mounted_and_translocated_binaries_are_volatile_but_installed_and_development_paths_are_not() {
    for binary in [
        "/Volumes/Koidrive/Koidrive.app/Contents/Helpers/koi",
        "/var/folders/ab/cd/T/AppTranslocation/123/d/Koidrive.app/Contents/Helpers/koi",
        "/private/var/folders/ab/cd/T/AppTranslocation/123/d/Koidrive.app/Contents/Helpers/koi",
    ] {
        assert!(volatile_binary(Path::new(binary)), "{binary}");
    }
    for binary in [
        "/Applications/Koidrive.app/Contents/Helpers/koi",
        "/Users/ada/Applications/Koidrive.app/Contents/Helpers/koi",
        "/Users/ada/koidrive/target/debug/koi",
        "/Users/ada/Volumes/Koidrive.app/Contents/Helpers/koi",
        "/Volumes-backup/Koidrive.app/Contents/Helpers/koi",
        "/Applications/AppTranslocation.app/Contents/Helpers/koi",
    ] {
        assert!(!volatile_binary(Path::new(binary)), "{binary}");
    }

    let dir = tempfile::tempdir().unwrap();
    let translocated = dir.path().join("AppTranslocation/koi");
    fs::create_dir_all(translocated.parent().unwrap()).unwrap();
    fs::write(&translocated, "").unwrap();
    let alias = dir.path().join("koi");
    std::os::unix::fs::symlink(&translocated, &alias).unwrap();
    assert!(volatile_binary(&alias));
}

#[tokio::test]
async fn a_volatile_helper_cannot_install_a_service_or_replace_one_with_another_volatile_path() {
    let caller = Service {
        binary: "/Volumes/Koidrive/Koidrive.app/Contents/Helpers/koi".into(),
        ..service(&[])
    };
    let fake = Fake::default();
    let report = converge(&fake, &caller, 1, false, true, &Daemon::Absent).await;
    assert_eq!(report.state, State::NotInstalled);
    assert_eq!(report.problem.as_deref(), Some(MOVE_TO_APPLICATIONS));
    assert!(report.text().contains("Move Koidrive to Applications"));
    assert!(fake.unit.borrow().is_none());

    running_on(
        &fake,
        Path::new("/Volumes/Older/Koidrive.app/Contents/Helpers/koi"),
    );
    let before = fake.unit.borrow().clone();
    let report = converge(&fake, &caller, 1, false, true, &Daemon::Absent).await;
    assert_eq!(report.problem.as_deref(), Some(MOVE_TO_APPLICATIONS));
    assert_eq!(*fake.unit.borrow(), before);
    assert_eq!(fake.restarts.get(), 0);
}

#[tokio::test]
async fn a_volatile_helper_keeps_an_older_stable_service_and_can_start_it_without_repointing_it() {
    let dir = tempfile::tempdir().unwrap();
    let stable = koi_printing(dir.path(), "koi", "koi 0.0.1\n", 0);
    let fake = Fake::default();
    running_on(&fake, &stable);
    let before = fake.unit.borrow().clone();
    for binary in [
        "/Volumes/Koidrive/Koidrive.app/Contents/Helpers/koi",
        "/private/var/folders/ab/cd/T/AppTranslocation/123/d/Koidrive.app/Contents/Helpers/koi",
    ] {
        let caller = Service {
            binary: binary.into(),
            ..service(&[])
        };
        let daemon = Daemon::Running(Some(version("0.0.1")));
        for _ in 0..2 {
            let report = converge(&fake, &caller, 1, false, false, &daemon).await;
            assert_eq!(report.state, State::Running);
            assert!(report.problem.is_none());
            assert_eq!(*fake.unit.borrow(), before);
        }
        assert_eq!(fake.restarts.get(), 0);

        fake.running.set(false);
        fake.switched_off.set(true);
        let left = converge(&fake, &caller, 1, false, false, &Daemon::Absent).await;
        assert_eq!(left.state, State::SwitchedOff);
        let started = converge(&fake, &caller, 1, false, true, &Daemon::Absent).await;
        assert_eq!((started.state, started.started), (State::Running, true));
        assert!(started.problem.is_none());
        assert_eq!(*fake.unit.borrow(), before);
    }
}

#[tokio::test]
async fn a_stable_helper_replaces_a_volatile_service_even_when_it_reports_a_newer_version() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("AppTranslocation")).unwrap();
    let volatile = koi_printing(dir.path(), "AppTranslocation/koi", "koi 99.0.0\n", 0);
    let fake = Fake::default();
    running_on(&fake, &volatile);
    let caller = service(&[]);
    assert_eq!(newer_installed(&fake, &caller), None);
    let report = converge(&fake, &caller, 1, false, false, &Daemon::Absent).await;
    assert_eq!(report.state, State::Running);
    assert!(report.problem.is_none());
    assert_eq!(runs(&fake), caller.binary);
    assert_eq!(fake.restarts.get(), 1);
}

#[tokio::test]
async fn a_service_on_a_newer_koi_keeps_it_and_one_on_an_older_or_broken_koi_moves_to_this_one() {
    let dir = tempfile::tempdir().unwrap();
    let caller = service(&[]);
    let this = this_version();
    let newer = koi_printing(dir.path(), "newer", "koi 99.0.0\n", 0);
    let same = koi_printing(dir.path(), "same", &format!("koi {this}\n"), 0);
    let fake = Fake::default();
    for (kept, running) in [(&newer, version("99.0.0")), (&same, this)] {
        running_on(&fake, kept);
        let report = converge(
            &fake,
            &caller,
            1,
            false,
            false,
            &Daemon::Running(Some(running)),
        )
        .await;
        assert_eq!(report.state, State::Running);
        assert_eq!(runs(&fake), kept.to_str().unwrap());
    }
    assert_eq!(fake.restarts.get(), 0);

    let not_executable = dir.path().join("not-executable");
    fs::write(&not_executable, "#!/bin/sh\necho 'koi 99.0.0'\n").unwrap();
    for moved in [
        koi_printing(dir.path(), "older", "koi 0.0.0-0\n", 0),
        koi_printing(dir.path(), "failing", "koi 99.0.0\n", 1),
        koi_printing(dir.path(), "other", "something 99.0.0\n", 0),
        koi_printing(dir.path(), "unparsable", "koi 99\n", 0),
        not_executable,
        dir.path().join("missing"),
    ] {
        running_on(&fake, &moved);
        let restarts = fake.restarts.get();
        converge(&fake, &caller, 1, false, false, &Daemon::Absent).await;
        assert_eq!(
            runs(&fake),
            "/home/ada/.local/bin/koi",
            "{}",
            moved.display()
        );
        assert_eq!(fake.restarts.get(), restarts + 1, "{}", moved.display());
    }
}

#[tokio::test]
async fn a_service_whose_daemon_is_older_than_its_binary_restarts_once_and_a_stopped_one_only_starts()
 {
    let fake = Fake::default();
    let caller = service(&[]);
    let this = this_version();
    converge(&fake, &caller, 1, false, false, &Daemon::Absent).await;
    let older = Daemon::Running(Some(version("0.0.0-0")));
    converge(&fake, &caller, 1, false, false, &older).await;
    assert_eq!(fake.restarts.get(), 1);
    converge(&fake, &caller, 1, false, false, &Daemon::Running(None)).await;
    assert_eq!(fake.restarts.get(), 2);
    converge(
        &fake,
        &caller,
        1,
        false,
        false,
        &Daemon::Running(Some(this)),
    )
    .await;
    assert_eq!(fake.restarts.get(), 2);
    fake.running.set(false);
    let started = converge(&fake, &caller, 1, false, false, &older).await;
    assert_eq!((started.state, started.started), (State::Running, true));
    assert_eq!(fake.restarts.get(), 2);
}

#[tokio::test]
async fn a_daemon_too_old_to_report_a_version_restarts_once_and_then_reports_the_binarys() {
    let dir = tempfile::tempdir().unwrap();
    let this = this_version();
    let same = koi_printing(dir.path(), "same", &format!("koi {this}\n"), 0);
    let fake = Fake::default();
    running_on(&fake, &same);
    converge(
        &fake,
        &service(&[]),
        1,
        false,
        false,
        &Daemon::Running(None),
    )
    .await;
    for _ in 0..3 {
        let restarted = Daemon::Running(Some(this.clone()));
        converge(&fake, &service(&[]), 1, false, false, &restarted).await;
    }
    assert_eq!(runs(&fake), same.to_str().unwrap());
    assert_eq!(fake.restarts.get(), 1);
}
