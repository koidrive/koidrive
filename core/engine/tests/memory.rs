// SPDX-License-Identifier: Apache-2.0

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use koidrive_engine::classify::Trees;
use koidrive_engine::commit::Commits;
use koidrive_engine::ids::IdGen;
use koidrive_engine::state::CommitsState;
use koidrive_engine::tree::{Content, FileIdentity, Kind, LocalNode, RemoteNode, Shape};
use koidrive_proto::v1::{ChunkRef, CommitResult, OpResult, op, op_result};

const ROOT: &str = "01a1eab7-d941-7d6a-9e0d-576a24408e65";
const NOW: u64 = 1_794_268_740_000;
const PACKAGES: usize = 600;
const TARGET: u64 = 600;
const FIRST_UPLOAD_CEILING: u64 = 2_950;
const RESTART_CEILING: u64 = 3_150;

struct Live;

thread_local! {
    static LIVE: Cell<i64> = const { Cell::new(0) };
}

fn add(bytes: i64) {
    let _ = LIVE.try_with(|live| live.set(live.get() + bytes));
}

unsafe impl GlobalAlloc for Live {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        add(layout.size() as i64);
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        add(layout.size() as i64);
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        add(new_size as i64 - layout.size() as i64);
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        add(-(layout.size() as i64));
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Live = Live;

fn live() -> i64 {
    LIVE.with(Cell::get)
}

struct Entry {
    id: String,
    parent: String,
    name: String,
    kind: Kind,
}

fn node_modules(ids: &mut IdGen) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut add = |parent: &str, name: String, kind: Kind| {
        let id = ids.uuid_v7(NOW);
        entries.push(Entry {
            id: id.clone(),
            parent: parent.to_owned(),
            name,
            kind,
        });
        id
    };
    let modules = add(ROOT, "node_modules".into(), Kind::Dir);
    for p in 0..PACKAGES {
        let package = add(&modules, format!("package-{p}"), Kind::Dir);
        for name in [
            "package.json",
            "README.md",
            "LICENSE",
            "index.js",
            "index.d.ts",
        ] {
            add(&package, name.into(), Kind::File);
        }
        for folder in ["lib", "dist"] {
            let folder = add(&package, folder.into(), Kind::Dir);
            for m in 0..12 {
                add(&folder, format!("module-{m}.js"), Kind::File);
            }
        }
    }
    entries
}

fn content(n: usize) -> Content {
    let digest = |salt: u8| {
        (0..32)
            .map(|i| (n as u8).wrapping_mul(31) ^ i ^ salt)
            .collect()
    };
    Content {
        size: 1_000 + n as u64,
        mtime_ns: 1_794_268_740_123_456_789,
        executable: false,
        chunks: vec![ChunkRef {
            hash: digest(1),
            size: 1_000 + n as u32,
            encoding: 0,
        }],
        chunk_count: 1,
        manifest_digest: digest(2),
        chunker: "fastcdc2020-1m-4m-16m".into(),
    }
}

fn shape(entry: &Entry, n: usize) -> Shape {
    Shape {
        parent: entry.parent.clone(),
        name: entry.name.clone(),
        kind: entry.kind,
        content: (entry.kind == Kind::File).then(|| content(n)),
    }
}

fn first_upload() -> (Trees, usize) {
    let mut ids = IdGen::new(1);
    let entries = node_modules(&mut ids);
    let mut trees = Trees::new(ROOT);
    trees.rules_scanned();
    for (n, entry) in entries.iter().enumerate() {
        let local = LocalNode {
            shape: shape(entry, n),
            identity: FileIdentity {
                ino: n as u128 + 1,
                birth_ns: Some(1_794_268_740_000_000_000),
                handle: None,
            },
            parked: false,
            left_out_when_placed: false,
            content_from_server: false,
        };
        trees.set_local(entry.id.clone(), local);
    }
    trees.settle(&Default::default());
    let mut commits = Commits::from_state(CommitsState::default()).unwrap();
    let mut seq = 0;
    while let Some(commit) = commits
        .build(&mut trees, &mut ids, NOW, "history", "me", seq)
        .cloned()
    {
        seq += 1;
        for op in commit.ops.iter().filter_map(|op| op.op.as_ref()) {
            let (id, shape) = match op {
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
            };
            let version_id = match shape.kind {
                Kind::File => ids.uuid_v7(NOW),
                Kind::Dir => String::new(),
            };
            let node = RemoteNode {
                shape,
                trashed: false,
                content_rev: seq,
                meta_rev: seq,
                meta_edit_rev: seq,
                trash_group: String::new(),
                version_id,
                made_over: Vec::new(),
                edit_rev: seq,
                pending_for: None,
                pending_by: None,
            };
            trees.set_remote_made_here(id, node);
        }
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
        trees.settle(commits.aliases());
    }
    assert!(trees.dirty().is_empty(), "{:?}", trees.work());
    assert_eq!(trees.synced().len(), entries.len());
    trees.take_changed();
    trees.take_changes();
    (trees, entries.len())
}

fn restarted() -> (Trees, usize) {
    let (synced, entries) = first_upload();
    let mut trees = Trees::new(ROOT);
    for (id, node) in synced.remote().iter() {
        trees.set_remote(id.clone(), node.clone());
    }
    for (id, shape) in synced.synced().iter() {
        trees.set_synced(id.clone(), Some(shape.clone()));
    }
    for (id, node) in synced.local().iter() {
        trees.set_local(id.clone(), node.clone());
    }
    trees.restore(synced.state()).unwrap();
    trees.take_changed();
    trees.take_changes();
    assert!(trees.dirty().is_empty(), "{:?}", trees.work());
    (trees, entries)
}

fn bytes_per_entry(build: fn() -> (Trees, usize)) -> u64 {
    let before = live();
    let (trees, entries) = build();
    let held = live() - before;
    drop(trees);
    held as u64 / entries as u64
}

fn assert_holds(first_upload_budget: u64, restart_budget: u64) {
    for (when, build, budget) in [
        (
            "after a first upload",
            first_upload as fn() -> _,
            first_upload_budget,
        ),
        ("after a restart", restarted, restart_budget),
    ] {
        let held = bytes_per_entry(build);
        println!("{when}: {held} bytes per entry");
        assert!(
            held <= budget,
            "{when}, the engine holds {held} bytes per entry of a synced node_modules folder, over {budget}"
        );
    }
}

#[test]
fn the_engine_holds_a_synced_folder_within_its_memory_ceiling() {
    assert_holds(FIRST_UPLOAD_CEILING, RESTART_CEILING);
}

#[test]
#[ignore]
fn the_engine_holds_a_synced_folder_in_600_bytes_per_entry() {
    assert_holds(TARGET, TARGET);
}
