// SPDX-License-Identifier: Apache-2.0

use crate::args::FolderAction;
use crate::error::CliError;
use crate::formatting::printable;
use crate::profile::unexpected;
use crate::report::{Done, FolderData, UnlinkedData};
use crate::{ipc, output, report, service};
use koidrive_driver::{FolderState, FolderStatus, Paths};
use koidrive_ipc::{Code, Key, Request, Response};
use std::path::{Path, PathBuf};

/// Runs a folder command in the daemon, or on its own when no daemon runs.
pub(crate) async fn folder(paths: &Paths, action: FolderAction) -> Result<Done, CliError> {
    let request = match action {
        FolderAction::Link { folder, space } => {
            let (folder, outcome) = link(paths, absolute(&folder)?, space, None).await?;
            let text = linked(&folder, outcome);
            return Ok(Done {
                outcome,
                tone: if outcome == report::Outcome::Ok {
                    output::Tone::Completed
                } else {
                    output::Tone::Neutral
                },
                ..Done::ok(FolderData::of(&folder), text)
                    .human(linked_summary(&folder, outcome), "")
            });
        }
        FolderAction::Unlink { folder } => Request::Unlink {
            folder: absolute(&folder)?,
        },
        FolderAction::Restart { folder, request } => {
            let folder = absolute(&folder)?;
            let key = Key::restart(&folder);
            Request::Restart { folder }.once(request.request_id, key)
        }
        FolderAction::Retry {
            folder,
            replan,
            request,
            ..
        } => {
            let folder = absolute(&folder)?;
            let key = Key::retry(&folder, replan);
            Request::Lift { folder, replan }.once(request.request_id, key)
        }
    };
    match ipc::run(paths, request).await? {
        Response::Unlinked { space_id, path } => {
            let summary = format!(
                "Unlinked {}. Its files and .koidrive folder stay on this device.\n",
                output::path(&path)
            );
            let text = format!(
                "Unlinked {} from space {}. Its files and its .koidrive folder stay.\n",
                printable(&path.display().to_string()),
                printable(&space_id)
            );
            let data = UnlinkedData {
                space_id: Some(space_id),
                path: path.display().to_string(),
            };
            Ok(Done::ok(data, text).human(summary, "").completed())
        }
        Response::NotLinked { path } => {
            let summary = format!("{} is not linked. Nothing changed.\n", output::path(&path));
            let path = path.display().to_string();
            let text = format!(
                "{} is not linked, so there is nothing to unlink.\n",
                printable(&path)
            );
            let data = UnlinkedData {
                space_id: None,
                path,
            };
            Ok(Done::ok(data, text).human(summary, "").unchanged())
        }
        Response::Restarted { folder } => restarted(folder),
        other => Err(unexpected(other)),
    }
}

pub(crate) async fn link(
    paths: &Paths,
    folder: PathBuf,
    space: String,
    approval: Option<koidrive_driver::LinkApproval>,
) -> Result<(FolderStatus, report::Outcome), CliError> {
    let request = match approval {
        Some(approval) => {
            service::reconcile(paths, false).await;
            Request::LinkApproved {
                folder,
                space,
                approval,
            }
        }
        None => Request::Link { folder, space },
    };
    match ipc::run(paths, request).await? {
        Response::Linked { folder } => Ok((folder, report::Outcome::Ok)),
        Response::AlreadyLinked { folder } => Ok((folder, report::Outcome::Unchanged)),
        other => Err(unexpected(other)),
    }
}

pub(crate) fn linked(folder: &FolderStatus, outcome: report::Outcome) -> String {
    let path = printable(&folder.path);
    let space = format!(
        "space {} ({})",
        printable(&folder.space_name),
        printable(&folder.space_id)
    );
    match outcome {
        report::Outcome::Unchanged => format!("{path} is already linked to {space}.\n"),
        _ => format!("Linked {path} to {space}.\n"),
    }
}

pub(crate) fn linked_summary(folder: &FolderStatus, outcome: report::Outcome) -> String {
    let path = output::path(Path::new(&folder.path));
    let space = printable(&folder.space_name);
    match outcome {
        report::Outcome::Unchanged => format!("{path} is already linked to {space}.\n"),
        _ => format!("Linked {path} to {space}.\n"),
    }
}

/// What `folder restart` reports for the folder as the restart left it.
pub(crate) fn restarted(folder: FolderStatus) -> Result<Done, CliError> {
    let path = printable(&folder.path);
    let text = match folder.state {
        FolderState::Halted => {
            let problem = printable(folder.problem.as_deref().unwrap_or_default());
            return Err(CliError::new(
                Code::Halted,
                format!("{path} cannot sync: {problem}"),
            ));
        }
        _ => format!("Syncing {path} again.\n"),
    };
    let summary = format!("Syncing {} again.\n", output::path(Path::new(&folder.path)));
    Ok(Done::ok(FolderData::of(&folder), text).human(summary, ""))
}

/// The folder as an absolute path, since the daemon does not share this command's working directory.
pub(crate) fn absolute(folder: &Path) -> Result<PathBuf, CliError> {
    std::path::absolute(folder).map_err(|error| {
        CliError::usage(format!(
            "{}: {error}",
            printable(&folder.display().to_string())
        ))
    })
}
