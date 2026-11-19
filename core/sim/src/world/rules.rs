// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use koidrive_engine::classify::Trees;
use koidrive_engine::names::{name_key, nfc};
use koidrive_engine::tree::{Kind, NodeId, Shape};
use koidrive_proto::v1::{NodeKind, NodeState, NodeStatus, Op, op};

use super::World;
use super::disk::Place;

#[derive(Default)]
pub(super) struct Unsynced {
    pub(super) inos: BTreeSet<u128>,
    pub(super) nodes: BTreeSet<NodeId>,
    pub(super) waiting: BTreeSet<NodeId>,
}

impl Unsynced {
    pub(super) fn holds(&self, trees: &Trees, id: &str) -> bool {
        self.nodes.contains(id)
            || (trees.local().get(id)).is_some_and(|node| self.inos.contains(&node.identity.ino))
    }
}

fn path_in<'a>(
    shape: impl Fn(&str) -> Option<&'a Shape>,
    root: &str,
    id: &str,
) -> Option<Vec<String>> {
    let mut names = Vec::new();
    let mut current = id.to_owned();
    while current != root {
        let above = shape(&current)?;
        names.push(above.name.clone());
        current = above.parent.clone();
    }
    names.reverse();
    Some(names)
}

pub(super) fn excluded_op(trees: &Trees, op: &Op) -> Option<String> {
    let local = |id: &str| trees.local().get(id).map(|node| &node.shape);
    let remote = |id: &str| trees.remote().live(id);
    let root = trees.local().root.as_str();
    let at = |parent: &str, name: &str, kind: Kind| {
        [path_in(local, root, parent), path_in(remote, root, parent)]
            .into_iter()
            .flatten()
            .map(|mut path| {
                path.push(name.to_owned());
                path
            })
            .find(|path| trees.ignores(path, kind))
    };
    let (id, path) = match op.op.as_ref()? {
        op::Op::CreateFile(create) => (
            &create.node_id,
            at(&create.parent_id, &create.name, Kind::File),
        ),
        op::Op::CreateDir(create) => (
            &create.node_id,
            at(&create.parent_id, &create.name, Kind::Dir),
        ),
        op::Op::Move(moved) => {
            let shape = local(&moved.node_id).or_else(|| remote(&moved.node_id));
            let kind = shape.map_or(Kind::File, |shape| shape.kind);
            (
                &moved.node_id,
                at(&moved.new_parent_id, &moved.new_name, kind),
            )
        }
        op::Op::UpdateContent(update) => (
            &update.node_id,
            [local(&update.node_id), remote(&update.node_id)]
                .into_iter()
                .flatten()
                .find_map(|shape| at(&shape.parent, &shape.name, Kind::File)),
        ),
        _ => return None,
    };
    path.map(|path| format!("node {id} at {path:?}"))
}

impl World {
    pub(super) fn unsynced(&self, d: usize, quiet: bool) -> Unsynced {
        let device = &self.devices[d];
        let mut out = Unsynced::default();
        if device.rules.exclude.is_empty() {
            return out;
        }
        let (disk, trees) = (&device.disk, &device.engine.trees);
        let root = self.space.space_id();
        let nodes = self.space.nodes();
        let live: BTreeMap<&str, &NodeState> = (nodes.iter())
            .filter(|node| node.status == NodeStatus::Live as i32 && node.node_id != root)
            .map(|node| (node.node_id.as_str(), node))
            .collect();
        let kind = |node: &NodeState| match node.kind == NodeKind::Dir as i32 {
            true => Kind::Dir,
            false => Kind::File,
        };
        let server_path = |id: &str| {
            let mut names = Vec::new();
            let mut current = id;
            while current != root {
                let node = live.get(current).filter(|_| names.len() <= live.len())?;
                names.push(node.name.clone());
                current = &node.parent_id;
            }
            names.reverse();
            Some(names)
        };
        let normal = |path: &[String]| path.iter().map(|name| nfc(name)).collect::<Vec<_>>();
        let mut on_server = BTreeSet::new();
        for (id, node) in &live {
            let Some(path) = server_path(id) else {
                continue;
            };
            if trees.ignores(&path, kind(node)) {
                out.nodes.insert((*id).to_owned());
            }
            on_server.insert(normal(&path));
        }
        let mut on_disk = BTreeSet::new();
        for (ino, entry) in disk.entries() {
            let Some(path) = disk.path(*ino) else {
                continue;
            };
            let kind = match entry.content {
                Some(_) => Kind::File,
                None => Kind::Dir,
            };
            if trees.ignores(&path, kind) {
                out.inos.insert(*ino);
            }
            on_disk.insert(normal(&path));
        }
        let local = |id: &str| trees.local().get(id).map(|node| &node.shape);
        let kept_in_l = (trees.local().iter())
            .filter(|(id, node)| {
                trees.recorded_local(id)
                    || path_in(local, root, id)
                        .is_some_and(|path| trees.ignores(&path, node.shape.kind))
            })
            .map(|(id, _)| id)
            .chain(trees.displaced_local().keys());
        out.nodes.extend(kept_in_l.cloned());
        let held: Vec<(u128, &NodeId)> = (trees.local().iter())
            .map(|(id, node)| (node.identity.ino, id))
            .collect();
        loop {
            let size = (out.inos.len(), out.nodes.len());
            for (ino, id) in &held {
                if out.inos.contains(ino) && live.contains_key(id.as_str()) {
                    out.nodes.insert((*id).clone());
                }
                if out.nodes.contains(*id) && disk.entries().contains_key(ino) {
                    out.inos.insert(*ino);
                }
            }
            let below: Vec<u128> = (disk.entries().iter())
                .filter(|(_, entry)| out.inos.iter().any(|top| disk.inside(&entry.place, *top)))
                .map(|(ino, _)| *ino)
                .collect();
            out.inos.extend(below);
            let kept: Vec<u128> = (disk.entries().iter())
                .filter(|(ino, entry)| {
                    let folder = Place::In(**ino);
                    let mut children = disk.children(&folder).peekable();
                    quiet
                        && entry.content.is_none()
                        && (disk.path(**ino))
                            .is_some_and(|path| !on_server.contains(&normal(&path)))
                        && children.peek().is_some()
                        && children.all(|child| out.inos.contains(&child))
                })
                .map(|(ino, _)| *ino)
                .collect();
            out.inos.extend(kept);
            let above = |id: &str| {
                std::iter::successors(live.get(id).map(|node| node.parent_id.as_str()), |id| {
                    live.get(id).map(|node| node.parent_id.as_str())
                })
                .take(live.len())
                .any(|id| out.nodes.contains(id))
            };
            let below: Vec<NodeId> = (live.keys())
                .filter(|id| above(id))
                .map(|id| (*id).to_owned())
                .collect();
            out.nodes.extend(below);
            let kept: Vec<NodeId> = (live.iter())
                .filter(|(id, node)| {
                    let mut children = (live.values())
                        .filter(|child| child.parent_id == **id)
                        .peekable();
                    quiet
                        && kind(node) == Kind::Dir
                        && self.intents.deleted(d, id)
                        && server_path(id).is_some_and(|path| !on_disk.contains(&normal(&path)))
                        && children.peek().is_some()
                        && children.all(|child| out.nodes.contains(&child.node_id))
                })
                .map(|(id, _)| (*id).to_owned())
                .collect();
            out.nodes.extend(kept);
            if (out.inos.len(), out.nodes.len()) == size {
                break;
            }
        }
        let keys = |path: &[String]| path.iter().map(|name| name_key(name)).collect::<Vec<_>>();
        let copies_left_out: BTreeMap<Vec<String>, u128> = (held.iter())
            .filter(|(ino, _)| out.inos.contains(ino))
            .filter_map(|(ino, _)| Some((keys(&disk.path(*ino)?), *ino)))
            .collect();
        out.waiting = (live.keys())
            .filter(|id| quiet && !out.nodes.contains(**id))
            .filter(|id| {
                let own = trees.local().get(id).map(|node| node.identity.ino);
                (server_path(id).and_then(|path| copies_left_out.get(&keys(&path))))
                    .is_some_and(|holder| Some(*holder) != own)
            })
            .map(|id| (*id).to_owned())
            .collect();
        out
    }
}
