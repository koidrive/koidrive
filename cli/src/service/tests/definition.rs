// SPDX-License-Identifier: Apache-2.0

#[test]
fn the_profile_of_an_installed_service_reads_back_from_its_plist_and_its_unit() {
    let home = Path::new("/Users/ada");
    let state = "/srv/a <b> & \"c\" 100%";
    let paths = Paths::from_vars(|name| match name {
        "HOME" => Some(home.into()),
        "XDG_STATE_HOME" => Some(state.into()),
        _ => None,
    })
    .unwrap();
    let service = Service::new(&paths, home, &home.join(".local/bin/koi")).unwrap();
    let expected = vec![
        ("XDG_CONFIG_HOME", Some("/Users/ada/.config".to_owned())),
        ("XDG_STATE_HOME", Some(state.to_owned())),
        ("XDG_RUNTIME_DIR", None),
    ];
    assert_eq!(plist_env(&service.plist()), expected);
    assert_eq!(unit_env(&service.unit()), expected);
    assert!(suffixed(
        &format!("{}.plist", service.label()),
        "dev.koidrive.koi",
        ".plist"
    ));
    assert!(suffixed(&service.unit_name(), "koidrive", ".service"));
    assert!(suffixed(
        "dev.koidrive.koi.plist",
        "dev.koidrive.koi",
        ".plist"
    ));
    assert!(suffixed("koidrive.service", "koidrive", ".service"));
    for other in [
        "dev.koidrive.koi.other.plist",
        "dev.koidrive.koix.plist",
        "dev.koidrive.koi.1234567.plist",
        "dev.koidrive.koi.ABCDEF12.plist",
    ] {
        assert!(!suffixed(other, "dev.koidrive.koi", ".plist"), "{other}");
    }
}

#[test]
fn the_default_profile_has_the_plain_names_and_any_other_one_names_of_its_own() {
    let default = service(&[("XDG_RUNTIME_DIR", "/run/user/1000")]);
    assert_eq!(default.unit_name(), "koidrive.service");
    assert_eq!(default.label(), "dev.koidrive.koi");
    let other = service(&[("XDG_STATE_HOME", "/srv/ada/state")]);
    let third = service(&[("XDG_STATE_HOME", "/srv/bob/state")]);
    assert!(
        other.unit_name().starts_with("koidrive-"),
        "{}",
        other.unit_name()
    );
    assert!(
        other.label().starts_with("dev.koidrive.koi."),
        "{}",
        other.label()
    );
    assert_ne!(other.unit_name(), third.unit_name());
    assert_eq!(other, service(&[("XDG_STATE_HOME", "/srv/ada/state")]));
}

#[test]
fn the_unit_runs_this_binary_with_the_profiles_directories() {
    let unit = service(&[("XDG_RUNTIME_DIR", "/run/user/1000")]).unit();
    let lines: Vec<&str> = unit.lines().filter(|line| !line.starts_with('#')).collect();
    for expected in [
        "ExecStart=\"/home/ada/.local/bin/koi\" _daemon",
        "Environment=\"XDG_CONFIG_HOME=/home/ada/.config\"",
        "Environment=\"XDG_STATE_HOME=/home/ada/.local/state\"",
        "Environment=\"XDG_RUNTIME_DIR=/run/user/1000\"",
        "Restart=on-failure",
        "WantedBy=default.target",
    ] {
        assert!(lines.contains(&expected), "{expected} in {unit}");
    }
    let section = lines.iter().position(|line| *line == "[Service]").unwrap();
    let environment = (lines.iter())
        .position(|line| line.starts_with("Environment=\"XDG"))
        .unwrap();
    assert!(environment > section, "{unit}");
}

#[test]
fn a_profile_without_a_runtime_directory_keeps_systemds_out_of_the_daemon() {
    let unit = service(&[]).unit();
    assert!(
        unit.lines()
            .any(|line| line == "UnsetEnvironment=XDG_RUNTIME_DIR"),
        "{unit}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn the_launch_agent_opens_this_profile_even_when_launchd_sets_a_runtime_directory() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("agent.plist");
    fs::write(&file, service(&[]).plist()).unwrap();
    let json = Command::new("plutil")
        .args(["-convert", "json", "-o", "-"])
        .arg(&file)
        .output()
        .unwrap();
    assert!(json.status.success());
    let plist: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    let mut env = std::collections::BTreeMap::from([
        ("HOME".to_owned(), "/home/ada".to_owned()),
        (
            "XDG_RUNTIME_DIR".to_owned(),
            "/inherited/runtime".to_owned(),
        ),
    ]);
    for (name, value) in plist["EnvironmentVariables"].as_object().unwrap() {
        env.insert(name.clone(), value.as_str().unwrap().to_owned());
    }
    let opened = Paths::from_vars(|name| env.get(name).map(Into::into)).unwrap();
    assert_eq!(opened, paths("/home/ada", &[]));
}

#[test]
fn quotes_specifiers_and_dollars_reach_the_daemon_as_written() {
    let home = "/home/ada";
    let paths = paths(home, &[("XDG_STATE_HOME", "/data/100% \"sure\"")]);
    let service = Service::new(&paths, Path::new(home), Path::new("/opt/my $bin/koi")).unwrap();
    let unit = service.unit();
    assert!(
        unit.contains("ExecStart=\"/opt/my $bin/koi\" _daemon\n"),
        "{unit}"
    );
    assert!(
        unit.contains("Environment=\"XDG_STATE_HOME=/data/100%% \\\"sure\\\"\"\n"),
        "{unit}"
    );
    assert!(
        service
            .plist()
            .contains("<string>/opt/my $bin/koi</string>")
    );
}

#[test]
fn paths_a_service_file_cannot_carry_are_refused() {
    let home = "/home/ada";
    let binary = Path::new("/opt/koi\n[Service]");
    let refused = Service::new(&paths(home, &[]), Path::new(home), binary).unwrap_err();
    assert!(refused.contains("cannot be written"), "{refused}");
}

#[test]
fn the_launch_agent_starts_at_login_stays_alive_and_logs_to_the_library() {
    let plist = service(&[("XDG_STATE_HOME", "/Users/ada/a&b")]).plist();
    for expected in [
        "<key>RunAtLoad</key>\n\t<true/>",
        "<key>KeepAlive</key>\n\t<true/>",
        "<string>/home/ada/.local/bin/koi</string>\n\t\t<string>_daemon</string>",
        "<key>XDG_STATE_HOME</key>\n\t\t<string>/Users/ada/a&amp;b</string>",
        "<string>/home/ada/Library/Logs/Koidrive/dev.koidrive.koi.",
    ] {
        assert!(plist.contains(expected), "{expected} in {plist}");
    }
}

#[cfg(target_os = "macos")]
#[test]
fn plutil_accepts_the_launch_agent() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("agent.plist");
    fs::write(
        &file,
        service(&[("XDG_STATE_HOME", "/Users/a <b>")]).plist(),
    )
    .unwrap();
    let lint = Command::new("plutil")
        .arg("-lint")
        .arg(&file)
        .output()
        .unwrap();
    assert!(
        lint.status.success(),
        "{}",
        String::from_utf8_lossy(&lint.stdout)
    );
}
