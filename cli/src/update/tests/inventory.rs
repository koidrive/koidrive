// SPDX-License-Identifier: Apache-2.0

#[test]
fn the_running_binary_determines_its_installation_method() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("standalone/koi");
    let app = dir
        .path()
        .join("Applications/Koidrive.app/Contents/Helpers/koi");
    let custom = dir
        .path()
        .join("Custom Apps/Koidrive.app/Contents/Helpers/koi");
    let formula = dir.path().join("brew/Cellar/koidrive/0.1.0/bin/koi");
    executable(&script, "#!/bin/sh\nexit 0\n");
    executable(&app, "#!/bin/sh\nexit 0\n");
    let script_command = format!("{} update", fs::canonicalize(&script).unwrap().display());
    let app_command = format!("{} update", fs::canonicalize(&app).unwrap().display());
    let formula_brew = fs::canonicalize(dir.path()).unwrap().join("brew/bin/brew");
    let formula_command = format!("{} upgrade koidrive", formula_brew.display());
    for (binary, method, command) in [
        (&script, Method::Installer, Some(script_command.as_str())),
        (&app, Method::App, Some(app_command.as_str())),
        (&custom, Method::AppDownload, None),
        (
            &formula,
            Method::Formula { brew: formula_brew },
            Some(formula_command.as_str()),
        ),
    ] {
        executable(binary, "#!/bin/sh\nexit 0\n");
        let install = Install::resolve(binary, "0.1.0", &[], std::slice::from_ref(&app)).unwrap();
        assert_eq!(install.method, method);
        assert_eq!(install.binary, fs::canonicalize(binary).unwrap());
        assert_eq!(
            install.command().map(|args| args.join(" ")).as_deref(),
            command
        );
        let available = update::Update::Available {
            latest: "0.2.0".into(),
            url: "https://github.com/koidrive/koidrive/releases/tag/v0.2.0".into(),
            checked_at_ms: 1,
        };
        assert_eq!(
            install.notice(&available, "0.1.0").command.as_deref(),
            command
        );
        let dev = Install::resolve(binary, "0.0.0-dev", &[], std::slice::from_ref(&app)).unwrap();
        assert_eq!(dev.method, Method::Development);
        assert_eq!(dev.command(), None);
        assert_eq!(dev.method.via(), None);
        assert_eq!(
            dev.notice(&available, "0.0.0-dev").state,
            crate::report::UpdateState::Off
        );
    }
    assert_eq!(
        Installs::under(Some(Path::new("/opt/homebrew")), None)
            .homebrew
            .len(),
        3
    );
}

#[test]
fn an_app_update_notice_names_the_resolved_helper() {
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("My Apps/Koidrive.app/Contents/Helpers/koi");
    executable(&binary, "#!/bin/sh\nexit 0\n");
    let alias = dir.path().join("alias");
    std::os::unix::fs::symlink(&binary, &alias).unwrap();
    let install = Install::resolve(&alias, "0.1.0", &[], std::slice::from_ref(&binary)).unwrap();
    let available = update::Update::Available {
        latest: "0.2.0".into(),
        url: "https://github.com/koidrive/koidrive/releases/tag/v0.2.0".into(),
        checked_at_ms: 1,
    };
    let resolved = fs::canonicalize(binary).unwrap();
    assert_eq!(
        install.notice(&available, "0.1.0").command,
        Some(format!(
            "{} update",
            crate::pairing::shell_word(resolved.to_str().unwrap())
        ))
    );
}

#[test]
fn a_homebrew_keg_maps_to_its_opt_link_under_every_prefix() {
    for prefix in [
        "/opt/homebrew",
        "/usr/local",
        "/home/linuxbrew/.linuxbrew",
        "/srv/brew",
    ] {
        let opt = Path::new(prefix).join("opt/koidrive/bin/koi");
        assert_eq!(homebrew_link(&opt), Some(opt.clone()));
        for version in ["0.1.0", "0.1.0-alpha.3", "0.1.2_1"] {
            let keg = format!("{prefix}/Cellar/koidrive/{version}/bin/koi");
            assert_eq!(
                homebrew_link(Path::new(&keg)),
                Some(Path::new(prefix).join("opt/koidrive/bin/koi")),
                "{keg}"
            );
        }
    }
    for other in [
        "/opt/homebrew/bin/koi",
        "/opt/homebrew/Cellar/other/0.1.0/bin/koi",
        "/opt/homebrew/Cellar/koidrive/0.1.0/libexec/koi",
        "/opt/homebrew/Cellar/koidrive/0.1.0/bin/koi2",
        "/home/ada/.local/bin/koi",
    ] {
        assert_eq!(homebrew_link(Path::new(other)), None, "{other}");
    }
}

#[test]
fn koi_update_refuses_to_run_as_root() {
    let refused = refuse_root(true).unwrap_err();
    assert_eq!(refused.code, Code::Usage);
    assert!(
        refused.message.contains("run it as your own user"),
        "{}",
        refused.message
    );
    assert!(refuse_root(false).is_ok());
}

#[test]
fn user_applications_are_discovered_without_treating_their_helper_as_install_sh() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let user = home.join("Applications/Koidrive.app/Contents/Helpers/koi");
    executable(&user, "#!/bin/sh\nexit 0\n");
    let installs = Installs::under(None, Some(home));
    assert!(installs.all().contains(&user));
    let alias = home.join("koi");
    std::os::unix::fs::symlink(&user, &alias).unwrap();
    for binary in [&user, &alias] {
        let install = Install::resolve(binary, "0.1.0", &[], &installs.apps).unwrap();
        assert_eq!(install.method, Method::App);
        assert_eq!(install.binary, fs::canonicalize(&user).unwrap());
    }
    assert_eq!(
        installs.all().iter().filter(|path| **path == user).count(),
        1
    );
    let no_home = Installs::under(None, None);
    assert_eq!(no_home.apps.len(), 1);
    assert!(no_home.apps[0].is_absolute());
}

#[test]
fn a_cask_helper_requires_a_matching_homebrew_registration() {
    let dir = tempfile::tempdir().unwrap();
    let prefix = dir.path().join("brew");
    let app = dir.path().join("custom apps/Koidrive.app");
    let helper = app.join("Contents/Helpers/koi");
    executable(&helper, "#!/bin/sh\nexit 0\n");
    let prefixes = [prefix.clone()];
    let classify = || Install::resolve(&helper, "0.1.0", &prefixes, &[]).unwrap();
    assert_eq!(classify().method, Method::AppDownload);
    assert_eq!(classify().method.via(), Some(Via::App));
    assert_eq!(classify().command(), None);
    assert_eq!(
        Install::resolve(&helper, "0.1.0", &prefixes, std::slice::from_ref(&helper))
            .unwrap()
            .method,
        Method::App
    );
    let registration = prefix.join("Caskroom/koidrive/0.1.0/Koidrive.app");
    let other = dir.path().join("other/Koidrive.app");
    executable(&other.join("Contents/Helpers/koi"), "#!/bin/sh\nexit 0\n");
    fs::create_dir_all(registration.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&other, &registration).unwrap();
    assert_eq!(classify().method, Method::AppDownload);
    fs::remove_file(&registration).unwrap();
    std::os::unix::fs::symlink(&app, &registration).unwrap();
    let elsewhere = classify();
    let brew = fs::canonicalize(&prefix).unwrap().join("bin/brew");
    let upgrade = [brew.to_str().unwrap(), "upgrade", "--cask", "koidrive"];
    assert_eq!(
        elsewhere.method,
        Method::CaskElsewhere { brew: brew.clone() }
    );
    assert_eq!(elsewhere.method.via(), Some(Via::Homebrew));
    assert_eq!(elsewhere.command().as_deref(), Some(upgrade.as_slice()));
    assert_eq!(elsewhere.fallback().as_deref(), Some(upgrade.as_slice()));
    let available = update::Update::Available {
        latest: "0.2.0".into(),
        url: "https://github.com/koidrive/koidrive/releases/tag/v0.2.0".into(),
        checked_at_ms: 1,
    };
    assert_eq!(
        elsewhere.notice(&available, "0.1.0").command.as_deref(),
        Some(format!("{} upgrade --cask koidrive", brew.display()).as_str())
    );
    assert!(!prefix.join("bin/koi").exists());
}

#[test]
fn a_cask_in_an_applications_folder_updates_itself_and_names_brew_only_as_its_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let prefix = dir.path().join("brew");
    let app = dir.path().join("Applications/Koidrive.app");
    let helper = app.join("Contents/Helpers/koi");
    executable(&helper, "#!/bin/sh\nexit 0\n");
    let registration = prefix.join("Caskroom/koidrive/0.1.0/Koidrive.app");
    fs::create_dir_all(registration.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&app, &registration).unwrap();
    let link = prefix.join("bin/koi");
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&helper, &link).unwrap();
    let cask = Install::resolve(
        &link,
        "0.1.0",
        std::slice::from_ref(&prefix),
        std::slice::from_ref(&helper),
    )
    .unwrap();
    let brew = fs::canonicalize(&prefix).unwrap().join("bin/brew");
    let resolved = fs::canonicalize(&helper).unwrap();
    assert_eq!(cask.method, Method::Cask { brew: brew.clone() });
    assert_eq!(cask.binary, resolved);
    assert_eq!(cask.method.via(), Some(Via::App));
    assert_eq!(
        cask.command().as_deref(),
        Some([resolved.to_str().unwrap(), "update"].as_slice())
    );
    assert_eq!(
        cask.fallback().as_deref(),
        Some([brew.to_str().unwrap(), "upgrade", "--cask", "koidrive"].as_slice())
    );
    let available = update::Update::Available {
        latest: "0.2.0".into(),
        url: "https://github.com/koidrive/koidrive/releases/tag/v0.2.0".into(),
        checked_at_ms: 1,
    };
    assert_eq!(
        cask.notice(&available, "0.1.0").command,
        Some(command_text(&[resolved.to_str().unwrap(), "update"]))
    );
    let app_only = Install::resolve(&helper, "0.1.0", &[], std::slice::from_ref(&helper)).unwrap();
    assert_eq!(app_only.method, Method::App);
    assert_eq!(app_only.fallback(), None);
}

#[test]
fn cask_and_installer_aliases_resolve_without_changing_their_links() {
    let dir = tempfile::tempdir().unwrap();
    let prefix = dir.path().join("custombrew");
    let app = dir.path().join("custom apps/Koidrive.app");
    let helper = app.join("Contents/Helpers/koi");
    executable(&helper, "#!/bin/sh\nexit 0\n");
    let link = prefix.join("bin/koi");
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&helper, &link).unwrap();
    assert_eq!(
        Install::resolve(&link, "0.1.0", &[], &[]).unwrap().method,
        Method::AppDownload
    );
    let registration = prefix.join("Caskroom/koidrive/0.1.0/Koidrive.app");
    fs::create_dir_all(registration.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&app, registration).unwrap();
    let cask = Install::resolve(&link, "0.1.0", &[], &[]).unwrap();
    assert_eq!(
        cask.method,
        Method::CaskElsewhere {
            brew: fs::canonicalize(&prefix).unwrap().join("bin/brew")
        }
    );
    assert_eq!(cask.binary, fs::canonicalize(&helper).unwrap());
    let other_prefix = dir.path().join("otherbrew");
    let other_registration = other_prefix.join("Caskroom/koidrive/0.1.0/Koidrive.app");
    fs::create_dir_all(other_registration.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&app, other_registration).unwrap();
    assert_eq!(
        Install::resolve(&link, "0.1.0", &[other_prefix], &[])
            .unwrap()
            .method,
        cask.method,
    );
    let script = dir.path().join("standalone/koi");
    executable(&script, "#!/bin/sh\nexit 0\n");
    let alias = dir.path().join("alias");
    std::os::unix::fs::symlink("standalone/koi", &alias).unwrap();
    let second = dir.path().join("second");
    std::os::unix::fs::symlink("alias", &second).unwrap();
    let install = Install::resolve(&second, "0.1.0", &[], &[]).unwrap();
    assert_eq!(install.method, Method::Installer);
    assert_eq!(
        install.binary.parent(),
        fs::canonicalize(script.parent().unwrap()).ok().as_deref()
    );
    assert_eq!(fs::read_link(second).unwrap(), Path::new("alias"));
    assert_eq!(fs::read_link(link).unwrap(), helper);
}

#[test]
fn unresolved_or_unrecognized_binaries_are_never_installer_targets() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing/koi");
    assert!(Install::resolve(&missing, "0.1.0", &[], &[]).is_err());
    let cycle = dir.path().join("koi");
    std::os::unix::fs::symlink("koi", &cycle).unwrap();
    assert!(Install::resolve(&cycle, "0.1.0", &[], &[]).is_err());
    for relative in ["different-name", "Other.app/Contents/Helpers/koi"] {
        let binary = dir.path().join(relative);
        executable(&binary, "#!/bin/sh\nexit 0\n");
        assert!(Install::resolve(&binary, "0.1.0", &[], &[]).is_err());
    }
}

#[test]
fn homebrew_without_an_environment_prefix_is_found_from_the_running_koi() {
    let dir = tempfile::tempdir().unwrap();
    let prefix = dir.path().join("custom");
    let keg = prefix.join("Cellar/koidrive/0.1.0/bin/koi");
    executable(&keg, "#!/bin/sh\nexit 0\n");
    let opt = prefix.join("opt/koidrive");
    fs::create_dir_all(opt.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(keg.ancestors().nth(2).unwrap(), &opt).unwrap();
    let link = prefix.join("bin/koi");
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("../opt/koidrive/bin/koi", &link).unwrap();
    for binary in [&keg, &opt.join("bin/koi"), &link] {
        let install = Install::resolve(binary, "0.1.0", &[], &[]).unwrap();
        let brew = fs::canonicalize(&prefix).unwrap().join("bin/brew");
        assert_eq!(install.method, Method::Formula { brew: brew.clone() });
        assert_eq!(
            install.command().as_deref(),
            Some([brew.to_str().unwrap(), "upgrade", "koidrive"].as_slice())
        );
    }
    fs::remove_file(keg).unwrap();
    assert!(Install::resolve(&link, "0.1.0", &[], &[]).is_err());
}

#[test]
fn another_install_never_changes_the_running_binarys_installation_method() {
    let dir = tempfile::tempdir().unwrap();
    let prefix = dir.path().join("brew");
    executable(&prefix.join("opt/koidrive/bin/koi"), "#!/bin/sh\nexit 0\n");
    let script = dir.path().join("standalone/koi");
    executable(&script, "#!/bin/sh\nexit 0\n");
    let install = Install::resolve(&script, "0.1.0", &[prefix], &[]).unwrap();
    assert_eq!(install.method, Method::Installer);
    assert_eq!(install.binary, fs::canonicalize(script).unwrap());
}

#[test]
fn formula_ownership_survives_a_linked_cellar_and_invocation_alias() {
    let dir = tempfile::tempdir().unwrap();
    let prefix = dir.path().join("brew");
    let version = dir.path().join("external store/koidrive/0.1.0");
    let binary = version.join("bin/koi");
    executable(&binary, "#!/bin/sh\nexit 0\n");
    let opt = prefix.join("opt/koidrive");
    fs::create_dir_all(opt.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&version, &opt).unwrap();
    let link = prefix.join("bin/koi");
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("../opt/koidrive/bin/koi", &link).unwrap();
    let alias = dir.path().join("alias");
    std::os::unix::fs::symlink(&link, &alias).unwrap();
    for invoked in [&alias, &link, &opt.join("bin/koi")] {
        let install = Install::resolve(invoked, "0.1.0", &[], &[]).unwrap();
        assert_eq!(
            install.method,
            Method::Formula {
                brew: fs::canonicalize(&prefix).unwrap().join("bin/brew")
            }
        );
        assert_eq!(install.binary, fs::canonicalize(&binary).unwrap());
    }
}

#[test]
fn invocation_hints_must_resolve_to_the_running_binary() {
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("actual/koi");
    executable(&binary, "#!/bin/sh\nexit 0\n");
    let kernel = fs::canonicalize(&binary).unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let alias = bin.join("koi");
    std::os::unix::fs::symlink(&binary, &alias).unwrap();
    assert_eq!(
        Install::invoked(&kernel, alias.as_os_str(), None),
        Some(alias.clone())
    );
    assert_eq!(
        Install::invoked(&kernel, OsStr::new("koi"), Some(bin.as_os_str())),
        Some(alias)
    );
    let other = dir.path().join("other/koi");
    executable(&other, "#!/bin/sh\nexit 0\n");
    assert_eq!(Install::invoked(&kernel, other.as_os_str(), None), None);
    let shadowed = std::env::join_paths([other.parent().unwrap(), &bin]).unwrap();
    assert_eq!(
        Install::invoked(&kernel, OsStr::new("koi"), Some(&shadowed)),
        None
    );
    assert_eq!(
        Install::invoked(&kernel, OsStr::new(""), Some(&shadowed)),
        None
    );
    let cwd = std::env::current_dir().unwrap();
    let relative_dir = tempfile::tempdir_in(&cwd).unwrap();
    let binary = relative_dir.path().join("koi");
    executable(&binary, "#!/bin/sh\nexit 0\n");
    let relative = Path::new(".").join(binary.strip_prefix(&cwd).unwrap());
    assert_eq!(
        Install::invoked(&binary, relative.as_os_str(), None),
        Some(relative)
    );
}

#[test]
fn a_homebrew_receipt_blocks_installer_fallback_without_an_owning_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let keg = dir.path().join("external/package/0.1.0");
    let binary = keg.join("bin/koi");
    executable(&binary, "#!/bin/sh\nexit 0\n");
    fs::write(keg.join("INSTALL_RECEIPT.json"), "{}").unwrap();
    let alias = dir.path().join("alias");
    std::os::unix::fs::symlink(&binary, &alias).unwrap();
    for invoked in [&binary, &alias] {
        let error = Install::resolve(invoked, "0.1.0", &[], &[]).err().unwrap();
        assert!(error.message.contains("Homebrew"), "{}", error.message);
    }
}
