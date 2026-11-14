// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, HashMap, VecDeque};

use std::fs;

use std::os::unix::fs::MetadataExt;

use koidrive_engine::commit::Lift;
use koidrive_engine::effects::BatchLimit;

use koidrive_engine::restore::{Outcome, RestoreIntent};

use koidrive_engine::tree::{Kind, LocalNode, Tree};

use koidrive_fs::{EntryKind, Marker, RootProblem, write_marker};

use koidrive_net::Committed;

use koidrive_proto::v1::{
    CaughtUp, ListTrash, ListTree, ListVersions, Op, PrepareUpload, Resume, ResyncRequired,
    chunk_plan, op,
};

use koidrive_sim::space::{Role, Session as Caller, SpaceDo};

use super::*;

use crate::config::SyncRules;

use crate::disk::tests::{Server, ready};

use crate::driver::{DriverError, lift_halt};

use crate::ignore::{Matcher, local_rules, matcher, scan_rules};

use crate::observe::Observed;

use crate::question::Answer;

const NOW: i64 = 1_700_000_000_000;

const WAIT: Duration = Duration::from_secs(60);

fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < WAIT, "{what} within {WAIT:?}");
    }
}

/// The server: the space, the bytes of the chunks devices uploaded, and the result of each commit by its ID.
struct Remote {
    now_ms: i64,
    space: SpaceDo,
    chunks: Server,
    results: HashMap<String, CommitResult>,
}

impl Remote {
    fn new() -> Remote {
        Self::new_at(NOW)
    }

    fn new_at(now_ms: i64) -> Remote {
        Remote {
            now_ms,
            space: SpaceDo::new(1, now_ms),
            chunks: Server::default(),
            results: HashMap::new(),
        }
    }

    /// Every page of the live tree, with `limit` nodes a page.
    fn pages(&self, limit: u32) -> Vec<TreePage> {
        let mut pages: Vec<TreePage> = Vec::new();
        loop {
            let cursor = pages
                .last()
                .map_or(String::new(), |page| page.next_cursor.clone());
            pages.push(self.space.list_tree(&ListTree { cursor, limit }));
            if pages.last().unwrap().next_cursor.is_empty() {
                return pages;
            }
        }
    }

    /// Every page of the trash, with `limit` nodes a page.
    fn trash_pages(&self, limit: u32) -> Vec<TrashPage> {
        let mut pages: Vec<TrashPage> = Vec::new();
        loop {
            let cursor = pages
                .last()
                .map_or(String::new(), |page| page.next_cursor.clone());
            pages.push(self.space.list_trash(&ListTrash { cursor, limit }));
            if pages.last().unwrap().next_cursor.is_empty() {
                return pages;
            }
        }
    }

    /// What `Transfer::upload` does: asks which chunks the space lacks, reads those from the folder and stores them.
    fn store(&mut self, caller: &Caller, upload: &Upload) -> Result<Vec<UploadReceipt>, ChunkError> {
        let mut chunks = upload.chunks.clone();
        chunks.sort_by(|a, b| a.hash.cmp(&b.hash));
        chunks.dedup_by(|a, b| a.hash == b.hash);
        let request = PrepareUpload {
            chunks: chunks.clone(),
        };
        let plan = self
            .space
            .prepare_upload(caller, self.now_ms, &request)
            .unwrap();
        let wanted = |chunk: &ChunkRef| {
            plan.chunks.iter().any(|plan| {
                plan.hash == chunk.hash && plan.action == chunk_plan::Action::Upload as i32
            })
        };
        for chunk in chunks.iter().filter(|chunk| !wanted(chunk)) {
            upload.credit.chunk(&chunk.hash);
        }
        let mut receipts = Vec::new();
        for chunk in chunks.iter().filter(|chunk| wanted(chunk)) {
            let bytes = upload.read(chunk).map_err(|source| ChunkError::Io {
                hash: chunk.hash.clone(),
                source,
            })?;
            if !koidrive_net::matches(chunk, &bytes) {
                return Err(ChunkError::SourceChanged {
                    hash: chunk.hash.clone(),
                });
            }
            self.chunks
                .chunks
                .insert(chunk.hash.clone(), bytes.to_vec());
            receipts.push(self.space.put_chunk(&chunk.hash, chunk.size, self.now_ms));
            crate::timing::add_bytes(&upload.timing_bytes, u64::from(chunk.size));
            upload.credit.chunk(&chunk.hash);
        }
        Ok(receipts)
    }

    /// What `Session::commit` does.
    fn commit(&mut self, caller: &Caller, commit: &Commit) -> Result<CommitResult, CallError> {
        match self.space.commit(caller, self.now_ms, commit) {
            Ok(committed) => {
                let id = commit.commit_id.clone();
                self.results.insert(id, committed.result.clone());
                Ok(committed.result)
            }
            Err(error) => Err(CallError::Server(ServerError(error))),
        }
    }

    /// The names of the live nodes besides the root, in order.
    fn names(&self) -> Vec<String> {
        let root = self.space.space_id();
        let live = koidrive_proto::v1::NodeStatus::Live as i32;
        let mut names: Vec<String> = (self.space.nodes().into_iter())
            .filter(|node| node.node_id != root && node.status == live)
            .map(|node| node.name)
            .collect();
        names.sort();
        names
    }

    fn tree(&self) -> BTreeMap<String, String> {
        let root = self.space.space_id();
        let live = koidrive_proto::v1::NodeStatus::Live as i32;
        let nodes: BTreeMap<String, koidrive_proto::v1::NodeState> =
            (self.space.nodes().into_iter())
                .filter(|node| node.status == live)
                .map(|node| (node.node_id.clone(), node))
                .collect();
        let path = |id: &String| {
            let mut names = Vec::new();
            let mut node = &nodes[id];
            while node.node_id != root {
                names.push(node.name.as_str());
                node = &nodes[&node.parent_id];
            }
            names.reverse();
            names.join("/")
        };
        (nodes.keys())
            .filter(|id| **id != root)
            .map(|id| (path(id), id.clone()))
            .collect()
    }
}

/// A device's folder and store on disk. They outlive the [`Device`], as they outlive a process.
struct Site {
    _dir: tempfile::TempDir,
    root: PathBuf,
    db: PathBuf,
    link: Link,
    caller: Caller,
}

impl Site {
    /// An empty folder linked to the remote's space, never synced.
    fn new(remote: &Remote, name: &str) -> Site {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("space");
        fs::create_dir(&root).unwrap();
        let space_id = remote.space.space_id().to_owned();
        let marker = Marker {
            space_id: space_id.clone(),
            effects: 0,
        };
        write_marker(&root, &marker).unwrap();
        let db = dir.path().join("store.sqlite");
        let link = Link {
            space_id,
            space_name: "test".into(),
            root: root.clone(),
            root_dev: fs::metadata(&root).unwrap().dev(),
            server_url: "https://files.example.com/".into(),
            device_id: name.into(),
            linked_at_ms: 0,
        };
        let mut store = Store::open(&db).unwrap();
        store.transaction(|tx| tx.set_link(&link)).unwrap();
        let caller = Caller {
            device_id: name.into(),
            user_id: "user".into(),
            device_name: name.into(),
            role: Role::Owner,
            max_format: MAX_FORMAT,
        };
        Site {
            _dir: dir,
            root,
            db,
            link,
            caller,
        }
    }

    /// The engine a process that started now would load from the store. Its trees must be trees.
    fn stored(&self) -> Engine {
        let mut store = Store::open(&self.db).unwrap();
        let engine = store
            .transaction(|tx| Engine::load(tx, &self.link.space_id))
            .unwrap();
        assert_eq!(engine.trees.check(), Ok(()));
        engine
    }

    /// Starts the folder as the task does once it holds the lock: the store, the engine, the disk, and the observer
    /// from L's snapshot.
    fn start(&self) -> Device {
        self.start_with(&SyncRules::default())
    }

    /// [`Site::start`] on a device with a `[sync]` list.
    fn start_with(&self, sync: &SyncRules) -> Device {
        let lock = claim::lock(&self.root)
            .unwrap()
            .expect("the folder is not locked");
        let local = local_rules(sync).unwrap();
        let folder = Folder::open(self.link.clone(), &lock, &self.db, local).unwrap();
        let observer = FsObserver::start(
            &self.root,
            &self.link.space_id,
            self.link.root_dev,
            folder.snapshot(),
            matcher(sync).unwrap(),
            || false,
        )
        .unwrap();
        Device {
            folder,
            observer,
            root: self.root.clone(),
            matcher: matcher(sync).unwrap(),
            caller: self.caller.clone(),
            work: VecDeque::new(),
            sent: Vec::new(),
            _lock: lock,
        }
    }
}

/// A running folder with what the shell holds for it.
struct Device {
    folder: Folder,
    observer: FsObserver,
    root: PathBuf,
    matcher: Arc<dyn Matcher>,
    caller: Caller,
    /// Work the folder asked for that the test has not run.
    work: VecDeque<Work>,
    /// Every commit handed to `Session::commit`, in order.
    sent: Vec<Commit>,
    _lock: claim::Flock,
}

impl Device {
    /// Takes what one step returned. The store must then give the engine the folder holds.
    fn take(&mut self, step: Result<Vec<Work>, Stop>) -> Result<(), Stop> {
        self.work.extend(step?);
        let folder = &mut self.folder;
        folder.track_progress(folder.view().state);
        if folder.timing.is_on() || folder.timing.has_first() {
            folder.record_status(folder.view().state);
        }
        let stored = folder
            .store
            .transaction(|tx| Engine::load(tx, &folder.link.space_id))
            .unwrap();
        let held = &folder.engine;
        assert_eq!(held.trees.check(), Ok(()));
        assert_eq!(stored.trees.local(), held.trees.local(), "L");
        assert_eq!(stored.trees.synced(), held.trees.synced(), "S");
        assert_eq!(stored.trees.remote(), held.trees.remote(), "R");
        assert_eq!(stored.trees.state(), held.trees.state());
        assert_eq!(stored.commits.state(), held.commits.state());
        assert_eq!(stored.effects.state(), held.effects.state());
        assert_eq!(stored.cursor, held.cursor);
        assert_eq!(stored.ids.state, held.ids.state);
        Ok(())
    }

    fn event(&mut self, event: Event) -> Result<(), Stop> {
        let step = self.folder.event(event);
        self.take(step)
    }

    fn connect(&mut self, remote: &Remote) {
        let welcome = remote.space.welcome(&self.caller);
        self.event(Event::Connected(welcome)).unwrap();
    }

    /// One pass of the observer's thread.
    fn scan(&mut self, wait: Duration) {
        self.try_scan(wait).unwrap();
    }

    /// One pass of the observer's thread. Returns the folders its batch could not read.
    fn try_scan(&mut self, wait: Duration) -> Result<Vec<PathBuf>, Stop> {
        let sections = self.folder.sections();
        let mut unreadable = Vec::new();
        if let Some(batch) = sections.observe(&mut self.observer, wait).unwrap() {
            unreadable.clone_from(&batch.observation.unreadable);
            let entries = self.observer.snapshot().entries.len() as u64;
            let step = self.folder.observed(batch, entries);
            self.take(step)?;
        }
        if !self.folder.scanned && self.observer.scanned().is_some() {
            let step = self.folder.scanned();
            self.take(step)?;
        }
        Ok(unreadable)
    }

    fn rescan(&mut self) {
        self.observer.reset(self.folder.engine.resets([PathBuf::new()]));
    }

    /// Observes until `done`.
    fn scan_until(&mut self, done: impl Fn(&Device) -> bool) {
        eventually("the observer brings the awaited state", || {
            done(self) || {
                self.scan(Duration::from_millis(100));
                false
            }
        });
    }

    fn settle(&mut self) {
        let mut caught_up = false;
        eventually("the observer catches up with the disk", || {
            self.scan(Duration::from_millis(100));
            let mut known = self.observer.snapshot().clone();
            let rescan = known.rescan_with_rules(
                &self.root,
                [PathBuf::new()],
                &mut scan_rules(&self.matcher),
            );
            let unseen = rescan.map_or(true, |rescan| {
                !rescan.changes.is_empty() || !rescan.unsettled.is_empty()
            });
            let now = self.observer.answered() && self.observer.scanned().is_some() && !unseen;
            let settled = now && caught_up;
            caught_up = now;
            settled
        });
    }

    /// What the session delivers after a `Resume`: the log after `since_seq`, with this device's own commits as
    /// their results, then `CaughtUp`.
    fn resume(&mut self, remote: &Remote, resume: Resume) -> Result<(), Stop> {
        let records = match remote.space.resume(&resume) {
            Ok(records) => records,
            Err(resync) => return self.event(Event::ResyncRequired(resync)),
        };
        for record in records {
            let event = if record.device_id == self.caller.device_id {
                Event::Committed(Committed {
                    result: remote.results[&record.commit_id].clone(),
                    commit_id: record.commit_id,
                })
            } else {
                Event::Changes(vec![record])
            };
            self.event(event)?;
        }
        self.caught_up(remote)
    }

    fn caught_up(&mut self, remote: &Remote) -> Result<(), Stop> {
        let head_seq = remote.space.head_seq();
        let (head_commit_id, head_committed_at_ms) =
            remote.space.token_at(head_seq).unwrap_or_default();
        self.event(Event::CaughtUp(CaughtUp {
            head_seq,
            head_commit_id,
            head_committed_at_ms,
        }))
    }

    /// The `Resume` of a session that follows the log from the cursor.
    fn since(&self) -> Resume {
        let cursor = self
            .folder
            .engine
            .cursor
            .clone()
            .expect("R follows the log");
        Resume {
            since_seq: cursor.last_seq,
            since_commit_id: cursor.commit_id,
            since_committed_at_ms: cursor.committed_at_ms,
            history_id: cursor.history_id,
        }
    }

    /// What a live session pushes: everything after the cursor.
    fn pull(&mut self, remote: &Remote) {
        self.resume(remote, self.since()).unwrap();
    }

    /// What a live session pushes when everything after the cursor arrives in one message.
    fn pull_at_once(&mut self, remote: &Remote) {
        let records = remote.space.resume(&self.since()).unwrap();
        self.event(Event::Changes(records)).unwrap();
        self.caught_up(remote).unwrap();
    }

    /// Runs one piece of work and feeds its result back.
    fn perform(&mut self, work: Work, remote: &mut Remote) -> Result<(), Stop> {
        let step = match work {
            Work::Subscribe(since) => return self.resume(remote, since),
            Work::List { trash } | Work::ListAfter(_, Listing { trash, .. }) => {
                let trash = match trash {
                    true => remote.trash_pages(TREE_PAGE),
                    false => Vec::new(),
                };
                let pages = remote.pages(TREE_PAGE);
                self.folder.listed(Ok(Listed::Done { pages, trash }))
            }
            Work::Stage(pending) | Work::StageAfter(_, pending) => {
                let staged = ready(pending.stage(&remote.chunks));
                self.folder.staged(pending, staged)
            }
            Work::Upload(upload) => {
                let receipts = remote.store(&self.caller, &upload);
                self.folder.uploaded(receipts)
            }
            Work::Send(commit) => {
                self.sent.push(commit.clone());
                let result = remote.commit(&self.caller, &commit);
                self.folder.sent(result)
            }
            Work::ResendAfter(_) => self.folder.resend(),
            Work::RetryAfter(_) => self.folder.retry(),
            Work::Reconnect => {
                let welcome = remote.space.welcome(&self.caller);
                self.folder.event(Event::Connected(welcome))
            }
        };
        self.take(step)
    }

    /// Runs the work the folder asked for and the work that follows from it, until none is left. Returns what ran.
    fn try_run(&mut self, remote: &mut Remote) -> Result<Vec<&'static str>, Stop> {
        let mut ran = Vec::new();
        let started = Instant::now();
        while let Some(work) = self.work.pop_front() {
            assert!(
                started.elapsed() < WAIT,
                "the folder stops asking for work within {WAIT:?}"
            );
            ran.push(match &work {
                Work::Subscribe { .. } => "subscribe",
                Work::List { .. } => "list",
                Work::ListAfter(..) => "list again",
                Work::Stage(_) => "stage",
                Work::StageAfter(..) => "stage again",
                Work::Upload(_) => "upload",
                Work::Send(_) => "send",
                Work::ResendAfter(_) => "wait",
                Work::RetryAfter(_) => "retry",
                Work::Reconnect => "reconnect",
            });
            self.perform(work, remote)?;
        }
        Ok(ran)
    }

    fn run(&mut self, remote: &mut Remote) -> Vec<&'static str> {
        self.try_run(remote).unwrap()
    }

    fn rest(&mut self, remote: &mut Remote) {
        eventually("the folder comes to rest", || {
            self.run(remote);
            self.settle();
            self.work.is_empty()
        });
    }

    /// Writes a file and syncs it to the space.
    fn create(&mut self, remote: &mut Remote, path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).unwrap();
        self.rescan();
        self.scan_until(|device| !device.work.is_empty());
        assert_eq!(self.run(remote), ["upload", "send"]);
    }

    fn state(&self) -> FolderState {
        self.folder.view().state
    }
}

/// A device that linked an empty folder to the remote's space and read the space's tree.
fn linked(remote: &mut Remote, name: &str) -> (Site, Device) {
    let site = Site::new(remote, name);
    let mut device = site.start();
    device.connect(remote);
    device.scan_until(|device| device.folder.scanned);
    device.run(remote);
    (site, device)
}

/// The names in a folder besides `.koidrive`, in order.
fn names(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name != koidrive_fs::PRIVATE_DIR)
        .collect();
    names.sort();
    names
}

/// Every file under a folder besides `.koidrive`, by its path from the folder, with its text, in order.
fn files(root: &Path) -> Vec<(String, String)> {
    let mut files = Vec::new();
    let mut dirs = vec![root.to_owned()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path == root.join(koidrive_fs::PRIVATE_DIR) {
                continue;
            }
            if path.is_dir() {
                dirs.push(path);
            } else {
                let name = path.strip_prefix(root).unwrap().to_str().unwrap();
                files.push((name.to_owned(), fs::read_to_string(&path).unwrap()));
            }
        }
    }
    files.sort();
    files
}

/// Every entry under a folder besides `.koidrive`, by its path from the folder, with the text of a file, in order.
fn entries(root: &Path) -> Vec<(String, Option<String>)> {
    let mut entries = Vec::new();
    let mut dirs = vec![root.to_owned()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path == root.join(koidrive_fs::PRIVATE_DIR) {
                continue;
            }
            let name = path
                .strip_prefix(root)
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned();
            if path.is_dir() {
                entries.push((name, None));
                dirs.push(path);
            } else {
                entries.push((name, Some(fs::read_to_string(&path).unwrap())));
            }
        }
    }
    entries.sort();
    entries
}

fn sync(remote: &mut Remote, a: (&Site, &mut Device), b: (&Site, &mut Device)) {
    let (mut busy, mut rounds) = (true, 0);
    while busy {
        rounds += 1;
        assert!(rounds < 200, "the devices do not come to rest");
        busy = false;
        for (site, device) in [(a.0, &mut *a.1), (b.0, &mut *b.1)] {
            match device.work.pop_front() {
                Some(work) => {
                    device.perform(work, remote).unwrap();
                    busy = true;
                }
                None => device.pull(remote),
            }
            device.settle();
            busy |= !device.work.is_empty();
            site.stored();
        }
    }
    assert_eq!(a.1.state(), FolderState::UpToDate);
    assert_eq!(b.1.state(), FolderState::UpToDate);
    assert_eq!(entries(&a.0.root), entries(&b.0.root));
}

/// Two devices that hold the folder `f` with `f/kept.txt`, written on `b`. Device `a` then wrote the file `a`, and
/// `b` planned the effect that writes it, which is returned unstaged.
fn planned_beside_a_folder(remote: &mut Remote) -> ((Site, Device), (Site, Device), Pending) {
    let (site_a, mut a) = linked(remote, "a");
    let (site, mut b) = linked(remote, "b");
    fs::create_dir(site.root.join("f")).unwrap();
    fs::write(site.root.join("f/kept.txt"), b"kept on b").unwrap();
    sync(remote, (&site_a, &mut a), (&site, &mut b));
    assert_eq!(entries(&site_a.root).len(), 2);
    a.create(remote, &site_a.root.join("a"), b"written on a");

    b.pull(remote);
    let Some(Work::Stage(pending)) = b.work.pop_front() else {
        panic!("the effect that writes a is staged");
    };
    assert!(b.work.is_empty());
    site.stored();
    ((site_a, a), (site, b), pending)
}

/// The user renames the folder `f` to `a`, which a batch reports, and back, which no batch has reported yet.
fn rename_to_a_and_back(site: &Site, b: &mut Device) {
    fs::rename(site.root.join("f"), site.root.join("a")).unwrap();
    b.scan_until(|b| {
        (b.folder.engine.trees.local().iter())
            .any(|(_, node)| node.shape.name == "a" && node.shape.kind == Kind::Dir)
    });
    site.stored();
    fs::rename(site.root.join("a"), site.root.join("f")).unwrap();
}

/// Device `a` synced `notes.txt` and stopped with its second commit built and uploaded but never sent. Returns its
/// site, which keeps the store, and that commit.
fn stopped_with_a_commit_in_flight(remote: &mut Remote) -> (Site, Commit) {
    let (site, mut a) = linked(remote, "a");
    a.create(remote, &site.root.join("notes.txt"), b"written on a");
    fs::write(site.root.join("draft.txt"), b"never sent").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let upload = a.work.pop_front().unwrap();
    a.perform(upload, remote).unwrap();
    let Some(Work::Send(commit)) = a.work.pop_front() else {
        panic!("the commit is sent after its upload");
    };
    assert_eq!(commit.device_seq, 2);
    (site, commit)
}

fn kinds(commit: &Commit) -> Vec<&'static str> {
    (commit.ops.iter())
        .map(|op: &Op| match &op.op {
            Some(op::Op::CreateFile(_)) => "create file",
            Some(op::Op::UpdateContent(_)) => "update content",
            Some(op::Op::Restore(_)) => "restore",
            Some(op::Op::Delete(_)) => "delete",
            _ => "another op",
        })
        .collect()
}

/// A refusal of the server that puts a call off for an hour.
fn put_off(code: koidrive_proto::v1::error::Code) -> ServerError {
    ServerError(koidrive_proto::v1::Error {
        code: code as i32,
        message: "not now".into(),
        retry_after_ms: 3_600_000,
        ..Default::default()
    })
}

/// Device `a` wrote `notes.txt`, and device `b` staged the effect that writes it. A file of the user's then holds the name
/// on `b`, which no batch has reported, so the effect fails with the name taken and the node is blocked. The user's
/// file is left as it was.
fn failed_at_a_taken_name(remote: &mut Remote) -> (Site, Device, Duration) {
    let (site_a, mut a) = linked(remote, "a");
    let (site, mut b) = linked(remote, "b");
    a.create(remote, &site_a.root.join("notes.txt"), b"written on a");
    b.pull(remote);
    let Some(Work::Stage(pending)) = b.work.pop_front() else {
        panic!("the effect that writes notes.txt is staged");
    };
    let wait = take_the_name_and_apply(remote, &site, &mut b, pending);
    (site, b, wait)
}

/// Puts a file of the user's at `notes.txt`, applies `pending` and returns the wait before its node is unblocked.
fn take_the_name_and_apply(
    remote: &Remote,
    site: &Site,
    b: &mut Device,
    pending: Pending,
) -> Duration {
    fs::write(site.root.join("notes.txt"), b"the user's").unwrap();
    let staged = ready(pending.stage(&remote.chunks));
    let step = b.folder.staged(pending, staged);
    b.take(step).unwrap();
    assert_eq!(b.folder.engine.effects.blocked().len(), 1);
    assert_eq!(
        fs::read(site.root.join("notes.txt")).unwrap(),
        b"the user's"
    );
    let Some(Work::RetryAfter(wait)) = b.work.pop_front() else {
        panic!("the node is unblocked after a wait: {:?}", b.work.len());
    };
    assert!(b.work.is_empty());
    wait
}

/// Device `b` links a folder holding an entry whose name only folds like the name of a file the space holds, and one more file,
/// and answers the question its first link asks. Returns its site and device and the space's file name.
fn a_first_link_with_a_name_clash(remote: &mut Remote, theirs: &str, mine: &str) -> (Site, Device) {
    let (site_a, mut a) = linked(remote, "a");
    a.create(remote, &site_a.root.join(theirs), b"written on a");

    let site = Site::new(remote, "b");
    fs::write(site.root.join(mine), b"already here").unwrap();
    fs::write(site.root.join("mine.txt"), b"mine").unwrap();
    let mut b = site.start();
    b.connect(remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(remote);
    b.settle();
    b.run(remote);
    assert!(
        b.sent.is_empty(),
        "{mine}: nothing is sent before an answer"
    );
    let mut kept = [
        (mine.to_owned(), "already here".to_owned()),
        ("mine.txt".to_owned(), "mine".to_owned()),
    ];
    kept.sort();
    assert_eq!(
        files(&site.root),
        kept,
        "{mine}: nothing is downloaded over it"
    );
    assert_eq!(remote.names(), [theirs]);
    assert_eq!(b.folder.view().questions, 1);
    (site, b)
}

/// Records a restore request in the site's store, as `Driver::restore` does from its own connection.
fn ask(site: &Site, intent: &RestoreIntent) -> String {
    let id = crate::restore::new_id(NOW as u64);
    let mut store = Store::open(&site.db).unwrap();
    store
        .transaction(|tx| tx.request_restore(&id, intent))
        .unwrap();
    id
}

/// The node R holds under `name`.
fn node_named(device: &Device, name: &str) -> String {
    let remote = device.folder.engine.trees.remote();
    let (id, _) = (remote.iter())
        .find(|(_, node)| node.shape.name == name)
        .expect("R holds the node");
    id.clone()
}

/// The restores that ended, as the site's store holds them. Nothing may wait in the inbox or in the engine.
fn ended(site: &Site) -> Vec<(String, Outcome)> {
    let mut store = Store::open(&site.db).unwrap();
    let (inbox, (queued, resolved)) = store
        .transaction(|tx| Ok::<_, koidrive_store::Error>((tx.restore_requests()?, tx.restores()?)))
        .unwrap();
    assert!(inbox.is_empty() && queued.is_empty());
    (resolved.into_iter())
        .map(|ended| (ended.id, ended.outcome))
        .collect()
}

/// Device `a` synced `notes.txt` and then deleted it, so the space holds the file in its trash. Returns the file's
/// node.
fn with_a_trashed_file(remote: &mut Remote) -> (Site, Device, String) {
    let (site, mut a) = linked(remote, "a");
    let path = site.root.join("notes.txt");
    a.create(remote, &path, b"written on a");
    let node = node_named(&a, "notes.txt");
    fs::remove_file(&path).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    assert_eq!(a.run(remote), ["send"]);
    assert!(remote.names().is_empty());
    (site, a, node)
}

fn notes() -> Vec<(String, String)> {
    vec![("notes.txt".to_owned(), "written on a".to_owned())]
}

/// Whether L, as the site's store holds it, has an entry named `name`.
fn local_holds(site: &Site, name: &str) -> bool {
    (site.stored().trees.local().iter()).any(|(_, node)| node.shape.name == name)
}

/// A device whose first commit, of `notes.txt`, the server refused with `code`. Returns the folder, the refused
/// commit and why the task stopped. The device is gone, so its folder and store are free.
fn refused_first_commit(remote: &mut Remote, code: Code) -> (Site, Commit, String) {
    let (site, mut a) = linked(remote, "a");
    fs::write(site.root.join("notes.txt"), b"written on a").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let upload = a.work.pop_front().unwrap();
    a.perform(upload, remote).unwrap();
    let Some(Work::Send(commit)) = a.work.pop_front() else {
        panic!("the commit is sent");
    };
    let refusal = ServerError(koidrive_proto::v1::Error {
        code: code as i32,
        message: "refused".into(),
        ..Default::default()
    });
    let step = a.folder.sent(Err(CallError::Server(refusal)));
    let stop = a.take(step).unwrap_err();
    (site, commit, stop.to_string())
}

/// Starts the folder again and runs what follows its `Welcome` and its first scan.
fn restarted(site: &Site, remote: &mut Remote) -> Device {
    let mut device = site.start();
    device.connect(remote);
    device.scan_until(|device| device.folder.scanned);
    device.run(remote);
    device
}

/// Device `b` synced `one.txt` from `a`. The server then lost its log: a space with the same history and no
/// commits takes its place.
fn behind_the_device(remote: &mut Remote) -> (Site, Device) {
    let (site_a, mut a) = linked(remote, "a");
    a.create(remote, &site_a.root.join("one.txt"), b"one");
    let (site, mut b) = linked(remote, "b");
    b.settle();
    assert_eq!(names(&site.root), ["one.txt"]);
    *remote = Remote::new();
    (site, b)
}

/// A server restored from a backup taken now, which holds only the commits `device` sent so far and their chunks.
fn backup(remote: &Remote, device: &Device) -> Remote {
    let mut restored = Remote::new();
    let chunks: Vec<ChunkRef> = (remote.chunks.chunks.iter())
        .map(|(hash, bytes)| ChunkRef {
            hash: hash.clone(),
            size: bytes.len() as u32,
            ..Default::default()
        })
        .collect();
    if !chunks.is_empty() {
        (restored.space)
            .prepare_upload(&device.caller, NOW, &PrepareUpload { chunks })
            .unwrap();
    }
    for (hash, bytes) in &remote.chunks.chunks {
        restored.chunks.chunks.insert(hash.clone(), bytes.clone());
        restored.space.put_chunk(hash, bytes.len() as u32, NOW);
    }
    for commit in &device.sent {
        restored.space.commit(&device.caller, NOW, commit).unwrap();
    }
    restored
}

/// The names in the folder's local trash.
fn trashed(root: &Path) -> Vec<String> {
    let trash = root.join(koidrive_fs::PRIVATE_DIR).join("trash");
    fs::read_dir(trash).map_or(Vec::new(), |entries| {
        entries
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect()
    })
}

/// Device `c` commits to the server that is behind device `b`, so the server's head is no longer below `b`'s
/// cursor when `b` lists the tree.
fn catch_up(remote: &mut Remote, b: &Device) {
    let (site_c, mut c) = linked(remote, "c");
    c.create(remote, &site_c.root.join("other.txt"), b"other");
    let cursor = b.folder.engine.cursor.as_ref().unwrap();
    assert!(remote.space.head_seq() >= cursor.last_seq);
}

/// What `b` holds and the server holds once `b` sent what the server lost and took `c`'s file.
fn caught_up(site: &Site, b: &Device, remote: &Remote, also: &[&str]) {
    let mut expected = vec![("one.txt".to_owned(), "one".to_owned())];
    expected.extend(
        also.iter()
            .map(|name| (format!("{name}.txt"), name.to_string())),
    );
    expected.push(("other.txt".to_owned(), "other".to_owned()));
    expected.sort();
    assert_eq!(files(&site.root), expected);
    assert_eq!(trashed(&site.root), Vec::<String>::new());
    let names: Vec<String> = expected.into_iter().map(|(name, _)| name).collect();
    assert_eq!(remote.names(), names);
    assert_eq!(b.state(), FolderState::UpToDate);
}

/// The size of the content the server holds for the node.
fn server_size(remote: &Remote, id: &str) -> u64 {
    let nodes = remote.space.nodes();
    let node = nodes.iter().find(|node| node.node_id == id).unwrap();
    let version = node.current.as_ref().unwrap();
    version.content.as_ref().unwrap().size
}

/// What takes the name of a node L keeps for an ignored file.
#[derive(Clone, Copy, Debug)]
enum Taker {
    NewFile,
    NewFolder,
    MovedFolder,
    MovedFile,
}

/// When the device restarts: never, while the taker arrives, or after the taker is synced.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Restart {
    Never,
    During,
    After,
}

/// The user moves `source/file` into the ignored folder, so L keeps the file's entry, and with `folder` also
/// removes `source`, so L keeps `source` for the file. Then `taker` takes the name L keeps: `source` with `folder`,
/// `source/file` without. The file's only local copy is in the ignored folder, so the server keeps the file with its
/// content and the folder it is in, whenever the device restarts. The server still holds the name, so the taker is
/// synced under a conflict name, on the server and on the disk.
fn a_name_l_keeps_is_taken(folder: bool, taker: Taker) {
    for restart in [Restart::Never, Restart::During, Restart::After] {
        let case = format!("{taker:?} {restart:?}");
        let mut remote = Remote::new();
        let (site, mut a) = linked(&mut remote, "a");
        fs::create_dir(site.root.join("source")).unwrap();
        a.create(&mut remote, &site.root.join("source/file"), b"kept");
        fs::create_dir(site.root.join("other")).unwrap();
        a.create(&mut remote, &site.root.join("other/inner"), b"inner");
        a.create(&mut remote, &site.root.join("note"), b"note");
        a.create(&mut remote, &site.root.join(".koiignore"), b"ignored/\n");
        let before = server_nodes(&remote);
        fs::create_dir(site.root.join("ignored")).unwrap();
        fs::rename(
            site.root.join("source/file"),
            site.root.join("ignored/file"),
        )
        .unwrap();
        if folder {
            fs::remove_dir(site.root.join("source")).unwrap();
        }
        let recorded = if folder { 2 } else { 1 };
        a.scan_until(|a| a.folder.engine.trees.ignored_local().len() == recorded);
        a.run(&mut remote);
        let start = |remote: &mut Remote| {
            let mut a = site.start();
            a.connect(remote);
            a.scan_until(|a| a.folder.scanned);
            a.run(remote);
            a
        };
        let target = site
            .root
            .join(if folder { "source" } else { "source/file" });
        let take = || match taker {
            Taker::NewFile => fs::write(&target, b"taker").unwrap(),
            Taker::NewFolder => {
                fs::create_dir(&target).unwrap();
                fs::write(target.join("new"), b"taker").unwrap();
            }
            Taker::MovedFolder => fs::rename(site.root.join("other"), &target).unwrap(),
            Taker::MovedFile => fs::rename(site.root.join("note"), &target).unwrap(),
        };
        if restart == Restart::During {
            drop(a);
            take();
            a = start(&mut remote);
        } else {
            take();
        }
        for _ in 0..3 {
            a.settle();
            a.run(&mut remote);
        }
        if restart == Restart::After {
            drop(a);
            a = start(&mut remote);
            for _ in 0..2 {
                a.settle();
                a.run(&mut remote);
            }
        }
        let after = server_nodes(&remote);
        let live = |name: &str| {
            let id = live_id(&before, name);
            after.iter().any(|(_, node, live)| *node == id && *live)
        };
        assert!(live("file") && live("source"), "{case}: {after:?}");
        assert!(live("inner") && live("note"), "{case}: {after:?}");
        assert_eq!(server_size(&remote, &live_id(&before, "file")), 4, "{case}");
        let taken = if folder { "source" } else { "file" };
        let conflict = format!("{taken} (conflict from a 2023-11-14)");
        live_id(&after, &conflict);
        assert!(target.with_file_name(&conflict).exists(), "{case}");
        assert!(!target.exists(), "{case}");
        assert_eq!(
            fs::read(site.root.join("ignored/file")).unwrap(),
            b"kept",
            "{case}"
        );
        assert_eq!(a.folder.engine.trees.check(), Ok(()), "{case}");
    }
}

/// The server's nodes by name, with their IDs and whether each is live.
type Nodes = Vec<(String, String, bool)>;

/// Every node on the server by name, with its ID and whether it is live, in order.
fn server_nodes(remote: &Remote) -> Nodes {
    let live = koidrive_proto::v1::NodeStatus::Live as i32;
    let mut nodes: Nodes = (remote.space.nodes().into_iter())
        .filter(|node| !node.parent_id.is_empty())
        .map(|node| (node.name, node.node_id, node.status == live))
        .collect();
    nodes.sort();
    nodes
}

/// The ID of the live node named `name` in `nodes`.
fn live_id(nodes: &[(String, String, bool)], name: &str) -> String {
    let found = nodes.iter().find(|(named, _, live)| named == name && *live);
    found
        .unwrap_or_else(|| panic!("no live {name} in {nodes:?}"))
        .1
        .clone()
}

/// Device `a` makes `files`, then device `b` makes each rename of `renames` in turn before its observer looks, and
/// both sync. Returns both sites and the server's nodes before and after the renames.
fn renamed_on_b(files: &[(&str, &[u8])], renames: &[(&str, &str)]) -> (Site, Site, Nodes, Nodes) {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    for (path, bytes) in files {
        let path = site_a.root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        a.create(&mut remote, &path, bytes);
    }
    let (site, mut b) = linked(&mut remote, "b");
    b.settle();
    let before = server_nodes(&remote);
    for (from, to) in renames {
        fs::rename(site.root.join(from), site.root.join(to)).unwrap();
    }
    sync(&mut remote, (&site_a, &mut a), (&site, &mut b));
    (site_a, site, before, server_nodes(&remote))
}

fn forbidden() -> ServerError {
    ServerError(koidrive_proto::v1::Error {
        code: Code::Forbidden as i32,
        message: "not a member".into(),
        ..Default::default()
    })
}

/// Device `a` synced the folder `d` with 15 files and 5 files beside it. The folder was removed while `a` was
/// stopped, so the first scan of the next start reports the whole removal. The guard holds it (design §6.1): the
/// server keeps every file and the store holds one question about `d`.
fn with_a_held_folder(remote: &mut Remote) -> (Site, Device) {
    let (site, mut a) = linked(remote, "a");
    fs::create_dir(site.root.join("d")).unwrap();
    for i in 0..15 {
        fs::write(site.root.join(format!("d/{i}.txt")), format!("in d {i}")).unwrap();
    }
    for i in 0..5 {
        fs::write(site.root.join(format!("{i}.txt")), format!("beside d {i}")).unwrap();
    }
    a.scan_until(|a| !a.work.is_empty());
    a.run(remote);
    a.settle();
    a.run(remote);
    assert_eq!(remote.names().len(), 21);
    drop(a);
    fs::remove_dir_all(site.root.join("d")).unwrap();

    let mut a = restarted(&site, remote);
    a.settle();
    a.run(remote);
    assert!(
        a.sent
            .iter()
            .all(|commit| !kinds(commit).contains(&"delete"))
    );
    assert_eq!(remote.names().len(), 21, "nothing is deleted on the server");
    assert_eq!(a.folder.view().questions, 1);
    (site, a)
}

/// Answers the one question of the site's store, as `Driver::answer` records it, and returns it.
fn answer(site: &Site, answer: Answer) -> crate::question::Question {
    let mut store = Store::open(&site.db).unwrap();
    store
        .transaction(|tx| {
            let questions = tx.questions()?;
            let [stored] = &questions[..] else {
                panic!("one question: {questions:?}");
            };
            let given = koidrive_store::GivenAnswer {
                answer: answer.as_str().into(),
                answered_at_ms: NOW,
            };
            assert!(tx.answer_question(&stored.question_id, &given)?);
            Ok::<_, koidrive_store::Error>(serde_json::from_slice(&stored.body).unwrap())
        })
        .unwrap()
}

/// A folder held while the device was stopped, a file removed beside it, and an answer given before
/// the device starts again.
fn answered_while_stopped(remote: &mut Remote, given: Answer) -> Device {
    let (site, a) = with_a_held_folder(remote);
    drop(a);
    fs::remove_file(site.root.join("0.txt")).unwrap();
    assert_eq!(answer(&site, given).items.len(), 1);
    let mut a = restarted(&site, remote);
    a.settle();
    a.run(remote);
    a
}

/// The questions of the site's store as `Driver::questions` shows them.
fn shown(site: &Site) -> Vec<crate::question::Question> {
    let mut store = Store::open(&site.db).unwrap();
    let rows = store.transaction(|tx| tx.questions()).unwrap();
    (rows.iter())
        .map(|row| serde_json::from_slice(&row.body).unwrap())
        .collect()
}

const COPY: &str = "main (conflict from b 2023-11-14).go";

/// Devices `a` and `b` hold `main.go`, and both edit it. `a` commits first, and `b` commits before it hears of
/// that, so the server keeps `b`'s edit as the conflict copy [`COPY`]. Then both sync.
fn edited_on_both(remote: &mut Remote) -> ((Site, Device), (Site, Device)) {
    let (site_a, mut a) = linked(remote, "a");
    a.create(remote, &site_a.root.join("main.go"), b"v1");
    let (site_b, mut b) = linked(remote, "b");
    b.settle();
    fs::write(site_a.root.join("main.go"), b"from a").unwrap();
    fs::write(site_b.root.join("main.go"), b"from b").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(remote);
    b.scan_until(|b| !b.work.is_empty());
    b.run(remote);
    sync(remote, (&site_a, &mut a), (&site_b, &mut b));
    ((site_a, a), (site_b, b))
}

/// The server is restored from a backup taken before `b` answered the conflict between `main.go` and [`COPY`],
/// after `b` carried the answer out. `b` sends what the answer wrote again, and the question stays closed.
fn answered_then_lost(given: Answer) -> (Site, Remote) {
    let mut remote = Remote::new();
    let ((site_a, mut a), (site_b, mut b)) = edited_on_both(&mut remote);
    let mut chunks = Server::default();
    chunks.chunks.clone_from(&remote.chunks.chunks);
    let mut restored = Remote {
        now_ms: remote.now_ms,
        space: remote.space.clone(),
        chunks,
        results: remote.results.clone(),
    };
    answer(&site_b, given);
    let step = b.folder.requested();
    b.take(step).unwrap();
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));
    assert_eq!(remote.names(), ["main.go"]);
    b.event(Event::Connected(restored.space.welcome(&b.caller)))
        .unwrap();
    for _ in 0..3 {
        b.pull(&restored);
        b.run(&mut restored);
        b.settle();
    }
    assert_eq!(b.state(), FolderState::UpToDate);
    (site_b, restored)
}

fn original_changed_before_the_answer_reaches_the_server(
    change: impl FnOnce(&Path),
) -> (Remote, Site, Site) {
    let mut remote = Remote::new();
    let ((site_a, mut a), (site_b, mut b)) = edited_on_both(&mut remote);
    answer(&site_b, Answer::KeepOriginal);
    let step = b.folder.requested();
    b.take(step).unwrap();
    assert!(matches!(b.work.front(), Some(Work::Send(_))));
    change(&site_a.root);
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    b.run(&mut remote);
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));
    (remote, site_a, site_b)
}

fn a_file_parked_behind_an_excluded_folder() -> (Remote, Site, Device, PathBuf) {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("a"), b"moved on a");
    let (site, mut b) = linked(&mut remote, "b");
    fs::rename(site_a.root.join("a"), site_a.root.join("x")).unwrap();
    a.settle();
    a.run(&mut remote);
    fs::create_dir(site_a.root.join("a")).unwrap();
    fs::rename(site_a.root.join("x"), site_a.root.join("a/ß")).unwrap();
    a.settle();
    a.run(&mut remote);
    let parked = site.root.join(".koidrive/parked").join(node_named(&b, "a"));
    b.pull_at_once(&remote);
    while !site.root.join("a").is_dir() {
        let work = b.work.pop_front().expect("the folder is created");
        b.perform(work, &mut remote).unwrap();
    }
    fs::create_dir(site.root.join("a/ß")).unwrap();
    fs::write(site.root.join("a/ß/inner.txt"), b"only on b").unwrap();
    b.scan_until(|b| {
        (b.folder.engine.trees.local().iter()).any(|(_, node)| node.shape.name == "ß")
    });
    drop(b);
    let sync = SyncRules {
        include: Vec::new(),
        exclude: vec!["ss/".into()],
    };
    let mut b = site.start_with(&sync);
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    for _ in 0..2 {
        b.run(&mut remote);
        b.settle();
    }
    assert_eq!(fs::read(&parked).unwrap(), b"moved on a");
    assert!(b.folder.view().problem.is_some());
    (remote, site, b, parked)
}

fn keep_original_lost_by_a_restore(deleted_on_c: bool) -> (Remote, Site, Vec<Commit>) {
    let mut remote = Remote::new();
    let ((site_a, mut a), (site_b, mut b)) = edited_on_both(&mut remote);
    let mut chunks = Server::default();
    chunks.chunks.clone_from(&remote.chunks.chunks);
    let mut restored = Remote {
        now_ms: remote.now_ms,
        space: remote.space.clone(),
        chunks,
        results: remote.results.clone(),
    };
    answer(&site_b, Answer::KeepOriginal);
    let step = b.folder.requested();
    b.take(step).unwrap();
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));
    assert_eq!(remote.names(), ["main.go"]);

    let (site_c, mut c) = linked(&mut restored, "c");
    c.settle();
    assert_eq!(names(&site_c.root), [COPY, "main.go"]);
    if deleted_on_c {
        fs::remove_file(site_c.root.join("main.go")).unwrap();
        c.scan_until(|c| !c.work.is_empty());
        c.run(&mut restored);
    }

    let sent = b.sent.len();
    b.event(Event::Connected(restored.space.welcome(&b.caller)))
        .unwrap();
    for _ in 0..3 {
        b.pull(&restored);
        b.run(&mut restored);
        b.settle();
    }
    let resent = b.sent[sent..].to_vec();
    (restored, site_b, resent)
}

fn resent_deletes(resent: &[Commit]) -> Vec<&koidrive_proto::v1::Delete> {
    (resent.iter().flat_map(|commit| &commit.ops))
        .filter_map(|sent| match &sent.op {
            Some(op::Op::Delete(delete)) => Some(delete),
            _ => None,
        })
        .collect()
}

fn keep_original_lost_below_a_removed_folder(deleted_on_c: bool) -> (Remote, Commit) {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir(site_a.root.join("d")).unwrap();
    a.create(&mut remote, &site_a.root.join("d/main.go"), b"v1");
    let (site_b, mut b) = linked(&mut remote, "b");
    b.settle();
    fs::write(site_a.root.join("d/main.go"), b"from a").unwrap();
    fs::write(site_b.root.join("d/main.go"), b"from b").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    b.scan_until(|b| !b.work.is_empty());
    b.run(&mut remote);
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));
    let mut chunks = Server::default();
    chunks.chunks.clone_from(&remote.chunks.chunks);
    let mut restored = Remote {
        now_ms: remote.now_ms,
        space: remote.space.clone(),
        chunks,
        results: remote.results.clone(),
    };
    answer(&site_b, Answer::KeepOriginal);
    let step = b.folder.requested();
    b.take(step).unwrap();
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));
    assert_eq!(remote.names(), ["d", "main.go"]);
    drop(b);
    fs::rename(site_b.root.join("d/main.go"), site_b.root.join("main.go")).unwrap();
    fs::remove_dir(site_b.root.join("d")).unwrap();

    let (site_c, mut c) = linked(&mut restored, "c");
    c.settle();
    fs::rename(site_c.root.join("d/main.go"), site_c.root.join("main.go")).unwrap();
    c.scan_until(|c| !c.work.is_empty());
    c.run(&mut restored);
    let mut b = site_b.start();
    b.connect(&restored);
    b.scan_until(|b| b.folder.scanned);
    let sent = loop {
        let work = b.work.pop_front().unwrap();
        if let Work::Send(commit) = work {
            break commit;
        }
        b.perform(work, &mut restored).unwrap();
    };
    if deleted_on_c {
        fs::remove_file(site_c.root.join("main.go")).unwrap();
        c.scan_until(|c| !c.work.is_empty());
        c.run(&mut restored);
    }
    b.perform(Work::Send(sent.clone()), &mut restored).unwrap();
    for _ in 0..3 {
        b.pull(&restored);
        b.run(&mut restored);
        b.settle();
    }
    (restored, sent)
}

fn keep_original_lost_then_its_folder_removed(original_moved_out: bool) -> (Remote, Site) {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir_all(site_a.root.join("d/e")).unwrap();
    a.create(&mut remote, &site_a.root.join("d/e/main.go"), b"v1");
    let (site_b, mut b) = linked(&mut remote, "b");
    b.settle();
    fs::write(site_a.root.join("d/e/main.go"), b"from a").unwrap();
    fs::write(site_b.root.join("d/e/main.go"), b"from b").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    b.scan_until(|b| !b.work.is_empty());
    b.run(&mut remote);
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));
    let mut chunks = Server::default();
    chunks.chunks.clone_from(&remote.chunks.chunks);
    let mut restored = Remote {
        now_ms: remote.now_ms,
        space: remote.space.clone(),
        chunks,
        results: remote.results.clone(),
    };
    answer(&site_b, Answer::KeepOriginal);
    let step = b.folder.requested();
    b.take(step).unwrap();
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));
    drop(b);
    if original_moved_out {
        fs::rename(site_b.root.join("d/e/main.go"), site_b.root.join("main.go")).unwrap();
    }
    fs::remove_dir_all(site_b.root.join("d")).unwrap();

    let mut b = site_b.start();
    b.connect(&restored);
    b.scan_until(|b| b.folder.scanned);
    for _ in 0..5 {
        b.run(&mut restored);
        b.pull(&restored);
        b.run(&mut restored);
        b.settle();
    }
    (restored, site_b)
}

fn keep_original_lost_after_the_original_was_deleted(deleted_on_b: bool) -> (Remote, Site) {
    let mut remote = Remote::new();
    let ((site_a, mut a), (site_b, mut b)) = edited_on_both(&mut remote);
    let mut chunks = Server::default();
    chunks.chunks.clone_from(&remote.chunks.chunks);
    let mut restored = Remote {
        now_ms: remote.now_ms,
        space: remote.space.clone(),
        chunks,
        results: remote.results.clone(),
    };
    answer(&site_b, Answer::KeepOriginal);
    let step = b.folder.requested();
    b.take(step).unwrap();
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));
    let site = if deleted_on_b { &site_b } else { &site_a };
    fs::remove_file(site.root.join("main.go")).unwrap();
    sync(&mut remote, (&site_a, &mut a), (&site_b, &mut b));
    assert_eq!(remote.names(), Vec::<String>::new());
    assert_eq!(names(&site_b.root), Vec::<String>::new());

    let (site_c, mut c) = linked(&mut restored, "c");
    c.settle();
    fs::remove_file(site_c.root.join("main.go")).unwrap();
    c.scan_until(|c| !c.work.is_empty());
    c.run(&mut restored);
    assert_eq!(restored.names(), [COPY]);

    b.event(Event::Connected(restored.space.welcome(&b.caller)))
        .unwrap();
    for _ in 0..3 {
        b.pull(&restored);
        b.run(&mut restored);
        b.settle();
    }
    (restored, site_b)
}

use crate::status::describe;

use koidrive_engine::question::{RecoveryCause, RecoveryTrigger};
