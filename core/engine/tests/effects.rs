// SPDX-License-Identifier: Apache-2.0

//! The disk effects that apply remote changes, and parking (docs/design/sync-engine.md sections
//! 2.5 and 5.2), run against an in-memory disk whose creates and renames never replace an entry and
//! whose names compare by name key, as on a case-insensitive volume.

use std::collections::BTreeMap;

use koidrive_engine::classify::{Class, Trees};
use koidrive_engine::effects::{
    Base, BatchLimit, Dir, Displaced, Effect, EffectError, Effects, Location,
};
use koidrive_engine::ids::IdGen;
use koidrive_engine::names::name_key;
use koidrive_engine::state::{EffectsChanges, EffectsState, TreesChanges, TreesState};
use koidrive_engine::tree::{
    Content, FileIdentity, Kind, LocalNode, RemoteNode, Shape, same_content,
};
use proptest::prelude::*;

const ROOT: &str = "root";
const NOW: u64 = 1_794_268_740_000;

fn content(digest: u8) -> Content {
    Content {
        size: 3,
        manifest_digest: vec![digest; 32],
        mtime_ns: 1_794_268_740_123_456_789,
        ..Default::default()
    }
}

fn file(parent: &str, name: &str, digest: u8) -> Shape {
    Shape {
        parent: parent.into(),
        name: name.into(),
        kind: Kind::File,
        content: Some(content(digest)),
    }
}

fn dir(parent: &str, name: &str) -> Shape {
    Shape {
        parent: parent.into(),
        name: name.into(),
        kind: Kind::Dir,
        content: None,
    }
}

fn identity(ino: u128) -> FileIdentity {
    FileIdentity {
        dev: 1,
        ino,
        birth_ns: Some(0),
    }
}

fn remote(shape: Shape) -> RemoteNode {
    RemoteNode {
        shape,
        trashed: false,
        content_rev: 1,
        meta_rev: 1,
        meta_edit_rev: 1,
        trash_group: String::new(),
        version_id: String::new(),
        edit_rev: 1,
        pending_for: None,
        pending_by: None,
    }
}

fn root_at(name: &str) -> Location {
    Location::At {
        base: Base::Root,
        dirs: vec![],
        name: name.into(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Area {
    Root,
    Parked,
    Dir(u128),
}

#[derive(Clone, Debug)]
struct Entry {
    area: Area,
    name: String,
    kind: Kind,
    content: Option<Content>,
}

/// Entries by inode number, which is also the identity's `ino`.
#[derive(Default)]
struct Disk {
    entries: BTreeMap<u128, Entry>,
    next: u128,
}

impl Disk {
    fn find(&self, area: Area, name: &str) -> Option<u128> {
        let key = name_key(name);
        self.entries
            .iter()
            .find(|(_, e)| e.area == area && name_key(&e.name) == key)
            .map(|(ino, _)| *ino)
    }

    fn resolve(&self, at: &Location) -> Result<(Area, String), String> {
        let (base, dirs, name) = match at {
            Location::Parked(node) => return Ok((Area::Parked, node.clone())),
            Location::At { base, dirs, name } => (base, dirs, name),
        };
        let mut area = match base {
            Base::Root => Area::Root,
            Base::Parked { node, identity } => match self.find(Area::Parked, node) {
                Some(ino) if ino == identity.ino => Area::Dir(ino),
                _ => return Err(format!("parked {node} is not {}", identity.ino)),
            },
        };
        for Dir { name, identity } in dirs {
            match self.find(area, name) {
                Some(ino) if ino == identity.ino && self.entries[&ino].kind == Kind::Dir => {
                    area = Area::Dir(ino)
                }
                _ => return Err(format!("directory {name} is not {}", identity.ino)),
            }
        }
        Ok((area, name.clone()))
    }

    fn existing(&self, at: &Location, identity: &FileIdentity) -> Result<u128, String> {
        let (area, name) = self.resolve(at)?;
        match self.find(area, &name) {
            Some(ino) if ino == identity.ino => Ok(ino),
            _ => Err(format!("{at:?} is not {}", identity.ino)),
        }
    }

    fn free(&self, at: &Location) -> Result<(Area, String), String> {
        let (area, name) = self.resolve(at)?;
        match self.find(area, &name) {
            None => Ok((area, name)),
            Some(_) => Err(format!("{at:?} is taken")),
        }
    }

    fn check_content(&self, ino: u128, displaced: &Displaced) -> Result<(), String> {
        match &self.entries[&ino].content {
            Some(c) if same_content(c, &displaced.content) => Ok(()),
            _ => Err(format!("{:?} holds other content", displaced.at)),
        }
    }

    fn add(&mut self, area: Area, name: String, kind: Kind, content: Option<Content>) -> u128 {
        self.next += 1;
        self.entries.insert(
            self.next,
            Entry {
                area,
                name,
                kind,
                content,
            },
        );
        self.next
    }

    /// Does the effect the way section 5.2 allows, or says why it cannot.
    fn apply(&mut self, effect: &Effect) -> Result<Option<FileIdentity>, String> {
        match effect {
            Effect::CreateDir { at } => {
                let (area, name) = self.free(at)?;
                Ok(Some(identity(self.add(area, name, Kind::Dir, None))))
            }
            Effect::WriteFile {
                at,
                content,
                replaces,
            } => {
                if let Some(displaced) = replaces {
                    let ino = self.existing(&displaced.at, &displaced.identity)?;
                    self.check_content(ino, displaced)?;
                    self.entries.remove(&ino);
                }
                let (area, name) = self.free(at)?;
                let ino = self.add(area, name, Kind::File, Some(content.clone()));
                Ok(Some(identity(ino)))
            }
            Effect::Move { from, identity, to } => {
                let ino = self.existing(from, identity)?;
                let (area, name) = self.free(to)?;
                let mut up = area;
                while let Area::Dir(parent) = up {
                    if parent == ino {
                        return Err(format!("{from:?} would move into itself"));
                    }
                    up = self.entries[&parent].area;
                }
                let entry = self.entries.get_mut(&ino).unwrap();
                entry.area = area;
                entry.name = name;
                Ok(None)
            }
            Effect::RemoveFile { displaced } => {
                let ino = self.existing(&displaced.at, &displaced.identity)?;
                self.check_content(ino, displaced)?;
                self.entries.remove(&ino);
                Ok(None)
            }
            Effect::RemoveDir { at, identity } => {
                let ino = self.existing(at, identity)?;
                if self.entries.values().any(|e| e.area == Area::Dir(ino)) {
                    return Err(format!("{at:?} is not empty"));
                }
                self.entries.remove(&ino);
                Ok(None)
            }
        }
    }

    /// The disk holds exactly what L says: each node's entry where L puts it.
    fn matches(&self, trees: &Trees) -> Result<(), String> {
        if self.entries.len() != trees.local().len() {
            return Err(format!(
                "{} entries on disk, {} in L",
                self.entries.len(),
                trees.local().len()
            ));
        }
        for (id, node) in trees.local().iter() {
            let entry = self
                .entries
                .get(&node.identity.ino)
                .ok_or_else(|| format!("{id} is not on disk"))?;
            let (area, name) = if node.parked {
                (Area::Parked, id.clone())
            } else if node.shape.parent == ROOT {
                (Area::Root, node.shape.name.clone())
            } else {
                let parent = &trees.local().get(&node.shape.parent).unwrap().identity;
                (Area::Dir(parent.ino), node.shape.name.clone())
            };
            let content_matches = match (&entry.content, &node.shape.content) {
                (Some(a), Some(b)) => same_content(a, b),
                (a, b) => a.is_none() && b.is_none(),
            };
            if entry.area != area || entry.name != name || !content_matches {
                return Err(format!("{id} is {entry:?} on disk"));
            }
        }
        Ok(())
    }
}

/// A space whose disk, L and S hold each node's first shape and whose R holds its second.
fn space(nodes: &[(&str, Option<Shape>, Option<Shape>)]) -> (Trees, Disk) {
    let mut trees = Trees::new(ROOT);
    let mut disk = Disk {
        next: nodes.len() as u128,
        ..Disk::default()
    };
    let ino = |id: &str| nodes.iter().position(|n| n.0 == id).unwrap() as u128 + 1;
    for (i, (id, synced, after)) in nodes.iter().enumerate() {
        trees.set_synced((*id).into(), synced.clone());
        if let Some(shape) = synced {
            let area = match shape.parent.as_str() {
                ROOT => Area::Root,
                parent => Area::Dir(ino(parent)),
            };
            disk.entries.insert(
                i as u128 + 1,
                Entry {
                    area,
                    name: shape.name.clone(),
                    kind: shape.kind,
                    content: shape.content.clone(),
                },
            );
            trees.set_local(
                (*id).into(),
                LocalNode {
                    shape: shape.clone(),
                    identity: identity(i as u128 + 1),
                    parked: false,
                },
            );
        }
        if let Some(shape) = after {
            trees.set_remote((*id).into(), remote(shape.clone()));
        }
    }
    (trees, disk)
}

fn take_changes(
    trees: &mut Trees,
    effects: &mut Effects,
    kept: &mut (TreesState, EffectsState),
) -> Result<(), String> {
    let now = (trees.state(), effects.state());
    let expected = (
        TreesChanges::between(&kept.0, &now.0),
        EffectsChanges::between(&kept.1, &now.1),
    );
    let taken = (trees.take_changes(), effects.take_changes());
    *kept = now;
    match taken == expected {
        true => Ok(()),
        false => Err(format!("took {taken:?}, the states differ by {expected:?}")),
    }
}

/// Settles, plans and does effects until none is left, checking the trees and the disk after each.
fn converge(trees: &mut Trees, disk: &mut Disk, budget: usize) -> Result<Vec<Effect>, String> {
    let (mut effects, mut ids, mut done) = (Effects::default(), IdGen::new(1), Vec::new());
    trees.take_changes();
    let mut kept = (trees.state(), effects.state());
    for _ in 0..=budget {
        trees.settle();
        take_changes(trees, &mut effects, &mut kept)?;
        let Some(record) = effects
            .plan(trees, None, &mut ids, NOW, BatchLimit::ONE)
            .first()
            .cloned()
        else {
            return Ok(done);
        };
        take_changes(trees, &mut effects, &mut kept)?;
        if record.counter != effects.cleared() + 1 {
            return Err(format!("counter {}", record.counter));
        }
        let created = disk.apply(&record.effect)?;
        effects
            .done(trees, None, &record.effect_id, created)
            .map_err(|e| format!("{e:?}"))?;
        take_changes(trees, &mut effects, &mut kept)?;
        trees.check().map_err(|e| format!("{e:?}"))?;
        disk.matches(trees)?;
        done.push(record.effect);
    }
    Err(format!("more than {budget} effects"))
}

fn converge_in_batches(
    trees: &mut Trees,
    disk: &mut Disk,
    budget: usize,
    limit: usize,
    turn: usize,
) -> Result<usize, String> {
    let (mut effects, mut ids, mut done) = (Effects::default(), IdGen::new(1), 0);
    let limit = BatchLimit {
        effects: limit,
        bytes: u64::MAX,
    };
    while done <= budget {
        trees.settle();
        let mut batch = effects.plan(trees, None, &mut ids, NOW, limit).to_vec();
        if batch.is_empty() {
            return Ok(done);
        }
        let counter = effects.cleared() + batch.len() as u64;
        if batch.iter().any(|record| record.counter != counter) {
            return Err(format!(
                "a batch of {} without the counter {counter}",
                batch.len()
            ));
        }
        let len = batch.len();
        batch.rotate_left(turn % len);
        let mut outcomes = Vec::new();
        for record in &batch {
            outcomes.push(disk.apply(&record.effect)?);
        }
        for (record, created) in batch.iter().zip(outcomes) {
            effects
                .done(trees, None, &record.effect_id, created)
                .map_err(|e| format!("{e:?}"))?;
        }
        trees.check().map_err(|e| format!("{e:?}"))?;
        disk.matches(trees)?;
        done += len;
    }
    Err(format!("more than {budget} effects"))
}

fn assert_in_sync(trees: &mut Trees) {
    trees.settle();
    assert_eq!(trees.work(), []);
    assert!(trees.local().iter().all(|(_, node)| !node.parked));
}

#[test]
fn creates_parents_before_children() {
    let (mut trees, mut disk) = space(&[
        ("f", None, Some(file("d", "f", 1))),
        ("d", None, Some(dir(ROOT, "d"))),
    ]);
    let done = converge(&mut trees, &mut disk, 10).unwrap();
    assert_eq!(
        done,
        [
            Effect::CreateDir { at: root_at("d") },
            Effect::WriteFile {
                at: Location::At {
                    base: Base::Root,
                    dirs: vec![Dir {
                        name: "d".into(),
                        identity: identity(3)
                    }],
                    name: "f".into()
                },
                content: content(1),
                replaces: None
            }
        ]
    );
    assert_in_sync(&mut trees);
}

#[test]
fn a_changed_file_is_replaced_through_quarantine() {
    let (mut trees, mut disk) = space(&[("f", Some(file(ROOT, "f", 1)), Some(file(ROOT, "f", 2)))]);
    let done = converge(&mut trees, &mut disk, 10).unwrap();
    assert_eq!(
        done,
        [Effect::WriteFile {
            at: root_at("f"),
            content: content(2),
            replaces: Some(Displaced {
                at: root_at("f"),
                identity: identity(1),
                content: content(1)
            })
        }]
    );
    assert_in_sync(&mut trees);
}

#[test]
fn a_file_changed_elsewhere_keeps_the_spelling_of_its_name_on_disk() {
    let (mut trees, mut disk) = space(&[(
        "f",
        Some(file(ROOT, "e\u{301}", 1)),
        Some(file(ROOT, "\u{e9}", 2)),
    )]);
    converge(&mut trees, &mut disk, 10).unwrap();
    let local = trees.local().get("f").map(|node| node.shape.name.as_str());
    assert_eq!(local, Some("e\u{301}"));
    assert_in_sync(&mut trees);
}

#[test]
fn a_file_moved_and_changed_is_written_at_its_new_place() {
    let (mut trees, mut disk) = space(&[("f", Some(file(ROOT, "a", 1)), Some(file(ROOT, "b", 2)))]);
    let done = converge(&mut trees, &mut disk, 10).unwrap();
    assert_eq!(
        done,
        [Effect::WriteFile {
            at: root_at("b"),
            content: content(2),
            replaces: Some(Displaced {
                at: root_at("a"),
                identity: identity(1),
                content: content(1)
            })
        }]
    );
    assert_in_sync(&mut trees);
}

#[test]
fn children_are_removed_before_their_directory() {
    let (mut trees, mut disk) = space(&[
        ("d", Some(dir(ROOT, "d")), None),
        ("f", Some(file("d", "f", 1)), None),
    ]);
    let done = converge(&mut trees, &mut disk, 10).unwrap();
    assert!(matches!(done[0], Effect::RemoveFile { .. }));
    assert_eq!(
        done[1..],
        [Effect::RemoveDir {
            at: root_at("d"),
            identity: identity(1)
        }]
    );
    assert_in_sync(&mut trees);
    assert!(disk.entries.is_empty());
}

#[test]
fn a_name_swap_parks_one_file() {
    let (mut trees, mut disk) = space(&[
        ("a", Some(file(ROOT, "x", 1)), Some(file(ROOT, "y", 1))),
        ("b", Some(file(ROOT, "y", 2)), Some(file(ROOT, "x", 2))),
    ]);
    let done = converge(&mut trees, &mut disk, 10).unwrap();
    assert_eq!(
        done,
        [
            Effect::Move {
                from: root_at("x"),
                identity: identity(1),
                to: Location::Parked("a".into())
            },
            Effect::Move {
                from: root_at("y"),
                identity: identity(2),
                to: root_at("x")
            },
            Effect::Move {
                from: Location::Parked("a".into()),
                identity: identity(1),
                to: root_at("y")
            },
        ]
    );
    assert_in_sync(&mut trees);
}

#[test]
fn a_directory_moved_under_its_former_child_is_parked_with_it() {
    let (mut trees, mut disk) = space(&[
        ("a", Some(dir(ROOT, "a")), Some(dir("b", "b"))),
        ("b", Some(dir("a", "b")), Some(dir(ROOT, "a"))),
    ]);
    let done = converge(&mut trees, &mut disk, 10).unwrap();
    assert_eq!(
        done,
        [
            Effect::Move {
                from: root_at("a"),
                identity: identity(1),
                to: Location::Parked("a".into())
            },
            Effect::Move {
                from: Location::At {
                    base: Base::Parked {
                        node: "a".into(),
                        identity: identity(1)
                    },
                    dirs: vec![],
                    name: "b".into()
                },
                identity: identity(2),
                to: root_at("a")
            },
            Effect::Move {
                from: Location::Parked("a".into()),
                identity: identity(1),
                to: Location::At {
                    base: Base::Root,
                    dirs: vec![Dir {
                        name: "a".into(),
                        identity: identity(2)
                    }],
                    name: "b".into()
                }
            },
        ]
    );
    assert_in_sync(&mut trees);
}

#[test]
fn a_case_only_rename_goes_through_the_parked_directory() {
    let (mut trees, mut disk) = space(&[(
        "f",
        Some(file(ROOT, "report", 1)),
        Some(file(ROOT, "Report", 1)),
    )]);
    let done = converge(&mut trees, &mut disk, 10).unwrap();
    assert_eq!(
        done,
        [
            Effect::Move {
                from: root_at("report"),
                identity: identity(1),
                to: Location::Parked("f".into())
            },
            Effect::Move {
                from: Location::Parked("f".into()),
                identity: identity(1),
                to: root_at("Report")
            },
        ]
    );
    assert_in_sync(&mut trees);
}

#[test]
fn only_a_node_whose_only_change_is_remote_gets_an_effect_and_one_at_a_time() {
    let (mut trees, _) = space(&[
        ("edited", Some(file(ROOT, "e", 1)), Some(file(ROOT, "e", 2))),
        ("g", Some(file(ROOT, "g", 1)), Some(file(ROOT, "g", 2))),
        ("h", Some(file(ROOT, "h", 1)), Some(file(ROOT, "h", 2))),
    ]);
    let mut edited = trees.local().get("edited").unwrap().clone();
    edited.shape.content = Some(content(3));
    trees.set_local("edited".into(), edited);
    trees.settle();
    assert_eq!(trees.class("edited"), Class::Concurrent);
    let (mut effects, mut ids) = (Effects::default(), IdGen::new(1));
    let first = effects
        .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
        .first()
        .unwrap()
        .clone();
    assert_eq!(first.node, "g");
    assert_eq!(first.counter, 1);
    assert_eq!(
        effects
            .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
            .first(),
        None
    );
    assert_eq!(effects.in_flight().first(), Some(&first));
}

fn marked(shape: Shape, pending_for: &str) -> RemoteNode {
    RemoteNode {
        pending_for: Some(pending_for.into()),
        ..remote(shape)
    }
}

#[test]
fn a_node_a_user_named_like_a_pending_name_is_never_renamed() {
    let (mut trees, mut disk) = space(&[(
        "d",
        Some(dir(ROOT, "A (pending)")),
        Some(dir(ROOT, "A (pending)")),
    )]);
    assert_eq!(converge(&mut trees, &mut disk, 10).unwrap(), []);
    assert_eq!(
        trees.local().get("d").unwrap().shape,
        dir(ROOT, "A (pending)")
    );
    assert_in_sync(&mut trees);
}

#[test]
fn a_marked_node_that_l_holds_elsewhere_keeps_the_users_move() {
    let (mut trees, mut disk) = space(&[("d", Some(dir(ROOT, "A (pending)")), None)]);
    trees.set_remote("d".into(), marked(dir(ROOT, "A (pending)"), "A"));
    let local = trees.local().get("d").unwrap().clone();
    trees.set_local(
        "d".into(),
        LocalNode {
            shape: dir(ROOT, "B"),
            ..local
        },
    );
    disk.entries.get_mut(&1).unwrap().name = "B".into();
    assert_eq!(converge(&mut trees, &mut disk, 10).unwrap(), []);
    assert_eq!(trees.local().get("d").unwrap().shape, dir(ROOT, "B"));
}

#[test]
fn a_failed_effect_is_not_emitted_again_unchanged() {
    let (mut trees, _) = space(&[("f", Some(file(ROOT, "f", 1)), Some(file(ROOT, "f", 2)))]);
    trees.settle();
    let (mut effects, mut ids) = (Effects::default(), IdGen::new(1));
    let first = effects
        .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
        .first()
        .unwrap()
        .clone();
    let id = first.effect_id.clone();
    assert_eq!(effects.failed(&mut trees, &id), Ok(()));
    assert_eq!(effects.cleared(), 1);
    assert_eq!(
        effects
            .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
            .first(),
        None
    );
    assert_eq!(effects.blocked().get("f"), Some(&first.effect));

    let same = trees.local().get("f").unwrap().clone();
    trees.set_local("f".into(), same.clone());
    assert_eq!(
        effects
            .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
            .first(),
        None
    );

    trees.set_local(
        "f".into(),
        LocalNode {
            identity: identity(9),
            ..same
        },
    );
    let again = effects
        .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
        .first()
        .unwrap();
    assert_eq!(again.counter, 2);
    assert_ne!(again.effect_id, id);
}

/// File d moves to e on the server, and new file z takes d's old name, but d's entry is gone from
/// the disk, so every effect for d fails. Once the park out of z's way has failed too, d gets no
/// effect until L changes or the caller unblocks it.
#[test]
fn a_node_whose_park_failed_gets_no_effect_until_l_changes() {
    let (mut trees, _) = space(&[
        ("d", Some(file(ROOT, "d", 1)), Some(file(ROOT, "e", 1))),
        ("z", None, Some(file(ROOT, "d", 2))),
    ]);
    trees.settle();
    let (mut effects, mut ids) = (Effects::default(), IdGen::new(1));
    let mut planned = Vec::new();
    while let Some(record) = effects
        .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
        .first()
        .cloned()
    {
        effects.failed(&mut trees, &record.effect_id).unwrap();
        planned.push(record.effect);
        if planned.len() == 6 {
            break;
        }
    }
    assert!(
        matches!(
            planned.as_slice(),
            [
                Effect::Move {
                    to: Location::At { .. },
                    ..
                },
                Effect::Move {
                    to: Location::Parked(_),
                    ..
                }
            ]
        ),
        "{planned:?}"
    );
    let mut found = trees.local().get("d").unwrap().clone();
    found.identity = identity(7);
    trees.set_local("d".into(), found);
    let again = effects
        .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
        .first()
        .unwrap();
    assert!(
        matches!(again.effect, Effect::Move { to: Location::At { .. }, identity: moved, .. } if moved == identity(7)),
        "{:?}",
        again.effect
    );
}

#[test]
fn a_blocked_node_can_be_unblocked_by_the_caller() {
    let (mut trees, _) = space(&[("f", Some(file(ROOT, "f", 1)), Some(file(ROOT, "f", 2)))]);
    trees.settle();
    let (mut effects, mut ids) = (Effects::default(), IdGen::new(1));
    let first = effects
        .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
        .first()
        .unwrap()
        .clone();
    effects.failed(&mut trees, &first.effect_id).unwrap();
    assert_eq!(
        effects
            .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
            .first(),
        None
    );
    assert!(effects.unblock("f"));
    assert!(!effects.unblock("f"));
    assert!(effects.blocked().is_empty());
    assert_eq!(
        effects
            .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
            .first()
            .map(|r| &r.effect),
        Some(&first.effect)
    );
}

#[test]
fn a_failed_create_is_planned_again_once_its_parent_has_a_new_identity() {
    let (mut trees, _) = space(&[
        ("d", Some(dir(ROOT, "d")), Some(dir(ROOT, "d"))),
        ("f", None, Some(file("d", "f", 1))),
    ]);
    trees.settle();
    let (mut effects, mut ids) = (Effects::default(), IdGen::new(1));
    let first = effects
        .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
        .first()
        .unwrap()
        .clone();
    effects.failed(&mut trees, &first.effect_id).unwrap();
    let mut parent = trees.local().get("d").unwrap().clone();
    parent.identity = identity(7);
    trees.set_local("d".into(), parent);
    trees.settle();
    assert_eq!(trees.class("f"), Class::Remote);
    let again = effects
        .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
        .first()
        .unwrap();
    assert_eq!(
        again.effect,
        Effect::WriteFile {
            at: Location::At {
                base: Base::Root,
                dirs: vec![Dir {
                    name: "d".into(),
                    identity: identity(7)
                }],
                name: "f".into()
            },
            content: content(1),
            replaces: None
        }
    );
    assert!(effects.blocked().is_empty());
}

#[test]
fn an_edit_observed_while_parked_is_moved_out_and_never_displaced() {
    let (mut trees, mut disk) = space(&[("f", Some(file(ROOT, "a", 1)), Some(file(ROOT, "A", 2)))]);
    trees.settle();
    let (mut effects, mut ids) = (Effects::default(), IdGen::new(1));
    let park = effects
        .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
        .first()
        .unwrap()
        .clone();
    assert!(matches!(
        park.effect,
        Effect::Move {
            to: Location::Parked(_),
            ..
        }
    ));
    disk.apply(&park.effect).unwrap();
    effects
        .done(&mut trees, None, &park.effect_id, None)
        .unwrap();
    let mut observed = trees.local().get("f").unwrap().clone();
    observed.shape.content = Some(content(3));
    disk.entries.get_mut(&1).unwrap().content = Some(content(3));
    trees.set_local("f".into(), observed);
    trees.settle();
    assert_eq!(trees.class("f"), Class::Concurrent);
    let out = effects
        .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
        .first()
        .unwrap()
        .clone();
    assert_eq!(
        out.effect,
        Effect::Move {
            from: Location::Parked("f".into()),
            identity: identity(1),
            to: root_at("A")
        }
    );
    disk.apply(&out.effect).unwrap();
    effects
        .done(&mut trees, None, &out.effect_id, None)
        .unwrap();
    disk.matches(&trees).unwrap();
    assert_eq!(trees.local().get("f").unwrap().shape, file(ROOT, "A", 3));
    trees.settle();
    assert_eq!(trees.class("f"), Class::Concurrent);
    assert_eq!(
        effects
            .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
            .first(),
        None
    );
}

#[test]
fn a_parked_edit_whose_node_was_removed_remotely_goes_back_to_its_place() {
    let (mut trees, mut disk) = space(&[
        ("a", Some(file(ROOT, "x", 1)), Some(file(ROOT, "y", 1))),
        ("b", Some(file(ROOT, "y", 2)), Some(file(ROOT, "x", 2))),
    ]);
    trees.settle();
    let (mut effects, mut ids) = (Effects::default(), IdGen::new(1));
    let park = effects
        .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
        .first()
        .unwrap()
        .clone();
    disk.apply(&park.effect).unwrap();
    effects
        .done(&mut trees, None, &park.effect_id, None)
        .unwrap();
    let mut observed = trees.local().get("a").unwrap().clone();
    observed.shape.content = Some(content(3));
    disk.entries.get_mut(&1).unwrap().content = Some(content(3));
    trees.set_local("a".into(), observed);
    trees.remove_remote("a");
    trees.remove_remote("b");
    trees.settle();
    let out = effects
        .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
        .first()
        .unwrap()
        .clone();
    assert_eq!(
        out.effect,
        Effect::Move {
            from: Location::Parked("a".into()),
            identity: identity(1),
            to: root_at("x")
        }
    );
}

#[test]
fn a_parked_edit_whose_name_the_remote_tree_gives_another_node_stays_parked() {
    let (mut trees, mut disk) = space(&[
        ("d", Some(dir(ROOT, "x")), None),
        ("k", Some(file("d", "k", 1)), None),
        ("a", Some(file(ROOT, "y", 1)), Some(file(ROOT, "x", 1))),
    ]);
    let trashed = |shape| RemoteNode {
        trashed: true,
        ..remote(shape)
    };
    trees.set_remote("d".into(), trashed(dir(ROOT, "x")));
    trees.set_remote("k".into(), trashed(file("d", "k", 1)));
    let edit = |trees: &mut Trees, disk: &mut Disk, id: &str, ino: u128| {
        let mut node = trees.local().get(id).unwrap().clone();
        node.shape.content = Some(content(9));
        disk.entries.get_mut(&ino).unwrap().content = Some(content(9));
        trees.set_local(id.into(), node);
    };
    edit(&mut trees, &mut disk, "k", 2);
    converge(&mut trees, &mut disk, 8).unwrap();
    assert!(trees.waiting().contains_key("a"));
    edit(&mut trees, &mut disk, "a", 3);
    trees.set_remote("a".into(), trashed(file(ROOT, "x", 1)));
    trees.set_remote("n".into(), remote(dir(ROOT, "x")));
    let done = converge(&mut trees, &mut disk, 8).unwrap();
    assert_eq!(
        done,
        [
            Effect::Move {
                from: root_at("x"),
                identity: identity(3),
                to: Location::Parked("a".into()),
            },
            Effect::CreateDir { at: root_at("x") },
        ]
    );
    assert!(trees.local().get("a").unwrap().parked);
}

#[test]
fn a_parked_edit_that_the_remote_tree_matches_is_still_moved_out() {
    let (mut trees, mut disk) = space(&[("f", Some(file(ROOT, "a", 1)), Some(file(ROOT, "A", 2)))]);
    trees.settle();
    let (mut effects, mut ids) = (Effects::default(), IdGen::new(1));
    let park = effects
        .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
        .first()
        .unwrap()
        .clone();
    disk.apply(&park.effect).unwrap();
    effects
        .done(&mut trees, None, &park.effect_id, None)
        .unwrap();
    let mut observed = trees.local().get("f").unwrap().clone();
    observed.shape.content = Some(content(3));
    disk.entries.get_mut(&1).unwrap().content = Some(content(3));
    trees.set_local("f".into(), observed);
    trees.set_remote("f".into(), remote(file(ROOT, "a", 3)));
    assert_eq!(trees.class("f"), Class::Same);
    trees.settle();
    assert_eq!(trees.class("f"), Class::InSync);
    assert!(trees.dirty().contains("f"));
    let out = effects
        .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
        .first()
        .unwrap()
        .clone();
    assert_eq!(
        out.effect,
        Effect::Move {
            from: Location::Parked("f".into()),
            identity: identity(1),
            to: root_at("a")
        }
    );
    disk.apply(&out.effect).unwrap();
    effects
        .done(&mut trees, None, &out.effect_id, None)
        .unwrap();
    disk.matches(&trees).unwrap();
    assert_in_sync(&mut trees);
}

#[test]
fn done_takes_only_the_effect_in_flight_and_a_created_entry_needs_its_identity() {
    let (mut trees, _) = space(&[("d", None, Some(dir(ROOT, "d")))]);
    trees.settle();
    let (mut effects, mut ids) = (Effects::default(), IdGen::new(1));
    let id = effects
        .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
        .first()
        .unwrap()
        .effect_id
        .clone();
    assert_eq!(
        effects.done(&mut trees, None, "other", Some(identity(5))),
        Err(EffectError::NotInFlight("other".into()))
    );
    assert_eq!(
        effects.done(&mut trees, None, &id, None),
        Err(EffectError::MissingIdentity)
    );
    assert!(!effects.in_flight().is_empty());
    assert_eq!(
        effects.done(&mut trees, None, &id, Some(identity(5))),
        Ok(None)
    );
    assert_eq!(trees.local().get("d").unwrap().identity, identity(5));
    assert_eq!(effects.cleared(), 1);
    assert_in_sync(&mut trees);
}

/// GUN-337. A download of `a` is in flight when a scan puts another node at `a`. L does not take
/// the download's outcome, so `done` refuses it and L keeps one entry named `a`. The effect stays
/// in flight for the caller to fail.
#[test]
fn done_refuses_an_outcome_whose_name_another_node_took() {
    let (mut trees, _) = space(&[("n", None, Some(file(ROOT, "a", 1)))]);
    trees.settle();
    let (mut effects, mut ids) = (Effects::default(), IdGen::new(1));
    let record = effects
        .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
        .first()
        .cloned()
        .unwrap();
    assert!(record.applies(&trees, None));
    trees.set_local(
        "u".into(),
        LocalNode {
            shape: file(ROOT, "a", 2),
            identity: identity(7),
            parked: false,
        },
    );
    assert!(!record.applies(&trees, None));
    assert_eq!(
        effects.done(&mut trees, None, &record.effect_id, Some(identity(5))),
        Err(EffectError::DoesNotApply(record.effect_id.clone()))
    );
    assert_eq!(effects.in_flight().first(), Some(&record));
    assert!(trees.local().get("n").is_none());
    assert_eq!(trees.check(), Ok(()));
}

/// One node's draw for one tree: ⊥, or its rank, a parent choice and a name.
type Draw = Option<(u8, u8, u8)>;
/// GUN-334. The server trashed `f`, and its removal was in flight when another device's commit
/// restored it (R8). The removal is the engine's own, not the user's, so `f` is downloaded again
/// and no Delete is sent for it.
#[test]
fn a_file_an_effect_removes_after_the_server_restored_it_is_downloaded_again() {
    let (mut trees, mut disk) = space(&[("f", Some(file(ROOT, "f", 1)), None)]);
    trees.settle();
    let (mut effects, mut ids) = (Effects::default(), IdGen::new(1));
    let record = effects
        .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
        .first()
        .unwrap()
        .clone();
    assert!(matches!(record.effect, Effect::RemoveFile { .. }));

    trees.set_remote("f".into(), remote(file(ROOT, "f", 1)));
    let created = disk.apply(&record.effect).unwrap();
    effects
        .done(&mut trees, None, &record.effect_id, created)
        .unwrap();
    trees.check().unwrap();
    trees.settle();
    assert_eq!(trees.class("f"), Class::Remote);

    let done = converge(&mut trees, &mut disk, 10).unwrap();
    assert!(matches!(done[..], [Effect::WriteFile { .. }]), "{done:?}");
    assert_in_sync(&mut trees);
}

/// GUN-349. A move to R's position is in flight when R moves the file back. The move is this
/// device's effect, not the user's, so no commit moves the file, and the effects move it back.
#[test]
fn a_move_r_undid_while_it_was_in_flight_is_moved_back() {
    let (mut trees, mut disk) = space(&[("f", Some(file(ROOT, "a", 1)), Some(file(ROOT, "b", 1)))]);
    trees.settle();
    let (mut effects, mut ids) = (Effects::default(), IdGen::new(1));
    let record = effects
        .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
        .first()
        .unwrap()
        .clone();
    assert!(matches!(record.effect, Effect::Move { .. }));

    trees.set_remote("f".into(), remote(file(ROOT, "a", 1)));
    let created = disk.apply(&record.effect).unwrap();
    effects
        .done(&mut trees, None, &record.effect_id, created)
        .unwrap();
    trees.check().unwrap();
    trees.settle();
    assert_eq!(trees.class("f"), Class::Remote);

    let done = converge(&mut trees, &mut disk, 10).unwrap();
    assert!(matches!(done[..], [Effect::Move { .. }]), "{done:?}");
    assert_in_sync(&mut trees);
}

/// A move to R's position is in flight when R moves the file back with new content, and the
/// user's copy reaches the same content, so S takes it. The move changes only the position, so
/// L and S keep that content, and the effects move the file back without a conflict.
#[test]
fn a_move_r_undid_with_new_content_while_it_was_in_flight_keeps_the_new_content() {
    let (mut trees, mut disk) = space(&[("f", Some(file(ROOT, "a", 1)), Some(file(ROOT, "b", 1)))]);
    trees.settle();
    let (mut effects, mut ids) = (Effects::default(), IdGen::new(1));
    let record = effects
        .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
        .first()
        .unwrap()
        .clone();
    assert!(matches!(record.effect, Effect::Move { .. }));

    trees.set_remote("f".into(), remote(file(ROOT, "a", 2)));
    let mut local = trees.local().get("f").unwrap().clone();
    local.shape.content = Some(content(2));
    trees.set_local("f".into(), local);
    disk.entries.get_mut(&1).unwrap().content = Some(content(2));
    trees.settle();
    assert_eq!(trees.synced().live("f"), Some(&file(ROOT, "a", 2)));
    let created = disk.apply(&record.effect).unwrap();
    effects
        .done(&mut trees, None, &record.effect_id, created)
        .unwrap();
    assert_eq!(trees.synced().live("f"), Some(&file(ROOT, "b", 2)));
    assert_eq!(trees.local().live("f"), Some(&file(ROOT, "b", 2)));
    trees.check().unwrap();
    trees.settle();
    assert_eq!(trees.class("f"), Class::Remote);

    let done = converge(&mut trees, &mut disk, 10).unwrap();
    assert!(matches!(done[..], [Effect::Move { .. }]), "{done:?}");
    assert_in_sync(&mut trees);
}

/// GUN-349. The user edited f while a move to R's position was in flight, and R moved f back.
/// The move still was this device's, so only the edit is committed, and f keeps its name.
#[test]
fn an_edit_made_while_a_move_r_undid_was_in_flight_is_uploaded_without_the_move() {
    let (mut trees, mut disk) = space(&[("f", Some(file(ROOT, "a", 1)), Some(file(ROOT, "b", 1)))]);
    trees.settle();
    let (mut effects, mut ids) = (Effects::default(), IdGen::new(1));
    let record = effects
        .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
        .first()
        .unwrap()
        .clone();
    assert!(matches!(record.effect, Effect::Move { .. }));

    trees.set_remote("f".into(), remote(file(ROOT, "a", 1)));
    let mut local = trees.local().get("f").unwrap().clone();
    local.shape.content = Some(content(2));
    trees.set_local("f".into(), local);
    disk.entries.get_mut(&1).unwrap().content = Some(content(2));
    trees.settle();
    let created = disk.apply(&record.effect).unwrap();
    effects
        .done(&mut trees, None, &record.effect_id, created)
        .unwrap();
    trees.settle();
    assert_eq!(trees.local().live("f"), Some(&file(ROOT, "b", 2)));
    assert_eq!(trees.synced().live("f"), Some(&file(ROOT, "b", 1)));
    let mut commits = koidrive_engine::commit::Commits::default();
    let commit = commits
        .build(&mut trees, &mut ids, NOW, "history", "me", 1)
        .unwrap();
    assert!(
        !(commit.ops.iter()).any(|op| matches!(&op.op, Some(koidrive_proto::v1::op::Op::Move(_)))),
        "{:?}",
        commit.ops
    );
    assert!(commit.ops.iter().any(|op| matches!(&op.op,
        Some(koidrive_proto::v1::op::Op::UpdateContent(update)) if update.node_id == "f" && update.content == Some(content(2)))),
        "{:?}", commit.ops);
}

/// The server renamed f while the user edited it, so its position is reverted with the edit
/// unsent. The move to the server's name keeps the edit, which is uploaded.
#[test]
fn a_reverted_move_keeps_an_edit_that_was_not_uploaded() {
    let (mut trees, mut disk) = space(&[("f", Some(file(ROOT, "a", 1)), Some(file(ROOT, "b", 1)))]);
    let mut local = trees.local().get("f").unwrap().clone();
    local.shape.content = Some(content(2));
    trees.set_local("f".into(), local);
    disk.entries.get_mut(&1).unwrap().content = Some(content(2));
    trees.revert("f".into());
    trees.settle();
    let (mut effects, mut ids) = (Effects::default(), IdGen::new(1));
    let record = effects
        .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
        .first()
        .unwrap()
        .clone();
    assert!(matches!(record.effect, Effect::Move { .. }));
    let created = disk.apply(&record.effect).unwrap();
    effects
        .done(&mut trees, None, &record.effect_id, created)
        .unwrap();
    trees.settle();
    assert!(trees.edited("f"));
    assert_eq!(trees.local().live("f").unwrap().content, Some(content(2)));
    let mut commits = koidrive_engine::commit::Commits::default();
    let commit = commits
        .build(&mut trees, &mut ids, NOW, "history", "me", 1)
        .unwrap();
    assert!(commit.ops.iter().any(|op| matches!(&op.op,
        Some(koidrive_proto::v1::op::Op::UpdateContent(update)) if update.node_id == "f" && update.content == Some(content(2)))),
        "{:?}", commit.ops);
}

/// GUN-332. The server renamed a file this device created, so its local position is reverted.
/// The user deleted the file before an effect moved it to the server's name. That delete is the
/// user's to commit, and no effect brings the file back.
#[test]
fn a_reverted_file_that_the_user_deleted_is_not_downloaded_again() {
    let renamed = file(ROOT, "a (conflict)", 1);
    let (mut trees, _) = space(&[
        ("d", Some(dir(ROOT, "a")), Some(dir(ROOT, "a"))),
        ("n", Some(renamed.clone()), Some(renamed)),
    ]);
    trees.revert("n".into());
    trees.remove_local("n");
    trees.settle();
    assert_eq!(trees.class("n"), Class::Local);
    let (mut effects, mut ids) = (Effects::default(), IdGen::new(1));
    assert_eq!(
        effects
            .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
            .first(),
        None
    );
}

/// A parked file holds an edit, and the user deleted its folder, which the server keeps. When R
/// trashes the file, the folder is created again so the file goes back to its local position and
/// the edit is uploaded.
#[test]
fn a_parked_edit_brings_back_its_deleted_folder_after_a_remote_trash() {
    let (mut trees, mut disk) = space(&[
        ("d", Some(dir(ROOT, "d")), Some(dir(ROOT, "d"))),
        ("f", Some(file("d", "f", 1)), None),
    ]);
    let mut trashed = remote(file("d", "f", 1));
    trashed.trashed = true;
    trashed.trash_group = "other-device".into();
    trees.set_remote("f".into(), trashed);
    let mut local = trees.local().get("f").unwrap().clone();
    local.parked = true;
    local.shape.content = Some(content(2));
    trees.set_local("f".into(), local);
    trees.remove_local("d");
    trees.revert("d".into());
    disk.entries.remove(&1);
    let entry = disk.entries.get_mut(&2).unwrap();
    entry.area = Area::Parked;
    entry.name = "f".into();
    entry.content = Some(content(2));
    trees.check().unwrap();
    disk.matches(&trees).unwrap();
    let done = converge(&mut trees, &mut disk, 10).unwrap();
    assert!(
        matches!(
            done.as_slice(),
            [Effect::CreateDir { .. }, Effect::Move { .. }]
        ),
        "{done:?}"
    );
    assert!(!trees.local().get("f").unwrap().parked);
    assert_eq!(trees.local().live("f").unwrap().content, Some(content(2)));
    assert!(trees.edited("f"));
    let (mut commits, mut ids) = (koidrive_engine::commit::Commits::default(), IdGen::new(1));
    let commit = commits
        .build(&mut trees, &mut ids, NOW, "history", "me", 7)
        .unwrap();
    assert!(commit.ops.iter().any(|op| match &op.op {
        Some(koidrive_proto::v1::op::Op::UpdateContent(op)) => op.content == Some(content(2)),
        Some(koidrive_proto::v1::op::Op::CreateFile(op)) => op.content == Some(content(2)),
        _ => false,
    }));
}

/// As above, but R moves the file into a folder the rules leave out instead of trashing it. A
/// parked node that is ignored goes back to its local position, so its deleted folder is created
/// again for it there too.
#[test]
fn a_parked_edit_brings_back_its_deleted_folder_after_a_remote_move_into_an_ignored_folder() {
    let (mut trees, mut disk) = space(&[
        ("d", Some(dir(ROOT, "d")), Some(dir(ROOT, "d"))),
        ("logs", Some(dir(ROOT, "logs")), Some(dir(ROOT, "logs"))),
        ("f", Some(file("d", "f", 1)), Some(file("logs", "f", 1))),
    ]);
    trees.set_rules(
        &[] as &[&str],
        Some(koidrive_engine::ignore::RuleFile::parse(b"logs/\n")),
    );
    let mut local = trees.local().get("f").unwrap().clone();
    local.parked = true;
    local.shape.content = Some(content(2));
    trees.set_local("f".into(), local);
    trees.remove_local("d");
    trees.revert("d".into());
    disk.entries.remove(&1);
    let entry = disk.entries.get_mut(&3).unwrap();
    entry.area = Area::Parked;
    entry.name = "f".into();
    entry.content = Some(content(2));
    trees.check().unwrap();
    disk.matches(&trees).unwrap();
    assert!(trees.ignored("f"));
    let done = converge(&mut trees, &mut disk, 10).unwrap();
    assert!(
        matches!(
            done.as_slice(),
            [Effect::CreateDir { .. }, Effect::Move { .. }]
        ),
        "{done:?}"
    );
    let f = trees.local().get("f").unwrap();
    assert!(!f.parked);
    assert_eq!(f.shape, file("d", "f", 2));
}

#[test]
fn a_reverted_folder_whose_r_folder_sits_inside_it_waits_in_place() {
    for rules in [&b""[..], b"b/\n"] {
        let (mut trees, mut disk) = space(&[
            ("y", Some(dir(ROOT, "b")), Some(dir(ROOT, "b"))),
            ("x", Some(dir("y", "c")), Some(dir("y", "c"))),
        ]);
        trees.set_rules(
            &[] as &[&str],
            Some(koidrive_engine::ignore::RuleFile::parse(rules)),
        );
        let mut x = trees.local().get("x").unwrap().clone();
        x.shape = dir(ROOT, "x");
        trees.set_local("x".into(), x);
        let mut y = trees.local().get("y").unwrap().clone();
        y.shape = dir("x", "a");
        trees.set_local("y".into(), y);
        trees.revert("x".into());
        disk.entries.insert(
            2,
            Entry {
                area: Area::Root,
                name: "x".into(),
                kind: Kind::Dir,
                content: None,
            },
        );
        disk.entries.insert(
            1,
            Entry {
                area: Area::Dir(2),
                name: "a".into(),
                kind: Kind::Dir,
                content: None,
            },
        );
        trees.check().unwrap();
        disk.matches(&trees).unwrap();
        let done = converge(&mut trees, &mut disk, 10).unwrap();
        assert!(done.is_empty(), "{done:?}");
        assert!(!trees.local().get("x").unwrap().parked);
    }
}

#[test]
fn a_reverted_folder_whose_inner_folder_an_excluded_node_holds_out_waits_in_place() {
    let (mut trees, mut disk) = space(&[
        ("e", Some(dir(ROOT, "e")), Some(dir(ROOT, "e"))),
        ("y", Some(dir(ROOT, "b")), Some(dir(ROOT, "b"))),
        ("x", Some(dir("y", "c")), Some(dir("y", "c"))),
    ]);
    trees.set_rules(
        &[] as &[&str],
        Some(koidrive_engine::ignore::RuleFile::parse(b"b/\n")),
    );
    for (id, shape, ino, area, name) in [
        ("e", dir(ROOT, "b"), 1, Area::Root, "b"),
        ("x", dir(ROOT, "x"), 3, Area::Root, "x"),
        ("y", dir("x", "a"), 2, Area::Dir(3), "a"),
    ] {
        let mut local = trees.local().get(id).unwrap().clone();
        local.shape = shape;
        trees.set_local(id.into(), local);
        let entry = disk.entries.get_mut(&ino).unwrap();
        entry.area = area;
        entry.name = name.into();
    }
    trees.revert("x".into());
    trees.revert("y".into());
    trees.check().unwrap();
    disk.matches(&trees).unwrap();
    assert!(trees.excluded("e") && !trees.excluded("x") && !trees.excluded("y"));
    let done = converge(&mut trees, &mut disk, 10).unwrap();
    assert!(done.is_empty(), "{done:?}");
    assert!(!trees.local().get("x").unwrap().parked);
}

#[test]
fn a_name_holder_whose_s_position_the_rules_leave_out_reaches_its_r_position() {
    let (mut trees, mut disk) = space(&[
        ("y", Some(dir(ROOT, "b")), Some(dir(ROOT, "x"))),
        ("x", Some(dir("y", "c")), Some(dir("y", "c"))),
        ("z", Some(file("x", "z", 1)), Some(file("x", "z", 1))),
        ("e", Some(dir(ROOT, "x")), Some(dir(ROOT, "h"))),
    ]);
    trees.set_rules(
        &[] as &[&str],
        Some(koidrive_engine::ignore::RuleFile::parse(b"b/\n")),
    );
    for (id, shape, ino, area, name) in [
        ("x", dir(ROOT, "x"), 2, Area::Root, "x"),
        ("y", dir("x", "a"), 1, Area::Dir(2), "a"),
        ("z", file(ROOT, "z", 1), 3, Area::Root, "z"),
        ("e", dir(ROOT, "g"), 4, Area::Root, "g"),
    ] {
        let mut local = trees.local().get(id).unwrap().clone();
        local.shape = shape;
        trees.set_local(id.into(), local);
        let entry = disk.entries.get_mut(&ino).unwrap();
        entry.area = area;
        entry.name = name.into();
    }
    trees.revert("x".into());
    trees.revert("y".into());
    trees.check().unwrap();
    disk.matches(&trees).unwrap();
    converge(&mut trees, &mut disk, 10).unwrap();
    assert!(trees.local().iter().all(|(_, node)| !node.parked));
    assert_eq!(trees.local().get("x").unwrap().shape, dir("y", "c"));
    assert_eq!(trees.local().get("y").unwrap().shape, dir(ROOT, "x"));
}

#[test]
fn a_reverted_parked_file_whose_r_name_an_excluded_node_holds_goes_back_to_its_local_position() {
    let (mut trees, mut disk) = space(&[
        ("e", Some(file(ROOT, "z", 9)), Some(file(ROOT, "z", 9))),
        ("x", Some(file(ROOT, "x", 1)), Some(file(ROOT, "A", 1))),
    ]);
    trees.set_rules(
        &[] as &[&str],
        Some(koidrive_engine::ignore::RuleFile::parse(b"a\n")),
    );
    let mut moved = trees.local().get("e").unwrap().clone();
    moved.shape = file(ROOT, "a", 9);
    trees.set_local("e".into(), moved);
    disk.entries.get_mut(&1).unwrap().name = "a".into();
    let mut local = trees.local().get("x").unwrap().clone();
    local.parked = true;
    local.shape = file(ROOT, "y", 1);
    trees.set_local("x".into(), local);
    trees.revert("x".into());
    let entry = disk.entries.get_mut(&2).unwrap();
    entry.area = Area::Parked;
    entry.name = "x".into();
    trees.check().unwrap();
    disk.matches(&trees).unwrap();
    assert!(trees.excluded("e"));
    converge(&mut trees, &mut disk, 10).unwrap();
    let x = trees.local().get("x").unwrap();
    assert!(!x.parked);
    assert_eq!(x.shape, file(ROOT, "y", 1));
}

#[test]
fn a_reverted_name_cycle_out_of_an_excluded_folder_reaches_its_r_positions() {
    let (mut trees, mut disk) = space(&[
        ("y", Some(dir(ROOT, "b")), Some(dir(ROOT, "x"))),
        ("x", Some(dir("y", "c")), Some(dir("y", "c"))),
    ]);
    trees.set_rules(
        &[] as &[&str],
        Some(koidrive_engine::ignore::RuleFile::parse(b"b/\n")),
    );
    for (id, shape, ino, area, name) in [
        ("x", dir(ROOT, "x"), 2, Area::Root, "x"),
        ("y", dir("x", "a"), 1, Area::Dir(2), "a"),
    ] {
        let mut local = trees.local().get(id).unwrap().clone();
        local.shape = shape;
        trees.set_local(id.into(), local);
        let entry = disk.entries.get_mut(&ino).unwrap();
        entry.area = area;
        entry.name = name.into();
    }
    trees.revert("x".into());
    trees.revert("y".into());
    trees.check().unwrap();
    disk.matches(&trees).unwrap();
    converge(&mut trees, &mut disk, 10).unwrap();
    assert!(trees.local().iter().all(|(_, node)| !node.parked));
    assert_eq!(trees.local().get("x").unwrap().shape, dir("y", "c"));
    assert_eq!(trees.local().get("y").unwrap().shape, dir(ROOT, "x"));
}

#[test]
fn a_reverted_case_only_rename_out_of_an_excluded_folder_reaches_its_r_name() {
    let (mut trees, mut disk) = space(&[
        ("x", Some(dir(ROOT, "b")), Some(dir(ROOT, "a"))),
        ("z", Some(file("x", "z", 1)), Some(file("x", "z", 1))),
        ("e", Some(dir(ROOT, "a")), Some(dir(ROOT, "h"))),
    ]);
    trees.set_rules(
        &[] as &[&str],
        Some(koidrive_engine::ignore::RuleFile::parse(b"b/\n")),
    );
    for (id, shape, ino, name) in [
        ("x", dir(ROOT, "A"), 1, "A"),
        ("z", file(ROOT, "z", 1), 2, "z"),
        ("e", dir(ROOT, "g"), 3, "g"),
    ] {
        let mut local = trees.local().get(id).unwrap().clone();
        local.shape = shape;
        trees.set_local(id.into(), local);
        let entry = disk.entries.get_mut(&ino).unwrap();
        entry.area = Area::Root;
        entry.name = name.into();
    }
    trees.revert("x".into());
    trees.check().unwrap();
    disk.matches(&trees).unwrap();
    converge(&mut trees, &mut disk, 10).unwrap();
    assert!(trees.local().iter().all(|(_, node)| !node.parked));
    assert_eq!(trees.local().get("x").unwrap().shape, dir(ROOT, "a"));
}

#[test]
fn two_reverted_files_moved_out_of_an_excluded_folder_swap_names_into_their_r_positions() {
    let (mut trees, mut disk) = space(&[
        ("logs", Some(dir(ROOT, "logs")), Some(dir(ROOT, "logs"))),
        ("a", Some(file("logs", "a", 1)), Some(file(ROOT, "q", 1))),
        ("b", Some(file("logs", "b", 2)), Some(file(ROOT, "p", 2))),
    ]);
    trees.set_rules(
        &[] as &[&str],
        Some(koidrive_engine::ignore::RuleFile::parse(b"logs/\n")),
    );
    for (id, name, ino) in [("a", "p", 2), ("b", "q", 3)] {
        let mut local = trees.local().get(id).unwrap().clone();
        local.shape = file(ROOT, name, if id == "a" { 1 } else { 2 });
        trees.set_local(id.into(), local);
        let entry = disk.entries.get_mut(&ino).unwrap();
        entry.area = Area::Root;
        entry.name = name.into();
    }
    trees.revert("a".into());
    trees.revert("b".into());
    trees.check().unwrap();
    disk.matches(&trees).unwrap();
    assert!(!trees.excluded("a") && !trees.excluded("b"));
    converge(&mut trees, &mut disk, 20).unwrap();
    assert!(trees.local().iter().all(|(_, node)| !node.parked));
    assert_eq!(trees.local().get("a").unwrap().shape, file(ROOT, "q", 1));
    assert_eq!(trees.local().get("b").unwrap().shape, file(ROOT, "p", 2));
}

#[test]
fn a_reverted_parked_file_whose_r_name_an_excluded_node_holds_brings_back_its_deleted_folder() {
    let (mut trees, mut disk) = space(&[
        ("logs", Some(dir(ROOT, "logs")), Some(dir(ROOT, "logs"))),
        ("e", Some(file(ROOT, "z", 9)), Some(file("logs", "z", 9))),
        ("d", Some(dir(ROOT, "d")), Some(dir(ROOT, "d"))),
        ("x", Some(file(ROOT, "x", 1)), Some(file(ROOT, "z", 1))),
    ]);
    trees.set_rules(
        &[] as &[&str],
        Some(koidrive_engine::ignore::RuleFile::parse(b"logs/\n")),
    );
    let mut local = trees.local().get("x").unwrap().clone();
    local.parked = true;
    local.shape = file("d", "y", 1);
    trees.set_local("x".into(), local);
    trees.revert("x".into());
    trees.remove_local("d");
    trees.revert("d".into());
    disk.entries.remove(&3);
    let entry = disk.entries.get_mut(&4).unwrap();
    entry.area = Area::Parked;
    entry.name = "x".into();
    trees.check().unwrap();
    disk.matches(&trees).unwrap();
    assert!(trees.excluded("e") && !trees.ignored("x"));
    converge(&mut trees, &mut disk, 10).unwrap();
    let x = trees.local().get("x").unwrap();
    assert!(!x.parked);
    assert_eq!(x.shape, file("d", "y", 1));
}

#[test]
fn a_reverted_parked_edit_whose_r_folder_the_rules_exclude_stays_at_its_local_position() {
    let (mut trees, mut disk) = space(&[
        ("logs", Some(dir(ROOT, "logs")), Some(dir(ROOT, "logs"))),
        ("f", Some(file(ROOT, "b", 1)), Some(file("logs", "f", 1))),
    ]);
    trees.set_rules(
        &[] as &[&str],
        Some(koidrive_engine::ignore::RuleFile::parse(b"logs/\n")),
    );
    let mut f = trees.local().get("f").unwrap().clone();
    f.shape = file(ROOT, "a", 2);
    f.parked = true;
    trees.set_local("f".into(), f);
    trees.revert("f".into());
    let entry = disk.entries.get_mut(&2).unwrap();
    entry.area = Area::Parked;
    entry.name = "f".into();
    entry.content = Some(content(2));
    trees.check().unwrap();
    disk.matches(&trees).unwrap();
    assert!(!trees.ignored("f") && trees.edited("f"));
    converge(&mut trees, &mut disk, 10).unwrap();
    let f = trees.local().get("f").unwrap();
    assert!(!f.parked);
    assert_eq!(f.shape, file(ROOT, "a", 2));
    let (mut commits, mut ids) = (koidrive_engine::commit::Commits::default(), IdGen::new(1));
    let commit = commits
        .build(&mut trees, &mut ids, NOW, "history", "me", 7)
        .unwrap();
    assert!(commit.ops.iter().any(|op| match &op.op {
        Some(koidrive_proto::v1::op::Op::UpdateContent(op)) => op.content == Some(content(2)),
        Some(koidrive_proto::v1::op::Op::CreateFile(op)) => op.content == Some(content(2)),
        _ => false,
    }));
}

#[test]
fn an_edit_whose_r_folder_the_rules_exclude_is_not_parked_to_free_its_name() {
    let (mut trees, mut disk) = space(&[
        ("logs", Some(dir(ROOT, "logs")), Some(dir(ROOT, "logs"))),
        ("f", Some(file(ROOT, "b", 1)), Some(file("logs", "f", 1))),
        ("g", Some(file(ROOT, "x", 3)), Some(file(ROOT, "a", 3))),
    ]);
    trees.set_rules(
        &[] as &[&str],
        Some(koidrive_engine::ignore::RuleFile::parse(b"logs/\n")),
    );
    let mut f = trees.local().get("f").unwrap().clone();
    f.shape = file(ROOT, "a", 2);
    trees.set_local("f".into(), f);
    trees.revert("f".into());
    let entry = disk.entries.get_mut(&2).unwrap();
    entry.name = "a".into();
    entry.content = Some(content(2));
    trees.check().unwrap();
    disk.matches(&trees).unwrap();
    let done = converge(&mut trees, &mut disk, 10).unwrap();
    assert!(done.is_empty(), "{done:?}");
    assert_eq!(trees.local().get("f").unwrap().shape, file(ROOT, "a", 2));
    let (mut commits, mut ids) = (koidrive_engine::commit::Commits::default(), IdGen::new(1));
    let commit = commits
        .build(&mut trees, &mut ids, NOW, "history", "me", 7)
        .unwrap();
    assert!(commit.ops.iter().any(|op| match &op.op {
        Some(koidrive_proto::v1::op::Op::UpdateContent(op)) => op.content == Some(content(2)),
        Some(koidrive_proto::v1::op::Op::CreateFile(op)) => op.content == Some(content(2)),
        _ => false,
    }));
}

#[test]
fn a_file_parked_for_a_name_swap_is_not_reported_while_the_swap_goes_on() {
    let (mut trees, mut disk) = space(&[
        ("a", Some(file(ROOT, "x", 1)), Some(file(ROOT, "y", 1))),
        ("b", Some(file(ROOT, "y", 2)), Some(file(ROOT, "x", 2))),
    ]);
    trees.settle();
    let (mut effects, mut ids) = (Effects::default(), IdGen::new(1));
    let park = effects
        .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
        .first()
        .unwrap()
        .clone();
    assert!(matches!(
        park.effect,
        Effect::Move {
            to: Location::Parked(_),
            ..
        }
    ));
    disk.apply(&park.effect).unwrap();
    effects
        .done(&mut trees, None, &park.effect_id, None)
        .unwrap();
    trees.settle();
    assert_eq!(
        koidrive_engine::effects::stuck(&trees, &effects, None),
        BTreeMap::new()
    );
}

#[test]
fn an_ignored_parked_file_whose_local_name_an_excluded_folder_holds_goes_into_it_as_r_has_it() {
    let (mut trees, mut disk) = space(&[
        ("e", Some(dir(ROOT, "A")), Some(dir(ROOT, "A"))),
        ("p", Some(file("e", "A", 1)), Some(file("e", "A", 1))),
    ]);
    trees.set_rules(
        &[] as &[&str],
        Some(koidrive_engine::ignore::RuleFile::parse(b"A\n")),
    );
    let mut local = trees.local().get("p").unwrap().clone();
    local.parked = true;
    local.shape = file(ROOT, "A", 1);
    trees.set_local("p".into(), local);
    trees.revert("p".into());
    let entry = disk.entries.get_mut(&2).unwrap();
    entry.area = Area::Parked;
    entry.name = "p".into();
    trees.check().unwrap();
    disk.matches(&trees).unwrap();
    assert!(trees.excluded("e") && trees.ignored("p"));
    converge(&mut trees, &mut disk, 10).unwrap();
    let p = trees.local().get("p").unwrap();
    assert!(!p.parked);
    assert_eq!(p.shape, file("e", "A", 1));
}

#[test]
fn a_parked_node_that_can_go_nowhere_is_reported_beside_a_failed_effect() {
    let (mut trees, mut disk) = space(&[
        ("h", Some(file(ROOT, "a", 1)), Some(file(ROOT, "a", 1))),
        ("far", None, Some(dir(ROOT, "archive"))),
        ("f", Some(file(ROOT, "f", 1)), Some(file("far", "f", 1))),
        ("g", None, Some(file(ROOT, "g", 1))),
    ]);
    trees.set_rules(
        &[] as &[&str],
        Some(koidrive_engine::ignore::RuleFile::parse(b"archive/\n")),
    );
    let mut f = trees.local().get("f").unwrap().clone();
    f.shape = file(ROOT, "a", 1);
    f.parked = true;
    trees.set_local("f".into(), f);
    let entry = disk.entries.get_mut(&3).unwrap();
    entry.area = Area::Parked;
    entry.name = "f".into();
    trees.check().unwrap();
    disk.matches(&trees).unwrap();
    trees.settle();
    let (mut effects, mut ids) = (Effects::default(), IdGen::new(1));
    let record = effects
        .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
        .first()
        .unwrap()
        .clone();
    assert_eq!(record.node, "g");
    effects.failed(&mut trees, &record.effect_id).unwrap();
    assert!(
        effects
            .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
            .is_empty()
    );
    let reported = koidrive_engine::effects::stuck(&trees, &effects, None);
    assert!(reported.contains_key("f"), "{reported:?}");
}

/// A valid tree over `n0`, `n1`, ... from one [`Draw`] per node, as in tests/trees.rs, where each
/// node has the kind and file content of `kinds`. S and R are drawn with their own ranks, so their
/// moves can swap, rotate and nest into each other.
fn draw_tree(draws: &[Draw], kinds: &[(bool, u8)]) -> Vec<Option<Shape>> {
    let mut order: Vec<usize> = (0..draws.len()).collect();
    order.sort_by_key(|&i| (draws[i].map(|draw| draw.0), i));
    let mut out: Vec<Option<Shape>> = vec![None; draws.len()];
    let mut placed: Vec<usize> = Vec::new();
    for i in order {
        let Some((_, parent, name)) = draws[i] else {
            continue;
        };
        let parent = placed
            .get(parent as usize % (placed.len() + 1))
            .filter(|&&p| kinds[p].0)
            .map_or(ROOT.to_owned(), |p| format!("n{p}"));
        let mut name = ["x", "X", "y"][name as usize].to_owned();
        if out
            .iter()
            .flatten()
            .any(|shape| shape.parent == parent && name_key(&shape.name) == name_key(&name))
        {
            name = format!("u{i}");
        }
        out[i] = Some(if kinds[i].0 {
            dir(&parent, &name)
        } else {
            file(&parent, &name, kinds[i].1)
        });
        placed.push(i);
    }
    out
}

fn draws() -> impl Strategy<Value = Draw> {
    prop_oneof![
        1 => Just(None),
        5 => (any::<u8>(), any::<u8>(), 0u8..3).prop_map(Some),
    ]
}

/// Whether the node is a directory, its file content in S and in R, and its draws for S and R.
type Node = (bool, u8, u8, Draw, Draw);

fn spaces() -> impl Strategy<Value = Vec<Node>> {
    proptest::collection::vec(
        (
            proptest::bool::weighted(0.6),
            0u8..2,
            0u8..2,
            draws(),
            draws(),
        ),
        1..8,
    )
}

proptest! {
    #![proptest_config(koidrive_test_support::regression_config(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/effects.proptest-regressions"),
        2048,
    ))]

    /// From any S = L and any R, doing the planned effects in order on a disk that never replaces
    /// an entry ends with L = S = R, nothing parked and the disk equal to L, within three effects
    /// per node. Every effect is checked against the disk as it is done.
    #[test]
    fn effects_bring_the_disk_to_the_remote_tree(nodes in spaces()) {
        let in_s: Vec<(bool, u8)> = nodes.iter().map(|n| (n.0, n.1)).collect();
        let in_r: Vec<(bool, u8)> = nodes.iter().map(|n| (n.0, n.2)).collect();
        let s = draw_tree(&nodes.iter().map(|n| n.3).collect::<Vec<_>>(), &in_s);
        let r = draw_tree(&nodes.iter().map(|n| n.4).collect::<Vec<_>>(), &in_r);
        let ids: Vec<String> = (0..nodes.len()).map(|i| format!("n{i}")).collect();
        let spec: Vec<(&str, Option<Shape>, Option<Shape>)> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| (id.as_str(), s[i].clone(), r[i].clone()))
            .collect();
        let (mut trees, mut disk) = space(&spec);
        prop_assert_eq!(trees.check(), Ok(()));
        let result = converge(&mut trees, &mut disk, 3 * nodes.len());
        prop_assert!(result.is_ok(), "{:?}", result);
        trees.settle();
        prop_assert_eq!(trees.work(), []);
        for id in &ids {
            prop_assert_eq!(trees.class(id), Class::InSync, "{}", id);
            prop_assert!(!trees.local().get(id).is_some_and(|node| node.parked));
        }
    }

    #[test]
    fn batches_of_effects_bring_the_disk_to_the_remote_tree(
        nodes in spaces(),
        limit in 1..8usize,
        turn in 0..8usize,
    ) {
        let in_s: Vec<(bool, u8)> = nodes.iter().map(|n| (n.0, n.1)).collect();
        let in_r: Vec<(bool, u8)> = nodes.iter().map(|n| (n.0, n.2)).collect();
        let s = draw_tree(&nodes.iter().map(|n| n.3).collect::<Vec<_>>(), &in_s);
        let r = draw_tree(&nodes.iter().map(|n| n.4).collect::<Vec<_>>(), &in_r);
        let ids: Vec<String> = (0..nodes.len()).map(|i| format!("n{i}")).collect();
        let spec: Vec<(&str, Option<Shape>, Option<Shape>)> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| (id.as_str(), s[i].clone(), r[i].clone()))
            .collect();
        let (mut trees, mut disk) = space(&spec);
        let result = converge_in_batches(&mut trees, &mut disk, 3 * nodes.len(), limit, turn);
        prop_assert!(result.is_ok(), "{:?}", result);
        trees.settle();
        prop_assert_eq!(trees.work(), []);
        for id in &ids {
            prop_assert_eq!(trees.class(id), Class::InSync, "{}", id);
            prop_assert!(!trees.local().get(id).is_some_and(|node| node.parked));
        }
    }
}

#[test]
fn a_first_download_comes_in_batches_of_independent_effects() {
    let mut nodes = vec![("d", None, Some(dir(ROOT, "d")))];
    let names: Vec<String> = (0..6).map(|i| format!("f{i}")).collect();
    for name in &names {
        nodes.push((name.as_str(), None, Some(file("d", name, 1))));
    }
    let (mut trees, mut disk) = space(&nodes);
    trees.settle();
    let (mut effects, mut ids) = (Effects::default(), IdGen::new(1));
    let limit = BatchLimit {
        effects: 100,
        bytes: u64::MAX,
    };
    let mut sizes = Vec::new();
    loop {
        trees.settle();
        let batch = effects.plan(&trees, None, &mut ids, NOW, limit).to_vec();
        if batch.is_empty() {
            break;
        }
        let keys: std::collections::BTreeSet<String> = (batch.iter())
            .filter_map(|record| record.after.as_ref())
            .map(|after| name_key(&after.name))
            .collect();
        assert_eq!(
            keys.len(),
            batch.len(),
            "no two effects of a batch take one name key"
        );
        sizes.push(batch.len());
        for record in &batch {
            let created = disk.apply(&record.effect).unwrap();
            effects
                .done(&mut trees, None, &record.effect_id, created)
                .unwrap();
        }
    }
    assert_eq!(sizes, [1, 6]);
    assert_eq!(effects.cleared(), 7);
    assert_in_sync(&mut trees);
}

#[test]
fn a_batch_stops_at_its_bytes_but_always_takes_one_effect() {
    let nodes: Vec<(&str, Option<Shape>, Option<Shape>)> = ["a", "b", "c"]
        .iter()
        .map(|name| (*name, None, Some(file(ROOT, name, 1))))
        .collect();
    let (mut trees, _) = space(&nodes);
    trees.settle();
    let (mut effects, mut ids) = (Effects::default(), IdGen::new(1));
    let limit = BatchLimit {
        effects: 100,
        bytes: 5,
    };
    assert_eq!(effects.plan(&trees, None, &mut ids, NOW, limit).len(), 1);
    let limit = BatchLimit {
        effects: 100,
        bytes: 6,
    };
    let mut effects = Effects::default();
    assert_eq!(effects.plan(&trees, None, &mut ids, NOW, limit).len(), 2);
}

#[test]
fn an_edit_saved_while_its_file_is_parked_survives_a_folder_taking_its_name() {
    let (mut trees, mut disk) = space(&[("a", Some(file(ROOT, "x", 1)), Some(file(ROOT, "X", 1)))]);
    trees.settle();
    let (mut effects, mut ids) = (Effects::default(), IdGen::new(1));
    let park = effects
        .plan(&trees, None, &mut ids, NOW, BatchLimit::ONE)
        .first()
        .cloned()
        .unwrap();
    assert!(
        matches!(
            park.effect,
            Effect::Move {
                to: Location::Parked(_),
                ..
            }
        ),
        "{park:?}"
    );
    let mut node = trees.local().get("a").unwrap().clone();
    node.shape.content = Some(content(9));
    trees.set_local("a".into(), node);
    disk.entries.get_mut(&1).unwrap().content = Some(content(9));
    let created = disk.apply(&park.effect).unwrap();
    effects
        .done(&mut trees, None, &park.effect_id, created)
        .unwrap();
    let a = trees.local().get("a").unwrap();
    assert!(a.parked && a.shape.content == Some(content(9)));
    trees.set_remote(
        "a".into(),
        RemoteNode {
            trashed: true,
            ..remote(file(ROOT, "X", 1))
        },
    );
    trees.set_remote("n".into(), remote(dir(ROOT, "x")));
    let done = converge(&mut trees, &mut disk, 8).unwrap();
    let mut commits = koidrive_engine::commit::Commits::default();
    let commit = commits
        .build(&mut trees, &mut ids, NOW, "history", "me", 7)
        .cloned();
    let sent = commit.as_ref().is_some_and(|commit| {
        commit.ops.iter().any(|op| match &op.op {
            Some(koidrive_proto::v1::op::Op::CreateFile(create)) => {
                create.content == Some(content(9))
            }
            Some(koidrive_proto::v1::op::Op::UpdateContent(update)) => {
                update.content == Some(content(9))
            }
            _ => false,
        })
    });
    let next = effects
        .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
        .first()
        .map(|record| record.effect.clone());
    assert!(
        sent || next.is_some(),
        "stuck: effects {done:?}, then no commit ({commit:?}) and no effect; a = {:?}, waiting {:?}",
        trees.local().get("a"),
        trees.waiting().keys().collect::<Vec<_>>()
    );
}
