// SPDX-License-Identifier: Apache-2.0

use std::cell::Cell;

use koidrive_proto::v1::op;

use crate::classify::Trees;
use crate::commit::Commits;
use crate::effects::{BatchLimit, Effects};
use crate::ids::IdGen;
use crate::ignore::{RuleFile, Source};
use crate::question::Answer;
use crate::tree::{
    Content, ENTRIES_BUDGET, ENTRIES_READ, FileIdentity, Kind, LocalNode, RemoteNode, Shape,
};

const ROOT: &str = "root";
const NOW: u64 = 1_794_268_740_000;

fn reads<T>(budget: u64, work: impl FnOnce() -> T) -> (T, u64) {
    let before = ENTRIES_READ.with(Cell::get);
    ENTRIES_BUDGET.with(|limit| limit.set(before.saturating_add(budget)));
    let done = work();
    ENTRIES_BUDGET.with(|limit| limit.set(u64::MAX));
    (done, ENTRIES_READ.with(Cell::get) - before)
}

fn file(parent: &str, name: &str, digest: u8) -> Shape {
    Shape {
        parent: parent.into(),
        name: name.into(),
        kind: Kind::File,
        content: Some(Content {
            size: 3,
            manifest_digest: vec![digest; 32],
            ..Default::default()
        }),
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

fn local(shape: Shape, ino: u128) -> LocalNode {
    LocalNode {
        left_out_when_placed: false,
        content_from_server: false,
        shape,
        identity: FileIdentity {
            ino,
            birth_ns: Some(0),
            handle: None,
        },
        parked: false,
    }
}

fn remote(shape: Shape) -> RemoteNode {
    RemoteNode {
        made_over: Vec::new(),
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

fn ops(commits: &mut Commits, trees: &mut Trees, ids: &mut IdGen) -> Vec<op::Op> {
    let commit = commits.build(trees, ids, NOW, "history", "me", 1).cloned();
    let ops = commit.map(|commit| commit.ops).unwrap_or_default();
    ops.into_iter().filter_map(|op| op.op).collect()
}

fn discarded_server_folder_let_in(files: usize, budget: u64) -> u64 {
    let names: Vec<String> = (0..files).map(|i| format!("y{i}")).collect();
    let server = std::iter::once(("x".to_owned(), dir(ROOT, "server")))
        .chain(names.iter().map(|name| (name.clone(), file("x", name, 9))));
    let mut trees = Trees::new(ROOT);
    trees.set_local("f".into(), local(dir(ROOT, "new"), 1));
    for (ino, (id, shape)) in server.enumerate() {
        trees.set_local(id.clone(), local(shape.clone(), ino as u128 + 2));
        trees.set_remote(id, remote(shape));
    }
    trees.settle(&Default::default());
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(7));
    commits.first_link(
        &mut trees,
        &mut ids,
        NOW,
        crate::first_link::PriorSync::Shown,
    );
    let [asked] = commits.questions() else {
        panic!("one question: {:?}", commits.questions());
    };
    let asked = asked.id.clone();
    trees.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(b"ignored\n")),
    );
    let mut moved = trees.local().get("x").unwrap().clone();
    moved.shape.parent = "f".into();
    moved.shape.name = "ignored".into();
    trees.set_local_changed("x".into(), moved);
    trees.settle(commits.aliases());
    assert!(commits.decide(&asked, Answer::Discard));
    assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
    trees.set_rules(&[] as &[&str], Source::Koiignore, None);
    trees.settle(commits.aliases());
    let (planned, read) = reads(budget, || {
        assert_eq!(ops(&mut commits, &mut trees, &mut ids), []);
        Effects::default()
            .plan(&trees, commits.in_flight(), &mut ids, NOW, BatchLimit::ONE)
            .to_vec()
    });
    assert!(
        planned.iter().any(|record| record.node == "x"),
        "{planned:?}"
    );
    read
}

#[test]
fn a_server_folder_a_discard_took_along_is_planned_in_linear_time_once_the_rules_let_it_in() {
    let two_hundred = discarded_server_folder_let_in(200, u64::MAX);
    discarded_server_folder_let_in(2000, two_hundred * 11);
}

fn renamed_folder_above(depth: usize, files: usize, budget: u64) -> u64 {
    let mut trees = Trees::new(ROOT);
    for i in 0..depth {
        let parent = if i == 0 {
            ROOT.to_owned()
        } else {
            format!("d{}", i - 1)
        };
        let shape = dir(&parent, &format!("n{i}"));
        let here = if i == 0 {
            dir(ROOT, "renamed")
        } else {
            shape.clone()
        };
        trees.set_local(format!("d{i}"), local(here, i as u128 + 1));
        trees.set_synced(format!("d{i}"), Some(shape.clone()));
        trees.set_remote(format!("d{i}"), remote(shape));
    }
    let bottom = format!("d{}", depth - 1);
    for j in 0..files {
        let leaf = file(&bottom, &format!("f{j}.txt"), 1);
        trees.set_local(format!("f{j}"), local(leaf.clone(), 1000 + j as u128));
        trees.set_synced(format!("f{j}"), Some(leaf.clone()));
        trees.set_remote(format!("f{j}"), remote(leaf));
        trees.set_remote(
            format!("g{j}"),
            remote(file(&bottom, &format!("g{j}.txt"), 2)),
        );
    }
    trees.set_rules(
        &[] as &[&str],
        Source::Koiignore,
        Some(RuleFile::parse(b"ss/\n")),
    );
    let (ops, read) = reads(budget, || {
        trees.settle(&Default::default());
        ops(&mut Commits::default(), &mut trees, &mut IdGen::new(1))
    });
    assert_eq!(ops.len(), 1);
    assert!(!trees.excluded("g0"));
    read
}

#[test]
fn planning_below_a_folder_renamed_here_is_linear_in_its_files_and_at_most_quadratic_in_its_depth()
{
    let two_hundred = renamed_folder_above(3, 200, u64::MAX);
    renamed_folder_above(3, 2000, two_hundred * 11);
    let half_as_deep = renamed_folder_above(30, 500, u64::MAX);
    renamed_folder_above(60, 500, half_as_deep * 4);
}
