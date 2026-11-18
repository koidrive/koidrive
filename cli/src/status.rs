// SPDX-License-Identifier: Apache-2.0

use crate::error::CliError;
use crate::formatting::{need_you, now_ms, printable, size};
use crate::profile::unexpected;
use crate::report::{
    AccountData, Decision, Done, FolderData, RemedyData, StatusData, UpdateNoticeData,
};
use crate::resolve::{question_brief, questions};
use crate::{ipc, output, report, service, update};
use koidrive_driver::{FolderState, FolderStatus, Paths, Progress, Question, Status};
use koidrive_ipc::{Request, Response};
use std::path::Path;

/// `koi status`: this device, its folders and the decisions waiting in them. It succeeds whenever it reads the
/// status; with `check` it exits with the status of the most urgent condition (see [`report::check_exit`]).
pub(crate) async fn status(paths: &Paths, check: bool) -> Result<Done, CliError> {
    let status = match ipc::run(paths, Request::Status).await? {
        Response::Status { status } => status,
        other => return Err(unexpected(other)),
    };
    let questions = match status.folders.iter().any(|folder| folder.questions > 0) {
        true => questions(paths).await?,
        false => Vec::new(),
    };
    let conditions = report::conditions(&status, &questions);
    let exit = if check {
        report::check_exit(&conditions)
    } else {
        0
    };
    let service = service::inspect(paths, status.running).await;
    let tone = if conditions.is_empty() && !service.failed() && service.blocked.is_empty() {
        output::Tone::Neutral
    } else {
        output::Tone::Attention
    };
    let version = (status.version.as_deref()).unwrap_or(env!("CARGO_PKG_VERSION"));
    let notice = match update::Install::current() {
        Ok(install) => install.notice(&status.update, version),
        Err(_) => UpdateNoticeData::of(&status.update, version, None),
    };
    let text = status_text(&status, &questions, &service.status_line(), &notice);
    let summary = status_summary(&status, &questions, &service, &notice);
    let data = StatusData {
        account: status.account.as_ref().map(AccountData::of),
        running: status.running,
        paused: status.paused,
        folders: status.folders.iter().map(FolderData::of).collect(),
        conditions,
        service,
        update: notice,
    };
    let mut done = Done::ok(data, text).human(summary, "");
    done.decisions = questions.iter().map(Decision::of).collect();
    done.exit = exit;
    done.tone = tone;
    if status.account.is_none() {
        done = done.then(&["koi", "setup"]);
    }
    if status.paused {
        done = done.then(&["koi", "resume"]);
    }
    for (path, command) in (status.folders.iter())
        .filter_map(|folder| Some((folder.path.as_deref()?, RemedyData::of(folder)?.command()?)))
    {
        done = done.then(&["koi", "folder", command, path]);
    }
    Ok(done)
}

pub(crate) fn status_summary(
    status: &Status,
    questions: &[Question],
    service: &service::Report,
    notice: &UpdateNoticeData,
) -> String {
    let mut text = match &status.account {
        Some(account) => format!("Connected as {}.\n", printable(&account.device_name)),
        None => "This device is not set up. Run koi setup.\n".to_owned(),
    };
    text.push_str(&service.summary());
    if status.paused {
        text.push_str("Sync is paused. Run koi resume to continue.\n");
    }
    if status.folders.is_empty() && status.account.is_some() {
        text.push_str("No linked folders. Run koi folder link <folder> --space <name>.\n");
    }
    for folder in &status.folders {
        let state = match folder.state {
            FolderState::Stopped => "Not syncing",
            FolderState::Connecting => "Connecting",
            FolderState::Bootstrapping => "Preparing the first sync",
            FolderState::CatchingUp => "Syncing",
            FolderState::UpToDate => "Up to date",
            FolderState::Offline if folder.unreachable().is_some() => "Offline",
            FolderState::Offline => "Offline. Check your connection",
            FolderState::Halted => "Sync stopped",
        };
        let name = match &folder.path {
            Some(root) => output::path(Path::new(root)),
            None => format!("The folder of space {}", printable(&folder.space_id)),
        };
        text.push_str(&format!("{name}: {state}.\n"));
        if let Some(progress) = &folder.progress {
            text.push_str(&format!("  {}\n", progress_text(progress)));
        }
        if let Some(problem) = &folder.problem {
            text.push_str(&format!("  {}\n", printable(problem)));
            if let Some((root, remedy)) = folder.path.as_deref().zip(RemedyData::of(folder)) {
                let path = output::command_path(Path::new(root));
                match remedy {
                    RemedyData::Relink => text.push_str(&format!(
                        "  If this copy is the one to keep, run koi folder relink {path}. Nothing it lacks is deleted on the server.\n"
                    )),
                    RemedyData::Restart | RemedyData::Root => text.push_str(&format!(
                        "  Fix the problem above, then run koi folder restart {path}\n"
                    )),
                    RemedyData::Unlink => {
                        text.push_str(&format!("  To finish, run koi folder unlink {path}\n"))
                    }
                    RemedyData::Membership => text.push_str(&format!(
                        "  Ask the space's owner to add this device again. If the space is gone, run koi folder unlink {path}; its files stay where they are.\n"
                    )),
                    RemedyData::Retry | RemedyData::Owner | RemedyData::SetUpAgain => {}
                }
            }
        }
        if let Some(root) = &folder.path {
            text.push_str(&held(folder, root));
        }
        if let Some(wait) = &folder.wait {
            text.push_str(&format!("  {}\n", output::wait_cause(wait, now_ms())));
        }
        if let Some(cause) = &folder.recovery {
            let described = koidrive_driver::describe(cause);
            if folder.problem.as_deref() != Some(described.as_str()) {
                let cause = match cause.trigger {
                    koidrive_engine::question::RecoveryTrigger::SpaceRewound => {
                        "The shared folder was rewound or restored on the server.".to_owned()
                    }
                    koidrive_engine::question::RecoveryTrigger::StoreRolledBack => {
                        "The folder's sync state is older than its history on the server."
                            .to_owned()
                    }
                    koidrive_engine::question::RecoveryTrigger::StoreOlderThanDisk => {
                        "The folder's sync state is older than its files on disk.".to_owned()
                    }
                };
                text.push_str(&format!("  {cause} Recovering sync.\n"));
            }
        }
        for question in questions
            .iter()
            .filter(|question| question.space_id == folder.space_id)
        {
            text.push_str(&question_brief(
                question,
                (status.account.as_ref()).map(|account| account.device_id.as_str()),
            ));
        }
    }
    if let Some(latest) = &notice.latest {
        text.push_str(&format!(
            "Update: Koidrive {} is out (this device runs {}).",
            printable(latest),
            printable(&notice.version)
        ));
        if let Some(command) = &notice.command {
            text.push_str(&format!(" Run: {}\n", printable(command)));
        } else if let Some(url) = &notice.url {
            text.push_str(&format!(" Download: {}\n", printable(url)));
        } else {
            text.push('\n');
        }
    }
    text
}

pub(crate) fn status_text(
    status: &Status,
    questions: &[Question],
    service: &str,
    notice: &UpdateNoticeData,
) -> String {
    let mut text = match &status.account {
        Some(account) => format!(
            "Device: {} ({}) on {}\n",
            printable(&account.device_name),
            printable(&account.device_id),
            printable(&account.server)
        ),
        None => "Device: not set up; run koi setup\n".to_owned(),
    };
    text.push_str(&format!(
        "Daemon: {}\n",
        if status.running {
            "running"
        } else {
            "not running"
        }
    ));
    text.push_str(service);
    if let Some(latest) = &notice.latest {
        text.push_str(&format!(
            "Update: Koidrive {} is out (this device runs {}).",
            printable(latest),
            printable(&notice.version),
        ));
        if let Some(command) = &notice.command {
            text.push_str(&format!(" Run: {}\n", printable(command)));
        } else if let Some(url) = &notice.url {
            text.push_str(&format!(" Download: {}\n", printable(url)));
        } else {
            text.push('\n');
        }
    }
    if status.paused {
        text.push_str("Sync: paused. To sync again, run: koi resume\n");
    }
    if status.folders.is_empty() {
        text.push_str("No linked folders; run koi folder link.\n");
    }
    for folder in &status.folders {
        text.push_str(&folder_text(folder));
        for question in (questions.iter()).filter(|question| question.space_id == folder.space_id) {
            text.push_str(&question_brief(
                question,
                (status.account.as_ref()).map(|account| account.device_id.as_str()),
            ));
        }
    }
    text
}

pub(crate) fn folder_text(folder: &FolderStatus) -> String {
    let seq = match (folder.last_seq, folder.head_seq) {
        (Some(last), Some(head)) => format!("seq {last} of {head}"),
        (Some(last), None) => format!("seq {last}"),
        _ => "not read yet".to_owned(),
    };
    let state = match folder.state {
        FolderState::Stopped => "Stopped",
        FolderState::Connecting => "Connecting",
        FolderState::Bootstrapping => "Bootstrapping",
        FolderState::CatchingUp => "Syncing",
        FolderState::UpToDate => "UpToDate",
        FolderState::Offline => "Offline",
        FolderState::Halted => "Halted",
    };
    let path = match &folder.path {
        Some(root) => printable(root),
        None => "(path unknown)".to_owned(),
    };
    let mut text = format!(
        "{path}  space {} ({})  {state}  {seq}, {} remote nodes{}, {}\n",
        printable(&folder.space_name),
        printable(&folder.space_id),
        folder.remote_nodes,
        folder
            .local_entries
            .map(|entries| format!(", {entries} local entries"))
            .unwrap_or_default(),
        need_you(folder.questions)
    );
    if let Some(progress) = &folder.progress {
        text.push_str(&format!("  {}\n", progress_text(progress)));
    }
    if let Some(problem) = &folder.problem {
        text.push_str(&format!("  {}\n", printable(problem)));
    }
    if let Some(root) = &folder.path {
        text.push_str(&held(folder, root));
    }
    if let Some(cause) = &folder.recovery {
        let described = koidrive_driver::describe(cause);
        if folder.problem.as_deref() != Some(described.as_str()) {
            text.push_str(&format!("  {described}\n"));
        }
    }
    text
}

fn progress_text(progress: &Progress) -> String {
    let files = |count: u64, way: &str| match count {
        0 => None,
        1 => Some(format!("1 file to {way}")),
        count => Some(format!("{count} files to {way}")),
    };
    let mut parts: Vec<String> = [
        files(progress.files_to_upload, "upload"),
        files(progress.files_to_download, "download"),
    ]
    .into_iter()
    .flatten()
    .collect();
    parts.push(format!(
        "{} of {}",
        size(progress.bytes_done),
        size(progress.bytes_total)
    ));
    if let Some(current) = &progress.current {
        parts.push(format!("now {}", printable(current)));
    }
    parts.join("; ")
}

fn held(folder: &FolderStatus, root: &str) -> String {
    let mut text = String::new();
    for disk in &folder.held_disks {
        text.push_str(&format!("  {}\n", printable(&disk.describe(root))));
    }
    if !folder.kept_entries.is_empty() {
        let kept = koidrive_driver::describe_kept(root, &folder.kept_entries);
        text.push_str(&format!("  {}\n", printable(&kept)));
    }
    text
}
