// SPDX-License-Identifier: Apache-2.0

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use koidrive_engine::classify::Trees;
use koidrive_engine::commit::Commits;
use koidrive_engine::effects::{BatchLimit, Effects};
use koidrive_engine::ids::IdGen;
use koidrive_engine::state::CommitsState;
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
        displaced: false,
        left_out_when_placed: false,
        content_from_server: false,
        shape: Shape {
            parent,
            name,
            kind,
            content,
        },
        identity: FileIdentity {
            ino,
            birth_ns: Some(0),
            handle: None,
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

#[derive(Clone, Copy, Debug)]
enum Ids {
    FoldersFirst,
    FilesFirst,
    Shuffled,
}

impl Ids {
    fn id(self, kind: Kind, n: usize) -> String {
        let (dir, file) = match self {
            Ids::FoldersFirst | Ids::Shuffled => ("d", "f"),
            Ids::FilesFirst => ("z", "a"),
        };
        let prefix = if kind == Kind::Dir { dir } else { file };
        match self {
            Ids::Shuffled => {
                let hash = (n as u64 + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 40;
                format!("{hash:06x}{prefix}{n}")
            }
            _ => format!("{prefix}{n:06}"),
        }
    }
}

#[derive(Clone, Copy)]
struct Upload {
    files: usize,
    per_dir: usize,
    ids: Ids,
    nested: bool,
    ignored: bool,
    cap: Option<usize>,
}

fn first_upload(upload: Upload) -> u64 {
    let Upload {
        files,
        per_dir,
        ids: order,
        nested,
        ignored,
        cap,
    } = upload;
    let mut trees = Trees::new(ROOT);
    trees.rules_scanned();
    let dirs = files.div_ceil(per_dir);
    for d in 0..dirs {
        let parent = match nested && d > 0 {
            true => order.id(Kind::Dir, (d - 1) / 3),
            false => ROOT.into(),
        };
        trees.set_local(
            order.id(Kind::Dir, d),
            local(parent, format!("folder {d}"), Kind::Dir, d as u128 + 1),
        );
    }
    for f in 0..files {
        let ino = (dirs + f) as u128 + 1;
        trees.set_local(
            order.id(Kind::File, f),
            local(
                order.id(Kind::Dir, f / per_dir),
                format!("file {f}"),
                Kind::File,
                ino,
            ),
        );
    }
    if ignored {
        trees.set_local(
            "!d".into(),
            local(ROOT.into(), "ignored".into(), Kind::Dir, 0),
        );
        for f in 0..files {
            let ino = (dirs + files + f) as u128 + 1;
            trees.set_local(
                format!("!f{f:06}"),
                local("!d".into(), format!("file {f}"), Kind::File, ino),
            );
        }
        trees.ignore_local("!d");
    }
    trees.settle(&Default::default());
    let state = CommitsState {
        cap,
        ..Default::default()
    };
    let mut commits = Commits::from_state(state).unwrap();
    let mut ids = IdGen::new(1);
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
                made_over: Vec::new(),
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
        trees.settle(commits.aliases());
    }
    let work = allocated() - before;
    assert!(
        trees.dirty().iter().all(|id| trees.excluded(id)),
        "{:?}",
        trees.work()
    );
    assert_eq!(sent, files + dirs);
    assert_eq!(trees.synced().len(), files + dirs);
    work
}

#[test]
fn a_first_upload_takes_work_in_proportion_to_its_files() {
    let ordered = [Ids::FoldersFirst, Ids::FilesFirst]
        .map(|ids| [1, 100, 5000].map(|per_dir| (ids, per_dir, false)));
    let cases = ordered
        .into_iter()
        .flatten()
        .chain([(Ids::FoldersFirst, 100, true)]);
    for (ids, per_dir, ignored) in cases {
        let upload = |files| Upload {
            files,
            per_dir,
            ids,
            nested: false,
            ignored,
            cap: None,
        };
        let small = first_upload(upload(2000));
        let large = first_upload(upload(20_000));
        let bytes = large as f64 / small as f64;
        assert!(
            bytes < 12.0,
            "{ids:?}, {per_dir} per folder, ignored folder {ignored}: 2000 files allocated {small} bytes and 20000 files {large}, {bytes:.1} times as many"
        );
    }
}

fn remote(parent: String, name: String, kind: Kind) -> RemoteNode {
    let content = (kind == Kind::File).then(|| Content {
        size: 3,
        manifest_digest: vec![7; 32],
        ..Default::default()
    });
    RemoteNode {
        made_over: Vec::new(),
        shape: Shape {
            parent,
            name,
            kind,
            content,
        },
        trashed: false,
        content_rev: 1,
        meta_rev: 1,
        trash_group: String::new(),
        version_id: String::new(),
        edit_rev: 1,
        meta_edit_rev: 1,
        pending_for: None,
        pending_by: None,
    }
}

fn first_download(files: usize, per_dir: usize) -> u64 {
    let mut trees = Trees::new(ROOT);
    trees.rules_scanned();
    let dirs = files.div_ceil(per_dir);
    for d in 0..dirs {
        let node = remote(ROOT.into(), format!("folder {d}"), Kind::Dir);
        trees.set_remote(format!("d{d:06}"), node);
    }
    for f in 0..files {
        let node = remote(
            format!("d{:06}", f / per_dir),
            format!("file {f}"),
            Kind::File,
        );
        trees.set_remote(format!("f{f:06}"), node);
    }
    trees.settle(&Default::default());
    let (mut effects, mut ids) = (Effects::default(), IdGen::new(1));
    let limit = BatchLimit {
        effects: 64,
        bytes: u64::MAX,
    };
    let before = allocated();
    let mut ino = 0;
    loop {
        let planned: Vec<String> = (effects.plan(&trees, None, &mut ids, NOW, limit).iter())
            .map(|record| record.effect_id.clone())
            .collect();
        if planned.is_empty() {
            break;
        }
        for effect_id in planned {
            ino += 1;
            let identity = FileIdentity {
                ino,
                birth_ns: Some(0),
                handle: None,
            };
            (effects.done(&mut trees, None, &effect_id, Some(identity))).unwrap();
            trees.settle(&Default::default());
        }
    }
    let work = allocated() - before;
    assert!(trees.dirty().is_empty(), "{:?}", trees.work());
    assert_eq!(trees.synced().len(), files + dirs);
    work
}

fn first_link_of_nested_folders(levels: usize, depth: usize) -> u64 {
    let mut trees = Trees::new(ROOT);
    trees.rules_scanned();
    let mut parent = ROOT.to_owned();
    for level in 0..levels {
        if level % depth == 0 {
            parent = ROOT.to_owned();
        }
        let folder = format!("d{level:06}");
        let name = format!("folder {level}");
        let ino = 2 * level as u128 + 1;
        trees.set_local(
            folder.clone(),
            local(parent.clone(), name.clone(), Kind::Dir, ino),
        );
        trees.set_remote(folder.clone(), remote(parent, name, Kind::Dir));
        let mut file = local(folder.clone(), "notes".into(), Kind::File, ino + 1);
        (file.shape.content.as_mut()).expect("a file").mtime_ns = 1_794_268_740_123_456_789;
        let shape = file.shape.clone();
        trees.set_local(format!("l{level:06}"), file);
        let mut node = remote(folder.clone(), "notes".into(), Kind::File);
        node.shape = shape;
        trees.set_remote(format!("r{level:06}"), node);
        parent = folder;
    }
    let before = allocated();
    trees.settle(&Default::default());
    let work = allocated() - before;
    assert!(trees.dirty().is_empty(), "{:?}", trees.work());
    assert_eq!(trees.synced().len(), 2 * levels);
    work
}

fn links_waiting_on_their_folders(folders: usize, settles: usize) -> (u64, u64) {
    let mut trees = Trees::new(ROOT);
    trees.rules_scanned();
    for f in 0..folders {
        let folder = format!("d{f:06}");
        let ino = 2 * f as u128 + 1;
        let local_folder = local(ROOT.into(), format!("mine {f}"), Kind::Dir, ino);
        trees.set_local(folder.clone(), local_folder);
        trees.set_remote(
            folder.clone(),
            remote(ROOT.into(), format!("theirs {f}"), Kind::Dir),
        );
        let mut file = local(folder.clone(), "notes".into(), Kind::File, ino + 1);
        (file.shape.content.as_mut()).expect("a file").mtime_ns = 1_794_268_740_123_456_789;
        let mut node = remote(folder.clone(), "notes".into(), Kind::File);
        node.shape = file.shape.clone();
        trees.set_local(format!("l{f:06}"), file);
        trees.set_remote(format!("r{f:06}"), node);
    }
    trees.settle(&Default::default());
    let before = allocated();
    for s in 0..settles {
        let ino = (2 * folders + s) as u128 + 1;
        let other = local(ROOT.into(), format!("other {s}"), Kind::File, ino);
        trees.set_local(format!("o{s:06}"), other);
        trees.settle(&Default::default());
    }
    let waiting = allocated() - before;
    let before = allocated();
    for f in 0..folders {
        let placed = local(
            ROOT.into(),
            format!("theirs {f}"),
            Kind::Dir,
            2 * f as u128 + 1,
        );
        trees.set_local_changed(format!("d{f:06}"), placed);
    }
    trees.settle(&Default::default());
    let placing = allocated() - before;
    assert_eq!(trees.synced().len(), 2 * folders);
    (waiting, placing)
}

#[test]
fn links_waiting_on_their_folders_cost_nothing_until_the_folders_enter_s() {
    let (small_waiting, small_placing) = links_waiting_on_their_folders(2000, 500);
    let (large_waiting, large_placing) = links_waiting_on_their_folders(20_000, 500);
    let waiting = large_waiting as f64 / small_waiting as f64;
    assert!(
        waiting < 2.0,
        "500 settles beside 2000 waiting links allocated {small_waiting} bytes and beside 20000 {large_waiting}, {waiting:.1} times as many"
    );
    let placing = large_placing as f64 / small_placing as f64;
    assert!(
        placing < 12.0,
        "placing 2000 folders allocated {small_placing} bytes and 20000 {large_placing}, {placing:.1} times as many"
    );
}

#[test]
fn a_first_link_of_nested_new_folders_takes_work_in_proportion_to_its_files() {
    for depth in [1, 10] {
        let small = first_link_of_nested_folders(2000, depth);
        let large = first_link_of_nested_folders(20_000, depth);
        let bytes = large as f64 / small as f64;
        assert!(
            bytes < 12.0,
            "{depth} deep: 2000 folders allocated {small} bytes and 20000 folders {large}, {bytes:.1} times as many"
        );
    }
}

#[test]
fn a_first_download_takes_work_in_proportion_to_its_files() {
    for per_dir in [1, 100, 5000] {
        let small = first_download(2000, per_dir);
        let large = first_download(20_000, per_dir);
        let bytes = large as f64 / small as f64;
        assert!(
            bytes < 12.0,
            "{per_dir} per folder: 2000 files allocated {small} bytes and 20000 files {large}, {bytes:.1} times as many"
        );
    }
}

#[test]
fn a_first_upload_in_small_commits_sends_every_file_whatever_order_its_ids_take() {
    for ids in [Ids::FoldersFirst, Ids::FilesFirst, Ids::Shuffled] {
        for nested in [false, true] {
            for per_dir in [1, 3, 40] {
                first_upload(Upload {
                    files: 150,
                    per_dir,
                    ids,
                    nested,
                    ignored: false,
                    cap: Some(5),
                });
            }
        }
    }
}
