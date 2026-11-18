// SPDX-License-Identifier: Apache-2.0

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
        let expected = if state == "stopped" || state == "unavailable" || service_problem.is_some()
        {
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
        let (_fixture, helper) = crate::app_install::verified_helper_fixture(actual_version).await;
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
        serde_json::from_value(serde_json::json!({"state":"failed", "problem":"offline"})).unwrap(),
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
            error
                .message
                .contains("Download Koidrive 99.0.0 from https://example.test/releases/v99.0.0")
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
            let error = finish_reconciliation(&paths, Some(prepared), &from, reconciled, deadline)
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
            Some("The Mac app could not open".into()),
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
