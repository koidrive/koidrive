// SPDX-License-Identifier: Apache-2.0

//! The effect executor against a real folder and a real store, with the network behind [`Fetch`] replaced by chunks
//! held in memory. A crash is a panic that unwinds out of the executor at a cut, dropping the store, the engine and
//! the lock; nothing runs between the cut and the next start, as after a process kill.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::future::Future;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use koidrive_chunk::{ChunkerConfig, chunks, content};
use koidrive_engine::classify::Trees;
use koidrive_engine::commit::Halt as CommitHalt;
use koidrive_engine::effects::{Base as EngineBase, Dir as EngineDir};
use koidrive_engine::state::{EffectsState, TreesState};
use koidrive_engine::tree::{FileIdentity, Kind, LocalNode, Shape};
use koidrive_fs::{
    FileId, Known, PRIVATE_DIR, QUARANTINE_DIR, STAGING_DIR, Snapshot, TRASH_DIR, read_marker,
    write_marker,
};
use koidrive_net::ServerError;
use koidrive_proto::v1::{NodeKind, NodeState, NodeStatus, VersionInfo};
use koidrive_store::{EngineState, SyncedNode};

use super::*;
use crate::claim;
use crate::ignore::Unfiltered;
use crate::observe::{FsObserver, Observer, READ_FILES};

const SPACE: &str = "0190f0a0-0000-7000-8000-000000000001";
const NODE: &str = "0190f0a0-0000-7000-8000-0000000000a1";
const SYNCED: &[u8] = b"the synced report";
const REMOTE: &[u8] = b"the report as the other device wrote it";
const REMOTE_MTIME: i64 = 1_700_000_000_123_456_789;

/// The user deletes a file whose removal was planned, after R brought the file back, and a crash
/// comes before the removal. Recovery finds the file gone and reports the removal as done, but the
/// observed delete stays the user's.
#[test]
fn a_delete_the_user_made_before_a_recovered_removal_stays_local() {
    let folder = Folder::new();
    fs::write(folder.root.join("report.txt"), SYNCED).unwrap();
    let saved = disk_content(&folder.root.join("report.txt"));
    folder.seed(&[], &[(NODE, SPACE, "report.txt")], 0);
    let mut running = folder.start(no_hook()).unwrap();
    let pending = running
        .disk
        .next(&mut running.engine, &mut running.store)
        .unwrap()
        .unwrap();
    assert!(matches!(
        pending.record().effect,
        EngineEffect::RemoveFile { .. }
    ));
    let restored = remote(NODE, "report.txt", Some(saved));
    let (_, restored_tree) = koidrive_engine::tree::RemoteNode::from_state(&restored).unwrap();
    running.engine.trees.set_remote(NODE.into(), restored_tree);
    let Running { engine, store, .. } = &mut running;
    store
        .transaction(|tx| {
            tx.put_remote_node(&restored)?;
            engine.checkpoint(tx)
        })
        .unwrap();
    fs::remove_file(folder.root.join("report.txt")).unwrap();
    batch(&mut running, |trees| trees.remove_local(NODE));
    assert_eq!(
        running.engine.trees.class(NODE),
        koidrive_engine::classify::Class::Local
    );
    drop((pending, running));
    let running = folder.start(no_hook()).unwrap();
    assert_eq!(
        running.engine.trees.class(NODE),
        koidrive_engine::classify::Class::Local
    );
}

/// The user writes the planned bytes before the download that would write them, after R moved on
/// to a newer version, and a crash comes before the download. Recovery finds the bytes and reports
/// the download as done, but the observed edit stays the user's.
#[test]
fn an_edit_the_user_made_before_a_recovered_download_stays_an_edit() {
    let folder = Folder::new();
    let mut server = Server::default();
    (SCENARIOS[2].setup)(&folder, &mut server);
    let mut running = folder.start(no_hook()).unwrap();
    let pending = running
        .disk
        .next(&mut running.engine, &mut running.store)
        .unwrap()
        .unwrap();
    assert!(matches!(
        pending.record().effect,
        EngineEffect::WriteFile { .. }
    ));
    let advanced = remote(
        NODE,
        "report.txt",
        Some(server.hold(b"a newer remote version", ChunkerConfig::V0)),
    );
    let (_, advanced_tree) = koidrive_engine::tree::RemoteNode::from_state(&advanced).unwrap();
    running.engine.trees.set_remote(NODE.into(), advanced_tree);
    let Running { engine, store, .. } = &mut running;
    store
        .transaction(|tx| {
            tx.put_remote_node(&advanced)?;
            engine.checkpoint(tx)
        })
        .unwrap();
    fs::write(folder.root.join("report.txt"), REMOTE).unwrap();
    let mut edited = running.local(NODE).unwrap().clone();
    edited.shape.content = Some(disk_content(&folder.root.join("report.txt")));
    batch(&mut running, |trees| trees.set_local(NODE.into(), edited));
    assert_eq!(
        running.engine.trees.class(NODE),
        koidrive_engine::classify::Class::Concurrent
    );
    drop((pending, running));
    let running = folder.start(no_hook()).unwrap();
    assert!(
        running.engine.trees.edited(NODE),
        "recovery hid the observed user edit: {:?}",
        running.engine.trees.class(NODE)
    );
}

/// The user saves the synced bytes again through a temporary file, so the observer sees a new entry
/// with a new modification time but the same content, and R then removes the file. R brings it back
/// while the removal runs. The removal is the effect's own, so no commit deletes the file.
#[test]
fn a_same_byte_save_before_a_removal_does_not_delete_the_restored_file() {
    let folder = Folder::new();
    let server = Server::default();
    let path = folder.root.join("report.txt");
    fs::write(&path, SYNCED).unwrap();
    let saved = disk_content(&path);
    let before = fs::metadata(&path).unwrap();
    folder.seed(&[], &[(NODE, SPACE, "report.txt")], 0);
    let mut running = folder.start(no_hook()).unwrap();
    let mut watching = observer(&folder.root, Snapshot::scan(&folder.root).unwrap());
    let temporary = folder.root.join("atomic-save");
    fs::write(&temporary, SYNCED).unwrap();
    fs::File::options()
        .write(true)
        .open(&temporary)
        .unwrap()
        .set_times(
            fs::FileTimes::new().set_modified(before.modified().unwrap() + Duration::from_secs(1)),
        )
        .unwrap();
    fs::rename(&temporary, &path).unwrap();
    assert_ne!(fs::metadata(&path).unwrap().ino(), before.ino());
    let fresh = observe_until(&mut watching, running.disk.sections().stamp(), |seen| {
        !seen.is_empty()
    });
    running
        .disk
        .observed(fresh, &mut running.engine, &mut running.store)
        .unwrap();
    assert!(koidrive_engine::tree::same(
        running.engine.trees.local().live(NODE),
        running.engine.trees.synced().live(NODE)
    ));
    assert_ne!(
        running.engine.trees.local().live(NODE),
        running.engine.trees.synced().live(NODE)
    );
    let pending = running
        .disk
        .next(&mut running.engine, &mut running.store)
        .unwrap()
        .unwrap();
    assert!(matches!(
        pending.record().effect,
        EngineEffect::RemoveFile { .. }
    ));
    let restored = remote(NODE, "report.txt", Some(saved));
    let (_, restored_tree) = koidrive_engine::tree::RemoteNode::from_state(&restored).unwrap();
    running.engine.trees.set_remote(NODE.into(), restored_tree);
    let Running { engine, store, .. } = &mut running;
    store
        .transaction(|tx| {
            tx.put_remote_node(&restored)?;
            engine.checkpoint(tx)
        })
        .unwrap();
    let staged = ready(pending.stage(&server)).unwrap();
    assert_eq!(
        one(running
            .disk
            .apply(staged, &mut running.engine, &mut running.store)
            .unwrap()),
        Settled::Done
    );
    let engine = &mut running.engine;
    let commit = engine
        .commits
        .build(&mut engine.trees, &mut engine.ids, 1, "history", "me", 2)
        .cloned();
    assert!(
        commit.is_none(),
        "the effect's own removal was committed: {commit:?}"
    );
}

/// A linked folder with its store.
struct Folder {
    dir: tempfile::TempDir,
    root: PathBuf,
    db: PathBuf,
    link: Link,
}

impl Folder {
    fn new() -> Folder {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("space");
        fs::create_dir(&root).unwrap();
        set_marker(&root, 0);
        let db = dir.path().join("store.sqlite");
        let link = Link {
            space_id: SPACE.into(),
            space_name: "test".into(),
            root: root.clone(),
            root_dev: fs::metadata(&root).unwrap().dev(),
            server_url: "https://files.example.com/".into(),
            device_id: "device".into(),
            linked_at_ms: 0,
        };
        let mut store = Store::open(&db).unwrap();
        store.transaction(|tx| tx.set_link(&link)).unwrap();
        Folder {
            dir,
            root,
            db,
            link,
        }
    }

    /// Writes R, S and L rows and the effect counter, with every node dirty.
    fn seed(&self, remote: &[NodeState], held: &[(&str, &str, &str)], cleared: u64) {
        let mut store = Store::open(&self.db).unwrap();
        store
            .transaction(|tx| {
                let mut dirty = BTreeSet::new();
                for node in remote {
                    tx.put_remote_node(node)?;
                    dirty.insert(node.node_id.clone());
                }
                for (id, parent, path) in held {
                    let shape = disk_shape(&self.root, parent, path);
                    tx.put_synced_node(&SyncedNode {
                        node_id: (*id).into(),
                        shape: shape.clone(),
                        pending_for: String::new(),
                    })?;
                    tx.put_local_node(&koidrive_store::LocalNode {
                        node_id: (*id).into(),
                        shape,
                        identity: Snapshot::scan(&self.root).unwrap().entries[Path::new(path)].id,
                        parked: false,
                    })?;
                    dirty.insert((*id).into());
                }
                tx.set_engine_state(&EngineState {
                    trees: TreesState {
                        dirty,
                        ..TreesState::default()
                    },
                    effects: EffectsState {
                        cleared,
                        ..EffectsState::default()
                    },
                    ..EngineState::default()
                })
            })
            .unwrap();
    }

    fn open(&self) -> (Store, Engine) {
        let mut store = Store::open(&self.db).unwrap();
        let engine = store.transaction(|tx| Engine::load(tx, SPACE)).unwrap();
        (store, engine)
    }

    /// Starts the disk, with `hook` at every cut when there is one, as after the observer's first batch of a folder
    /// without rules files: the engine plans.
    fn start(&self, hook: Option<Hook>) -> Result<Running, Stop> {
        let mut running = self.start_unobserved(hook)?;
        running.engine.rules(Vec::new(), true);
        Ok(running)
    }

    /// Starts the disk before any observation.
    fn start_unobserved(&self, hook: Option<Hook>) -> Result<Running, Stop> {
        let (mut store, mut engine) = self.open();
        let lock = claim::lock(&self.root)
            .unwrap()
            .expect("the folder is not locked");
        let disk = match hook {
            Some(hook) => Disk::start_with_hook(&lock, &self.link, &mut engine, &mut store, hook)?,
            None => Disk::start(&lock, &self.link, &mut engine, &mut store)?,
        };
        Ok(Running {
            disk,
            engine,
            store,
            _lock: lock,
        })
    }

    fn marker(&self) -> u64 {
        read_marker(&self.root).unwrap().unwrap().effects
    }

    fn private(&self, dir: &str) -> PathBuf {
        self.root.join(PRIVATE_DIR).join(dir)
    }
}

/// A started disk with what it acts on.
struct Running {
    disk: Disk,
    engine: Engine,
    store: Store,
    _lock: claim::Flock,
}

impl Pending {
    fn record(&self) -> &EffectRecord {
        match self.effects.as_slice() {
            [planned] => &planned.record,
            effects => panic!("a batch of {} effects", effects.len()),
        }
    }
}

fn one(settled: Vec<(String, Settled)>) -> Settled {
    match <[_; 1]>::try_from(settled) {
        Ok([(_, settled)]) => settled,
        Err(settled) => panic!("a batch of {} effects: {settled:?}", settled.len()),
    }
}

impl Running {
    fn step(&mut self, server: &Server) -> Option<Settled> {
        self.step_batch(server).map(one)
    }

    fn step_batch(&mut self, server: &Server) -> Option<Vec<(String, Settled)>> {
        let pending = self.disk.next(&mut self.engine, &mut self.store).unwrap()?;
        let staged = ready(pending.stage(server)).unwrap();
        Some(
            self.disk
                .apply(staged, &mut self.engine, &mut self.store)
                .unwrap(),
        )
    }

    fn local(&self, id: &str) -> Option<&LocalNode> {
        self.engine.trees.local().get(id)
    }
}

/// Chunks held in memory, standing in for the server and the chunk store.
#[derive(Default)]
pub(crate) struct Server {
    pub chunks: HashMap<Vec<u8>, Vec<u8>>,
    versions: HashMap<String, Vec<ChunkRef>>,
    /// Flips a bit of every chunk it hands over.
    corrupt: bool,
    /// How many more downloads fail for now before one goes through.
    pub unavailable: Mutex<u32>,
    /// While set, every download is refused with this error.
    pub refusal: Mutex<Option<ServerError>>,
    pub call_error: Mutex<Option<CallError>>,
    pub missing: BTreeSet<Vec<u8>>,
    /// Runs once, before the first chunk is handed over.
    during: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    calls: Mutex<u32>,
    handed: Mutex<Vec<Vec<u8>>>,
}

impl Server {
    fn hold(&mut self, data: &[u8], config: ChunkerConfig) -> Content {
        let mut refs = Vec::new();
        for chunk in chunks(data, config) {
            let chunk = chunk.unwrap();
            refs.push(chunk.chunk_ref());
            self.chunks.insert(chunk.hash.to_vec(), chunk.data);
        }
        content(refs, REMOTE_MTIME, false, &config)
    }
}

impl Fetch for Server {
    async fn version_chunks(&self, version_id: &str) -> Result<Vec<ChunkRef>, ChunkError> {
        self.versions
            .get(version_id)
            .cloned()
            .ok_or(ChunkError::NoTarget { hash: Vec::new() })
    }

    async fn download(
        &self,
        chunks: &[ChunkRef],
        write: &mut (dyn FnMut(&ChunkRef, &[u8]) -> io::Result<()> + Send),
    ) -> Result<(), ChunkError> {
        *self.calls.lock().unwrap() += 1;
        if let Some(error) = self.call_error.lock().unwrap().clone() {
            return Err(ChunkError::Call(error));
        }
        let missing: Vec<Vec<u8>> = (chunks.iter())
            .filter(|chunk| self.missing.contains(&chunk.hash))
            .map(|chunk| chunk.hash.clone())
            .collect();
        if !missing.is_empty() {
            return Err(ChunkError::Call(CallError::Server(ServerError(
                koidrive_proto::v1::Error {
                    code: koidrive_proto::v1::error::Code::MissingChunks as i32,
                    message: "some chunks are not stored".into(),
                    missing_chunks: missing,
                    retry_after_ms: 0,
                },
            ))));
        }
        if let Some(error) = self.refusal.lock().unwrap().clone() {
            return Err(ChunkError::Refused {
                hash: chunks[0].hash.clone(),
                status: 429,
                error: Some(error),
            });
        }
        {
            let mut unavailable = self.unavailable.lock().unwrap();
            if *unavailable > 0 {
                *unavailable -= 1;
                return Err(ChunkError::Unavailable {
                    hash: chunks[0].hash.clone(),
                    message: "the server is busy".into(),
                });
            }
        }
        let mut handed = BTreeSet::new();
        for chunk in chunks {
            if !handed.insert(chunk.hash.clone()) {
                continue;
            }
            if let Some(during) = self.during.lock().unwrap().take() {
                during();
            }
            let mut bytes =
                self.chunks
                    .get(&chunk.hash)
                    .cloned()
                    .ok_or_else(|| ChunkError::NoTarget {
                        hash: chunk.hash.clone(),
                    })?;
            if self.corrupt {
                bytes[0] ^= 1;
            }
            self.handed.lock().unwrap().push(chunk.hash.clone());
            write(chunk, &bytes).map_err(|source| ChunkError::Io {
                hash: chunk.hash.clone(),
                source,
            })?;
        }
        Ok(())
    }
}

/// The output of a future that never waits, as every future of [`Server`] is.
pub(crate) fn ready<F: Future>(future: F) -> F::Output {
    match pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("the future waited"),
    }
}

/// The payload a crash unwinds with. `resume_unwind` skips the panic hook, so a crash prints nothing.
struct Crash;

fn crash_at(cut: Cut) -> Option<Hook> {
    Some(Box::new(move |reached| {
        if reached == cut {
            resume_unwind(Box::new(Crash));
        }
    }))
}

fn no_hook() -> Option<Hook> {
    None
}

fn set_marker(root: &Path, effects: u64) {
    write_marker(
        root,
        &Marker {
            space_id: SPACE.into(),
            effects,
        },
    )
    .unwrap();
}

/// The content of the file at `path`, as the observer reports it.
fn disk_content(path: &Path) -> Content {
    let meta = fs::metadata(path).unwrap();
    let refs = chunks(fs::File::open(path).unwrap(), ChunkerConfig::V0)
        .map(|chunk| chunk.unwrap().chunk_ref())
        .collect();
    let mtime = i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec());
    content(
        refs,
        mtime as i64,
        meta.mode() & 0o111 != 0,
        &ChunkerConfig::V0,
    )
}

fn disk_shape(root: &Path, parent: &str, path: &str) -> koidrive_store::Shape {
    let full = root.join(path);
    let dir = fs::symlink_metadata(&full).unwrap().is_dir();
    koidrive_store::Shape {
        parent_id: parent.into(),
        name: Path::new(path)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .into(),
        kind: if dir { NodeKind::Dir } else { NodeKind::File } as i32,
        content: (!dir).then(|| disk_content(&full)),
    }
}

fn remote(id: &str, name: &str, content: Option<Content>) -> NodeState {
    NodeState {
        node_id: id.into(),
        parent_id: SPACE.into(),
        name: name.into(),
        kind: if content.is_some() {
            NodeKind::File
        } else {
            NodeKind::Dir
        } as i32,
        status: NodeStatus::Live as i32,
        content_rev: 1,
        meta_rev: 1,
        edit_rev: 1,
        meta_edit_rev: 1,
        trash_group: String::new(),
        current: content.map(|content| VersionInfo {
            version_id: format!("version-{id}"),
            content: Some(content),
            ..VersionInfo::default()
        }),
        pending_for: String::new(),
        pending_by: String::new(),
    }
}

/// The names in the private folder `dir`.
fn names(folder: &Folder, dir: &str) -> Vec<String> {
    match fs::read_dir(folder.private(dir)) {
        Ok(entries) => entries
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Every file and folder under `dir` with the bytes of each file.
fn tree(dir: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut found = BTreeMap::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(next) = todo.pop() {
        for entry in fs::read_dir(&next).unwrap() {
            let path = entry.unwrap().path();
            let relative = path.strip_prefix(dir).unwrap().to_path_buf();
            if path.is_dir() {
                todo.push(path);
                found.insert(relative, None);
            } else {
                found.insert(relative, Some(fs::read(&path).unwrap()));
            }
        }
    }
    found
}

/// Whether some file in the local trash holds `bytes`.
fn trashed(folder: &Folder, bytes: &[u8]) -> bool {
    match fs::metadata(folder.private(TRASH_DIR)) {
        Ok(_) => tree(&folder.private(TRASH_DIR))
            .values()
            .any(|held| held.as_deref() == Some(bytes)),
        Err(_) => false,
    }
}

/// One effect kind on a small tree.
struct Scenario {
    name: &'static str,
    setup: fn(&Folder, &mut Server),
    /// Checks the folder once the effect is done.
    check: fn(&Folder),
    /// The bytes the effect displaces, which must stay at their name or reach the trash.
    displaces: Option<&'static [u8]>,
    /// The first cut after which the next start finds the effect done.
    done_from: Cut,
    /// What a batch puts into L after the plan so that L no longer takes the effect, while the disk stays as it
    /// was. `None` for an effect that L always takes.
    refuse: Option<fn(&mut Trees)>,
}

const OTHER: &str = "0190f0a0-0000-7000-8000-0000000000b2";

/// A node that only L holds, as after a batch that reported an entry the disk no longer has.
fn other(parent: &str, name: &str, kind: Kind) -> LocalNode {
    let content = (kind == Kind::File).then(|| Content {
        size: 1,
        manifest_digest: vec![7; 32],
        ..Content::default()
    });
    LocalNode {
        shape: Shape {
            parent: parent.into(),
            name: name.into(),
            kind,
            content,
        },
        identity: FileIdentity {
            dev: 1,
            ino: 1,
            birth_ns: None,
        },
        parked: false,
    }
}

const SCENARIOS: [Scenario; 6] = [
    Scenario {
        name: "create a folder",
        setup: |folder, _| folder.seed(&[remote(NODE, "docs", None)], &[], 0),
        check: |folder| assert!(folder.root.join("docs").is_dir()),
        displaces: None,
        done_from: Cut::Step(Step::Placed),
        refuse: Some(|trees| trees.set_local(OTHER.into(), other(SPACE, "docs", Kind::Dir))),
    },
    Scenario {
        name: "write a new file",
        setup: |folder, server| {
            let content = server.hold(REMOTE, ChunkerConfig::V0);
            folder.seed(&[remote(NODE, "report.txt", Some(content))], &[], 0);
        },
        check: |folder| {
            let path = folder.root.join("report.txt");
            assert_eq!(fs::read(&path).unwrap(), REMOTE);
            let meta = fs::metadata(&path).unwrap();
            assert_eq!(
                i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
                i128::from(REMOTE_MTIME)
            );
        },
        displaces: None,
        done_from: Cut::Step(Step::Placed),
        refuse: Some(|trees| trees.set_local(OTHER.into(), other(SPACE, "report.txt", Kind::Dir))),
    },
    Scenario {
        name: "replace a file",
        setup: |folder, server| {
            fs::write(folder.root.join("report.txt"), SYNCED).unwrap();
            let content = server.hold(REMOTE, ChunkerConfig::V0);
            folder.seed(
                &[remote(NODE, "report.txt", Some(content))],
                &[(NODE, SPACE, "report.txt")],
                0,
            );
        },
        check: |folder| assert_eq!(fs::read(folder.root.join("report.txt")).unwrap(), REMOTE),
        displaces: Some(SYNCED),
        done_from: Cut::Step(Step::Quarantined),
        refuse: Some(|trees| {
            let mut renamed = trees.local().get(NODE).unwrap().clone();
            renamed.shape.name = "renamed.txt".into();
            trees.set_local(NODE.into(), renamed);
            trees.set_local(OTHER.into(), other(SPACE, "report.txt", Kind::File));
        }),
    },
    Scenario {
        name: "move a file",
        setup: |folder, _| {
            fs::write(folder.root.join("report.txt"), SYNCED).unwrap();
            let content = disk_content(&folder.root.join("report.txt"));
            folder.seed(
                &[remote(NODE, "renamed.txt", Some(content))],
                &[(NODE, SPACE, "report.txt")],
                0,
            );
        },
        check: |folder| {
            assert!(!folder.root.join("report.txt").exists());
            assert_eq!(fs::read(folder.root.join("renamed.txt")).unwrap(), SYNCED);
        },
        displaces: None,
        done_from: Cut::Step(Step::Placed),
        refuse: Some(|trees| {
            trees.set_local(OTHER.into(), other(SPACE, "renamed.txt", Kind::File))
        }),
    },
    Scenario {
        name: "remove a file",
        setup: |folder, _| {
            fs::write(folder.root.join("report.txt"), SYNCED).unwrap();
            folder.seed(&[], &[(NODE, SPACE, "report.txt")], 0);
        },
        check: |folder| assert!(!folder.root.join("report.txt").exists()),
        displaces: Some(SYNCED),
        done_from: Cut::Step(Step::Quarantined),
        refuse: None,
    },
    Scenario {
        name: "remove a folder",
        setup: |folder, _| {
            fs::create_dir(folder.root.join("docs")).unwrap();
            folder.seed(&[], &[(NODE, SPACE, "docs")], 0);
        },
        check: |folder| assert!(!folder.root.join("docs").exists()),
        displaces: None,
        done_from: Cut::Step(Step::Quarantined),
        refuse: Some(|trees| trees.set_local(OTHER.into(), other(NODE, "new.txt", Kind::File))),
    },
];

/// The cuts an uninterrupted run of the scenario reaches, in order.
fn cuts(scenario: &Scenario) -> Vec<Cut> {
    let folder = Folder::new();
    let mut server = Server::default();
    (scenario.setup)(&folder, &mut server);
    let reached = Arc::new(Mutex::new(Vec::new()));
    let record = reached.clone();
    let mut running = folder
        .start(Some(Box::new(move |cut| record.lock().unwrap().push(cut))))
        .unwrap();
    assert_eq!(
        running.step(&server),
        Some(Settled::Done),
        "{}",
        scenario.name
    );
    reached.lock().unwrap().clone()
}

/// Checks a folder whose effect is done and acknowledged: L holds R's shape and S holds L's, in memory and in the
/// store, the disk holds the effect's result, the marker equals the effect counter, and nothing is left in
/// quarantine or staging.
fn assert_done(scenario: &Scenario, folder: &Folder, running: &Running, context: &str) {
    assert!(running.engine.effects.in_flight().is_empty(), "{context}");
    assert!(running.engine.effects.blocked().is_empty(), "{context}");
    let remote = running.engine.trees.remote().live(NODE);
    let local = running.local(NODE).map(|node| &node.shape);
    assert!(
        koidrive_engine::tree::same(local, remote),
        "{context}: L {local:?}, R {remote:?}"
    );
    // The acknowledgement settles the trees in its own transaction. With S left behind, the node's next change
    // would count as concurrent.
    let synced = running.engine.trees.synced().get(NODE);
    assert_eq!(synced, local, "{context}: S");
    let (_, stored) = folder.open();
    assert_eq!(
        stored.trees.synced().get(NODE),
        local,
        "{context}: S in the store"
    );
    (scenario.check)(folder);
    assert_eq!(
        folder.marker(),
        running.engine.effects.cleared(),
        "{context}"
    );
    assert_eq!(
        names(folder, QUARANTINE_DIR),
        Vec::<String>::new(),
        "{context}"
    );
    assert_eq!(
        names(folder, STAGING_DIR),
        Vec::<String>::new(),
        "{context}"
    );
    if let Some(bytes) = scenario.displaces {
        assert!(
            trashed(folder, bytes),
            "{context}: the displaced bytes are not in the trash"
        );
    }
}

/// Crashes the scenario's effect at `cut`, starts again, checks what the start left, and finishes the effect.
fn crash_and_finish(scenario: &Scenario, cut: Cut, cuts: &[Cut]) {
    let context = format!("{} crashed at {cut:?}", scenario.name);
    let folder = Folder::new();
    let mut server = Server::default();
    (scenario.setup)(&folder, &mut server);
    let crashed = catch_unwind(AssertUnwindSafe(|| {
        let mut running = folder.start(crash_at(cut)).unwrap();
        running.step(&server);
    }));
    assert!(
        crashed.is_err_and(|payload| payload.is::<Crash>()),
        "{context}: no crash"
    );

    let mut running = folder.start(no_hook()).unwrap();
    assert!(running.engine.effects.in_flight().is_empty(), "{context}");
    assert_eq!(
        folder.marker(),
        running.engine.effects.cleared(),
        "{context}"
    );
    assert_eq!(
        names(&folder, QUARANTINE_DIR),
        Vec::<String>::new(),
        "{context}"
    );
    assert_eq!(
        names(&folder, STAGING_DIR),
        Vec::<String>::new(),
        "{context}"
    );
    if let Some(bytes) = scenario.displaces {
        let at_name = fs::read(folder.root.join("report.txt")).is_ok_and(|held| held == bytes);
        assert!(
            at_name || trashed(&folder, bytes),
            "{context}: the displaced bytes are neither at their name nor in the trash"
        );
    }
    let position = |wanted: Cut| cuts.iter().position(|reached| *reached == wanted);
    let done = position(cut) >= position(scenario.done_from);
    let planned = cut != Cut::BeforePlan;
    let cleared = running.engine.effects.cleared();
    assert_eq!(cleared, u64::from(planned), "{context}");
    let again = running.step(&server);
    if done {
        assert_eq!(
            again, None,
            "{context}: done at the start, yet planned again"
        );
    } else {
        assert_eq!(again, Some(Settled::Done), "{context}: not planned again");
    }
    assert_eq!(running.step(&server), None, "{context}");
    assert_done(scenario, &folder, &running, &context);
}

#[test]
fn a_crash_at_every_cut_of_every_effect_is_recovered_on_the_next_start() {
    for scenario in &SCENARIOS {
        let cuts = cuts(scenario);
        assert_eq!(cuts.first(), Some(&Cut::BeforePlan), "{}", scenario.name);
        assert_eq!(cuts.last(), Some(&Cut::AfterAck), "{}", scenario.name);
        assert!(
            cuts.contains(&scenario.done_from),
            "{}: {cuts:?}",
            scenario.name
        );
        for &cut in &cuts {
            crash_and_finish(scenario, cut, &cuts);
        }
    }
}

#[test]
fn a_replaced_file_the_user_changed_before_the_start_leaves_s() {
    let remove: fn(&Path) = |file| fs::remove_file(file).unwrap();
    let write: fn(&Path) = |file| fs::write(file, b"edited").unwrap();
    let cases = [
        (Cut::Step(Step::Quarantined), write),
        (Cut::Step(Step::Placed), remove),
        (Cut::Step(Step::Placed), write),
        (Cut::Step(Step::Trashed), remove),
        (Cut::Step(Step::Trashed), write),
        (Cut::BeforeAck, remove),
        (Cut::BeforeAck, write),
    ];
    for (cut, change) in cases {
        let folder = Folder::new();
        let mut server = Server::default();
        (SCENARIOS[2].setup)(&folder, &mut server);
        let crashed = catch_unwind(AssertUnwindSafe(|| {
            let mut running = folder.start(crash_at(cut)).unwrap();
            running.step(&server);
        }));
        assert!(
            crashed.is_err_and(|payload| payload.is::<Crash>()),
            "{cut:?}: no crash"
        );
        change(&folder.root.join("report.txt"));
        let running = folder.start(no_hook()).unwrap();
        assert_eq!(running.engine.trees.synced().get(NODE), None, "{cut:?}");
        let (_, stored) = folder.open();
        assert_eq!(stored.trees.synced().get(NODE), None, "{cut:?}");
        assert!(trashed(&folder, SYNCED), "{cut:?}");
    }
}

#[test]
fn a_name_the_user_takes_while_its_replaced_file_is_in_quarantine_leaves_s() {
    let folder = Folder::new();
    let mut server = Server::default();
    (SCENARIOS[2].setup)(&folder, &mut server);
    let target = folder.root.join("report.txt");
    let hook: Option<Hook> = Some(Box::new(move |reached| {
        if reached == Cut::Step(Step::Quarantined) {
            fs::write(&target, b"saved by the user").unwrap();
        }
    }));
    let mut running = folder.start(hook).unwrap();
    assert!(matches!(running.step(&server), Some(Settled::Failed(_))));
    assert_eq!(running.engine.trees.synced().get(NODE), None);
    assert!(trashed(&folder, SYNCED));
}

#[test]
fn a_crash_during_recovery_is_recovered_on_the_start_after() {
    for scenario in SCENARIOS
        .iter()
        .filter(|scenario| scenario.displaces.is_some())
    {
        for recovery_cut in [
            Cut::Step(Step::Marked),
            Cut::Step(Step::Compared),
            Cut::Step(Step::Placed),
            Cut::Step(Step::Trashed),
            Cut::BeforeAck,
            Cut::AfterAck,
        ] {
            let context = format!(
                "{} crashed at Quarantined, then at {recovery_cut:?}",
                scenario.name
            );
            let folder = Folder::new();
            let mut server = Server::default();
            (scenario.setup)(&folder, &mut server);
            let _ = catch_unwind(AssertUnwindSafe(|| {
                let mut running = folder
                    .start(crash_at(Cut::Step(Step::Quarantined)))
                    .unwrap();
                running.step(&server);
            }));
            let reached =
                catch_unwind(AssertUnwindSafe(|| folder.start(crash_at(recovery_cut)))).is_err();
            let mut running = folder.start(no_hook()).unwrap();
            assert_eq!(
                running.step(&server),
                None,
                "{context} (crashed: {reached})"
            );
            assert_done(scenario, &folder, &running, &context);
        }
    }
}

/// Changes L as a batch does: in one transaction that ends with the checkpoint.
fn batch(running: &mut Running, change: impl FnOnce(&mut Trees)) {
    change(&mut running.engine.trees);
    let Running { engine, store, .. } = running;
    store.transaction(|tx| engine.checkpoint(tx)).unwrap();
}

/// What the user sees under the root.
fn visible(folder: &Folder) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut found = tree(&folder.root);
    found.retain(|path, _| !path.starts_with(PRIVATE_DIR));
    found
}

/// Plans and stages the scenario's effect, then lets a batch change L so that it no longer takes the effect. With
/// `apply` the effect is applied next and the process stops at `cut`, if any; without it the process stops before
/// the effect section. Then starts again, checks that the store loads and nothing under the root changed, and
/// finishes the effect once a batch has taken the stale node out of L.
fn refuse_and_finish(scenario: &Scenario, refuse: fn(&mut Trees), cut: Option<Cut>, apply: bool) {
    let context = format!(
        "{}, refused, stopped at {cut:?}, applied: {apply}",
        scenario.name
    );
    let folder = Folder::new();
    let mut server = Server::default();
    (scenario.setup)(&folder, &mut server);
    let seen = visible(&folder);
    let (_, planned_for) = folder.open();
    let stopped = catch_unwind(AssertUnwindSafe(|| {
        let mut running = folder.start(cut.and_then(crash_at)).unwrap();
        let (disk, engine, store) = (&mut running.disk, &mut running.engine, &mut running.store);
        let pending = disk.next(engine, store).unwrap().expect("an effect");
        let staged = ready(pending.stage(&server)).unwrap();
        batch(&mut running, refuse);
        assert!(
            !pending
                .record()
                .applies(&running.engine.trees, running.engine.commits.in_flight()),
            "{context}"
        );
        if apply {
            let (disk, engine, store) =
                (&mut running.disk, &mut running.engine, &mut running.store);
            let settled = one(disk.apply(staged, engine, store).unwrap());
            let why = "L changed since it was planned";
            assert_eq!(settled, Settled::Failed(why.into()), "{context}");
        }
    }));
    match stopped {
        Err(payload) if !payload.is::<Crash>() => resume_unwind(payload),
        stopped => assert_eq!(stopped.is_err(), cut.is_some(), "{context}"),
    }

    let mut running = folder.start(no_hook()).unwrap();
    assert_eq!(running.engine.trees.check(), Ok(()), "{context}");
    let (_, stored) = folder.open();
    assert_eq!(
        stored.trees.local(),
        running.engine.trees.local(),
        "{context}"
    );
    assert!(running.engine.effects.in_flight().is_empty(), "{context}");
    assert!(running.engine.effects.blocked().is_empty(), "{context}");
    assert_eq!(running.engine.effects.cleared(), 1, "{context}");
    assert_eq!(folder.marker(), 1, "{context}");
    assert_eq!(
        names(&folder, QUARANTINE_DIR),
        Vec::<String>::new(),
        "{context}"
    );
    assert_eq!(
        names(&folder, STAGING_DIR),
        Vec::<String>::new(),
        "{context}"
    );
    assert_eq!(visible(&folder), seen, "{context}");
    assert_eq!(
        running.step(&server),
        None,
        "{context}: planned while L holds the stale node"
    );

    batch(&mut running, |trees| {
        let held: Vec<String> = trees.local().iter().map(|(id, _)| id.clone()).collect();
        for id in held {
            trees.remove_local(&id);
        }
        for (id, node) in planned_for.trees.local().iter() {
            trees.set_local(id.clone(), node.clone());
        }
    });
    assert_eq!(
        running.step(&server),
        Some(Settled::Done),
        "{context}: not planned again"
    );
    assert_eq!(running.step(&server), None, "{context}");
    assert_done(scenario, &folder, &running, &context);
}

#[test]
fn an_effect_l_no_longer_takes_leaves_a_store_that_loads_at_every_cut() {
    for scenario in &SCENARIOS {
        let Some(refuse) = scenario.refuse else {
            continue;
        };
        refuse_and_finish(scenario, refuse, None, false);
        refuse_and_finish(scenario, refuse, None, true);
        for cut in [Cut::Step(Step::Marked), Cut::BeforeAck, Cut::AfterAck] {
            refuse_and_finish(scenario, refuse, Some(cut), true);
        }
    }
}

/// The marker is the only thing an effect that L no longer takes writes under the root.
#[test]
fn an_effect_l_no_longer_takes_reaches_no_cut_that_changes_the_folder() {
    for scenario in &SCENARIOS {
        let Some(refuse) = scenario.refuse else {
            continue;
        };
        let folder = Folder::new();
        let mut server = Server::default();
        (scenario.setup)(&folder, &mut server);
        let reached = Arc::new(Mutex::new(Vec::new()));
        let record = reached.clone();
        let hook: Hook = Box::new(move |cut| record.lock().unwrap().push(cut));
        let mut running = folder.start(Some(hook)).unwrap();
        let (disk, engine, store) = (&mut running.disk, &mut running.engine, &mut running.store);
        let pending = disk.next(engine, store).unwrap().expect("an effect");
        let staged = ready(pending.stage(&server)).unwrap();
        batch(&mut running, refuse);
        let (disk, engine, store) = (&mut running.disk, &mut running.engine, &mut running.store);
        disk.apply(staged, engine, store).unwrap();
        assert_eq!(
            *reached.lock().unwrap(),
            [
                Cut::BeforePlan,
                Cut::AfterPlan,
                Cut::Step(Step::Marked),
                Cut::BeforeAck,
                Cut::AfterAck
            ],
            "{}",
            scenario.name
        );
    }
}

/// A folder whose store has cleared three effects and holds a fourth in flight, planned for writing a file.
fn three_cleared_and_one_in_flight() -> (Folder, Server, String) {
    let folder = Folder::new();
    let mut server = Server::default();
    let content = server.hold(REMOTE, ChunkerConfig::V0);
    folder.seed(&[remote(NODE, "report.txt", Some(content))], &[], 3);
    set_marker(&folder.root, 3);
    let _ = catch_unwind(AssertUnwindSafe(|| {
        folder
            .start(crash_at(Cut::AfterPlan))
            .unwrap()
            .step(&server);
    }));
    let (_, engine) = folder.open();
    let record = engine.effects.in_flight().first().unwrap().clone();
    assert_eq!(record.counter, 4);
    (folder, server, record.effect_id)
}

#[test]
fn a_marker_below_the_effect_counter_pauses_without_writing() {
    let folder = Folder::new();
    folder.seed(&[], &[], 3);
    set_marker(&folder.root, 2);
    let before = tree(&folder.root);
    assert!(matches!(
        folder.start(no_hook()),
        Err(Stop::DiskBehind {
            marker: 2,
            cleared: 3
        })
    ));
    assert_eq!(tree(&folder.root), before);
}

#[test]
fn a_marker_equal_to_the_effect_counter_starts() {
    let folder = Folder::new();
    folder.seed(&[], &[], 3);
    set_marker(&folder.root, 3);
    let running = folder.start(no_hook()).unwrap();
    assert_eq!(running.engine.effects.cleared(), 3);
    assert_eq!(folder.marker(), 3);
}

#[test]
fn a_marker_one_above_with_an_effect_in_flight_is_recovered() {
    let (folder, server, _) = three_cleared_and_one_in_flight();
    set_marker(&folder.root, 4);
    let mut running = folder.start(no_hook()).unwrap();
    assert_eq!(running.engine.effects.cleared(), 4);
    assert_eq!(folder.marker(), 4);
    assert_eq!(running.step(&server), Some(Settled::Done));
    assert_eq!(folder.marker(), 5);
    assert_eq!(fs::read(folder.root.join("report.txt")).unwrap(), REMOTE);
}

#[test]
fn a_marker_above_what_the_store_accounts_for_continues_from_the_marker_and_recovers() {
    let (folder, _, effect_id) = three_cleared_and_one_in_flight();
    set_marker(&folder.root, 6);
    fs::create_dir_all(folder.private(QUARANTINE_DIR)).unwrap();
    fs::write(folder.private(QUARANTINE_DIR).join(&effect_id), SYNCED).unwrap();
    let report = fs::read(folder.root.join("report.txt")).ok();
    let running = folder.start(no_hook()).unwrap();
    assert_eq!(running.engine.effects.cleared(), 6);
    assert!(running.engine.effects.in_flight().is_empty());
    assert_eq!(folder.marker(), 6);
    assert_eq!(fs::read(folder.root.join("report.txt")).ok(), report);
    assert_eq!(
        fs::read_dir(folder.private(QUARANTINE_DIR))
            .unwrap()
            .count(),
        0
    );
    let back: Vec<Vec<u8>> = (fs::read_dir(&folder.root).unwrap())
        .map(|entry| entry.unwrap())
        .filter(|entry| entry.file_name().to_string_lossy().contains(" (recovered "))
        .map(|entry| fs::read(entry.path()).unwrap())
        .collect();
    assert_eq!(back, [SYNCED]);
    drop(running);
    let (_, engine) = folder.open();
    assert_eq!(engine.effects.cleared(), 6);
    assert!(engine.effects.in_flight().is_empty());
    assert!(matches!(
        engine.commits.halted(),
        Some(CommitHalt::Recovery(_))
    ));

    let idle = Folder::new();
    idle.seed(&[], &[], 3);
    set_marker(&idle.root, 4);
    let running = idle.start(no_hook()).unwrap();
    assert_eq!(running.engine.effects.cleared(), 4);
    assert!(matches!(
        running.engine.commits.halted(),
        Some(CommitHalt::Recovery(_))
    ));
}

#[test]
fn a_missing_marker_halts_without_writing() {
    let folder = Folder::new();
    folder.seed(&[], &[], 0);
    fs::remove_file(folder.root.join(PRIVATE_DIR).join("space.json")).unwrap();
    let before = tree(&folder.root);
    assert!(matches!(
        folder.start(no_hook()),
        Err(Stop::Root(RootProblem::NoMarker))
    ));
    assert_eq!(tree(&folder.root), before);
}

#[test]
fn an_interrupted_effect_is_planned_again_after_a_restart() {
    let folder = Folder::new();
    let mut server = Server::default();
    (SCENARIOS[1].setup)(&folder, &mut server);
    let _ = catch_unwind(AssertUnwindSafe(|| {
        folder
            .start(crash_at(Cut::Step(Step::Marked)))
            .unwrap()
            .step(&server);
    }));
    let (_, engine) = folder.open();
    let interrupted = engine
        .effects
        .in_flight()
        .first()
        .unwrap()
        .effect_id
        .clone();
    let mut running = folder.start(no_hook()).unwrap();
    assert!(running.engine.effects.blocked().is_empty());
    let pending = running
        .disk
        .next(&mut running.engine, &mut running.store)
        .unwrap()
        .expect("planned again");
    assert_eq!(pending.record().node, NODE);
    assert_ne!(pending.record().effect_id, interrupted);
    assert_eq!(
        pending.record().effect,
        engine.effects.in_flight().first().unwrap().effect
    );
}

#[test]
fn a_start_that_recovers_an_effect_has_its_paths_read_again() {
    let folder = Folder::new();
    let mut server = Server::default();
    (SCENARIOS[1].setup)(&folder, &mut server);
    let _ = catch_unwind(AssertUnwindSafe(|| {
        folder
            .start(crash_at(Cut::Step(Step::Placed)))
            .unwrap()
            .step(&server);
    }));
    let running = folder.start(no_hook()).unwrap();
    let sections = running.disk.sections();
    let mut watching = observer(&folder.root, running.engine.snapshot());
    let resets = catch_up(&mut watching, &sections);
    let scopes: Vec<&Path> = resets.iter().map(|reset| reset.scope.as_path()).collect();
    assert_eq!(scopes, [Path::new("report.txt")]);
    let read = settle(&mut watching).into_iter().any(|change| {
        matches!(change, Observed::Modified { path, content: Some(_), .. } if path == Path::new("report.txt"))
    });
    assert!(read, "the file the effect placed before the crash is read");
    drop((watching, running));

    let running = folder.start(no_hook()).unwrap();
    assert!(running.disk.sections().take_resets().0.is_empty());
    let mut watching = observer(&folder.root, running.engine.snapshot());
    assert_eq!(
        settle(&mut watching),
        [],
        "a file that matches L is trusted"
    );
}

#[test]
fn an_effect_that_met_a_taken_name_or_a_changed_file_stays_blocked() {
    for (scenario, disturb) in [
        (&SCENARIOS[1], "another entry holds its name"),
        (&SCENARIOS[2], "the entry changed on this device"),
    ] {
        let folder = Folder::new();
        let mut server = Server::default();
        (scenario.setup)(&folder, &mut server);
        let mut running = folder.start(no_hook()).unwrap();
        let pending = running
            .disk
            .next(&mut running.engine, &mut running.store)
            .unwrap()
            .unwrap();
        let staged = ready(pending.stage(&server)).unwrap();
        fs::write(folder.root.join("report.txt"), b"the user's own words").unwrap();
        let settled = running
            .disk
            .apply(staged, &mut running.engine, &mut running.store)
            .unwrap();
        assert_eq!(one(settled), Settled::Failed(disturb.into()));
        assert_eq!(
            fs::read(folder.root.join("report.txt")).unwrap(),
            b"the user's own words"
        );
        assert_eq!(running.step(&server), None, "{disturb}");
        drop((running, pending));
        let mut running = folder.start(no_hook()).unwrap();
        assert!(
            running.engine.effects.blocked().contains_key(NODE),
            "{disturb}"
        );
        assert_eq!(running.step(&server), None, "{disturb}");
        assert_eq!(folder.marker(), 1);
    }
}

/// The user edits a synced file that the other device also edited, in a scan of more files than one observation
/// reads, so the edit is reported without its content and L keeps the synced content. The download of the other
/// device's edit must not replace the file. Once the edit is read, the download waits and the next commit forks the
/// edit off the node into a file of its own, which the server keeps apart as a conflict copy.
#[test]
fn a_remote_edit_never_replaces_a_file_whose_read_was_deferred() {
    let folder = Folder::new();
    let mut server = Server::default();
    (SCENARIOS[2].setup)(&folder, &mut server);
    let previous = Snapshot::scan(&folder.root).unwrap();
    for i in 0..READ_FILES {
        fs::write(folder.root.join(format!("{i:04}.txt")), b"x").unwrap();
    }
    let report = folder.root.join("report.txt");
    fs::write(&report, b"the user's own words").unwrap();
    let mut running = folder.start(no_hook()).unwrap();
    let sections = running.disk.sections();
    let mut watching = observer(&folder.root, previous);

    let deferred = observe_until(&mut watching, sections.stamp(), |seen| !seen.is_empty());
    assert!(
        deferred.observation.changes.iter().any(|change| matches!(
            change,
            Observed::Modified { path, content: None, .. } if path == Path::new("report.txt")
        )),
        "the edit is past the budget"
    );
    running
        .disk
        .observed(deferred, &mut running.engine, &mut running.store)
        .unwrap();
    assert_eq!(
        running.step(&server),
        Some(Settled::Failed("the entry changed on this device".into()))
    );
    assert_eq!(fs::read(&report).unwrap(), b"the user's own words");

    catch_up(&mut watching, &sections);
    let read = observe_until(&mut watching, sections.stamp(), |seen| {
        seen.iter().any(|change| {
            matches!(change, Observed::Modified { path, content: Some(_), .. } if path == Path::new("report.txt"))
        })
    });
    running
        .disk
        .observed(read, &mut running.engine, &mut running.store)
        .unwrap();
    assert_eq!(running.step(&server), None);
    let engine = &mut running.engine;
    engine
        .commits
        .build(&mut engine.trees, &mut engine.ids, 1, "history", "me", 2);
    let edit = disk_content(&report);
    let at_report: Vec<(&String, &LocalNode)> = (running.engine.trees.local().iter())
        .filter(|(_, node)| node.shape.name == "report.txt")
        .collect();
    assert!(
        matches!(
            at_report.as_slice(),
            [(id, node)] if id.as_str() != NODE
                && node.shape.content.as_ref().map(|content| &content.manifest_digest)
                    == Some(&edit.manifest_digest)
        ),
        "the edit is forked off the node: {at_report:?}"
    );
    assert_eq!(running.step(&server), None);
    assert_eq!(fs::read(&report).unwrap(), b"the user's own words");
}

/// The user replaces a synced folder with another one and moves the folder's synced file back into it, while the
/// observer's snapshot predates the file, in a scan of more files than one observation reads. The batch takes the
/// replaced folder out of L, so the file is read at once although it is past the budget, and keeps its node.
#[test]
fn a_file_created_under_a_replaced_folder_is_read_at_once_and_keeps_its_node() {
    let folder = Folder::new();
    let d = "0190f0a0-0000-7000-8000-0000000000d1";
    let incoming = "0190f0a0-0000-7000-8000-0000000000d2";
    fs::create_dir(folder.root.join("d")).unwrap();
    fs::create_dir(folder.root.join("incoming")).unwrap();
    let previous = Snapshot::scan(&folder.root).unwrap();
    fs::write(folder.root.join("d/x"), SYNCED).unwrap();
    let mut child = remote(NODE, "x", Some(disk_content(&folder.root.join("d/x"))));
    child.parent_id = d.into();
    folder.seed(
        &[
            remote(d, "d", None),
            remote(incoming, "incoming", None),
            child,
        ],
        &[
            (d, SPACE, "d"),
            (incoming, SPACE, "incoming"),
            (NODE, d, "d/x"),
        ],
        0,
    );
    let mut running = folder.start(no_hook()).unwrap();
    fs::rename(folder.root.join("d/x"), folder.dir.path().join("x")).unwrap();
    fs::rename(folder.root.join("incoming"), folder.root.join("d")).unwrap();
    fs::rename(folder.dir.path().join("x"), folder.root.join("d/x")).unwrap();
    for i in 0..READ_FILES {
        fs::write(folder.root.join(format!("{i:04}.txt")), b"a").unwrap();
    }
    let mut watching = observer(&folder.root, previous);
    let stamp = running.disk.sections().stamp();

    let first = observe_until(&mut watching, stamp, |seen| !seen.is_empty());
    assert!(
        first.observation.changes.iter().any(|change| matches!(
            change,
            Observed::Created { path, content: Some(_), .. } if path == Path::new("d/x")
        )),
        "the file is read although it is past the budget"
    );
    running
        .disk
        .observed(first, &mut running.engine, &mut running.store)
        .unwrap();
    assert_eq!(running.engine.path_of(NODE), Some(PathBuf::from("d/x")));
}

/// The user renames a synced file, which has the bytes of a download that stopped before it touched the disk, onto
/// the download's name. Recovery finds those bytes at the name (design §5.3), but L holds that entry as the other
/// node, so the download fails instead of taking the entry.
#[test]
fn a_restart_does_not_take_a_file_that_l_holds_as_another_node_for_a_download() {
    let folder = Folder::new();
    let mut server = Server::default();
    let content = server.hold(REMOTE, ChunkerConfig::V0);
    fs::write(folder.root.join("b.txt"), REMOTE).unwrap();
    let synced = disk_content(&folder.root.join("b.txt"));
    folder.seed(
        &[
            remote(NODE, "a.txt", Some(content)),
            remote(OTHER, "b.txt", Some(synced)),
        ],
        &[(OTHER, SPACE, "b.txt")],
        0,
    );
    let mut running = folder.start(no_hook()).unwrap();
    let pending = running
        .disk
        .next(&mut running.engine, &mut running.store)
        .unwrap()
        .expect("the download");
    assert_eq!(pending.record().node, NODE);
    drop((running, pending));
    fs::rename(folder.root.join("b.txt"), folder.root.join("a.txt")).unwrap();

    let running = folder.start(no_hook()).unwrap();
    let held = running.local(OTHER).unwrap().identity;
    assert_ne!(running.local(NODE).map(|node| node.identity), Some(held));
    assert!(running.engine.effects.blocked().contains_key(NODE));
    assert_eq!(fs::read(folder.root.join("a.txt")).unwrap(), REMOTE);
}

/// The user renames a folder or a file whose removal another device asked for, and a batch reports the rename before
/// the removal applies. A restart finds nothing at the old name (design §5.3), but L shows the node moved, so the
/// removal fails and L keeps the node where the user put it.
#[test]
fn a_restart_does_not_take_a_renamed_entry_for_removed() {
    for folder_kind in [true, false] {
        let folder = Folder::new();
        let (from, to) = (folder.root.join("docs"), folder.root.join("notes"));
        if folder_kind {
            fs::create_dir(&from).unwrap();
        } else {
            fs::write(&from, SYNCED).unwrap();
        }
        folder.seed(&[], &[(NODE, SPACE, "docs")], 0);
        let mut running = folder.start(no_hook()).unwrap();
        let pending = running
            .disk
            .next(&mut running.engine, &mut running.store)
            .unwrap()
            .expect("the removal");
        fs::rename(&from, &to).unwrap();
        batch(&mut running, |trees| {
            let mut renamed = trees.local().get(NODE).unwrap().clone();
            renamed.shape.name = "notes".into();
            trees.set_local(NODE.into(), renamed);
        });
        drop((running, pending));

        let running = folder.start(no_hook()).unwrap();
        let name = running.local(NODE).map(|node| node.shape.name.as_str());
        assert_eq!(name, Some("notes"), "folder: {folder_kind}");
        assert!(
            running.engine.effects.blocked().contains_key(NODE),
            "folder: {folder_kind}"
        );
        assert!(to.exists(), "folder: {folder_kind}");
    }
}

/// Moves the root aside and puts an empty folder at its path.
fn replace_root(root: &Path) -> PathBuf {
    let aside = root.with_file_name("moved");
    fs::rename(root, &aside).unwrap();
    fs::create_dir(root).unwrap();
    aside
}

#[test]
fn a_root_replaced_during_a_download_receives_nothing() {
    let folder = Folder::new();
    let mut server = Server::default();
    (SCENARIOS[1].setup)(&folder, &mut server);
    let root = folder.root.clone();
    *server.during.lock().unwrap() = Some(Box::new(move || {
        replace_root(&root);
    }));
    let mut running = folder.start(no_hook()).unwrap();
    let pending = running
        .disk
        .next(&mut running.engine, &mut running.store)
        .unwrap()
        .unwrap();
    match ready(pending.stage(&server)) {
        Err(Stall::Halt(stop)) => assert!(stop.to_string().contains("no longer names"), "{stop}"),
        other => panic!("{:?}", other.map(|_| ())),
    }
    assert_eq!(tree(&folder.root), BTreeMap::new());
    assert!(!running.engine.effects.in_flight().is_empty());
    let aside = folder.dir.path().join("moved");
    assert!(!aside.join("report.txt").exists());
}

#[test]
fn a_root_replaced_before_an_effect_applies_receives_nothing() {
    let folder = Folder::new();
    let mut server = Server::default();
    (SCENARIOS[1].setup)(&folder, &mut server);
    let mut running = folder.start(no_hook()).unwrap();
    let pending = running
        .disk
        .next(&mut running.engine, &mut running.store)
        .unwrap()
        .unwrap();
    let staged = ready(pending.stage(&server)).unwrap();
    let aside = replace_root(&folder.root);
    let before = tree(&aside);
    assert!(matches!(
        running
            .disk
            .apply(staged, &mut running.engine, &mut running.store),
        Err(Stop::Withdrawn(_))
    ));
    assert_eq!(tree(&folder.root), BTreeMap::new());
    assert_eq!(tree(&aside), before);
    drop(running);
    let (_, engine) = folder.open();
    assert!(!engine.effects.in_flight().is_empty());
}

/// Observes until `done` accepts the changes seen so far, and returns them as one batch.
fn observe_until(
    observer: &mut FsObserver,
    stamp: Stamp,
    done: impl Fn(&[Observed]) -> bool,
) -> Batch {
    let (mut seen, mut rules) = (Vec::new(), Vec::new());
    loop {
        if let Some(observation) = observer.observe(Duration::from_millis(200)).unwrap() {
            seen.extend(observation.changes.iter().cloned());
            rules.extend(observation.rules.iter().cloned());
            if done(&seen) {
                return Batch {
                    stamp,
                    answered: None,
                    observation: Observation {
                        changes: seen,
                        rules,
                        ..observation
                    },
                };
            }
        }
    }
}

fn observer(root: &Path, previous: Snapshot) -> FsObserver {
    let dev = fs::metadata(root).unwrap().dev();
    FsObserver::start(root, SPACE, dev, previous, Arc::new(Unfiltered), || false).unwrap()
}

fn created(changes: &[Observed], name: &str) -> Option<(FileId, Option<Content>)> {
    changes.iter().find_map(|change| match change {
        Observed::Created {
            path, id, content, ..
        } if path == Path::new(name) => Some((*id, content.clone())),
        _ => None,
    })
}

#[test]
fn a_root_replaced_between_a_scan_and_its_consumption_is_refused() {
    let folder = Folder::new();
    fs::write(folder.root.join("notes.txt"), b"notes").unwrap();
    folder.seed(&[], &[], 0);
    let mut running = folder.start(no_hook()).unwrap();
    let sections = running.disk.sections();
    let mut watching = observer(&folder.root, Snapshot::default());
    let batch = observe_until(&mut watching, sections.stamp(), |seen| {
        created(seen, "notes.txt").is_some()
    });
    let aside = replace_root(&folder.root);
    set_marker(&folder.root, 0);
    fs::write(folder.root.join("other.txt"), b"other").unwrap();
    assert!(matches!(
        running
            .disk
            .observed(batch, &mut running.engine, &mut running.store),
        Err(Stop::Withdrawn(_))
    ));
    assert!(running.engine.trees.local().is_empty());

    let mut replacement = observer(&folder.root, Snapshot::default());
    let batch = observe_until(&mut replacement, sections.stamp(), |seen| {
        created(seen, "other.txt").is_some()
    });
    assert!(matches!(
        running
            .disk
            .observed(batch, &mut running.engine, &mut running.store),
        Err(Stop::OtherRoot)
    ));
    assert!(running.engine.trees.local().is_empty());
    assert_eq!(fs::read(aside.join("notes.txt")).unwrap(), b"notes");
}

#[test]
fn a_stale_batch_does_not_undo_an_effect_and_the_rescan_reports_its_result() {
    let folder = Folder::new();
    let mut server = Server::default();
    (SCENARIOS[2].setup)(&folder, &mut server);
    fs::write(folder.root.join("notes.txt"), b"notes").unwrap();
    let mut running = folder.start(no_hook()).unwrap();
    let sections = running.disk.sections();
    let mut watching = observer(&folder.root, Snapshot::default());
    let stale = observe_until(&mut watching, sections.stamp(), |seen| {
        created(seen, "report.txt").is_some() && created(seen, "notes.txt").is_some()
    });
    let (old, _) = created(&stale.observation.changes, "report.txt").unwrap();

    assert_eq!(running.step(&server), Some(Settled::Done));
    let written = running.local(NODE).unwrap().clone();
    assert_ne!(written.identity, file_identity(old));
    running
        .disk
        .observed(stale, &mut running.engine, &mut running.store)
        .unwrap();
    assert_eq!(running.local(NODE), Some(&written));
    let notes: Vec<&LocalNode> = running
        .engine
        .trees
        .local()
        .iter()
        .filter(|(_, node)| node.shape.name == "notes.txt")
        .map(|(_, node)| node)
        .collect();
    assert_eq!(
        notes.len(),
        1,
        "a change outside the effect's paths is kept"
    );

    let known = Known {
        kind: EntryKind::File,
        id: file_id(written.identity),
    };
    let reset = Reset {
        scope: "report.txt".into(),
        known: BTreeMap::from([(PathBuf::from("report.txt"), known)]),
    };
    let resets = catch_up(&mut watching, &sections);
    assert_eq!(
        resets,
        vec![reset.clone(), reset],
        "the effect and the dropped creation each reset report.txt to L"
    );
    let fresh = observe_until(&mut watching, sections.stamp(), |seen| !seen.is_empty());
    assert_eq!(
        fresh.observation.changes,
        vec![Observed::Modified {
            path: "report.txt".into(),
            id: known.id,
            linked: false,
            content: written.shape.content.clone(),
        }],
        "the rescan reads the written file again and reports nothing else"
    );
    running
        .disk
        .observed(fresh, &mut running.engine, &mut running.store)
        .unwrap();
    assert_eq!(settle(&mut watching), vec![]);
    assert_eq!(watching.snapshot(), &Snapshot::scan(&folder.root).unwrap());
    assert_eq!(running.local(NODE), Some(&written));
    drop(running);
    let (_, engine) = folder.open();
    assert_eq!(engine.trees.local().get(NODE), Some(&written));
}

#[test]
fn a_download_that_fails_for_now_keeps_the_effect_and_stages_again() {
    let folder = Folder::new();
    let mut server = Server::default();
    (SCENARIOS[1].setup)(&folder, &mut server);
    *server.unavailable.lock().unwrap() = 1;
    let mut running = folder.start(no_hook()).unwrap();
    let pending = running
        .disk
        .next(&mut running.engine, &mut running.store)
        .unwrap()
        .unwrap();
    match ready(pending.stage(&server)) {
        Err(Stall::Later { wait, .. }) => assert_eq!(wait, STAGE_AGAIN),
        other => panic!("{:?}", other.map(|_| ())),
    }
    assert!(!running.engine.effects.in_flight().is_empty());
    assert!(!folder.root.join("report.txt").exists());
    let staged = ready(pending.stage(&server)).unwrap();
    assert_eq!(
        one(running
            .disk
            .apply(staged, &mut running.engine, &mut running.store)
            .unwrap()),
        Settled::Done
    );
    assert_eq!(fs::read(folder.root.join("report.txt")).unwrap(), REMOTE);
}

/// Writes `data` as a new file named `name` and returns the folder after the effect.
fn write_new(name: &str, data: &[u8], config: ChunkerConfig) -> (Folder, Settled) {
    let folder = Folder::new();
    let mut server = Server::default();
    let content = server.hold(data, config);
    folder.seed(&[remote(NODE, name, Some(content))], &[], 0);
    let mut running = folder.start(no_hook()).unwrap();
    let settled = running.step(&server).unwrap();
    assert_eq!(folder.marker(), running.engine.effects.cleared());
    assert_eq!(names(&folder, STAGING_DIR), Vec::<String>::new());
    (folder, settled)
}

#[test]
fn a_chunk_that_repeats_is_written_at_every_offset() {
    let config = ChunkerConfig::new(64, 256, 1024, 512).unwrap();
    let data = vec![0u8; 5_000];
    let mut server = Server::default();
    let content = server.hold(&data, config);
    let distinct: BTreeSet<&Vec<u8>> = content.chunks.iter().map(|chunk| &chunk.hash).collect();
    assert!(distinct.len() < content.chunks.len(), "{content:?}");
    let (folder, settled) = write_new("zeros", &data, config);
    assert_eq!(settled, Settled::Done);
    assert_eq!(fs::read(folder.root.join("zeros")).unwrap(), data);
}

#[test]
fn an_empty_file_is_written() {
    let (folder, settled) = write_new("empty", b"", ChunkerConfig::V0);
    assert_eq!(settled, Settled::Done);
    assert_eq!(fs::read(folder.root.join("empty")).unwrap(), b"");
}

#[test]
fn a_name_keeps_its_exact_bytes() {
    let name = "re\u{301}sume\u{301} 한글.txt";
    let (folder, settled) = write_new(name, REMOTE, ChunkerConfig::V0);
    assert_eq!(settled, Settled::Done);
    let names: Vec<Vec<u8>> = fs::read_dir(&folder.root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().as_bytes().to_vec())
        .filter(|found| found.as_slice() != PRIVATE_DIR.as_bytes())
        .collect();
    assert_eq!(names, vec![name.as_bytes().to_vec()]);
}

#[test]
fn a_corrupted_download_fails_the_effect_and_writes_nothing() {
    let folder = Folder::new();
    let mut server = Server::default();
    (SCENARIOS[1].setup)(&folder, &mut server);
    server.corrupt = true;
    let mut running = folder.start(no_hook()).unwrap();
    let settled = running.step(&server).unwrap();
    assert!(
        matches!(&settled, Settled::Failed(why) if why.contains("cannot be downloaded")),
        "{settled:?}"
    );
    assert!(!folder.root.join("report.txt").exists());
    assert_eq!(names(&folder, STAGING_DIR), Vec::<String>::new());
    assert_eq!(folder.marker(), 1);
    assert!(running.engine.effects.blocked().contains_key(NODE));
    assert_eq!(running.step(&server), None);
}

#[test]
fn a_content_without_its_chunk_list_is_fetched_through_the_remote_version() {
    let folder = Folder::new();
    let mut server = Server::default();
    let mut content = server.hold(REMOTE, ChunkerConfig::V0);
    let chunks = std::mem::take(&mut content.chunks);
    server.versions.insert(format!("version-{NODE}"), chunks);
    folder.seed(&[remote(NODE, "report.txt", Some(content))], &[], 0);
    let mut running = folder.start(no_hook()).unwrap();
    assert_eq!(running.step(&server), Some(Settled::Done));
    assert_eq!(fs::read(folder.root.join("report.txt")).unwrap(), REMOTE);

    let folder = Folder::new();
    let mut server = Server::default();
    let mut content = server.hold(REMOTE, ChunkerConfig::V0);
    content.chunks.clear();
    folder.seed(&[remote(NODE, "report.txt", Some(content))], &[], 0);
    let mut running = folder.start(no_hook()).unwrap();
    let settled = running.step(&server).unwrap();
    assert!(matches!(settled, Settled::Failed(_)), "{settled:?}");
    assert!(!folder.root.join("report.txt").exists());
    assert_eq!(folder.marker(), 1);
}

fn record(effect: EngineEffect) -> EffectRecord {
    EffectRecord {
        effect_id: "0190f0a0-0000-7000-8000-0000000000e1".into(),
        node: NODE.into(),
        counter: 1,
        effect,
        after: None,
    }
}

#[test]
fn translation_copies_names_and_identities_and_refuses_what_is_not_a_name() {
    let no_birth = FileIdentity {
        dev: 7,
        ino: 8,
        birth_ns: None,
    };
    let born = FileIdentity {
        birth_ns: Some(-5),
        ..no_birth
    };
    let at = EngineLocation::At {
        base: EngineBase::Parked {
            node: "parked-node".into(),
            identity: born,
        },
        dirs: vec![EngineDir {
            name: "dossier".into(),
            identity: no_birth,
        }],
        name: "e\u{301}t\u{e9}.txt".into(),
    };
    let synced = Content {
        size: 3,
        mtime_ns: -1,
        executable: true,
        ..Content::default()
    };
    let (effect, contents) = translate(&record(EngineEffect::RemoveFile {
        displaced: EngineDisplaced {
            at: at.clone(),
            identity: no_birth,
            content: synced.clone(),
        },
    }))
    .unwrap();
    let Action::RemoveFile { displaced } = effect.action else {
        panic!("{effect:?}");
    };
    assert_eq!(
        displaced.at,
        Location::At {
            base: Base::Parked {
                name: "parked-node".into(),
                id: FileId {
                    dev: 7,
                    ino: 8,
                    birth_ns: Some(-5)
                },
            },
            dirs: vec![Dir {
                name: "dossier".into(),
                id: FileId {
                    dev: 7,
                    ino: 8,
                    birth_ns: None
                },
            }],
            name: OsString::from("e\u{301}t\u{e9}.txt"),
        }
    );
    assert_eq!(
        displaced.entry,
        Entry {
            kind: EntryKind::File,
            id: file_id(no_birth),
            size: 3,
            mtime_ns: -1,
            executable: true,
            linked: false,
        }
    );
    assert_eq!(contents.displaced, Some(synced));
    assert_eq!(file_identity(file_id(no_birth)), no_birth);
    assert_eq!(file_identity(file_id(born)), born);

    for bad in ["", ".", "..", "a/b", "/", "a\0b"] {
        let effect = EngineEffect::CreateDir {
            at: EngineLocation::At {
                base: EngineBase::Root,
                dirs: Vec::new(),
                name: bad.into(),
            },
        };
        assert!(
            matches!(translate(&record(effect)), Err(Stop::Record { .. })),
            "{bad:?}"
        );
    }
    let parked = EngineEffect::Move {
        from: EngineLocation::Parked("..".into()),
        identity: no_birth,
        to: at,
    };
    assert!(translate(&record(parked)).is_err());
}

#[test]
fn a_record_whose_name_is_not_a_file_name_is_never_committed() {
    let folder = Folder::new();
    folder.seed(&[remote(NODE, "a/b", None)], &[], 0);
    let mut running = folder.start(no_hook()).unwrap();
    assert!(matches!(
        running.disk.next(&mut running.engine, &mut running.store),
        Err(Stop::Record { .. })
    ));
    drop(running);
    let (_, engine) = folder.open();
    assert!(engine.effects.in_flight().is_empty());
    assert_eq!(folder.marker(), 0);
}

#[test]
fn kinds_map_one_to_one() {
    let identity = FileIdentity {
        dev: 1,
        ino: 2,
        birth_ns: Some(3),
    };
    let here = EngineLocation::At {
        base: EngineBase::Root,
        dirs: Vec::new(),
        name: "here".into(),
    };
    let content = Content {
        size: 1,
        ..Content::default()
    };
    let cases = [
        EngineEffect::CreateDir { at: here.clone() },
        EngineEffect::WriteFile {
            at: here.clone(),
            content: content.clone(),
            replaces: Some(EngineDisplaced {
                at: EngineLocation::Parked(NODE.into()),
                identity,
                content: content.clone(),
            }),
        },
        EngineEffect::Move {
            from: here.clone(),
            identity,
            to: EngineLocation::Parked(NODE.into()),
        },
        EngineEffect::RemoveDir {
            at: here.clone(),
            identity,
        },
    ];
    let kinds: Vec<&str> = cases
        .into_iter()
        .map(
            |effect| match translate(&record(effect)).unwrap().0.action {
                Action::CreateDir { .. } => "create",
                Action::WriteFile {
                    replaces:
                        Some(Displaced {
                            at: Location::Parked(_),
                            ..
                        }),
                    ..
                } => "replace",
                Action::Move {
                    to: Location::Parked(_),
                    id,
                    ..
                } if id == file_id(identity) => "park",
                Action::RemoveDir { id, .. } if id == file_id(identity) => "remove",
                other => panic!("{other:?}"),
            },
        )
        .collect();
    assert_eq!(kinds, ["create", "replace", "park", "remove"]);
}

/// Passes the resets the disk requested to the observer, and returns them.
fn catch_up(watching: &mut FsObserver, sections: &Sections) -> Vec<Reset> {
    let (resets, _) = sections.take_resets();
    watching.reset(resets.clone());
    resets
}

/// Every change the observer reports within about a second.
fn settle(watching: &mut FsObserver) -> Vec<Observed> {
    let mut seen = Vec::new();
    for _ in 0..3 {
        if let Some(observation) = watching.observe(Duration::from_millis(300)).unwrap() {
            seen.extend(observation.changes);
        }
    }
    seen
}

fn removed(changes: &[Observed], name: &str) -> bool {
    changes
        .iter()
        .any(|change| matches!(change, Observed::Removed { path, .. } if path == Path::new(name)))
}

fn renamed(changes: &[Observed], from: &str, to: &str) -> bool {
    changes.iter().any(|change| {
        matches!(change, Observed::Renamed { from: f, to: t, .. } if f == Path::new(from) && t == Path::new(to))
    })
}

#[test]
fn a_file_deleted_once_its_replacement_is_placed_leaves_l_after_the_rescan() {
    let folder = Folder::new();
    let mut server = Server::default();
    (SCENARIOS[2].setup)(&folder, &mut server);
    let report = folder.root.join("report.txt");
    let delete: Hook = Box::new(move |cut| {
        if cut == Cut::Step(Step::Placed) {
            fs::remove_file(&report).unwrap();
        }
    });
    let mut running = folder.start(Some(delete)).unwrap();
    let sections = running.disk.sections();
    let mut watching = observer(&folder.root, Snapshot::scan(&folder.root).unwrap());

    assert_eq!(running.step(&server), Some(Settled::Done));
    assert!(running.local(NODE).is_some());
    catch_up(&mut watching, &sections);
    let fresh = observe_until(&mut watching, sections.stamp(), |seen| {
        removed(seen, "report.txt")
    });
    running
        .disk
        .observed(fresh, &mut running.engine, &mut running.store)
        .unwrap();
    assert_eq!(running.local(NODE), None);
}

#[test]
fn a_rename_dropped_with_a_stale_batch_reaches_l_after_the_rescan() {
    let folder = Folder::new();
    let mut server = Server::default();
    (SCENARIOS[2].setup)(&folder, &mut server);
    let mut running = folder.start(no_hook()).unwrap();
    let sections = running.disk.sections();
    let mut watching = observer(&folder.root, Snapshot::scan(&folder.root).unwrap());
    let pending = running
        .disk
        .next(&mut running.engine, &mut running.store)
        .unwrap()
        .unwrap();
    let staged = ready(pending.stage(&server)).unwrap();

    fs::rename(folder.root.join("report.txt"), folder.root.join("mine.txt")).unwrap();
    let held = observe_until(&mut watching, sections.stamp(), |seen| {
        renamed(seen, "report.txt", "mine.txt")
    });
    assert_eq!(
        one(running
            .disk
            .apply(staged, &mut running.engine, &mut running.store)
            .unwrap()),
        Settled::Failed("the entry is not there".into())
    );
    let before = running.local(NODE).unwrap().clone();
    assert_eq!(before.shape.name, "report.txt");
    running
        .disk
        .observed(held, &mut running.engine, &mut running.store)
        .unwrap();
    assert_eq!(
        running.local(NODE),
        Some(&before),
        "the stale rename is dropped"
    );

    catch_up(&mut watching, &sections);
    let fresh = observe_until(&mut watching, sections.stamp(), |seen| {
        renamed(seen, "report.txt", "mine.txt")
    });
    running
        .disk
        .observed(fresh, &mut running.engine, &mut running.store)
        .unwrap();
    let moved = running.local(NODE).unwrap();
    assert_eq!(moved.shape.name, "mine.txt");
    assert_eq!(moved.identity, before.identity);
}

#[test]
fn a_removal_dropped_with_a_stale_batch_reaches_l_after_the_rescan() {
    let folder = Folder::new();
    let mut server = Server::default();
    (SCENARIOS[2].setup)(&folder, &mut server);
    let mut running = folder.start(no_hook()).unwrap();
    let sections = running.disk.sections();
    let mut watching = observer(&folder.root, Snapshot::scan(&folder.root).unwrap());
    let stale = sections.stamp();

    assert_eq!(running.step(&server), Some(Settled::Done));
    fs::remove_file(folder.root.join("report.txt")).unwrap();
    let held = observe_until(&mut watching, stale, |seen| removed(seen, "report.txt"));
    running
        .disk
        .observed(held, &mut running.engine, &mut running.store)
        .unwrap();
    assert!(
        running.local(NODE).is_some(),
        "the stale removal is dropped"
    );

    catch_up(&mut watching, &sections);
    let fresh = observe_until(&mut watching, sections.stamp(), |seen| {
        removed(seen, "report.txt")
    });
    running
        .disk
        .observed(fresh, &mut running.engine, &mut running.store)
        .unwrap();
    assert_eq!(running.local(NODE), None);
    drop(running);
    let (_, engine) = folder.open();
    assert_eq!(engine.trees.local().get(NODE), None);
}

#[test]
fn a_reset_built_from_an_older_l_repeats_a_change_harmlessly() {
    let folder = Folder::new();
    let mut server = Server::default();
    (SCENARIOS[2].setup)(&folder, &mut server);
    let mut running = folder.start(no_hook()).unwrap();
    let sections = running.disk.sections();
    let mut watching = observer(&folder.root, Snapshot::scan(&folder.root).unwrap());
    assert_eq!(running.step(&server), Some(Settled::Done));
    let older = catch_up(&mut watching, &sections);

    fs::remove_file(folder.root.join("report.txt")).unwrap();
    let removal = observe_until(&mut watching, sections.stamp(), |seen| {
        removed(seen, "report.txt")
    });
    running
        .disk
        .observed(removal, &mut running.engine, &mut running.store)
        .unwrap();
    assert_eq!(running.local(NODE), None);
    let local: Vec<(String, LocalNode)> = running
        .engine
        .trees
        .local()
        .iter()
        .map(|(id, node)| (id.clone(), node.clone()))
        .collect();

    watching.reset(older);
    let repeated = observe_until(&mut watching, sections.stamp(), |seen| {
        removed(seen, "report.txt")
    });
    running
        .disk
        .observed(repeated, &mut running.engine, &mut running.store)
        .unwrap();
    let after: Vec<(String, LocalNode)> = running
        .engine
        .trees
        .local()
        .iter()
        .map(|(id, node)| (id.clone(), node.clone()))
        .collect();
    assert_eq!(after, local);
}

#[test]
fn an_edit_that_keeps_size_and_mtime_while_a_file_is_placed_reaches_l_after_the_rescan() {
    let folder = Folder::new();
    let mut server = Server::default();
    (SCENARIOS[2].setup)(&folder, &mut server);
    let path = folder.root.join("report.txt");
    let edit: Hook = Box::new({
        let path = path.clone();
        move |cut| {
            if cut == Cut::Step(Step::Placed) {
                let placed = fs::metadata(&path).unwrap();
                let mut edited = fs::read(&path).unwrap();
                edited[0] ^= 1;
                fs::write(&path, edited).unwrap();
                let times = fs::FileTimes::new().set_modified(placed.modified().unwrap());
                let file = fs::File::options().write(true).open(&path).unwrap();
                file.set_times(times).unwrap();
            }
        }
    });
    let mut running = folder.start(Some(edit)).unwrap();
    let sections = running.disk.sections();
    let mut watching = observer(&folder.root, Snapshot::scan(&folder.root).unwrap());
    assert_eq!(running.step(&server), Some(Settled::Done));
    let acknowledged = running.local(NODE).unwrap().shape.content.clone().unwrap();
    let edited = disk_content(&path);
    assert_eq!(edited.size, acknowledged.size);
    assert_eq!(edited.mtime_ns, acknowledged.mtime_ns);
    assert_ne!(edited.manifest_digest, acknowledged.manifest_digest);

    catch_up(&mut watching, &sections);
    let fresh = observe_until(&mut watching, sections.stamp(), |seen| !seen.is_empty());
    running
        .disk
        .observed(fresh, &mut running.engine, &mut running.store)
        .unwrap();
    let held = running.local(NODE).unwrap().shape.content.clone().unwrap();
    assert_eq!(
        held.manifest_digest, edited.manifest_digest,
        "L keeps the downloaded content for a file the user edited"
    );
}

#[test]
fn l_resets_to_the_kinds_and_identities_the_scan_saw() {
    use std::os::unix::fs::PermissionsExt;

    let folder = Folder::new();
    folder.seed(&[], &[], 0);
    let root = &folder.root;
    fs::create_dir_all(root.join("docs/deep")).unwrap();
    fs::write(root.join("docs/a.txt"), b"a").unwrap();
    fs::write(root.join("docs/deep/b.txt"), b"bb").unwrap();
    fs::write(root.join("run.sh"), b"#!/bin/sh\n").unwrap();
    fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(root.join("empty"), b"").unwrap();
    let mut running = folder.start(no_hook()).unwrap();
    let sections = running.disk.sections();
    let mut watching = observer(root, Snapshot::default());
    let batch = observe_until(&mut watching, sections.stamp(), |seen| seen.len() >= 6);
    running
        .disk
        .observed(batch, &mut running.engine, &mut running.store)
        .unwrap();

    let scanned = watching.snapshot().entries.clone();
    let known = |under: &str| -> BTreeMap<PathBuf, Known> {
        scanned
            .iter()
            .filter(|(path, _)| path.starts_with(under))
            .map(|(path, &Entry { kind, id, .. })| (path.clone(), Known { kind, id }))
            .collect()
    };
    let resets = running
        .engine
        .resets([PathBuf::new(), PathBuf::from("docs")]);
    assert_eq!(resets[0].known, known(""));
    assert_eq!(resets[1].known, known("docs"));

    let held = |running: &Running| -> Vec<(String, LocalNode)> {
        let local = running.engine.trees.local();
        local
            .iter()
            .map(|(id, node)| (id.clone(), node.clone()))
            .collect()
    };
    let local = held(&running);
    watching.reset(resets);
    let fresh = observe_until(&mut watching, sections.stamp(), |seen| seen.len() >= 4);
    let read: Vec<&Path> = fresh
        .observation
        .changes
        .iter()
        .map(|change| match change {
            Observed::Modified {
                path,
                content: Some(_),
                ..
            } => path.as_path(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        read,
        ["docs/a.txt", "docs/deep/b.txt", "empty", "run.sh"].map(Path::new),
        "every file L holds is read again, and no folder is reported"
    );
    running
        .disk
        .observed(fresh, &mut running.engine, &mut running.store)
        .unwrap();
    assert_eq!(held(&running), local);
    assert_eq!(settle(&mut watching), vec![]);
    assert_eq!(watching.snapshot().entries, scanned);
}

#[test]
fn nothing_is_planned_before_the_first_batch_and_its_rules_keep_a_match_from_being_written() {
    const LOG: &str = "0190f0a0-0000-7000-8000-0000000000a2";
    let folder = Folder::new();
    let mut server = Server::default();
    let content = server.hold(REMOTE, ChunkerConfig::V0);
    fs::write(folder.root.join(".koiignore"), "*.log\n").unwrap();
    folder.seed(
        &[
            remote(LOG, "build.log", Some(content.clone())),
            remote(NODE, "report.txt", Some(content)),
        ],
        &[],
        0,
    );
    let mut running = folder.start_unobserved(no_hook()).unwrap();
    let planned = running.disk.next(&mut running.engine, &mut running.store);
    assert!(planned.unwrap().is_none(), "the rules are not known yet");

    let sections = running.disk.sections();
    let mut watching = observer(&folder.root, Snapshot::default());
    let first = observe_until(&mut watching, sections.stamp(), |_| true);
    assert_eq!(first.observation.rules.len(), 1);
    running
        .disk
        .observed(first, &mut running.engine, &mut running.store)
        .unwrap();
    assert_eq!(running.step(&server), Some(Settled::Done));
    assert_eq!(running.step(&server), None);
    assert_eq!(fs::read(folder.root.join("report.txt")).unwrap(), REMOTE);
    assert!(!folder.root.join("build.log").exists());
}

#[test]
fn a_file_moved_into_an_ignored_folder_keeps_its_entry_in_l() {
    let folder = Folder::new();
    let report = folder.root.join("report.txt");
    fs::write(&report, SYNCED).unwrap();
    fs::write(folder.root.join(".koiignore"), "/build/\n").unwrap();
    fs::create_dir(folder.root.join("build")).unwrap();
    folder.seed(
        &[remote(NODE, "report.txt", Some(disk_content(&report)))],
        &[(NODE, SPACE, "report.txt")],
        0,
    );
    let mut running = folder.start_unobserved(no_hook()).unwrap();
    let sections = running.disk.sections();
    let mut watching = observer(&folder.root, Snapshot::default());
    let first = observe_until(&mut watching, sections.stamp(), |_| true);
    running
        .disk
        .observed(first, &mut running.engine, &mut running.store)
        .unwrap();
    let synced = running.local(NODE).cloned().expect("the synced file");

    fs::rename(&report, folder.root.join("build/report.txt")).unwrap();
    let moved = observe_until(&mut watching, sections.stamp(), |seen| {
        renamed(seen, "report.txt", "build/report.txt")
    });
    assert!(!removed(&moved.observation.changes, "report.txt"));
    running
        .disk
        .observed(moved, &mut running.engine, &mut running.store)
        .unwrap();
    assert_eq!(running.local(NODE), Some(&synced), "L has no place for it");
    assert!(running.engine.trees.ignored_local().contains(NODE));
    let planned = running.disk.next(&mut running.engine, &mut running.store);
    assert!(planned.unwrap().is_none());
}

/// A file the disk trashed whose metadata alone changed afterwards, as `chmod -R` on the folder does, still holds
/// the content its effect recorded, so reclamation deletes it when it is due instead of bringing it back.
#[test]
fn a_trashed_file_whose_metadata_alone_changed_is_reclaimed_when_due() {
    use std::os::unix::fs::PermissionsExt;
    let folder = Folder::new();
    let mut server = Server::default();
    (SCENARIOS[2].setup)(&folder, &mut server);
    let mut running = folder.start(no_hook()).unwrap();
    assert_eq!(running.step(&server), Some(Settled::Done));
    let entries: Vec<PathBuf> = fs::read_dir(folder.private(TRASH_DIR))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(entries.len(), 1);
    std::thread::sleep(Duration::from_millis(50));
    let mode = fs::metadata(&entries[0]).unwrap().permissions().mode();
    fs::set_permissions(&entries[0], fs::Permissions::from_mode(mode ^ 0o004)).unwrap();

    let due = SystemTime::now() + koidrive_fs::TRASH_RETENTION + Duration::from_secs(60);
    let reclaimed = running.disk.reclaim_trash(due, u64::MAX).unwrap();
    assert_eq!(reclaimed.removed.len(), 1, "{reclaimed:?}");
    assert!(reclaimed.recovered.is_empty(), "{reclaimed:?}");
}

/// Another device's deletion removes a file whose edit was observed and then undone before the
/// removal, with 10 files of the deletion counted before it. The space is the 10 files S holds
/// after the removal and the 11 the deletion removed, so the notice opens.
#[test]
fn a_removal_counts_against_the_files_s_holds_after_it() {
    use koidrive_engine::guard::{Counted, RemoteDeletion, Trash};
    let folder = Folder::new();
    let path = folder.root.join("report.txt");
    fs::write(&path, SYNCED).unwrap();
    let mut rows = vec![(NODE.to_owned(), SPACE.to_owned(), "report.txt".to_owned())];
    let mut r_nodes = Vec::new();
    for i in 0..10 {
        let id = format!("loose{i}");
        let name = format!("stay{i}.txt");
        fs::write(folder.root.join(&name), SYNCED).unwrap();
        r_nodes.push(remote(
            &id,
            &name,
            Some(disk_content(&folder.root.join(&name))),
        ));
        rows.push((id, SPACE.to_owned(), name));
    }
    let held: Vec<_> = rows
        .iter()
        .map(|(id, parent, path)| (id.as_str(), parent.as_str(), path.as_str()))
        .collect();
    folder.seed(&r_nodes, &held, 0);
    let (mut store, _) = folder.open();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    store
        .transaction(|tx| {
            let mut state = tx.engine_state()?;
            state.commits.guard.remote.insert(
                RemoteDeletion {
                    top: "earlier".into(),
                    trash: Trash::Group {
                        group: "0:0".into(),
                        seq: 0,
                    },
                },
                Counted {
                    at_ms: now,
                    files: 10,
                },
            );
            tx.set_engine_state(&state)
        })
        .unwrap();
    drop(store);
    let mut running = folder.start(no_hook()).unwrap();
    let pending = running
        .disk
        .next(&mut running.engine, &mut running.store)
        .unwrap()
        .unwrap();
    let original_mtime = fs::metadata(&path).unwrap().modified().unwrap();
    fs::write(&path, b"temporary user edit").unwrap();
    let edited = disk_content(&path);
    batch(&mut running, |trees| {
        let mut local = trees.local().get(NODE).unwrap().clone();
        local.shape.content = Some(edited);
        trees.set_local(NODE.into(), local);
    });
    fs::write(&path, SYNCED).unwrap();
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(original_mtime))
        .unwrap();
    let staged = ready(pending.stage(&Server::default())).unwrap();
    assert!(
        pending
            .record()
            .applies(&running.engine.trees, running.engine.commits.in_flight())
    );
    assert_eq!(
        one(running
            .disk
            .apply(staged, &mut running.engine, &mut running.store)
            .unwrap()),
        Settled::Done
    );
    assert!(!path.exists());
    assert_eq!(running.engine.trees.synced_files(), 10);
    assert_eq!(
        running
            .engine
            .commits
            .state()
            .guard
            .remote
            .values()
            .map(|c| c.files)
            .sum::<u64>(),
        11
    );
    assert_eq!(
        running.engine.commits.questions().len(),
        1,
        "11 of 21 files cross the share"
    );
}

#[test]
fn a_parked_fork_whose_name_is_taken_is_uploaded_from_the_parked_directory() {
    let fork = "0190f0a0-0000-7000-8000-0000000000f1";
    let folder = Folder::new();
    fs::write(folder.root.join("c.txt"), b"the server's c.txt").unwrap();
    let held = disk_content(&folder.root.join("c.txt"));
    folder.seed(
        &[remote(NODE, "c.txt", Some(held))],
        &[(NODE, SPACE, "c.txt")],
        0,
    );
    let mut running = folder.start(no_hook()).unwrap();
    let parked = folder.private("parked");
    fs::create_dir_all(&parked).unwrap();
    let entry = parked.join(running.engine.trees.parked_name(fork));
    fs::write(&entry, b"the person's own c.txt").unwrap();
    let content = disk_content(&entry);
    let meta = fs::metadata(&entry).unwrap();
    batch(&mut running, |trees| {
        trees.set_local(
            fork.into(),
            LocalNode {
                shape: Shape {
                    parent: SPACE.into(),
                    name: "c.txt".into(),
                    kind: Kind::File,
                    content: Some(content),
                },
                identity: FileIdentity {
                    dev: meta.dev(),
                    ino: meta.ino().into(),
                    birth_ns: None,
                },
                parked: true,
            },
        )
    });
    let engine = &mut running.engine;
    let commit = (engine.commits)
        .build(&mut engine.trees, &mut engine.ids, 1, "history", "me", 2)
        .cloned()
        .unwrap();
    assert!(
        commit
            .ops
            .iter()
            .any(|op| matches!(&op.op, Some(op::Op::CreateFile(create)) if create.node_id == fork))
    );
    let upload = running.disk.upload(&commit, &running.engine);
    let read: Vec<u8> = (upload.chunks.iter())
        .flat_map(|chunk| upload.read(chunk).unwrap().to_vec())
        .collect();
    assert_eq!(read, b"the person's own c.txt");
}

const BATCHED: [&str; 5] = [
    "0190f0a0-0000-7000-8000-0000000000c1",
    "0190f0a0-0000-7000-8000-0000000000c2",
    "0190f0a0-0000-7000-8000-0000000000c3",
    "0190f0a0-0000-7000-8000-0000000000c4",
    "0190f0a0-0000-7000-8000-0000000000c5",
];
const MOVED: &[u8] = b"the moved file";
const REMOVED: &[u8] = b"the removed file";
const NEWER_NOTES: &[u8] = b"the notes as the other device wrote them";

fn five_effects(folder: &Folder, server: &mut Server) {
    fs::write(folder.root.join("notes.txt"), SYNCED).unwrap();
    fs::write(folder.root.join("a.txt"), MOVED).unwrap();
    fs::write(folder.root.join("old.txt"), REMOVED).unwrap();
    let moved = disk_content(&folder.root.join("a.txt"));
    let written = server.hold(REMOTE, ChunkerConfig::V0);
    let replacing = server.hold(NEWER_NOTES, ChunkerConfig::V0);
    folder.seed(
        &[
            remote(BATCHED[0], "docs", None),
            remote(BATCHED[1], "report.txt", Some(written)),
            remote(BATCHED[2], "notes.txt", Some(replacing)),
            remote(BATCHED[3], "b.txt", Some(moved)),
        ],
        &[
            (BATCHED[2], SPACE, "notes.txt"),
            (BATCHED[3], SPACE, "a.txt"),
            (BATCHED[4], SPACE, "old.txt"),
        ],
        0,
    );
}

fn five_effects_done() -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    BTreeMap::from([
        (PathBuf::from("docs"), None),
        (PathBuf::from("report.txt"), Some(REMOTE.to_vec())),
        (PathBuf::from("notes.txt"), Some(NEWER_NOTES.to_vec())),
        (PathBuf::from("b.txt"), Some(MOVED.to_vec())),
    ])
}

fn assert_all_done(folder: &Folder, running: &Running, context: &str) {
    assert_eq!(running.engine.effects.in_flight(), [], "{context}");
    assert!(running.engine.effects.blocked().is_empty(), "{context}");
    let (_, stored) = folder.open();
    for node in BATCHED {
        let remote = running.engine.trees.remote().live(node);
        let local = running.local(node).map(|node| &node.shape);
        assert!(
            koidrive_engine::tree::same(local, remote),
            "{context}: {node}: L {local:?}, R {remote:?}"
        );
        assert_eq!(
            running.engine.trees.synced().get(node),
            local,
            "{context}: {node}: S"
        );
        assert_eq!(
            stored.trees.synced().get(node),
            local,
            "{context}: {node}: S in the store"
        );
    }
    assert_eq!(visible(folder), five_effects_done(), "{context}");
    assert_eq!(
        folder.marker(),
        running.engine.effects.cleared(),
        "{context}"
    );
    assert_eq!(
        names(folder, QUARANTINE_DIR),
        Vec::<String>::new(),
        "{context}"
    );
    assert_eq!(
        names(folder, STAGING_DIR),
        Vec::<String>::new(),
        "{context}"
    );
    for bytes in [SYNCED, REMOVED] {
        assert!(
            trashed(folder, bytes),
            "{context}: displaced bytes are not in the trash"
        );
    }
}

#[test]
fn independent_effects_are_applied_in_one_section_and_acknowledged_in_one_transaction() {
    let folder = Folder::new();
    let mut server = Server::default();
    five_effects(&folder, &mut server);
    let reached = Arc::new(Mutex::new(Vec::new()));
    let record = reached.clone();
    let mut running = folder
        .start(Some(Box::new(move |cut| record.lock().unwrap().push(cut))))
        .unwrap();

    let settled = running.step_batch(&server).expect("a batch");

    let mut nodes: Vec<&str> = settled.iter().map(|(node, _)| node.as_str()).collect();
    nodes.sort();
    assert_eq!(nodes, BATCHED);
    assert!(
        settled.iter().all(|(_, settled)| *settled == Settled::Done),
        "{settled:?}"
    );
    let reached = reached.lock().unwrap().clone();
    let once = |cut: Cut| reached.iter().filter(|reached| **reached == cut).count();
    for cut in [
        Cut::BeforePlan,
        Cut::AfterPlan,
        Cut::Step(Step::Marked),
        Cut::BeforeAck,
        Cut::AfterAck,
    ] {
        assert_eq!(once(cut), 1, "{cut:?} in {reached:?}");
    }
    assert_eq!(running.engine.effects.cleared(), 5);
    assert_eq!(running.step_batch(&server), None);
    assert_all_done(&folder, &running, "uninterrupted");
}

#[test]
fn a_crash_at_every_cut_of_a_batch_is_recovered_on_the_next_start() {
    let reached = {
        let folder = Folder::new();
        let mut server = Server::default();
        five_effects(&folder, &mut server);
        let reached = Arc::new(Mutex::new(Vec::new()));
        let record = reached.clone();
        let mut running = folder
            .start(Some(Box::new(move |cut| record.lock().unwrap().push(cut))))
            .unwrap();
        running.step_batch(&server).expect("a batch");
        reached.lock().unwrap().clone()
    };
    assert!(reached.len() > 15, "{reached:?}");
    for (at, cut) in reached.iter().enumerate() {
        let context = format!("crashed at cut {at}, {cut:?}");
        let folder = Folder::new();
        let mut server = Server::default();
        five_effects(&folder, &mut server);
        let crashed = catch_unwind(AssertUnwindSafe(|| {
            let mut count = 0;
            let hook: Hook = Box::new(move |_| {
                if count == at {
                    resume_unwind(Box::new(Crash));
                }
                count += 1;
            });
            let mut running = folder.start(Some(hook)).unwrap();
            running.step_batch(&server);
        }));
        assert!(
            crashed.is_err_and(|payload| payload.is::<Crash>()),
            "{context}: no crash"
        );

        let mut running = folder.start(no_hook()).unwrap();
        assert_eq!(running.engine.effects.in_flight(), [], "{context}");
        assert_eq!(
            folder.marker(),
            running.engine.effects.cleared(),
            "{context}"
        );
        let planned = *cut != Cut::BeforePlan;
        assert_eq!(
            running.engine.effects.cleared(),
            5 * u64::from(planned),
            "{context}"
        );
        assert_eq!(
            names(&folder, QUARANTINE_DIR),
            Vec::<String>::new(),
            "{context}"
        );
        assert_eq!(
            names(&folder, STAGING_DIR),
            Vec::<String>::new(),
            "{context}"
        );
        for (name, bytes) in [("notes.txt", SYNCED), ("old.txt", REMOVED)] {
            let at_name = fs::read(folder.root.join(name)).is_ok_and(|held| held == bytes);
            assert!(
                at_name || trashed(&folder, bytes),
                "{context}: the bytes of {name} are neither at their name nor in the trash"
            );
        }
        let mut batches = 0;
        while let Some(settled) = running.step_batch(&server) {
            assert!(
                settled.iter().all(|(_, settled)| *settled == Settled::Done),
                "{context}: {settled:?}"
            );
            batches += 1;
            assert!(
                batches <= 1,
                "{context}: planned again after the effects were done"
            );
        }
        assert_all_done(&folder, &running, &context);
    }
}

#[test]
fn a_batch_downloads_each_distinct_chunk_once_for_every_file_that_holds_it() {
    let folder = Folder::new();
    let mut server = Server::default();
    let shared = server.hold(REMOTE, ChunkerConfig::V0);
    let twice = server.hold(NEWER_NOTES, ChunkerConfig::V0);
    let alone = server.hold(MOVED, ChunkerConfig::V0);
    let files = [
        ("a.txt", &shared, REMOTE),
        ("b.txt", &twice, NEWER_NOTES),
        ("c.txt", &shared, REMOTE),
        ("d.txt", &alone, MOVED),
        ("e.txt", &twice, NEWER_NOTES),
        ("f.txt", &shared, REMOTE),
    ];
    let nodes: Vec<NodeState> = (files.iter().enumerate())
        .map(|(at, (name, content, _))| {
            remote(
                &format!("0190f0a0-0000-7000-8000-0000000000d{at}"),
                name,
                Some((*content).clone()),
            )
        })
        .collect();
    folder.seed(&nodes, &[], 0);
    let mut running = folder.start(no_hook()).unwrap();

    let settled = running.step_batch(&server).expect("a batch");

    assert_eq!(settled.len(), files.len());
    assert!(
        settled.iter().all(|(_, settled)| *settled == Settled::Done),
        "{settled:?}"
    );
    assert_eq!(*server.calls.lock().unwrap(), 1);
    let mut handed = server.handed.lock().unwrap().clone();
    handed.sort();
    let mut distinct: Vec<Vec<u8>> = ([&shared, &twice, &alone].into_iter())
        .flat_map(|content| content.chunks.iter().map(|chunk| chunk.hash.clone()))
        .collect();
    distinct.sort();
    assert_eq!(handed, distinct);
    for (name, _, bytes) in files {
        assert_eq!(fs::read(folder.root.join(name)).unwrap(), bytes, "{name}");
    }
    assert_eq!(names(&folder, STAGING_DIR), Vec::<String>::new());
    assert_eq!(running.step_batch(&server), None);
}

#[test]
fn a_chunk_the_server_lost_fails_only_the_writes_of_the_batch_that_list_it() {
    let folder = Folder::new();
    let mut server = Server::default();
    let kept = server.hold(REMOTE, ChunkerConfig::V0);
    let lost = server.hold(NEWER_NOTES, ChunkerConfig::V0);
    let other = server.hold(MOVED, ChunkerConfig::V0);
    for chunk in &lost.chunks {
        server.chunks.remove(&chunk.hash);
    }
    let lost_node = "0190f0a0-0000-7000-8000-0000000000e1";
    folder.seed(
        &[
            remote("0190f0a0-0000-7000-8000-0000000000e0", "a.txt", Some(kept)),
            remote(lost_node, "b.txt", Some(lost)),
            remote("0190f0a0-0000-7000-8000-0000000000e2", "c.txt", Some(other)),
        ],
        &[],
        0,
    );
    let mut running = folder.start(no_hook()).unwrap();

    let settled = running.step_batch(&server).expect("a batch");

    assert_eq!(settled.len(), 3);
    for (node, settled) in &settled {
        match node == lost_node {
            true => assert!(
                matches!(settled, Settled::Failed(why) if why.starts_with("its content cannot be downloaded")),
                "{settled:?}"
            ),
            false => assert_eq!(*settled, Settled::Done, "{node}"),
        }
    }
    assert_eq!(fs::read(folder.root.join("a.txt")).unwrap(), REMOTE);
    assert_eq!(fs::read(folder.root.join("c.txt")).unwrap(), MOVED);
    assert!(!folder.root.join("b.txt").exists());
    assert_eq!(names(&folder, STAGING_DIR), Vec::<String>::new());
    assert!(running.engine.effects.blocked().contains_key(lost_node));
}

#[test]
fn files_that_share_a_chunk_are_staged_within_a_small_descriptor_limit() {
    const CHILD: &str = "KOIDRIVE_TEST_FEW_DESCRIPTORS";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "disk::tests::files_that_share_a_chunk_are_staged_within_a_small_descriptor_limit",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    let limit = rustix::process::getrlimit(rustix::process::Resource::Nofile);
    rustix::process::setrlimit(
        rustix::process::Resource::Nofile,
        rustix::process::Rlimit {
            current: Some(128),
            maximum: limit.maximum,
        },
    )
    .unwrap();
    let folder = Folder::new();
    let mut server = Server::default();
    let config = ChunkerConfig::new(64, 256, 1024, 0).unwrap();
    let files: Vec<Vec<u8>> = (0..300)
        .map(|index| {
            let mut bytes = vec![b'x'; 2048];
            bytes.extend(format!("the end of file {index}").as_bytes());
            bytes
        })
        .collect();
    let nodes: Vec<NodeState> = (files.iter().enumerate())
        .map(|(index, bytes)| {
            remote(
                &format!("0190f0a0-0000-7000-8000-{:012x}", 0x10000 + index),
                &format!("file-{index}.txt"),
                Some(server.hold(bytes, config)),
            )
        })
        .collect();
    folder.seed(&nodes, &[], 0);
    let mut running = folder.start(no_hook()).unwrap();

    let settled = running.step_batch(&server).expect("a batch");

    assert_eq!(settled.len(), files.len());
    assert!(
        settled.iter().all(|(_, settled)| *settled == Settled::Done),
        "{settled:?}"
    );
    for (index, bytes) in files.iter().enumerate() {
        assert_eq!(
            fs::read(folder.root.join(format!("file-{index}.txt"))).unwrap(),
            *bytes
        );
    }
}

#[test]
fn a_chunk_the_server_reports_missing_fails_only_the_writes_of_the_batch_that_list_it() {
    let folder = Folder::new();
    let mut server = Server::default();
    let kept = server.hold(REMOTE, ChunkerConfig::V0);
    let missing = server.hold(NEWER_NOTES, ChunkerConfig::V0);
    let other = server.hold(MOVED, ChunkerConfig::V0);
    server.missing = missing
        .chunks
        .iter()
        .map(|chunk| chunk.hash.clone())
        .collect();
    let missing_node = "0190f0a0-0000-7000-8000-0000000000f1";
    folder.seed(
        &[
            remote("0190f0a0-0000-7000-8000-0000000000f0", "a.txt", Some(kept)),
            remote(missing_node, "b.txt", Some(missing)),
            remote("0190f0a0-0000-7000-8000-0000000000f2", "c.txt", Some(other)),
        ],
        &[],
        0,
    );
    let mut running = folder.start(no_hook()).unwrap();

    let settled = running.step_batch(&server).expect("a batch");

    assert_eq!(settled.len(), 3);
    for (node, settled) in &settled {
        match node == missing_node {
            true => assert!(
                matches!(settled, Settled::Failed(why) if why.starts_with("its content cannot be downloaded")),
                "{settled:?}"
            ),
            false => assert_eq!(*settled, Settled::Done, "{node}"),
        }
    }
    assert_eq!(fs::read(folder.root.join("a.txt")).unwrap(), REMOTE);
    assert_eq!(fs::read(folder.root.join("c.txt")).unwrap(), MOVED);
    assert!(!folder.root.join("b.txt").exists());
    assert_eq!(names(&folder, STAGING_DIR), Vec::<String>::new());
    assert!(running.engine.effects.blocked().contains_key(missing_node));
}
