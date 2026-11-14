// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use koidrive_engine::names::name_key;
use koidrive_engine::tree::{RemoteNode, same_content};
use koidrive_fs::{EntryKind, Snapshot, check_root};
use koidrive_net::{ApiError, Auth, ServerError};
use koidrive_proto::v1::{
    CreateShareLinkRequest, ListTree, NodeKind, NodeState, NodeStatus, ShareLink, SpaceInfo,
    error::Code as ServerCode, rpc_request::Call, rpc_response,
};
use koidrive_store::Store;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::driver::{now_ms, store_error};
use crate::engine::{Engine, file_id};
use crate::observe::{open_beneath, read_content};
use crate::{Driver, DriverError, FolderState, FolderStatus};

const TREE_PAGE: u32 = 1000;
const SHORT_ID_DIGITS: usize = 10;
const LINK_ID_CHARS: usize = 22;
const PASSWORD_CHARS: std::ops::RangeInclusive<usize> = 8..=256;

/// A value that grants access, such as a link's URL or password. `Debug` shows `Secret(..)`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(pub String);

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}

/// How long a new link works.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Expiry {
    /// The server's default: 7 days after the link is made.
    #[default]
    Default,
    /// This many milliseconds after the request.
    After {
        ms: u64,
    },
    Never,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkState {
    Active,
    Expired,
    Revoked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    File,
    Folder,
}

/// The file or folder a link opens, where the space holds it now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    /// The names from the space's root down to it, joined with `/`; empty for the root.
    pub path: String,
    pub kind: TargetKind,
}

/// A share link as its owner sees it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedLink {
    /// Names the link without revealing it: the first 10 hex digits of the SHA-256 of its secret.
    pub id: String,
    pub url: Secret,
    pub space_id: String,
    pub space_name: String,
    /// The folder that syncs the space on this device, if one does.
    pub folder: Option<String>,
    pub node_id: String,
    /// None if the node or an ancestor is trashed or gone, or its path could not be fetched after revocation.
    pub target: Option<Target>,
    pub created_at_ms: i64,
    pub expires_at_ms: Option<i64>,
    pub revoked_at_ms: Option<i64>,
    pub has_password: bool,
    pub state: LinkState,
}

/// A link after [`Driver::revoke_share_link`]; `already` when it was revoked before.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revoked {
    pub link: SharedLink,
    pub already: bool,
}

impl Driver {
    /// Makes a link to the file or folder at the absolute `path`, which a linked folder holds and the space has.
    pub async fn share(
        &self,
        path: &Path,
        expiry: Expiry,
        password: Option<Secret>,
    ) -> Result<SharedLink, DriverError> {
        if let Some(password) = &password
            && !PASSWORD_CHARS.contains(&password.0.chars().count())
        {
            return Err(DriverError::PasswordLength);
        }
        let auth = self.signed_in()?;
        let folders = self.status().folders;
        let (folder, names) = locate(&folders, path)?;
        let nodes = server_tree(&auth, &folder.space_id).await?;
        let tree = Tree::new(&folder.space_id, &nodes);
        let Some(node_id) = tree.find(&names) else {
            return Err(match std::fs::symlink_metadata(path) {
                Ok(_) => DriverError::NotOnServer(path.to_owned()),
                Err(_) => DriverError::PathNotFound(path.to_owned()),
            });
        };
        let node_id = node_id.to_owned();
        let store = self.paths().store(&folder.space_id)?;
        let root = PathBuf::from(&folder.path);
        let requested = path.to_owned();
        let space_id = folder.space_id.clone();
        let selected = node_id.clone();
        let nodes = tokio::task::spawn_blocking(move || {
            let (engine, link) = Store::open(&store)
                .and_then(|mut store| {
                    store.transaction(|tx| Ok((Engine::load(tx, &space_id)?, tx.link()?)))
                })
                .map_err(store_error(&store))?;
            let unsynced = || DriverError::NotOnServer(requested.clone());
            let link = link.ok_or_else(unsynced)?;
            let tree = Tree::new(&space_id, &nodes);
            if !verified(&root, link.root_dev, &names, &selected, &tree, &engine)
                .map_err(|_| unsynced())?
            {
                return Err(unsynced());
            }
            Ok(nodes)
        })
        .await
        .map_err(|error| DriverError::Io {
            path: path.to_owned(),
            error: std::io::Error::other(error),
        })??;
        let tree = Tree::new(&folder.space_id, &nodes);
        let now = now_ms();
        let request = CreateShareLinkRequest {
            node_id,
            expires_at_ms: match expiry {
                Expiry::After { ms } => now.saturating_add_unsigned(ms),
                Expiry::Default | Expiry::Never => 0,
            },
            no_expiry: expiry == Expiry::Never,
            password: password.map(|password| password.0).unwrap_or_default(),
            request_id: koidrive_net::request_id(),
        };
        let link = koidrive_net::resend_lost(|| {
            (auth.api()).create_share_link(&auth, &folder.space_id, &request)
        })
        .await
        .map_err(|error| match error.server().map(ServerError::code) {
            Some(ServerCode::LimitExceeded) => DriverError::ShareLinkLimit,
            Some(ServerCode::NotFound) => DriverError::NotOnServer(path.to_owned()),
            _ => DriverError::Api(error),
        })?;
        Ok(described(
            link,
            &folder.space_name,
            Some(folder),
            &tree,
            now,
        ))
    }

    /// Every link of the spaces this device's user owns, ended ones included, newest first.
    pub async fn share_links(&self) -> Result<Vec<SharedLink>, DriverError> {
        let auth = self.signed_in()?;
        let folders = self.status().folders;
        let mut found = Vec::new();
        for (space, links) in links_by_space(&auth).await? {
            found.extend(self.describe(&auth, &folders, &space, links).await?);
        }
        found.sort_by_key(|link| std::cmp::Reverse(link.created_at_ms));
        Ok(found)
    }

    /// Ends the link that `link` names: its [`SharedLink::id`], its URL, or the secret in its URL. Revoking a link
    /// that is already revoked changes nothing.
    pub async fn revoke_share_link(&self, link: &Secret) -> Result<Revoked, DriverError> {
        let auth = self.signed_in()?;
        let wanted = LinkRef::parse(&link.0).ok_or(DriverError::NotAShareLink)?;
        let mut matched: Vec<(SpaceInfo, ShareLink)> = Vec::new();
        for (space, links) in links_by_space(&auth).await? {
            for link in links.into_iter().filter(|link| wanted.matches(link)) {
                matched.push((space.clone(), link));
            }
        }
        let (space, mut link) = match matched.len() {
            0 => return Err(DriverError::ShareLinkNotFound),
            1 => matched.remove(0),
            _ => {
                return Err(DriverError::ShareLinkAmbiguous(short_id(
                    &matched[0].1.link_id,
                )));
            }
        };
        let already = link.revoked_at_ms != 0;
        if !already {
            (auth.api())
                .revoke_share_link(&auth, &space.space_id, &link.link_id)
                .await?;
            link.revoked_at_ms = now_ms();
        }
        let folders = self.status().folders;
        let link = match self
            .describe(&auth, &folders, &space, vec![link.clone()])
            .await
        {
            Ok(mut links) => links.remove(0),
            Err(_) => SharedLink {
                target: None,
                ..described(
                    link,
                    &space.name,
                    folders
                        .iter()
                        .find(|folder| folder.space_id == space.space_id),
                    &Tree::new(&space.space_id, &[]),
                    now_ms(),
                )
            },
        };
        Ok(Revoked { link, already })
    }

    async fn describe(
        &self,
        auth: &Auth,
        folders: &[FolderStatus],
        space: &SpaceInfo,
        links: Vec<ShareLink>,
    ) -> Result<Vec<SharedLink>, DriverError> {
        let folder = (folders.iter()).find(|folder| folder.space_id == space.space_id);
        let cached = folder.is_some_and(|folder| folder.state == FolderState::UpToDate);
        let mut nodes = if cached {
            self.remote_nodes(&space.space_id)?
        } else {
            Vec::new()
        };
        let missing = !cached || {
            let tree = Tree::new(&space.space_id, &nodes);
            (links.iter()).any(|link| tree.target(&link.node_id).is_none())
        };
        if missing {
            nodes = server_tree(auth, &space.space_id).await?;
        }
        let tree = Tree::new(&space.space_id, &nodes);
        let now = now_ms();
        Ok(links
            .into_iter()
            .map(|link| described(link, &space.name, folder, &tree, now))
            .collect())
    }

    fn remote_nodes(&self, space_id: &str) -> Result<Vec<NodeState>, DriverError> {
        let path = self.paths().store(space_id)?;
        Store::open(&path)
            .and_then(|mut store| store.transaction(|tx| tx.remote_nodes()))
            .map_err(store_error(&path))
    }
}

fn verified(
    root: &Path,
    root_dev: u64,
    names: &[String],
    node_id: &str,
    tree: &Tree<'_>,
    engine: &Engine,
) -> std::io::Result<bool> {
    let flags = rustix::fs::OFlags::RDONLY
        | rustix::fs::OFlags::DIRECTORY
        | rustix::fs::OFlags::NOFOLLOW
        | rustix::fs::OFlags::CLOEXEC;
    let held = std::fs::File::from(rustix::fs::open(root, flags, rustix::fs::Mode::empty())?);
    let identity = held.metadata()?;
    if check_root(root, tree.root, root_dev).is_err() {
        return Ok(false);
    }
    let Some(target) = tree.target(node_id) else {
        return Ok(false);
    };
    let scope = Path::new(&target.path);
    let local_scope = if node_id == tree.root {
        PathBuf::new()
    } else {
        let Some(path) = engine.path_of(node_id) else {
            return Ok(false);
        };
        path
    };
    let mut disk = Snapshot::default();
    let scanned = disk
        .rescan(root, [PathBuf::new()])
        .map_err(std::io::Error::other)?;
    if scanned.root.dev != identity.dev() || scanned.root.ino != u128::from(identity.ino()) {
        return Ok(false);
    }
    if scanned
        .unreadable
        .iter()
        .chain(&scanned.unsettled)
        .any(|blocked| local_scope.starts_with(blocked) || blocked.starts_with(&local_scope))
    {
        return Ok(false);
    }
    let mut selected = None;
    for (&id, remote) in &tree.by_id {
        let Some(target) = tree.target(id) else {
            continue;
        };
        let remote_path = Path::new(&target.path);
        if !remote_path.starts_with(scope) && !scope.starts_with(remote_path) {
            continue;
        }
        let Some(local) = engine.trees.local().get(id) else {
            return Ok(false);
        };
        let Ok((_, remote_shape)) = RemoteNode::from_state(remote) else {
            return Ok(false);
        };
        if local.parked
            || engine.trees.ignored_local().contains(id)
            || !local.shape.same_position(&remote_shape.shape)
            || local.shape.kind != remote_shape.shape.kind
        {
            return Ok(false);
        }
        let Some(path) = engine.path_of(id) else {
            return Ok(false);
        };
        let Some(entry) = disk.entries.get(&path) else {
            return Ok(false);
        };
        if entry.id != file_id(local.identity) {
            return Ok(false);
        }
        match (entry.kind, remote_shape.shape.content.as_ref()) {
            (EntryKind::Dir, None) => {}
            (EntryKind::File, Some(expected)) => {
                let Some(content) = read_content(root, &path, entry, &|| false)? else {
                    return Ok(false);
                };
                if !same_content(&content, expected) {
                    return Ok(false);
                }
            }
            _ => return Ok(false),
        }
        if id == node_id {
            selected = Some(entry.id);
        }
    }
    let after = std::fs::symlink_metadata(root)?;
    if after.dev() != identity.dev()
        || after.ino() != identity.ino()
        || !after.is_dir()
        || check_root(root, tree.root, root_dev).is_err()
    {
        return Ok(false);
    }
    if node_id == tree.root {
        return Ok(true);
    }
    let Some(expected) = selected else {
        return Ok(false);
    };
    let typed: PathBuf = names.iter().collect();
    let actual = open_beneath(root, &typed)?.metadata()?;
    Ok(actual.dev() == expected.dev && u128::from(actual.ino()) == expected.ino)
}

enum LinkRef {
    Short(String),
    Secret(String),
}

impl LinkRef {
    fn parse(text: &str) -> Option<LinkRef> {
        let text = text.trim();
        let secret = match text.rsplit_once("/s/") {
            Some((_, rest)) => rest.split(['/', '?', '#']).next().unwrap_or_default(),
            None => text,
        };
        let base64url = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
        if secret.len() == LINK_ID_CHARS && secret.chars().all(base64url) {
            return Some(LinkRef::Secret(secret.to_owned()));
        }
        let short = text.to_ascii_lowercase();
        (short.len() == SHORT_ID_DIGITS && short.chars().all(|c| c.is_ascii_hexdigit()))
            .then_some(LinkRef::Short(short))
    }

    fn matches(&self, link: &ShareLink) -> bool {
        match self {
            LinkRef::Short(id) => short_id(&link.link_id) == *id,
            LinkRef::Secret(secret) => link.link_id == *secret,
        }
    }
}

fn short_id(link_id: &str) -> String {
    let digest = Sha256::digest(link_id.as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    hex[..SHORT_ID_DIGITS].to_owned()
}

fn described(
    link: ShareLink,
    space_name: &str,
    folder: Option<&FolderStatus>,
    tree: &Tree,
    now: i64,
) -> SharedLink {
    let expires_at_ms = (link.expires_at_ms != 0).then_some(link.expires_at_ms);
    let revoked_at_ms = (link.revoked_at_ms != 0).then_some(link.revoked_at_ms);
    let state = match (revoked_at_ms, expires_at_ms) {
        (Some(_), _) => LinkState::Revoked,
        (None, Some(expires)) if expires <= now => LinkState::Expired,
        (None, _) => LinkState::Active,
    };
    SharedLink {
        id: short_id(&link.link_id),
        target: tree.target(&link.node_id),
        url: Secret(link.url),
        space_id: link.space_id,
        space_name: space_name.to_owned(),
        folder: folder.map(|folder| folder.path.clone()),
        node_id: link.node_id,
        created_at_ms: link.created_at_ms,
        expires_at_ms,
        revoked_at_ms,
        has_password: link.has_password,
        state,
    }
}

struct Tree<'a> {
    root: &'a str,
    by_id: HashMap<&'a str, &'a NodeState>,
    by_name: HashMap<(&'a str, String), &'a str>,
}

impl<'a> Tree<'a> {
    fn new(root: &'a str, nodes: &'a [NodeState]) -> Self {
        let live: Vec<&NodeState> = (nodes.iter())
            .filter(|node| node.status() == NodeStatus::Live && node.node_id != root)
            .collect();
        Tree {
            root,
            by_id: (live.iter())
                .map(|node| (node.node_id.as_str(), *node))
                .collect(),
            by_name: (live.iter())
                .map(|node| {
                    let key = (node.parent_id.as_str(), name_key(&node.name));
                    (key, node.node_id.as_str())
                })
                .collect(),
        }
    }

    fn find(&self, names: &[String]) -> Option<&'a str> {
        (names.iter()).try_fold(self.root, |parent, name| {
            self.by_name.get(&(parent, name_key(name))).copied()
        })
    }

    fn target(&self, node_id: &str) -> Option<Target> {
        if node_id == self.root {
            return Some(Target {
                path: String::new(),
                kind: TargetKind::Folder,
            });
        }
        let node = self.by_id.get(node_id)?;
        let mut names = vec![node.name.as_str()];
        let mut parent = node.parent_id.as_str();
        while parent != self.root {
            let above = self.by_id.get(parent)?;
            names.push(above.name.as_str());
            parent = above.parent_id.as_str();
            if names.len() > self.by_id.len() {
                return None;
            }
        }
        names.reverse();
        Some(Target {
            path: names.join("/"),
            kind: match node.kind() {
                NodeKind::Dir => TargetKind::Folder,
                _ => TargetKind::File,
            },
        })
    }
}

fn locate<'a>(
    folders: &'a [FolderStatus],
    path: &Path,
) -> Result<(&'a FolderStatus, Vec<String>), DriverError> {
    if !path.is_absolute() {
        return Err(DriverError::RelativePath(path.to_owned()));
    }
    let outside = || DriverError::OutsideLinkedFolders(path.to_owned());
    let mut components: VecDeque<_> = path
        .components()
        .map(|component| component.as_os_str().to_owned())
        .collect();
    let mut prefix = PathBuf::new();
    let mut symlinks = 0;
    while let Some(component) = components.pop_front() {
        match Path::new(&component).components().next().unwrap() {
            Component::CurDir => {}
            Component::ParentDir => {
                if folders
                    .iter()
                    .any(|folder| prefix.starts_with(&folder.path))
                    && !std::fs::symlink_metadata(&prefix).is_ok_and(|metadata| metadata.is_dir())
                {
                    return Err(DriverError::NotOnServer(path.to_owned()));
                }
                prefix.pop();
            }
            Component::Normal(name) => {
                let inside = folders
                    .iter()
                    .any(|folder| prefix.starts_with(&folder.path));
                prefix.push(name);
                let metadata = std::fs::symlink_metadata(&prefix);
                if inside {
                    if metadata.is_ok_and(|metadata| {
                        metadata.file_type().is_symlink()
                            || (!components.is_empty() && !metadata.is_dir())
                    }) {
                        return Err(DriverError::NotOnServer(path.to_owned()));
                    }
                    continue;
                }
                let metadata = metadata.map_err(|_| outside())?;
                if metadata.file_type().is_symlink() {
                    symlinks += 1;
                    if symlinks > 40 {
                        return Err(outside());
                    }
                    let target = std::fs::read_link(&prefix).map_err(|_| outside())?;
                    prefix.pop();
                    for component in target.components().rev() {
                        components.push_front(component.as_os_str().to_owned());
                    }
                } else if !metadata.is_dir() {
                    return Err(outside());
                } else {
                    prefix = std::fs::canonicalize(&prefix).map_err(|_| outside())?;
                }
            }
            other => prefix.push(other),
        }
    }
    folders
        .iter()
        .filter_map(|folder| Some((folder, below(Path::new(&folder.path), &prefix)?)))
        .max_by_key(|(folder, _)| folder.path.len())
        .ok_or_else(outside)
}

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

async fn links_by_space(auth: &Auth) -> Result<Vec<(SpaceInfo, Vec<ShareLink>)>, DriverError> {
    let mut found = Vec::new();
    for space in auth.api().list_spaces(auth).await?.spaces {
        let mut links = Vec::new();
        let mut cursor = String::new();
        let mut seen = HashSet::new();
        loop {
            let api = auth.api();
            let page = match api.list_share_links(auth, &space.space_id, &cursor).await {
                Ok(page) => page,
                Err(ApiError::Server { status: 403, .. }) => break,
                Err(error) => return Err(error.into()),
            };
            links.extend(page.links);
            if page.next_cursor.is_empty() {
                break;
            }
            if !seen.insert(page.next_cursor.clone()) {
                return Err(ApiError::Decode("ShareLinkList").into());
            }
            cursor = page.next_cursor;
        }
        if !links.is_empty() {
            found.push((space, links));
        }
    }
    Ok(found)
}

async fn server_tree(auth: &Auth, space_id: &str) -> Result<Vec<NodeState>, DriverError> {
    let mut nodes = Vec::new();
    let mut cursor = String::new();
    let mut seen = HashSet::new();
    let mut head = None;
    loop {
        let call = Call::ListTree(ListTree {
            cursor,
            limit: TREE_PAGE,
        });
        let page = match auth.api().rpc(auth, space_id, call).await?.result {
            Some(rpc_response::Result::Tree(page)) => page,
            Some(rpc_response::Result::Error(error)) => {
                return Err(DriverError::Refused(ServerError(error)));
            }
            _ => return Err(ApiError::Decode("TreePage").into()),
        };
        let at = (page.head_seq, page.head_commit_id.clone());
        if head.as_ref().is_some_and(|head| head != &at) {
            return Err(DriverError::ShareTreeChanged);
        }
        head = Some(at);
        nodes.extend(page.nodes);
        if page.next_cursor.is_empty() {
            return Ok(nodes);
        }
        if !seen.insert(page.next_cursor.clone()) {
            return Err(ApiError::Decode("TreePage").into());
        }
        cursor = page.next_cursor;
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;
    use std::time::Duration;

    use koidrive_net::{Api, Credentials};
    use koidrive_proto::v1::{AccessToken, RpcResponse, ShareLinkList, SpaceList, TreePage};
    use prost::Message;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::Paths;

    const SPACE: &str = "space-1";

    fn node(id: &str, parent: &str, name: &str, kind: NodeKind, status: NodeStatus) -> NodeState {
        NodeState {
            node_id: id.into(),
            parent_id: parent.into(),
            name: name.into(),
            kind: kind as i32,
            status: status as i32,
            ..NodeState::default()
        }
    }

    fn link(link_id: &str, node_id: &str) -> ShareLink {
        ShareLink {
            link_id: link_id.into(),
            url: format!("https://files.example.com/s/{link_id}"),
            space_id: SPACE.into(),
            node_id: node_id.into(),
            created_at_ms: 1_000,
            ..ShareLink::default()
        }
    }

    fn folder(path: &Path) -> FolderStatus {
        FolderStatus {
            space_id: SPACE.into(),
            space_name: "Team".into(),
            path: path.to_str().unwrap().into(),
            state: FolderState::UpToDate,
            history_id: None,
            last_seq: None,
            head_seq: None,
            remote_nodes: 0,
            local_entries: None,
            questions: 0,
            problem: None,
            wait: None,
            recovery: None,
            progress: None,
        }
    }

    fn names(path: &str) -> Vec<String> {
        path.split('/').map(str::to_owned).collect()
    }

    async fn serve(replies: Vec<(String, u16, Vec<u8>)>) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}/", listener.local_addr().unwrap());
        let served = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(10), async move {
                for (expected, status, body) in replies {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    let mut buffer = [0; 4096];
                    let head_end = loop {
                        let read = socket.read(&mut buffer).await.unwrap();
                        assert_ne!(read, 0);
                        request.extend_from_slice(&buffer[..read]);
                        if let Some(at) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                            break at + 4;
                        }
                    };
                    let head = String::from_utf8_lossy(&request[..head_end]);
                    assert_eq!(head.lines().next().unwrap(), format!("{expected} HTTP/1.1"));
                    let length: usize = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse().unwrap())
                        })
                        .unwrap_or(0);
                    while request.len() < head_end + length {
                        let read = socket.read(&mut buffer).await.unwrap();
                        assert_ne!(read, 0);
                        request.extend_from_slice(&buffer[..read]);
                    }
                    let head = format!(
                        "HTTP/1.1 {status} Reply\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    socket.write_all(head.as_bytes()).await.unwrap();
                    socket.write_all(&body).await.unwrap();
                }
            })
            .await
            .unwrap();
        });
        (server, served)
    }

    fn auth(server: &str) -> Auth {
        Auth::new(
            Api::local_development(server.parse().unwrap()).unwrap(),
            Credentials {
                user_id: "user".into(),
                device_id: "device".into(),
                refresh_credential: "refresh".into(),
            },
        )
        .with_token(AccessToken {
            token: "token".into(),
            expires_at_ms: i64::MAX,
            ..AccessToken::default()
        })
    }

    fn cache(driver: &Driver, space_id: &str, nodes: &[NodeState]) {
        let path = driver.paths().store(space_id).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        Store::open(path)
            .unwrap()
            .transaction(|tx| {
                for node in nodes {
                    tx.put_remote_node(node)?;
                }
                Ok::<_, koidrive_store::Error>(())
            })
            .unwrap();
    }

    #[test]
    fn a_link_is_named_by_its_short_id_its_secret_or_its_url() {
        let secret = "AAAAAAAAAAAAAAAAAAAAAA";
        let other = link("BBBBBBBBBBBBBBBBBBBBBQ", "n");
        let shared = link(secret, "n");
        let id = short_id(secret);
        let named = [
            id.clone(),
            id.to_ascii_uppercase(),
            secret.to_owned(),
            format!("https://files.example.com/s/{secret}"),
            format!(" https://files.example.com/prefix/s/{secret}/?x#y\n"),
        ];
        for text in named {
            let wanted = LinkRef::parse(&text).unwrap();
            assert!(wanted.matches(&shared), "{text:?}");
            assert!(!wanted.matches(&other), "{text:?}");
        }
        assert_eq!(id.len(), 10);
    }

    #[test]
    fn text_that_names_no_link_is_refused() {
        for text in [
            "",
            "abc",
            "0123456789a",
            "012345678g",
            "AAAAAAAAAAAAAAAAAAAAAAA",
            "AAAAAAAAAAAAAAAAAAAAA=",
            "https://files.example.com/AAAAAAAAAAAAAAAAAAAAAA",
        ] {
            assert!(LinkRef::parse(text).is_none(), "{text:?}");
        }
    }

    #[test]
    fn a_path_finds_its_live_node_by_name_key_and_nothing_below_a_trashed_folder() {
        let nodes = vec![
            node("docs", SPACE, "Docs", NodeKind::Dir, NodeStatus::Live),
            node("cafe", "docs", "Café.txt", NodeKind::File, NodeStatus::Live),
            node("old", SPACE, "Old", NodeKind::Dir, NodeStatus::Trashed),
            node("kept", "old", "kept.txt", NodeKind::File, NodeStatus::Live),
            node(
                "gone",
                "docs",
                "gone.txt",
                NodeKind::File,
                NodeStatus::Trashed,
            ),
        ];
        let tree = Tree::new(SPACE, &nodes);

        assert_eq!(tree.find(&names("docs/CAFE\u{301}.TXT")), Some("cafe"));
        assert_eq!(tree.find(&[]), Some(SPACE));
        assert_eq!(tree.find(&names("Old/kept.txt")), None);
        assert_eq!(tree.find(&names("Docs/gone.txt")), None);
        assert_eq!(
            tree.target("cafe"),
            Some(Target {
                path: "Docs/Café.txt".into(),
                kind: TargetKind::File
            })
        );
        assert_eq!(
            tree.target(SPACE),
            Some(Target {
                path: String::new(),
                kind: TargetKind::Folder
            })
        );
        assert_eq!(tree.target("kept"), None);
        assert_eq!(tree.target("gone"), None);
        assert_eq!(tree.target("unknown"), None);
    }

    #[test]
    fn a_cycle_of_parents_reaches_no_target() {
        let nodes = vec![
            node("a", "b", "a", NodeKind::Dir, NodeStatus::Live),
            node("b", "a", "b", NodeKind::Dir, NodeStatus::Live),
        ];
        assert_eq!(Tree::new(SPACE, &nodes).target("a"), None);
    }

    #[test]
    fn a_link_is_revoked_expired_or_active_by_its_record_and_the_clock() {
        let nodes = vec![node("f", SPACE, "f.txt", NodeKind::File, NodeStatus::Live)];
        let tree = Tree::new(SPACE, &nodes);
        let state = |expires_at_ms: i64, revoked_at_ms: i64| {
            let link = ShareLink {
                expires_at_ms,
                revoked_at_ms,
                ..link("AAAAAAAAAAAAAAAAAAAAAA", "f")
            };
            described(link, "Team", None, &tree, 5_000).state
        };
        assert_eq!(state(0, 0), LinkState::Active);
        assert_eq!(state(5_001, 0), LinkState::Active);
        assert_eq!(state(5_000, 0), LinkState::Expired);
        assert_eq!(state(0, 4_000), LinkState::Revoked);
        assert_eq!(state(1_000, 4_000), LinkState::Revoked);
    }

    #[test]
    fn a_described_link_keeps_its_secret_out_of_debug_output() {
        let secret = "AAAAAAAAAAAAAAAAAAAAAA";
        let nodes = vec![node("f", SPACE, "f.txt", NodeKind::File, NodeStatus::Live)];
        let shared = described(
            link(secret, "f"),
            "Team",
            None,
            &Tree::new(SPACE, &nodes),
            0,
        );
        assert_eq!(
            shared.url.0,
            format!("https://files.example.com/s/{secret}")
        );
        assert!(!format!("{shared:?}").contains(secret));
        assert!(!format!("{:?}", Secret("hunter22".into())).contains("hunter22"));
    }

    #[test]
    fn a_path_is_located_below_its_linked_folder_without_following_links_inside_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let linked = root.join("Sync");
        let outside = root.join("Elsewhere");
        std::fs::create_dir_all(linked.join("Docs")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, linked.join("Docs/away")).unwrap();
        symlink(linked.join("Docs"), root.join("shortcut")).unwrap();
        let folders = [folder(&linked)];

        let located = |path: &Path| locate(&folders, path).map(|(_, names)| names);
        assert_eq!(located(&linked).unwrap(), Vec::<String>::new());
        assert!(matches!(
            located(&linked.join("Docs/away/x.txt")),
            Err(DriverError::NotOnServer(_))
        ));
        assert_eq!(
            located(&root.join("shortcut/new.txt")).unwrap(),
            names("Docs/new.txt")
        );
        assert_eq!(
            located(&linked.join("Docs/../Docs/./a.txt")).unwrap(),
            names("Docs/a.txt")
        );
        assert!(matches!(
            located(&outside.join("x.txt")),
            Err(DriverError::OutsideLinkedFolders(_))
        ));
        assert!(matches!(
            located(Path::new("Sync/x.txt")),
            Err(DriverError::RelativePath(_))
        ));
    }

    #[test]
    fn sharing_verifies_local_identity_and_content_for_files_and_subtrees() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Sync");
        std::fs::create_dir_all(root.join("Docs")).unwrap();
        koidrive_fs::write_marker(
            &root,
            &koidrive_fs::Marker {
                space_id: SPACE.into(),
                effects: 0,
            },
        )
        .unwrap();
        let file = root.join("Docs/contract.txt");
        std::fs::write(&file, b"private").unwrap();
        let snapshot = Snapshot::scan(&root).unwrap();
        let content = read_content(
            &root,
            Path::new("Docs/contract.txt"),
            &snapshot.entries[Path::new("Docs/contract.txt")],
            &|| false,
        )
        .unwrap()
        .unwrap();
        let nodes = vec![
            node("docs", SPACE, "Docs", NodeKind::Dir, NodeStatus::Live),
            NodeState {
                current: Some(koidrive_proto::v1::VersionInfo {
                    content: Some(content),
                    ..Default::default()
                }),
                ..node(
                    "file",
                    "docs",
                    "contract.txt",
                    NodeKind::File,
                    NodeStatus::Live,
                )
            },
            node("trash", SPACE, "Trash", NodeKind::Dir, NodeStatus::Trashed),
            node(
                "hidden",
                "trash",
                "hidden.txt",
                NodeKind::File,
                NodeStatus::Live,
            ),
        ];
        let mut store = Store::open(dir.path().join("store.sqlite")).unwrap();
        let engine = store
            .transaction(|tx| {
                for (node, path) in nodes.iter().zip(["Docs", "Docs/contract.txt"]) {
                    let (_, remote) = RemoteNode::from_state(node).unwrap();
                    tx.put_remote_node(node)?;
                    let shape = koidrive_store::Shape {
                        parent_id: remote.shape.parent,
                        name: remote.shape.name,
                        kind: node.kind,
                        content: remote.shape.content,
                    };
                    tx.put_synced_node(&koidrive_store::SyncedNode {
                        node_id: node.node_id.clone(),
                        shape: shape.clone(),
                    })?;
                    tx.put_local_node(&koidrive_store::LocalNode {
                        node_id: node.node_id.clone(),
                        shape,
                        identity: snapshot.entries[Path::new(path)].id,
                        parked: false,
                    })?;
                }
                Engine::load(tx, SPACE)
            })
            .unwrap();
        let tree = Tree::new(SPACE, &nodes);
        let dev = std::fs::metadata(&root).unwrap().dev();
        let valid = || {
            [
                verified(
                    &root,
                    dev,
                    &names("Docs/contract.txt"),
                    "file",
                    &tree,
                    &engine,
                )
                .unwrap(),
                verified(&root, dev, &names("Docs"), "docs", &tree, &engine).unwrap(),
                verified(&root, dev, &[], SPACE, &tree, &engine).unwrap(),
            ]
        };
        assert_eq!(valid(), [true; 3]);
        let private = root.join("Docs/private");
        std::fs::create_dir(&private).unwrap();
        std::fs::set_permissions(&private, std::os::unix::fs::PermissionsExt::from_mode(0o0))
            .unwrap();
        let readable_file = valid();
        std::fs::set_permissions(
            &private,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        assert_eq!(readable_file, [true, false, false]);
        std::fs::write(root.join("not-synced-yet.txt"), b"a future folder addition").unwrap();
        assert_eq!(valid(), [true; 3]);

        let mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
        std::fs::write(&file, b"public!").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(mtime))
            .unwrap();
        assert_eq!(valid(), [false; 3]);
        std::fs::write(&file, b"private").unwrap();
        assert_eq!(valid(), [true; 3]);

        let saved = root.join("Docs/contract-private.txt");
        std::fs::rename(&file, &saved).unwrap();
        std::fs::write(&file, b"private").unwrap();
        assert_eq!(valid(), [false; 3]);
        std::fs::remove_file(&file).unwrap();
        std::fs::rename(&saved, &file).unwrap();
        assert_eq!(valid(), [true; 3]);
        if !root.join("DOCS").exists() {
            std::fs::create_dir(root.join("DOCS")).unwrap();
            std::fs::write(root.join("DOCS/contract.txt"), b"private").unwrap();
            assert!(
                !verified(
                    &root,
                    dev,
                    &names("DOCS/contract.txt"),
                    "file",
                    &tree,
                    &engine
                )
                .unwrap()
            );
        }
        std::fs::rename(root.join("Docs"), root.join("Moved")).unwrap();
        std::fs::create_dir(root.join("Docs")).unwrap();
        std::fs::write(&file, b"private").unwrap();
        assert_eq!(valid(), [false; 3]);
    }

    #[tokio::test]
    async fn server_tree_pages_must_describe_one_unchanged_head() {
        for second in [(10, "first"), (11, "next"), (10, "other-history")] {
            let replies = [(10, "first"), second]
                .into_iter()
                .enumerate()
                .map(|(i, (seq, id))| {
                    let page = TreePage {
                        next_cursor: if i == 0 { "next".into() } else { String::new() },
                        head_seq: seq,
                        head_commit_id: id.into(),
                        ..Default::default()
                    };
                    (
                        format!("POST /v1/spaces/{SPACE}/rpc"),
                        200,
                        RpcResponse {
                            result: Some(rpc_response::Result::Tree(page)),
                            ..Default::default()
                        }
                        .encode_to_vec(),
                    )
                })
                .collect();
            let (server, served) = serve(replies).await;
            let result = server_tree(&auth(&server), SPACE).await;
            if second == (10, "first") {
                assert!(result.unwrap().is_empty());
            } else {
                assert!(matches!(result, Err(DriverError::ShareTreeChanged)));
            }
            served.await.unwrap();
        }
    }

    #[test]
    fn linked_roots_accept_the_spellings_the_filesystem_accepts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let linked = root.join("Projects/Cafe\u{301}");
        std::fs::create_dir_all(linked.join("Docs")).unwrap();
        std::fs::create_dir(root.join("Elsewhere")).unwrap();
        symlink(root.join("Elsewhere"), linked.join("Docs/away")).unwrap();
        symlink(&linked, root.join("Alias")).unwrap();
        let linked = linked.canonicalize().unwrap();
        let folders = [folder(&linked)];

        for spelling in [
            "Projects/Cafe\u{301}",
            "projects/Cafe\u{301}",
            "PROJECTS/CAFE\u{301}",
            "Projects/Caf\u{e9}",
            "projects/CAF\u{c9}",
            "Alias",
            "alias",
        ] {
            let typed = root.join(spelling);
            let path = typed.join("Docs/plan.txt");
            if !typed.exists() {
                assert!(matches!(
                    locate(&folders, &path),
                    Err(DriverError::OutsideLinkedFolders(_))
                ));
                continue;
            }
            let (found, path) = locate(&folders, &path).unwrap();
            assert_eq!(found.path, linked.to_str().unwrap());
            assert_eq!(path, names("Docs/plan.txt"));
            assert_eq!(
                locate(&folders, &typed.join("Docs/../server-only.txt"))
                    .unwrap()
                    .1,
                names("server-only.txt")
            );
            assert!(matches!(
                locate(&folders, &typed.join("Docs/away/..")),
                Err(DriverError::NotOnServer(_))
            ));
        }
    }

    #[test]
    fn external_symlinks_are_resolved_before_parent_components() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let linked = root.join("Sync");
        std::fs::create_dir_all(linked.join("Docs")).unwrap();
        symlink(linked.join("Docs"), root.join("shortcut")).unwrap();
        symlink("Sync/Docs", root.join("relative")).unwrap();
        symlink("relative", root.join("chain")).unwrap();
        let folders = [folder(&linked)];

        for alias in ["shortcut", "relative", "chain"] {
            let (_, found) = locate(&folders, &root.join(alias).join("../Sync")).unwrap();
            assert_eq!(found, names("Sync"));
            let (_, found) =
                locate(&folders, &root.join(alias).join("../server-only/note.txt")).unwrap();
            assert_eq!(found, names("server-only/note.txt"));
        }
    }

    #[test]
    fn parent_components_can_leave_and_reenter_linked_folders() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let linked = root.join("Sync");
        let other = root.join("Other");
        std::fs::create_dir_all(linked.join("Docs")).unwrap();
        std::fs::create_dir_all(other.join("Docs")).unwrap();
        symlink(linked.join("Docs"), root.join("shortcut")).unwrap();
        std::fs::write(root.join("file"), b"file").unwrap();
        symlink("cycle", root.join("cycle")).unwrap();
        let folders = [folder(&linked), folder(&other)];

        for path in [
            linked.join("../shortcut/../Sync"),
            linked.join("Docs/../../shortcut/../Sync"),
        ] {
            let (found, names) = locate(&folders, &path).unwrap();
            assert_eq!(found.path, linked.to_str().unwrap());
            assert_eq!(names, vec!["Sync"]);
        }
        let (found, names) = locate(&folders, &linked.join("../Other/Docs/../file")).unwrap();
        assert_eq!(found.path, other.to_str().unwrap());
        assert_eq!(names, vec!["file"]);
        for path in [
            "server-only/..",
            "server-only/../Docs",
            "Docs/missing/../..",
        ] {
            assert!(matches!(
                locate(&folders, &linked.join(path)),
                Err(DriverError::NotOnServer(_))
            ));
        }
        for path in [
            linked.join(".."),
            linked.join("../missing/../Sync"),
            root.join("file/../Sync"),
            root.join("cycle/../Sync"),
        ] {
            assert!(matches!(
                locate(&folders, &path),
                Err(DriverError::OutsideLinkedFolders(_))
            ));
        }
    }

    #[test]
    fn internal_symlinks_are_rejected_even_when_parent_components_cancel_them() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let linked = root.join("Sync");
        std::fs::create_dir_all(linked.join("Docs")).unwrap();
        std::fs::create_dir(root.join("Elsewhere")).unwrap();
        symlink(root.join("Elsewhere"), linked.join("away")).unwrap();
        symlink("Docs", linked.join("inside")).unwrap();
        symlink("missing", linked.join("broken")).unwrap();
        symlink("cycle", linked.join("cycle")).unwrap();
        symlink(linked.join("away"), root.join("shortcut")).unwrap();
        let folders = [folder(&linked)];

        for name in ["away", "inside", "broken", "cycle"] {
            for suffix in ["", "/child", "/..", "/../Docs", "/../../Sync"] {
                assert!(matches!(
                    locate(&folders, &linked.join(format!("{name}{suffix}"))),
                    Err(DriverError::NotOnServer(_))
                ));
            }
        }
        for suffix in ["shortcut", "shortcut/..", "shortcut/../../Sync"] {
            assert!(matches!(
                locate(&folders, &root.join(suffix)),
                Err(DriverError::NotOnServer(_))
            ));
        }
    }

    #[test]
    fn an_internal_file_cannot_be_traversed_or_cancelled_into_a_broader_target() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let linked = root.join("Sync");
        std::fs::create_dir(&linked).unwrap();
        std::fs::write(linked.join("file.txt"), b"file").unwrap();
        let folders = [folder(&linked)];

        assert_eq!(
            locate(&folders, &linked.join("file.txt")).unwrap().1,
            names("file.txt")
        );
        for path in ["file.txt/child", "file.txt/..", "file.txt/../other.txt"] {
            assert!(matches!(
                locate(&folders, &linked.join(path)),
                Err(DriverError::NotOnServer(_))
            ));
        }
    }

    #[tokio::test]
    async fn folders_that_are_not_up_to_date_describe_current_server_targets() {
        let dir = tempfile::tempdir().unwrap();
        let driver = Driver::open(Paths::under(dir.path())).unwrap();
        let space = SpaceInfo {
            space_id: "a1".into(),
            name: "Team".into(),
            ..SpaceInfo::default()
        };
        cache(
            &driver,
            &space.space_id,
            &[
                node("docs", "a1", "Before", NodeKind::Dir, NodeStatus::Live),
                node(
                    "file",
                    "docs",
                    "before.txt",
                    NodeKind::File,
                    NodeStatus::Live,
                ),
                node("trash", "a1", "Trash", NodeKind::Dir, NodeStatus::Live),
                node(
                    "child",
                    "trash",
                    "child.txt",
                    NodeKind::File,
                    NodeStatus::Live,
                ),
                node("gone", "a1", "gone.txt", NodeKind::File, NodeStatus::Live),
            ],
        );
        for state in [
            FolderState::Stopped,
            FolderState::Connecting,
            FolderState::Bootstrapping,
            FolderState::CatchingUp,
            FolderState::Offline,
            FolderState::Halted,
        ] {
            let reply = RpcResponse {
                result: Some(rpc_response::Result::Tree(TreePage {
                    nodes: vec![
                        node("docs", "a1", "After", NodeKind::Dir, NodeStatus::Live),
                        node("file", "a1", "moved.txt", NodeKind::File, NodeStatus::Live),
                        node("trash", "a1", "Trash", NodeKind::Dir, NodeStatus::Trashed),
                        node(
                            "child",
                            "trash",
                            "child.txt",
                            NodeKind::File,
                            NodeStatus::Live,
                        ),
                    ],
                    ..TreePage::default()
                })),
                ..RpcResponse::default()
            };
            let (server, served) = serve(vec![(
                "POST /v1/spaces/a1/rpc".into(),
                200,
                reply.encode_to_vec(),
            )])
            .await;
            let mut folder = folder(dir.path());
            folder.space_id = space.space_id.clone();
            folder.state = state;
            let links = ["docs", "file", "child", "gone"].map(|node| ShareLink {
                space_id: space.space_id.clone(),
                ..link("AAAAAAAAAAAAAAAAAAAAAA", node)
            });
            let described = tokio::time::timeout(
                Duration::from_secs(10),
                driver.describe(&auth(&server), &[folder], &space, links.into()),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(described[0].target.as_ref().unwrap().path, "After");
            assert_eq!(described[1].target.as_ref().unwrap().path, "moved.txt");
            assert_eq!(described[2].target, None);
            assert_eq!(described[3].target, None);
            served.await.unwrap();
        }
    }

    #[tokio::test]
    async fn up_to_date_folders_use_cached_targets() {
        let dir = tempfile::tempdir().unwrap();
        let driver = Driver::open(Paths::under(dir.path())).unwrap();
        let space = SpaceInfo {
            space_id: "a1".into(),
            name: "Team".into(),
            ..SpaceInfo::default()
        };
        cache(
            &driver,
            &space.space_id,
            &[node(
                "file",
                "a1",
                "file.txt",
                NodeKind::File,
                NodeStatus::Live,
            )],
        );
        let mut folder = folder(dir.path());
        folder.space_id = space.space_id.clone();
        let links = vec![ShareLink {
            space_id: space.space_id.clone(),
            ..link("AAAAAAAAAAAAAAAAAAAAAA", "file")
        }];
        let described = driver
            .describe(&auth("http://127.0.0.1:1/"), &[folder], &space, links)
            .await
            .unwrap();
        assert_eq!(described[0].target.as_ref().unwrap().path, "file.txt");
    }

    #[tokio::test]
    async fn up_to_date_folders_refresh_targets_missing_from_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let driver = Driver::open(Paths::under(dir.path())).unwrap();
        let space = SpaceInfo {
            space_id: "a1".into(),
            name: "Team".into(),
            ..SpaceInfo::default()
        };
        cache(&driver, &space.space_id, &[]);
        let reply = RpcResponse {
            result: Some(rpc_response::Result::Tree(TreePage {
                nodes: vec![node(
                    "file",
                    "a1",
                    "file.txt",
                    NodeKind::File,
                    NodeStatus::Live,
                )],
                ..TreePage::default()
            })),
            ..RpcResponse::default()
        };
        let (server, served) = serve(vec![(
            "POST /v1/spaces/a1/rpc".into(),
            200,
            reply.encode_to_vec(),
        )])
        .await;
        let mut folder = folder(dir.path());
        folder.space_id = space.space_id.clone();
        let links = ["file", "unknown"].map(|node| ShareLink {
            space_id: space.space_id.clone(),
            ..link("AAAAAAAAAAAAAAAAAAAAAA", node)
        });
        let described = tokio::time::timeout(
            Duration::from_secs(10),
            driver.describe(&auth(&server), &[folder], &space, links.into()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(described[0].target.as_ref().unwrap().path, "file.txt");
        assert_eq!(described[1].target, None);
        served.await.unwrap();
    }

    #[tokio::test]
    async fn revocation_succeeds_when_target_metadata_is_unavailable_including_on_retry() {
        for revoked_at_ms in [0, 900] {
            for node_id in ["file", SPACE] {
                let shared = ShareLink {
                    revoked_at_ms,
                    ..link("AAAAAAAAAAAAAAAAAAAAAA", node_id)
                };
                let mut replies = vec![
                    (
                        "POST /v1/token".into(),
                        200,
                        AccessToken {
                            token: "token".into(),
                            expires_at_ms: i64::MAX,
                            ..AccessToken::default()
                        }
                        .encode_to_vec(),
                    ),
                    (
                        "GET /v1/spaces".into(),
                        200,
                        SpaceList {
                            spaces: vec![SpaceInfo {
                                space_id: SPACE.into(),
                                name: "Team".into(),
                                ..SpaceInfo::default()
                            }],
                            ..SpaceList::default()
                        }
                        .encode_to_vec(),
                    ),
                    (
                        format!("GET /v1/spaces/{SPACE}/links"),
                        200,
                        ShareLinkList {
                            links: vec![shared.clone()],
                            ..ShareLinkList::default()
                        }
                        .encode_to_vec(),
                    ),
                ];
                if revoked_at_ms == 0 {
                    replies.push((
                        format!("DELETE /v1/spaces/{SPACE}/links/{}", shared.link_id),
                        200,
                        Vec::new(),
                    ));
                }
                replies.push((format!("POST /v1/spaces/{SPACE}/rpc"), 403, Vec::new()));
                let (server, served) = serve(replies).await;
                let dir = tempfile::tempdir().unwrap();
                let paths = Paths::under(dir.path());
                crate::Credential {
                    server,
                    user_id: "user".into(),
                    user_name: None,
                    device_id: "device".into(),
                    device_name: "test".into(),
                    refresh_credential: "refresh".into(),
                }
                .save_new(&paths.credential())
                .unwrap();
                let driver = Driver::open(paths).unwrap();
                let revoked = tokio::time::timeout(
                    Duration::from_secs(10),
                    driver.revoke_share_link(&Secret(shared.link_id)),
                )
                .await
                .unwrap()
                .unwrap();
                assert_eq!(revoked.link.state, LinkState::Revoked);
                assert_eq!(revoked.already, revoked_at_ms != 0);
                assert_eq!(revoked.link.target, None);
                assert_eq!(revoked.link.node_id, node_id);
                assert!(revoked.link.revoked_at_ms.unwrap() > 0);
                served.await.unwrap();
            }
        }
    }

    #[tokio::test]
    async fn a_password_outside_8_to_256_characters_is_refused_before_anything_is_sent() {
        let dir = tempfile::tempdir().unwrap();
        let driver = Driver::open(Paths::under(dir.path())).unwrap();
        for password in ["", "seven77", &"x".repeat(257)] {
            let outcome = driver
                .share(dir.path(), Expiry::Default, Some(Secret(password.into())))
                .await;
            assert!(
                matches!(outcome, Err(DriverError::PasswordLength)),
                "{password:?}"
            );
        }
        let outcome = driver
            .share(dir.path(), Expiry::Default, Some(Secret("é".repeat(8))))
            .await;
        assert!(
            matches!(outcome, Err(DriverError::NotPaired)),
            "{outcome:?}"
        );
    }
}
