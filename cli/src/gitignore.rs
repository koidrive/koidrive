// SPDX-License-Identifier: Apache-2.0

use std::path::{Path, PathBuf};

use clap::Subcommand;
use koidrive_driver::{DriverError, Paths};
use koidrive_ipc::{Request, Response};
use schemars::JsonSchema;
use serde::Serialize;

use crate::error::CliError;
use crate::folder::absolute;
use crate::formatting::{count, printable, size};
use crate::profile::unexpected;
use crate::report::Done;
use crate::terminal::Terminal;
use crate::{ipc, output};

#[derive(Subcommand)]
pub(crate) enum GitignoreAction {
    /// Show whether this device leaves out what .gitignore files exclude, and count the files only they exclude in
    /// each linked folder.
    Status {
        /// Count only this linked folder.
        folder: Option<PathBuf>,
    },
    /// Leave out what .gitignore files exclude. The files stay on this device and on the server.
    On,
    /// Sync what .gitignore files exclude, after showing how many files would start syncing.
    Off {
        /// Turn it off without asking.
        #[arg(long)]
        yes: bool,
    },
}

/// `koi gitignore on` and `koi gitignore off`'s result as JSON data.
#[derive(Serialize, JsonSchema)]
pub(crate) struct GitignoreData {
    /// Whether this device leaves out what .gitignore files exclude.
    follows: bool,
}

/// `koi gitignore status`'s result as JSON data.
#[derive(Serialize, JsonSchema)]
pub(crate) struct GitignoreStatusData {
    /// Whether this device leaves out what .gitignore files exclude.
    follows: bool,
    folders: Vec<GitignoreFolderData>,
}

/// The regular files of a linked folder that only its .gitignore files exclude, counted on disk when the command ran.
#[derive(Serialize, JsonSchema)]
pub(crate) struct GitignoreFolderData {
    space_id: String,
    path: String,
    files: u64,
    bytes: u64,
}

pub(crate) async fn run(
    paths: &Paths,
    action: GitignoreAction,
    terminal: &Terminal,
) -> Result<Done, CliError> {
    match action {
        GitignoreAction::Status { folder } => {
            let folder = folder.as_deref().map(absolute).transpose()?;
            let follows = follows(paths).await?;
            let folders = counted(paths, folder.as_deref()).await?;
            Ok(status(follows, folders))
        }
        GitignoreAction::On => set(paths, true).await,
        GitignoreAction::Off { yes } => {
            if !yes && follows(paths).await? {
                confirm(paths, terminal).await?;
            }
            set(paths, false).await
        }
    }
}

async fn follows(paths: &Paths) -> Result<bool, CliError> {
    match ipc::run(paths, Request::Gitignore).await? {
        Response::Gitignore { follows } => Ok(follows),
        other => Err(unexpected(other)),
    }
}

async fn set(paths: &Paths, follow: bool) -> Result<Done, CliError> {
    match ipc::run(paths, Request::SetGitignore { follow }).await? {
        Response::GitignoreSet { follows, already } => Ok(set_done(follows, already)),
        other => Err(unexpected(other)),
    }
}

fn set_done(follows: bool, already: bool) -> Done {
    let text = match (follows, already) {
        (true, false) => {
            "This device leaves out what .gitignore files exclude. The files stay on this device and on the server.\n"
        }
        (true, true) => "This device already leaves out what .gitignore files exclude.\n",
        (false, false) => {
            "This device syncs what .gitignore files exclude. To leave it out again, run: koi gitignore on\n"
        }
        (false, true) => "This device already syncs what .gitignore files exclude.\n",
    };
    let done = Done::ok(GitignoreData { follows }, text);
    match already {
        true => done.unchanged(),
        false => done.completed(),
    }
}

async fn counted(paths: &Paths, only: Option<&Path>) -> Result<Vec<GitignoreFolderData>, CliError> {
    let mut folders = match ipc::run(paths, Request::Status).await? {
        Response::Status { status } => status.folders,
        other => return Err(unexpected(other)),
    };
    if let Some(only) = only {
        let canonical = std::fs::canonicalize(only).ok();
        folders.retain(|folder| {
            let path = Path::new(&folder.path);
            path == only || canonical.as_deref() == Some(path)
        });
        if folders.is_empty() {
            return Err(CliError::from(DriverError::NotLinked(only.to_path_buf())));
        }
    }
    let mut counted = Vec::new();
    for folder in folders {
        let request = Request::GitignoreOnly {
            folder: PathBuf::from(&folder.path),
        };
        match ipc::run(paths, request).await? {
            Response::GitignoreOnly { files, bytes } => counted.push(GitignoreFolderData {
                space_id: folder.space_id,
                path: folder.path,
                files,
                bytes,
            }),
            other => return Err(unexpected(other)),
        }
    }
    Ok(counted)
}

fn status(follows: bool, folders: Vec<GitignoreFolderData>) -> Done {
    let render = |shown: &dyn Fn(&str) -> String| {
        let mut text = match follows {
            true => "This device leaves out what .gitignore files exclude.\n",
            false => "This device syncs what .gitignore files exclude.\n",
        }
        .to_owned();
        if !folders.is_empty() {
            text.push_str("Files only .gitignore files exclude:\n");
            for line in rows(&folders, shown) {
                text.push_str(&format!("{line}\n"));
            }
        }
        text.push_str(match follows {
            true => "Turn off: koi gitignore off\n",
            false => "Turn on: koi gitignore on\n",
        });
        text
    };
    let text = render(&|path| printable(path));
    let summary = render(&|path| output::path(Path::new(path)));
    Done::ok(GitignoreStatusData { follows, folders }, text).human(summary, "")
}

fn rows(folders: &[GitignoreFolderData], shown: &dyn Fn(&str) -> String) -> Vec<String> {
    let names: Vec<String> = folders.iter().map(|folder| shown(&folder.path)).collect();
    let width = names
        .iter()
        .map(|name| name.chars().count())
        .max()
        .unwrap_or(0);
    (names.iter().zip(folders))
        .map(|(name, folder)| {
            let amount = match folder.files {
                0 => "no files".to_owned(),
                files => format!("{}, {}", count(files, "file"), size(folder.bytes)),
            };
            format!("  {name:<width$}  {amount}")
        })
        .collect()
}

async fn confirm(paths: &Paths, terminal: &Terminal) -> Result<(), CliError> {
    if !terminal.guided() {
        return Err(CliError::usage(
            "koi gitignore off asks before it syncs what .gitignore files exclude; run it on a terminal, or add --yes. koi gitignore status counts those files",
        )
        .then(&["koi", "gitignore", "off", "--yes"]));
    }
    let progress = terminal.progress("Counting the files only .gitignore files exclude");
    let folders = match counted(paths, None).await {
        Ok(folders) => folders,
        Err(error) => {
            progress.fail();
            return Err(error);
        }
    };
    drop(progress);
    if folders.iter().all(|folder| folder.files == 0) {
        return Ok(());
    }
    terminal.message("These files start syncing:");
    for line in rows(&folders, &|path| output::path(Path::new(path))) {
        terminal.message(&line);
    }
    match terminal
        .confirm("Sync what .gitignore files exclude?", false)
        .await?
    {
        true => Ok(()),
        false => Err("stopped; nothing was changed".into()),
    }
}
