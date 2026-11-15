// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};
use std::ops::RangeInclusive;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use koidrive_engine::classify::Trees;
use koidrive_engine::commit::{Commits, Failure};
use koidrive_engine::ids::IdGen;
use koidrive_engine::tree::{Content, FileIdentity, Kind, LocalNode, RemoteNode, Shape};
use koidrive_proto::v1::{self as proto, ChunkRef, NodeStatus, op, op_result};
use koidrive_sim::space::{Role, Session, SpaceDo};
use sha2::{Digest, Sha256};

const NOW: u64 = 1_794_268_740_000;
const NAMES: [&str; 5] = ["a", "b", "c", "d", "e"];
const DIRS: [&str; 2] = ["n", "m"];
const BURIAL: usize = 9;
const COMMITS: usize = 12;
const CAPS: [usize; 2] = [1, 2];
const EXAMPLES: usize = 3;
const CHUNK: [u8; 32] = [7; 32];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Spot {
    Root,
    In(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Remote {
    Kept,
    Trashed,
    Forgotten,
    Into(Spot),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Local {
    Kept,
    Removed,
    Into(Spot),
    Onto(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unseen {
    Nothing,
    Trash(usize),
    Bury(usize),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct World {
    synced: Vec<Spot>,
    remote: Vec<Remote>,
    local: Vec<Local>,
    created: Vec<(Kind, Local)>,
    ranks: Vec<usize>,
}

struct Bounds {
    existing: RangeInclusive<usize>,
    local_changes: usize,
    onto: usize,
    changes: usize,
    created: &'static [&'static [Kind]],
    nested_created: bool,
    every_order_up_to: usize,
}

const FULL: Bounds = Bounds {
    existing: 2..=4,
    local_changes: 3,
    onto: 2,
    changes: 5,
    created: &[
        &[],
        &[Kind::Dir],
        &[Kind::File],
        &[Kind::Dir, Kind::File],
        &[Kind::Dir, Kind::Dir],
    ],
    nested_created: true,
    every_order_up_to: 4,
};

const THREE_NODES: Bounds = Bounds {
    existing: 2..=3,
    ..FULL
};

const FOUR_NODES: Bounds = Bounds {
    existing: 4..=4,
    local_changes: 2,
    ..FULL
};

const SLICE: Bounds = Bounds {
    existing: 2..=2,
    local_changes: 2,
    onto: 1,
    changes: 3,
    created: &[&[], &[Kind::Dir], &[Kind::File]],
    nested_created: false,
    every_order_up_to: 3,
};

fn orders(bounds: &Bounds, k: usize, len: usize) -> Vec<Vec<usize>> {
    if len <= bounds.every_order_up_to {
        return permutations(len);
    }
    let made = len - k;
    vec![
        (0..len).collect(),
        (0..len)
            .map(|i| if i < k { made + i } else { i - k })
            .collect(),
        (0..len).rev().collect(),
    ]
}

fn node_id(i: usize) -> String {
    format!("01a1c2c0-39a0-7{:03x}-8000-{:012x}", i + 1, i + 1)
}

fn content() -> Content {
    let chunk = ChunkRef {
        hash: CHUNK.to_vec(),
        size: 3,
        encoding: 0,
    };
    let mut listed = CHUNK.to_vec();
    listed.extend(3u32.to_le_bytes());
    Content {
        size: 3,
        chunks: vec![chunk],
        chunk_count: 1,
        manifest_digest: Sha256::digest(&listed).to_vec(),
        ..Default::default()
    }
}

fn shape(world: &World, root: &str, kind: Kind, (spot, name): (Spot, String)) -> Shape {
    Shape {
        parent: world.spot_id(root, spot),
        name,
        kind,
        content: (kind == Kind::File).then(content),
    }
}

fn permutations(n: usize) -> Vec<Vec<usize>> {
    if n == 0 {
        return vec![vec![]];
    }
    let mut all = Vec::new();
    for shorter in permutations(n - 1) {
        for at in 0..n {
            let mut next = shorter.clone();
            next.insert(at, n - 1);
            all.push(next);
        }
    }
    all
}

fn relabel_spot(spot: Spot, to: &[usize]) -> Spot {
    match spot {
        Spot::Root => Spot::Root,
        Spot::In(i) => Spot::In(to.get(i).copied().unwrap_or(i)),
    }
}

fn ordered(synced: &[Spot]) -> bool {
    (synced.iter().enumerate()).all(|(i, spot)| !matches!(spot, Spot::In(j) if *j >= i))
}

fn canonical_order(synced: &[Spot]) -> Vec<usize> {
    (permutations(synced.len()).into_iter())
        .filter_map(|to| {
            let mut moved = vec![Spot::Root; synced.len()];
            for (i, spot) in synced.iter().enumerate() {
                moved[to[i]] = relabel_spot(*spot, &to);
            }
            ordered(&moved).then_some((moved, to))
        })
        .min()
        .map(|(_, to)| to)
        .expect("an ordered labelling exists")
}

fn forests(k: usize) -> Vec<Vec<Spot>> {
    let mut all = vec![vec![]];
    for i in 0..k {
        all = (all.into_iter())
            .flat_map(|prefix: Vec<Spot>| {
                (std::iter::once(Spot::Root).chain((0..i).map(Spot::In))).map(move |spot| {
                    let mut next = prefix.clone();
                    next.push(spot);
                    next
                })
            })
            .collect();
    }
    let mut kept: Vec<Vec<Spot>> = (all.into_iter())
        .filter(|synced| {
            canonical_order(synced)
                .iter()
                .enumerate()
                .all(|(i, to)| i == *to)
        })
        .collect();
    kept.sort();
    kept.dedup();
    kept
}

impl World {
    fn id(&self, i: usize) -> String {
        node_id(self.ranks.get(i).copied().unwrap_or(i))
    }

    fn spot_id(&self, root: &str, spot: Spot) -> String {
        match spot {
            Spot::Root => root.to_string(),
            Spot::In(i) => self.id(i),
        }
    }

    fn existing(&self) -> usize {
        self.synced.len()
    }

    fn len(&self) -> usize {
        self.synced.len() + self.created.len()
    }

    fn kind(&self, i: usize) -> Kind {
        if i < self.existing() {
            Kind::Dir
        } else {
            self.created[i - self.existing()].0
        }
    }

    fn label(&self, i: usize) -> String {
        if i == BURIAL {
            return "burial".into();
        }
        if i < self.existing() {
            return NAMES[i].into();
        }
        let at = i - self.existing();
        match self.created[at].0 {
            Kind::File => "f".into(),
            Kind::Dir => {
                let before = self.created[..at]
                    .iter()
                    .filter(|(kind, _)| *kind == Kind::Dir);
                DIRS[before.count()].into()
            }
        }
    }

    fn local_of(&self, i: usize) -> Local {
        if i < self.existing() {
            self.local[i]
        } else {
            self.created[i - self.existing()].1
        }
    }

    fn synced_place(&self, i: usize) -> (Spot, String) {
        (self.synced[i], self.label(i))
    }

    fn remote_place(&self, i: usize) -> (Spot, String) {
        match self.remote[i] {
            Remote::Into(spot) if spot == self.synced[i] => (spot, format!("{}2", self.label(i))),
            Remote::Into(spot) => (spot, self.label(i)),
            Remote::Kept | Remote::Trashed | Remote::Forgotten => self.synced_place(i),
        }
    }

    fn local_place(&self, i: usize) -> Option<(Spot, String)> {
        match self.local_of(i) {
            Local::Kept => Some(self.synced_place(i)),
            Local::Removed => None,
            Local::Into(spot) if i < self.existing() && spot == self.synced[i] => {
                Some((spot, format!("{}1", self.label(i))))
            }
            Local::Into(spot) => Some((spot, self.label(i))),
            Local::Onto(holder) => Some(self.remote_place(holder)),
        }
    }

    fn remote_live(&self, i: usize) -> bool {
        self.remote_above(i, &[Remote::Trashed, Remote::Forgotten])
    }

    fn recorded(&self, i: usize) -> bool {
        self.remote_above(i, &[Remote::Forgotten])
    }

    fn remote_above(&self, i: usize, gone: &[Remote]) -> bool {
        let mut at = i;
        for _ in 0..=self.existing() {
            if gone.contains(&self.remote[at]) {
                return false;
            }
            match self.remote_place(at).0 {
                Spot::Root => return true,
                Spot::In(up) => at = up,
            }
        }
        false
    }

    fn remote_acyclic(&self) -> bool {
        (0..self.existing()).all(|i| {
            let mut at = i;
            for _ in 0..=self.existing() {
                match self.remote_place(at).0 {
                    Spot::Root => return true,
                    Spot::In(up) => at = up,
                }
            }
            false
        })
    }

    fn changes(&self) -> usize {
        self.local
            .iter()
            .filter(|local| **local != Local::Kept)
            .count()
            + self
                .remote
                .iter()
                .filter(|remote| **remote != Remote::Kept)
                .count()
            + self.created.len()
    }

    fn admits(&self, bounds: &Bounds) -> bool {
        let order = canonical_order(&self.synced);
        order.iter().enumerate().all(|(i, to)| i == *to) && self.fits(bounds)
    }

    fn fits(&self, bounds: &Bounds) -> bool {
        let k = self.existing();
        let len = self.len();
        if !bounds.existing.contains(&k) || self.remote.len() != k || self.local.len() != k {
            return false;
        }
        let kinds = self.created.iter().map(|(kind, _)| *kind);
        if !bounds
            .created
            .iter()
            .any(|config| config.iter().copied().eq(kinds.clone()))
        {
            return false;
        }
        if !ordered(&self.synced) {
            return false;
        }
        if !self.ranks.is_empty() && !orders(bounds, k, len).contains(&self.ranks) {
            return false;
        }
        let nested = bounds.nested_created
            && self.created.len() == 2
            && self.created[0].0 == Kind::Dir
            && self.created[1] == (Kind::Dir, Local::Into(Spot::In(k)));
        if self.changes() > bounds.changes + usize::from(nested) {
            return false;
        }
        if self
            .remote
            .iter()
            .filter(|remote| **remote != Remote::Kept)
            .count()
            > 1
        {
            return false;
        }
        let stray = |i: usize, spot: Spot, bound: usize| matches!(spot, Spot::In(j) if j >= bound || j == i);
        if (self.remote.iter().enumerate())
            .any(|(i, remote)| matches!(remote, Remote::Into(spot) if stray(i, *spot, k)))
        {
            return false;
        }
        if !self.remote_acyclic() {
            return false;
        }
        if self
            .local
            .iter()
            .filter(|local| **local != Local::Kept)
            .count()
            > bounds.local_changes
        {
            return false;
        }
        if self.local.iter().all(|local| *local == Local::Kept) && self.created.is_empty() {
            return false;
        }
        let onto = (0..len).filter(|&i| matches!(self.local_of(i), Local::Onto(_)));
        if onto.count() > bounds.onto {
            return false;
        }
        for i in 0..len {
            let bad = match self.local_of(i) {
                Local::Kept | Local::Removed => i >= k,
                Local::Into(spot) => {
                    stray(i, spot, len) || matches!(spot, Spot::In(j) if self.kind(j) == Kind::File)
                }
                Local::Onto(holder) => holder >= k || holder == i || !self.remote_live(holder),
            };
            if bad {
                return false;
            }
        }
        let places: Vec<Option<(Spot, String)>> = (0..len).map(|i| self.local_place(i)).collect();
        for (i, place) in places.iter().enumerate() {
            let Some((spot, _)) = place else {
                continue;
            };
            let mut at = *spot;
            let mut steps = 0;
            while let Spot::In(up) = at {
                if up == i || steps > len {
                    return false;
                }
                let Some((above, _)) = &places[up] else {
                    return false;
                };
                at = *above;
                steps += 1;
            }
        }
        let mut taken = BTreeSet::new();
        if !places
            .iter()
            .flatten()
            .all(|place| taken.insert(place.clone()))
        {
            return false;
        }
        let referenced = |i: usize| {
            (0..k).any(|j| self.synced[j] == Spot::In(i) || self.remote_place(j).0 == Spot::In(i))
                || places
                    .iter()
                    .flatten()
                    .any(|(spot, _)| *spot == Spot::In(i))
                || (0..len).any(|j| self.local_of(j) == Local::Onto(i))
        };
        (0..k).all(|i| {
            self.local[i] != Local::Kept || self.remote[i] != Remote::Kept || referenced(i)
        })
    }

    fn canonical(&self) -> World {
        let order = canonical_order(&self.synced);
        let k = self.existing();
        let to: Vec<usize> = (0..self.len())
            .map(|i| if i < k { order[i] } else { i })
            .collect();
        let spot = |spot: Spot| relabel_spot(spot, &to);
        let local = |local: Local| match local {
            Local::Into(at) => Local::Into(spot(at)),
            Local::Onto(holder) => Local::Onto(to[holder]),
            other => other,
        };
        let mut world = World {
            synced: vec![Spot::Root; k],
            remote: vec![Remote::Kept; k],
            local: vec![Local::Kept; k],
            created: (self.created.iter())
                .map(|(kind, at)| (*kind, local(*at)))
                .collect(),
            ranks: self.ranks.clone(),
        };
        for i in 0..k {
            world.synced[to[i]] = spot(self.synced[i]);
            world.remote[to[i]] = match self.remote[i] {
                Remote::Into(at) => Remote::Into(spot(at)),
                other => other,
            };
            world.local[to[i]] = local(self.local[i]);
            if let Some(&rank) = self.ranks.get(i) {
                world.ranks[to[i]] = rank;
            }
        }
        world
    }

    fn unseen(&self) -> Vec<Unseen> {
        let live: Vec<usize> = (0..self.existing())
            .filter(|&i| self.remote_live(i))
            .collect();
        (std::iter::once(Unseen::Nothing))
            .chain(live.iter().map(|&i| Unseen::Trash(i)))
            .chain(live.iter().map(|&i| Unseen::Bury(i)))
            .collect()
    }

    fn label_of(&self, root: &str, id: &str) -> String {
        if id == root {
            return "root".into();
        }
        (0..self.len())
            .chain([BURIAL])
            .find(|&i| {
                if i == BURIAL {
                    node_id(i) == id
                } else {
                    self.id(i) == id
                }
            })
            .map(|i| self.label(i))
            .unwrap_or_else(|| id.to_string())
    }

    fn place(&self, (spot, name): &(Spot, String)) -> String {
        match spot {
            Spot::Root => format!("root/{name}"),
            Spot::In(i) => format!("{}/{name}", self.label(*i)),
        }
    }

    fn describe(&self) -> String {
        let mut rows = Vec::new();
        for i in 0..self.existing() {
            let remote = match self.remote[i] {
                Remote::Trashed => "trashed".to_string(),
                Remote::Forgotten => "none".to_string(),
                _ => self.place(&self.remote_place(i)),
            };
            let local = self
                .local_place(i)
                .map_or("removed".to_string(), |place| self.place(&place));
            rows.push(format!(
                "{} dir: S {} R {} L {}",
                self.label(i),
                self.place(&self.synced_place(i)),
                remote,
                local
            ));
        }
        for i in self.existing()..self.len() {
            let kind = if self.kind(i) == Kind::File {
                "file"
            } else {
                "dir"
            };
            let place = self.local_place(i).expect("created nodes are in L");
            rows.push(format!(
                "{} new {kind}: L {}",
                self.label(i),
                self.place(&place)
            ));
        }
        rows.join("; ")
    }

    fn show_ranks(&self) -> String {
        if self.ranks.is_empty() {
            return String::new();
        }
        let mut by_rank: Vec<usize> = (0..self.len()).collect();
        by_rank.sort_by_key(|&i| self.ranks[i]);
        let labels: Vec<String> = by_rank.into_iter().map(|i| self.label(i)).collect();
        format!("; ids in order {}", labels.join(" < "))
    }

    fn show_unseen(&self, unseen: Unseen) -> String {
        match unseen {
            Unseen::Nothing => "nothing unseen".into(),
            Unseen::Trash(i) => format!("unseen trash of {}", self.label(i)),
            Unseen::Bury(i) => format!("unseen burial of {}", self.label(i)),
        }
    }

    fn show_op(&self, root: &str, op: &proto::Op) -> String {
        let at = |id: &str| self.label_of(root, id);
        let pending = |name: &str| {
            if name.is_empty() {
                String::new()
            } else {
                format!(" for {name}")
            }
        };
        match op.op.as_ref().expect("ops are set") {
            op::Op::CreateDir(o) => format!(
                "CreateDir {}→{}/{}{}",
                at(&o.node_id),
                at(&o.parent_id),
                o.name,
                pending(&o.pending_for)
            ),
            op::Op::CreateFile(o) => format!(
                "CreateFile {}→{}/{}",
                at(&o.node_id),
                at(&o.parent_id),
                o.name
            ),
            op::Op::UpdateContent(o) => format!("UpdateContent {}", at(&o.node_id)),
            op::Op::Move(o) => format!(
                "Move {}→{}/{}{}",
                at(&o.node_id),
                at(&o.new_parent_id),
                o.new_name,
                pending(&o.pending_for)
            ),
            op::Op::Delete(o) => format!("Delete {}", at(&o.node_id)),
            op::Op::Restore(o) => format!(
                "Restore {}→{}/{}",
                at(&o.node_id),
                at(&o.target_parent_id),
                o.name
            ),
        }
    }
}

fn session(device: &str) -> Session {
    Session {
        device_id: device.into(),
        user_id: "user".into(),
        device_name: device.into(),
        role: Role::Owner,
        max_format: 1,
    }
}

fn wrap(op: op::Op) -> proto::Op {
    proto::Op { op: Some(op) }
}

fn mkdir(id: String, parent: String, name: String) -> proto::Op {
    wrap(op::Op::CreateDir(proto::CreateDir {
        node_id: id,
        parent_id: parent,
        name,
        pending_for: String::new(),
    }))
}

fn mv(id: String, parent: String, name: String) -> proto::Op {
    wrap(op::Op::Move(proto::Move {
        node_id: id,
        new_parent_id: parent,
        new_name: name,
        from: None,
        pending_for: String::new(),
    }))
}

fn delete(id: String) -> proto::Op {
    wrap(op::Op::Delete(proto::Delete {
        node_id: id,
        requires: None,
    }))
}

fn other(space: &mut SpaceDo, ids: &mut IdGen, ops: Vec<proto::Op>) {
    let commit = proto::Commit {
        commit_id: ids.uuid_v7(NOW),
        history_id: space.history_id().into(),
        device_seq: space.device_seq("other") + 1,
        base_seq: space.head_seq(),
        ops,
        ..Default::default()
    };
    let done = space
        .commit(&session("other"), NOW as i64, &commit)
        .unwrap();
    let rejected = op_result::Code::Rejected as i32;
    assert!(
        done.result.results.iter().all(|r| r.code != rejected),
        "{done:?}"
    );
}

fn server(world: &World) -> SpaceDo {
    let mut space = SpaceDo::new(11, NOW as i64);
    let root = space.space_id().to_string();
    let mut ids = IdGen::new(13);
    let made = (0..world.existing())
        .filter(|&i| world.recorded(i))
        .map(|i| {
            mkdir(
                world.id(i),
                world.spot_id(&root, world.synced[i]),
                world.label(i),
            )
        })
        .collect::<Vec<_>>();
    if !made.is_empty() {
        other(&mut space, &mut ids, made);
    }
    for i in 0..world.existing() {
        match world.remote[i] {
            Remote::Kept | Remote::Forgotten => {}
            Remote::Trashed => other(&mut space, &mut ids, vec![delete(world.id(i))]),
            Remote::Into(_) => {
                let (spot, name) = world.remote_place(i);
                other(
                    &mut space,
                    &mut ids,
                    vec![mv(world.id(i), world.spot_id(&root, spot), name)],
                );
            }
        }
    }
    let chunks = content().chunks;
    let upload = proto::PrepareUpload { chunks };
    space
        .prepare_upload(&session("me"), NOW as i64, &upload)
        .unwrap();
    space.put_chunk(&CHUNK, 3, NOW as i64);
    space
}

fn unsee(world: &World, space: &mut SpaceDo, unseen: Unseen) {
    let root = space.space_id().to_string();
    let mut ids = IdGen::new(14);
    match unseen {
        Unseen::Nothing => {}
        Unseen::Trash(i) => other(space, &mut ids, vec![delete(world.id(i))]),
        Unseen::Bury(i) => {
            let name = world.remote_place(i).1;
            let ops = vec![
                mkdir(node_id(BURIAL), root, "burial".into()),
                mv(world.id(i), node_id(BURIAL), name),
                delete(node_id(BURIAL)),
            ];
            other(space, &mut ids, ops);
        }
    }
}

fn learn(trees: &mut Trees, space: &SpaceDo) {
    let root = space.space_id();
    for node in space
        .nodes()
        .into_iter()
        .filter(|node| node.node_id != root)
    {
        let (id, held) = RemoteNode::from_state(&node).unwrap();
        trees.set_remote(id, held);
    }
}

fn settle(planner: &mut Planner, commit_id: &str, result: &proto::CommitResult, space: &SpaceDo) {
    let mut made = BTreeSet::new();
    for change in &result.changes {
        let Some(proto::change_record::Change::Node(state)) = &change.change else {
            continue;
        };
        if state.node_id == space.space_id() {
            continue;
        }
        let (id, node) = RemoteNode::from_state(state).unwrap();
        made.insert(id.clone());
        if planner.commits.answers_in_flight(commit_id, &id) {
            planner.trees.set_remote_answered_here(id, node);
        } else if planner.commits.edits_in_flight(commit_id, &id) {
            planner.trees.set_remote_edited_here(id, node);
        } else {
            planner.trees.set_remote_made_here(id, node);
        }
    }
    for node in space
        .nodes()
        .into_iter()
        .filter(|node| node.node_id != space.space_id() && !made.contains(&node.node_id))
    {
        let (id, held) = RemoteNode::from_state(&node).unwrap();
        planner.trees.set_remote(id, held);
    }
}

fn trees(world: &World, space: &SpaceDo) -> Trees {
    let root = space.space_id().to_string();
    let mut trees = Trees::new(&root);
    for i in 0..world.len() {
        if let Some(place) = world.local_place(i) {
            let identity = FileIdentity {
                ino: i as u128 + 1,
                birth_ns: Some(0),
            };
            let node = LocalNode {
                shape: shape(world, &root, world.kind(i), place),
                identity,
                parked: false,
                left_out_when_placed: false,
                content_from_server: false,
            };
            trees.set_local(world.id(i), node);
        }
        let synced =
            (i < world.existing()).then(|| shape(world, &root, Kind::Dir, world.synced_place(i)));
        trees.set_synced(world.id(i), synced);
    }
    learn(&mut trees, space);
    trees.settle();
    trees
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Verdict {
    rejected: Vec<usize>,
    revived: Vec<String>,
    made_up: Vec<usize>,
    made_up_by_delete: Vec<usize>,
    lost: Vec<String>,
    refused: bool,
}

impl Verdict {
    fn safe(&self) -> bool {
        self.revived.is_empty() && self.made_up.is_empty() && self.lost.is_empty()
    }

    fn clean(&self) -> bool {
        self.safe()
            && self.rejected.is_empty()
            && self.made_up_by_delete.is_empty()
            && !self.refused
    }
}

fn target(op: &proto::Op) -> &str {
    match op.op.as_ref().expect("ops are set") {
        op::Op::CreateDir(o) => &o.node_id,
        op::Op::CreateFile(o) => &o.node_id,
        op::Op::UpdateContent(o) => &o.node_id,
        op::Op::Move(o) => &o.node_id,
        op::Op::Delete(o) => &o.node_id,
        op::Op::Restore(o) => &o.node_id,
    }
}

fn wanted(op: &proto::Op) -> Option<(&str, &str)> {
    match op.op.as_ref().expect("ops are set") {
        op::Op::CreateDir(o) => Some((&o.parent_id, &o.name)),
        op::Op::CreateFile(o) => Some((&o.parent_id, &o.name)),
        op::Op::Move(o) => Some((&o.new_parent_id, &o.new_name)),
        op::Op::Restore(o) if !o.name.is_empty() => Some((&o.target_parent_id, &o.name)),
        _ => None,
    }
}

#[derive(PartialEq)]
enum Leave {
    Stays,
    Moves,
    Deletes,
}

fn leaves(world: &World, root: &str, space: &SpaceDo, holder: &str) -> Leave {
    let Some(i) = (0..world.len()).find(|&i| world.id(i) == holder) else {
        return Leave::Stays;
    };
    match world.local_of(i) {
        Local::Removed => return Leave::Deletes,
        Local::Kept => return Leave::Stays,
        _ if i < world.existing() => return Leave::Moves,
        _ => {}
    }
    let node = space.node(holder).expect("the holder exists");
    let place = world
        .local_place(i)
        .map(|(spot, name)| (world.spot_id(root, spot), name));
    if place == Some((node.parent_id, node.name)) {
        Leave::Stays
    } else {
        Leave::Moves
    }
}

fn apply(space: &mut SpaceDo, commit: &proto::Commit) -> Option<proto::CommitResult> {
    match space.commit(&session("me"), NOW as i64, commit) {
        Ok(done) => Some(done.result),
        Err(error) if error.code == proto::error::Code::HeadMoved as i32 => None,
        Err(error) => panic!("{error:?}"),
    }
}

fn trashed(space: &SpaceDo) -> BTreeSet<String> {
    (space.nodes().into_iter())
        .filter(|node| node.status() == NodeStatus::Trashed)
        .map(|node| node.node_id)
        .collect()
}

fn judge(
    world: &World,
    before: &SpaceDo,
    commit: &proto::Commit,
) -> (SpaceDo, proto::CommitResult, Verdict) {
    let root = before.space_id().to_string();
    let mut after = before.clone();
    let mut verdict = Verdict::default();
    let Some(result) = apply(&mut after, commit) else {
        verdict.refused = true;
        return (after, proto::CommitResult::default(), verdict);
    };
    for (at, (op, done)) in commit.ops.iter().zip(&result.results).enumerate() {
        if done.code == op_result::Code::Rejected as i32 {
            verdict.rejected.push(at);
        }
        if done.code != op_result::Code::AppliedRenamed as i32 {
            continue;
        }
        let Some((parent, name)) = wanted(op) else {
            continue;
        };
        let mut prefix = before.clone();
        if at > 0 {
            let mut head = commit.clone();
            head.ops.truncate(at);
            apply(&mut prefix, &head);
        }
        let holder = (prefix.nodes().into_iter()).find(|node| {
            node.parent_id == parent
                && node.name == name
                && node.node_id != target(op)
                && node.status() == NodeStatus::Live
        });
        match holder.map(|holder| leaves(world, &root, &prefix, &holder.node_id)) {
            Some(Leave::Moves) => verdict.made_up.push(at),
            Some(Leave::Deletes) => verdict.made_up_by_delete.push(at),
            _ => {}
        }
    }
    let targets: BTreeSet<&str> = commit.ops.iter().map(target).collect();
    let path = |id: &str| {
        let mut path = Vec::new();
        let mut at = after.node(id);
        while let Some(node) = at {
            at = (node.parent_id != root)
                .then(|| after.node(&node.parent_id))
                .flatten();
            path.push(node.node_id);
        }
        path
    };
    let holding: BTreeSet<String> = targets.iter().flat_map(|id| path(id)).collect();
    for id in trashed(before) {
        if after
            .node(&id)
            .is_none_or(|node| node.status() != NodeStatus::Live)
        {
            continue;
        }
        let restored = path(&id).iter().any(|up| targets.contains(up.as_str()));
        if !restored && !holding.contains(&id) {
            verdict.revived.push(world.label_of(&root, &id));
        }
    }
    let kept: BTreeSet<String> = (0..world.len())
        .filter(|&i| world.local_place(i).is_some())
        .map(|i| world.id(i))
        .collect();
    for id in trashed(&after) {
        let was_live = before
            .node(&id)
            .is_some_and(|node| node.status() == NodeStatus::Live);
        if was_live && kept.contains(&id) {
            verdict.lost.push(world.label_of(&root, &id));
        }
    }
    (after, result, verdict)
}

struct Sent {
    before: SpaceDo,
    commit: proto::Commit,
    result: proto::CommitResult,
    verdict: Verdict,
}

#[derive(Clone, Copy, PartialEq)]
enum Stop {
    Quiet,
    Busy,
    Stalled,
    Refused,
}

struct Run {
    unseen: Unseen,
    before: SpaceDo,
    sent: Vec<Sent>,
    end: SpaceDo,
    stop: Stop,
}

#[derive(Clone)]
struct Planner {
    trees: Trees,
    commits: Commits,
    ids: IdGen,
    history: String,
    base: u64,
}

impl Planner {
    fn new(world: &World, space: &SpaceDo) -> Self {
        Planner {
            trees: trees(world, space),
            commits: Commits::default(),
            ids: IdGen::new(12),
            history: space.history_id().to_string(),
            base: space.head_seq(),
        }
    }

    fn build(&mut self) -> Option<proto::Commit> {
        (0..2).find_map(|_| {
            (self.commits)
                .build(
                    &mut self.trees,
                    &mut self.ids,
                    NOW,
                    &self.history,
                    "me",
                    self.base,
                )
                .cloned()
        })
    }
}

fn relevant(world: &World, space: &SpaceDo, first: Option<&proto::Commit>, unseen: Unseen) -> bool {
    let hidden = match unseen {
        Unseen::Nothing => return true,
        Unseen::Trash(i) | Unseen::Bury(i) => world.id(i),
    };
    let Some(first) = first else {
        return false;
    };
    let nodes = space.nodes();
    let mut touched: BTreeSet<&str> = BTreeSet::new();
    for op in &first.ops {
        touched.insert(target(op));
        if let Some((parent, name)) = wanted(op) {
            touched.insert(parent);
            let holder = nodes
                .iter()
                .find(|node| node.parent_id == parent && node.name == name);
            touched.extend(holder.map(|node| node.node_id.as_str()));
        }
    }
    touched.into_iter().any(|id| {
        let mut at = space.node(id);
        while let Some(node) = at {
            if node.node_id == hidden {
                return true;
            }
            at = space.node(&node.parent_id);
        }
        false
    })
}

fn run(
    world: &World,
    space: &SpaceDo,
    planner: &Planner,
    first: Option<&proto::Commit>,
    unseen: Unseen,
    fits: usize,
) -> Run {
    let mut server = space.clone();
    unsee(world, &mut server, unseen);
    let before = server.clone();
    let mut planner = planner.clone();
    let mut next = first.cloned();
    let mut sent = Vec::new();
    let mut splits = 0;
    let stop = loop {
        let Some(mut commit) = next.take() else {
            break Stop::Quiet;
        };
        if sent.len() == COMMITS || splits == COMMITS {
            break Stop::Busy;
        }
        if commit.ops.len() > fits {
            splits += 1;
            let error = proto::Error {
                code: proto::error::Code::LimitExceeded as i32,
                ..Default::default()
            };
            if planner.commits.failed(&mut planner.trees, &error) != Ok(Failure::Split) {
                break Stop::Stalled;
            }
            planner.trees.settle();
            next = planner.build();
            continue;
        }
        commit.device_seq = server.device_seq("me") + 1;
        let (after, result, verdict) = judge(world, &server, &commit);
        if verdict.refused {
            sent.push(Sent {
                before: server.clone(),
                commit,
                result,
                verdict,
            });
            break Stop::Refused;
        }
        let at = std::mem::replace(&mut server, after);
        planner.commits.received(result.clone()).unwrap();
        settle(&mut planner, &commit.commit_id, &result, &server);
        let acknowledged = planner
            .commits
            .acknowledge(&mut planner.trees, server.head_seq());
        planner.base = server.head_seq();
        planner.trees.settle();
        sent.push(Sent {
            before: at,
            commit,
            result,
            verdict,
        });
        if !acknowledged {
            break Stop::Stalled;
        }
        next = planner.build();
    };
    Run {
        unseen,
        before,
        sent,
        end: server,
        stop,
    }
}

fn stand_in(op: &proto::Op) -> Option<proto::Op> {
    let pending = |name: &str| format!("{name} (pending)");
    match op.op.as_ref()? {
        op::Op::Move(o) if o.pending_for.is_empty() => Some(wrap(op::Op::Move(proto::Move {
            new_name: pending(&o.new_name),
            pending_for: o.new_name.clone(),
            ..o.clone()
        }))),
        op::Op::CreateDir(o) if o.pending_for.is_empty() => {
            Some(wrap(op::Op::CreateDir(proto::CreateDir {
                name: pending(&o.name),
                pending_for: o.name.clone(),
                ..o.clone()
            })))
        }
        _ => None,
    }
}

fn after_stand_in(op: &proto::Op) -> proto::Op {
    match op.op.as_ref() {
        Some(op::Op::CreateDir(o)) => mv(o.node_id.clone(), o.parent_id.clone(), o.name.clone()),
        _ => op.clone(),
    }
}

struct Search<'a> {
    world: &'a World,
    befores: Vec<&'a SpaceDo>,
    template: proto::Commit,
    ops: Vec<proto::Op>,
    stand_ins: Vec<(usize, proto::Op)>,
    limit: usize,
    longest: Vec<usize>,
}

impl Search<'_> {
    fn originals(&self, prefix: &[usize]) -> usize {
        prefix.iter().filter(|&&i| i < self.ops.len()).count()
    }

    fn op(&self, prefix: &[usize], at: usize) -> proto::Op {
        let i = prefix[at];
        if i >= self.ops.len() {
            return self.stand_ins[i - self.ops.len()].1.clone();
        }
        let stood = (prefix[..at].iter())
            .any(|&j| j >= self.ops.len() && self.stand_ins[j - self.ops.len()].0 == i);
        if stood {
            after_stand_in(&self.ops[i])
        } else {
            self.ops[i].clone()
        }
    }

    fn dfs(&mut self, prefix: &mut Vec<usize>) -> bool {
        for next in 0..self.ops.len() + self.stand_ins.len() {
            if prefix.contains(&next) {
                continue;
            }
            if next >= self.ops.len() {
                let of = self.stand_ins[next - self.ops.len()].0;
                if prefix.contains(&of) || prefix.iter().any(|&j| j >= self.ops.len()) {
                    continue;
                }
            }
            prefix.push(next);
            let mut commit = self.template.clone();
            commit.ops = (0..prefix.len()).map(|at| self.op(prefix, at)).collect();
            let mut viable = true;
            let mut safe = true;
            for before in &self.befores {
                let verdict = judge(self.world, before, &commit).2;
                if !verdict.made_up.is_empty() || !verdict.rejected.is_empty() || verdict.refused {
                    viable = false;
                    break;
                }
                safe &= verdict.safe();
            }
            if viable {
                let originals = self.originals(prefix);
                if safe && originals > self.originals(&self.longest) {
                    self.longest = prefix.clone();
                    if originals == self.ops.len() {
                        return true;
                    }
                }
                if originals < self.limit && self.dfs(prefix) {
                    return true;
                }
            }
            prefix.pop();
        }
        false
    }

    fn longest(
        world: &World,
        befores: Vec<&SpaceDo>,
        template: &proto::Commit,
        ops: &[proto::Op],
        limit: usize,
    ) -> (usize, Vec<proto::Op>) {
        let stand_ins = (ops.iter().enumerate())
            .filter_map(|(i, op)| stand_in(op).map(|op| (i, op)))
            .collect();
        let mut search = Search {
            world,
            befores,
            template: template.clone(),
            ops: ops.to_vec(),
            stand_ins,
            limit,
            longest: Vec::new(),
        };
        search.dfs(&mut Vec::new());
        let longest = search.longest.clone();
        let placed = search.originals(&longest);
        (
            placed,
            (0..longest.len())
                .map(|at| search.op(&longest, at))
                .collect(),
        )
    }
}

fn naive(world: &World, root: &str) -> Vec<proto::Op> {
    let mut ops = Vec::new();
    for i in 0..world.len() {
        if i < world.existing() && world.remote[i] != Remote::Kept {
            continue;
        }
        let id = world.id(i);
        match (world.local_of(i), world.local_place(i)) {
            (Local::Kept, _) => {}
            (Local::Removed, _) | (_, None) => ops.push(delete(id)),
            (_, Some((spot, name))) if i < world.existing() => {
                ops.push(mv(id, world.spot_id(root, spot), name))
            }
            (_, Some((spot, name))) if world.kind(i) == Kind::Dir => {
                ops.push(mkdir(id, world.spot_id(root, spot), name))
            }
            (_, Some((spot, name))) => ops.push(wrap(op::Op::CreateFile(proto::CreateFile {
                node_id: id,
                parent_id: world.spot_id(root, spot),
                name,
                content: Some(content()),
            }))),
        }
    }
    ops
}

fn goals(world: &World) -> u64 {
    (0..world.len())
        .filter(|&i| {
            world.local_of(i) != Local::Kept
                && (i >= world.existing() || world.remote[i] == Remote::Kept)
        })
        .fold(0, |goals, i| goals | 1 << i)
}

fn standing(world: &World, root: &str, space: &SpaceDo, i: usize) -> Option<proto::NodeState> {
    if let Some(node) = space.node(&world.id(i)) {
        return Some(node);
    }
    let (spot, name) = world
        .local_place(i)
        .filter(|_| i >= world.existing() && world.kind(i) == Kind::Dir)?;
    let parent = match spot {
        Spot::Root => root.to_string(),
        Spot::In(j) => standing(world, root, space, j)?.node_id,
    };
    (space.nodes().into_iter()).find(|node| {
        node.parent_id == parent && node.name == name && node.status() == NodeStatus::Live
    })
}

fn reached(world: &World, root: &str, space: &SpaceDo, goals: u64) -> u64 {
    let at = |i: usize| match world.local_place(i) {
        None => space
            .node(&world.id(i))
            .is_none_or(|node| node.status() != NodeStatus::Live),
        Some((spot, name)) => {
            let parent = match spot {
                Spot::Root => Some(root.to_string()),
                Spot::In(j) => standing(world, root, space, j).map(|node| node.node_id),
            };
            standing(world, root, space, i).is_some_and(|node| {
                node.status() == NodeStatus::Live
                    && Some(&node.parent_id) == parent.as_ref()
                    && node.name == name
            })
        }
    };
    (0..world.len())
        .filter(|&i| goals & 1 << i != 0 && at(i))
        .fold(0, |done, i| done | 1 << i)
}

type Fingerprint = Vec<(String, String, String, i32)>;

fn fingerprint(space: &SpaceDo) -> Fingerprint {
    let mut nodes: Vec<_> = (space.nodes().into_iter())
        .map(|node| (node.node_id, node.parent_id, node.name, node.status))
        .collect();
    nodes.sort();
    nodes
}

const REACH_VISITS: usize = 20_000;
const REACH_STAND_INS: usize = 2;
const REACH_STARTS: usize = 256;

struct Reach<'a> {
    world: &'a World,
    root: String,
    befores: Vec<&'a SpaceDo>,
    template: proto::Commit,
    ops: Vec<proto::Op>,
    stand_ins: Vec<(usize, proto::Op)>,
    goals: u64,
    seen: BTreeSet<(u64, Vec<Fingerprint>)>,
    visits: usize,
    reached: BTreeSet<u64>,
    leaves: Vec<Vec<SpaceDo>>,
}

impl Reach<'_> {
    fn op(&self, prefix: &[usize], at: usize) -> proto::Op {
        let i = prefix[at];
        if i >= self.ops.len() {
            return self.stand_ins[i - self.ops.len()].1.clone();
        }
        let stood = (prefix[..at].iter())
            .any(|&j| j >= self.ops.len() && self.stand_ins[j - self.ops.len()].0 == i);
        if stood {
            after_stand_in(&self.ops[i])
        } else {
            self.ops[i].clone()
        }
    }

    fn dfs(&mut self, prefix: &mut Vec<usize>, used: u64) -> bool {
        let mut extended = false;
        for next in 0..self.ops.len() + self.stand_ins.len() {
            if used & 1 << next != 0
                || self.visits >= REACH_VISITS
                || self.reached.contains(&self.goals)
            {
                continue;
            }
            if next >= self.ops.len() {
                let of = self.stand_ins[next - self.ops.len()].0;
                let stood = prefix.iter().filter(|&&j| j >= self.ops.len()).count();
                if used & 1 << of != 0 || stood >= REACH_STAND_INS {
                    continue;
                }
            }
            self.visits += 1;
            prefix.push(next);
            let mut commit = self.template.clone();
            commit.ops = (0..prefix.len()).map(|at| self.op(prefix, at)).collect();
            let mut afters = Vec::new();
            let mut viable = true;
            let mut safe = true;
            for before in &self.befores {
                let (after, _, verdict) = judge(self.world, before, &commit);
                if !verdict.made_up.is_empty() || !verdict.rejected.is_empty() || verdict.refused {
                    viable = false;
                    break;
                }
                safe &= verdict.safe();
                afters.push(after);
            }
            if viable
                && self
                    .seen
                    .insert((used | 1 << next, afters.iter().map(fingerprint).collect()))
            {
                if safe {
                    self.reached
                        .insert(reached(self.world, &self.root, &afters[0], self.goals));
                }
                let longer = self.dfs(prefix, used | 1 << next);
                if safe {
                    extended = true;
                    if !longer {
                        self.leaves.push(afters);
                    }
                }
            }
            prefix.pop();
        }
        extended
    }

    fn reachable(
        world: &World,
        befores: Vec<&SpaceDo>,
    ) -> (BTreeSet<u64>, bool, Vec<Vec<SpaceDo>>) {
        let start = befores[0];
        let root = start.space_id().to_string();
        let ops = naive(world, &root);
        let stand_ins = (ops.iter().enumerate())
            .filter_map(|(i, op)| stand_in(op).map(|op| (i, op)))
            .collect();
        let template = proto::Commit {
            commit_id: IdGen::new(15 + start.device_seq("me")).uuid_v7(NOW),
            history_id: start.history_id().into(),
            device_seq: start.device_seq("me") + 1,
            base_seq: start.head_seq(),
            ..Default::default()
        };
        let mut reach = Reach {
            world,
            goals: goals(world),
            reached: BTreeSet::from([reached(world, &root, start, goals(world))]),
            root,
            befores,
            template,
            ops,
            stand_ins,
            seen: BTreeSet::new(),
            visits: 0,
            leaves: Vec::new(),
        };
        reach.dfs(&mut Vec::new(), 0);
        (reach.reached, reach.visits >= REACH_VISITS, reach.leaves)
    }

    fn over_commits(world: &World, befores: Vec<&SpaceDo>) -> (BTreeSet<u64>, bool) {
        let (mut reached, mut capped, mut leaves) = Reach::reachable(world, befores);
        let mut started = BTreeSet::new();
        while let Some(state) = leaves.pop() {
            if !started.insert(state.iter().map(fingerprint).collect::<Vec<_>>()) {
                continue;
            }
            if started.len() > REACH_STARTS {
                capped = true;
                break;
            }
            let (more, more_capped, more_leaves) = Reach::reachable(world, state.iter().collect());
            reached.extend(more);
            capped |= more_capped;
            leaves.extend(more_leaves);
        }
        (reached, capped)
    }
}

struct Report {
    runs: usize,
    hard: bool,
    classes: Vec<String>,
    size: usize,
    text: String,
}

fn show_runs(world: &World, runs: &[Run]) -> String {
    let mut text = String::new();
    for run in runs {
        let root = run.before.space_id();
        let commits: Vec<String> = (run.sent.iter())
            .map(|sent| {
                let ops: Vec<String> = (sent.commit.ops.iter().zip(&sent.result.results))
                    .map(|(op, done)| {
                        let shown = world.show_op(root, op);
                        match op_result::Code::try_from(done.code) {
                            Ok(op_result::Code::Applied) => shown,
                            Ok(op_result::Code::AppliedRenamed) => {
                                format!("{shown} [renamed {}]", done.name)
                            }
                            Ok(code) => format!("{shown} [{code:?} {}]", done.reject_reason),
                            Err(_) => format!("{shown} [code {}]", done.code),
                        }
                    })
                    .collect();
                let mut line = format!("[{}]", ops.join(", "));
                if !sent.verdict.clean() {
                    line += &format!(" {:?}", sent.verdict);
                }
                line
            })
            .collect();
        text += &format!(
            "\n    {}: {}{}",
            world.show_unseen(run.unseen),
            if commits.is_empty() {
                "nothing sent".into()
            } else {
                commits.join(" then ")
            },
            match run.stop {
                Stop::Quiet | Stop::Refused => "",
                Stop::Busy => " (still sending)",
                Stop::Stalled => " (acknowledge refused)",
            }
        );
    }
    text
}

fn kinds(verdicts: &[&Verdict]) -> Vec<&'static str> {
    let mut kinds = Vec::new();
    if verdicts.iter().any(|verdict| !verdict.revived.is_empty()) {
        kinds.push("revival");
    }
    if verdicts.iter().any(|verdict| !verdict.made_up.is_empty()) {
        kinds.push("made-up name");
    }
    if verdicts.iter().any(|verdict| !verdict.lost.is_empty()) {
        kinds.push("lost");
    }
    kinds
}

fn resolve(
    world: &World,
    befores: Vec<&SpaceDo>,
    commit: &proto::Commit,
    root: &str,
) -> (&'static str, String) {
    let full = commit.ops.len();
    let (placed, common) = Search::longest(world, befores.clone(), commit, &commit.ops, full);
    let shown: Vec<String> = common.iter().map(|op| world.show_op(root, op)).collect();
    let shown = shown.join(", ");
    if placed == full {
        let resolution = if common.len() > full {
            "fixed by reordering with a stand-in name"
        } else {
            "fixed by reordering"
        };
        return (
            resolution,
            format!("\n    safe in every variant: [{shown}]"),
        );
    }
    let each = befores.len() > 1
        && (befores.iter()).all(|before| {
            Search::longest(world, vec![before], commit, &commit.ops, full).0 == full
        });
    if each {
        return (
            "needs unseen state",
            format!("\n    longest safe in every variant: [{shown}]"),
        );
    }
    if placed == 0 {
        return ("only an empty commit is safe", String::new());
    }
    (
        "fixed only by deferring",
        format!("\n    longest safe in every variant: [{shown}]"),
    )
}

fn check(world: &World, space: &SpaceDo) -> Report {
    let root = space.space_id().to_string();
    let mut planner = Planner::new(world, space);
    let first = planner.build();
    let runs: Vec<Run> = (world.unseen().into_iter())
        .filter(|&unseen| relevant(world, space, first.as_ref(), unseen))
        .map(|unseen| run(world, space, &planner, first.as_ref(), unseen, usize::MAX))
        .collect();
    let befores = || runs.iter().map(|run| &run.before).collect::<Vec<_>>();
    let mut classes = Vec::new();
    let mut hard = false;
    let mut notes = String::new();
    let firsts: Vec<&Verdict> = runs
        .iter()
        .filter_map(|run| run.sent.first())
        .map(|sent| &sent.verdict)
        .collect();
    let first_resolution = (firsts.iter().any(|verdict| !verdict.safe())).then(|| {
        let (resolution, note) = resolve(world, befores(), &runs[0].sent[0].commit, &root);
        notes += &note;
        resolution
    });
    for kind in kinds(&firsts)
        .into_iter()
        .filter(|&kind| kind != "made-up name")
    {
        hard = true;
        classes.push(format!("first commit: {kind}"));
        classes.push(format!(
            "first commit: {kind} → {}",
            first_resolution.unwrap()
        ));
    }
    let later = runs.iter().flat_map(|run| run.sent.iter().skip(1));
    if let Some(sent) = later
        .clone()
        .find(|sent| !sent.verdict.revived.is_empty() || !sent.verdict.lost.is_empty())
    {
        hard = true;
        let (resolution, note) = resolve(world, vec![&sent.before], &sent.commit, &root);
        for kind in kinds(&[&sent.verdict])
            .into_iter()
            .filter(|&kind| kind != "made-up name")
        {
            classes.push(format!("later commit: {kind}"));
            classes.push(format!("later commit: {kind} → {resolution}"));
        }
        notes += &note;
    }
    let made_up = |sent: &Sent| -> u64 {
        (sent.verdict.made_up.iter())
            .filter_map(|&at| {
                (0..world.len()).find(|&i| world.id(i) == target(&sent.commit.ops[at]))
            })
            .fold(0, |made_up, i| made_up | 1 << i)
    };
    let made_up_first: u64 = runs
        .iter()
        .filter_map(|run| run.sent.first())
        .map(made_up)
        .fold(0, |a, b| a | b);
    let made_up_later: u64 = later.map(made_up).fold(0, |a, b| a | b);
    let goals = goals(world);
    let end = reached(world, &root, &runs[0].end, goals);
    if made_up_first | made_up_later != 0 || end != goals {
        let (reached, capped, _) = Reach::reachable(world, befores());
        if capped {
            classes.push("reach search capped".into());
        }
        let better = reached
            .iter()
            .copied()
            .find(|&mask| mask & end == end && mask != end);
        let show = |mask: u64| {
            (0..world.len())
                .filter(|&i| mask & 1 << i != 0)
                .map(|i| world.label(i))
                .collect::<Vec<_>>()
                .join(", ")
        };
        notes += &format!("\n    planner reaches: [{}]", show(end));
        if let Some(mask) = better {
            notes += &format!("\n    a safe order reaches: [{}]", show(mask));
        }
        let at = if made_up_first != 0 { "first" } else { "later" };
        let resolution = first_resolution
            .filter(|_| made_up_first != 0)
            .map_or(String::new(), |r| format!(" → {r}"));
        let made_up = made_up_first | made_up_later;
        match better {
            Some(mask) if mask & made_up != 0 => {
                hard = true;
                let class = format!("{at} commit: made-up name where a safe order gives the name");
                classes.push(format!("{class}{resolution}"));
                classes.push(class);
            }
            Some(mask) => {
                let added = mask & !end;
                let deletes = (0..world.len())
                    .filter(|&i| added & 1 << i != 0)
                    .all(|i| world.local_place(i).is_none());
                if firsts
                    .iter()
                    .any(|verdict| !verdict.made_up_by_delete.is_empty())
                {
                    classes.push(
                        "stops short of names a safe order reaches, where the holder is deleted"
                            .into(),
                    );
                } else if deletes {
                    classes.push("stops short of deletes a safe order sends".into());
                } else {
                    hard = true;
                    classes.push("stops short of names a safe order reaches".into());
                }
            }
            None if made_up != 0 => {
                let (later, unsettled) = Reach::over_commits(world, befores());
                let class = if (later.iter()).any(|&mask| mask & end == end && mask & made_up != 0)
                {
                    hard = true;
                    format!("{at} commit: made-up name where a later commit gives the name")
                } else if unsettled {
                    hard = true;
                    format!("{at} commit: made-up name the search over commits could not settle")
                } else {
                    format!("{at} commit: made-up name where no schedule gives the name")
                };
                classes.push(format!("{class}{resolution}"));
                classes.push(class);
            }
            None => classes.push("stops short of names no safe order reaches".into()),
        }
    }
    let planned = first.as_ref().map_or(0, |commit| commit.ops.len());
    let unseens: Vec<Unseen> = runs.iter().map(|run| run.unseen).collect();
    let changed = (0..world.len())
        .filter(|&i| world.local_of(i) != Local::Kept)
        .fold(0, |changed, i| changed | 1 << i);
    let uncapped = reached(world, &root, &runs[0].end, changed);
    let caps = CAPS.into_iter().filter(|&fits| fits < planned);
    'caps: for (fits, unseen) in
        caps.flat_map(|fits| unseens.iter().map(move |&unseen| (fits, unseen)))
    {
        let capped = run(world, space, &planner, first.as_ref(), unseen, fits);
        let capped_end = reached(world, &root, &capped.end, changed);
        let verdicts: Vec<&Verdict> = capped.sent.iter().map(|sent| &sent.verdict).collect();
        let mut found = Vec::new();
        for kind in kinds(&verdicts)
            .into_iter()
            .filter(|&kind| kind != "made-up name")
        {
            found.push(format!("under a cap of {fits}: {kind}"));
        }
        match capped.stop {
            Stop::Busy => found.push(format!(
                "under a cap of {fits}: still sending after {COMMITS} commits"
            )),
            Stop::Stalled => found.push(format!(
                "under a cap of {fits}: acknowledge or split refused"
            )),
            _ if unseen == Unseen::Nothing && capped_end & uncapped != uncapped => found.push(
                format!("under a cap of {fits}: stops short of names the uncapped run reaches"),
            ),
            _ => {}
        }
        if !found.is_empty() {
            hard = true;
            classes.extend(found);
            notes += &format!(
                "\n    under a cap of {fits}:{}",
                show_runs(world, std::slice::from_ref(&capped))
            );
            break 'caps;
        }
    }
    if runs.iter().any(|run| run.stop == Stop::Busy) {
        hard = true;
        classes.push(format!("still sending after {COMMITS} commits"));
    }
    if runs.iter().any(|run| run.stop == Stop::Stalled) {
        hard = true;
        classes.push("acknowledge refused".into());
    }
    if firsts
        .iter()
        .any(|verdict| !verdict.made_up_by_delete.is_empty())
    {
        classes.push("made-up name where the holder is deleted".into());
    }
    if firsts.iter().any(|verdict| !verdict.rejected.is_empty()) {
        classes.push("rejected op".into());
    }
    if runs.iter().any(|run| run.stop == Stop::Refused) {
        classes.push("commit refused because the head moved".into());
    }
    let total: usize = runs
        .iter()
        .flat_map(|run| &run.sent)
        .map(|sent| sent.commit.ops.len())
        .sum();
    if total == 0 {
        classes.push("sends nothing".into());
        let ops = naive(world, &root);
        let template = proto::Commit {
            commit_id: IdGen::new(15).uuid_v7(NOW),
            history_id: space.history_id().into(),
            device_seq: 1,
            base_seq: space.head_seq(),
            ..Default::default()
        };
        let (placed, safe) = Search::longest(world, befores(), &template, &ops, 2);
        if placed > 0 {
            classes.push("sends nothing while an op is safe".into());
            let shown: Vec<String> = safe.iter().map(|op| world.show_op(&root, op)).collect();
            notes += &format!("\n    safe: [{}]", shown.join(", "));
        }
    }
    classes.sort();
    classes.dedup();
    if classes.is_empty() {
        classes.push("clean".into());
    }
    let size = world.len() * 100 + world.changes() * 10 + planned;
    let text = if classes == ["clean"] {
        String::new()
    } else {
        format!(
            "{}{}{}{}",
            world.describe(),
            world.show_ranks(),
            show_runs(world, &runs),
            notes
        )
    };
    Report {
        runs: runs.len(),
        hard,
        classes,
        size,
        text,
    }
}

#[derive(Default)]
struct Tally {
    worlds: usize,
    runs: usize,
    hard: usize,
    classes: BTreeMap<String, (usize, Vec<(usize, String)>)>,
}

impl Tally {
    fn note(&mut self, class: String, count: usize, examples: Vec<(usize, String)>) {
        let entry = self.classes.entry(class).or_default();
        entry.0 += count;
        entry.1.extend(examples);
        entry.1.sort();
        entry.1.dedup();
        entry.1.truncate(EXAMPLES);
    }

    fn add(&mut self, report: Report) {
        self.worlds += 1;
        self.runs += report.runs;
        self.hard += usize::from(report.hard);
        for class in report.classes {
            let example = (!report.text.is_empty()).then(|| (report.size, report.text.clone()));
            self.note(class, 1, example.into_iter().collect());
        }
    }

    fn merge(&mut self, other: Tally) {
        self.worlds += other.worlds;
        self.runs += other.runs;
        self.hard += other.hard;
        for (class, (count, examples)) in other.classes {
            self.note(class, count, examples);
        }
    }

    fn summary(&self) -> String {
        let mut text = format!(
            "{} worlds, {} runs, {} hard",
            self.worlds, self.runs, self.hard
        );
        for (class, (count, examples)) in &self.classes {
            text += &format!("\n{class}: {count}");
            for (_, example) in examples {
                text += &format!("\n  {example}");
            }
        }
        text
    }
}

fn remotes(k: usize) -> Vec<Vec<Remote>> {
    let mut all = vec![vec![Remote::Kept; k]];
    for i in 0..k {
        let menu = [Remote::Trashed, Remote::Forgotten, Remote::Into(Spot::Root)].into_iter();
        for remote in menu.chain(
            (0..k)
                .filter(|&j| j != i)
                .map(|j| Remote::Into(Spot::In(j))),
        ) {
            let mut next = vec![Remote::Kept; k];
            next[i] = remote;
            all.push(next);
        }
    }
    all
}

fn local_menu(i: usize, k: usize, len: usize) -> Vec<Local> {
    let mut menu = Vec::new();
    if i < k {
        menu.extend([Local::Kept, Local::Removed]);
    }
    menu.push(Local::Into(Spot::Root));
    menu.extend(
        (0..len)
            .filter(|&j| j != i)
            .map(|j| Local::Into(Spot::In(j))),
    );
    menu.extend((0..k).filter(|&j| j != i).map(Local::Onto));
    menu
}

struct Fill<'a> {
    bounds: &'a Bounds,
    synced: &'a [Spot],
    remote: &'a [Remote],
    config: &'a [Kind],
    menus: Vec<Vec<Local>>,
}

impl Fill<'_> {
    fn each(&self, picked: &mut Vec<Local>, changed: usize, visit: &mut dyn FnMut(World)) {
        let k = self.synced.len();
        if picked.len() == self.menus.len() {
            let world = World {
                synced: self.synced.to_vec(),
                remote: self.remote.to_vec(),
                local: picked[..k].to_vec(),
                created: self
                    .config
                    .iter()
                    .copied()
                    .zip(picked[k..].iter().copied())
                    .collect(),
                ranks: Vec::new(),
            };
            if world.fits(self.bounds) {
                visit(world);
            }
            return;
        }
        for local in &self.menus[picked.len()] {
            let change = usize::from(picked.len() < k && *local != Local::Kept);
            if changed + change > self.bounds.local_changes {
                continue;
            }
            picked.push(*local);
            self.each(picked, changed + change, visit);
            picked.pop();
        }
    }
}

fn each_world(bounds: &Bounds, synced: &[Spot], remote: &[Remote], visit: &mut dyn FnMut(World)) {
    let k = synced.len();
    for config in bounds.created {
        let len = k + config.len();
        let fill = Fill {
            bounds,
            synced,
            remote,
            config,
            menus: (0..len).map(|i| local_menu(i, k, len)).collect(),
        };
        fill.each(&mut Vec::with_capacity(len), 0, visit);
    }
}

fn sweep(bounds: &Bounds, threads: usize, (shard, shards): (usize, usize)) -> Tally {
    let jobs: Vec<(Vec<Spot>, Vec<Remote>)> = (bounds.existing.clone())
        .flat_map(|k| {
            (forests(k).into_iter()).flat_map(move |synced| {
                remotes(k)
                    .into_iter()
                    .map(move |remote| (synced.clone(), remote))
            })
        })
        .enumerate()
        .filter(|(at, _)| at % shards == shard)
        .map(|(_, job)| job)
        .collect();
    let next = AtomicUsize::new(0);
    let total = Mutex::new(Tally::default());
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let mut tally = Tally::default();
                while let Some((synced, remote)) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let probe = World {
                        synced: synced.clone(),
                        remote: remote.clone(),
                        local: vec![Local::Kept; synced.len()],
                        created: vec![],
                        ranks: Vec::new(),
                    };
                    if !probe.remote_acyclic() {
                        continue;
                    }
                    let mut spaces: BTreeMap<Vec<usize>, SpaceDo> = BTreeMap::new();
                    each_world(bounds, synced, remote, &mut |mut world| {
                        for ranks in orders(bounds, world.existing(), world.len()) {
                            world.ranks = ranks;
                            let space = (spaces.entry(world.ranks[..world.existing()].to_vec()))
                                .or_insert_with(|| server(&world));
                            tally.add(check(&world, space));
                        }
                    });
                }
                total.lock().unwrap().merge(tally);
            });
        }
    });
    total.into_inner().unwrap()
}

fn threads() -> usize {
    std::env::var("PLANNER_MODEL_THREADS")
        .ok()
        .and_then(|threads| threads.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |threads| threads.get()))
}

fn world(synced: &[Spot], remote: &[Remote], local: &[Local], created: &[(Kind, Local)]) -> World {
    World {
        synced: synced.to_vec(),
        remote: remote.to_vec(),
        local: local.to_vec(),
        created: created.to_vec(),
        ranks: Vec::new(),
    }
}

fn repros() -> Vec<(&'static str, World)> {
    use Local::{Into as To, Kept, Onto, Removed};
    use Spot::{In, Root};
    let kept = Remote::Kept;
    let held = Remote::Into(Root);
    let ordered = |world: World, ranks: &[usize]| World {
        ranks: ranks.to_vec(),
        ..world
    };
    let renamed = world(
        &[Root, Root],
        &[kept, kept],
        &[Onto(1), To(In(2))],
        &[(Kind::Dir, To(In(0)))],
    );
    vec![
        (
            "a rename onto the name of a folder moved into a new folder inside it",
            ordered(renamed.clone(), &[1, 2, 0]),
        ),
        (
            "planned: a folder renamed onto a name moving into it",
            renamed,
        ),
        (
            "a cycle against R through a new folder",
            ordered(
                world(
                    &[Root, Root, Root],
                    &[kept, kept, Remote::Into(In(0))],
                    &[To(In(1)), To(In(3)), Kept],
                    &[(Kind::Dir, To(In(2)))],
                ),
                &[3, 2, 0, 1],
            ),
        ),
        (
            "a folder moved out onto the name of a folder moving into it",
            world(
                &[Root, In(0), Root],
                &[kept, kept, kept],
                &[Kept, Onto(2), To(In(1))],
                &[(Kind::File, To(In(1)))],
            ),
        ),
        (
            "a folder moved out onto a name R holds",
            ordered(
                world(
                    &[Root, In(0), Root],
                    &[kept, kept, held],
                    &[Kept, Onto(2), Kept],
                    &[(Kind::File, To(In(1)))],
                ),
                &[0, 1, 3, 2],
            ),
        ),
        (
            "a folder moved onto the name of a held delete",
            world(
                &[Root, In(0), Root],
                &[kept, kept, kept],
                &[Kept, Onto(2), Removed],
                &[(Kind::Dir, To(Root)), (Kind::File, To(In(1)))],
            ),
        ),
        (
            "a holder stuck behind a hard wait",
            ordered(
                world(
                    &[Root, Root, Root, Root],
                    &[kept, kept, Remote::Forgotten, kept],
                    &[Onto(3), To(In(2)), Kept, To(In(1))],
                    &[(Kind::File, To(In(0)))],
                ),
                &[1, 2, 3, 4, 0],
            ),
        ),
        (
            "a transitive name wait",
            world(
                &[Root, Root, Root, Root],
                &[kept, kept, kept, held],
                &[Onto(1), To(In(2)), Onto(3), Kept],
                &[],
            ),
        ),
        (
            "a create name wait",
            world(
                &[Root, Root, Root, Root],
                &[kept, kept, kept, held],
                &[Onto(1), To(In(4)), Onto(3), Kept],
                &[(Kind::Dir, To(In(2)))],
            ),
        ),
        (
            "planned: a folder renamed onto the name of a folder moving two new folders deep",
            world(
                &[Root, Root, Root, Root],
                &[kept, kept, kept, held],
                &[Onto(1), To(In(5)), Onto(3), Kept],
                &[(Kind::Dir, To(In(2))), (Kind::Dir, To(In(4)))],
            ),
        ),
        (
            "engine_sync: a folder takes the name of a node that moves into it, one op per commit",
            world(&[Root, Root], &[kept, kept], &[Onto(1), To(In(0))], &[]),
        ),
        (
            "engine_sync: a folder takes the name of a node that moves into it with a folder created in it, one op per commit",
            world(
                &[Root, Root],
                &[kept, kept],
                &[Onto(1), To(In(0))],
                &[(Kind::Dir, To(In(0)))],
            ),
        ),
    ]
}

#[test]
fn the_known_repros_are_worlds_of_the_full_sweep() {
    for (name, world) in repros() {
        assert!(
            world.canonical().admits(&FULL),
            "{name}: {}",
            world.describe()
        );
    }
}

#[test]
fn the_known_repros_keep_trashed_folders_trashed_and_names_given() {
    let mut failed = Vec::new();
    for (name, world) in repros() {
        let world = world.canonical();
        let report = check(&world, &server(&world));
        println!("{name}: {:?}\n  {}", report.classes, report.text);
        if report.hard {
            failed.push(name);
        }
    }
    assert!(failed.is_empty(), "{failed:?}");
}

#[test]
fn every_world_of_the_slice_keeps_trashed_folders_trashed_and_names_given() {
    let tally = sweep(&SLICE, threads(), (0, 1));
    println!("{}", tally.summary());
    assert_eq!(tally.hard, 0, "{}", tally.summary());
}

fn full_sweep(bounds: &Bounds, (shard, shards): (usize, usize)) {
    let tally = sweep(bounds, threads(), (shard, shards));
    println!("shard {shard} of {shards}: {}", tally.summary());
    assert_eq!(tally.hard, 0, "{}", tally.summary());
}

macro_rules! shards {
    ($($name:ident = ($bounds:ident, $shard:literal of $shards:literal),)*) => {
        $(
            #[test]
            #[ignore = "the full sweep runs for minutes; run its shards on heavy-tests.yml"]
            fn $name() {
                full_sweep(&$bounds, ($shard, $shards));
            }
        )*
    };
}

shards! {
    three_nodes_shard_0_keeps_trashed_folders_trashed_and_names_given = (THREE_NODES, 0 of 8),
    three_nodes_shard_1_keeps_trashed_folders_trashed_and_names_given = (THREE_NODES, 1 of 8),
    three_nodes_shard_2_keeps_trashed_folders_trashed_and_names_given = (THREE_NODES, 2 of 8),
    three_nodes_shard_3_keeps_trashed_folders_trashed_and_names_given = (THREE_NODES, 3 of 8),
    three_nodes_shard_4_keeps_trashed_folders_trashed_and_names_given = (THREE_NODES, 4 of 8),
    three_nodes_shard_5_keeps_trashed_folders_trashed_and_names_given = (THREE_NODES, 5 of 8),
    three_nodes_shard_6_keeps_trashed_folders_trashed_and_names_given = (THREE_NODES, 6 of 8),
    three_nodes_shard_7_keeps_trashed_folders_trashed_and_names_given = (THREE_NODES, 7 of 8),
    four_nodes_shard_0_keeps_trashed_folders_trashed_and_names_given = (FOUR_NODES, 0 of 16),
    four_nodes_shard_1_keeps_trashed_folders_trashed_and_names_given = (FOUR_NODES, 1 of 16),
    four_nodes_shard_2_keeps_trashed_folders_trashed_and_names_given = (FOUR_NODES, 2 of 16),
    four_nodes_shard_3_keeps_trashed_folders_trashed_and_names_given = (FOUR_NODES, 3 of 16),
    four_nodes_shard_4_keeps_trashed_folders_trashed_and_names_given = (FOUR_NODES, 4 of 16),
    four_nodes_shard_5_keeps_trashed_folders_trashed_and_names_given = (FOUR_NODES, 5 of 16),
    four_nodes_shard_6_keeps_trashed_folders_trashed_and_names_given = (FOUR_NODES, 6 of 16),
    four_nodes_shard_7_keeps_trashed_folders_trashed_and_names_given = (FOUR_NODES, 7 of 16),
    four_nodes_shard_8_keeps_trashed_folders_trashed_and_names_given = (FOUR_NODES, 8 of 16),
    four_nodes_shard_9_keeps_trashed_folders_trashed_and_names_given = (FOUR_NODES, 9 of 16),
    four_nodes_shard_10_keeps_trashed_folders_trashed_and_names_given = (FOUR_NODES, 10 of 16),
    four_nodes_shard_11_keeps_trashed_folders_trashed_and_names_given = (FOUR_NODES, 11 of 16),
    four_nodes_shard_12_keeps_trashed_folders_trashed_and_names_given = (FOUR_NODES, 12 of 16),
    four_nodes_shard_13_keeps_trashed_folders_trashed_and_names_given = (FOUR_NODES, 13 of 16),
    four_nodes_shard_14_keeps_trashed_folders_trashed_and_names_given = (FOUR_NODES, 14 of 16),
    four_nodes_shard_15_keeps_trashed_folders_trashed_and_names_given = (FOUR_NODES, 15 of 16),
}
