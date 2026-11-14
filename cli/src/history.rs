// SPDX-License-Identifier: Apache-2.0

//! `koi history trash` and `koi history versions`: what the space of a linked folder keeps after a delete or
//! an edit. Both read the server through its HTTP RPC route with this profile's credential. They change no file here or
//! on the server, and without a daemon they read the profile without writing to it. `koi history restore` reads
//! the same listings to find what to restore and records the request in the folder's store, for the folder's engine to
//! send (design §6.3). `koi history requests` lists those requests and how each went.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use koidrive_driver::{
    Driver, FolderState, FolderStatus, Paths, RestoreRejection, RestoreRequest, RestoreState,
    RestoreStatus, RestoreWait,
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

/// The most entries asked for in one page; the server sends at most this many.
const PAGE: u32 = 1000;
const UNEXPECTED: &str = "the server answered with another kind of result";

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
    let (linked, below) = locate(paths, folder).await?;
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
        folder: linked.path.clone(),
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
    let (linked, below) = locate(paths, file).await?;
    if below.is_empty() {
        return Err(CliError::usage(format!(
            "{} is the linked folder itself; name a file in it",
            linked.path
        ))
        .because(Reason::LinkedFolderItself));
    }
    let space = linked.space_id.as_str();
    let live = tree(&auth, space).await?;
    let node = find(&live, space, &below).ok_or_else(|| {
        CliError::usage(format!(
            "the server has no file at {} in {}; koi history trash lists deleted ones",
            printable(&below.join("/")),
            linked.path
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
    let (linked, below, restore) = match checked(paths, path, version, node).await {
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
        folder: linked.path.clone(),
        path: below.join("/"),
    };
    match ipc::run(paths, request.once(request_id, key)).await? {
        Response::Restore {
            id, folder, path, ..
        } => {
            let state = (folder == linked.path).then_some(linked.state);
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
        .find(|linked| linked.path == folder && linked.space_id == space_id)
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
) -> Result<(FolderStatus, Vec<String>, RestoreRequest), CliError> {
    let auth = signed_in(paths)?;
    let (linked, below) = locate(paths, path).await?;
    if below.is_empty() {
        return Err(CliError::usage(format!(
            "{} is the linked folder itself; name a file or folder in it",
            linked.path
        ))
        .because(Reason::LinkedFolderItself));
    }
    let space = linked.space_id.as_str();
    let shown = printable(&below.join("/"));
    let live = tree(&auth, space).await?;
    let request = match version {
        Some(version_id) => {
            let file = find(&live, space, &below).ok_or_else(|| {
                CliError::usage(format!(
                    "the server has no file at {shown} in {}; koi history trash lists deleted ones",
                    linked.path
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
            RestoreRequest::Version {
                node_id: file.node_id.clone(),
                version_id,
                saved_at_ms: Some(saved_at_ms),
            }
        }
        None => {
            let (trashed, _) = trash_nodes(&auth, space).await?;
            let deleted =
                trashed_at(&live, &trashed, space, &below, node.as_deref()).map_err(|why| {
                    CliError::usage(why.message(&shown, &linked)).because(why.reason())
                })?;
            RestoreRequest::FromTrash {
                node_id: deleted.node_id.clone(),
                parent_id: None,
                name: None,
            }
        }
    };
    Ok((linked, below, request))
}

/// What `koi history restore` reports for the restore `id` of `path` in `folder`. `state` is the folder's state while it is
/// linked to the restore's space. Without it, nothing follows: `koi history requests` needs the folder linked.
fn asked(folder: &str, path: &str, id: String, state: Option<FolderState>) -> Done {
    let shown = printable(path);
    let mut note = format!("Asked to restore {shown} (restore {}).\n", printable(&id));
    match state {
        Some(FolderState::Stopped) => {
            note.push_str(&format!(
                "The folder sends it once the daemon runs; koi status shows why it doesn't. koi history requests {} shows how it went.\n",
                printable(folder)
            ))
        }
        Some(_) => note.push_str(&format!(
            "The folder sends it with its next commit. koi history requests {} shows how it went.\n",
            printable(folder)
        )),
        None => {}
    }
    let value = printable(&id);
    let data = RestoreAskedData {
        request_id: id,
        folder: folder.to_owned(),
        path: path.to_owned(),
    };
    let done = Done {
        outcome: Outcome::Accepted,
        ..Done::value(data, &value, note)
    };
    match state {
        Some(_) => done.then(&["koi", "history", "requests", folder]),
        None => done,
    }
}

/// `koi history requests [folder]`: the restores asked for in the folder's space, those that ended first.
async fn requests(paths: &Paths, folder: &Path) -> Result<Done, CliError> {
    let (linked, _) = locate(paths, folder).await?;
    let request = Request::Restores {
        space_id: linked.space_id.clone(),
    };
    let restores = match ipc::run(paths, request).await? {
        Response::Restores { restores } => restores,
        other => return Err(unexpected(other)),
    };
    let data = RequestsData {
        folder: linked.path.clone(),
        requests: restores.iter().map(RequestData::of).collect(),
    };
    Ok(Done::ok(data, restores_report(&restores, &linked)))
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

    fn message(&self, shown: &str, linked: &FolderStatus) -> String {
        match self {
            Self::None { live: true } => {
                format!("{shown} is not deleted; koi history versions lists the versions of a file")
            }
            Self::None { live: false } => format!(
                "nothing deleted at {shown} is in the trash of {}; koi history trash lists what is",
                linked.path
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

/// The folders the profile links, from the daemon, or from the profile's stores read without writing anything when no
/// daemon runs.
async fn linked_folders(paths: &Paths) -> Result<Vec<FolderStatus>, CliError> {
    let reached = match koidrive_ipc::ask(paths, &Request::Status).await? {
        Some(response) => ipc::Reached::Daemon(Box::new(response)),
        None => {
            ipc::reach(paths, &Request::Status, ipc::STARTING, None, || {
                Driver::linked_folders(paths)
            })
            .await?
        }
    };
    match reached {
        ipc::Reached::Daemon(response) => match *response {
            Response::Status { status } => Ok(status.folders),
            other => Err(unexpected(other)),
        },
        ipc::Reached::Profile(folders) => Ok(folders),
    }
}

/// The linked folder that holds `path`, and the names below it as typed. `path` need not exist here, since a file
/// deleted on this device may still be on the server. Symbolic links are followed only up to the linked folder: below
/// it a link is a node of the space like any other, so the names stay as typed and never select the node a local link
/// points to.
async fn locate(paths: &Paths, path: &Path) -> Result<(FolderStatus, Vec<String>), CliError> {
    let folders = linked_folders(paths).await?;
    let typed = lexical(&crate::absolute(path)?);
    let components: Vec<Component> = typed.components().collect();
    let mut prefix = PathBuf::new();
    for (index, component) in components.iter().enumerate() {
        prefix.push(component);
        let Ok(real) = std::fs::canonicalize(&prefix) else {
            break;
        };
        let inside = folders
            .iter()
            .filter_map(|folder| Some((folder, below(Path::new(&folder.path), &real)?)))
            .max_by_key(|(folder, _)| folder.path.len());
        if let Some((folder, mut names)) = inside {
            names.extend(
                components[index + 1..]
                    .iter()
                    .map(|name| name.as_os_str().to_string_lossy().into_owned()),
            );
            return Ok((folder.clone(), names));
        }
    }
    Err(CliError::usage(format!(
        "{} is not in a linked folder; koi status lists them",
        printable(&typed.display().to_string())
    ))
    .because(Reason::NotInLinkedFolder))
}

/// `path` without `.` names, and with each `..` removing the name before it, worked out without reading the disk.
fn lexical(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normal.pop();
            }
            other => normal.push(other),
        }
    }
    normal
}

/// The names of `path` below `root`, or `None` when it is not inside it.
fn below(root: &Path, path: &Path) -> Option<Vec<String>> {
    path.strip_prefix(root)
        .ok()?
        .components()
        .map(|component| match component {
            Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect()
}

/// Whether `names` begins with `prefix`, comparing names as the server does.
fn starts_with(names: &[String], prefix: &[String]) -> bool {
    names.len() >= prefix.len()
        && names
            .iter()
            .zip(prefix)
            .all(|(name, wanted)| name_key(name) == name_key(wanted))
}

async fn call(auth: &Auth, space: &str, call: Call) -> Result<rpc_response::Result, CliError> {
    match auth.api().rpc(auth, space, call).await?.result {
        Some(rpc_response::Result::Error(error)) => Err(CliError::rpc_refusal(error)),
        Some(result) => Ok(result),
        None => Err("the server sent an empty answer".into()),
    }
}

/// Every page of a listing, from the empty cursor to the page whose `next_cursor` is empty. A cursor the server sends
/// twice ends the listing with an error instead of a loop.
async fn every_page<T>(
    mut page: impl AsyncFnMut(String) -> Result<(Vec<T>, String), CliError>,
) -> Result<Vec<T>, CliError> {
    let mut all = Vec::new();
    let mut cursor = String::new();
    let mut seen = HashSet::new();
    loop {
        let (items, next) = page(cursor).await?;
        all.extend(items);
        if next.is_empty() {
            return Ok(all);
        }
        if !seen.insert(next.clone()) {
            return Err("the server sent the same page cursor twice".into());
        }
        cursor = next;
    }
}

async fn tree(auth: &Auth, space: &str) -> Result<Vec<NodeState>, CliError> {
    every_page(async |cursor| {
        match call(
            auth,
            space,
            Call::ListTree(ListTree {
                cursor,
                limit: PAGE,
            }),
        )
        .await?
        {
            rpc_response::Result::Tree(page) => Ok((page.nodes, page.next_cursor)),
            _ => Err(UNEXPECTED.into()),
        }
    })
    .await
}

async fn device_names(auth: &Auth) -> Result<HashMap<String, String>, CliError> {
    match crate::devices(auth).await {
        Ok(devices) => Ok((devices.into_iter())
            .map(|device| (device.device_id, device.name))
            .collect()),
        Err(error) if error.code == koidrive_ipc::Code::AuthRequired => Err(error),
        Err(error) => {
            tracing::warn!(
                error = error.message,
                "listing the account's devices failed; history names none"
            );
            Ok(HashMap::new())
        }
    }
}

async fn trash_nodes(
    auth: &Auth,
    space: &str,
) -> Result<(Vec<NodeState>, HashMap<String, Trashed>), CliError> {
    let pages = every_page(async |cursor| {
        match call(
            auth,
            space,
            Call::ListTrash(ListTrash {
                cursor,
                limit: PAGE,
            }),
        )
        .await?
        {
            rpc_response::Result::Trash(page) => {
                Ok((vec![(page.nodes, page.trashed)], page.next_cursor))
            }
            _ => Err(UNEXPECTED.into()),
        }
    })
    .await?;
    let (mut nodes, mut deleted) = (Vec::new(), HashMap::new());
    for (page, trashed) in pages {
        nodes.extend(page);
        deleted.extend(
            (trashed.into_iter())
                .filter(|deletion| deletion.trashed_at_ms > 0)
                .map(|deletion| (deletion.node_id.clone(), deletion)),
        );
    }
    Ok((nodes, deleted))
}

async fn versions_and_current(
    auth: &Auth,
    space: &str,
    node: &NodeState,
) -> Result<(Vec<VersionInfo>, Option<String>), CliError> {
    let mut current = (node.current.as_ref()).map(|version| version.version_id.clone());
    for read in 0..4 {
        if read > 0 {
            current = (tree(auth, space).await?.into_iter())
                .find(|again| again.node_id == node.node_id)
                .and_then(|again| again.current)
                .map(|version| version.version_id);
        }
        let kept = versions_of(auth, space, &node.node_id).await?;
        let listed = (current.as_ref())
            .and_then(|current| kept.iter().find(|version| &version.version_id == current));
        if listed.is_some_and(|listed| kept.iter().all(|version| version.seq <= listed.seq)) {
            return Ok((kept, current));
        }
    }
    Err(CliError::new(
        koidrive_ipc::Code::Unavailable,
        "the file kept changing while its versions were read; try again",
    ))
}

async fn versions_of(
    auth: &Auth,
    space: &str,
    node_id: &str,
) -> Result<Vec<VersionInfo>, CliError> {
    every_page(async |cursor| {
        let request = ListVersions {
            node_id: node_id.to_owned(),
            cursor,
            limit: PAGE,
        };
        match call(auth, space, Call::ListVersions(request)).await? {
            rpc_response::Result::Versions(list) => Ok((list.versions, list.next_cursor)),
            _ => Err(UNEXPECTED.into()),
        }
    })
    .await
}

/// The live node at `names` below the space's root, whose node id is the space id. Names compare by `name_key`, under which
/// no two live children of a folder collide, so a name differing only in case or Unicode form still finds its node.
fn find<'a>(live: &'a [NodeState], root: &str, names: &[String]) -> Option<&'a NodeState> {
    let children: HashMap<(&str, String), &NodeState> = live
        .iter()
        .map(|node| ((node.parent_id.as_str(), name_key(&node.name)), node))
        .collect();
    let mut parent = root;
    let mut found = None;
    for name in names {
        let node = *children.get(&(parent, name_key(name)))?;
        parent = &node.node_id;
        found = Some(node);
    }
    found
}

/// The names from the root down to `node`, through the nodes in `by_id`. A parent the listings do not hold, such as
/// one purged since, shows as "…".
fn names_of(node: &NodeState, by_id: &HashMap<&str, &NodeState>, root: &str) -> Vec<String> {
    let mut names = vec![node.name.clone()];
    let mut parent = node.parent_id.as_str();
    while parent != root {
        match by_id.get(parent) {
            Some(next) if names.len() <= by_id.len() => {
                names.push(next.name.clone());
                parent = &next.parent_id;
            }
            _ => {
                names.push("…".into());
                break;
            }
        }
    }
    names.reverse();
    names
}

/// One deletion in the trash: a trashed node whose parent was not trashed with it, and what was trashed beneath it.
#[derive(Debug)]
struct Deleted<'a> {
    node: &'a NodeState,
    deletion: Option<&'a Trashed>,
    names: Vec<String>,
    files: u64,
    bytes: u64,
}

impl Deleted<'_> {
    fn entry(&self, names: &HashMap<String, String>) -> TrashEntry {
        TrashEntry {
            node_id: self.node.node_id.clone(),
            path: self.names.join("/"),
            kind: kind(self.node),
            trash_group: self.node.trash_group.clone(),
            files: self.files,
            bytes: self.bytes,
            modified_at_ms: (self.node.current.as_ref())
                .filter(|_| self.node.kind() != NodeKind::Dir)
                .map(|version| version.created_at_ms),
            deleted_at_ms: self.deletion.map(|deletion| deletion.trashed_at_ms),
            deleted_by_device_id: (self.deletion)
                .map(|deletion| deletion.device_id.clone())
                .filter(|device| !device.is_empty()),
            deleted_by_device_name: (self.deletion)
                .and_then(|deletion| names.get(&deletion.device_id).cloned()),
        }
    }

    fn deleted_when(&self, names: &HashMap<String, String>, now_ms: i64) -> Option<String> {
        let deletion = self.deletion?;
        let when = ago(deletion.trashed_at_ms, now_ms);
        Some(match names.get(&deletion.device_id) {
            Some(name) => format!("deleted {when} on {}", printable(name)),
            None => format!("deleted {when}"),
        })
    }
}

/// The trash as deletions, sorted by path. A file counts with its own size; a folder counts the files trashed with it.
fn deletions<'a>(
    live: &'a [NodeState],
    trash: &'a [NodeState],
    deleted: &'a HashMap<String, Trashed>,
    root: &str,
) -> Vec<Deleted<'a>> {
    let by_id: HashMap<&str, &NodeState> = live
        .iter()
        .chain(trash)
        .map(|node| (node.node_id.as_str(), node))
        .collect();
    let trashed: HashMap<&str, &NodeState> = trash
        .iter()
        .map(|node| (node.node_id.as_str(), node))
        .collect();
    let top = |node: &'a NodeState| -> &'a NodeState {
        let mut top = node;
        for _ in 0..trash.len() {
            match trashed.get(top.parent_id.as_str()) {
                Some(parent) if parent.trash_group == top.trash_group => top = parent,
                _ => break,
            }
        }
        top
    };
    let mut found: HashMap<&str, Deleted<'a>> = HashMap::new();
    for node in trash {
        let head = top(node);
        let deleted = found
            .entry(head.node_id.as_str())
            .or_insert_with(|| Deleted {
                node: head,
                deletion: deleted.get(&head.node_id),
                names: names_of(head, &by_id, root),
                files: 0,
                bytes: 0,
            });
        if node.kind() != NodeKind::Dir {
            deleted.files += 1;
            deleted.bytes += content_size(node.current.as_ref());
        }
    }
    let mut found: Vec<_> = found.into_values().collect();
    found.sort_by(|a, b| {
        a.names
            .cmp(&b.names)
            .then(a.node.node_id.cmp(&b.node.node_id))
    });
    found
}

fn content_size(version: Option<&VersionInfo>) -> u64 {
    version
        .and_then(|version| version.content.as_ref())
        .map_or(0, |content| content.size)
}

fn kind(node: &NodeState) -> NodeType {
    match node.kind() {
        NodeKind::Dir => NodeType::Folder,
        NodeKind::Symlink => NodeType::Symlink,
        _ => NodeType::File,
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
            printable(&linked.path)
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

fn restores_report(restores: &[RestoreStatus], linked: &FolderStatus) -> String {
    if restores.is_empty() {
        return format!("No restore was asked for in {}.\n", printable(&linked.path));
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
mod tests {
    use koidrive_driver::Credential;
    use koidrive_proto::v1::{
        AccessToken, Content, Error, NodeStatus, RpcRequest, RpcResponse, TrashPage, TreePage,
        VersionList,
    };
    use prost::Message;

    use super::*;
    use crate::report::RequestedData;
    use crate::setup::fake::{Fake, proto};

    #[tokio::test]
    async fn history_waits_for_a_daemon_that_holds_the_profile_before_its_socket_answers() {
        let (_dir, paths, _root) = crate::ipc::tests::linked_profile();
        let driver = Driver::open(paths.clone()).unwrap();
        let socket = paths.socket();
        let daemon = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            ipc::serve(koidrive_ipc::bind(&socket).unwrap(), driver).await;
        });
        let folders = linked_folders(&paths).await.unwrap();
        daemon.abort();
        assert_eq!(folders.len(), 1);
    }

    const SPACE: &str = "0194f8a2-0000-7000-8000-00000000000a";

    fn node(
        id: &str,
        parent: &str,
        name: &str,
        kind: NodeKind,
        size: u64,
        group: &str,
    ) -> NodeState {
        let status = if group.is_empty() {
            NodeStatus::Live
        } else {
            NodeStatus::Trashed
        };
        NodeState {
            node_id: id.into(),
            parent_id: parent.into(),
            name: name.into(),
            kind: kind as i32,
            status: status as i32,
            trash_group: group.into(),
            current: Some(version(&format!("{id}-1"), size, 0)),
            ..NodeState::default()
        }
    }

    fn version(id: &str, size: u64, created_at_ms: i64) -> VersionInfo {
        VersionInfo {
            version_id: id.into(),
            content: Some(Content {
                size,
                ..Content::default()
            }),
            created_at_ms,
            ..VersionInfo::default()
        }
    }

    fn names(path: &str) -> Vec<String> {
        path.split('/').map(String::from).collect()
    }

    fn answer(result: rpc_response::Result) -> RpcResponse {
        RpcResponse {
            req_id: 1,
            result: Some(result),
        }
    }

    /// A signed-in device of the server `fake`, which answers its token request first.
    fn signed_in_to(fake: &Fake) -> Auth {
        let credential = Credential {
            server: fake.base.to_string(),
            user_id: "user".into(),
            user_name: None,
            device_id: "0194f8a2-0000-7000-8000-000000000001".into(),
            device_name: "laptop".into(),
            refresh_credential: "refresh".into(),
        };
        Auth::new(credential.api().unwrap(), credential.net())
    }

    fn token() -> crate::setup::fake::Reply {
        let token = AccessToken {
            token: "access".into(),
            expires_at_ms: i64::MAX,
            ..Default::default()
        };
        proto("POST", "/v1/token", token)
    }

    #[tokio::test]
    async fn every_page_follows_the_cursor_to_the_last_page() {
        let mut asked = Vec::new();
        let all = every_page(async |cursor: String| {
            asked.push(cursor.clone());
            Ok(match cursor.as_str() {
                "" => (vec![1, 2], "b".to_owned()),
                "b" => (vec![3], "c".to_owned()),
                _ => (vec![4], String::new()),
            })
        })
        .await
        .unwrap();
        assert_eq!(all, [1, 2, 3, 4]);
        assert_eq!(asked, ["", "b", "c"]);

        let error = every_page(async |_cursor: String| Ok((vec![0], "same".to_owned())))
            .await
            .unwrap_err();
        assert!(
            error.message.contains("same page cursor twice"),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn versions_are_read_page_by_page_over_http() {
        let rpc = format!("/v1/spaces/{SPACE}/rpc");
        let page = |ids: &[&str], next: &str| {
            let list = VersionList {
                versions: ids.iter().map(|id| version(id, 1, 0)).collect(),
                next_cursor: next.into(),
            };
            proto("POST", &rpc, answer(rpc_response::Result::Versions(list)))
        };
        let fake = Fake::start(vec![
            token(),
            page(&["v3", "v2"], "after-v2"),
            page(&["v1"], ""),
        ])
        .await;

        let kept = versions_of(&signed_in_to(&fake), SPACE, "node")
            .await
            .unwrap();

        let ids: Vec<_> = kept
            .iter()
            .map(|version| version.version_id.as_str())
            .collect();
        assert_eq!(ids, ["v3", "v2", "v1"]);
        let asked: Vec<_> = fake
            .seen()
            .iter()
            .filter(|seen| seen.path == rpc)
            .map(
                |seen| match RpcRequest::decode(seen.body.as_slice()).unwrap().call {
                    Some(Call::ListVersions(list)) => (list.node_id, list.cursor),
                    other => panic!("expected ListVersions, got {other:?}"),
                },
            )
            .collect();
        assert_eq!(
            asked,
            [
                ("node".to_owned(), String::new()),
                ("node".to_owned(), "after-v2".to_owned())
            ]
        );
    }

    #[tokio::test]
    async fn the_tree_and_the_trash_are_read_over_http_and_a_refusal_stops_them() {
        let rpc = format!("/v1/spaces/{SPACE}/rpc");
        let tree_page = TreePage {
            nodes: vec![node("docs", SPACE, "docs", NodeKind::Dir, 0, "")],
            ..TreePage::default()
        };
        let trash_page = TrashPage {
            nodes: vec![
                node("old", "docs", "old.txt", NodeKind::File, 12, "g"),
                node("older", "docs", "older.txt", NodeKind::File, 1, "h"),
            ],
            trashed: vec![
                Trashed {
                    node_id: "old".into(),
                    trashed_at_ms: 1_700,
                    device_id: "desk".into(),
                },
                Trashed {
                    node_id: "older".into(),
                    trashed_at_ms: 0,
                    device_id: String::new(),
                },
            ],
            ..TrashPage::default()
        };
        let refusal = Error {
            message: "not a member of this space".into(),
            ..Error::default()
        };
        let fake = Fake::start(vec![
            token(),
            proto("POST", &rpc, answer(rpc_response::Result::Tree(tree_page))),
            proto(
                "POST",
                &rpc,
                answer(rpc_response::Result::Trash(trash_page)),
            ),
            proto("POST", &rpc, answer(rpc_response::Result::Error(refusal))),
        ])
        .await;
        let auth = signed_in_to(&fake);

        let live = tree(&auth, SPACE).await.unwrap();
        let (trashed, deleted) = trash_nodes(&auth, SPACE).await.unwrap();
        let error = tree(&auth, SPACE).await.unwrap_err();

        assert_eq!(live[0].node_id, "docs");
        assert_eq!(trashed[0].node_id, "old");
        assert_eq!(
            (
                deleted.len(),
                deleted["old"].trashed_at_ms,
                deleted["old"].device_id.as_str()
            ),
            (1, 1_700, "desk")
        );
        assert_eq!(
            error.message,
            "the server refused: not a member of this space"
        );
    }

    #[test]
    fn a_path_finds_its_live_node_by_name_key() {
        let live = vec![
            node("docs", SPACE, "Docs", NodeKind::Dir, 0, ""),
            node("cafe", "docs", "Café.txt", NodeKind::File, 5, ""),
            node("other", SPACE, "café.txt", NodeKind::File, 1, ""),
        ];
        let found = |path: &str| find(&live, SPACE, &names(path)).map(|node| node.node_id.as_str());

        assert_eq!(found("docs/CAFE\u{301}.TXT"), Some("cafe"));
        assert_eq!(found("Docs/Café.txt"), Some("cafe"));
        assert_eq!(found("café.txt"), Some("other"));
        assert_eq!(found("docs/missing.txt"), None);
        assert_eq!(found("café.txt/below"), None);
    }

    fn linked_on(server: &str) -> (tempfile::TempDir, Paths, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let paths = Paths::under(&base.join("profile"));
        let root = base.join("Koidrive");
        std::fs::create_dir_all(&root).unwrap();
        Credential {
            server: server.into(),
            user_id: "user".into(),
            user_name: None,
            device_id: "d1".into(),
            device_name: "laptop".into(),
            refresh_credential: "refresh".into(),
        }
        .save_new(&paths.credential())
        .unwrap();
        let store = paths.store(SPACE).unwrap();
        std::fs::create_dir_all(store.parent().unwrap()).unwrap();
        koidrive_store::Store::open(&store)
            .unwrap()
            .transaction(|tx| {
                tx.set_link(&koidrive_store::Link {
                    space_id: SPACE.into(),
                    space_name: "Projects".into(),
                    root: root.clone(),
                    root_dev: 1,
                    server_url: server.into(),
                    device_id: "d1".into(),
                    linked_at_ms: 1,
                })
            })
            .unwrap();
        (dir, paths, root)
    }

    /// A profile signed in to a server that never answers, whose folder `Koidrive` syncs `SPACE`, and the record of the
    /// finished request `done` that restored `a.txt` in `space_id`.
    async fn silent_profile(
        space_id: &str,
    ) -> (tempfile::TempDir, Paths, PathBuf, tokio::net::TcpListener) {
        let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (dir, paths, root) = linked_on(&format!("http://{}/", silent.local_addr().unwrap()));
        let request::Begun::Claimed(claim) = request::begin(
            &paths,
            &RequestId::parse("done").unwrap(),
            &restore_key(&root),
        )
        .unwrap() else {
            panic!("the request was claimed before");
        };
        claim
            .finish(&Response::Restore {
                id: "restore-1".into(),
                space_id: space_id.into(),
                folder: root.display().to_string(),
                path: "a.txt".into(),
            })
            .unwrap();
        (dir, paths, root, silent)
    }

    fn restore_key(root: &Path) -> Key {
        Key::restore(&root.join("a.txt"), None, None)
    }

    async fn restore_again(paths: &Paths, root: &Path, id: &str) -> Result<Done, CliError> {
        let file = root.join("a.txt");
        let run = restore(
            paths,
            &file,
            None,
            None,
            Some(RequestId::parse(id).unwrap()),
        );
        tokio::time::timeout(std::time::Duration::from_secs(10), run)
            .await
            .expect("the run waited for the server")
    }

    #[tokio::test]
    async fn each_refusal_of_history_names_its_reason() {
        let rpc = format!("/v1/spaces/{SPACE}/rpc");
        let tree = || {
            let nodes = vec![
                node("docs", SPACE, "docs", NodeKind::Dir, 0, ""),
                node("a", "docs", "a.txt", NodeKind::File, 3, ""),
            ];
            let page = TreePage {
                nodes,
                ..TreePage::default()
            };
            proto("POST", &rpc, answer(rpc_response::Result::Tree(page)))
        };
        let trash = || {
            let nodes = vec![
                node("b1", "docs", "b.txt", NodeKind::File, 1, "g1"),
                node("b2", "docs", "b.txt", NodeKind::File, 2, "g2"),
                node("c", "docs", "c.txt", NodeKind::File, 2, "g3"),
            ];
            let page = TrashPage {
                nodes,
                ..TrashPage::default()
            };
            proto("POST", &rpc, answer(rpc_response::Result::Trash(page)))
        };
        let kept = || {
            let list = VersionList {
                versions: vec![version("a-1", 3, 2), version("a-0", 1, 1)],
                next_cursor: String::new(),
            };
            proto("POST", &rpc, answer(rpc_response::Result::Versions(list)))
        };
        let restore = |path: &'static str, version: Option<&str>, node: Option<&str>| {
            (
                path,
                Some((version.map(String::from), node.map(String::from))),
            )
        };
        let listing = |path: &'static str| (path, None);
        let cases = [
            (
                listing("../elsewhere.txt"),
                vec![],
                Reason::NotInLinkedFolder,
            ),
            (listing(""), vec![], Reason::LinkedFolderItself),
            (
                listing("docs/gone.txt"),
                vec![token(), tree()],
                Reason::NotFound,
            ),
            (listing("docs"), vec![token(), tree()], Reason::IsFolder),
            (restore("", None, None), vec![], Reason::LinkedFolderItself),
            (
                restore("docs/gone.txt", Some("v"), None),
                vec![token(), tree()],
                Reason::NotFound,
            ),
            (
                restore("docs", Some("v"), None),
                vec![token(), tree()],
                Reason::IsFolder,
            ),
            (
                restore("docs/a.txt", Some("a-1"), None),
                vec![token(), tree(), kept()],
                Reason::AlreadyCurrent,
            ),
            (
                restore("docs/a.txt", Some("a-9"), None),
                vec![token(), tree(), kept()],
                Reason::NoSuchVersion,
            ),
            (
                restore("docs/a.txt", None, None),
                vec![token(), tree(), trash()],
                Reason::NotDeleted,
            ),
            (
                restore("docs/gone.txt", None, None),
                vec![token(), tree(), trash()],
                Reason::NotFound,
            ),
            (
                restore("docs/b.txt", None, None),
                vec![token(), tree(), trash()],
                Reason::SeveralDeletions,
            ),
            (
                restore("docs/b.txt", None, Some("c")),
                vec![token(), tree(), trash()],
                Reason::NotInTrash,
            ),
        ];
        for ((path, restoring), replies, reason) in cases {
            let fake = Fake::start(replies).await;
            let (_dir, paths, root) = linked_on(fake.base.as_str());
            let path = root.join(path);
            let error = match restoring {
                None => versions(&paths, &path).await.map(drop).unwrap_err(),
                Some((version, node)) => checked(&paths, &path, version, node)
                    .await
                    .map(drop)
                    .unwrap_err(),
            };
            assert_eq!(
                (error.code, error.reason),
                (koidrive_ipc::Code::Usage, Some(reason)),
                "{path:?}: {error:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_save_between_reading_the_tree_and_the_versions_reads_both_again() {
        let rpc = format!("/v1/spaces/{SPACE}/rpc");
        let saved = |id: &str, seq: u64| VersionInfo {
            seq,
            ..version(id, seq, seq as i64)
        };
        let tree = |current: VersionInfo| {
            let page = TreePage {
                nodes: vec![NodeState {
                    current: Some(current),
                    ..node("a", SPACE, "a.txt", NodeKind::File, 3, "")
                }],
                ..TreePage::default()
            };
            proto("POST", &rpc, answer(rpc_response::Result::Tree(page)))
        };
        let kept = || {
            let list = VersionList {
                versions: vec![saved("a-2", 3), saved("a-1", 2)],
                next_cursor: String::new(),
            };
            proto("POST", &rpc, answer(rpc_response::Result::Versions(list)))
        };
        let no_devices = koidrive_proto::v1::DeviceList::default();
        let fake = Fake::start(vec![
            token(),
            tree(saved("a-1", 2)),
            kept(),
            tree(saved("a-2", 3)),
            kept(),
            proto("GET", "/v1/devices", no_devices),
            token(),
            tree(saved("a-1", 2)),
            kept(),
            tree(saved("a-2", 3)),
            kept(),
        ])
        .await;
        let (_dir, paths, root) = linked_on(fake.base.as_str());
        let file = root.join("a.txt");

        let done = versions(&paths, &file).await.unwrap();
        let refused = checked(&paths, &file, Some("a-2".into()), None)
            .await
            .unwrap_err();

        let current: Vec<serde_json::Value> = (done.data["versions"].as_array().unwrap().iter())
            .map(|version| version["current"].clone())
            .collect();
        assert_eq!(current, [serde_json::json!(true), serde_json::json!(false)]);
        assert_eq!(refused.reason, Some(Reason::AlreadyCurrent), "{refused:?}");
    }

    #[tokio::test]
    async fn a_file_that_keeps_changing_while_its_versions_are_read_asks_to_try_again() {
        let rpc = format!("/v1/spaces/{SPACE}/rpc");
        let saved = |id: &str, seq: u64| VersionInfo {
            seq,
            ..version(id, seq, seq as i64)
        };
        let tree = || {
            let page = TreePage {
                nodes: vec![NodeState {
                    current: Some(saved("a-1", 2)),
                    ..node("a", SPACE, "a.txt", NodeKind::File, 3, "")
                }],
                ..TreePage::default()
            };
            proto("POST", &rpc, answer(rpc_response::Result::Tree(page)))
        };
        let kept = || {
            let list = VersionList {
                versions: vec![saved("a-2", 3), saved("a-1", 2)],
                next_cursor: String::new(),
            };
            proto("POST", &rpc, answer(rpc_response::Result::Versions(list)))
        };
        let mut replies = vec![token()];
        for _ in 0..4 {
            replies.extend([tree(), kept()]);
        }
        let fake = Fake::start(replies).await;
        let (_dir, paths, root) = linked_on(fake.base.as_str());

        let error = versions(&paths, &root.join("a.txt")).await.unwrap_err();

        assert_eq!(error.code, koidrive_ipc::Code::Unavailable, "{error:?}");
    }

    #[tokio::test]
    async fn two_saves_in_one_commit_keep_the_current_version_the_tree_names() {
        let rpc = format!("/v1/spaces/{SPACE}/rpc");
        let in_one_commit = |id: &str| VersionInfo {
            seq: 2,
            ..version(id, 3, 2)
        };
        let tree = || {
            let page = TreePage {
                nodes: vec![NodeState {
                    current: Some(in_one_commit("a-1")),
                    ..node("a", SPACE, "a.txt", NodeKind::File, 3, "")
                }],
                ..TreePage::default()
            };
            proto("POST", &rpc, answer(rpc_response::Result::Tree(page)))
        };
        let kept = || {
            let list = VersionList {
                versions: vec![in_one_commit("a-2"), in_one_commit("a-1")],
                next_cursor: String::new(),
            };
            proto("POST", &rpc, answer(rpc_response::Result::Versions(list)))
        };
        let no_devices = koidrive_proto::v1::DeviceList::default();
        let fake = Fake::start(vec![
            token(),
            tree(),
            kept(),
            proto("GET", "/v1/devices", no_devices),
            token(),
            tree(),
            kept(),
        ])
        .await;
        let (_dir, paths, root) = linked_on(fake.base.as_str());
        let file = root.join("a.txt");

        let done = versions(&paths, &file).await.unwrap();
        let older = checked(&paths, &file, Some("a-2".into()), None).await;

        let current: Vec<serde_json::Value> = (done.data["versions"].as_array().unwrap().iter())
            .map(|version| version["current"].clone())
            .collect();
        assert_eq!(current, [serde_json::json!(false), serde_json::json!(true)]);
        assert!(older.is_ok(), "{:?}", older.err());
    }

    #[tokio::test]
    async fn versions_without_the_device_list_name_no_device() {
        let rpc = format!("/v1/spaces/{SPACE}/rpc");
        let tree = TreePage {
            nodes: vec![node("a", SPACE, "a.txt", NodeKind::File, 3, "")],
            ..TreePage::default()
        };
        let kept = VersionList {
            versions: vec![version("a-1", 3, 2), version("a-0", 3, 1)],
            next_cursor: String::new(),
        };
        let unavailable = crate::setup::fake::Reply {
            method: "GET",
            path: "/v1/devices".into(),
            status: 500,
            content_type: "text/plain",
            body: b"the device list is unavailable".to_vec(),
        };
        let fake = Fake::start(vec![
            token(),
            proto("POST", &rpc, answer(rpc_response::Result::Tree(tree))),
            proto("POST", &rpc, answer(rpc_response::Result::Versions(kept))),
            unavailable,
        ])
        .await;
        let (_dir, paths, root) = linked_on(fake.base.as_str());

        let done = versions(&paths, &root.join("a.txt")).await.unwrap();

        let names: Vec<serde_json::Value> = (done.data["versions"].as_array().unwrap().iter())
            .map(|version| version["device_name"].clone())
            .collect();
        assert_eq!(names, [serde_json::Value::Null, serde_json::Value::Null]);
    }

    #[tokio::test]
    async fn versions_name_the_device_that_saved_each() {
        let rpc = format!("/v1/spaces/{SPACE}/rpc");
        let tree = TreePage {
            nodes: vec![node("a", SPACE, "a.txt", NodeKind::File, 3, "")],
            ..TreePage::default()
        };
        let saved_by = |id: &str, device: &str| VersionInfo {
            device_id: device.into(),
            ..version(id, 3, 1)
        };
        let kept = VersionList {
            versions: vec![
                saved_by("a-2", "d1"),
                saved_by("a-1", "d0"),
                saved_by("a-0", "elsewhere"),
            ],
            next_cursor: String::new(),
        };
        let revoked = koidrive_proto::v1::DeviceInfo {
            device_id: "d0".into(),
            name: "old laptop".into(),
            revoked_at_ms: 5,
            ..Default::default()
        };
        let devices = koidrive_proto::v1::DeviceList {
            devices: vec![
                koidrive_proto::v1::DeviceInfo {
                    device_id: "d1".into(),
                    name: "laptop".into(),
                    ..Default::default()
                },
                revoked,
            ],
            next_cursor: String::new(),
        };
        let fake = Fake::start(vec![
            token(),
            proto("POST", &rpc, answer(rpc_response::Result::Tree(tree))),
            proto("POST", &rpc, answer(rpc_response::Result::Versions(kept))),
            proto("GET", "/v1/devices", devices),
        ])
        .await;
        let (_dir, paths, root) = linked_on(fake.base.as_str());

        let done = versions(&paths, &root.join("a.txt")).await.unwrap();

        let names: Vec<serde_json::Value> = (done.data["versions"].as_array().unwrap().iter())
            .map(|version| version["device_name"].clone())
            .collect();
        assert_eq!(
            names,
            [
                serde_json::json!("laptop"),
                serde_json::json!("old laptop"),
                serde_json::Value::Null
            ]
        );
    }

    #[test]
    fn a_reason_reaches_the_envelope_and_an_error_without_one_leaves_it_out() {
        let validator =
            jsonschema::validator_for(&crate::schema::envelope("history restore").unwrap())
                .unwrap();
        let printed = |error: CliError| {
            let envelope = serde_json::to_value(crate::report::Envelope::failed(
                "history restore",
                error,
                None,
            ))
            .unwrap();
            assert!(validator.is_valid(&envelope), "{envelope}");
            envelope["error"].clone()
        };

        let refused = printed(CliError::usage("no such version").because(Reason::NoSuchVersion));
        let failed = printed(CliError::from("the server sent an empty answer"));

        assert_eq!(refused["reason"], "no_such_version");
        assert!(failed.get("reason").is_none(), "{failed}");
    }

    #[test]
    fn a_request_names_its_path_when_it_was_asked_for_and_when_its_version_was_saved() {
        let restore = |path: Option<&str>| RestoreStatus {
            id: "0194f8a2-0000-7000-8000-000000000001".into(),
            path: path.map(String::from),
            asked_at_ms: 1_738_000_000_000,
            request: RestoreRequest::Version {
                node_id: "note".into(),
                version_id: "note-0".into(),
                saved_at_ms: Some(1_737_000_000_000),
            },
            state: RestoreState::Restored,
        };

        let found = RequestData::of(&restore(Some("docs/note.txt")));
        let unknown = RequestData::of(&restore(None));

        assert_eq!(
            (found.path.as_deref(), found.asked_at_ms),
            (Some("docs/note.txt"), 1_738_000_000_000)
        );
        assert_eq!(
            found.request,
            RequestedData::Version {
                node_id: "note".into(),
                version_id: "note-0".into(),
                saved_at_ms: Some(1_737_000_000_000),
            }
        );
        assert_eq!(unknown.path, None);
    }

    #[tokio::test]
    async fn a_finished_restore_is_reported_from_its_record_without_the_server() {
        let (_dir, paths, root, silent) = silent_profile(SPACE).await;
        let done = restore_again(&paths, &root, "done").await.unwrap();
        assert_eq!(done.text, "restore-1\n");
        assert_eq!(
            done.next,
            [koidrive_ipc::Next::of(&[
                "koi",
                "history",
                "requests",
                &root.display().to_string()
            ])]
        );

        let request::Begun::Claimed(claim) = request::begin(
            &paths,
            &RequestId::parse("going").unwrap(),
            &restore_key(&root),
        )
        .unwrap() else {
            panic!("the request was claimed before");
        };
        drop(claim);
        let error = restore_again(&paths, &root, "going").await.unwrap_err();
        assert_eq!(error.code, koidrive_ipc::Code::Interrupted, "{error:?}");

        let called =
            tokio::time::timeout(std::time::Duration::from_millis(100), silent.accept()).await;
        assert!(called.is_err(), "the server was called");
    }

    #[tokio::test]
    async fn a_replayed_restore_offers_no_follow_up_once_its_folder_syncs_another_space() {
        let (_dir, paths, root, _silent) =
            silent_profile("0194f8a2-0000-7000-8000-00000000000b").await;
        let done = restore_again(&paths, &root, "done").await.unwrap();
        assert_eq!(done.text, "restore-1\n");
        assert_eq!(done.note, "Asked to restore a.txt (restore restore-1).\n");
        assert!(done.next.is_empty(), "{:?}", done.next);
    }

    #[tokio::test]
    async fn without_a_daemon_the_commands_leave_a_fresh_profile_alone() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(&dir.path().join("profile"));
        let folder = dir.path().join("Sync");
        std::fs::create_dir(&folder).unwrap();

        let trashed = trash(&paths, &folder).await.unwrap_err();
        let kept = versions(&paths, &folder.join("a.txt")).await.unwrap_err();

        assert_eq!(
            trashed.code,
            koidrive_ipc::Code::AuthRequired,
            "{trashed:?}"
        );
        assert_eq!(kept.code, koidrive_ipc::Code::AuthRequired, "{kept:?}");
        assert!(!dir.path().join("profile").exists());
    }

    #[test]
    fn a_path_is_normalized_without_reading_the_disk() {
        assert_eq!(
            lexical(Path::new("/home/ada/./Koidrive/docs/../link.txt")),
            Path::new("/home/ada/Koidrive/link.txt")
        );
    }

    #[test]
    fn a_path_below_a_linked_folder_gives_its_names() {
        let root = Path::new("/home/ada/Koidrive");
        assert_eq!(
            below(root, Path::new("/home/ada/Koidrive/docs/a.txt")),
            Some(names("docs/a.txt"))
        );
        assert_eq!(below(root, root), Some(Vec::new()));
        assert_eq!(below(root, Path::new("/home/ada/Koidrive2/a.txt")), None);
        assert!(starts_with(&names("Docs/a.txt"), &names("docs")));
        assert!(!starts_with(&names("docs"), &names("docs/a.txt")));
    }

    #[test]
    fn the_trash_lists_each_deletion_once_with_what_it_held() {
        let live = vec![node("docs", SPACE, "docs", NodeKind::Dir, 0, "")];
        let trash = vec![
            node("photos", SPACE, "photos", NodeKind::Dir, 0, "g1"),
            node("cat", "photos", "cat.jpg", NodeKind::File, 300, "g1"),
            node("trip", "photos", "trip", NodeKind::Dir, 0, "g1"),
            node("dog", "trip", "dog.jpg", NodeKind::File, 700, "g1"),
            node("late", "photos", "late.jpg", NodeKind::File, 9, "g2"),
            NodeState {
                current: Some(version("old-1", 12, 1_700)),
                ..node("old", "docs", "old.txt", NodeKind::File, 12, "g3")
            },
            node("lost", "purged", "lost.txt", NodeKind::File, 5, "g4"),
        ];

        let deleted: HashMap<String, Trashed> = [("photos", 5_000, "desk"), ("old", 4_000, "gone")]
            .map(|(node, at, device)| {
                let deletion = Trashed {
                    node_id: node.into(),
                    trashed_at_ms: at,
                    device_id: device.into(),
                };
                (node.to_owned(), deletion)
            })
            .into();
        let names = HashMap::from([("desk".to_owned(), "Ada's Desk".to_owned())]);
        let found = deletions(&live, &trash, &deleted, SPACE);

        let summary: Vec<_> = found
            .iter()
            .map(|deleted| (deleted.names.join("/"), deleted.files, deleted.bytes))
            .collect();
        assert_eq!(
            summary,
            [
                ("docs/old.txt".to_owned(), 1, 12),
                ("photos".to_owned(), 2, 1000),
                ("photos/late.jpg".to_owned(), 1, 9),
                ("…/lost.txt".to_owned(), 1, 5),
            ]
        );
        let modified: Vec<Option<i64>> = (found.iter())
            .map(|deleted| deleted.entry(&names).modified_at_ms)
            .collect();
        assert_eq!(modified, [Some(1_700), None, Some(0), Some(0)]);
        let deletion: Vec<_> = (found.iter())
            .map(|deleted| {
                let entry = deleted.entry(&names);
                (
                    entry.deleted_at_ms,
                    entry.deleted_by_device_id,
                    entry.deleted_by_device_name,
                )
            })
            .collect();
        assert_eq!(
            deletion,
            [
                (Some(4_000), Some("gone".into()), None),
                (Some(5_000), Some("desk".into()), Some("Ada's Desk".into())),
                (None, None, None),
                (None, None, None),
            ]
        );
        let linked = FolderStatus {
            recovery: None,
            path: "/home/ada/Koidrive".into(),
            ..serde_json::from_value(serde_json::json!({
                "space_id": SPACE, "space_name": "Home", "path": "", "state": "up_to_date",
                "history_id": null, "last_seq": null, "head_seq": null, "remote_nodes": 0,
                "local_entries": null, "questions": 0
            }))
            .unwrap()
        };
        assert_eq!(
            trash_report(&found, &linked, &names, 5_000 + 3 * 3_600_000),
            "     12 B  docs/old.txt  (deleted 3 h ago)\n   1.0 kB  photos/  (folder, 2 files, deleted 3 h ago on Ada's Desk)\n      9 B  photos/late.jpg\n      5 B  …/lost.txt\n"
        );
        assert_eq!(
            trash_report(&[], &linked, &HashMap::new(), 0),
            "Nothing from /home/ada/Koidrive is in the trash.\n"
        );
        let forged = FolderStatus {
            recovery: None,
            path: "/home/ada/Docs\u{1b}[31m\nFORGED".into(),
            ..linked
        };
        assert_eq!(
            trash_report(&[], &forged, &HashMap::new(), 0),
            "Nothing from /home/ada/Docs\\u{1b}[31m\\u{a}FORGED is in the trash.\n"
        );
        assert_eq!(
            restores_report(&[], &forged),
            "No restore was asked for in /home/ada/Docs\\u{1b}[31m\\u{a}FORGED.\n"
        );
    }

    #[test]
    fn a_restore_picks_the_deletion_that_had_the_path() {
        let live = vec![
            node("docs", SPACE, "docs", NodeKind::Dir, 0, ""),
            node("note", "docs", "note.txt", NodeKind::File, 4, ""),
        ];
        let trash = vec![
            node("photos", SPACE, "photos", NodeKind::Dir, 0, "g1"),
            node("cat", "photos", "cat.jpg", NodeKind::File, 300, "g1"),
            node("old-b", "docs", "old.txt", NodeKind::File, 12, "g3"),
            node("old-a", "docs", "old.txt", NodeKind::File, 7, "g2"),
        ];
        let pick = |path: &str, node: Option<&str>| {
            trashed_at(&live, &trash, SPACE, &names(path), node)
                .map(|picked| picked.node_id.as_str())
                .map_err(|why| match why {
                    NotPicked::Several(found) => {
                        format!(
                            "several: {:?}",
                            found.iter().map(|node| &node.node_id).collect::<Vec<_>>()
                        )
                    }
                    other => format!("{other:?}"),
                })
        };

        assert_eq!(pick("photos", None), Ok("photos"));
        assert_eq!(
            pick("photos/cat.jpg", None),
            Ok("cat"),
            "a file trashed with its folder"
        );
        assert_eq!(
            pick("docs/old.txt", None),
            Err(r#"several: ["old-a", "old-b"]"#.into())
        );
        assert_eq!(pick("docs/old.txt", Some("old-b")), Ok("old-b"));
        assert_eq!(
            pick("docs/old.txt", Some("cat")),
            Err(r#"NotThere("cat")"#.into())
        );
        assert_eq!(
            pick("docs/note.txt", None),
            Err("None { live: true }".into())
        );
        assert_eq!(
            pick("docs/new.txt", None),
            Err("None { live: false }".into())
        );

        let several = trashed_at(&live, &trash, SPACE, &names("docs/old.txt"), None).unwrap_err();
        assert_eq!(
            several.message("docs/old.txt", &linked()),
            "2 deletions had the path docs/old.txt; pick one with --node: old-a (file, 7 B), old-b (file, 12 B)"
        );
    }

    fn linked() -> FolderStatus {
        FolderStatus {
            recovery: None,
            path: "/home/ada/Koidrive".into(),
            ..serde_json::from_value(serde_json::json!({
                "space_id": SPACE, "space_name": "Home", "path": "", "state": "up_to_date",
                "history_id": null, "last_seq": null, "head_seq": null, "remote_nodes": 0,
                "local_entries": null, "questions": 0
            }))
            .unwrap()
        }
    }

    #[test]
    fn the_restores_report_says_what_holds_a_pending_restore_back() {
        let restores = [
            RestoreStatus {
                id: "r1".into(),
                path: None,
                asked_at_ms: 0,
                request: RestoreRequest::FromTrash {
                    node_id: "cat".into(),
                    parent_id: None,
                    name: None,
                },
                state: RestoreState::Renamed {
                    name: "cat (2).jpg".into(),
                },
            },
            RestoreStatus {
                id: "r2".into(),
                path: None,
                asked_at_ms: 0,
                request: RestoreRequest::Version {
                    node_id: "note".into(),
                    version_id: "note-0".into(),
                    saved_at_ms: None,
                },
                state: RestoreState::Pending { waits_for: None },
            },
        ];
        let folder = FolderStatus {
            recovery: None,
            state: FolderState::Stopped,
            questions: 1,
            problem: Some("the root is missing".into()),
            ..linked()
        };

        assert_eq!(
            restores_report(&restores, &folder),
            concat!(
                "r1  deleted node cat\n",
                "  restored as cat (2).jpg, since its name was taken\n",
                "r2  version note-0 of node note\n",
                "  pending: waits for the folder to sync\n",
                "A pending restore goes out with a commit of the folder once the earlier changes of its node have synced.\n",
                "The folder is not syncing: run koi _daemon.\n",
                "The folder has a problem: the root is missing\n",
                "1 needs you: koi resolve lists them.\n",
            )
        );
        let two = FolderStatus {
            recovery: None,
            questions: 2,
            ..linked()
        };
        assert!(
            restores_report(&restores, &two).ends_with("2 need you: koi resolve lists them.\n")
        );
        assert_eq!(
            restores_report(&restores[..1], &folder),
            "r1  deleted node cat\n  restored as cat (2).jpg, since its name was taken\n"
        );
        assert_eq!(
            restores_report(&[], &folder),
            "No restore was asked for in /home/ada/Koidrive.\n"
        );
    }

    #[test]
    fn the_versions_report_marks_the_current_one() {
        let kept = [version("note-1", 20, 3_000_000), version("note-0", 10, 0)];

        let report = versions_report("note.txt", &kept, Some("note-1"), 3_000_000 + 7_200_000);

        assert_eq!(
            report,
            "note.txt: 2 versions, newest first\ncurrent       20 B  2 h ago         note-1\n              10 B  never           note-0\n"
        );
    }
}
