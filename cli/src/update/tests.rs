#[cfg(test)]
mod tests {
    use std::os::unix::process::CommandExt;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    fn executable(path: &Path, script: &str) {
        super::fixture::executable(path, script.as_bytes());
    }

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
            let install =
                Install::resolve(binary, "0.1.0", &[], std::slice::from_ref(&app)).unwrap();
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
            let dev =
                Install::resolve(binary, "0.0.0-dev", &[], std::slice::from_ref(&app)).unwrap();
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
        let install =
            Install::resolve(&alias, "0.1.0", &[], std::slice::from_ref(&binary)).unwrap();
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
    fn a_quiet_echo_holds_child_lines_until_a_failure_replays_them() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("logs/update.log");
        let mut quiet = Echo::open(&log, false, false);
        quiet.line("Signature verified for SHA256SUMS", false);
        quiet.line("SHA256 mismatch", true);
        assert_eq!(
            quiet.held,
            ["Signature verified for SHA256SUMS", "SHA256 mismatch"]
        );
        quiet.replay();
        assert!(quiet.held.is_empty());
        drop(quiet);
        assert_eq!(
            fs::read_to_string(&log).unwrap(),
            "Signature verified for SHA256SUMS\nSHA256 mismatch\n"
        );
        for (json, verbose) in [(false, true), (true, false)] {
            let mut shown = Echo::open(&log, json, verbose);
            shown.line("Signature verified for SHA256SUMS", false);
            assert!(shown.held.is_empty());
        }
    }

    fn echo(dir: &Path) -> Echo {
        Echo::open(&dir.join("logs/update.log"), true, false)
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
    fn update_does_not_succeed_while_the_service_is_still_starting() {
        let report: service::Report = serde_json::from_value(serde_json::json!({
            "manager": "launchd", "name": "fixture", "state": "starting"
        }))
        .unwrap();
        let result = updated(
            &Version::new(1, 0, 0),
            Version::new(1, 0, 1),
            Via::Installer,
            report,
            None,
            true,
        );
        let error = result.unwrap_err();
        assert_eq!(error.code, Code::Unavailable);
        assert!(error.partial.is_some());
    }

    #[test]
    fn update_errors_name_the_failed_report_rather_than_success_or_retry_text() {
        for (state, service_problem, app_problem) in [
            ("stopped", None, None),
            ("unavailable", None, None),
            ("running", Some("Service reconciliation failed"), None),
            ("running", None, Some("App signature is invalid")),
            (
                "running",
                Some("Service reconciliation failed"),
                Some("App signature is invalid"),
            ),
            ("running", None, None),
            ("switched_off", None, None),
        ] {
            let report: service::Report = serde_json::from_value(serde_json::json!({
                "manager": "launchd", "name": "fixture", "state": state,
                "problem": service_problem
            }))
            .unwrap();
            let expected =
                if state == "stopped" || state == "unavailable" || service_problem.is_some() {
                    Some(report.failure_message())
                } else {
                    app_problem.map(str::to_owned)
                };
            for present in [true, false] {
                let app: crate::app::Report = if present {
                    serde_json::from_value(serde_json::json!({
                        "state": "present", "path": "/fixture/Koidrive.app", "version": "99.0.0",
                        "change": "installed", "launch": "requested", "problem": app_problem
                    }))
                    .unwrap()
                } else if let Some(problem) = app_problem {
                    crate::app::Report::Failed {
                        committed: Vec::new(),
                        retired: Vec::new(),
                        preserved: Vec::new(),
                        problem: problem.into(),
                        residue: Vec::new(),
                    }
                } else {
                    crate::app::Report::skipped("ci")
                };
                assert_eq!(
                    update_problem(&report, Some(&app)),
                    expected,
                    "{state}, {app:?}"
                );
            }
        }
    }

    fn app_fixture(
        dir: &Path,
        actual: &str,
        change: crate::app::Change,
        problem: Option<String>,
    ) -> crate::app::Prepared {
        let path = dir.join("user Applications/Koidrive.app");
        let binary = path.join("Contents/Helpers/koi");
        executable(
            &binary,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\ncase \"$1\" in\n--version) echo 'koi {actual}' ;;\n_reconcile) echo '{{\"data\":{{\"service\":{{\"manager\":\"launchd\",\"name\":\"fixture\",\"state\":\"running\"}},\"app\":{{\"state\":\"skipped\",\"reason\":\"bundled_helper\"}}}}}}' ;;\nesac\n"
            ),
        );
        crate::app::Prepared::fixture(crate::app::Report::Present {
            committed: Vec::new(),
            path,
            version: "99.0.0".into(),
            change,
            launch: if problem.is_some() {
                crate::app::Launch::Failed
            } else {
                crate::app::Launch::NotRequested
            },
            problem,
            retired: Vec::new(),
            residue: Vec::new(),
            preserved: Vec::new(),
        })
    }

    #[tokio::test]
    #[cfg(target_os = "macos")]
    async fn selected_development_app_discovery_uses_its_helper_instead_of_the_invoking_app() {
        let (_system_fixture, system) =
            crate::app_install::verified_helper_fixture_with_metadata("98.2.0", None).await;
        let (_user_fixture, invoking) = crate::app_install::verified_helper_fixture("99.0.0").await;
        let installs = Installs {
            homebrew: Vec::new(),
            script: None,
            apps: vec![system.path().to_path_buf(), invoking.path().to_path_buf()],
        };
        assert!(installs.all().contains(&system.path().to_path_buf()));
        assert!(installs.all().contains(&invoking.path().to_path_buf()));
        let selected = Install::resolve(system.path(), "98.2.0", &[], &installs.apps).unwrap();
        assert_eq!(selected.method, Method::App);
        assert_eq!(selected.binary, fs::canonicalize(system.path()).unwrap());
        assert!(system.version().is_none());
        let deadline = tokio::time::Instant::now() + RECONCILE;
        system.check(deadline).await.unwrap();
        let actual = service::version_of(system.path()).unwrap();
        system.check(deadline).await.unwrap();
        assert_eq!(actual.to_string(), "98.2.0");
        let discovered = discover_app_version(&system, deadline).await.unwrap();
        assert_eq!(
            discovered, actual,
            "The selected system app differs from the invoking user app"
        );
        assert!(!invoking.path().with_extension("calls").exists());
    }

    #[tokio::test]
    #[cfg(target_os = "macos")]
    async fn current_ci_skip_reconciles_the_verified_existing_helper() {
        for (actual_version, state, problem, fails) in [
            ("99.0.0", "running", None, false),
            ("100.0.0", "switched_off", None, false),
            ("99.0.0", "off", None, false),
            ("99.0.0", "stopped", None, true),
            ("99.0.0", "unavailable", None, true),
            ("99.0.0", "switched_off", Some("registration failed"), true),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let version = Version::parse(actual_version).unwrap();
            let target = Version::parse("99.0.0").unwrap();
            let prepared = crate::app::Prepared::fixture(crate::app::Report::skipped("ci"));
            assert!(prepared.helper().is_none());
            let (_fixture, helper) =
                crate::app_install::verified_helper_fixture(actual_version).await;
            let original = fs::read_to_string(helper.path()).unwrap();
            let fields = format!(
                "\"state\":{},\"problem\":{}",
                serde_json::to_string(state).unwrap(),
                serde_json::to_string(&problem).unwrap()
            );
            executable(
                helper.path(),
                &original.replace("\"state\":\"running\"", &fields),
            );
            let deadline = tokio::time::Instant::now() + RECONCILE;
            let from = discover_app_version(&helper, deadline).await.unwrap();
            assert_eq!(from, version);
            let helper = select_app_helper(&prepared, helper, &from, &target, deadline)
                .await
                .unwrap()
                .unwrap();
            let binary = UpdateBinary::App(Box::new(helper));
            let paths = Paths::under(dir.path());
            let manager = service::system();
            let reconciled = reconcile_updated(
                &binary,
                &target,
                manager.as_ref(),
                service::Service::new(&paths, dir.path(), binary.path()),
                &mut echo(dir.path()),
                deadline,
            )
            .await;
            let (actual, service, app) =
                finish_reconciliation(&paths, Some(prepared), &version, reconciled, deadline)
                    .await
                    .unwrap();
            assert_eq!(actual, version);
            assert_eq!(update_problem(&service, app.as_ref()).is_some(), fails);
            assert_eq!(serde_json::to_value(&service).unwrap()["state"], state);
            assert!(matches!(app, Some(crate::app::Report::Skipped { reason }) if reason == "ci"));
            assert_eq!(
                fs::read_to_string(binary.path().with_extension("calls")).unwrap(),
                "--version\n--version\n_reconcile --json\n"
            );
        }
    }

    #[tokio::test]
    async fn committed_app_failures_are_updated_partials_even_at_the_same_version() {
        let dir = tempfile::tempdir().unwrap();
        for from in ["1.0.0", "99.0.0"] {
            let app: crate::app::Report = serde_json::from_value(serde_json::json!({
                "state":"failed", "problem":"source changed",
                "committed":[{"path":dir.path().join("Koidrive.app"), "version":"99.0.0", "change":"replaced"}],
                "retired":[dir.path().join("retired.app")], "residue":[dir.path().join("receipt.json")]
            })).unwrap();
            let error = app_not_updated(
                &Paths::under(dir.path()),
                &Version::parse(from).unwrap(),
                "Download Koidrive 99.0.0 from https://example.test/release",
                crate::app::Prepared::fixture(app),
                tokio::time::Instant::now(),
            )
            .await;
            assert_eq!(error.code, Code::Failed);
            assert!(
                error
                    .message
                    .starts_with("source changed\nDownload Koidrive")
            );
            let partial = error.partial.unwrap();
            assert_eq!(partial.data["from"], from);
            assert_eq!(partial.data["to"], "99.0.0");
            assert_eq!(partial.data["result"], "updated");
            assert_ne!(serde_json::to_value(partial.outcome).unwrap(), "unchanged");
            assert_eq!(partial.data["app"]["state"], "failed");
            assert!(partial.data["app"].get("version").is_none());
            assert!(!partial.text.contains("was not updated"));
            assert!(!partial.text.contains("is installed"));
            assert_eq!(partial.text.matches("source changed").count(), 1);
            assert_eq!(partial.text.matches("Download Koidrive").count(), 1);
        }
    }

    #[tokio::test]
    #[cfg(target_os = "macos")]
    async fn ci_skip_fallback_requires_a_current_verified_app_and_never_accepts_errors() {
        let target = Version::parse("99.0.0").unwrap();
        for (reason, version, selected) in [
            ("ci", "98.0.0", false),
            ("ci", "100.0.0", true),
            ("platform", "99.0.0", false),
            ("architecture", "99.0.0", false),
        ] {
            let (_fixture, helper) = crate::app_install::verified_helper_fixture(version).await;
            let binary = helper.path().to_path_buf();
            let prepared = crate::app::Prepared::fixture(crate::app::Report::skipped(reason));
            let result = select_app_helper(
                &prepared,
                helper,
                &Version::parse(version).unwrap(),
                &target,
                tokio::time::Instant::now() + RECONCILE,
            )
            .await
            .unwrap();
            assert_eq!(result.is_some(), selected);
            assert!(!binary.with_extension("calls").exists());
        }
        let (_fixture, helper) = crate::app_install::verified_helper_fixture("99.0.0").await;
        let prepared = crate::app::Prepared::fixture(
            serde_json::from_value(serde_json::json!({"state":"failed", "problem":"offline"}))
                .unwrap(),
        );
        assert!(
            select_app_helper(
                &prepared,
                helper,
                &target,
                &target,
                tokio::time::Instant::now() + RECONCILE
            )
            .await
            .unwrap()
            .is_none()
        );
    }

    #[tokio::test]
    #[cfg(target_os = "macos")]
    async fn initial_app_discovery_rejects_changes_during_the_version_probe() {
        for invalid in [
            "signer",
            "helper",
            "during_version",
            "during_helper",
            "invalid_version",
        ] {
            let (_fixture, helper) =
                crate::app_install::verified_helper_fixture_with_metadata("99.0.0", None).await;
            let binary = helper.path().to_path_buf();
            let bundle = binary.ancestors().nth(3).unwrap();
            match invalid {
                "signer" => fs::write(bundle.join("Signer"), "OTHERTEAM").unwrap(),
                "helper" => {
                    fs::rename(&binary, binary.with_extension("old")).unwrap();
                    executable(&binary, "#!/bin/sh\necho unsafe > \"$0.calls\"\n");
                }
                "during_version" => executable(
                    &binary,
                    &format!(
                        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\nprintf OTHERTEAM > '{}/Signer'\necho 'koi 98.0.0'\n",
                        bundle.display()
                    ),
                ),
                "during_helper" => executable(
                    &binary,
                    "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\n/bin/mv \"$0\" \"$0.old\"\n/bin/cp \"$0.old\" \"$0\"\necho 'koi 98.0.0'\n",
                ),
                _ => executable(
                    &binary,
                    "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\necho 'not-a-version'\n",
                ),
            }
            let error = discover_app_version(&helper, tokio::time::Instant::now() + RECONCILE)
                .await
                .unwrap_err();
            if invalid == "invalid_version" {
                assert!(
                    matches!(error, ReconcileError::Failed(error) if error.message.contains("during app discovery"))
                );
            } else {
                assert!(matches!(error, ReconcileError::Unverified(_)));
            }
            if matches!(
                invalid,
                "during_version" | "during_helper" | "invalid_version"
            ) {
                assert_eq!(
                    fs::read_to_string(binary.with_extension("calls")).unwrap(),
                    "--version\n"
                );
            } else {
                assert!(!binary.with_extension("calls").exists());
            }
        }
    }

    #[tokio::test]
    #[cfg(target_os = "macos")]
    async fn ci_skip_uses_the_discovered_version_instead_of_missing_or_different_metadata() {
        for (metadata, actual, selected) in [
            (None, "99.0.0", true),
            (None, "98.0.0", false),
            (Some("100.0.0"), "98.0.0", false),
        ] {
            let (_fixture, helper) =
                crate::app_install::verified_helper_fixture_with_metadata(actual, metadata).await;
            let binary = helper.path().to_path_buf();
            let deadline = tokio::time::Instant::now() + RECONCILE;
            let from = discover_app_version(&helper, deadline).await.unwrap();
            assert_eq!(from.to_string(), actual);
            let prepared = crate::app::Prepared::fixture(crate::app::Report::skipped("ci"));
            let helper = select_app_helper(
                &prepared,
                helper,
                &from,
                &Version::parse("99.0.0").unwrap(),
                deadline,
            )
            .await
            .unwrap();
            assert_eq!(helper.is_some(), selected);
            assert_eq!(
                fs::read_to_string(binary.with_extension("calls")).unwrap(),
                "--version\n"
            );
        }
    }

    #[tokio::test]
    #[cfg(target_os = "macos")]
    async fn partial_update_versions_require_commits_or_a_verified_target_helper() {
        for (actual, committed) in [
            ("invalid", false),
            ("98.0.0", false),
            ("99.0.0", false),
            ("100.0.0", false),
            ("invalid", true),
            ("98.0.0", true),
            ("99.0.0", true),
            ("100.0.0", true),
        ] {
            let (_fixture, helper) = crate::app_install::verified_helper_fixture("99.0.0").await;
            let binary = helper.path().to_path_buf();
            let bundle = binary.ancestors().nth(3).unwrap().to_path_buf();
            executable(&binary, &format!("#!/bin/sh\necho 'koi {actual}'\n"));
            let committed = if committed {
                serde_json::json!([{"path":bundle, "version":"99.0.0", "change":"replaced"}])
            } else {
                serde_json::json!([])
            };
            let prepared = crate::app::Prepared::fixture(serde_json::from_value(serde_json::json!({
                "state":"present", "path":bundle, "version":"99.0.0", "change":"unchanged", "launch":"not_requested", "committed":committed
            })).unwrap());
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths::under(dir.path());
            let manager = service::system();
            let deadline = tokio::time::Instant::now() + RECONCILE;
            let reconciled = reconcile_updated(
                &UpdateBinary::App(Box::new(helper)),
                &Version::parse("99.0.0").unwrap(),
                manager.as_ref(),
                Err("The profile cannot construct its service".into()),
                &mut echo(dir.path()),
                deadline,
            )
            .await;
            let error = finish_reconciliation(
                &paths,
                Some(prepared),
                &Version::parse("1.0.0").unwrap(),
                reconciled,
                deadline,
            )
            .await
            .unwrap_err();
            let partial = error.partial.unwrap();
            assert_eq!(partial.data["to"], "99.0.0", "{actual}, {committed}");
            assert_eq!(partial.data["result"], "updated");
            assert_eq!(serde_json::to_value(partial.outcome).unwrap(), "ok");
            assert_eq!(
                partial.data["app"]["committed"]
                    .as_array()
                    .map(Vec::len)
                    .unwrap_or(0),
                committed.as_array().unwrap().len()
            );
            assert!(partial.data.get("service").is_none());
        }
    }

    #[tokio::test]
    #[cfg(target_os = "macos")]
    async fn failed_version_probes_cannot_preserve_a_substituted_app_as_present() {
        for actual in ["invalid", "98.0.0", "99.0.0"] {
            let (_fixture, helper) = crate::app_install::verified_helper_fixture("99.0.0").await;
            let binary = helper.path().to_path_buf();
            let bundle = binary.ancestors().nth(3).unwrap().to_path_buf();
            let prepared = crate::app::Prepared::fixture(serde_json::from_value(serde_json::json!({
                "state":"present", "path":bundle, "version":"99.0.0", "change":"unchanged", "launch":"not_requested"
            })).unwrap());
            executable(
                &binary,
                &format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\nprintf OTHERTEAM > '{}/Signer'\necho 'koi {actual}'\n",
                    bundle.display()
                ),
            );
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths::under(dir.path());
            let manager = service::system();
            let deadline = tokio::time::Instant::now() + RECONCILE;
            let reconciled = reconcile_updated(
                &UpdateBinary::App(Box::new(helper)),
                &Version::parse("99.0.0").unwrap(),
                manager.as_ref(),
                service::Service::new(&paths, dir.path(), &binary),
                &mut echo(dir.path()),
                deadline,
            )
            .await;
            assert!(
                matches!(&reconciled, Err(ReconcileError::Unverified(_))),
                "{actual}: {reconciled:?}"
            );
            let error = finish_reconciliation(
                &paths,
                Some(prepared),
                &Version::parse("1.0.0").unwrap(),
                reconciled,
                deadline,
            )
            .await
            .unwrap_err();
            let partial = error.partial.unwrap();
            assert_eq!(partial.data["app"]["state"], "failed");
            assert_eq!(partial.data["result"], "unchanged");
            assert_eq!(partial.data["to"], "1.0.0");
            assert!(!partial.text.contains("is installed"));
            assert_eq!(
                fs::read_to_string(binary.with_extension("calls")).unwrap(),
                "--version\n"
            );
        }
    }

    #[test]
    fn reused_apps_report_verified_version_advancement_without_a_namespace_commit() {
        for from in ["1.0.0", "99.0.0"] {
            for state in ["running", "switched_off", "stopped"] {
                let service: service::Report = serde_json::from_value(serde_json::json!({
                    "manager":"launchd", "name":"fixture", "state":state
                }))
                .unwrap();
                let app = serde_json::from_value(serde_json::json!({
                    "state":"present", "path":"/fixture/Koidrive.app", "version":"99.0.0", "change":"unchanged", "launch":"not_requested"
                })).unwrap();
                let result = updated(
                    &Version::parse(from).unwrap(),
                    Version::parse("99.0.0").unwrap(),
                    Via::App,
                    service,
                    Some(app),
                    false,
                );
                let done = if state == "stopped" {
                    result.unwrap_err().partial.map(|done| *done).unwrap()
                } else {
                    result.unwrap()
                };
                assert_eq!(done.data["from"], from);
                assert_eq!(done.data["to"], "99.0.0");
                assert_eq!(done.data["app"]["change"], "unchanged");
                assert_eq!(done.data["service"]["state"], state);
                if from == "1.0.0" {
                    assert_eq!(done.data["result"], "updated");
                    assert_ne!(serde_json::to_value(done.outcome).unwrap(), "unchanged");
                    assert!(
                        done.text
                            .starts_with("Updated Koidrive from 1.0.0 to 99.0.0.")
                    );
                } else {
                    assert_eq!(done.data["result"], "unchanged");
                    assert_eq!(serde_json::to_value(done.outcome).unwrap(), "unchanged");
                    assert!(
                        done.text
                            .starts_with("Koidrive 99.0.0 is the newest release.")
                    );
                }
            }
        }
    }

    #[tokio::test]
    #[cfg(target_os = "macos")]
    async fn substituted_app_helpers_are_rejected_at_each_execution_boundary() {
        for substitution in ["signer", "helper", "bundle", "during_version"] {
            let (_fixture, helper) = crate::app_install::verified_helper_fixture("99.0.0").await;
            let binary = helper.path().to_path_buf();
            let bundle = binary.ancestors().nth(3).unwrap().to_path_buf();
            let committed =
                serde_json::json!([{"path":bundle, "version":"99.0.0", "change":"replaced"}]);
            let prepared = crate::app::Prepared::fixture(serde_json::from_value(serde_json::json!({
                "state":"present", "path":bundle, "version":"99.0.0", "change":"replaced", "launch":"not_requested", "committed":committed,
            })).unwrap());
            match substitution {
                "signer" => fs::write(bundle.join("Signer"), "OTHERTEAM").unwrap(),
                "helper" => {
                    fs::rename(&binary, binary.with_extension("old")).unwrap();
                    executable(&binary, "#!/bin/sh\necho unsafe > \"$0.calls\"\n");
                }
                "during_version" => executable(
                    &binary,
                    &format!(
                        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\ncase \"$1\" in\n--version) printf OTHERTEAM > '{}/Signer'; echo 'koi 99.0.0' ;;\n_reconcile) echo unsafe >> \"$0.calls\" ;;\nesac\n",
                        bundle.display()
                    ),
                ),
                _ => {
                    fs::rename(&bundle, bundle.with_extension("displaced")).unwrap();
                    fs::create_dir_all(binary.parent().unwrap()).unwrap();
                    executable(&binary, "#!/bin/sh\necho unsafe > \"$0.calls\"\n");
                }
            }
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths::under(dir.path());
            let manager = service::system();
            let version = Version::parse("99.0.0").unwrap();
            let deadline = tokio::time::Instant::now() + RECONCILE;
            let reconciled = reconcile_updated(
                &UpdateBinary::App(Box::new(helper)),
                &version,
                manager.as_ref(),
                service::Service::new(&paths, dir.path(), &binary),
                &mut echo(dir.path()),
                deadline,
            )
            .await;
            assert!(matches!(&reconciled, Err(ReconcileError::Unverified(_))));
            let error = finish_reconciliation(
                &paths,
                Some(prepared),
                &Version::parse("1.0.0").unwrap(),
                reconciled,
                deadline,
            )
            .await
            .unwrap_err();
            let partial = error.partial.unwrap();
            assert_eq!(partial.data["result"], "updated");
            assert_eq!(partial.data["to"], "99.0.0");
            assert_eq!(partial.data["app"]["state"], "failed");
            assert_eq!(partial.data["app"]["committed"], committed);
            assert!(!partial.text.contains("is installed"));
            if substitution == "during_version" {
                assert_eq!(
                    fs::read_to_string(binary.with_extension("calls")).unwrap(),
                    "--version\n"
                );
            } else {
                assert!(!binary.with_extension("calls").exists());
            }
        }
    }

    #[tokio::test]
    async fn unsuccessful_app_preparation_preserves_its_report_in_an_unchanged_partial() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        for app in [
            crate::app::Report::Failed {
                committed: Vec::new(),
                retired: Vec::new(),
                preserved: Vec::new(),
                problem: "Checksum mismatch".into(),
                residue: vec![dir.path().join("retained-mount")],
            },
            crate::app::Report::skipped("ci"),
            crate::app::Report::skipped("platform"),
            crate::app::Report::skipped("architecture"),
        ] {
            let expected = serde_json::to_value(&app).unwrap();
            let prepared = crate::app::Prepared::fixture(app);
            assert_eq!(prepared.helper(), None);
            let error = app_not_updated(
                &paths,
                &Version::parse("1.0.0").unwrap(),
                "Download Koidrive 99.0.0 from https://example.test/releases/v99.0.0",
                prepared,
                tokio::time::Instant::now(),
            )
            .await;
            assert_eq!(error.code, Code::Failed);
            assert!(
                error.message.contains(
                    "Download Koidrive 99.0.0 from https://example.test/releases/v99.0.0"
                )
            );
            let partial = error.partial.unwrap();
            assert_eq!(serde_json::to_value(partial.outcome).unwrap(), "unchanged");
            assert_eq!(partial.data["from"], "1.0.0");
            assert_eq!(partial.data["to"], "1.0.0");
            assert_eq!(partial.data["result"], "unchanged");
            assert_eq!(partial.data["via"], "app");
            assert_eq!(partial.data["app"], expected);
            assert!(partial.data.get("service").is_none());
            assert!(!partial.text.contains("Updated Koidrive"));
        }
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn an_app_reconciliation_error_retains_the_installed_app_in_its_partial_result() {
        for (actual, valid_service) in [("invalid", true), ("98.0.0", true), ("99.0.0", false)] {
            for result in [UpdateResult::Updated, UpdateResult::Unchanged] {
                let dir = tempfile::tempdir().unwrap();
                let paths = Paths::under(dir.path());
                let path = dir.path().join("Koidrive.app");
                let koi = path.join("Contents/Helpers/koi");
                let marker = dir.path().join("must-not-reconcile");
                executable(
                    &koi,
                    &format!(
                        "#!/bin/sh\ncase \"$1\" in\n--version) echo 'koi {actual}' ;;\n_reconcile) echo ran > '{}' ;;\nesac\n",
                        marker.display()
                    ),
                );
                let old = dir.path().join("retired-helper");
                let residue = dir.path().join("retained-residue");
                fs::write(&old, "old helper remains usable").unwrap();
                fs::write(&residue, "preserved residue").unwrap();
                let prepared = crate::app::Prepared::fixture(crate::app::Report::Present {
                    committed: Vec::new(),
                    path,
                    version: "99.0.0".into(),
                    change: crate::app::Change::Replaced,
                    launch: crate::app::Launch::NotRequested,
                    problem: None,
                    retired: vec![old.clone()],
                    residue: vec![residue.clone()],
                    preserved: Vec::new(),
                });
                let manager = service::system();
                let service = if valid_service {
                    service::Service::new(&paths, dir.path(), &koi)
                } else {
                    Err("The profile cannot construct its service".into())
                };
                let target = Version::parse("99.0.0").unwrap();
                let deadline = tokio::time::Instant::now() + RECONCILE;
                let reconciled = reconcile_updated(
                    &UpdateBinary::Other(koi.clone()),
                    &target,
                    manager.as_ref(),
                    service,
                    &mut echo(dir.path()),
                    deadline,
                )
                .await;
                assert!(reconciled.is_err());
                let from = Version::parse(if result == UpdateResult::Updated {
                    "1.0.0"
                } else {
                    "99.0.0"
                })
                .unwrap();
                let error =
                    finish_reconciliation(&paths, Some(prepared), &from, reconciled, deadline)
                        .await
                        .unwrap_err();
                let partial = error
                    .partial
                    .expect("the installed app must remain in the error result");
                assert_eq!(partial.data["app"]["state"], "present");
                assert_eq!(partial.data["app"]["version"], "99.0.0");
                assert_eq!(partial.data["app"]["problem"], error.message);
                assert_eq!(partial.data["app"]["retired"], serde_json::json!([old]));
                assert_eq!(partial.data["app"]["residue"], serde_json::json!([residue]));
                assert_eq!(partial.data["via"], "app");
                assert_eq!(partial.data["to"], "99.0.0");
                assert_eq!(
                    partial.data["result"],
                    serde_json::to_value(result).unwrap()
                );
                assert!(partial.data.get("service").is_none());
                if result == UpdateResult::Unchanged {
                    assert_eq!(serde_json::to_value(partial.outcome).unwrap(), "unchanged");
                }
                assert!(!marker.exists());
                assert_eq!(fs::read(old).unwrap(), b"old helper remains usable");
                assert_eq!(fs::read(residue).unwrap(), b"preserved residue");
            }
        }
    }

    #[tokio::test]
    async fn the_app_update_wait_uses_only_its_remaining_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let koi = dir.path().join("koi");
        let pid_file = dir.path().join("pid");
        executable(
            &koi,
            &format!(
                "#!/bin/sh\ncase \"$1\" in\n--version) echo 'koi 99.0.0' ;;\n_reconcile) echo $$ > '{}'; exec /bin/sleep 30 ;;\nesac\n",
                pid_file.display()
            ),
        );
        let paths = Paths::under(dir.path());
        let manager = service::system();
        let service = service::Service::new(&paths, dir.path(), &koi);
        let mut echo = echo(dir.path());
        let target = Version::parse("99.0.0").unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let binary = UpdateBinary::Other(koi.clone());
        let tail = reconcile_updated(
            &binary,
            &target,
            manager.as_ref(),
            service,
            &mut echo,
            deadline,
        );
        tokio::pin!(tail);
        let ready = async {
            loop {
                if let Some(pid) = fs::read_to_string(&pid_file)
                    .ok()
                    .and_then(|text| text.trim().parse::<i32>().ok())
                    .and_then(rustix::process::Pid::from_raw)
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        let pid = tokio::select! {
            result = &mut tail => panic!("The reconciliation ended before the hanging helper started: {result:?}"),
            pid = ready => pid,
        };
        assert!(rustix::process::test_kill_process(pid).is_ok());
        let (_, report, app) = tokio::time::timeout(Duration::from_secs(5), tail)
            .await
            .unwrap()
            .unwrap();
        assert!(report.failed());
        assert!(app.is_none());
        assert!(tokio::time::Instant::now() >= deadline);
        assert!(rustix::process::test_kill_process(pid).is_ok());
        rustix::process::kill_process(pid, rustix::process::Signal::KILL).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while rustix::process::test_kill_process(pid).is_ok() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let old = dir.path().join("retired-helper");
        fs::write(&old, "old helper remains usable").unwrap();
        let prepared = crate::app::Prepared::fixture(crate::app::Report::Present {
            committed: Vec::new(),
            path: dir.path().join("Koidrive.app"),
            version: "99.0.0".into(),
            change: crate::app::Change::Replaced,
            launch: crate::app::Launch::NotRequested,
            problem: None,
            retired: vec![old.clone()],
            residue: Vec::new(),
            preserved: Vec::new(),
        });
        let app = prepared.finish(&paths, Ok(&report), deadline).await;
        assert!(
            matches!(app, crate::app::Report::Present { problem: Some(_), retired, .. } if retired == vec![old.clone()])
        );
        assert_eq!(fs::read(old).unwrap(), b"old helper remains usable");
    }

    #[tokio::test]
    async fn app_updates_verify_the_prepared_helper_before_one_shared_reconcile() {
        for (change, actual) in [
            (crate::app::Change::Unchanged, "99.0.0"),
            (crate::app::Change::Replaced, "100.0.0"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let prepared = app_fixture(
                dir.path(),
                actual,
                change,
                Some("The menu bar app could not open".into()),
            );
            let koi = prepared.helper().unwrap();
            assert!(koi.starts_with(dir.path()));
            let paths = Paths::under(dir.path());
            let service = service::Service::new(&paths, dir.path(), &koi);
            let manager = service::system();
            let (version, report, forwarded) = reconcile_updated(
                &UpdateBinary::Other(koi.clone()),
                &Version::parse("99.0.0").unwrap(),
                manager.as_ref(),
                service,
                &mut echo(dir.path()),
                tokio::time::Instant::now() + RECONCILE,
            )
            .await
            .unwrap();
            assert_eq!(version.to_string(), actual);
            assert!(
                matches!(forwarded, Some(crate::app::Report::Skipped { reason }) if reason == "bundled_helper")
            );
            let before = serde_json::to_value(&report).unwrap();
            let app = prepared
                .finish(
                    &paths,
                    Ok(&report),
                    tokio::time::Instant::now() + Duration::from_secs(10),
                )
                .await;
            assert!(app.failed());
            assert_eq!(serde_json::to_value(&report).unwrap(), before);
            assert!(!report.failed());
            assert_eq!(
                fs::read_to_string(koi.with_extension("calls")).unwrap(),
                "--version\n_reconcile --json\n"
            );
        }
    }

    #[tokio::test]
    async fn a_prepared_app_does_not_bypass_actual_helper_version_verification() {
        for version in ["0.1.0", "not-semver"] {
            let dir = tempfile::tempdir().unwrap();
            let prepared = app_fixture(dir.path(), version, crate::app::Change::Replaced, None);
            let koi = prepared.helper().unwrap();
            let paths = Paths::under(dir.path());
            let service = service::Service::new(&paths, dir.path(), &koi);
            let manager = service::system();
            let error = reconcile_updated(
                &UpdateBinary::Other(koi.clone()),
                &Version::parse("99.0.0").unwrap(),
                manager.as_ref(),
                service,
                &mut echo(dir.path()),
                tokio::time::Instant::now() + RECONCILE,
            )
            .await
            .unwrap_err();
            assert!(matches!(error, ReconcileError::Failed(error) if error.code == Code::Failed));
            assert_eq!(
                fs::read_to_string(koi.with_extension("calls")).unwrap(),
                "--version\n"
            );
        }
    }

    #[tokio::test]
    async fn the_shared_update_tail_forwards_app_failure_without_changing_service() {
        for changed in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let koi = dir.path().join("koi");
            let mut app = serde_json::json!({"state":"failed", "problem":"offline", "residue":["fixture-residue"]});
            if changed {
                app["committed"] = serde_json::json!([{"path":"/Applications/Koidrive.app", "version":"99.0.0", "change":"replaced"}]);
            }
            let report = serde_json::json!({"manager":"launchd", "name":"fixture", "state":"off"});
            let envelope = serde_json::json!({"data":{"service":report, "app":app}});
            executable(
                &koi,
                &format!(
                    "#!/bin/sh\ncase \"$1\" in\n--version) echo 'koi 99.0.0' ;;\n_reconcile) printf '%s\\n' '{envelope}' ;;\nesac\n"
                ),
            );
            let paths = Paths::under(dir.path());
            let manager = service::system();
            let (_, service, forwarded) = reconcile_updated(
                &UpdateBinary::Other(koi.clone()),
                &Version::parse("99.0.0").unwrap(),
                manager.as_ref(),
                service::Service::new(&paths, dir.path(), &koi),
                &mut echo(dir.path()),
                tokio::time::Instant::now() + RECONCILE,
            )
            .await
            .unwrap();
            assert_eq!(serde_json::to_value(service).unwrap(), report);
            let forwarded = forwarded.unwrap();
            assert_eq!(!forwarded.committed().is_empty(), changed);
            assert_eq!(serde_json::to_value(forwarded).unwrap(), app);
        }
    }

    #[test]
    fn reconcile_subprocess() {
        let Some(dir) = std::env::var_os("KOI_RECONCILE_TEST_DIR").map(PathBuf::from) else {
            return;
        };
        let mode = std::env::var("KOI_RECONCILE_TEST_MODE").unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let mut command = Command::new(dir.join("koi"));
            command.arg(&dir);
            let mut echo = echo(&dir);
            if mode == "cancel" {
                assert!(
                    tokio::time::timeout(
                        Duration::from_millis(100),
                        echo.run(command, Duration::from_secs(10), Child::Reconcile)
                    )
                    .await
                    .is_err()
                );
            } else {
                let within = if mode == "timeout" {
                    Duration::from_millis(100)
                } else {
                    Duration::from_secs(10)
                };
                let result = echo.run(command, within, Child::Reconcile).await.unwrap();
                assert!(result.is_none());
            }
        });
    }

    fn surviving_reconcile(mode: &str) {
        let dir = tempfile::tempdir().unwrap();
        executable(
            &dir.path().join("koi"),
            "#!/bin/sh\ntouch \"$1/ready\"\ni=0\nwhile [ ! -f \"$1/continue\" ] && [ $i -lt 250 ]; do sleep 0.02; i=$((i+1)); done\n[ -f \"$1/continue\" ] && touch \"$1/completed\"\n",
        );
        let mut parent = std::process::Command::new(std::env::current_exe().unwrap());
        parent
            .args([
                "--exact",
                "update::tests::reconcile_subprocess",
                "--nocapture",
            ])
            .env("KOI_RECONCILE_TEST_DIR", dir.path())
            .env("KOI_RECONCILE_TEST_MODE", mode)
            .process_group(0);
        let mut parent = parent.spawn().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !dir.path().join("ready").exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(dir.path().join("ready").exists());
        if mode == "sigint" {
            assert!(
                std::process::Command::new("/bin/kill")
                    .args(["-INT", "--", &format!("-{}", parent.id())])
                    .status()
                    .unwrap()
                    .success()
            );
        }
        loop {
            if let Some(status) = parent.try_wait().unwrap() {
                if mode == "sigint" {
                    assert!(!status.success());
                } else {
                    assert!(status.success());
                }
                break;
            }
            if std::time::Instant::now() >= deadline {
                parent.kill().unwrap();
                parent.wait().unwrap();
                panic!("foreground reconcile wait did not end");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        fs::write(dir.path().join("continue"), "").unwrap();
        while !dir.path().join("completed").exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            dir.path().join("completed").exists(),
            "reconciliation must finish after {mode}"
        );
    }

    #[test]
    fn reconciliation_survives_foreground_process_group_interrupt() {
        surviving_reconcile("sigint");
    }

    #[test]
    fn reconciliation_survives_foreground_wait_timeout() {
        surviving_reconcile("timeout");
    }

    #[test]
    fn reconciliation_survives_foreground_wait_cancellation() {
        surviving_reconcile("cancel");
    }

    #[tokio::test]
    async fn cancellable_children_are_killed_when_the_wait_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let mut command = Command::new("/bin/sh");
        let pid_file = dir.path().join("pid");
        command
            .args(["-c", "echo $$ > \"$1\"; exec sleep 30", "sh"])
            .arg(&pid_file);
        let result = echo(dir.path())
            .run(command, Duration::from_millis(100), Child::Progress)
            .await
            .unwrap();
        assert!(result.is_none());
        let pid = fs::read_to_string(pid_file).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if !std::process::Command::new("/bin/kill")
                .args(["-0", pid.trim()])
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "cancellable child is still alive"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
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
        let app_only =
            Install::resolve(&helper, "0.1.0", &[], std::slice::from_ref(&helper)).unwrap();
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

    async fn serve_once(listener: TcpListener, body: &'static str) {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 4096];
        while !request.ends_with(b"\r\n\r\n") {
            let read = stream.read(&mut buffer).await.unwrap();
            assert!(read > 0);
            request.extend_from_slice(&buffer[..read]);
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    }

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
}
