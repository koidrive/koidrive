// SPDX-License-Identifier: Apache-2.0

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use common::{api, claim, server_url};
use koidrive_driver::{Driver, FolderState, FolderStatus, Paths};
use koidrive_net::{Auth, Credentials};
use koidrive_proto::v1::CreateSpaceRequest;
use tokio::sync::{Mutex, OnceCell};

const PEAK_HEAP_BYTES_PER_ENTRY_CEILING: u64 = 3_218;
const SYNCED_HEAP_BYTES_PER_ENTRY_CEILING: u64 = 2_392;
const FOOTPRINT_TARGET: (u64, u64) = (750, 1536 << 20);

static HEAP: AtomicU64 = AtomicU64::new(0);
static PEAK_HEAP: AtomicU64 = AtomicU64::new(0);
static OWNER: OnceCell<Credentials> = OnceCell::const_new();
static ONE_RUN: Mutex<()> = Mutex::const_new(());

struct Counting;

impl Counting {
    fn grew(&self, by: usize) {
        let heap = HEAP.fetch_add(by as u64, Ordering::Relaxed) + by as u64;
        PEAK_HEAP.fetch_max(heap, Ordering::Relaxed);
    }
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            self.grew(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            self.grew(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        HEAP.fetch_sub(layout.size() as u64, Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let moved = unsafe { System.realloc(ptr, layout, new_size) };
        if !moved.is_null() {
            HEAP.fetch_sub(layout.size() as u64, Ordering::Relaxed);
            self.grew(new_size);
        }
        moved
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

#[cfg(target_os = "macos")]
fn footprint_bytes() -> u64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage_info_v4>::zeroed();
    let read = unsafe {
        libc::proc_pid_rusage(
            std::process::id() as libc::c_int,
            libc::RUSAGE_INFO_V4,
            usage.as_mut_ptr().cast(),
        )
    };
    assert_eq!(read, 0, "proc_pid_rusage failed");
    unsafe { usage.assume_init() }.ri_phys_footprint
}

#[cfg(not(target_os = "macos"))]
fn footprint_bytes() -> u64 {
    let status = fs::read_to_string("/proc/self/status").unwrap();
    ["RssAnon:", "VmSwap:"]
        .iter()
        .map(|field| {
            let line = status.lines().find(|line| line.starts_with(field)).unwrap();
            line[field.len()..]
                .trim()
                .trim_end_matches(" kB")
                .parse::<u64>()
                .unwrap()
                * 1024
        })
        .sum()
}

fn scale_tree(root: &Path, projects: usize, packages: usize, distinct_projects: usize) -> u64 {
    let body = "module.exports = function () { return Object.freeze({ ok: true, value: 42 }); };\n"
        .repeat(4);
    let threads: Vec<_> = (0..projects)
        .map(|project| {
            let root = root.join(format!("project{project:02}/node_modules"));
            let body = body.clone();
            std::thread::spawn(move || {
                let mut entries = 2u64;
                for package in 0..packages {
                    let version = if project < distinct_projects {
                        project
                    } else {
                        distinct_projects
                    };
                    let dir = root.join(format!("pkg-{package:04}"));
                    let content =
                        |name: &str| format!("// pkg-{package:04}@{version} {name}\n{body}");
                    let mut files = vec![
                        "package.json".to_owned(),
                        "README.md".to_owned(),
                        "LICENSE".to_owned(),
                        "index.js".to_owned(),
                    ];
                    for a in 0..3 {
                        for b in 0..3 {
                            for file in 0..4 {
                                files.push(format!("lib/m{a}/s{b}/f{file}.js"));
                            }
                        }
                    }
                    for name in &files {
                        let path = dir.join(name);
                        fs::create_dir_all(path.parent().unwrap()).unwrap();
                        fs::write(&path, content(name)).unwrap();
                    }
                    entries += 1 + 1 + 3 + 9 + files.len() as u64;
                }
                entries
            })
        })
        .collect();
    threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .sum()
}

struct Run {
    entries: u64,
    status: FolderStatus,
    seconds: u64,
    heap: u64,
    peak_heap: u64,
    footprint: u64,
    peak_footprint: u64,
}

impl Run {
    fn memory(&self) -> serde_json::Value {
        serde_json::json!({
            "heap_bytes": self.heap,
            "peak_heap_bytes": self.peak_heap,
            "footprint_bytes": self.footprint,
            "peak_footprint_bytes": self.peak_footprint,
            "peak_footprint_target_bytes": FOOTPRINT_TARGET.1,
        })
    }
}

async fn first_sync(
    projects: usize,
    distinct_projects: usize,
    window: Duration,
    mut done: impl FnMut(&FolderStatus, Duration, u64) -> bool,
) -> Run {
    let _one_run = ONE_RUN.lock().await;
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .try_init();
    let base =
        std::env::var("KOIDRIVE_SCALE_DIR").map_or_else(|_| std::env::temp_dir(), Into::into);
    let home = tempfile::tempdir_in(base).unwrap();
    let folder = home.path().join("folder");
    let made = Instant::now();
    let entries = scale_tree(&folder, projects, 925, distinct_projects);
    println!(
        "{}",
        serde_json::json!({ "tree": true, "entries": entries, "seconds": made.elapsed().as_secs() })
    );
    let owner = Arc::new(Auth::new(api(), OWNER.get_or_init(claim).await.clone()));
    let request = CreateSpaceRequest {
        name: "scale".into(),
        request_id: String::new(),
    };
    let space = owner.api().create_space(&owner, &request).await.unwrap();
    let driver = Driver::open(Paths::under(&home.path().join("profile"))).unwrap();
    let pairing = owner.api().pair_start(&owner).await.unwrap();
    driver
        .pair(server_url().as_str(), &pairing.code, "scale")
        .await
        .unwrap();
    PEAK_HEAP.store(HEAP.load(Ordering::Relaxed), Ordering::Relaxed);
    let started = Instant::now();
    driver.link(&folder, &space.space_id).await.unwrap();
    driver.start();
    let mut peak = footprint_bytes();
    let mut measured = Instant::now();
    let status = loop {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if measured.elapsed() >= Duration::from_secs(2) {
            peak = peak.max(footprint_bytes());
            measured = Instant::now();
        }
        let status = driver.status().folders.remove(0);
        if done(&status, started.elapsed(), entries) || started.elapsed() >= window {
            break status;
        }
    };
    let (heap, peak_heap) = (
        HEAP.load(Ordering::Relaxed),
        PEAK_HEAP.load(Ordering::Relaxed),
    );
    let footprint = footprint_bytes();
    let seconds = started.elapsed().as_secs();
    driver.stop().await;
    Run {
        entries,
        status,
        seconds,
        heap,
        peak_heap,
        footprint,
        peak_footprint: peak.max(footprint),
    }
}

#[derive(Default)]
struct Stalls {
    first_move: Option<Duration>,
    last_move: Option<Duration>,
    last: (u64, u64),
    longest: Duration,
}

impl Stalls {
    fn saw(&mut self, remote_nodes: u64, bytes_done: Option<u64>, at: Duration) {
        let now = (remote_nodes, bytes_done.unwrap_or(0));
        if let Some(moved) = self.last_move {
            self.longest = self.longest.max(at.saturating_sub(moved));
        }
        if now.0 > self.last.0 || now.1 > self.last.1 {
            self.first_move.get_or_insert(at);
            self.last_move = Some(at);
        }
        self.last = now;
    }

    fn judge(&self, allowed: Duration) -> Result<(), String> {
        match self.first_move {
            None => Err("the first sync never moved".into()),
            Some(_) if self.longest > allowed => Err(format!(
                "the first sync stood still for {:?}, longer than {allowed:?}",
                self.longest
            )),
            Some(_) => Ok(()),
        }
    }
}

fn whole_tree_on_server(
    state: FolderState,
    moving: bool,
    remote_nodes: u64,
    entries: u64,
) -> Result<(), String> {
    match state == FolderState::UpToDate && !moving && remote_nodes == entries + 1 {
        true => Ok(()),
        false => Err(format!(
            "{remote_nodes} of the {} nodes of the tree reached the server, and the folder is {state:?}",
            entries + 1
        )),
    }
}

fn whole_tree(status: &FolderStatus, entries: u64) -> Result<(), String> {
    let moving = status.progress.is_some();
    whole_tree_on_server(status.state, moving, status.remote_nodes, entries)
}

#[test]
fn the_stall_gate_fails_a_sync_that_gains_nothing_while_its_progress_vanishes() {
    let mut stalls = Stalls::default();
    for tick in 0..=64 {
        let at = Duration::from_millis(250 * tick);
        let bytes = (at < Duration::from_secs(8)).then_some(4096);
        stalls.saw(10, bytes, at);
    }
    assert!(stalls.judge(Duration::from_secs(8)).is_err());
}

#[test]
fn the_stall_gate_counts_a_pause_between_two_samples() {
    let mut stalls = Stalls::default();
    stalls.saw(10, None, Duration::ZERO);
    stalls.saw(11, None, Duration::from_secs(10));
    assert!(stalls.judge(Duration::from_secs(8)).is_err());
    stalls.saw(12, None, Duration::from_secs(11));
    assert_eq!(stalls.longest, Duration::from_secs(10));
}

#[test]
fn the_heap_gate_judges_only_a_tree_that_reached_the_server_whole() {
    assert!(whole_tree_on_server(FolderState::UpToDate, false, 2, 199_808).is_err());
    assert!(whole_tree_on_server(FolderState::UpToDate, false, 199_809, 199_808).is_ok());
}

fn seconds(name: &str, default: u64) -> Duration {
    Duration::from_secs(std::env::var(name).map_or(default, |s| s.parse().unwrap()))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the Worker under wrangler dev and a few GB of disk"]
async fn a_first_sync_of_two_million_entries_keeps_moving() {
    let window = seconds("KOIDRIVE_SCALE_SECONDS", 600);
    let longest_allowed = seconds("KOIDRIVE_SCALE_MAX_STALL", 8);
    let mut stalls = Stalls::default();
    let mut problems = Vec::new();
    let run = first_sync(40, 4, window, |status, elapsed, _| {
        if let Some(problem) = &status.problem {
            problems.push(problem.clone());
        }
        if status.state == FolderState::UpToDate {
            return true;
        }
        let moved = stalls.first_move.is_some();
        let bytes = (status.progress.as_ref()).map(|progress| progress.bytes_done);
        stalls.saw(status.remote_nodes, bytes, elapsed);
        if !moved && let Some(at) = stalls.first_move {
            println!("{}", serde_json::json!({ "first_move": at.as_secs() }));
        }
        false
    })
    .await;
    println!(
        "{}",
        serde_json::json!({
            "summary": true,
            "memory": run.memory(),
            "seconds": run.seconds,
            "state": format!("{:?}", run.status.state),
            "remote_nodes": run.status.remote_nodes,
            "bytes_done": run.status.progress.as_ref().map(|progress| progress.bytes_done),
            "longest_stall_ms": stalls.longest.as_millis() as u64,
            "problems": problems.len(),
        })
    );
    assert!(
        problems.is_empty(),
        "the folder showed problems: {:?}",
        &problems[..problems.len().min(5)]
    );
    stalls.judge(longest_allowed).unwrap();
    if run.status.state == FolderState::UpToDate {
        whole_tree(&run.status, run.entries).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the Worker under wrangler dev and a few hundred MB of disk"]
async fn a_first_sync_holds_at_most_its_heap_ceilings_per_entry() {
    let limit = seconds("KOIDRIVE_SCALE_SECONDS", 1800);
    let run = first_sync(4, 1, limit, |status, _, entries| {
        whole_tree(status, entries).is_ok() || status.state == FolderState::Halted
    })
    .await;
    let (synced, peak) = (run.heap / run.entries, run.peak_heap / run.entries);
    println!(
        "{}",
        serde_json::json!({
            "summary": true,
            "entries": run.entries,
            "seconds": run.seconds,
            "state": format!("{:?}", run.status.state),
            "remote_nodes": run.status.remote_nodes,
            "memory": run.memory(),
            "heap_bytes_per_entry": synced,
            "peak_heap_bytes_per_entry": peak,
            "footprint_bytes_per_entry": run.footprint / run.entries,
            "footprint_target_bytes_per_entry": FOOTPRINT_TARGET.0,
        })
    );
    if let Err(why) = whole_tree(&run.status, run.entries) {
        panic!(
            "the first sync of {} entries did not finish within {limit:?}: {why}",
            run.entries
        );
    }
    assert!(
        peak <= PEAK_HEAP_BYTES_PER_ENTRY_CEILING,
        "a first sync held at most {peak} heap bytes per entry, more than its ceiling of {PEAK_HEAP_BYTES_PER_ENTRY_CEILING}"
    );
    assert!(
        synced <= SYNCED_HEAP_BYTES_PER_ENTRY_CEILING,
        "a completed first sync holds {synced} heap bytes per entry, more than its ceiling of {SYNCED_HEAP_BYTES_PER_ENTRY_CEILING}"
    );
}
