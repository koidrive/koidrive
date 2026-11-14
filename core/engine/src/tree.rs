// SPDX-License-Identifier: Apache-2.0

//! The three trees of the design (docs/design/sync-engine.md section 2.2): the remote tree R, the
//! local tree L and the synced tree S, all keyed by `node_id` and sharing the root node.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use koidrive_proto::v1::{self as proto, NodeKind, NodeStatus};

use crate::names::{name_key, same_name};

pub type NodeId = String;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Dir,
    File,
}

/// A file's content as the driver describes it (spec v0 section 5.1). The engine never sees bytes.
pub type Content = proto::Content;

/// Two contents are equal for sync purposes when size, manifest digest and executable bit are equal;
/// mtime alone is not a change.
pub fn same_content(a: &Content, b: &Content) -> bool {
    a.size == b.size && a.manifest_digest == b.manifest_digest && a.executable == b.executable
}

/// What every tree records about a node: its position, its kind and, for a file, its content.
#[derive(Clone, Debug, PartialEq)]
pub struct Shape {
    pub parent: NodeId,
    pub name: String,
    pub kind: Kind,
    pub content: Option<Content>,
}

impl Shape {
    /// Equality as the planner compares trees: parents and kinds exactly, names by their NFC form
    /// (L keeps the spelling on disk), contents by [`same_content`]. A rename that changes only
    /// case is a change.
    pub fn same(&self, other: &Shape) -> bool {
        self.same_position(other)
            && self.kind == other.kind
            && match (&self.content, &other.content) {
                (Some(a), Some(b)) => same_content(a, b),
                (None, None) => true,
                _ => false,
            }
    }

    pub fn same_position(&self, other: &Shape) -> bool {
        self.parent == other.parent && same_name(&self.name, &other.name)
    }
}

/// Shape equality where `None` is ⊥, the node being absent from that tree.
pub fn same(a: Option<&Shape>, b: Option<&Shape>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.same(b),
        (None, None) => true,
        _ => false,
    }
}

/// A node of a tree. Only live nodes take part in comparisons and in the tree invariants.
pub trait Node {
    fn shape(&self) -> &Shape;
    fn is_live(&self) -> bool {
        true
    }
    /// False for an entry that is not under its shape's parent on disk: a parked node (section
    /// 2.5). Such an entry holds no name, and a chain of parents ends at it.
    fn is_placed(&self) -> bool {
        true
    }
}

impl Node for Shape {
    fn shape(&self) -> &Shape {
        self
    }
}

/// A node of R: the `NodeState` this device applied (spec v0 section 5.2).
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteNode {
    pub shape: Shape,
    pub trashed: bool,
    pub content_rev: u64,
    pub meta_rev: u64,
    pub meta_edit_rev: u64,
    pub trash_group: String,
    pub version_id: String,
    pub edit_rev: u64,
    pub pending_for: Option<String>,
    pub pending_by: Option<String>,
}

impl Node for RemoteNode {
    fn shape(&self) -> &Shape {
        &self.shape
    }
    fn is_live(&self) -> bool {
        !self.trashed
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateError {
    UnknownKind(i32),
    UnknownStatus(i32),
    MissingContent,
}

impl RemoteNode {
    /// Reads a `NodeState` record. Kinds other than directory and file are refused, since this
    /// version syncs neither symbolic links nor special files.
    pub fn from_state(state: &proto::NodeState) -> Result<(NodeId, RemoteNode), StateError> {
        let kind = match NodeKind::try_from(state.kind) {
            Ok(NodeKind::Dir) => Kind::Dir,
            Ok(NodeKind::File) => Kind::File,
            _ => return Err(StateError::UnknownKind(state.kind)),
        };
        let trashed = match NodeStatus::try_from(state.status) {
            Ok(NodeStatus::Live) => false,
            Ok(NodeStatus::Trashed) => true,
            _ => return Err(StateError::UnknownStatus(state.status)),
        };
        let content = match kind {
            Kind::Dir => None,
            Kind::File => Some(
                state
                    .current
                    .as_ref()
                    .and_then(|version| version.content.clone())
                    .ok_or(StateError::MissingContent)?,
            ),
        };
        let node = RemoteNode {
            shape: Shape {
                parent: state.parent_id.clone(),
                name: state.name.clone(),
                kind,
                content,
            },
            trashed,
            content_rev: state.content_rev,
            meta_rev: state.meta_rev,
            meta_edit_rev: match state.meta_edit_rev {
                0 => state.meta_rev,
                rev => rev,
            },
            trash_group: state.trash_group.clone(),
            version_id: (state.current.as_ref())
                .map(|version| version.version_id.clone())
                .unwrap_or_default(),
            edit_rev: match state.edit_rev {
                0 => state.content_rev,
                edit_rev => edit_rev,
            },
            pending_for: Some(state.pending_for.clone()).filter(|name| !name.is_empty()),
            pending_by: Some(state.pending_by.clone()).filter(|device| !device.is_empty()),
        };
        Ok((state.node_id.clone(), node))
    }
}

/// The identity `core/fs` reports for an entry, which tells a rename from a delete and a create.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    pub dev: u64,
    pub ino: u128,
    /// `None` when the filesystem does not report a birth time, which never equals a known one.
    pub birth_ns: Option<i64>,
}

/// A node of L: the disk as last observed, under its exact on-disk name.
#[derive(Clone, Debug, PartialEq)]
pub struct LocalNode {
    /// For a parked node (section 2.5), its S position, so that parking is not a local change.
    pub shape: Shape,
    pub identity: FileIdentity,
    pub parked: bool,
}

impl Node for LocalNode {
    fn shape(&self) -> &Shape {
        &self.shape
    }
    fn is_placed(&self) -> bool {
        !self.parked
    }
}

/// How sibling names must differ in a tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Names {
    /// R and S: no two live children of a directory share a `name_key` (spec v0 section 6.1).
    ByKey,
    /// L: exact names only, since a case-sensitive file system can hold both `a` and `A`.
    Exact,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeError {
    MissingParent(NodeId),
    ParentNotDir(NodeId),
    Cycle(NodeId),
    NameTaken { parent: NodeId, name: String },
    RootStored,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Tree<N> {
    pub root: NodeId,
    nodes: BTreeMap<NodeId, N>,
}

impl<N: Node> Tree<N> {
    pub fn new(root: impl Into<NodeId>) -> Self {
        Self {
            root: root.into(),
            nodes: BTreeMap::new(),
        }
    }

    pub fn get(&self, id: &str) -> Option<&N> {
        self.nodes.get(id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut N> {
        self.nodes.get_mut(id)
    }

    /// The node's shape if it is live here, ⊥ otherwise.
    pub fn live(&self, id: &str) -> Option<&Shape> {
        self.nodes
            .get(id)
            .filter(|node| node.is_live())
            .map(Node::shape)
    }

    pub fn insert(&mut self, id: NodeId, node: N) -> Option<N> {
        self.nodes.insert(id, node)
    }

    pub fn remove(&mut self, id: &str) -> Option<N> {
        self.nodes.remove(id)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&NodeId, &N)> {
        self.nodes.iter()
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The path of `id` relative to the root, through the parents the tree holds, live or not. `None` when a node on
    /// the way is missing or not placed, or the parents loop.
    pub fn path(&self, id: &str) -> Option<PathBuf> {
        let mut names = Vec::new();
        let mut current = id;
        while current != self.root {
            let node = self.get(current).filter(|node| node.is_placed())?;
            names.push(node.shape().name.as_str());
            current = &node.shape().parent;
            if names.len() > self.len() {
                return None;
            }
        }
        Some(names.into_iter().rev().collect())
    }

    /// Checks the invariants of section 2.2 over live nodes: every placed node's parent is the root
    /// or a live directory, there are no cycles, and sibling names differ as `names` requires.
    pub fn check(&self, names: Names) -> Result<(), TreeError> {
        if self.nodes.contains_key(&self.root) {
            return Err(TreeError::RootStored);
        }
        let mut seen = BTreeSet::new();
        let placed = self
            .nodes
            .iter()
            .filter(|(_, node)| node.is_live() && node.is_placed());
        for (id, node) in placed {
            let shape = node.shape();
            if shape.parent != self.root {
                let parent = self
                    .live(&shape.parent)
                    .ok_or_else(|| TreeError::MissingParent(id.clone()))?;
                if parent.kind != Kind::Dir {
                    return Err(TreeError::ParentNotDir(id.clone()));
                }
            }
            let key = match names {
                Names::ByKey => name_key(&shape.name),
                Names::Exact => shape.name.clone(),
            };
            if !seen.insert((shape.parent.as_str(), key)) {
                return Err(TreeError::NameTaken {
                    parent: shape.parent.clone(),
                    name: shape.name.clone(),
                });
            }
        }
        for id in self.nodes.keys() {
            if self.live(id).is_some() && !self.reaches_root(id) {
                return Err(TreeError::Cycle(id.clone()));
            }
        }
        Ok(())
    }

    /// True when following live parents from `id` reaches the root, or a node that is not placed,
    /// within as many steps as there are nodes.
    pub fn reaches_root(&self, id: &str) -> bool {
        let mut current = id;
        for _ in 0..=self.nodes.len() {
            if current == self.root {
                return true;
            }
            match self.nodes.get(current).filter(|node| node.is_live()) {
                Some(node) if !node.is_placed() => return true,
                Some(node) => current = &node.shape().parent,
                None => return false,
            }
        }
        false
    }
}
