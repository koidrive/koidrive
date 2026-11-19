// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use clap::Subcommand;
use koidrive_driver::Paths;
use koidrive_ipc::{GitignoreFolder, Request, Response};
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
    /// Folders the count leaves out because Koidrive can't read them or their rules file, as absolute paths.
    unreadable: Vec<String>,
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
    let folders: BTreeMap<String, String> = match ipc::run(paths, Request::Status).await? {
        Response::Status { status } => (status.folders.into_iter())
            .filter_map(|folder| {
                Some((folder.space_id, folder.path?.to_string_lossy().into_owned()))
            })
            .collect(),
        other => return Err(unexpected(other)),
    };
    let linked = match only {
        Some(only) => vec![GitignoreFolder::Folder(only.to_path_buf())],
        None => folders
            .keys()
            .cloned()
            .map(GitignoreFolder::Space)
            .collect(),
    };
    let mut counted = Vec::new();
    for of in linked {
        match ipc::run(paths, Request::GitignoreOnly { of }).await? {
            Response::GitignoreUnlinked { .. } => {}
            Response::GitignoreOnly {
                space_id,
                files,
                bytes,
                unreadable,
            } => counted.push(GitignoreFolderData {
                path: match (folders.get(&space_id), only) {
                    (Some(path), _) => path.clone(),
                    (None, only) => {
                        only.map_or_else(String::new, |only| only.to_string_lossy().into_owned())
                    }
                },
                space_id,
                files,
                bytes,
                unreadable: (unreadable.iter())
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect(),
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
        for line in unreadable(&folders, shown) {
            text.push_str(&format!("{line}\n"));
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

fn unreadable(folders: &[GitignoreFolderData], shown: &dyn Fn(&str) -> String) -> Vec<String> {
    let mut lines: Vec<String> = (folders.iter())
        .flat_map(|folder| &folder.unreadable)
        .map(|path| format!("  {}", shown(path)))
        .collect();
    if !lines.is_empty() {
        lines.insert(0, "Not counted, because Koidrive can't read them:".into());
    }
    lines
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
    let shown = |path: &str| output::path(Path::new(path));
    let unreadable = unreadable(&folders, &shown);
    if unreadable.is_empty() && folders.iter().all(|folder| folder.files == 0) {
        return Ok(());
    }
    terminal.message("These files start syncing:");
    for line in rows(&folders, &shown).iter().chain(&unreadable) {
        terminal.message(line);
    }
    match terminal
        .confirm("Sync what .gitignore files exclude?", false)
        .await?
    {
        true => Ok(()),
        false => Err("stopped; nothing was changed".into()),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use koidrive_driver::Driver;
    use koidrive_ipc::{bind, read_line, write_line};

    use super::*;
    use crate::ipc::tests::{SPACE, linked_at, linked_profile};

    #[tokio::test]
    async fn a_folder_unlinked_while_the_folders_are_counted_drops_only_its_row() {
        let (_dir, paths, root) = linked_profile();
        linked_at(&paths, SPACE, &root);
        fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        fs::write(root.join("a.log"), "abc").unwrap();
        let other = root.with_file_name("Other");
        fs::create_dir(&other).unwrap();
        let unlinked = "0190f0a0-0000-7000-8000-000000000002";
        linked_at(&paths, unlinked, &other);
        let driver = Driver::open(paths.clone()).unwrap();
        let daemon = bind(&paths.socket()).unwrap();
        let served = tokio::spawn(async move {
            loop {
                let (stream, _) = daemon.accept().await.unwrap();
                let (mut reader, mut writer) = stream.into_split();
                let line = read_line(&mut reader).await.unwrap();
                let request: Request = serde_json::from_str(&line).unwrap();
                if matches!(&request, Request::GitignoreOnly { of: GitignoreFolder::Space(space) } if space == unlinked)
                {
                    driver.unlink(&other).await.unwrap();
                }
                let response = crate::ipc::handle(&driver, request).await;
                write_line(&mut writer, &response).await.unwrap();
            }
        });

        let rows = counted(&paths, None).await;
        served.abort();
        let rows: Vec<_> = (rows.unwrap().into_iter())
            .map(|row| (row.space_id, row.path, row.files, row.bytes))
            .collect();
        assert_eq!(
            rows,
            [(SPACE.to_owned(), root.to_string_lossy().into_owned(), 1, 3)]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_folder_asked_for_while_it_is_being_unlinked_is_left_out() {
        let (_dir, paths, root) = linked_profile();
        linked_at(&paths, SPACE, &root);
        let parked = (root.join(koidrive_fs::PRIVATE_DIR)).join(koidrive_fs::PARKED_DIR);
        fs::create_dir_all(&parked).unwrap();
        fs::write(parked.join("held.txt"), "held").unwrap();
        let driver = Driver::open(paths.clone()).unwrap();
        let store = paths.store(SPACE).unwrap();
        let busy = rusqlite::Connection::open(&store).unwrap();
        (busy.execute_batch("PRAGMA locking_mode = EXCLUSIVE; BEGIN EXCLUSIVE; COMMIT;")).unwrap();
        let server = tokio::spawn(crate::ipc::serve(
            bind(&paths.socket()).unwrap(),
            driver.clone(),
        ));
        let unlinking = tokio::spawn({
            let (driver, root) = (driver.clone(), root.clone());
            async move { driver.unlink(&root).await.map(drop) }
        });
        while !store.with_file_name("unlinking").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        let rows = counted(&paths, Some(&root)).await;
        let still_linked = (driver.status().folders.iter()).any(|folder| folder.space_id == SPACE);
        drop(busy);
        unlinking.await.unwrap().unwrap();
        server.abort();
        assert!(still_linked);
        assert_eq!(rows.unwrap().len(), 0);
    }
}
