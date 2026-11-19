// SPDX-License-Identifier: Apache-2.0

//! `koi history trash` and `koi history versions`: what the space of a linked folder keeps after a delete or
//! an edit. Both read the server through its HTTP RPC route with this profile's credential. They change no file here or
//! on the server, and without a daemon they read the profile without writing to it. `koi history restore` reads
//! the same listings to find what to restore and records the request in the folder's store, for the folder's engine to
//! send (design §6.3). `koi history requests` lists those requests and how each went.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use koidrive_driver::{
    Driver, FolderState, FolderStatus, ItemKind, Paths, RestoreRejection, RestoreRequest,
    RestoreState, RestoreStatus, RestoreWait,
};
use koidrive_engine::names::name_key;
use koidrive_ipc::{Key, Reason, Request, RequestId, Response};
use koidrive_net::Auth;
use koidrive_proto::v1::{
    ListTrash, ListTree, ListVersions, NodeKind, NodeState, Trashed, VersionInfo,
    rpc_request::Call, rpc_response,
};

use crate::error::CliError;
use crate::ipc;
use crate::report::{
    Done, NodeType, Outcome, RequestData, RequestsData, RestoreAskedData, TrashData, TrashEntry,
    VersionData, VersionsData,
};
use crate::request;
use crate::{ago, now_ms, printable, signed_in, size, unexpected};

mod locate;
mod server;

#[cfg(test)]
use locate::below;
use locate::{Located, lexical, linked_folders, locate, starts_with};
use server::{
    Deleted, content_size, deletions, device_names, find, kind, names_of, trash_nodes, tree,
    versions_and_current,
};
#[cfg(test)]
use server::{every_page, versions_of};

#[derive(clap::Subcommand)]
pub enum HistoryAction {
    /// List what was deleted in a linked folder and is still in its space's trash.
    Trash {
        /// A linked folder, or a folder inside one.
        #[arg(default_value = ".")]
        folder: PathBuf,
    },
    /// List the versions the server keeps of a file in a linked folder, newest first.
    Versions { file: PathBuf },
    /// Bring a deleted file or folder back from its space's trash to where it was, or make an earlier version of a
    /// file current again. The daemon sends the request with the folder's next commit.
    Restore {
        /// The deleted file or folder at the path it had, or the file whose version to restore.
        path: PathBuf,
        /// Restore this version of the file, as koi history versions lists it.
        #[arg(long)]
        version: Option<String>,
        /// The deleted node to restore, as koi history trash --json lists it, when several deletions had this
        /// path.
        #[arg(long, conflicts_with = "version")]
        node: Option<String>,
        #[command(flatten)]
        request: crate::RequestArg,
    },
    /// List the restores asked for in a linked folder and how each one went.
    Requests {
        /// A linked folder, or a folder inside one.
        #[arg(default_value = ".")]
        folder: PathBuf,
    },
}

pub async fn run(paths: &Paths, action: HistoryAction) -> Result<Done, CliError> {
    match action {
        HistoryAction::Trash { folder } => trash(paths, &folder).await,
        HistoryAction::Versions { file } => versions(paths, &file).await,
        HistoryAction::Restore {
            path,
            version,
            node,
            request,
        } => restore(paths, &path, version, node, request.request_id).await,
        HistoryAction::Requests { folder } => requests(paths, &folder).await,
    }
}

/// `koi history trash [folder]`: the deletions in the trash of the folder's space, or of the part under `folder`
/// when it is a folder inside a linked one.
async fn trash(paths: &Paths, folder: &Path) -> Result<Done, CliError> {
    let auth = signed_in(paths)?;
    let Located {
        folder: linked,
        root,
        below,
    } = locate(paths, folder).await?;
    let space = linked.space_id.as_str();
    let live = tree(&auth, space).await?;
    let (trashed, deletions_of) = trash_nodes(&auth, space).await?;
    let mut found = deletions(&live, &trashed, &deletions_of, space);
    found.retain(|deleted| starts_with(&deleted.names, &below));
    let names = match found.iter().any(|deleted| deleted.deletion.is_some()) {
        true => device_names(&auth).await?,
        false => HashMap::new(),
    };
    let data = TrashData {
        folder: root.to_string_lossy().into_owned(),
        entries: found.iter().map(|deleted| deleted.entry(&names)).collect(),
    };
    Ok(Done::ok(
        data,
        trash_report(&found, &linked, &names, now_ms()),
    ))
}

/// `koi history versions <file>`: the versions the server keeps of a live file, newest first.
async fn versions(paths: &Paths, file: &Path) -> Result<Done, CliError> {
    let auth = signed_in(paths)?;
    let Located {
        folder: linked,
        root,
        below,
    } = locate(paths, file).await?;
    if below.is_empty() {
        return Err(CliError::usage(format!(
            "{} is the linked folder itself; name a file in it",
            root.display()
        ))
        .because(Reason::LinkedFolderItself));
    }
    let space = linked.space_id.as_str();
    let live = tree(&auth, space).await?;
    let node = find(&live, space, &below).ok_or_else(|| {
        CliError::usage(format!(
            "the server has no file at {} in {}; koi history trash lists deleted ones",
            printable(&below.join("/")),
            root.display()
        ))
        .because(Reason::NotFound)
    })?;
    if node.kind() == NodeKind::Dir {
        return Err(CliError::usage(format!(
            "{} is a folder; name a file in it",
            printable(&below.join("/"))
        ))
        .because(Reason::IsFolder));
    }
    let (kept, current) = versions_and_current(&auth, space, node).await?;
    let current = current.as_deref();
    let names = device_names(&auth).await?;
    let data = VersionsData {
        path: below.join("/"),
        node_id: node.node_id.clone(),
        versions: (kept.iter())
            .map(|version| VersionData {
                version_id: version.version_id.clone(),
                seq: version.seq,
                size: content_size(Some(version)),
                device_id: version.device_id.clone(),
                device_name: names.get(&version.device_id).cloned(),
                created_at_ms: version.created_at_ms,
                current: Some(version.version_id.as_str()) == current,
            })
            .collect(),
    };
    let text = versions_report(&below.join("/"), &kept, current, now_ms());
    Ok(Done::ok(data, text))
}

/// `koi history restore <path>`: asks the folder's engine to bring the deletion at `path` back from the trash, or, with
/// `version`, to make that version of the live file at `path` current again. `node` picks one of several deletions
/// that had the path. The request is checked against the server's listings first, so a typo fails here and not after
/// the next commit. With `request_id`, a restore already asked for under that ID is reported again from its record,
/// without reading the server. The record is read again when a check fails, since another run with the ID may have
/// asked for the restore meanwhile.
async fn restore(
    paths: &Paths,
    path: &Path,
    version: Option<String>,
    node: Option<String>,
    request_id: Option<RequestId>,
) -> Result<Done, CliError> {
    let key = Key::restore(
        &lexical(&crate::absolute(path)?),
        version.as_deref(),
        node.as_deref(),
    );
    if let Some(id) = &request_id
        && let Some(recorded) = request::recorded(paths, id, &key)?
    {
        return replayed(paths, id, recorded).await;
    }
    let (
        Located {
            folder: linked,
            root,
            below,
        },
        restore,
        kind,
        files,
    ) = match checked(paths, path, version, node).await {
        Ok(checked) => checked,
        Err(error) => {
            let Some(id) = &request_id else {
                return Err(error);
            };
            return match request::recorded(paths, id, &key)? {
                None => Err(error),
                Some(recorded) => replayed(paths, id, recorded).await,
            };
        }
    };
    let request = Request::Restore {
        space_id: linked.space_id.clone(),
        restore,
        folder: root.clone(),
        path: below.join("/"),
        kind: Some(kind),
        files,
    };
    match ipc::run(paths, request.once(request_id, key)).await? {
        Response::Restore {
            id, folder, path, ..
        } => {
            let state = (folder == root).then_some(linked.state);
            Ok(asked(&folder, &path, id, state))
        }
        other => Err(unexpected(other)),
    }
}

/// Reports the restore that the run of `id` recorded. Its folder's state comes from the profile, and only while the
/// folder is linked to the restore's space.
async fn replayed(
    paths: &Paths,
    id: &RequestId,
    recorded: serde_json::Value,
) -> Result<Done, CliError> {
    let Ok(Response::Restore {
        id: restore_id,
        space_id,
        folder,
        path,
    }) = serde_json::from_value(recorded)
    else {
        return Err(format!("the record of the request {id} is not a restore").into());
    };
    let state = (linked_folders(paths).await.ok().into_iter().flatten())
        .find(|linked| linked.path.as_ref() == Some(&folder) && linked.space_id == space_id)
        .map(|linked| linked.state);
    Ok(asked(&folder, &path, restore_id, state))
}

/// The folder `path` is in, the names below its root, and the restore to ask for, once the server's listings show that
/// the restore can be made.
async fn checked(
    paths: &Paths,
    path: &Path,
    version: Option<String>,
    node: Option<String>,
) -> Result<(Located, RestoreRequest, ItemKind, Option<u64>), CliError> {
    let auth = signed_in(paths)?;
    let Located {
        folder: linked,
        root,
        below,
    } = locate(paths, path).await?;
    if below.is_empty() {
        return Err(CliError::usage(format!(
            "{} is the linked folder itself; name a file or folder in it",
            root.display()
        ))
        .because(Reason::LinkedFolderItself));
    }
    let space = linked.space_id.as_str();
    let shown = printable(&below.join("/"));
    let live = tree(&auth, space).await?;
    let (request, kind, files) = match version {
        Some(version_id) => {
            let file = find(&live, space, &below).ok_or_else(|| {
                CliError::usage(format!(
                    "the server has no file at {shown} in {}; koi history trash lists deleted ones",
                    root.display()
                ))
                .because(Reason::NotFound)
            })?;
            if file.kind() == NodeKind::Dir {
                return Err(CliError::usage(format!(
                    "{shown} is a folder; only a file has versions"
                ))
                .because(Reason::IsFolder));
            }
            let (kept, current) = versions_and_current(&auth, space, file).await?;
            if current.as_deref() == Some(version_id.as_str()) {
                return Err(CliError::usage(format!(
                    "{version_id} is already the current version of {shown}"
                ))
                .because(Reason::AlreadyCurrent));
            }
            let Some(saved_at_ms) = (kept.iter())
                .find(|kept| kept.version_id == version_id)
                .map(|kept| kept.created_at_ms)
            else {
                return Err(CliError::usage(format!(
                    "{shown} has no version {version_id}; koi history versions lists them"
                ))
                .because(Reason::NoSuchVersion));
            };
            let request = RestoreRequest::Version {
                node_id: file.node_id.clone(),
                version_id,
                saved_at_ms: Some(saved_at_ms),
            };
            (request, ItemKind::File, Some(1))
        }
        None => {
            let (trashed, _) = trash_nodes(&auth, space).await?;
            let deleted = trashed_at(&live, &trashed, space, &below, node.as_deref())
                .map_err(|why| CliError::usage(why.message(&shown, &root)).because(why.reason()))?;
            let request = RestoreRequest::FromTrash {
                node_id: deleted.node_id.clone(),
                parent_id: None,
                name: None,
            };
            let (kind, files) = match deleted.kind() {
                NodeKind::Dir => {
                    let files = (deletions(&live, &trashed, &HashMap::new(), space).into_iter())
                        .find(|top| top.node.node_id == deleted.node_id)
                        .map(|top| top.files);
                    (ItemKind::Folder, files)
                }
                _ => (ItemKind::File, Some(1)),
            };
            (request, kind, files)
        }
    };
    Ok((
        Located {
            folder: linked,
            root,
            below,
        },
        request,
        kind,
        files,
    ))
}

/// What `koi history restore` reports for the restore `id` of `path` in `folder`. `state` is the folder's state while it is
/// linked to the restore's space. Without it, nothing follows: `koi history requests` needs the folder linked.
fn asked(folder: &Path, path: &str, id: String, state: Option<FolderState>) -> Done {
    let shown = printable(path);
    let folder_text = folder.to_string_lossy();
    let mut note = format!("Asked to restore {shown} (restore {}).\n", printable(&id));
    match state {
        Some(FolderState::Stopped) => {
            note.push_str(&format!(
                "The folder sends it once the daemon runs; koi status shows why it doesn't. koi history requests {} shows how it went.\n",
                printable(&folder_text)
            ))
        }
        Some(_) => note.push_str(&format!(
            "The folder sends it with its next commit. koi history requests {} shows how it went.\n",
            printable(&folder_text)
        )),
        None => {}
    }
    let value = printable(&id);
    let mut summary = format!("Restore requested for {shown}.\n");
    let tone = match state {
        None => {
            summary.push_str("Run koi status to check whether this folder is still linked.\n");
            crate::output::Tone::Attention
        }
        Some(FolderState::Stopped | FolderState::Halted | FolderState::Offline) => {
            summary.push_str(
                "The folder is not syncing. Run koi status to see what needs attention.\n",
            );
            crate::output::Tone::Attention
        }
        Some(_) => {
            summary.push_str("It waits for the folder to sync.\n");
            crate::output::Tone::Neutral
        }
    };
    if state.is_some() {
        summary.push_str(&format!(
            "Check restore: koi history requests {}\n",
            crate::output::command_path(folder)
        ));
    }
    let data = RestoreAskedData {
        request_id: id,
        folder: folder_text.to_string(),
        path: path.to_owned(),
    };
    let done = Done {
        outcome: Outcome::Accepted,
        tone,
        ..Done::value(data, &value, note)
    }
    .human(format!("{value}\n"), summary);
    match state {
        Some(_) => done.then(&["koi", "history", "requests", &folder_text]),
        None => done,
    }
}

/// `koi history requests [folder]`: the restores asked for in the folder's space, those that ended first.
async fn requests(paths: &Paths, folder: &Path) -> Result<Done, CliError> {
    let Located {
        folder: linked,
        root,
        ..
    } = locate(paths, folder).await?;
    let request = Request::Restores {
        space_id: linked.space_id.clone(),
    };
    let restores = match ipc::run(paths, request).await? {
        Response::Restores { restores } => restores,
        other => return Err(unexpected(other)),
    };
    let data = RequestsData {
        folder: root.to_string_lossy().into_owned(),
        requests: restores.iter().map(RequestData::of).collect(),
    };
    let pending = restores
        .iter()
        .any(|restore| matches!(restore.state, RestoreState::Pending { .. }));
    let needs_attention = restores.iter().any(|restore| {
        matches!(
            restore.state,
            RestoreState::Rejected { .. }
                | RestoreState::DoesNotFit
                | RestoreState::Pending { waits_for: Some(_) }
        )
    }) || pending
        && (matches!(
            linked.state,
            FolderState::Stopped | FolderState::Halted | FolderState::Offline
        ) || linked.problem.is_some()
            || linked.questions > 0
            || linked.wait.is_some());
    Ok(Done {
        tone: if needs_attention {
            crate::output::Tone::Attention
        } else {
            crate::output::Tone::Neutral
        },
        ..Done::ok(data, restores_report(&restores, &linked))
            .human(restores_summary(&restores, &linked), "")
    })
}

/// Why no single deletion could be picked.
#[derive(Debug, PartialEq)]
enum NotPicked<'a> {
    /// No deletion had the path. `live` says whether a live node has it.
    None { live: bool },
    /// Several did, and none was picked.
    Several(Vec<&'a NodeState>),
    /// None of the deletions at the path is the node asked for.
    NotThere(String),
}

impl NotPicked<'_> {
    fn reason(&self) -> Reason {
        match self {
            Self::None { live: true } => Reason::NotDeleted,
            Self::None { live: false } => Reason::NotFound,
            Self::Several(_) => Reason::SeveralDeletions,
            Self::NotThere(_) => Reason::NotInTrash,
        }
    }

    fn message(&self, shown: &str, root: &Path) -> String {
        match self {
            Self::None { live: true } => {
                format!("{shown} is not deleted; koi history versions lists the versions of a file")
            }
            Self::None { live: false } => format!(
                "nothing deleted at {shown} is in the trash of {}; koi history trash lists what is",
                root.display()
            ),
            Self::Several(found) => {
                let candidates: Vec<String> = (found.iter())
                    .map(|node| {
                        format!(
                            "{} ({}, {})",
                            node.node_id,
                            kind(node),
                            size(content_size(node.current.as_ref()))
                        )
                    })
                    .collect();
                format!(
                    "{} deletions had the path {shown}; pick one with --node: {}",
                    found.len(),
                    candidates.join(", ")
                )
            }
            Self::NotThere(node) => {
                format!(
                    "no deletion at {shown} is node {node}; koi history trash --json lists them"
                )
            }
        }
    }
}

/// The trashed node that had the path `names`, or the one of them that is `node`. A node trashed with its folder
/// counts at its own path, so a file can be restored on its own.
fn trashed_at<'a>(
    live: &'a [NodeState],
    trash: &'a [NodeState],
    root: &str,
    names: &[String],
    node: Option<&str>,
) -> Result<&'a NodeState, NotPicked<'a>> {
    let by_id: HashMap<&str, &NodeState> = live
        .iter()
        .chain(trash)
        .map(|node| (node.node_id.as_str(), node))
        .collect();
    let mut found: Vec<&NodeState> = trash
        .iter()
        .filter(|trashed| {
            let had = names_of(trashed, &by_id, root);
            had.len() == names.len() && starts_with(&had, names)
        })
        .collect();
    if let Some(node) = node {
        found.retain(|trashed| trashed.node_id == node);
        return match found.as_slice() {
            [one] => Ok(one),
            _ => Err(NotPicked::NotThere(node.to_owned())),
        };
    }
    match found.as_slice() {
        [] => Err(NotPicked::None {
            live: find(live, root, names).is_some(),
        }),
        [one] => Ok(one),
        _ => {
            found.sort_by(|a, b| a.node_id.cmp(&b.node_id));
            Err(NotPicked::Several(found))
        }
    }
}

fn trash_report(
    found: &[Deleted],
    linked: &FolderStatus,
    names: &HashMap<String, String>,
    now_ms: i64,
) -> String {
    if found.is_empty() {
        return format!(
            "Nothing from {} is in the trash.\n",
            crate::output::folder_path(linked)
        );
    }
    let mut report = String::new();
    for deleted in found {
        let path = printable(&deleted.names.join("/"));
        let when = deleted.deleted_when(names, now_ms);
        let line = match (deleted.node.kind(), when) {
            (NodeKind::Dir, when) => format!(
                "{:>9}  {path}/  (folder, {}{})",
                size(deleted.bytes),
                crate::count(deleted.files, "file"),
                when.map(|when| format!(", {when}")).unwrap_or_default()
            ),
            (_, Some(when)) => format!("{:>9}  {path}  ({when})", size(deleted.bytes)),
            (_, None) => format!("{:>9}  {path}", size(deleted.bytes)),
        };
        report.push_str(&line);
        report.push('\n');
    }
    report
}

fn restores_summary(restores: &[RestoreStatus], linked: &FolderStatus) -> String {
    if restores.is_empty() {
        return format!(
            "No restore requests for {}.\n",
            crate::output::folder(linked)
        );
    }
    let mut report = String::new();
    for restore in restores {
        let path = restore
            .path
            .as_ref()
            .map(|path| printable(path.unquoted()))
            .unwrap_or_else(|| "Item with an unknown path".to_owned());
        let state = match &restore.state {
            RestoreState::Pending {
                waits_for: Some(RestoreWait::NodeProblem),
            } => "waiting for a sync problem to be fixed; run koi status".to_owned(),
            RestoreState::DoesNotFit => {
                "not sent: this restore exceeds the server's limit".to_owned()
            }
            state => restore_state(state),
        };
        report.push_str(&format!("{path}: {state}\n"));
    }
    if restores
        .iter()
        .any(|restore| matches!(restore.state, RestoreState::Pending { .. }))
    {
        report.push_str(
            "Pending restores wait for the folder to sync. Check progress with koi status.\n",
        );
        if matches!(
            linked.state,
            FolderState::Stopped | FolderState::Halted | FolderState::Offline
        ) {
            report.push_str(
                "The folder is not syncing. Run koi status to see what needs attention.\n",
            );
        }
        if let Some(problem) = &linked.problem {
            report.push_str(&format!(
                "The folder has a problem: {}\n",
                printable(problem)
            ));
        }
        if let Some(wait) = &linked.wait {
            report.push_str(&format!("{}\n", crate::output::wait_cause(wait, now_ms())));
        }
        if linked.questions > 0 {
            report.push_str(&format!(
                "{}: koi resolve lists them.\n",
                crate::need_you(linked.questions)
            ));
        }
    }
    report
}

fn restores_report(restores: &[RestoreStatus], linked: &FolderStatus) -> String {
    if restores.is_empty() {
        return format!(
            "No restore was asked for in {}.\n",
            crate::output::folder_path(linked)
        );
    }
    let mut report = String::new();
    for restore in restores {
        let what = match &restore.request {
            RestoreRequest::FromTrash { node_id, .. } => format!("deleted node {node_id}"),
            RestoreRequest::Version {
                node_id,
                version_id,
                ..
            } => format!("version {version_id} of node {node_id}"),
        };
        report.push_str(&format!(
            "{}  {what}\n  {}\n",
            restore.id,
            restore_state(&restore.state)
        ));
    }
    let pending = restores
        .iter()
        .any(|restore| matches!(restore.state, RestoreState::Pending { .. }));
    if pending {
        report.push_str(
            "A pending restore goes out with a commit of the folder once the earlier changes of its node have synced.\n",
        );
        if linked.state == FolderState::Stopped {
            report.push_str("The folder is not syncing: run koi _daemon.\n");
        }
        if let Some(problem) = &linked.problem {
            report.push_str(&format!(
                "The folder has a problem: {}\n",
                printable(problem)
            ));
        }
        if linked.questions > 0 {
            report.push_str(&format!(
                "{}: {} lists them.\n",
                crate::need_you(linked.questions),
                crate::RESOLVE
            ));
        }
    }
    report
}

fn restore_state(state: &RestoreState) -> String {
    match state {
        RestoreState::Pending { waits_for: None } => "pending: waits for the folder to sync".into(),
        RestoreState::Pending {
            waits_for: Some(RestoreWait::NodeProblem),
        } => "pending: the server refused a change this device made to the node, and nothing is sent for it until that problem is cleared"
            .into(),
        RestoreState::Restored => "restored".into(),
        RestoreState::Renamed { name } => {
            format!("restored as {}, since its name was taken", printable(name))
        }
        RestoreState::Copied { name, .. } => format!(
            "restored as a new file {}, since another device changed the file meanwhile",
            printable(name)
        ),
        RestoreState::Rejected { reason } => format!("refused: {}", rejection(reason)),
        RestoreState::DoesNotFit => {
            "not sent: it brings back more nodes than one commit can hold".into()
        }
    }
}

fn rejection(reason: &RestoreRejection) -> String {
    match reason {
        RestoreRejection::NotFound => {
            "the server no longer has it, or the folder to restore into; it was purged or expired"
                .into()
        }
        RestoreRejection::NotFile => "only a file has versions".into(),
        RestoreRejection::InvalidName => "its name is not valid".into(),
        RestoreRejection::ParentNotDir => "the place to restore into is a file".into(),
        RestoreRejection::Cycle => "the folder to restore into is inside it".into(),
        RestoreRejection::Root => "it is the root of the space".into(),
        RestoreRejection::ReadOnly => "the server takes no writes from this device".into(),
        RestoreRejection::Cancelled => "it was cancelled during recovery".into(),
        RestoreRejection::Other(reason) => reason.clone(),
    }
}

fn versions_report(path: &str, kept: &[VersionInfo], current: Option<&str>, now_ms: i64) -> String {
    let mut report = format!(
        "{}: {}, newest first\n",
        printable(path),
        crate::count(kept.len() as u64, "version")
    );
    for version in kept {
        let marker = if Some(version.version_id.as_str()) == current {
            "current"
        } else {
            ""
        };
        report.push_str(&format!(
            "{marker:>7}  {:>9}  {:<14}  {}\n",
            size(content_size(Some(version))),
            ago(version.created_at_ms, now_ms),
            version.version_id
        ));
    }
    report
}

#[cfg(test)]
mod tests;
