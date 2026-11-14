// SPDX-License-Identifier: Apache-2.0

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use koidrive_engine::classify::Trees;
use koidrive_engine::commit::Commits;
use koidrive_engine::ids::IdGen;
use koidrive_engine::tree::{Content, FileIdentity, Kind, LocalNode, RemoteNode, Shape};
use koidrive_proto::v1::{CommitResult, OpResult, op, op_result};

const ROOT: &str = "root";
const NOW: u64 = 1_794_268_740_000;

struct Counting;

thread_local! {
    static ALLOCATED: Cell<u64> = const { Cell::new(0) };
}

fn count(bytes: usize) {
    let _ = ALLOCATED.try_with(|allocated| allocated.set(allocated.get() + bytes as u64));
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count(new_size.saturating_sub(layout.size()));
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn allocated() -> u64 {
    ALLOCATED.with(Cell::get)
}

fn local(parent: String, name: String, kind: Kind, ino: u128) -> LocalNode {
    let content = (kind == Kind::File).then(|| Content {
        size: 3,
        manifest_digest: vec![7; 32],
        ..Default::default()
    });
    LocalNode {
        shape: Shape {
            parent,
            name,
            kind,
            content,
        },
        identity: FileIdentity {
            dev: 1,
            ino,
            birth_ns: Some(0),
        },
        parked: false,
    }
}

fn created(op: &op::Op) -> (String, Shape) {
    match op {
        op::Op::CreateDir(create) => (
            create.node_id.clone(),
            Shape {
                parent: create.parent_id.clone(),
                name: create.name.clone(),
                kind: Kind::Dir,
                content: None,
            },
        ),
        op::Op::CreateFile(create) => (
            create.node_id.clone(),
            Shape {
                parent: create.parent_id.clone(),
                name: create.name.clone(),
                kind: Kind::File,
                content: create.content.clone(),
            },
        ),
        op => panic!("a first upload only creates: {op:?}"),
    }
}

fn first_upload(files: usize, per_dir: usize) -> u64 {
    let mut trees = Trees::new(ROOT);
    trees.rules_scanned();
    let dirs = files.div_ceil(per_dir);
    for d in 0..dirs {
        trees.set_local(
            format!("d{d:06}"),
            local(ROOT.into(), format!("folder {d}"), Kind::Dir, d as u128 + 1),
        );
    }
    for f in 0..files {
        let parent = format!("d{:06}", f / per_dir);
        let ino = (dirs + f) as u128 + 1;
        trees.set_local(
            format!("f{f:06}"),
            local(parent, format!("file {f}"), Kind::File, ino),
        );
    }
    trees.settle();
    let (mut commits, mut ids) = (Commits::default(), IdGen::new(1));
    let before = allocated();
    let (mut seq, mut sent) = (0, 0);
    while let Some(commit) = commits
        .build(&mut trees, &mut ids, NOW, "history", "me", seq)
        .cloned()
    {
        seq += 1;
        for op in commit.ops.iter().filter_map(|op| op.op.as_ref()) {
            let (id, shape) = created(op);
            let node = RemoteNode {
                shape,
                trashed: false,
                content_rev: seq,
                meta_rev: seq,
                trash_group: String::new(),
                version_id: String::new(),
                edit_rev: seq,
                meta_edit_rev: seq,
                pending_for: None,
                pending_by: None,
            };
            trees.set_remote_made_here(id, node);
        }
        sent += commit.ops.len();
        let applied = OpResult {
            code: op_result::Code::Applied as i32,
            ..Default::default()
        };
        let result = CommitResult {
            seq,
            results: vec![applied; commit.ops.len()],
            ..Default::default()
        };
        commits.received(result).unwrap();
        assert!(commits.acknowledge(&mut trees, seq));
        trees.settle();
    }
    let work = allocated() - before;
    assert!(trees.dirty().is_empty(), "{:?}", trees.work());
    assert_eq!(sent, files + dirs);
    assert_eq!(trees.synced().len(), files + dirs);
    work
}

#[test]
fn a_first_upload_takes_work_in_proportion_to_its_files() {
    for per_dir in [100, 5000] {
        let small = first_upload(2000, per_dir);
        let large = first_upload(20_000, per_dir);
        let bytes = large as f64 / small as f64;
        assert!(
            bytes < 12.0,
            "{per_dir} per folder: 2000 files allocated {small} bytes and 20000 files {large}, {bytes:.1} times as many"
        );
    }
}
