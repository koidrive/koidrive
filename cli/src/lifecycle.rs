// SPDX-License-Identifier: Apache-2.0

use crate::args::Command;
use crate::device::DeviceAction;
use crate::dispatch::execute;
use crate::error::CliError;
use crate::folder::FolderAction;
use crate::report::{Done, WithService};
use crate::{app, app_install, ipc, output, pairing, report, service, setup, terminal};
use koidrive_driver::{FolderState, Paths};
use koidrive_ipc::{Code, Request, Response};
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;

/// Runs the command. After one that may change the folders this profile links or its pairing, `service start` or
/// `_reconcile`, it brings the background service in line with the profile and reports it as `service`.
pub(crate) async fn run(
    paths: Paths,
    command: Command,
    json: bool,
    verbose: bool,
    terminal: Arc<terminal::Terminal>,
) -> Result<Done, CliError> {
    let lifecycle_deadline = matches!(command, Command::Reconcile { .. })
        .then(|| tokio::time::Instant::now() + std::time::Duration::from_secs(380));
    let app_policy = match &command {
        Command::Setup { no_app, .. } => Some(if *no_app {
            Err("opt_out")
        } else if !terminal.guided() {
            Err("unattended_setup")
        } else {
            Ok(app_install::AppLaunch::Open)
        }),
        Command::Reconcile { no_app } => Some(if *no_app {
            Err("service_handoff")
        } else if std::env::current_exe().is_ok_and(|binary| app::bundled(&binary)) {
            Err("bundled_helper")
        } else {
            Ok(app_install::AppLaunch::Preserve)
        }),
        _ => None,
    };
    let setup_app = matches!(command, Command::Setup { .. });
    let asked = matches!(command, Command::Service { .. });
    let shows_state = asked || matches!(command, Command::Reconcile { .. });
    let reconciles = shows_state
        || matches!(
            command,
            Command::Folder {
                action: FolderAction::Link { .. } | FolderAction::Unlink { .. }
            } | Command::Device {
                action: DeviceAction::Leave { .. }
            } | Command::Resume
                | Command::Setup { .. }
                | Command::Join { .. }
                | Command::Server {
                    action: setup::ServerAction::Claim { .. }
                        | setup::ServerAction::Deploy { .. }
                        | setup::ServerAction::AddDevice { .. }
                }
        );
    let done = execute(paths.clone(), command, json, verbose, terminal.clone()).await?;
    let service = if reconciles {
        let _progress = terminal
            .setup_guided()
            .then(|| terminal.progress("Starting background sync..."));
        let report = service::reconcile(&paths, asked).await;
        let failed = shows_state
            && report.failed()
            && (asked || report.state != service::State::SwitchedOff || report.problem.is_some());
        let failure = failed.then(|| report.failure_message());
        if let Some(message) = failure {
            return Err(CliError {
                partial: Some(Box::new(with_service(done, report))),
                ..CliError::new(Code::Failed, message)
            });
        }
        Some(report)
    } else {
        None
    };
    let app = match app_policy {
        Some(policy) => Some(match policy {
            Ok(launch) => {
                let _progress = (terminal.setup_guided() && cfg!(target_os = "macos"))
                    .then(|| terminal.progress("Preparing the Koidrive app..."));
                let deadline = lifecycle_deadline.unwrap_or_else(|| {
                    tokio::time::Instant::now() + std::time::Duration::from_secs(400)
                });
                app::install(&paths, &service::this_version(), launch, deadline).await
            }
            Err(reason) => (app::Report::skipped(reason), None),
        }),
        None => None,
    };
    if terminal.setup_guided() {
        finish_setup(&paths, done, service, app, &terminal).await
    } else {
        finish_lifecycle(done, service, app, setup_app, shows_state)
    }
}

pub(crate) async fn finish_setup(
    paths: &Paths,
    done: Done,
    service: Option<service::Report>,
    app: Option<(app::Report, Option<service::Report>)>,
    terminal: &terminal::Terminal,
) -> Result<Done, CliError> {
    let final_service = app
        .as_ref()
        .and_then(|(_, report)| report.as_ref())
        .or(service.as_ref());
    let progress = terminal.continue_progress("Starting background sync...");
    let status = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        ipc::run(paths, Request::Status),
    )
    .await;
    drop(progress);
    let status = match status {
        Ok(Ok(Response::Status { status })) => Some(status),
        Ok(Ok(_)) => None,
        Ok(Err(error)) => {
            terminal.message(&error.message);
            None
        }
        Err(_) => None,
    };
    let mut summary = done.note.clone();
    match final_service {
        Some(report) if report.state == service::State::Off => {
            terminal.message("Turn on background sync in config.toml, then run koi service start.");
            summary =
                "Folder linked. Background sync is off in config.toml.\nStart sync: koi service start\n".into();
        }
        Some(report)
            if report.failed()
                || !report.blocked.is_empty()
                || report.state != service::State::Running =>
        {
            if report.state == service::State::Running {
                if let Some(problem) = &report.problem {
                    terminal.message(problem);
                }
                for blocked in &report.blocked {
                    terminal.message(&format!(
                        "macOS can't read {} in the background.",
                        setup::shown_folder(Path::new(&blocked.folder))
                    ));
                    if blocked.waiting {
                        terminal.message("If macOS asks for folder access, click Allow.");
                    } else {
                        terminal.message("Allow folder access in System Settings > Privacy & Security > Files & Folders, or add koi under Full Disk Access.");
                        terminal.message(&format!(
                            "Then run: koi folder restart {}",
                            pairing::shell_word(&blocked.folder)
                        ));
                    }
                }
            } else {
                let reason = report.text();
                for line in reason.lines() {
                    terminal.message(line);
                }
            }
            summary =
                "Folder linked. Background sync needs attention.\nCheck sync: koi status\n".into();
        }
        _ => {
            match status {
                Some(status) if status.paused => {
                    terminal.message("Sync is paused.");
                    summary = "Folder linked. Sync is paused.\nResume sync: koi resume\n".into();
                }
                Some(status)
                    if final_service.is_some() && status.running && !status.folders.is_empty() =>
                {
                    let blocked = status.folders.iter().find(|folder| {
                        folder.questions > 0
                            || matches!(
                                folder.state,
                                FolderState::Stopped | FolderState::Halted | FolderState::Offline
                            )
                    });
                    if let Some(folder) = blocked {
                        let state = match folder.state {
                            FolderState::Stopped => Some("stopped"),
                            FolderState::Halted => Some("halted"),
                            FolderState::Offline => Some("offline"),
                            _ => None,
                        };
                        if let Some(state) = state {
                            terminal.message(&format!(
                                "Sync is {state} for {}.",
                                setup::shown_folder(Path::new(&folder.path))
                            ));
                        }
                        if folder.questions > 0 {
                            terminal.message(&format!(
                                "Review pending decisions for {}: koi resolve",
                                setup::shown_folder(Path::new(&folder.path))
                            ));
                        }
                        if let Some(problem) = &folder.problem {
                            terminal.message(problem);
                        }
                        summary =
                            "Folder linked. Sync needs attention.\nCheck sync: koi status\n".into();
                    } else {
                        terminal.complete("Background sync is running");
                    }
                }
                _ => {
                    summary = "Folder linked. Could not confirm background sync.\nCheck sync: koi status\n".into();
                }
            }
        }
    }
    if let Some(report) = final_service
        && report.manager == service::ServiceManager::Systemd
        && report.state == service::State::Running
        && report.lingers == Some(false)
    {
        terminal.message("Background sync stops when you log out. To keep it running, run: sudo loginctl enable-linger \"$USER\"");
    }
    if let Some((report, _)) = &app {
        match report {
            app::Report::Present { .. } if !report.failed() => {
                terminal.complete("Koidrive app is ready")
            }
            _ if report.failed() => {
                for line in report.text().lines() {
                    terminal.message(line);
                }
            }
            _ => {}
        }
    }
    let mut result = finish_lifecycle(done, service, app, true, false);
    match &mut result {
        Ok(done) => {
            done.text.clear();
            done.note = summary;
            done.human = None;
        }
        Err(error) => {
            if let Some(done) = &mut error.partial {
                done.text.clear();
                done.note.clear();
                done.human = None;
            }
        }
    }
    result
}

fn with_service(mut done: Done, report: service::Report) -> Done {
    let data = match done.data.take() {
        Value::Null => Value::Object(serde_json::Map::new()),
        data => data,
    };
    done.data = serde_json::to_value(WithService {
        data,
        service: report,
    })
    .expect("the CLI's own types serialize");
    done
}

pub(crate) fn finish_lifecycle(
    mut done: Done,
    service: Option<service::Report>,
    app: Option<(app::Report, Option<service::Report>)>,
    setup_app: bool,
    shows_state: bool,
) -> Result<Done, CliError> {
    let (app, handoff) = app.map_or((None, None), |(app, service)| (Some(app), service));
    let service = if setup_app {
        handoff.or(service)
    } else {
        service
    };
    if let Some(report) = service {
        if report.failed() || !report.blocked.is_empty() {
            done.tone = output::Tone::Attention;
        }
        let text = match report.text() {
            text if text.is_empty() && shows_state => report.status_line(),
            text => text,
        };
        if !text.is_empty() {
            let human = done.human.get_or_insert_with(|| report::HumanOutput {
                text: done.text.clone(),
                note: done.note.clone(),
            });
            human.text.push_str(&report.summary());
        }
        done = with_service(done, report);
        done.text.push_str(&text);
    }
    if let Some(report) = app {
        let failed = setup_app && report.failed();
        done.text.push_str(&report.text());
        if let Some(human) = &mut done.human {
            human.text.push_str(&report.text());
        }
        done.data = serde_json::to_value(report::WithApp {
            data: done.data,
            app: Some(report),
        })
        .expect("the CLI's own types serialize");
        if failed {
            return Err(CliError {
                partial: Some(Box::new(done)),
                ..CliError::new(
                    Code::Failed,
                    "The device is connected, but app setup is incomplete; run koi setup to retry, or use --no-app",
                )
            });
        }
    }
    Ok(done)
}
