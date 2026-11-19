use super::*;

/// The most entries asked for in one page; the server sends at most this many.
const PAGE: u32 = 1000;
const UNEXPECTED: &str = "the server answered with another kind of result";

fn unexpected_result() -> CliError {
    CliError::from(UNEXPECTED).human("The server returned a different history result than requested. Run this command with --verbose for details, or koi diagnose to collect a report.")
}

async fn call(auth: &Auth, space: &str, call: Call) -> Result<rpc_response::Result, CliError> {
    match auth.api().rpc(auth, space, call).await.map_err(CliError::api_retry_safe)?.result {
        Some(rpc_response::Result::Error(error)) => Err(CliError::rpc_refusal(error)),
        Some(result) => Ok(result),
        None => Err(CliError::from("the server sent an empty answer")
            .human("The server returned no history result. Run this command with --verbose for details, or koi diagnose to collect a report.")),
    }
}

/// Every page of a listing, from the empty cursor to the page whose `next_cursor` is empty. A cursor the server sends
/// twice ends the listing with an error instead of a loop.
pub(super) async fn every_page<T>(
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
            return Err(CliError::from("the server sent the same page cursor twice")
                .human("The server repeated its history page marker, so Koidrive stopped reading the history. The results are incomplete. Run koi diagnose to collect a report."));
        }
        cursor = next;
    }
}

pub(super) async fn tree(auth: &Auth, space: &str) -> Result<Vec<NodeState>, CliError> {
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
            _ => Err(unexpected_result()),
        }
    })
    .await
}

pub(super) async fn device_names(auth: &Auth) -> Result<HashMap<String, String>, CliError> {
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

pub(super) async fn trash_nodes(
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
            _ => Err(unexpected_result()),
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

pub(super) async fn versions_and_current(
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

pub(super) async fn versions_of(
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
            _ => Err(unexpected_result()),
        }
    })
    .await
}

/// The live node at `names` below the space's root, whose node id is the space id. Names compare by `name_key`, under which
/// no two live children of a folder collide, so a name differing only in case or Unicode form still finds its node.
pub(super) fn find<'a>(
    live: &'a [NodeState],
    root: &str,
    names: &[String],
) -> Option<&'a NodeState> {
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
pub(super) fn names_of(
    node: &NodeState,
    by_id: &HashMap<&str, &NodeState>,
    root: &str,
) -> Vec<String> {
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
pub(super) struct Deleted<'a> {
    pub(super) node: &'a NodeState,
    pub(super) deletion: Option<&'a Trashed>,
    pub(super) names: Vec<String>,
    pub(super) files: u64,
    pub(super) bytes: u64,
}

impl Deleted<'_> {
    pub(super) fn entry(&self, names: &HashMap<String, String>) -> TrashEntry {
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

    pub(super) fn deleted_when(
        &self,
        names: &HashMap<String, String>,
        now_ms: i64,
    ) -> Option<String> {
        let deletion = self.deletion?;
        let when = ago(deletion.trashed_at_ms, now_ms);
        Some(match names.get(&deletion.device_id) {
            Some(name) => format!("deleted {when} on {}", printable(name)),
            None => format!("deleted {when}"),
        })
    }
}

/// The trash as deletions, sorted by path. A file counts with its own size; a folder counts the files trashed with it.
pub(super) fn deletions<'a>(
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

pub(super) fn files_trashed_with(trash: &[NodeState], folder: &NodeState) -> u64 {
    let trashed: HashMap<&str, &NodeState> = trash
        .iter()
        .map(|node| (node.node_id.as_str(), node))
        .collect();
    let inside = |node: &NodeState| {
        let mut at = node;
        for _ in 0..trash.len() {
            match trashed.get(at.parent_id.as_str()) {
                Some(parent) if parent.trash_group == at.trash_group => at = parent,
                _ => return false,
            }
            if at.node_id == folder.node_id {
                return true;
            }
        }
        false
    };
    (trash.iter())
        .filter(|node| node.kind() != NodeKind::Dir && inside(node))
        .count() as u64
}

pub(super) fn content_size(version: Option<&VersionInfo>) -> u64 {
    version
        .and_then(|version| version.content.as_ref())
        .map_or(0, |content| content.size)
}

pub(super) fn kind(node: &NodeState) -> NodeType {
    match node.kind() {
        NodeKind::Dir => NodeType::Folder,
        NodeKind::Symlink => NodeType::Symlink,
        _ => NodeType::File,
    }
}
