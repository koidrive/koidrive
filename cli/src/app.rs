// SPDX-License-Identifier: Apache-2.0

use std::path::{Path, PathBuf};
#[cfg(any(target_os = "macos", test))]
use std::process::Stdio;
#[cfg(test)]
use std::time::Duration;

use koidrive_driver::Paths;
#[cfg(target_os = "macos")]
use koidrive_ipc::{Request, Response};
use schemars::JsonSchema;
use semver::Version;
use serde::{Deserialize, Serialize};
#[cfg(any(target_os = "macos", test))]
use tokio::io::AsyncReadExt;
use tokio::time::Instant;

use crate::app_install::{self, AppInstall, AppLaunch, AppSkip};
#[cfg(target_os = "macos")]
use crate::app_install::{AppChange, AppLaunchResult};
#[cfg(target_os = "macos")]
use crate::ipc;
use crate::service;

static HANDOFF: tokio::sync::Mutex<Option<tokio::process::Child>> =
    tokio::sync::Mutex::const_new(None);

pub(crate) async fn cancel_active() {
    app_install::cancel_active().await;
    if let Some(mut child) = HANDOFF.lock().await.take() {
        stop_handoff(&mut child).await;
    }
}

async fn stop_handoff(child: &mut tokio::process::Child) {
    if let Some(group) = child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .and_then(rustix::process::Pid::from_raw)
    {
        let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    }
    let _ = child.kill().await;
}

#[derive(Debug, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum Report {
    Skipped {
        reason: String,
    },
    Present {
        path: PathBuf,
        version: String,
        change: Change,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        committed: Vec<app_install::CommitFact>,
        launch: Launch,
        #[serde(skip_serializing_if = "Option::is_none")]
        problem: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        retired: Vec<PathBuf>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        residue: Vec<PathBuf>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        preserved: Vec<PathBuf>,
    },
    Failed {
        problem: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        committed: Vec<app_install::CommitFact>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        retired: Vec<PathBuf>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        preserved: Vec<PathBuf>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        residue: Vec<PathBuf>,
    },
}

#[derive(Debug, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Change {
    Unchanged,
    Installed,
    Replaced,
}

#[derive(Debug, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Launch {
    NotRequested,
    Requested,
    Failed,
}

impl Report {
    pub(crate) fn skipped(reason: &str) -> Self {
        Self::Skipped {
            reason: reason.into(),
        }
    }

    pub(crate) fn failed(&self) -> bool {
        self.problem().is_some()
    }

    pub(crate) fn committed(&self) -> &[app_install::CommitFact] {
        match self {
            Self::Present { committed, .. } | Self::Failed { committed, .. } => committed,
            Self::Skipped { .. } => &[],
        }
    }

    pub(crate) fn problem(&self) -> Option<&str> {
        match self {
            Self::Skipped { .. } => None,
            Self::Present { problem, .. } => problem.as_deref(),
            Self::Failed { problem, .. } => Some(problem),
        }
    }

    pub(crate) fn text(&self) -> String {
        match self {
            Self::Skipped { .. } => String::new(),
            Self::Present {
                path,
                version,
                problem,
                preserved,
                ..
            } => {
                let mut installed = format!(
                    "Koidrive {version} is installed in {}.\n",
                    crate::printable(&path.display().to_string())
                );
                for path in preserved {
                    installed.push_str(&format!(
                        "An older Koidrive app remains in {}.\n",
                        crate::printable(&path.display().to_string())
                    ));
                }
                match problem {
                    Some(problem) => format!(
                        "{installed}App setup is incomplete: {}\nRetry: koi setup\n",
                        crate::printable(problem)
                    ),
                    None => installed,
                }
            }
            Self::Failed {
                problem, committed, ..
            } => {
                let mut text = String::new();
                for commit in committed {
                    let action = match commit.change {
                        app_install::CommitChange::Installed => "installation",
                        app_install::CommitChange::Replaced => "replacement",
                    };
                    text.push_str(&format!(
                        "Koidrive {} {action} committed in {}. Its current state could not be verified.\n",
                        commit.version,
                        crate::printable(&commit.path.display().to_string()),
                    ));
                }
                let failure = if committed.is_empty() {
                    "The Koidrive app could not be installed"
                } else {
                    "The Koidrive app setup did not complete"
                };
                text.push_str(&format!(
                    "{failure}: {}\nRetry: koi setup\n",
                    crate::printable(problem)
                ));
                text
            }
        }
    }
}

pub(crate) fn bundled(binary: &Path) -> bool {
    binary.file_name().is_some_and(|name| name == "koi")
        && binary
            .parent()
            .is_some_and(|dir| dir.file_name().is_some_and(|name| name == "Helpers"))
        && binary
            .parent()
            .and_then(Path::parent)
            .is_some_and(|dir| dir.file_name().is_some_and(|name| name == "Contents"))
        && binary
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .is_some_and(|dir| dir.extension().is_some_and(|name| name == "app"))
}

pub(crate) struct Prepared {
    report: Report,
    #[cfg(target_os = "macos")]
    retired: Vec<crate::app_install::RetiredApp>,
}

pub(crate) async fn prepare(target: &Version, launch: AppLaunch, deadline: Instant) -> Prepared {
    Prepared::from_install(app_install::ensure(target, launch, deadline).await)
}

impl Prepared {
    pub(crate) fn from_install(result: Result<AppInstall, app_install::AppInstallError>) -> Self {
        #[cfg(target_os = "macos")]
        let mut retained = Vec::new();
        let report = match result {
            Ok(AppInstall::Skipped(reason)) => Report::skipped(match reason {
                AppSkip::Ci => "ci",
                AppSkip::Platform => "platform",
                AppSkip::Architecture => "architecture",
            }),
            #[cfg(target_os = "macos")]
            Ok(AppInstall::Present {
                change,
                committed,
                path,
                version,
                launch,
                retired,
                residue,
                preserved,
            }) => {
                let mut problem = match &launch {
                    AppLaunchResult::Failed(problem) => Some(problem.clone()),
                    _ => None,
                };
                if !residue.is_empty() {
                    problem.get_or_insert_with(|| {
                        "Installer cleanup is incomplete; the listed residue was preserved.".into()
                    });
                }
                let retired_paths = retired.iter().map(|old| old.bundle.clone()).collect();
                retained = retired;
                Report::Present {
                    committed,
                    path,
                    version: version.to_string(),
                    change: match change {
                        AppChange::Unchanged => Change::Unchanged,
                        AppChange::Installed => Change::Installed,
                        AppChange::Replaced => Change::Replaced,
                    },
                    launch: match launch {
                        AppLaunchResult::NotRequested => Launch::NotRequested,
                        AppLaunchResult::Requested => Launch::Requested,
                        AppLaunchResult::Failed(_) => Launch::Failed,
                    },
                    problem,
                    retired: retired_paths,
                    residue,
                    preserved,
                }
            }
            #[cfg(target_os = "macos")]
            Err(error) => Report::Failed {
                problem: error.to_string(),
                residue: error.residue,
                committed: error.committed,
                retired: error.retired,
                preserved: error.preserved,
            },
            #[cfg(not(target_os = "macos"))]
            Err(error) => match error {},
        };
        Prepared {
            report,
            #[cfg(target_os = "macos")]
            retired: retained,
        }
    }

    #[cfg(test)]
    pub(crate) fn fixture(report: Report) -> Self {
        Self {
            report,
            #[cfg(target_os = "macos")]
            retired: Vec::new(),
        }
    }

    #[cfg(any(target_os = "macos", test))]
    pub(crate) fn helper(&self) -> Option<PathBuf> {
        match &self.report {
            Report::Present { path, .. } => Some(path.join("Contents/Helpers/koi")),
            _ => None,
        }
    }

    #[cfg(target_os = "macos")]
    pub(crate) async fn verified_helper(
        &self,
        deadline: Instant,
    ) -> Result<Option<app_install::VerifiedHelper>, app_install::AppInstallError> {
        let Report::Present { path, version, .. } = &self.report else {
            return Ok(None);
        };
        let helper =
            app_install::verify_existing(&path.join("Contents/Helpers/koi"), deadline).await?;
        helper.require_version(version)?;
        Ok(Some(helper))
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn skipped_in_ci(&self) -> bool {
        matches!(&self.report, Report::Skipped { reason } if reason == "ci")
    }

    pub(crate) fn committed(&self) -> &[app_install::CommitFact] {
        self.report.committed()
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn invalidate(self, mut error: app_install::AppInstallError) -> Self {
        if let Report::Present {
            committed,
            retired,
            residue,
            preserved,
            ..
        } = self.report
        {
            error.committed.extend(committed);
            error.retired.extend(retired);
            error.residue.extend(residue);
            error.preserved.extend(preserved);
        }
        Self::from_install(Err(error))
    }

    fn needs_handoff(&self) -> bool {
        matches!(&self.report, Report::Present { change, retired, .. } if *change != Change::Unchanged || !retired.is_empty())
    }

    pub(crate) async fn finish(
        mut self,
        paths: &Paths,
        handoff: Result<&service::Report, String>,
        deadline: Instant,
    ) -> Report {
        #[cfg(not(target_os = "macos"))]
        let _ = (paths, deadline);
        if !self.needs_handoff() {
            return self.report;
        }
        let failure = match handoff {
            Ok(report) if !report.failed() => None,
            Ok(report) => Some(report.failure_message()),
            Err(error) => Some(error),
        };
        if let Some(error) = failure {
            let Report::Present { problem, .. } = &mut self.report else {
                unreachable!()
            };
            problem.get_or_insert(error);
            return self.report;
        }
        #[cfg(target_os = "macos")]
        if !self.retired.is_empty() {
            let status = tokio::time::timeout_at(deadline, ipc::run(paths, Request::Status)).await;
            let Report::Present {
                version,
                problem,
                retired,
                residue,
                ..
            } = &mut self.report
            else {
                unreachable!()
            };
            let target =
                Version::parse(version).expect("the installer verified the full release version");
            let confirmed = matches!(status, Ok(Ok(Response::Status { status })) if status.running && status.version.as_deref().and_then(|text| Version::parse(text).ok()).is_some_and(|running| running.cmp_precedence(&target).is_ge()));
            if confirmed {
                for old in std::mem::take(&mut self.retired) {
                    match old.discard() {
                        Ok(()) => retired.retain(|path| path != &old.bundle),
                        Err(error) => {
                            problem.get_or_insert_with(|| {
                                format!("The previous app was retained: {error}")
                            });
                            residue.extend(error.residue);
                        }
                    }
                }
            } else {
                problem.get_or_insert_with(|| "The previous app is retained until the new daemon answers. Check sync: koi status".into());
            }
        }
        self.report
    }
}

pub(crate) async fn install(
    paths: &Paths,
    target: &Version,
    launch: AppLaunch,
    deadline: Instant,
) -> (Report, Option<service::Report>) {
    let prepared = prepare(target, launch, deadline).await;
    #[cfg(target_os = "macos")]
    if prepared.needs_handoff() {
        let handoff = reconcile(
            &prepared.helper().expect("a present app has a helper"),
            deadline,
        )
        .await;
        let report = handoff.as_ref().ok().cloned();
        return (
            prepared
                .finish(paths, handoff.as_ref().map_err(Clone::clone), deadline)
                .await,
            report,
        );
    }
    #[cfg(not(target_os = "macos"))]
    let _ = paths;
    (prepared.report, None)
}

#[cfg(any(target_os = "macos", test))]
async fn reconcile(binary: &Path, deadline: Instant) -> Result<service::Report, String> {
    const MAX_OUTPUT: usize = 16 * 1024 * 1024;
    let mut active = HANDOFF.lock().await;
    let child = crate::program::found_async(binary)
        .and_then(|mut command| {
            command
                .args(["_reconcile", "--json"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .process_group(0)
                .kill_on_drop(true)
                .spawn()
        })
        .map_err(|error| format!("The installed app's helper did not run: {error}"))?;
    *active = Some(child);
    let child = active.as_mut().expect("the handoff child was just started");
    let stdout = child.stdout.take().expect("the helper has piped stdout");
    let mut output = stdout.take((MAX_OUTPUT + 1) as u64);
    let mut printed = Vec::new();
    let result = tokio::time::timeout_at(deadline, async {
        output.read_to_end(&mut printed).await?;
        if printed.len() > MAX_OUTPUT {
            return Ok(None);
        }
        child.wait().await.map(Some)
    })
    .await;
    let status = match result {
        Ok(Ok(Some(status))) => status,
        Ok(Ok(None)) => {
            stop_handoff(child).await;
            active.take();
            return Err("The installed app's helper returned too much output.".into());
        }
        _ => {
            stop_handoff(child).await;
            active.take();
            return Err("The installed app's helper did not complete reconciliation.".into());
        }
    };
    active.take();
    let envelope: serde_json::Value = serde_json::from_slice(&printed)
        .map_err(|_| "The installed app's helper returned no JSON envelope.")?;
    if !status.success() {
        return Err("The installed app's helper could not reconcile background sync.".into());
    }
    serde_json::from_value(envelope["data"]["service"].clone())
        .map_err(|_| "The installed app's helper returned no service report.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tokio::process::Command;

    fn helper(home: &Path, body: &str) -> PathBuf {
        let helper = home.join("koi");
        std::fs::write(&helper, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        helper
    }

    #[test]
    fn committed_failure_is_historical_and_never_selects_a_helper() {
        let value = serde_json::json!({
            "state": "failed", "problem": "source changed",
            "committed": [{"path": "/Applications/Koidrive.app", "version": "99.0.0", "change": "replaced"}],
            "retired": ["/Applications/.koidrive-retired-owned/Koidrive.app"],
            "residue": ["/cache/receipt-owned.json"], "preserved": ["/other/Koidrive.app"]
        });
        let report: Report = serde_json::from_value(value.clone()).unwrap();
        let prepared = Prepared::fixture(report);
        assert!(prepared.helper().is_none());
        assert_eq!(prepared.committed().len(), 1);
        assert!(prepared.report.failed());
        assert!(
            prepared
                .report
                .text()
                .contains("current state could not be verified")
        );
        assert!(!prepared.report.text().contains("is installed"));
        assert_eq!(serde_json::to_value(prepared.report).unwrap(), value);
        let old = serde_json::json!({"state":"failed", "problem":"offline"});
        assert_eq!(
            serde_json::to_value(serde_json::from_value::<Report>(old.clone()).unwrap()).unwrap(),
            old
        );
    }

    #[test]
    fn historical_commit_versions_reject_malformed_json_and_keep_the_string_wire_shape() {
        for state in ["failed", "present"] {
            let mut value = serde_json::json!({
                "state":state, "problem":"source changed", "path":"/Applications/Koidrive.app",
                "version":"99.0.0", "change":"replaced", "launch":"not_requested",
                "committed":[{"path":"/Applications/Koidrive.app", "version":"99.0.0-beta.1+build", "change":"replaced"}],
            });
            let report: Report = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(
                serde_json::to_value(report).unwrap()["committed"],
                value["committed"]
            );
            for invalid in ["", "latest", "99.0", "v99.0.0", "99.0.0\nforged"] {
                value["committed"][0]["version"] = serde_json::json!(invalid);
                assert!(
                    serde_json::from_value::<Report>(value.clone()).is_err(),
                    "{state}: {invalid:?}"
                );
            }
        }
    }

    #[test]
    fn only_a_bundled_helper_skips_lifecycle_installation() {
        assert!(bundled(Path::new(
            "/Applications/Koidrive.app/Contents/Helpers/koi"
        )));
        for path in [
            "/opt/homebrew/bin/koi",
            "/tmp/Contents/Helpers/koi",
            "/tmp/Koidrive.app/Helpers/koi",
            "/tmp/Koidrive.app/Contents/Helpers/koi-other",
        ] {
            assert!(!bundled(Path::new(path)), "{path}");
        }
    }

    #[tokio::test]
    async fn skipped_and_failed_preparations_never_select_a_helper_or_reconcile() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::under(home.path());
        for report in [
            Report::skipped("ci"),
            Report::skipped("platform"),
            Report::skipped("architecture"),
            Report::Failed {
                committed: Vec::new(),
                retired: Vec::new(),
                preserved: Vec::new(),
                problem: "checksum mismatch".into(),
                residue: vec![home.path().join("mount")],
            },
        ] {
            let before = serde_json::to_value(&report).unwrap();
            let prepared = Prepared::fixture(report);
            assert_eq!(prepared.helper(), None);
            let report = prepared
                .finish(&paths, Err("must not reconcile".into()), Instant::now())
                .await;
            assert_eq!(serde_json::to_value(report).unwrap(), before);
        }
        assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    #[cfg(target_os = "macos")]
    async fn preserved_alternatives_do_not_fail_app_setup() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("user/Koidrive.app");
        let preserved = home.path().join("system/Koidrive.app");
        let service: service::Report = serde_json::from_value(serde_json::json!({
            "manager": "launchd", "name": "fixture", "state": "running"
        }))
        .unwrap();
        for change in [AppChange::Installed, AppChange::Unchanged] {
            let prepared = Prepared::from_install(Ok(AppInstall::Present {
                committed: Vec::new(),
                path: path.clone(),
                version: Version::parse("99.0.0").unwrap(),
                change,
                launch: AppLaunchResult::Requested,
                retired: Vec::new(),
                residue: Vec::new(),
                preserved: vec![preserved.clone()],
            }));
            assert!(!prepared.report.failed());
            assert_eq!(prepared.helper(), Some(path.join("Contents/Helpers/koi")));
            let report = prepared
                .finish(&Paths::under(home.path()), Ok(&service), Instant::now())
                .await;
            assert!(!report.failed());
            assert_eq!(report.problem(), None);
            assert_eq!(
                report.text(),
                format!(
                    "Koidrive 99.0.0 is installed in {}.\nAn older Koidrive app remains in {}.\n",
                    path.display(),
                    preserved.display()
                )
            );
            let mut encoded = serde_json::to_value(&report).unwrap();
            assert_eq!(encoded["preserved"], serde_json::json!([preserved]));
            assert!(encoded.get("problem").is_none());
            assert!(encoded.get("residue").is_none());
            assert_eq!(
                serde_json::from_value::<Report>(encoded.clone()).unwrap(),
                report
            );
            encoded.as_object_mut().unwrap().remove("preserved");
            let compatible = serde_json::from_value::<Report>(encoded).unwrap();
            assert!(!compatible.failed());
            assert!(
                serde_json::to_value(compatible)
                    .unwrap()
                    .get("preserved")
                    .is_none()
            );
        }
    }

    #[tokio::test]
    #[cfg(target_os = "macos")]
    async fn preserved_alternatives_do_not_mask_installer_problems() {
        let home = tempfile::tempdir().unwrap();
        for cleanup_failed in [false, true] {
            let residue = if cleanup_failed {
                vec![home.path().join("owned-stage")]
            } else {
                Vec::new()
            };
            let prepared = Prepared::from_install(Ok(AppInstall::Present {
                committed: Vec::new(),
                path: home.path().join("user/Koidrive.app"),
                version: Version::parse("99.0.0").unwrap(),
                change: AppChange::Installed,
                launch: if cleanup_failed {
                    AppLaunchResult::Requested
                } else {
                    AppLaunchResult::Failed("Launch failed".into())
                },
                retired: Vec::new(),
                residue: residue.clone(),
                preserved: vec![home.path().join("system/Koidrive.app")],
            }));
            assert!(prepared.report.failed());
            let Report::Present {
                problem,
                residue: remaining,
                preserved,
                ..
            } = prepared.report
            else {
                panic!()
            };
            assert!(problem.is_some());
            assert_eq!(remaining, residue);
            assert_eq!(preserved.len(), 1);
        }
    }

    #[tokio::test]
    async fn a_supplied_handoff_failure_is_reported_without_another_reconcile() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("Koidrive.app");
        let binary = path.join("Contents/Helpers/koi");
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::write(&binary, "must not execute").unwrap();
        let prepared = Prepared::fixture(Report::Present {
            committed: Vec::new(),
            path,
            version: "99.0.0".into(),
            change: Change::Replaced,
            launch: Launch::NotRequested,
            problem: None,
            retired: Vec::new(),
            residue: Vec::new(),
            preserved: Vec::new(),
        });
        let report = prepared
            .finish(
                &Paths::under(home.path()),
                Err("The handoff failed".into()),
                Instant::now(),
            )
            .await;
        assert!(
            matches!(report, Report::Present { problem: Some(problem), .. } if problem == "The handoff failed")
        );
        assert_eq!(std::fs::read(&binary).unwrap(), b"must not execute");
    }

    #[tokio::test]
    async fn an_unchanged_app_with_pending_retirements_still_requires_handoff() {
        let home = tempfile::tempdir().unwrap();
        for retired in [
            Vec::new(),
            vec![
                home.path().join("older.app"),
                home.path().join("previous.app"),
            ],
        ] {
            let pending = !retired.is_empty();
            let prepared = Prepared::fixture(Report::Present {
                committed: Vec::new(),
                path: home.path().join("Koidrive.app"),
                version: "99.0.0".into(),
                change: Change::Unchanged,
                launch: Launch::NotRequested,
                problem: None,
                retired: retired.clone(),
                residue: Vec::new(),
                preserved: Vec::new(),
            });
            assert_eq!(prepared.needs_handoff(), pending);
            let report = prepared
                .finish(
                    &Paths::under(home.path()),
                    Err("The recovered handoff failed".into()),
                    Instant::now(),
                )
                .await;
            let Report::Present {
                problem,
                retired: remaining,
                ..
            } = &report
            else {
                panic!()
            };
            assert_eq!(
                problem.as_deref(),
                pending.then_some("The recovered handoff failed")
            );
            assert_eq!(*remaining, retired);
            let encoded = serde_json::to_vec(&report).unwrap();
            assert_eq!(serde_json::from_slice::<Report>(&encoded).unwrap(), report);
        }
        assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn retirement_requires_a_successful_handoff_and_a_current_running_daemon() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        const TARGET: &str = "0.2.0-beta.3";
        for (state, daemon_status, discard) in [
            (None, Some((true, Some(TARGET))), false),
            (Some("stopped"), Some((true, Some(TARGET))), false),
            (Some("running"), None, false),
            (Some("running"), Some((false, Some(TARGET))), false),
            (Some("running"), Some((true, None)), false),
            (Some("running"), Some((true, Some("invalid"))), false),
            (Some("running"), Some((true, Some("0.1.0"))), false),
            (Some("running"), Some((true, Some("0.2.0-beta.2"))), false),
            (Some("running"), Some((true, Some(TARGET))), true),
            (
                Some("running"),
                Some((true, Some("0.2.0-beta.3+build"))),
                true,
            ),
            (Some("running"), Some((true, Some("0.2.0"))), true),
            (Some("running"), Some((true, Some("1.0.0"))), true),
        ] {
            let (_fixture, path, retired) = app_install::pending_retirements_fixture().await;
            assert_eq!(retired.len(), 2);
            let pending: Vec<_> = retired.iter().map(|old| old.bundle.clone()).collect();
            let selected_helper = std::fs::read(path.join("Contents/Helpers/koi")).unwrap();
            assert_eq!(selected_helper, TARGET.as_bytes());
            let home = tempfile::tempdir().unwrap();
            let paths = Paths::under(home.path());
            let connections = Arc::new(AtomicUsize::new(0));
            let daemon = daemon_status.map(|(running, version)| {
                let listener = koidrive_ipc::bind(&paths.socket()).unwrap();
                let connections = Arc::clone(&connections);
                let status = koidrive_driver::Status {
                    running,
                    version: version.map(str::to_owned),
                    ..Default::default()
                };
                tokio::spawn(async move {
                    let (stream, _) = listener.accept().await.unwrap();
                    let (reader, mut writer) = stream.into_split();
                    let request = koidrive_ipc::read_line(reader).await.unwrap();
                    assert!(matches!(
                        serde_json::from_str::<Request>(&request).unwrap(),
                        Request::Status
                    ));
                    connections.fetch_add(1, Ordering::SeqCst);
                    koidrive_ipc::write_line(&mut writer, &Response::Status { status })
                        .await
                        .unwrap();
                })
            });
            let handoff = state.map(|state| {
                serde_json::from_value::<service::Report>(serde_json::json!({
                    "manager": "launchd", "name": "fixture", "state": state
                }))
                .unwrap()
            });
            let prepared = Prepared {
                report: Report::Present {
                    committed: Vec::new(),
                    path: path.clone(),
                    version: TARGET.into(),
                    change: Change::Unchanged,
                    launch: Launch::NotRequested,
                    problem: None,
                    retired: pending.clone(),
                    residue: Vec::new(),
                    preserved: Vec::new(),
                },
                retired,
            };
            assert!(prepared.needs_handoff());
            let report = prepared
                .finish(
                    &paths,
                    handoff.as_ref().ok_or_else(|| "handoff failed".into()),
                    Instant::now() + Duration::from_secs(10),
                )
                .await;
            let contacted = state == Some("running") && daemon_status.is_some();
            if let Some(daemon) = daemon {
                if contacted {
                    daemon.await.unwrap();
                } else {
                    daemon.abort();
                    assert!(daemon.await.unwrap_err().is_cancelled());
                }
            }
            assert_eq!(connections.load(Ordering::SeqCst), usize::from(contacted));
            let Report::Present {
                retired,
                problem,
                residue,
                ..
            } = report
            else {
                panic!()
            };
            assert_eq!(problem.is_none(), discard, "{state:?} {daemon_status:?}");
            assert!(residue.is_empty());
            assert_eq!(retired, if discard { Vec::new() } else { pending.clone() });
            for old in pending {
                assert_eq!(old.exists(), !discard);
                if !discard {
                    assert_eq!(
                        std::fs::read(old.join("Contents/Helpers/koi")).unwrap(),
                        b"0.1.0"
                    );
                }
            }
            assert_eq!(
                std::fs::read(path.join("Contents/Helpers/koi")).unwrap(),
                selected_helper
            );
        }
    }

    #[tokio::test]
    async fn the_installed_helper_must_return_its_service_report() {
        if std::env::var_os("KOI_APP_HANDOFF_TEST").is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "app::tests::the_installed_helper_must_return_its_service_report",
                    "--nocapture",
                ])
                .env("KOI_APP_HANDOFF_TEST", "1")
                .kill_on_drop(true)
                .output()
                .await
                .unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let value = serde_json::json!({"manager":"launchd", "name":"example", "state":"off"});
        let envelope = serde_json::json!({"data":{"service": value}});
        let binary = helper(home.path(), &format!("printf '%s' '{envelope}'"));
        let report = reconcile(&binary, Instant::now() + Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(serde_json::to_value(report).unwrap(), value);
        let binary = helper(home.path(), "printf '%s' '{\"data\":{}}'");
        assert!(
            reconcile(&binary, Instant::now() + Duration::from_secs(10))
                .await
                .is_err()
        );
        let binary = helper(home.path(), &format!("printf '%s' '{envelope}'; exit 1"));
        assert!(
            reconcile(&binary, Instant::now() + Duration::from_secs(10))
                .await
                .is_err()
        );
        cancelling_a_helper_handoff_reaps_it_before_shutdown().await;
    }

    async fn cancelling_a_helper_handoff_reaps_it_before_shutdown() {
        let home = tempfile::tempdir().unwrap();
        let pid_file = home.path().join("pid");
        let binary = helper(
            home.path(),
            &format!("echo $$ > '{}'; exec /bin/sleep 60", pid_file.display()),
        );
        let task = tokio::spawn(async move {
            reconcile(&binary, Instant::now() + Duration::from_secs(60)).await
        });
        let pid = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(pid) = std::fs::read_to_string(&pid_file)
                    .ok()
                    .and_then(|text| text.trim().parse::<i32>().ok())
                {
                    break rustix::process::Pid::from_raw(pid).unwrap();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(rustix::process::test_kill_process(pid).is_ok());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        cancel_active().await;
        assert_eq!(
            rustix::process::test_kill_process(pid),
            Err(rustix::io::Errno::SRCH)
        );
    }

    #[tokio::test]
    async fn handoff_shutdown_stops_the_helper_process_group() {
        if std::env::var_os("KOI_APP_HANDOFF_DESCENDANT_TEST").is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "app::tests::handoff_shutdown_stops_the_helper_process_group",
                    "--nocapture",
                ])
                .env("KOI_APP_HANDOFF_DESCENDANT_TEST", "1")
                .kill_on_drop(true)
                .output()
                .await
                .unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let mut failures = Vec::new();
        for (timed_out, parent_exits) in [(false, false), (true, false), (false, true)] {
            let home = tempfile::tempdir().unwrap();
            let parent_file = home.path().join("parent");
            let descendant_file = home.path().join("descendant");
            let body = if parent_exits {
                format!(
                    "echo $$ > '{}'\n/bin/sh -c 'echo $$ > \"{}\"; exec /bin/sleep 60' &",
                    parent_file.display(),
                    descendant_file.display()
                )
            } else {
                format!(
                    "echo $$ > '{}'\n/bin/sh -c 'echo $$ > \"{}\"; exec /bin/sleep 60'\n:",
                    parent_file.display(),
                    descendant_file.display()
                )
            };
            let binary = helper(home.path(), &body);
            let task = tokio::spawn(async move {
                reconcile(&binary, Instant::now() + Duration::from_secs(10)).await
            });
            let (parent, descendant) = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let read = |path: &Path| {
                        std::fs::read_to_string(path)
                            .ok()?
                            .trim()
                            .parse::<i32>()
                            .ok()
                            .and_then(rustix::process::Pid::from_raw)
                    };
                    if let (Some(parent), Some(descendant)) =
                        (read(&parent_file), read(&descendant_file))
                    {
                        break (parent, descendant);
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_ne!(parent, descendant);
            assert!(rustix::process::test_kill_process(descendant).is_ok());
            if parent_exits {
                tokio::time::timeout(Duration::from_secs(2), async {
                    loop {
                        match rustix::process::waitid(
                            rustix::process::WaitId::Pid(parent),
                            rustix::process::WaitIdOptions::EXITED
                                | rustix::process::WaitIdOptions::NOHANG
                                | rustix::process::WaitIdOptions::NOWAIT,
                        ) {
                            Ok(Some(_)) => break,
                            Ok(None) => tokio::task::yield_now().await,
                            Err(error) => {
                                panic!("the helper was reaped before its stdout closed: {error}")
                            }
                        }
                    }
                })
                .await
                .unwrap();
            }
            if timed_out {
                assert!(task.await.unwrap().is_err());
            } else {
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
                cancel_active().await;
            }
            assert_eq!(
                rustix::process::test_kill_process(parent),
                Err(rustix::io::Errno::SRCH)
            );
            let stopped = tokio::time::timeout(Duration::from_secs(2), async {
                while rustix::process::test_kill_process(descendant).is_ok() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .is_ok();
            if !stopped {
                let _ = rustix::process::kill_process(descendant, rustix::process::Signal::KILL);
                failures.push((timed_out, parent_exits));
            }
        }
        assert!(
            failures.is_empty(),
            "surviving descendants, timeout modes: {failures:?}"
        );
    }

    #[test]
    fn large_handoff_envelopes_preserve_the_service_and_enforce_the_output_bound() {
        if std::env::var_os("KOI_APP_LARGE_HANDOFF_TEST").is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "app::tests::large_handoff_envelopes_preserve_the_service_and_enforce_the_output_bound", "--nocapture"])
                .env("KOI_APP_LARGE_HANDOFF_TEST", "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let home = tempfile::tempdir().unwrap();
            let service = serde_json::json!({"manager":"launchd", "name":"fixture", "state":"running"});
            let base = serde_json::json!({"data":{"service":service}, "decisions":[]});
            let mut large = base.clone();
            large["decisions"] = serde_json::json!([{"items":vec![serde_json::json!({"label":"x".repeat(200)}); 6000]}]);
            let limit = 16 * 1024 * 1024;
            let base_len = serde_json::to_vec(&base).unwrap().len();
            let mut boundary = base.clone();
            boundary["padding"] = serde_json::json!("x".repeat(limit - base_len - 13));
            let mut oversized = boundary.clone();
            oversized["padding"] = serde_json::json!("x".repeat(limit));
            for (value, succeeds) in [(large, true), (boundary, true), (oversized, false)] {
                let bytes = serde_json::to_vec(&value).unwrap();
                assert!(bytes.len() > 1024 * 1024);
                assert_eq!(bytes.len() <= limit, succeeds);
                let path = home.path().join("envelope.json");
                std::fs::write(&path, bytes).unwrap();
                let binary = helper(home.path(), &format!("exec /bin/cat '{}'", path.display()));
                let result = reconcile(&binary, Instant::now() + Duration::from_secs(10)).await;
                if succeeds {
                    assert_eq!(serde_json::to_value(result.unwrap()).unwrap(), service);
                } else {
                    assert!(result.unwrap_err().contains("too much output"));
                }
                assert!(HANDOFF.lock().await.is_none());
            }
        });
    }

    #[test]
    fn an_app_failure_keeps_the_existing_service_result() {
        let service = serde_json::json!({"manager":"launchd", "name":"example", "state":"off"});
        let value = serde_json::to_value(crate::report::WithApp {
            data: serde_json::json!({"service":service}),
            app: Some(Report::Failed {
                committed: Vec::new(),
                retired: Vec::new(),
                preserved: Vec::new(),
                problem: "offline".into(),
                residue: Vec::new(),
            }),
        })
        .unwrap();
        assert_eq!(value["service"], service);
        assert_eq!(value["app"]["state"], "failed");
    }
}
