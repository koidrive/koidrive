// SPDX-License-Identifier: Apache-2.0

//! Whole devices against the reference server in one thread (docs/design/sync-engine.md section
//! 7.3). Each device has a disk, an engine, the copy of the engine state its store holds, and a
//! connection. A schedule names one step at a time: a user action on a disk, a rescan, an
//! effect, a commit, the delivery of any one message, or a fault. A fault can also restore the
//! server from an earlier copy in the same history, which loses the commits since (section 2.8).
//! After the schedule the faults stop and the devices run until nothing is left to do.
//!
//! The oracle uses the harness's own records and the server's state, never the engine's
//! bookkeeping. Every user write carries a tag in its content's `mtime_ns`, which the engine
//! does not compare, so two files with the same bytes are still two writes. A broken property is
//! a [`Failure`] of one of two kinds.
//!
//! [`Failure::Broken`]: a write is lost, or a state or a call is refused.
//!
//! - An effect takes a file off a disk only when the server holds its write, in its tree or in
//!   its trash.
//! - A commit takes a write out of the server's live tree only when the user of some device
//!   overwrote or deleted a file that held it, or when the commit deletes a folder above the
//!   file, which sends the write to the server's trash. The engine tells contents apart by their
//!   bytes, so a user who overwrites or deletes a file does the same to a later write of the same
//!   bytes to that file, and a commit that gives a file a write of the bytes it holds takes none
//!   out.
//! - A write the server does not hold stays on the disk of the device it was made on until that
//!   device's user overwrites or deletes it.
//! - A write that a restore took from the server while a disk held it is on the server again
//!   once the devices are quiet, unless a user overwrote or deleted it, or a person discarded it
//!   in answer to a recovery question and it is in the local trash of that person's device.
//! - L, S and R are valid trees after every step, the store gives back the state it was given,
//!   and the engine and the server accept every outcome and message the harness reports.
//! - The engine loads what the store holds after every transaction. The transaction that records
//!   the outcome of an effect comes before the disk is observed again, so L can still hold an
//!   entry that the user has moved or deleted since the last scan.
//!
//! [`Failure::Diverged`]: the faults have stopped, and the devices did not reach one tree.
//!
//! - Every disk equals the server's live tree, and every engine reports L = S = R with nothing
//!   in flight, blocked, parked or halted.
//! - No commit moved a node to a name that the user of its device never gave to anything, other
//!   than the name the server marks the node as pending for (RFC 0017) and, until the devices are
//!   quiet again, a name that L or the server held for a node whose shape a restore took from the
//!   server (section 4.4), a name the server gave that device's lost placement of that node, and
//!   the pending name a swap goes through to one of these names (design section 2.6), and no commit
//!   took a write out of the server's live tree that the user
//!   of its device never overwrote or deleted.
//! - No node keeps a pending name that no user gave.
//! - No write is in two files of the server, except one a restore took from the server while a
//!   disk held it (section 4.4). Until the devices are quiet again, it may be in one file for each
//!   node that held it, or for each device that held it in a node the restore rolled back to
//!   other content, and then in the files the server holds it in.
//! - A recovery copy of a write the restore kept shares its source's lineage only when that
//!   write's retained source version is newer than the copying device's newest own commit the
//!   server retains from before the server's last restore in the history, and the source holds
//!   different bytes when the copy applies. The device then cannot prove that the restore kept
//!   the write. Each device has its own bound. The
//!   copy shares the lineage only while the source holds other content; unrelated copies and
//!   copies of exposed writes keep the file-count check above. A recovery copy whose device's
//!   own retained commit shows the server kept the write holds it in no file once the devices
//!   are quiet, unless a restore took the write from the server or a user copied it.
//! - Every write was in the server's live tree at some time, unless the user who made it
//!   overwrote or deleted it first.

pub mod device;
pub mod disk;

use std::collections::{BTreeMap, BTreeSet};

use koidrive_engine::classify::Trees;
use koidrive_engine::commit::{self, Base, Reconnect};
use koidrive_engine::effects::{BatchLimit, Effect, EffectRecord, location_of};
use koidrive_engine::first_link::{self, History, Marks};
use koidrive_engine::names::{nfc, same_name};
use koidrive_engine::question::{Answer, QuestionKind, RecoveryCause, RecoveryTrigger};
use koidrive_engine::tree::{Content, NodeId, RemoteNode, Shape, same, same_content};
use koidrive_proto::v1::{
    ChunkRef, Commit, CommitResult, Error, ListVersions, Move, NodeKind, NodeState, NodeStatus,
    PrepareUpload, Resume, UploadReceipt, change_record, chunk_plan, error, op, op_result,
    resync_required,
};
use sha2::{Digest, Sha256};

use self::device::{Device, Engine, Link, Message};
pub use self::disk::Cut;
use self::disk::{Applied, Disk, Place, Refused};
use crate::space::{Role, Session, SpaceDo};

const NOW: i64 = 1_794_268_740_000;
/// Names that collide by case, by case folding and by normalization, so the server renames and
/// merges often. "\u{c9}" and "E\u{301}" are one name spelled in NFC and in NFD, and "\u{e9}"
/// collides with both by case.
pub const NAMES: &[&str] = &[
    "a", "A", "b", "c.txt", "C.TXT", "ß", "ss", "\u{c9}", "E\u{301}", "\u{e9}",
];
/// The name a scripted step gives with the name index `NAMES.len()`: the name the server gives
/// `A` while another node holds `a`.
pub const PENDING_NAME: &str = "A (pending)";
/// The contents a device writes. The pool is small, so separate files with the same bytes are
/// common on one device.
pub const TEXTS: u8 = 4;
/// The rounds the devices get to become quiet once the faults stop.
const ROUNDS: usize = 200;
const BATCH: u64 = 4;

/// A broken property, with what broke it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// A write was lost, or the engine, the store or the server refused a state or a call that
    /// the design promises.
    Broken(String),
    /// The devices did not reach the server's tree once the faults stopped, or a commit made a
    /// change that no user made.
    Diverged(String),
}

impl Failure {
    /// The same failure, with where it happened in front.
    fn at(self, place: String) -> Self {
        match self {
            Failure::Broken(why) => Failure::Broken(format!("{place}: {why}")),
            Failure::Diverged(why) => Failure::Diverged(format!("{place}: {why}")),
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (Failure::Broken(why) | Failure::Diverged(why)) = self;
        out.write_str(why)
    }
}

/// One change a user makes on a device's disk. The numbers pick among what the disk holds when
/// the action runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Action {
    /// 0 creates a folder, 1 creates a file, 2 overwrites a file, 3 moves or renames an entry
    /// and 4 deletes one with everything in it.
    pub kind: u8,
    pub target: usize,
    pub parent: usize,
    pub name: usize,
    pub text: u8,
}

/// How the effect in flight ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The disk does it, or refuses it because the disk is not what the effect expects.
    Run,
    /// It fails before it touches the disk, as a failed download does.
    Fail,
    /// The disk does it as far as the cut, and the device crashes before the store hears of it.
    Crash(Cut),
}

/// What happens to the commit a device sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply {
    /// The server answers it.
    Served,
    /// The server refuses it with this code before it applies anything. The server answers a
    /// commit that it already holds, and it gives `HEAD_MOVED` only to a commit that requires the
    /// head it was built on. For those the commit is served.
    Refused(error::Code),
    /// The chunk uploads are lost, so the server answers `MISSING_CHUNKS` for new chunks.
    NoChunks,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Act {
    MakeDir,
    MakeFile,
    Overwrite,
    Rename,
    Delete,
    Copy,
    Save { keeps_mark: bool },
}

impl Act {
    fn of(kind: u8) -> Act {
        match kind % 5 {
            0 => Act::MakeDir,
            1 => Act::MakeFile,
            2 => Act::Overwrite,
            3 => Act::Rename,
            _ => Act::Delete,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    User(Action),
    /// A full rescan of the disk.
    Scan,
    /// Plans the next effect when none is in flight, and ends the one in flight otherwise.
    Effect(Outcome),
    /// Builds the next commit when none is in flight, and sends the one in flight otherwise.
    Commit(Reply),
    Connect,
    /// Delivers one message on its way to the device, not necessarily the oldest.
    Deliver(usize),
    /// Makes the server send one message a second time.
    Duplicate(usize),
    /// Breaks the connection. Every message on its way is lost.
    Drop,
    /// Stops the device between two store transactions. The next step that is not a user action
    /// restarts it from the store.
    Crash,
    /// Does the first of these that has something to do, starting at the one the number picks:
    /// deliver a message, plan or end an effect, build or send a commit, rescan.
    Work(usize),
    /// Copies the server's storage. Any device may take the step; it does not touch the device.
    Backup,
    /// Restores the server's storage from the last copy, in the same history, and restarts the
    /// server, so every connection breaks. The commits since the copy are lost, and the server
    /// gives their seqs and device slots again (design section 2.8).
    Restore,
    /// Restores the server's storage from the copy before the last one, as [`Step::Restore`]
    /// does with the last.
    RestoreEarlier,
    /// Rewinds the space to the last copy of the server's storage with a new history, as its
    /// owner can (RFC 0003), so every connection breaks and every device recovers (section 6.4).
    Rewind,
    CopyStore,
    /// The user copies the target file to the parent and name the action picks.
    Copy(Action),
    /// The user saves the target file as an app that writes a new file and renames it over the old
    /// one does, which keeps the mark or not.
    Save {
        target: usize,
        keeps_mark: bool,
    },
    /// Answers the device's first open recovery question with the answer the number picks among
    /// those it offers (section 6.4).
    Answer(usize),
    /// Crashes the device and puts the last copy of its store back, as a store restored from a
    /// backup is. The disk stays as it is.
    RollBack,
    /// Takes the device away for good, with whatever it still owes the server.
    Leave,
}

/// The content of a write: three bytes that name the device and the text, so writes of different
/// devices never have the same bytes, and the write's tag.
fn content(device: u8, text: u8, write: i64) -> Content {
    let bytes = [device, text, text];
    let chunks = vec![ChunkRef {
        hash: Sha256::digest(bytes).to_vec(),
        size: 3,
        encoding: 0,
    }];
    let list: Vec<u8> = (chunks.iter())
        .flat_map(|c| c.hash.iter().copied().chain(c.size.to_le_bytes()))
        .collect();
    Content {
        size: 3,
        mtime_ns: write,
        chunk_count: 1,
        manifest_digest: Sha256::digest(list).to_vec(),
        chunks,
        ..Default::default()
    }
}

fn sources(commit: &Commit) -> impl Iterator<Item = (&NodeId, &Content)> {
    commit.ops.iter().filter_map(|op| match &op.op {
        Some(op::Op::CreateFile(create)) => Some((&create.node_id, create.content.as_ref()?)),
        Some(op::Op::UpdateContent(update)) => Some((&update.node_id, update.content.as_ref()?)),
        _ => None,
    })
}

fn readable(disk: &Disk, trees: &Trees, node: &str, content: &Content) -> bool {
    let (Some(at), Some(local)) = (location_of(trees, node), trees.local().get(node)) else {
        return false;
    };
    disk.reads(&at, local.identity.ino, content.mtime_ns)
}

fn upload(
    space: &mut SpaceDo,
    session: &Session,
    now_ms: i64,
    disk: &Disk,
    trees: &Trees,
    commit: &Commit,
) -> Vec<UploadReceipt> {
    let chunks: Vec<ChunkRef> = sources(commit)
        .filter(|(node, content)| readable(disk, trees, node, content))
        .map(|(_, content)| content)
        .flat_map(|content| content.chunks.clone())
        .collect();
    if chunks.is_empty() {
        return Vec::new();
    }
    let plan = space
        .prepare_upload(session, now_ms, &PrepareUpload { chunks })
        .expect("chunks of three bytes");
    (plan.chunks.iter())
        .filter(|chunk| chunk.action == chunk_plan::Action::Upload as i32)
        .map(|chunk| space.put_chunk(&chunk.hash, 3, now_ms))
        .collect()
}

/// The writes the server's live tree holds, each with the node that holds it.
fn live_writes(space: &SpaceDo) -> BTreeMap<i64, NodeId> {
    (space.nodes().into_iter())
        .filter(|node| node.status == NodeStatus::Live as i32)
        .filter_map(|node| Some((node.current?.content?.mtime_ns, node.node_id)))
        .collect()
}

fn uncredit(kept: &mut BTreeMap<i64, BTreeMap<NodeId, u128>>, id: &NodeId, ino: u128, write: i64) {
    let Some(nodes) = kept.get_mut(&write) else {
        return;
    };
    nodes.retain(|kept, kept_ino| kept != id && *kept_ino != ino);
    if nodes.is_empty() {
        kept.remove(&write);
    }
}

/// For each node of the server, the writes it has held, each with the digest of its bytes.
type Held = BTreeMap<NodeId, BTreeMap<i64, Vec<u8>>>;

/// True when some node of the server, live or trashed, has held the write.
fn stored(held: &Held, write: i64) -> bool {
    held.values().any(|writes| writes.contains_key(&write))
}

/// The name that `name` is the pending name of, which a swap goes through on the server (design
/// section 2.6).
fn pending_stem(name: &str) -> Option<&str> {
    let (stem, n) = name.strip_suffix(')')?.rsplit_once(" (pending")?;
    let numbered = |n: &str| {
        n.strip_prefix(' ')
            .is_some_and(|n| n.parse::<u32>().is_ok())
    };
    (n.is_empty() || numbered(n)).then_some(stem)
}

/// Whether `asked` takes `name`, or `name` is the pending name of a name it takes. A pending
/// name may only be passed through: [`World::converged`] fails on a node that keeps one.
fn given(asked: impl Fn(&str) -> bool, name: &str) -> bool {
    asked(name) || pending_stem(name).is_some_and(asked)
}

fn retained(space: &SpaceDo, node: &str) -> Vec<Vec<u8>> {
    let mut digests = Vec::new();
    let mut cursor = String::new();
    loop {
        let request = ListVersions {
            node_id: node.into(),
            cursor,
            limit: 1000,
        };
        let Ok(list) = space.list_versions(&request) else {
            return digests;
        };
        digests.extend(
            (list.versions.into_iter())
                .filter_map(|version| Some(version.content?.manifest_digest)),
        );
        if list.next_cursor.is_empty() {
            return digests;
        }
        cursor = list.next_cursor;
    }
}

fn trashed_copy(
    held: &Held,
    discarded: &BTreeSet<i64>,
    synced: Option<&Shape>,
    retained: &[Vec<u8>],
    d: usize,
    record: &EffectRecord,
    applied: &Applied,
) -> Result<Option<i64>, Failure> {
    let Some(trashed) = applied.trashed.as_ref() else {
        return Ok(None);
    };
    let write = trashed.mtime_ns;
    if stored(held, write) || discarded.contains(&write) {
        return Ok(None);
    }
    let digest = &trashed.manifest_digest;
    let unchanged = (synced.and_then(|shape| shape.content.as_ref()))
        .is_some_and(|content| content.manifest_digest == *digest);
    if !unchanged || !retained.contains(digest) {
        return Err(Failure::Broken(format!(
            "{:?} removed write {write} from the disk of device {d}, and the server never held that write",
            record.effect
        )));
    }
    Ok(Some(write))
}

/// Whether an effect that trashes a write the server never held downloads a lost write of the same
/// file that its author sent again over a user's change back to the bytes a restore rolled the
/// file back to. The
/// engine takes such a change for the content R holds, so the resend replaces it, and the bytes
/// stay only in the file's versions (design section 4.4).
fn resent_over_revert(
    exposed: &BTreeMap<i64, Exposure>,
    rolled_back: &BTreeMap<NodeId, Vec<u8>>,
    record: &EffectRecord,
    applied: &Applied,
) -> bool {
    let (Effect::WriteFile { content, .. }, Some(trashed)) = (&record.effect, &applied.trashed)
    else {
        return false;
    };
    (exposed.get(&content.mtime_ns))
        .is_some_and(|exposure| exposure.nodes.contains_key(&record.node))
        && (rolled_back.get(&record.node)).is_some_and(|digest| *digest == trashed.manifest_digest)
}

/// Applies the result of the commit in flight to R when it is the next commit, and acknowledges
/// the commit once R holds its records (section 2.7).
fn acknowledge(engine: &mut Engine) -> Result<(), Failure> {
    let next = (engine.commits.in_flight())
        .and_then(|flight| Some((flight.commit.commit_id.clone(), flight.result.clone()?)));
    if let Some((commit_id, result)) =
        next.filter(|(_, result)| result.seq == engine.cursor.seq + 1)
    {
        engine.apply(&result.changes, Some(&commit_id))?;
        engine.cursor = match result.committed_at_ms {
            0 => Base::from(result.seq),
            committed_at_ms => Base {
                seq: result.seq,
                commit_id,
                committed_at_ms,
            },
        };
    }
    engine
        .commits
        .acknowledge(&mut engine.trees, engine.cursor.seq);
    Ok(())
}

/// True when the write is in a trashed file below one of these folders.
fn trashed_under(space: &SpaceDo, write: i64, folders: &BTreeSet<&str>) -> bool {
    let nodes = space.nodes();
    let Some(file) = nodes.iter().find(|node| {
        let content = (node.current.as_ref()).and_then(|version| version.content.as_ref());
        node.status != NodeStatus::Live as i32 && content.is_some_and(|c| c.mtime_ns == write)
    }) else {
        return false;
    };
    let mut parent = file.parent_id.as_str();
    for _ in 0..nodes.len() {
        if folders.contains(parent) {
            return true;
        }
        let Some(above) = nodes.iter().find(|node| node.node_id == parent) else {
            return false;
        };
        parent = &above.parent_id;
    }
    false
}

/// The listing a device whose R the server is behind reads in place of the log: every node of the
/// server's tree and trash at its head (section 4.4).
fn listing(space: &SpaceDo) -> Message {
    listing_with(space, History::new())
}

fn listing_with(space: &SpaceDo, history: History) -> Message {
    let (commit_id, committed_at_ms) = space.token_at(space.head_seq()).unwrap_or_default();
    Message::Listing {
        nodes: space.nodes(),
        head: Base {
            seq: space.head_seq(),
            commit_id,
            committed_at_ms,
        },
        history,
    }
}

fn history_since(space: &SpaceDo, from: &Resume, trees: &Trees) -> History {
    let Ok(records) = space.resume(from) else {
        return History::new();
    };
    let mut log = History::new();
    for record in records {
        for change in &record.changes {
            if let Some(change_record::Change::Node(state)) = &change.change
                && let Ok((id, node)) = RemoteNode::from_state(state)
            {
                log.entry(id).or_default().push(node.shape);
            }
        }
    }
    first_link::since(trees.remote(), log)
}

/// The files a write a restore took from the server may end in, until the devices are quiet
/// again (design sections 2.4 and 4.4).
#[derive(Default)]
struct Exposure {
    /// For each node that held the write, on the server or on a device, the number of devices
    /// that held it there, and whether the restore rolled the node back to other content. Such a
    /// node is created again once, since the server refuses a second create of it with
    /// ID_EXISTS. Once the node holds other content, from the restore or from a later commit,
    /// it can end in one file for each of those devices: the author sends the write again over
    /// the other content, and each other device keeps its copy beside R's. Once the devices are
    /// quiet again, these are the nodes the server holds the write in, one file each.
    nodes: BTreeMap<NodeId, (usize, bool)>,
    /// The disk entries that held the write under no node of L at the restore.
    loose: usize,
    replaced: BTreeSet<NodeId>,
}

impl Exposure {
    fn rolled_back(&self, node: &str) -> bool {
        self.nodes
            .get(node)
            .is_some_and(|(_, rolled_back)| *rolled_back)
    }
}

#[derive(Clone, Debug)]
struct RecoveryCopy {
    source: NodeId,
    write: i64,
    device: usize,
    bound: u64,
    versions: Vec<u64>,
}

impl RecoveryCopy {
    fn permitted(&self) -> bool {
        self.versions.iter().any(|seq| *seq > self.bound)
    }
}

/// A copy of the server's storage, with what the server had held up to that time.
struct Backup {
    space: SpaceDo,
    held_commits: Vec<BTreeSet<u64>>,
    in_history: Held,
    spent: Vec<Spent>,
    placements: Vec<BTreeSet<Placement>>,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Placement {
    commit: String,
    node: NodeId,
    name: String,
}

fn copy_bound(held: &BTreeSet<u64>, restored_to: Option<u64>) -> u64 {
    (held.iter())
        .rev()
        .find(|seq| restored_to.is_none_or(|point| **seq <= point))
        .copied()
        .unwrap_or(0)
}

fn unspend(spent: &mut Vec<Spent>, touches: &mut [BTreeMap<u128, BTreeSet<u64>>], backup: &Backup) {
    let (lost, still): (Vec<Spent>, Vec<Spent>) = std::mem::take(spent)
        .into_iter()
        .partition(|spent| !backup.spent.contains(spent));
    *spent = still;
    for Spent {
        device, ino, at, ..
    } in lost
    {
        touches[device].entry(ino).or_default().insert(at);
    }
    for kept in &backup.spent {
        if (touches[kept.device].get_mut(&kept.ino)).is_some_and(|times| times.remove(&kept.at)) {
            spent.push(kept.clone());
        }
    }
}

fn note(seen: &mut BTreeMap<&'static str, u64>, what: &'static str) {
    *seen.entry(what).or_default() += 1;
}

pub struct World {
    space: SpaceDo,
    pub devices: Vec<Device>,
    /// How often each kind of event happened, so a test can tell that the schedules reach it.
    pub seen: BTreeMap<&'static str, u64>,
    /// Prints every step and every turn after the faults stop, with the state each leaves.
    pub trace: bool,
    /// The tag of the last write.
    writes: i64,
    /// The device whose user made each write.
    writers: BTreeMap<i64, usize>,
    /// For each device, the writes its user overwrote or deleted on its disk.
    removed: Vec<BTreeSet<i64>>,
    overwrote: BTreeMap<i64, i64>,
    /// For each device, the names its user gave to entries on its disk.
    named: Vec<BTreeSet<&'static str>>,
    /// The writes the server's live tree has held at some time.
    published: BTreeSet<i64>,
    /// What every node of the server, live or trashed, has held at some time.
    held: Held,
    in_history: Held,
    /// The first change a commit made that no user asked for.
    unasked: Option<String>,
    /// The writes that a download left on a disk in a file the engine then took for a new one. A
    /// crash kept the store from hearing of the download, and the user renamed the file before
    /// the restart (section 5.3). The server holds such a write in two files.
    copied: BTreeSet<i64>,
    /// The last copy of the server's storage.
    backup: Option<Backup>,
    earlier_backup: Option<Backup>,
    /// The writes that a restore took from the server while no disk held them. The server may
    /// stay without them, or hold them again in a file for each device that placed one from R.
    retired: BTreeSet<i64>,
    /// The writes that a restore took from the server while a device still held them, each with
    /// the files the server may hold it in.
    exposed: BTreeMap<i64, Exposure>,
    held_commits: Vec<BTreeSet<u64>>,
    restored_to: Option<u64>,
    planned_copies: Vec<BTreeMap<NodeId, RecoveryCopy>>,
    recovery_copies: BTreeMap<NodeId, RecoveryCopy>,
    restored_names: Vec<BTreeMap<NodeId, BTreeSet<String>>>,
    placements: Vec<BTreeSet<Placement>>,
    /// The files whose content a restore rolled back, also to an earlier write of the same bytes,
    /// each with the digest of the content the restored server holds, until the devices are quiet
    /// again.
    rolled_back: BTreeMap<NodeId, Vec<u8>>,
    rewound: BTreeMap<i64, BTreeSet<usize>>,
    clock: u64,
    touches: Vec<BTreeMap<u128, BTreeSet<u64>>>,
    touched_nodes: Vec<BTreeMap<NodeId, u128>>,
    merged: Vec<BTreeMap<NodeId, BTreeSet<u128>>>,
    actions: Vec<BTreeMap<u64, (u128, Change)>>,
    scanned: Vec<u64>,
    built: Vec<u64>,
    built_copies: Vec<u64>,
    spent: Vec<Spent>,
    republished: BTreeSet<i64>,
    kept: Vec<BTreeMap<i64, BTreeMap<NodeId, u128>>>,
    approved: Vec<BTreeSet<u128>>,
    moved_since_rewind: BTreeSet<i64>,
    /// The writes a person discarded in answer to a recovery question, which go to the local trash.
    discarded: BTreeSet<i64>,
    discards: Vec<BTreeSet<i64>>,
    binned_copies: BTreeSet<i64>,
    /// How many recovery questions the devices answered once the faults stopped.
    answers: usize,
}

#[derive(Clone, Copy, PartialEq)]
enum Change {
    Made,
    Edited,
    Moved,
}

impl Change {
    fn carries(self, action: Change) -> bool {
        self == Change::Made || self == action
    }

    fn settled(self, here: &Shape, there: &Shape) -> bool {
        match self {
            Change::Made => here.same(there),
            Change::Moved => here.same_position(there),
            Change::Edited => match (&here.content, &there.content) {
                (Some(here), Some(there)) => same_content(here, there),
                (None, None) => true,
                _ => false,
            },
        }
    }
}

#[derive(Clone, PartialEq)]
struct Spent {
    seq: u64,
    device: usize,
    ino: u128,
    at: u64,
}

impl World {
    /// Devices with empty disks, connected to an empty space.
    pub fn new(devices: usize) -> Self {
        let space = SpaceDo::new(5, NOW);
        let devices = (0..devices)
            .map(|d| {
                let engine = Engine::new(space.space_id(), space.history_id(), 100 + d as u64);
                Device {
                    session: Session {
                        device_id: format!("device-{d}"),
                        user_id: "user".into(),
                        device_name: format!("device-{d}"),
                        role: Role::Owner,
                        max_format: 1,
                    },
                    disk: Disk::default(),
                    store: engine.clone(),
                    engine,
                    down: false,
                    link: Some(Link::default()),
                    recovery: None,
                    recovery_from: Resume::default(),
                    store_copy: None,
                    rolled_back: false,
                    restored_ahead: false,
                    left: false,
                }
            })
            .collect::<Vec<_>>();
        let count = devices.len();
        Self {
            space,
            removed: vec![BTreeSet::new(); count],
            overwrote: BTreeMap::new(),
            named: vec![BTreeSet::new(); devices.len()],
            restored_names: vec![BTreeMap::new(); devices.len()],
            placements: vec![BTreeSet::new(); devices.len()],
            kept: vec![BTreeMap::new(); devices.len()],
            approved: vec![BTreeSet::new(); devices.len()],
            devices,
            seen: BTreeMap::new(),
            trace: false,
            writes: 0,
            writers: BTreeMap::new(),
            published: BTreeSet::new(),
            held: Held::new(),
            in_history: Held::new(),
            unasked: None,
            copied: BTreeSet::new(),
            backup: None,
            earlier_backup: None,
            retired: BTreeSet::new(),
            exposed: BTreeMap::new(),
            held_commits: vec![BTreeSet::new(); count],
            restored_to: None,
            planned_copies: vec![BTreeMap::new(); count],
            recovery_copies: BTreeMap::new(),
            rolled_back: BTreeMap::new(),
            rewound: BTreeMap::new(),
            clock: 0,
            touches: vec![BTreeMap::new(); count],
            touched_nodes: vec![BTreeMap::new(); count],
            merged: vec![BTreeMap::new(); count],
            actions: vec![BTreeMap::new(); count],
            scanned: vec![0; count],
            built: vec![0; count],
            built_copies: vec![0; count],
            spent: Vec::new(),
            republished: BTreeSet::new(),
            moved_since_rewind: BTreeSet::new(),
            discarded: BTreeSet::new(),
            discards: vec![BTreeSet::new(); count],
            binned_copies: BTreeSet::new(),
            answers: 0,
        }
    }

    pub fn space(&self) -> &SpaceDo {
        &self.space
    }

    /// Forgets the server's log up to its head. A device that resumes below it is refused.
    pub fn truncate_log(&mut self) {
        let head = self.space.head_seq();
        self.space.truncate_log(head);
    }

    /// Runs one phase: a schedule of steps, after which the faults stop, the devices become
    /// quiet and the end state is checked, so the next phase starts from trees that every device
    /// holds. A failure names the phase and the step that broke a property.
    pub fn phase(&mut self, phase: usize, schedule: &[(usize, Step)]) -> Result<(), Failure> {
        self.run(phase, schedule)?;
        let quiet = self.quiesce();
        if self.trace {
            eprintln!(
                "after phase {phase}: {quiet:?}
{}",
                self.dump()
            );
        }
        quiet.map_err(|failure| failure.at(format!("after phase {phase}")))?;
        self.recovered();
        Ok(())
    }

    /// Runs the steps of a phase without letting the devices become quiet, so that a test can
    /// change a disk while the devices still recover from a restore.
    pub fn run(&mut self, phase: usize, schedule: &[(usize, Step)]) -> Result<(), Failure> {
        for (index, (device, step)) in schedule.iter().enumerate() {
            let device = device % self.devices.len();
            let done = self.step(device, step);
            if self.trace {
                eprintln!(
                    "phase {phase}, step {index}: {step:?} on device {device}: {done:?}
{}",
                    self.dump()
                );
            }
            let at = format!("phase {phase}, step {index} ({step:?} on device {device})");
            done.map_err(|failure| failure.at(at))?;
        }
        Ok(())
    }

    /// Ends what a restore allowed once the devices are quiet again: no later commit sends a name
    /// the restore took from the server, and a write the restore took stays in the files the
    /// server holds it in now.
    fn recovered(&mut self) {
        for device in &mut self.devices {
            if !device.recovering() {
                device.rolled_back = false;
                device.restored_ahead = false;
            }
        }
        for names in &mut self.restored_names {
            names.clear();
        }
        self.rolled_back.clear();
        let mut files: BTreeMap<i64, BTreeMap<NodeId, (usize, bool)>> = BTreeMap::new();
        for node in self.space.nodes() {
            if node.status != NodeStatus::Live as i32 {
                continue;
            }
            if let Some(content) = node.current.and_then(|current| current.content) {
                (files.entry(content.mtime_ns).or_default()).insert(node.node_id, (1, false));
            }
        }
        for (write, exposure) in &mut self.exposed {
            *exposure = Exposure {
                nodes: files.remove(write).unwrap_or_default(),
                ..Exposure::default()
            };
        }
    }

    fn step(&mut self, d: usize, step: &Step) -> Result<(), Failure> {
        self.clock += 1;
        match step {
            Step::Backup => {
                self.earlier_backup = self.backup.replace(Backup {
                    space: self.space.clone(),
                    held_commits: self.held_commits.clone(),
                    in_history: self.in_history.clone(),
                    spent: self.spent.clone(),
                    placements: self.placements.clone(),
                });
            }
            Step::Restore => self.restore(false),
            Step::RestoreEarlier => self.restore(true),
            Step::Rewind => self.rewind(),
            _ if self.devices[d].left => {}
            Step::Leave => {
                let device = &mut self.devices[d];
                device.left = true;
                device.link = None;
            }
            Step::User(action) => self.act(d, action, Act::of(action.kind)),
            Step::Copy(action) => self.act(d, action, Act::Copy),
            Step::Save { target, keeps_mark } => {
                let action = Action {
                    kind: 0,
                    target: *target,
                    parent: 0,
                    name: 0,
                    text: 0,
                };
                let keeps_mark = *keeps_mark;
                self.act(d, &action, Act::Save { keeps_mark });
            }
            Step::Answer(choice) => {
                self.answer(d, *choice)?;
            }
            Step::CopyStore => {
                let device = &mut self.devices[d];
                device.store_copy = Some(device.store.clone());
                self.built_copies[d] = self.built[d];
            }
            Step::RollBack => {
                let device = &mut self.devices[d];
                if let Some(copy) = device.store_copy.clone() {
                    note(&mut self.seen, "store rolled back");
                    if matches!(
                        device.engine.commits.halted(),
                        Some(commit::Halt::Recovery(_))
                    ) {
                        note(&mut self.seen, "store rolled back during a recovery");
                    }
                    let ids = device.engine.ids.clone();
                    device.store = Engine { ids, ..copy };
                    self.built[d] = self.built_copies[d];
                    device.rolled_back = true;
                    device.down = true;
                    device.link = None;
                }
            }
            _ if self.devices[d].down => self.restart(d)?,
            Step::Scan => {
                self.scan(d)?;
            }
            Step::Effect(outcome) => {
                if self.effect(d, *outcome)? && *outcome != Outcome::Run {
                    self.effect(d, *outcome)?;
                }
            }
            Step::Commit(reply) => {
                if self.commit(d, *reply)? && *reply != Reply::Served {
                    self.commit(d, *reply)?;
                }
            }
            Step::Connect => self.connect(d)?,
            Step::Deliver(index) => {
                self.deliver(d, *index)?;
            }
            Step::Work(pick) => {
                for turn in 0..4 {
                    let worked = match (pick + turn) % 4 {
                        0 => self.deliver(d, pick / 4)?,
                        1 => self.effect(d, Outcome::Run)?,
                        2 => self.commit(d, Reply::Served)?,
                        _ => self.scan(d)?,
                    };
                    if worked {
                        break;
                    }
                }
            }
            Step::Duplicate(index) => {
                if let Some(link) = self.devices[d]
                    .link
                    .as_mut()
                    .filter(|l| !l.inbox.is_empty())
                {
                    let message = link.inbox[index % link.inbox.len()].clone();
                    link.inbox.push(message);
                }
            }
            Step::Drop => self.devices[d].link = None,
            Step::Crash => {
                if matches!(
                    self.devices[d].engine.commits.halted(),
                    Some(commit::Halt::Recovery(_))
                ) {
                    note(&mut self.seen, "crash during a recovery");
                }
                self.devices[d].down = true;
                self.devices[d].link = None;
            }
        }
        self.revoke_asked_again(d);
        let trees = &self.devices[d].engine.trees;
        (trees.check()).map_err(|(tree, error)| {
            Failure::Broken(format!("{tree} is not a valid tree: {error:?}"))
        })
    }

    fn act(&mut self, d: usize, action: &Action, act: Act) {
        let disk = &mut self.devices[d].disk;
        let visible = disk.visible();
        let dirs: Vec<Place> = std::iter::once(Place::Root)
            .chain(
                (visible.iter())
                    .filter(|ino| disk.entries()[ino].content.is_none())
                    .map(|ino| Place::In(*ino)),
            )
            .collect();
        let place = dirs[action.parent % dirs.len()].clone();
        let name = NAMES.get(action.name).copied().unwrap_or(PENDING_NAME);
        let target = (!visible.is_empty()).then(|| visible[action.target % visible.len()]);
        let chain = |mut folder: Place| {
            let mut inos = Vec::new();
            while let Place::In(ino) = folder {
                inos.push(ino);
                folder = disk.entries()[&ino].place.clone();
            }
            inos
        };
        let above = chain(place.clone());
        let itself: Vec<u128> = (target.into_iter())
            .flat_map(|ino| std::iter::once(ino).chain(chain(disk.entries()[&ino].place.clone())))
            .collect();
        let write = self.writes + 1;
        let mut touched: Vec<u128> = Vec::new();
        match (act, target) {
            (Act::MakeDir, _) => {
                if let Some(new) = disk.create(place, name, None) {
                    self.named[d].insert(name);
                    touched = std::iter::once(new).chain(above).collect();
                }
            }
            (Act::MakeFile, _) => {
                let content = content(d as u8, action.text % TEXTS, write);
                if let Some(new) = disk.create(place, name, Some(content)) {
                    self.named[d].insert(name);
                    self.writes = write;
                    self.writers.insert(write, d);
                    touched = std::iter::once(new).chain(above).collect();
                }
            }
            (Act::Overwrite, Some(ino)) => {
                let entry = &disk.entries()[&ino];
                let fresh = (0..TEXTS)
                    .map(|i| content(d as u8, (action.text % TEXTS + i) % TEXTS, write))
                    .find(|content| !entry.held.contains(&content.manifest_digest));
                if let (Some(_), Some(fresh)) = (&entry.content, fresh)
                    && let Some(old) = disk.overwrite(ino, fresh)
                {
                    self.removed[d].insert(old.mtime_ns);
                    self.overwrote.insert(write, old.mtime_ns);
                    self.writes = write;
                    self.writers.insert(write, d);
                    touched = itself;
                }
            }
            (Act::Rename, Some(ino)) => {
                if disk.rename(ino, place, name).is_ok() {
                    self.named[d].insert(name);
                    let moved = disk.entries()[&ino].content.as_ref().map(|c| c.mtime_ns);
                    (self.republished).extend(moved.filter(|write| self.discarded.contains(write)));
                    (self.moved_since_rewind)
                        .extend(moved.filter(|write| self.rewound.contains_key(write)));
                    touched = itself.into_iter().chain(above).collect();
                }
            }
            (Act::Delete, Some(ino)) => {
                let deleted = disk.delete(ino);
                self.removed[d].extend(deleted.iter().map(|content| content.mtime_ns));
            }
            (Act::Copy, Some(ino)) => {
                if let Some(new) = disk.copy(ino, place, name) {
                    self.named[d].insert(name);
                    touched = std::iter::once(new).chain(above).collect();
                }
            }
            (Act::Save { keeps_mark }, Some(ino)) => {
                if let Some(new) = disk.save(ino, keeps_mark) {
                    touched = std::iter::once(new)
                        .chain(itself.into_iter().skip(1))
                        .collect();
                }
            }
            _ => {}
        }
        let local = self.devices[d].engine.trees.local();
        if let Some(own) = touched.first() {
            let change = match act {
                Act::Overwrite => Change::Edited,
                Act::Rename => Change::Moved,
                _ => Change::Made,
            };
            self.actions[d].insert(self.clock, (*own, change));
        }
        for ino in touched {
            self.touches[d].entry(ino).or_default().insert(self.clock);
            if let Some((node, _)) = local.iter().find(|(_, node)| node.identity.ino == ino) {
                self.touched_nodes[d].insert(node.clone(), ino);
            }
        }
    }

    fn rewind(&mut self) {
        let Some(backup) = &self.backup else {
            return;
        };
        note(&mut self.seen, "server rewound");
        unspend(&mut self.spent, &mut self.touches, backup);
        let backed_up: BTreeSet<i64> = (backup.space.nodes().into_iter())
            .filter_map(|node| Some(node.current?.content?.mtime_ns))
            .collect();
        let still_held = |write: i64| {
            (self.devices.iter())
                .any(|device| device.disk.holds(write) || device.disk.binned(write))
                || self.removed.iter().any(|removed| removed.contains(&write))
        };
        let taken: Vec<i64> = (self.writers.keys())
            .filter(|write| {
                stored(&self.in_history, **write) && !stored(&backup.in_history, **write)
                    || self.exposed.contains_key(write)
                        && !backed_up.contains(write)
                        && still_held(**write)
            })
            .copied()
            .collect();
        self.space.rewind(&backup.space, self.server_now());
        self.restored_to = None;
        self.held_commits = backup.held_commits.clone();
        self.placements.clone_from(&backup.placements);
        self.in_history = backup.in_history.clone();
        self.rewound
            .retain(|write, _| !stored(&backup.in_history, *write));
        for write in taken {
            let holders: BTreeSet<usize> = (self.devices.iter().enumerate())
                .filter(|(_, device)| device.disk.holds(write))
                .map(|(d, _)| d)
                .collect();
            if !holders.is_empty() {
                note(&mut self.seen, "rewind takes a write a disk holds");
            }
            if holders.len() == 1 {
                note(&mut self.seen, "rewind takes a write only one disk holds");
            }
            self.exposed.remove(&write);
            self.republished.remove(&write);
            for kept in &mut self.kept {
                kept.remove(&write);
            }
            self.moved_since_rewind.remove(&write);
            self.rewound.entry(write).or_default().extend(holders);
        }
        for device in &mut self.devices {
            device.link = None;
        }
    }

    fn recover(&mut self, d: usize, trigger: RecoveryTrigger, why: String) -> Result<(), Failure> {
        let device = &mut self.devices[d];
        let ahead = device.restored_ahead && trigger == RecoveryTrigger::StoreRolledBack;
        if device.engine.history == self.space.history_id() && !device.rolled_back && !ahead {
            return Err(Failure::Broken(format!(
                "recovery started with no rewind, no rolled-back store and no restore of commits it lost: {why}"
            )));
        }
        note(
            &mut self.seen,
            match device.engine.history == self.space.history_id() {
                true if device.rolled_back => "recovery starts after a store rollback",
                true => "recovery starts after a restore brings back commits the device saw lost",
                false => "recovery starts after a rewind",
            },
        );
        let engine = &self.devices[d].engine;
        let from = Resume {
            history_id: engine.history.clone(),
            since_seq: engine.cursor.seq,
            since_commit_id: engine.cursor.commit_id.clone(),
            since_committed_at_ms: engine.cursor.committed_at_ms,
        };
        let since = history_since(&self.space, &from, &engine.trees);
        let history = self.space.history_id().to_owned();
        let device = &mut self.devices[d];
        device.recovery = Some(why);
        device.recovery_from = from;
        device
            .engine
            .commits
            .enter_recovery(RecoveryCause::of(trigger));
        device.engine.history = history;
        device.engine.behind = true;
        let listed = listing_with(&self.space, since);
        if let Some(link) = &mut device.link {
            link.inbox.push(listed);
        }
        device.persist()
    }

    /// Answers the first open recovery question of device `d` with the answer `choice` picks among
    /// those it offers. The writes a Discard sends to the local trash are no longer required on
    /// the server, and a Publish or a Delete replaces what the server holds as the person chose,
    /// as if they had overwritten or deleted it. Publishing what a store that lost track of it
    /// finds on disk may copy a write the server holds elsewhere, as the person chose. False when
    /// no recovery question waits for an answer.
    fn answer(&mut self, d: usize, choice: usize) -> Result<bool, Failure> {
        let device = &mut self.devices[d];
        let down = device.down;
        let engine = match down {
            true => &mut device.store,
            false => &mut device.engine,
        };
        let open = (engine.commits.questions().iter()).find(|question| {
            matches!(question.kind, QuestionKind::Recovery { .. }) && question.answer.is_none()
        });
        let Some(question) = open.cloned() else {
            return Ok(false);
        };
        let answers = question.kind.answers();
        let answer = answers[choice % answers.len()];
        note(
            &mut self.seen,
            match answer {
                Answer::Publish => "recovery answer publishes",
                Answer::Keep => "recovery answer keeps",
                Answer::Discard => "recovery answer discards",
                Answer::Delete => "recovery answer deletes",
                Answer::Restore => "recovery answer restores",
                Answer::Repeat => "recovery answer repeats a restore",
                _ => "recovery answer cancels a restore",
            },
        );
        let trees = &engine.trees;
        let content = |shape: Option<&Shape>| Some(shape?.content.as_ref()?.mtime_ns);
        let held: Vec<(NodeId, i64, u128)> = (question.holds())
            .filter_map(|id| {
                let node = trees.local().get(id)?;
                Some((id.clone(), content(Some(&node.shape))?, node.identity.ino))
            })
            .collect();
        let local: Vec<i64> = held.iter().map(|(_, write, _)| *write).collect();
        let inos: Vec<u128> = (question.holds())
            .filter_map(|id| Some(trees.local().get(id)?.identity.ino))
            .collect();
        let remote: Vec<i64> = (question.holds())
            .filter_map(|id| content(trees.remote().live(id)))
            .collect();
        if matches!(answer, Answer::Publish | Answer::Keep) {
            self.approved[d].extend(&inos);
            for (id, write, ino) in &held {
                self.kept[d]
                    .entry(*write)
                    .or_default()
                    .insert(id.clone(), *ino);
            }
            for held in question.holds() {
                let mut at = held;
                while let Some(node) = trees.local().get(at) {
                    let name = node.shape.name.as_str();
                    self.named[d].extend(NAMES.iter().find(|known| **known == name));
                    (self.restored_names[d].entry(at.clone()).or_default()).insert(name.to_owned());
                    at = &node.shape.parent;
                }
            }
        }
        match answer {
            Answer::Discard => {
                let moved = local.iter().filter(|w| self.moved_since_rewind.contains(w));
                self.republished.extend(moved);
                for (id, write, ino) in &held {
                    uncredit(&mut self.kept[d], id, *ino, *write);
                }
                for ino in &inos {
                    self.approved[d].remove(ino);
                }
                self.discards[d].extend(&local);
                self.discarded.extend(local);
            }
            Answer::Publish => {
                self.copied.extend(local);
                self.removed[d].extend(remote);
            }
            Answer::Delete => self.removed[d].extend(remote),
            _ => {}
        }
        assert!(engine.commits.decide(&question.id, answer));
        if !down {
            device.persist()?;
        }
        Ok(true)
    }

    fn revoke_asked_again(&mut self, d: usize) {
        let device = &self.devices[d];
        let engine = match device.down {
            true => &device.store,
            false => &device.engine,
        };
        let open = (engine.commits.questions().iter()).filter(|question| {
            matches!(question.kind, QuestionKind::Recovery { .. }) && question.answer.is_none()
        });
        let asked: Vec<(NodeId, Option<i64>, u128)> = (open.flat_map(|question| question.holds()))
            .filter_map(|id| {
                let node = engine.trees.local().get(id)?;
                let write = node.shape.content.as_ref().map(|content| content.mtime_ns);
                Some((id.clone(), write, node.identity.ino))
            })
            .collect();
        for (id, write, ino) in asked {
            self.approved[d].remove(&ino);
            if let Some(write) = write {
                uncredit(&mut self.kept[d], &id, ino, write);
            }
        }
    }

    /// Restores the server from the last copy. A write is lost with the restore when the server
    /// held it only after the copy, or when a file of the server held it before the restore and
    /// no longer holds it after: it is exposed, and retired as well when no disk holds it.
    fn restore(&mut self, earlier: bool) {
        let backup = if earlier {
            &self.earlier_backup
        } else {
            &self.backup
        };
        let Some(backup) = backup else {
            return;
        };
        if backup.space.head_seq() < self.space.head_seq() {
            note(&mut self.seen, "server restored, commits lost");
        }
        unspend(&mut self.spent, &mut self.touches, backup);
        type Shape = (String, String, i32, Option<Vec<u8>>);
        let shapes = |space: &SpaceDo| -> BTreeMap<NodeId, Shape> {
            (space.nodes().into_iter())
                .map(|node| {
                    let digest = node.current.and_then(|current| current.content);
                    let digest = digest.map(|content| content.manifest_digest);
                    (
                        node.node_id,
                        (node.parent_id, node.name, node.status, digest),
                    )
                })
                .collect()
        };
        let writes = |space: &SpaceDo| -> BTreeSet<(NodeId, i64)> {
            (space.nodes().into_iter())
                .filter_map(|node| Some((node.node_id, node.current?.content?.mtime_ns)))
                .collect()
        };
        let before = shapes(&self.space);
        let held_before = writes(&self.space);
        let lost_history: BTreeSet<i64> = (self.writers.keys())
            .filter(|write| {
                stored(&self.in_history, **write) && !stored(&backup.in_history, **write)
            })
            .copied()
            .collect();
        for (device, (held, restored)) in
            (self.devices.iter_mut()).zip(self.held_commits.iter().zip(&backup.held_commits))
        {
            device.restored_ahead |= !restored.is_subset(held);
        }
        self.restored_to = Some(backup.space.head_seq());
        self.space.restore(&backup.space);
        self.held_commits = backup.held_commits.clone();
        self.in_history = backup.in_history.clone();
        self.rewound
            .retain(|write, _| !stored(&backup.in_history, *write));
        let after = shapes(&self.space);
        let held_after = writes(&self.space);
        for (id, (.., digest)) in &after {
            let rewritten =
                (held_after.iter()).any(|held| held.0 == *id && !held_before.contains(held));
            if let Some(digest) = digest
                && (rewritten
                    || before
                        .get(id)
                        .is_none_or(|(.., held)| held.as_ref() != Some(digest)))
            {
                self.rolled_back.insert(id.clone(), digest.clone());
            }
        }
        let changed: BTreeSet<&NodeId> = (before.keys().chain(after.keys()))
            .filter(|id| before.get(*id) != after.get(*id))
            .collect();
        for (d, names) in self.restored_names.iter_mut().enumerate() {
            for placement in self.placements[d].difference(&backup.placements[d]) {
                names
                    .entry(placement.node.clone())
                    .or_default()
                    .insert(placement.name.clone());
            }
        }
        self.placements.clone_from(&backup.placements);
        for (device, names) in self.devices.iter_mut().zip(&mut self.restored_names) {
            device.link = None;
            for id in &changed {
                if let Some((_, name, ..)) = before.get(*id) {
                    names.entry((*id).clone()).or_default().insert(name.clone());
                }
            }
            for (id, node) in device.engine.trees.local().iter() {
                if changed.contains(id) {
                    names
                        .entry(id.clone())
                        .or_default()
                        .insert(node.shape.name.clone());
                }
            }
        }
        for write in self.writers.keys() {
            let taken = (held_before.difference(&held_after)).any(|(_, held)| held == write);
            if !taken && !lost_history.contains(write) {
                continue;
            }
            if self.devices.iter().any(|device| device.disk.holds(*write)) {
                self.retired.remove(write);
            } else if taken || !self.exposed.contains_key(write) {
                self.retired.insert(*write);
            }
            let holds =
                |content: &Option<Content>| content.as_ref().is_some_and(|c| c.mtime_ns == *write);
            let mut holders: BTreeMap<&NodeId, usize> = (held_before.iter())
                .filter(|(_, held)| held == write)
                .map(|(id, _)| (id, 0))
                .collect();
            let mut unknown = 0;
            for device in &self.devices {
                let mut nodes: BTreeSet<&NodeId> = BTreeSet::new();
                let trees = &device.engine.trees;
                let local = trees
                    .local()
                    .iter()
                    .filter(|(_, node)| holds(&node.shape.content));
                let remote = trees
                    .remote()
                    .iter()
                    .filter(|(_, node)| holds(&node.shape.content));
                nodes.extend(local.map(|(id, _)| id).chain(remote.map(|(id, _)| id)));
                for (ino, entry) in device.disk.entries() {
                    if !holds(&entry.content) {
                        continue;
                    }
                    match (trees.local().iter()).find(|(_, node)| node.identity.ino == *ino) {
                        Some((id, _)) => {
                            nodes.insert(id);
                        }
                        None => unknown += 1,
                    }
                }
                for id in nodes {
                    *holders.entry(id).or_default() += 1;
                }
            }
            let kept = |id: &NodeId| {
                held_after.contains(&(id.clone(), *write)) && after[id].2 == NodeStatus::Live as i32
            };
            let exposure = self.exposed.entry(*write).or_default();
            exposure.loose = unknown.max(exposure.loose);
            for (id, devices) in holders {
                let (held, rolled_back) = exposure.nodes.entry(id.clone()).or_default();
                *held = devices.max(*held);
                *rolled_back |= after.contains_key(id) && !kept(id);
            }
        }
    }

    /// A full rescan. True when L changed.
    fn scan(&mut self, d: usize) -> Result<bool, Failure> {
        let device = &mut self.devices[d];
        if device.rescan() > 0 {
            note(
                &mut self.seen,
                "rescan lets a problem node be planned again",
            );
        }
        self.scanned[d] = self.clock;
        let changed = device.engine != device.store;
        device.persist()?;
        Ok(changed)
    }

    /// Plans the next effect, or ends the one in flight. With `Outcome::Fail` or
    /// `Outcome::Crash` a step does both. True when there was an effect to plan or to end.
    ///
    /// As in the driver, the disk gets an effect only while L still takes its outcome
    /// (`EffectRecord::applies`). An effect that L no longer takes fails, and is planned again once
    /// L has changed. The outcome goes to the store in its own transaction, before the disk is
    /// observed again. As in the driver, no effect is planned while a commit is in flight, unless
    /// committing has stopped.
    fn effect(&mut self, d: usize, outcome: Outcome) -> Result<bool, Failure> {
        let device = &mut self.devices[d];
        let batch = device.engine.effects.in_flight().to_vec();
        if batch.is_empty() {
            let commits = &device.engine.commits;
            let listing = device.link.as_ref().is_some_and(|link| link.listing);
            if device.engine.behind
                || listing
                || commits.in_flight().is_some() && commits.halted().is_none()
            {
                return Ok(false);
            }
            let Engine {
                trees,
                effects,
                ids,
                commits,
                ..
            } = &mut device.engine;
            trees.settle();
            let size = match outcome {
                Outcome::Run => 1 + self.clock % BATCH,
                Outcome::Fail | Outcome::Crash(_) => BATCH,
            };
            let limit = BatchLimit {
                effects: size as usize,
                bytes: u64::MAX,
            };
            let planned =
                !(effects.plan(trees, commits.in_flight(), ids, NOW as u64, limit)).is_empty();
            device.persist()?;
            return Ok(planned);
        }
        let unlucky = match outcome {
            Outcome::Crash(_) => batch.len() - 1,
            _ => (self.clock.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) as usize % batch.len(),
        };
        for (index, record) in batch.iter().enumerate() {
            if index != unlucky || outcome != Outcome::Fail {
                self.devices[d].disk.stage(record);
            }
        }
        let mut ends = Vec::new();
        for (index, record) in batch.iter().enumerate() {
            let outcome = if index == unlucky {
                outcome
            } else {
                Outcome::Run
            };
            let device = &mut self.devices[d];
            let applies = record.applies(&device.engine.trees, device.engine.commits.in_flight());
            let until = match outcome {
                Outcome::Run if applies => Some(Cut::Done),
                Outcome::Crash(cut) if applies => Some(cut),
                _ => None,
            };
            let applied = device.disk.apply(record, until);
            if let Some(applied) = applied.as_ref().filter(|applied| {
                !resent_over_revert(&self.exposed, &self.rolled_back, record, applied)
            }) {
                let synced = device.engine.trees.synced().live(&record.node);
                let retained = retained(&self.space, &record.node);
                let copy = trashed_copy(
                    &self.held,
                    &self.discarded,
                    synced,
                    &retained,
                    d,
                    record,
                    applied,
                )?;
                self.binned_copies.extend(copy);
            }
            let applied = applied.map(|applied| applied.outcome);
            if let Outcome::Crash(cut) = outcome {
                note(
                    &mut self.seen,
                    match applied {
                        _ if !device.disk.quarantine.is_empty() => {
                            "crash with a displaced entry in quarantine"
                        }
                        _ if !device.disk.staged.is_empty() => "crash with a download staged",
                        Some(Err(_)) => "crash after the disk refused an effect",
                        _ if applied.is_none() || cut == Cut::Quarantined => {
                            "crash before an effect reached the disk"
                        }
                        _ => "crash after an effect reached the disk",
                    },
                );
                if index > 0 {
                    note(
                        &mut self.seen,
                        "crash after part of a batch reached the disk",
                    );
                }
                device.down = true;
                device.link = None;
                return Ok(false);
            }
            ends.push((record, applied, applies, outcome));
        }
        if batch.len() > 1 {
            note(&mut self.seen, "a batch of effects ends in one transaction");
        }
        let device = &mut self.devices[d];
        for (record, applied, applies, outcome) in ends {
            let Engine {
                trees,
                effects,
                commits,
                ids,
                cursor,
                ..
            } = &mut device.engine;
            let reported = match applied {
                Some(Ok(identity)) => {
                    (effects.done(trees, commits.in_flight(), &record.effect_id, identity)).map(
                        |removal| {
                            if let Some(removal) = removal {
                                commits.count_remote(removal, trees, cursor.seq, ids, NOW as u64);
                            }
                        },
                    )
                }
                Some(Err(Refused::Overtaken)) => effects.overtaken(trees, &record.effect_id),
                _ => effects.failed(trees, &record.effect_id).map(|()| {
                    if !applies {
                        effects.unblock(&record.node);
                    }
                }),
            };
            note(
                &mut self.seen,
                match (applied, outcome) {
                    (Some(Ok(_)), _) if record.after.is_none() => "effect removed an entry",
                    (Some(Ok(_)), _)
                        if trees
                            .local()
                            .get(&record.node)
                            .is_some_and(|node| node.parked) =>
                    {
                        "effect parked a node"
                    }
                    (Some(Ok(_)), _) => "effect done",
                    (_, Outcome::Fail) => "effect failed before the disk",
                    (None, _) => "effect not applied, L no longer takes it",
                    _ => "effect refused by the disk",
                },
            );
            reported.map_err(|error| {
                Failure::Broken(format!("the effect's outcome was refused: {error:?}"))
            })?;
            let placed =
                (device.engine.trees.local().get(&record.node)).filter(|node| !node.parked);
            if matches!(applied, Some(Ok(_)))
                && let Some(node) = placed
                && let Some(entry) = device.disk.entries().get(&node.identity.ino)
                && entry.name != node.shape.name
            {
                return Err(Failure::Broken(format!(
                    "an effect left node {} named {:?} in L while the disk names it {:?}",
                    record.node, node.shape.name, entry.name
                )));
            }
        }
        device.persist()?;
        device.engine.scan(&device.disk);
        self.scanned[d] = self.clock;
        device.persist()?;
        Ok(true)
    }

    /// Restarts a crashed device: the engine comes back from the store, the effect in flight is
    /// ended from what the disk shows, and the disk is scanned again (sections 2.8 and 5.3).
    ///
    /// As in the driver, an effect that recovery reports as done fails when L refutes it
    /// (`Effects::confirms`), and so does an effect whose outcome L no longer takes
    /// (`EffectRecord::applies`).
    fn restart(&mut self, d: usize) -> Result<(), Failure> {
        let device = &mut self.devices[d];
        device.down = false;
        device.engine = device.store.reload()?;
        let effects = &device.engine.effects;
        let (marker, cleared) = (device.disk.marker(), effects.cleared());
        if marker > cleared + effects.in_flight().len() as u64 {
            let trees = &device.engine.trees;
            device.disk.bring_back_all(|parked| {
                (trees
                    .local()
                    .get(parked)
                    .map(|node| node.shape.name.clone()))
                .or_else(|| trees.synced().live(parked).map(|shape| shape.name.clone()))
                .or_else(|| {
                    trees
                        .remote()
                        .get(parked)
                        .map(|node| node.shape.name.clone())
                })
                .unwrap_or_else(|| parked.to_owned())
            });
            device.engine.effects.catch_up(marker);
            self.recover(
                d,
                RecoveryTrigger::StoreOlderThanDisk,
                format!("the root marker counts {marker} effects, more than the store"),
            )?;
        }
        if marker < cleared {
            return Err(Failure::Broken(format!(
                "the store counts {cleared} effects, more than the root marker's {marker}"
            )));
        }
        let device = &mut self.devices[d];
        let batch = device.engine.effects.in_flight().to_vec();
        let mut recovered = Vec::with_capacity(batch.len());
        for record in &batch {
            let quarantined = device.disk.quarantine.contains_key(&record.effect_id);
            let applied = device.disk.recover(record);
            if !resent_over_revert(&self.exposed, &self.rolled_back, record, &applied) {
                let synced = device.engine.trees.synced().live(&record.node);
                let retained = retained(&self.space, &record.node);
                let copy = trashed_copy(
                    &self.held,
                    &self.discarded,
                    synced,
                    &retained,
                    d,
                    record,
                    &applied,
                )?;
                self.binned_copies.extend(copy);
            }
            let engine = &device.engine;
            let refuted = matches!(
                &applied.outcome,
                Ok(identity) if !engine.effects.confirms(&engine.trees, &record.effect_id, identity.as_ref())
            );
            recovered.push((quarantined, applied, refuted));
        }
        for (record, (quarantined, applied, refuted)) in batch.iter().zip(recovered) {
            let Engine {
                trees,
                effects,
                commits,
                ids,
                cursor,
                ..
            } = &mut device.engine;
            let download = match &record.effect {
                Effect::WriteFile { content, .. } => Some(content),
                _ => None,
            };
            let applies = record.applies(trees, commits.in_flight());
            let reported = match applied.outcome {
                Ok(identity) if applies && !refuted => {
                    // A file that the user made with the bytes of the download counts as the
                    // download. The server holds that write from then on.
                    let taken = identity
                        .as_ref()
                        .zip(download)
                        .and_then(|(identity, content)| {
                            let found = device.disk.entries()[&identity.ino].content.as_ref()?;
                            (found.mtime_ns != content.mtime_ns).then_some(found)
                        });
                    if let Some(found) = taken {
                        note(
                            &mut self.seen,
                            "restart takes a file of the user for the download",
                        );
                        for held in [&mut self.held, &mut self.in_history] {
                            let writes = held.entry(record.node.clone()).or_default();
                            writes.insert(found.mtime_ns, found.manifest_digest.clone());
                        }
                        self.published.insert(found.mtime_ns);
                    }
                    note(
                        &mut self.seen,
                        match quarantined {
                            true => "restart ends an effect that left an entry in quarantine",
                            false => "restart finds the effect in flight done",
                        },
                    );
                    (effects.done(trees, commits.in_flight(), &record.effect_id, identity)).map(
                        |removal| {
                            if let Some(removal) = removal {
                                commits.count_remote(removal, trees, cursor.seq, ids, NOW as u64);
                            }
                        },
                    )
                }
                Err(Refused::Overtaken) => {
                    note(&mut self.seen, "restart finds a write its user overtook");
                    if let Some(content) = download.filter(|c| device.disk.holds(c.mtime_ns)) {
                        self.copied.insert(content.mtime_ns);
                    }
                    effects.overtaken(trees, &record.effect_id)
                }
                outcome => {
                    note(
                        &mut self.seen,
                        match outcome {
                            Ok(_) if refuted => "restart finds a done effect that L refutes",
                            Ok(_) => "restart finds an effect that L no longer takes",
                            Err(Refused::Interrupted) => {
                                "restart finds the effect in flight not done"
                            }
                            Err(_) => "restart finds the effect in flight refused by the disk",
                        },
                    );
                    if let Some(content) = download.filter(|c| device.disk.holds(c.mtime_ns)) {
                        self.copied.insert(content.mtime_ns);
                    }
                    // The driver plans an interrupted effect again, and one that L no longer
                    // takes once L has changed. An effect that failed for another reason waits
                    // until the disk is observed.
                    effects.failed(trees, &record.effect_id).map(|()| {
                        if !applies || matches!(outcome, Err(Refused::Interrupted)) {
                            effects.unblock(&record.node);
                        }
                    })
                }
            };
            reported.map_err(|error| {
                Failure::Broken(format!("the effect's outcome was refused: {error:?}"))
            })?;
        }
        if batch.len() > 1 {
            note(&mut self.seen, "restart ends a batch of effects");
        }
        if !batch.is_empty() {
            device.persist()?;
        }
        if device.engine.commits.in_flight().is_some() {
            note(&mut self.seen, "restart with a commit in flight");
        }
        if device.rescan() > 0 {
            note(
                &mut self.seen,
                "rescan lets a problem node be planned again",
            );
        }
        self.scanned[d] = self.clock;
        device.persist()
    }

    /// The backlog after R's last seq, or the listing when the server is behind R.
    fn resume(&mut self, d: usize) -> Result<Message, Failure> {
        let device = &mut self.devices[d];
        if device.engine.behind {
            let history = match device.recovering() {
                true => history_since(&self.space, &device.recovery_from, &device.engine.trees),
                false => History::new(),
            };
            return Ok(listing_with(&self.space, history));
        }
        let engine = &mut device.engine;
        let resume = Resume {
            history_id: engine.history.clone(),
            since_seq: engine.cursor.seq,
            since_commit_id: engine.cursor.commit_id.clone(),
            since_committed_at_ms: engine.cursor.committed_at_ms,
        };
        match self.space.resume(&resume) {
            Ok(backlog) => Ok(Message::Records(backlog)),
            Err(resync) if resync.reason() == resync_required::Reason::AheadOfServer => {
                note(&mut self.seen, "resume finds the server behind");
                engine.server_behind(Vec::new());
                Ok(listing(&self.space))
            }
            Err(resync) if resync.reason() == resync_required::Reason::LogTruncated => {
                note(&mut self.seen, "resume finds the log truncated");
                let link = self.devices[d]
                    .link
                    .as_mut()
                    .expect("a resume runs on a link");
                link.listing = true;
                Ok(listing(&self.space))
            }
            Err(error) => Err(Failure::Broken(format!(
                "the server refused a resume: {error:?}"
            ))),
        }
    }

    fn connect(&mut self, d: usize) -> Result<(), Failure> {
        if self.devices[d].link.is_some() {
            return Ok(());
        }
        if self.space.refuses() {
            note(&mut self.seen, "server refuses a history a rewind replaced");
            return Ok(());
        }
        let welcome = self.space.welcome(&self.devices[d].session);
        if self.devices[d].engine.history != welcome.history_id {
            self.recover(
                d,
                RecoveryTrigger::SpaceRewound,
                "the server's history changed".into(),
            )?;
        }
        let in_flight = self.devices[d].engine.commits.edited_in_flight();
        let commits = &mut self.devices[d].engine.commits;
        let reconnect = match commits.halted() {
            Some(commit::Halt::Recovery(_)) => Reconnect::Idle,
            _ => commits.connected(&welcome),
        };
        match reconnect {
            Reconnect::Recovery => {
                self.recover(
                    d,
                    RecoveryTrigger::StoreRolledBack,
                    format!(
                        "the server counts {} commits of this device, which the store cannot account for",
                        welcome.device_seq
                    ),
                )?;
            }
            Reconnect::AheadOfServer => {
                note(&mut self.seen, "reconnect finds the server lost commits");
                self.devices[d].engine.server_behind(in_flight);
            }
            Reconnect::Resend if welcome.device_seq > self.devices[d].engine.commits.accepted() => {
                note(
                    &mut self.seen,
                    "reconnect resends a commit the server applied",
                );
            }
            Reconnect::Resend => note(
                &mut self.seen,
                "reconnect resends a commit the server never got",
            ),
            Reconnect::Idle => {}
        }
        self.devices[d].link = Some(Link {
            head: welcome.head_seq,
            ..Default::default()
        });
        let backlog = self.resume(d)?;
        let link = self.devices[d].link.as_mut().expect("set above");
        link.inbox.push(backlog);
        self.devices[d].persist()
    }

    /// Receives one message as section 4.4 asks: commits apply in seq order, a commit R already
    /// holds is skipped, and a gap asks for the backlog again and drops the rest of the batch.
    /// Records that arrive while R waits for a listing are dropped, and the listing asks for the
    /// backlog after its head, as the driver subscribes again from the listing's seq.
    fn deliver(&mut self, d: usize, index: usize) -> Result<bool, Failure> {
        let device = &mut self.devices[d];
        let behind = device.engine.behind;
        let Some(link) = device.link.as_mut().filter(|l| !l.inbox.is_empty()) else {
            return Ok(false);
        };
        let waits = behind || link.listing;
        let message = link.inbox.remove(index % link.inbox.len());
        match message {
            Message::Records(_) if waits => {
                note(
                    &mut self.seen,
                    "records dropped while R waits for a listing",
                );
            }
            Message::Listing { .. } if !waits => {
                note(&mut self.seen, "listing arrives twice");
            }
            Message::Listing {
                nodes,
                head,
                history,
            } => {
                note(&mut self.seen, "listing replaces R");
                let Engine {
                    trees,
                    commits,
                    cursor,
                    behind,
                    ..
                } = &mut self.devices[d].engine;
                let recovering = match commits.halted() {
                    Some(commit::Halt::Recovery(cause)) => Some(cause),
                    _ => None,
                };
                let trashed = trees.trashed_here();
                let edited = trees.edited_here();
                let synced = trees.taken_from_others(|seq| commits.lost_here(seq));
                let moved = trees.moved_here();
                let untaken = trees.untaken_changes(|seq| commits.lost_here(seq));
                let records = trees.remote_records(|seq| commits.lost_here(seq));
                let removed = trees.removed_here();
                let root = trees.remote().root.clone();
                let mut listed = BTreeMap::new();
                for state in nodes.iter().filter(|state| state.node_id != root) {
                    let (id, node) = RemoteNode::from_state(state).map_err(|error| {
                        Failure::Broken(format!("unreadable node state: {error:?}"))
                    })?;
                    listed.insert(id, node);
                }
                let gone: Vec<NodeId> = (trees.remote().iter())
                    .map(|(id, _)| id)
                    .filter(|id| !listed.contains_key(*id))
                    .cloned()
                    .collect();
                for id in gone {
                    trees.remove_remote(&id);
                }
                for (id, node) in listed {
                    if trees.remote().get(&id) != Some(&node) {
                        trees.set_listed_remote(id, node);
                    }
                }
                if recovering.is_none() {
                    trees.listed_removals(removed, |seq| commits.listed_here(seq));
                }
                if *behind && recovering.is_none() {
                    trees.forget_lost(
                        trashed,
                        edited,
                        synced,
                        moved,
                        untaken,
                        records,
                        commits.held_through(),
                        commits.accepted(),
                    );
                    commits.forget_lost_slots();
                }
                *cursor = head;
                *behind = false;
                let link = self.devices[d].link.as_mut().expect("checked above");
                link.listing = false;
                if let Some(cause) = recovering {
                    note(&mut self.seen, "recovery classifies the trees");
                    let device_seq = self.space.welcome(&self.devices[d].session).device_seq;
                    let device = &mut self.devices[d];
                    device.engine.scan(&device.disk);
                    self.scanned[d] = self.clock;
                    let Engine {
                        trees,
                        commits,
                        ids,
                        ..
                    } = &mut device.engine;
                    let disk = &mut device.disk;
                    let unheld: Vec<(NodeId, u128)> = (trees.local().iter())
                        .filter(|(id, _)| trees.synced().get(id).is_none())
                        .map(|(id, node)| (id.clone(), node.identity.ino))
                        .collect();
                    let marks: Marks = (unheld.iter())
                        .filter_map(|(id, ino)| {
                            let node = disk.mark_of(&location_of(trees, id)?, *ino)?;
                            Some((id.clone(), node))
                        })
                        .collect();
                    commits.recover(trees, device_seq, &cause, &history, &marks, ids, NOW as u64);
                    let adopted = (trees.local().iter())
                        .filter(|(id, node)| {
                            trees.synced().get(id).is_some()
                                && unheld.iter().any(|(_, ino)| *ino == node.identity.ino)
                        })
                        .filter_map(|(id, node)| {
                            Some((location_of(trees, id)?, node.identity.ino, id))
                        });
                    for (at, ino, id) in adopted {
                        disk.mark(&at, ino, id);
                    }
                    for (id, node) in trees.local().iter() {
                        let (Some(here), Some(there)) = (
                            node.shape.content.as_ref(),
                            (trees.remote().get(id)).and_then(|r| r.shape.content.as_ref()),
                        ) else {
                            continue;
                        };
                        if here.mtime_ns != there.mtime_ns && same_content(here, there) {
                            let writes = self.held.entry(id.clone()).or_default();
                            writes.insert(here.mtime_ns, here.manifest_digest.clone());
                            self.published.insert(here.mtime_ns);
                        }
                    }
                    device.persist()?;
                }
                let backlog = self.resume(d)?;
                let link = self.devices[d].link.as_mut().expect("checked above");
                link.inbox.push(backlog);
            }
            Message::Records(records) => {
                for record in records {
                    let engine = &mut self.devices[d].engine;
                    if record.seq > engine.cursor.seq + 1 {
                        note(&mut self.seen, "records arrive past a gap");
                        let backlog = self.resume(d)?;
                        let link = self.devices[d].link.as_mut().expect("checked above");
                        link.inbox.push(backlog);
                        break;
                    }
                    if record.seq == engine.cursor.seq + 1 {
                        let made = (engine.commits.own_record(record.seq, &record.commit_id))
                            .then_some(record.commit_id.as_str());
                        engine.apply(&record.changes, made)?;
                        engine.cursor = Base {
                            seq: record.seq,
                            commit_id: record.commit_id.clone(),
                            committed_at_ms: record.committed_at_ms,
                        };
                        let flight = engine.commits.in_flight();
                        if flight.is_some_and(|f| f.commit.commit_id == record.commit_id) {
                            note(&mut self.seen, "own commit replayed before its result");
                        }
                        let engine = &mut self.devices[d].engine;
                        acknowledge(engine)?;
                    } else {
                        note(&mut self.seen, "records arrive twice");
                    }
                }
            }
            Message::Answer { request, result } => {
                if link.awaiting != Some(request) {
                    note(&mut self.seen, "answer arrives twice");
                    return Ok(true);
                }
                link.awaiting = None;
                let device = &mut self.devices[d];
                let refused = |error| Failure::Broken(format!("the answer was refused: {error:?}"));
                let problems = device.engine.commits.problems().len();
                let in_flight = device.engine.commits.edited_in_flight();
                match result {
                    Ok(result) => {
                        note(
                            &mut self.seen,
                            match result.seq {
                                seq if seq <= device.engine.cursor.seq => {
                                    "result arrives after its records"
                                }
                                seq if seq == device.engine.cursor.seq + 1 => {
                                    "result arrives in order"
                                }
                                _ => "result arrives before earlier commits",
                            },
                        );
                        let flight = device.engine.commits.in_flight().expect("answered");
                        let given = commit::given_nodes(&flight.commit, &result);
                        device.engine.commits.received(result).map_err(refused)?;
                        let trees = &device.engine.trees;
                        for (sent, node) in given {
                            let Some((id, local)) = [&node, &sent]
                                .into_iter()
                                .find_map(|id| Some((id, trees.local().get(id)?)))
                            else {
                                continue;
                            };
                            if let Some(at) = location_of(trees, id) {
                                device.disk.mark(&at, local.identity.ino, &node);
                            }
                        }
                    }
                    Err(error) => match device
                        .engine
                        .commits
                        .failed(&mut device.engine.trees, &error)
                        .map_err(refused)?
                    {
                        commit::Failure::Resend => {
                            let flight = device.engine.commits.in_flight().expect("kept to resend");
                            let gone = sources(&flight.commit).any(|(node, content)| {
                                !readable(&device.disk, &device.engine.trees, node, content)
                            });
                            if error.code == error::Code::MissingChunks as i32 && gone {
                                note(&mut self.seen, "commit abandoned, its bytes are gone");
                                device.engine.commits.abandon();
                                device.engine.scan(&device.disk);
                                self.scanned[d] = self.clock;
                            } else {
                                note(&mut self.seen, "commit refused, sent again");
                            }
                        }
                        commit::Failure::Abandoned => {
                            note(&mut self.seen, "commit refused, planned again");
                        }
                        commit::Failure::Split => note(
                            &mut self.seen,
                            match device.engine.commits.problems().len() > problems {
                                true => {
                                    "commit of one op refused for its size, its node is a problem"
                                }
                                false => "commit refused for its size, planned again in parts",
                            },
                        ),
                        commit::Failure::Resync => {
                            note(&mut self.seen, "commit ahead of the server, R listed again");
                            device.engine.server_behind(in_flight);
                            let listed = listing(&self.space);
                            (device.link.as_mut()).expect("answered").inbox.push(listed);
                        }
                        commit::Failure::Halted(commit::Halt::Recovery(cause)) => {
                            note(&mut self.seen, "recovery starts after a refused commit");
                            device.persist()?;
                            let code = error::Code::try_from(error.code);
                            return self
                                .recover(
                                    d,
                                    cause.trigger,
                                    format!("the server refused a commit with {code:?}"),
                                )
                                .map(|()| true);
                        }
                        failure => {
                            return Err(Failure::Broken(format!(
                                "{error:?} on a commit led to {failure:?}"
                            )));
                        }
                    },
                }
            }
        }
        acknowledge(&mut self.devices[d].engine)?;
        self.devices[d].persist()?;
        Ok(true)
    }

    /// Builds the next commit, or sends the one in flight, once the device is connected and
    /// caught up. With a reply other than `Reply::Served` a step does both. True when the
    /// trees changed or a commit was built or sent.
    fn server_now(&self) -> i64 {
        NOW + self.clock as i64
    }

    fn commit(&mut self, d: usize, reply: Reply) -> Result<bool, Failure> {
        self.revoke_asked_again(d);
        let now_ms = self.server_now();
        let device = &mut self.devices[d];
        let Some(link) = device.link.as_mut() else {
            return Ok(false);
        };
        let engine = &mut device.engine;
        if engine.behind || link.listing || engine.cursor.seq < link.head {
            return Ok(false);
        }
        let Some(flight) = engine.commits.in_flight() else {
            let history = engine.history.clone();
            engine.trees.settle();
            let Engine {
                trees,
                commits,
                ids,
                cursor,
                ..
            } = engine;
            let device_id = &device.session.device_id;
            if commits.due(trees, device_id) {
                let inos: BTreeMap<u128, NodeId> = (trees.local().iter())
                    .map(|(id, node)| (node.identity.ino, id.clone()))
                    .collect();
                commits.build(trees, ids, NOW as u64, &history, device_id, cursor.clone());
                self.built[d] = self.scanned[d];
                let bound = copy_bound(&self.held_commits[d], self.restored_to);
                let ops = commits.in_flight().map(|flight| &flight.commit.ops);
                for op in ops.into_iter().flatten() {
                    let Some(op::Op::CreateFile(create)) = &op.op else {
                        continue;
                    };
                    self.planned_copies[d].remove(&create.node_id);
                    let source = (trees.local().get(&create.node_id))
                        .and_then(|node| inos.get(&node.identity.ino))
                        .filter(|source| **source != create.node_id);
                    let (Some(source), Some(content)) = (source, &create.content) else {
                        continue;
                    };
                    let versions = self.space.list_versions(&ListVersions {
                        node_id: source.clone(),
                        limit: 1000,
                        ..Default::default()
                    });
                    let versions: Vec<u64> = (versions.iter().flat_map(|list| &list.versions))
                        .filter(|version| {
                            (version.content.as_ref())
                                .is_some_and(|held| held.mtime_ns == content.mtime_ns)
                        })
                        .map(|version| version.seq)
                        .collect();
                    if !versions.is_empty() {
                        let copy = RecoveryCopy {
                            source: source.clone(),
                            write: content.mtime_ns,
                            device: d,
                            bound,
                            versions,
                        };
                        self.planned_copies[d].insert(create.node_id.clone(), copy);
                    }
                }
            }
            if commits.in_flight().is_none() && !commits.problems().is_empty() {
                note(&mut self.seen, "no commit built while a node is a problem");
            }
            let changed = device.engine != device.store;
            device.persist()?;
            return Ok(changed);
        };
        if flight.result.is_some() || link.awaiting.is_some() || engine.commits.halted().is_some() {
            return Ok(false);
        }
        let commit = flight.commit.clone();
        link.requests += 1;
        let request = link.requests;
        link.awaiting = Some(request);
        let applied = self.space.device_seq(&device.session.device_id) >= commit.device_seq;
        let stale = commit.require_head && commit.base_seq < self.space.head_seq();
        let refusal = match reply {
            // The codes say that nothing was applied, so the server never gives one for a commit
            // it holds, and it gives `HEAD_MOVED` only to a commit that requires its base.
            Reply::Refused(code) if applied || code == error::Code::HeadMoved && !stale => {
                note(
                    &mut self.seen,
                    "refusal the server cannot give, commit served",
                );
                None
            }
            Reply::Refused(code) => Some(Error {
                code: code as i32,
                retry_after_ms: 1,
                ..Default::default()
            }),
            Reply::Served | Reply::NoChunks => None,
        };
        let result = match refusal {
            Some(error) => Err(error),
            None => {
                let receipts = match reply {
                    Reply::NoChunks => Vec::new(),
                    _ => upload(
                        &mut self.space,
                        &device.session,
                        now_ms,
                        &device.disk,
                        &device.engine.trees,
                        &commit,
                    ),
                };
                let before = live_writes(&self.space);
                let session = device.session.clone();
                let sent = Commit { receipts, ..commit };
                let held = self.space.nodes();
                let finishes = |moved: &Move| {
                    (self.space.node(&moved.node_id)).is_some_and(|node| {
                        node.status == NodeStatus::Live as i32
                            && node.parent_id == moved.new_parent_id
                            && (node.pending_for == moved.new_name || node.name == moved.new_name)
                    })
                };
                let made_up: Vec<(usize, &Move)> = (sent.ops.iter().enumerate())
                    .filter_map(|(index, op)| match &op.op {
                        Some(op::Op::Move(moved)) if !applied && !finishes(moved) => {
                            let unchanged = held.iter().any(|node| {
                                node.node_id == moved.node_id
                                    && node.parent_id == moved.new_parent_id
                                    && same_name(&node.name, &moved.new_name)
                            });
                            let restored = self.restored_names[d].get(&moved.node_id);
                            let asked = |name: &str| {
                                self.named[d].iter().any(|named| same_name(named, name))
                                    || restored.is_some_and(|names| {
                                        names.iter().any(|restored| same_name(restored, name))
                                    })
                            };
                            (!unchanged && !given(asked, &moved.new_name)).then_some((index, moved))
                        }
                        _ => None,
                    })
                    .collect();
                let trashed: BTreeSet<NodeId> = (self.space.nodes().into_iter())
                    .filter(|node| node.status == NodeStatus::Trashed as i32)
                    .map(|node| node.node_id)
                    .collect();
                let resends: BTreeSet<NodeId> = (self.space.nodes().into_iter())
                    .filter(|node| node.status == NodeStatus::Live as i32)
                    .filter(|node| node.edit_rev < node.content_rev)
                    .map(|node| node.node_id)
                    .collect();
                match self.space.commit(&session, now_ms, &sent) {
                    Ok(committed) => {
                        if !self.exposed.is_empty() {
                            let nodes = self.space.nodes();
                            for (write, exposure) in &mut self.exposed {
                                let replaced = (nodes.iter())
                                    .filter(|node| exposure.nodes.contains_key(&node.node_id))
                                    .filter(|node| {
                                        (node.current.as_ref())
                                            .and_then(|version| version.content.as_ref())
                                            .is_some_and(|content| content.mtime_ns != *write)
                                    })
                                    .map(|node| node.node_id.clone());
                                exposure.replaced.extend(replaced);
                            }
                        }
                        let answer = (self.space.recorded_result(&sent.commit_id))
                            .cloned()
                            .unwrap_or_default();
                        let named = made_up_name(&made_up, &answer, &held, &self.space);
                        if let Some(moved) = named {
                            self.unasked.get_or_insert_with(|| {
                                format!(
                                    "a commit of device {d} moves node {} to the name {:?}, which \
                                     the user of device {d} never gave to anything",
                                    moved.node_id, moved.new_name
                                )
                            });
                        }
                        if let Some(record) = committed.record {
                            let placed: BTreeSet<&str> = (sent.ops.iter())
                                .zip(&committed.result.results)
                                .filter(|(_, result)| {
                                    matches!(
                                        result.code(),
                                        op_result::Code::Applied | op_result::Code::AppliedRenamed
                                    )
                                })
                                .filter_map(|(op, _)| match &op.op {
                                    Some(op::Op::Move(moved)) => Some(moved.node_id.as_str()),
                                    Some(op::Op::CreateFile(made)) => Some(made.node_id.as_str()),
                                    Some(op::Op::CreateDir(made)) => Some(made.node_id.as_str()),
                                    _ => None,
                                })
                                .collect();
                            self.placements[d].extend(
                                crate::space::node_states(&record.changes)
                                    .filter(|state| placed.contains(state.node_id.as_str()))
                                    .map(|state| Placement {
                                        commit: record.commit_id.clone(),
                                        node: state.node_id.clone(),
                                        name: state.name.clone(),
                                    }),
                            );
                            self.held_commits[d].insert(record.seq);
                            let built = self.built[d];
                            let nodes = self.space.nodes();
                            let parents: BTreeMap<&str, &str> = (nodes.iter())
                                .map(|node| (node.node_id.as_str(), node.parent_id.as_str()))
                                .collect();
                            let local = self.devices[d].engine.trees.local();
                            let synced = self.devices[d].engine.trees.synced();
                            let ino = |node: &str| {
                                (local.get(node).map(|node| node.identity.ino))
                                    .or_else(|| self.touched_nodes[d].get(node).copied())
                            };
                            let restored: BTreeSet<&str> = (sent.ops.iter())
                                .filter_map(|op| match &op.op {
                                    Some(op::Op::Restore(restore)) => {
                                        Some(restore.node_id.as_str())
                                    }
                                    _ => None,
                                })
                                .collect();
                            let merges: Vec<(NodeId, u128)> = (sent.ops.iter())
                                .zip(&committed.result.results)
                                .filter(|(_, outcome)| {
                                    outcome.code == op_result::Code::MergedInto as i32
                                })
                                .filter_map(|(op, outcome)| match &op.op {
                                    Some(op::Op::CreateDir(create)) => {
                                        Some((outcome.node_id.clone(), ino(&create.node_id)?))
                                    }
                                    _ => None,
                                })
                                .collect();
                            let touched = |node: &str| {
                                let merged = self.merged[d].get(node).into_iter().flatten();
                                restored.contains(node)
                                    || (ino(node).into_iter().chain(merged.copied()))
                                        .filter_map(|ino| self.touches[d].get(&ino))
                                        .any(|at| at.first().is_some_and(|at| *at <= built))
                            };
                            let asked = |node: &str| {
                                touched(node)
                                    || ino(node).is_some_and(|ino| self.approved[d].contains(&ino))
                            };
                            fn chain<'a>(
                                parents: &BTreeMap<&'a str, &'a str>,
                                node: &'a str,
                            ) -> Vec<&'a str> {
                                std::iter::successors(Some(node), |id| parents.get(id).copied())
                                    .collect()
                            }
                            let revived = (nodes.iter()).find(|node| {
                                let id = node.node_id.as_str();
                                node.status == NodeStatus::Live as i32
                                    && trashed.contains(id)
                                    && !asked(id)
                                    && !(chain(&parents, id).into_iter().skip(1))
                                        .take_while(|id| trashed.contains(*id))
                                        .any(touched)
                                    && !(nodes.iter()).any(|other| {
                                        asked(&other.node_id)
                                            && chain(&parents, &other.node_id)
                                                .contains(&node.node_id.as_str())
                                    })
                            });
                            if (nodes.iter()).any(|node| {
                                node.status == NodeStatus::Live as i32
                                    && trashed.contains(&node.node_id)
                                    && pending_stem(&node.name).is_some()
                            }) {
                                note(
                                    &mut self.seen,
                                    "a node at a pending name comes back from the trash",
                                );
                            }
                            if let Some(node) = revived {
                                return Err(Failure::Broken(format!(
                                    "a commit of device {d} brought back node {} as {:?}, which a \
                                     user deleted, and the user of device {d} never changed it or \
                                     put anything in it since: {:?}",
                                    node.node_id, node.name, sent.ops
                                )));
                            }
                            let changes: Vec<(u128, Change)> = (sent.ops.iter())
                                .zip(&committed.result.results)
                                .filter(|(_, outcome)| {
                                    outcome.code != op_result::Code::Rejected as i32
                                })
                                .filter_map(|(op, outcome)| match &op.op {
                                    Some(op::Op::CreateDir(_))
                                        if outcome.code == op_result::Code::MergedInto as i32 =>
                                    {
                                        Some((&outcome.node_id, Change::Made))
                                    }
                                    Some(op::Op::CreateDir(op)) => {
                                        Some((&op.node_id, Change::Made))
                                    }
                                    Some(op::Op::CreateFile(op)) => {
                                        Some((&op.node_id, Change::Made))
                                    }
                                    Some(op::Op::UpdateContent(op)) => {
                                        Some((&op.node_id, Change::Edited))
                                    }
                                    Some(op::Op::Move(op)) => Some((&op.node_id, Change::Moved)),
                                    _ => None,
                                })
                                .filter(|(node, _)| {
                                    (nodes.iter()).any(|live| {
                                        live.node_id == **node
                                            && live.status == NodeStatus::Live as i32
                                    })
                                })
                                .filter_map(|(node, change)| Some((ino(node)?, change)))
                                .chain(merges.iter().map(|(_, ino)| (*ino, Change::Made)))
                                .collect();
                            for (into, ino) in merges {
                                self.merged[d].entry(into).or_default().insert(ino);
                            }
                            for (ino, _) in &changes {
                                self.approved[d].remove(ino);
                            }
                            let carried: BTreeSet<u64> = (self.actions[d].range(..=built))
                                .filter(|(_, (own, action))| {
                                    let settled = || {
                                        (local.iter()).any(|(id, here)| {
                                            here.identity.ino == *own
                                                && (synced.get(id)).is_some_and(|synced| {
                                                    action.settled(&here.shape, synced)
                                                })
                                        })
                                    };
                                    (changes.iter()).any(|(ino, change)| {
                                        ino == own && (change.carries(*action) || settled())
                                    })
                                })
                                .map(|(at, _)| *at)
                                .collect();
                            for (ino, times) in &mut self.touches[d] {
                                for at in &carried {
                                    if times.remove(at) {
                                        self.spent.push(Spent {
                                            seq: record.seq,
                                            device: d,
                                            ino: *ino,
                                            at: *at,
                                        });
                                    }
                                }
                            }
                            let after = live_writes(&self.space);
                            let answered: Vec<i64> = (self.rewound.keys())
                                .filter(|write| !before.contains_key(write))
                                .filter(|write| {
                                    let kept = self.kept[d].get(*write);
                                    let ino = |node: &NodeId| Some(local.get(node)?.identity.ino);
                                    after.get(*write).is_some_and(|node| {
                                        kept.is_some_and(|nodes| {
                                            nodes.contains_key(node)
                                                || ino(node).is_some_and(|ino| {
                                                    nodes.values().any(|kept| *kept == ino)
                                                })
                                        })
                                    })
                                })
                                .copied()
                                .collect();
                            self.republished.extend(answered);
                            let resent = (self.rewound.keys()).find(|write| {
                                after.contains_key(write) && !self.republished.contains(write)
                            });
                            if let Some(write) = resent {
                                return Err(Failure::Broken(format!(
                                    "a commit of device {d} sent write {write} again after a \
                                     rewind took it from the server, though no user answered to \
                                     keep it"
                                )));
                            }
                            let folders: BTreeSet<&str> = (sent.ops.iter())
                                .filter_map(|op| match &op.op {
                                    Some(op::Op::Delete(delete)) => Some(delete.node_id.as_str()),
                                    _ => None,
                                })
                                .collect();
                            let digests: BTreeMap<NodeId, Vec<u8>> =
                                (self.space.nodes().into_iter())
                                    .filter(|node| node.status == NodeStatus::Live as i32)
                                    .filter_map(|node| {
                                        Some((node.node_id, node.current?.content?.manifest_digest))
                                    })
                                    .collect();
                            for (op, outcome) in sent.ops.iter().zip(&committed.result.results) {
                                let Some(op::Op::CreateFile(create)) = &op.op else {
                                    continue;
                                };
                                let created =
                                    [op_result::Code::Applied, op_result::Code::AppliedRenamed]
                                        .map(|code| code as i32)
                                        .contains(&outcome.code);
                                let content = create.content.as_ref();
                                if let Some(copy) = self.planned_copies[d].get(&create.node_id)
                                    && created
                                    && content.is_some_and(|content| content.mtime_ns == copy.write)
                                    && (digests.get(&copy.source)).is_some_and(|digest| {
                                        Some(digest) != content.map(|c| &c.manifest_digest)
                                    })
                                {
                                    self.recovery_copies
                                        .insert(create.node_id.clone(), copy.clone());
                                }
                            }
                            let gone = before.iter().filter(|(w, node)| {
                                let digest = self.held.get(*node).and_then(|held| held.get(*w));
                                !after.contains_key(w) && digests.get(*node) != digest
                            });
                            for (lost, node) in gone {
                                // The engine tells contents apart by their bytes. A user who
                                // overwrites or deletes a file therefore does the same to a later
                                // write of the same bytes to that file.
                                let held = self.held.get(node);
                                let digest = held.and_then(|held| held.get(lost));
                                let replaced = |write: &i64| {
                                    std::iter::successors(self.overwrote.get(write), |write| {
                                        self.overwrote.get(write)
                                    })
                                    .any(|write| write == lost)
                                };
                                let asked = |removed: &BTreeSet<i64>| {
                                    (removed.iter()).any(|write| {
                                        write == lost
                                            || replaced(write)
                                                && held.is_some_and(|held| held.contains_key(write))
                                            || digest.is_some()
                                                && held.and_then(|held| held.get(write)) == digest
                                    })
                                };
                                let resent = (sent.ops.iter()).any(|op| match &op.op {
                                    Some(op::Op::UpdateContent(update)) => {
                                        update.node_id == *node
                                            && self.rolled_back.contains_key(node)
                                            && ((update.content.as_ref()).is_some_and(|content| {
                                                (self.exposed.get(&content.mtime_ns))
                                                    .is_some_and(|e| e.rolled_back(node))
                                            }) || (self.exposed.iter()).any(
                                                |(write, exposure)| {
                                                    exposure.rolled_back(node)
                                                        && self.removed[d].contains(write)
                                                },
                                            ))
                                    }
                                    _ => false,
                                });
                                if asked(&self.removed[d])
                                    || resent && self.removed.iter().any(asked)
                                    || resends.contains(node)
                                        && (nodes.iter()).any(|trashed| {
                                            trashed.node_id == *node
                                                && trashed.status != NodeStatus::Live as i32
                                        })
                                        && (chain(&parents, node).iter())
                                            .any(|id| folders.contains(id))
                                        && (self.removed[d].iter()).any(|write| {
                                            held.is_some_and(|held| held.contains_key(write))
                                        })
                                {
                                    continue;
                                }
                                let why = if self.removed.iter().any(asked) {
                                    format!(
                                        "a commit of device {d} took write {lost} out of the \
                                         server's tree. The user of another device overwrote or \
                                         deleted it, and the user of device {d} never did: {:?}",
                                        sent.ops
                                    )
                                } else if trashed_under(&self.space, *lost, &folders) {
                                    // The server sends what a deleted folder holds to its trash.
                                    // That write is not lost, but no user deleted it.
                                    format!(
                                        "a commit of device {d} deleted a folder, and write {lost} \
                                         went to the trash with it, which the user of device {d} \
                                         never had in that folder"
                                    )
                                } else {
                                    return Err(Failure::Broken(format!(
                                        "a commit of device {d} took write {lost} out of the \
                                         server's tree, and no user overwrote or deleted it: {:?}",
                                        sent.ops
                                    )));
                                };
                                self.unasked.get_or_insert(why);
                            }
                            self.published.extend(after.into_keys());
                            for node in self.space.nodes() {
                                if let Some(content) = node.current.and_then(|v| v.content) {
                                    for held in [&mut self.held, &mut self.in_history] {
                                        let writes = held.entry(node.node_id.clone()).or_default();
                                        writes.insert(
                                            content.mtime_ns,
                                            content.manifest_digest.clone(),
                                        );
                                    }
                                }
                            }
                            note(&mut self.seen, "commit applied");
                            for outcome in &committed.result.results {
                                use op_result::Code;
                                note(
                                    &mut self.seen,
                                    match Code::try_from(outcome.code) {
                                        Ok(Code::ConflictCopy) => "op made a conflict copy",
                                        Ok(Code::MergedInto) => "op merged a folder",
                                        Ok(Code::AppliedRenamed) => "op renamed by the server",
                                        Ok(Code::Superseded) => "op superseded",
                                        Ok(Code::Rejected) => "op rejected",
                                        _ => "op applied",
                                    },
                                );
                            }
                            for (op, outcome) in sent.ops.iter().zip(&committed.result.results) {
                                let answer = match op_result::Code::try_from(outcome.code) {
                                    Ok(
                                        op_result::Code::Applied | op_result::Code::AppliedRenamed,
                                    ) => "move with from applied",
                                    Ok(op_result::Code::Superseded) => "move with from superseded",
                                    _ => continue,
                                };
                                if let Some(op::Op::Move(moved)) = &op.op
                                    && moved.from.is_some()
                                {
                                    note(&mut self.seen, answer);
                                }
                            }
                            for (other, device) in self.devices.iter_mut().enumerate() {
                                if let Some(link) = device.link.as_mut().filter(|_| other != d) {
                                    link.inbox.push(Message::Records(vec![record.clone()]));
                                }
                            }
                        } else {
                            note(&mut self.seen, "commit answered from the stored result");
                        }
                        Ok(committed.result)
                    }
                    Err(error) => Err(error),
                }
            }
        };
        let link = self.devices[d].link.as_mut().expect("checked above");
        link.inbox.push(Message::Answer { request, result });
        Ok(true)
    }

    /// Stops the faults, lets the devices become quiet and checks the end state. A lost write is
    /// reported before a tree the devices did not reach.
    fn quiesce(&mut self) -> Result<(), Failure> {
        let quiet = self.quiet();
        if let Err(Failure::Broken(_)) = quiet {
            return quiet;
        }
        self.kept()?;
        match self.unasked.take() {
            Some(why) => Err(Failure::Diverged(why)),
            None => quiet,
        }
    }

    /// Every device restarts if it is down, reconnects, tries its failed effects again and
    /// rescans. Then the devices take turns until a whole round changes nothing.
    fn quiet(&mut self) -> Result<(), Failure> {
        for d in 0..self.devices.len() {
            if self.devices[d].left {
                continue;
            }
            if self.devices[d].down {
                self.restart(d)?;
            }
            if !self.devices[d].engine.effects.in_flight().is_empty() {
                self.effect(d, Outcome::Run)?;
            }
            self.devices[d].link = None;
            self.connect(d)?;
            let device = &mut self.devices[d];
            let blocked: Vec<NodeId> = device.engine.effects.blocked().keys().cloned().collect();
            for node in blocked {
                device.engine.effects.unblock(&node);
            }
            self.scan(d)?;
        }
        for _ in 0..ROUNDS {
            let before: Vec<Engine> = self.devices.iter().map(|d| d.engine.clone()).collect();
            for d in 0..self.devices.len() {
                if self.devices[d].left {
                    continue;
                }
                while self.answer(d, self.answers)? {
                    self.answers += 1;
                }
                let at = |failure: Failure| {
                    failure.at(format!("after the faults stopped, on device {d}"))
                };
                while self.deliver(d, 0).map_err(at)? {}
                self.effect(d, Outcome::Run).map_err(at)?;
                self.effect(d, Outcome::Run).map_err(at)?;
                self.commit(d, Reply::Served).map_err(at)?;
                self.commit(d, Reply::Served).map_err(at)?;
                let trees = &self.devices[d].engine.trees;
                (trees.check())
                    .map_err(|(tree, error)| {
                        Failure::Broken(format!("{tree} is not a valid tree: {error:?}"))
                    })
                    .map_err(at)?;
                if self.trace {
                    eprintln!(
                        "quiet round, device {d}:
{}",
                        self.dump()
                    );
                }
            }
            let refused = self.space.refuses();
            let idle = |device: &Device| {
                device.left || refused || device.link.as_ref().is_some_and(|l| l.inbox.is_empty())
            };
            if self.devices.iter().all(idle)
                && self.devices.iter().map(|d| &d.engine).eq(before.iter())
            {
                return match refused {
                    true => Ok(()),
                    false => self.converged(),
                };
            }
        }
        Err(Failure::Diverged(format!(
            "the devices were not quiet {ROUNDS} rounds after the faults stopped"
        )))
    }

    /// The server's nodes and every device's disk and trees, one line each, for reading a failing
    /// schedule step by step. A file shows the tag of the write it holds.
    fn dump(&self) -> String {
        use std::fmt::Write;
        let short = |id: &str| id[id.len().saturating_sub(4)..].to_owned();
        let file = |content: &Option<Content>| match content {
            Some(content) => format!("w{}", content.mtime_ns),
            None => "dir".to_owned(),
        };
        let mut out = format!(
            "server at seq {}
",
            self.space.head_seq()
        );
        for node in self
            .space
            .nodes()
            .iter()
            .filter(|node| node.node_id != self.space.space_id())
        {
            let content = node
                .current
                .as_ref()
                .and_then(|version| version.content.clone());
            let trashed = if node.status == NodeStatus::Live as i32 {
                ""
            } else {
                " trashed"
            };
            let (id, parent) = (short(&node.node_id), short(&node.parent_id));
            writeln!(
                out,
                "  {id} in {parent} {:?} {}{trashed}",
                node.name,
                file(&content)
            )
            .unwrap();
        }
        for (d, device) in self.devices.iter().enumerate() {
            let engine = &device.engine;
            let link = match &device.link {
                _ if device.down => "down".to_owned(),
                None => "offline".to_owned(),
                Some(link) => format!("online, {} messages on their way", link.inbox.len()),
            };
            writeln!(out, "device {d}: {link}, last_seq {}", engine.cursor.seq).unwrap();
            for (ino, entry) in device.disk.entries() {
                let place = match &entry.place {
                    Place::Root => "root".to_owned(),
                    Place::In(parent) => format!("ino {parent}"),
                    Place::Parked(node) => format!("parked {}", short(node)),
                };
                writeln!(
                    out,
                    "  disk ino {ino} in {place} {:?} {}",
                    entry.name,
                    file(&entry.content)
                )
                .unwrap();
            }
            let trees = &engine.trees;
            let mut ids: BTreeSet<&NodeId> = trees.local().iter().map(|(id, _)| id).collect();
            ids.extend(trees.synced().iter().map(|(id, _)| id));
            ids.extend(trees.remote().iter().map(|(id, _)| id));
            for id in ids {
                let shape = |shape: Option<&koidrive_engine::tree::Shape>| match shape {
                    Some(shape) => format!(
                        "{} {:?} {}",
                        short(&shape.parent),
                        shape.name,
                        file(&shape.content)
                    ),
                    None => "-".to_owned(),
                };
                let local = trees.local().get(id);
                let mut flags = String::new();
                for (flag, set) in [
                    (" ino", local.is_some()),
                    (" parked", local.is_some_and(|node| node.parked)),
                    (
                        " trashed",
                        trees.remote().get(id).is_some_and(|node| node.trashed),
                    ),
                    (" dirty", trees.dirty().contains(id)),
                    (" reverted", trees.reverted().contains(id)),
                    (" waiting", trees.waiting().contains_key(id)),
                    (" blocked", engine.effects.blocked().contains_key(id)),
                    (" problem", engine.commits.problems().contains(id)),
                    (" held", trees.held(id)),
                ] {
                    if set {
                        flags.push_str(flag);
                    }
                    if flag == " ino"
                        && let Some(node) = local
                    {
                        write!(flags, " {}", node.identity.ino).unwrap();
                    }
                }
                let (l, s) = (
                    shape(local.map(|node| &node.shape)),
                    shape(trees.synced().live(id)),
                );
                let r = shape(trees.remote().get(id).map(|node| &node.shape));
                writeln!(out, "  node {} L[{l}] S[{s}] R[{r}]{flags}", short(id)).unwrap();
            }
            if let Some(record) = engine.effects.in_flight().first() {
                writeln!(
                    out,
                    "  effect in flight for {}: {:?}",
                    short(&record.node),
                    record.effect
                )
                .unwrap();
            }
            if let Some(flight) = engine.commits.in_flight() {
                let result = flight.result.as_ref().map(|result| result.seq);
                writeln!(
                    out,
                    "  commit in flight at base {}, result {result:?}:",
                    flight.commit.base_seq
                )
                .unwrap();
                for op in &flight.commit.ops {
                    writeln!(out, "    {:?}", op.op).unwrap();
                }
            }
            for question in engine.commits.questions() {
                let holds: Vec<String> = question.holds().map(|id| short(id)).collect();
                writeln!(out, "  question {:?} holds {holds:?}", question.kind).unwrap();
            }
        }
        out
    }

    /// Every write is on the disk of the device whose user made it, unless the server has held it
    /// or that user overwrote or deleted it, or discarded it in answer to a recovery question,
    /// which sends it to the local trash. A write a restore took from the server while a disk
    /// held it is on the server again, unless a user overwrote or deleted it. A write a rewind
    /// took from the server stays on every disk that held it, or in its local trash, unless that
    /// disk's user overwrote or deleted it.
    fn kept(&self) -> Result<(), Failure> {
        let lost = (self.writers.iter()).find(|(write, writer)| {
            let disk = &self.devices[**writer].disk;
            !stored(&self.held, **write)
                && !self.rewound.contains_key(write)
                && !self.removed[**writer].contains(write)
                && !disk.holds(**write)
                && !((self.discarded.contains(write) || self.binned_copies.contains(write))
                    && disk.binned(**write))
        });
        if let Some((write, writer)) = lost {
            return Err(Failure::Broken(format!(
                "write {write} left the disk of device {writer}: the server never held it, and \
                 its user never overwrote or deleted it"
            )));
        }
        let server: BTreeSet<i64> = (self.space.nodes().into_iter())
            .filter_map(|node| Some(node.current?.content?.mtime_ns))
            .collect();
        let dropped = (self.rewound.iter())
            .flat_map(|(write, holders)| holders.iter().map(move |d| (*write, *d)))
            .find(|(write, d)| {
                let disk = &self.devices[*d].disk;
                !server.contains(write)
                    && !self.removed[*d].contains(write)
                    && !disk.holds(*write)
                    && !disk.binned(*write)
            });
        if let Some((write, d)) = dropped {
            return Err(Failure::Broken(format!(
                "a rewind took write {write} from the server while the disk of device {d} held \
                 it, and it left that disk though its user never overwrote or deleted it"
            )));
        }
        let resent = (self.rewound.keys())
            .find(|write| server.contains(write) && !self.republished.contains(write));
        if let Some(write) = resent {
            return Err(Failure::Broken(format!(
                "a rewind took write {write} from the server, and a device sent it again though no \
                 user answered to keep it"
            )));
        }
        let refused = self.space.refuses();
        let gone = (self.exposed.keys()).find(|write| {
            !server.contains(write)
                && !self.retired.contains(write)
                && !self.removed.iter().any(|removed| removed.contains(write))
                && !(self.devices.iter().zip(&self.discards)).any(|(device, discards)| {
                    discards.contains(write) && device.disk.binned(**write)
                })
                && !(refused
                    && (self.devices.iter())
                        .any(|device| device.disk.holds(**write) || device.disk.binned(**write)))
        });
        match gone {
            Some(write) => Err(Failure::Broken(format!(
                "a restore took write {write} from the server while a disk held it, and the \
                 server does not hold it again, though no user overwrote or deleted it"
            ))),
            None => Ok(()),
        }
    }

    /// The server's live tree by path.
    fn server_tree(&self) -> Result<BTreeMap<Vec<String>, Option<Content>>, Failure> {
        let nodes = self.space.nodes();
        let live = |id: &str| {
            (nodes.iter()).find(|node| node.node_id == id && node.status == NodeStatus::Live as i32)
        };
        let mut tree = BTreeMap::new();
        for node in nodes
            .iter()
            .filter(|node| node.node_id != self.space.space_id())
        {
            if node.status != NodeStatus::Live as i32 {
                continue;
            }
            let mut path = vec![node.name.clone()];
            let mut parent = node.parent_id.as_str();
            while parent != self.space.space_id() {
                let above = live(parent).filter(|_| path.len() <= nodes.len());
                let above = above.ok_or_else(|| {
                    Failure::Broken(format!(
                        "the server's node {} is not under the root",
                        node.node_id
                    ))
                })?;
                path.push(above.name.clone());
                parent = &above.parent_id;
            }
            path.reverse();
            let content = node
                .current
                .as_ref()
                .and_then(|version| version.content.clone());
            tree.insert(path, content.filter(|_| node.kind == NodeKind::File as i32));
        }
        Ok(tree)
    }

    /// The end state of sections 3.3 and 3.4.
    fn converged(&self) -> Result<(), Failure> {
        let server = self.server_tree()?;
        let nodes: Vec<(NodeId, RemoteNode)> = (self.space.nodes().iter())
            .filter(|state| state.node_id != self.space.space_id())
            .map(|state| RemoteNode::from_state(state).expect("a file or a folder"))
            .collect();
        for (d, device) in self.devices.iter().enumerate() {
            if device.left {
                continue;
            }
            let engine = &device.engine;
            let fail =
                |why: String| Err(Failure::Diverged(format!("device {d} is quiet, but {why}")));
            if let Some(flight) = engine.commits.in_flight() {
                return fail(format!("a commit is in flight: {flight:?}"));
            }
            if let Some(record) = engine.effects.in_flight().first() {
                return fail(format!("an effect is in flight: {record:?}"));
            }
            if engine.behind {
                return fail("R waits for a listing".into());
            }
            if !engine.effects.blocked().is_empty() {
                return fail(format!(
                    "effects are blocked: {:?}",
                    engine.effects.blocked()
                ));
            }
            if !engine.commits.problems().is_empty() || engine.commits.halted().is_some() {
                let (problems, halted) = (engine.commits.problems(), engine.commits.halted());
                return fail(format!(
                    "committing has problems {problems:?} or has stopped: {halted:?}"
                ));
            }
            if let Some((id, _)) = engine.trees.local().iter().find(|(_, node)| node.parked) {
                return fail(format!("node {id} is parked"));
            }
            let state = engine.trees.state();
            if !state.resends.is_empty()
                || !state.lost_positions.is_empty()
                || !state.lost_creates.is_empty()
            {
                return fail(format!(
                    "it still sends nodes again: {:?}, at {:?}, lost creates {:?}",
                    state.resends, state.lost_positions, state.lost_creates
                ));
            }
            let mut trees = engine.trees.clone();
            trees.settle();
            let unsettled = trees.work();
            if !unsettled.is_empty() {
                return fail(format!("nodes are not in sync: {unsettled:?}"));
            }
            let mut disk: BTreeMap<Vec<String>, Option<Content>> = BTreeMap::new();
            for (ino, entry) in device.disk.entries() {
                let Some(path) = device.disk.path(*ino) else {
                    return fail("its parked directory is not empty".into());
                };
                let normal: Vec<String> = path.iter().map(|name| nfc(name)).collect();
                if disk.insert(normal, entry.content.clone()).is_some() {
                    return fail(format!("its disk has two entries named {path:?} in NFC"));
                }
            }
            let paths: BTreeSet<&Vec<String>> = disk.keys().chain(server.keys()).collect();
            let equal = |here: Option<&Option<Content>>, there: Option<&Option<Content>>| match (
                here, there,
            ) {
                (Some(Some(here)), Some(Some(there))) => same_content(here, there),
                (here, there) => here == there,
            };
            if let Some(path) = paths
                .into_iter()
                .find(|path| !equal(disk.get(*path), server.get(*path)))
            {
                let (here, there) = (disk.get(path), server.get(path));
                return fail(format!(
                    "its disk has {here:?} at {path:?} and the server has {there:?}"
                ));
            }
            let trees = &engine.trees;
            let mut ids: BTreeSet<&NodeId> = nodes.iter().map(|(id, _)| id).collect();
            ids.extend(trees.local().iter().map(|(id, _)| id));
            ids.extend(trees.synced().iter().map(|(id, _)| id));
            ids.extend(trees.remote().iter().map(|(id, _)| id));
            for id in ids {
                let on_server = (nodes.iter())
                    .find(|(other, node)| other == id && !node.trashed)
                    .map(|(_, node)| &node.shape);
                let local = trees.local().get(id).map(|node| &node.shape);
                for (tree, shape) in [
                    ('L', local),
                    ('S', trees.synced().live(id)),
                    ('R', trees.remote().live(id)),
                ] {
                    if !same(shape, on_server) {
                        return fail(format!(
                            "{tree} has {shape:?} for node {id} and the server has {on_server:?}"
                        ));
                    }
                }
            }
        }
        let named: BTreeSet<&str> = self.named.iter().flatten().copied().collect();
        let kept = (nodes.iter()).find(|(_, node)| {
            let name = node.shape.name.as_str();
            !node.trashed
                && pending_stem(name).is_some()
                && !named.iter().any(|named| same_name(named, name))
        });
        if let Some((id, node)) = kept {
            return Err(Failure::Diverged(format!(
                "node {id} keeps the pending name {:?}, which no user gave to anything",
                node.shape.name
            )));
        }
        let mut files: BTreeMap<i64, Vec<&Vec<String>>> = BTreeMap::new();
        for (path, content) in &server {
            if let Some(content) = content {
                files.entry(content.mtime_ns).or_default().push(path);
            }
        }
        let allowed = |write: i64| {
            let Some(exposure) = self.exposed.get(&write) else {
                return 1;
            };
            let files = (exposure.nodes.iter()).map(|(id, (devices, rolled_back))| {
                if *rolled_back || exposure.replaced.contains(id) {
                    (*devices).max(1)
                } else {
                    1
                }
            });
            (exposure.loose + files.sum::<usize>()).max(1)
        };
        let lineages = |write: i64, paths: usize| {
            if self.exposed.contains_key(&write) {
                return paths;
            }
            let holders: BTreeSet<&NodeId> = (nodes.iter())
                .filter(|(_, node)| !node.trashed)
                .filter(|(_, node)| {
                    (node.shape.content.as_ref()).is_some_and(|c| c.mtime_ns == write)
                })
                .map(|(id, _)| id)
                .collect();
            let roots: BTreeSet<&NodeId> = (holders.iter())
                .map(|&holder| match self.recovery_copies.get(holder) {
                    Some(copy)
                        if copy.permitted()
                            && copy.write == write
                            && !holders.contains(&copy.source) =>
                    {
                        &copy.source
                    }
                    _ => holder,
                })
                .collect();
            paths.saturating_sub(holders.len() - roots.len())
        };
        let twice = |(write, paths): &(&i64, &Vec<_>)| {
            lineages(**write, paths.len()) > allowed(**write) && !self.copied.contains(*write)
        };
        if let Some((write, paths)) = files.iter().find(twice) {
            return Err(Failure::Diverged(format!(
                "write {write} is in more than one file on the server, and no user copied it: {paths:?}"
            )));
        }
        let forbidden = (nodes.iter()).find_map(|(id, node)| {
            let copy = self
                .recovery_copies
                .get(id)
                .filter(|copy| !copy.permitted())?;
            let holds = (node.shape.content.as_ref()).is_some_and(|c| c.mtime_ns == copy.write);
            let excused =
                self.exposed.contains_key(&copy.write) || self.copied.contains(&copy.write);
            (!node.trashed && holds && !excused).then_some((&node.shape.name, copy))
        });
        if let Some((name, copy)) = forbidden {
            return Err(Failure::Diverged(format!(
                "write {} is in the recovery copy {name:?} of node {}, but device {}'s own commit {} shows the server kept it at {:?}",
                copy.write, copy.source, copy.device, copy.bound, copy.versions
            )));
        }
        let live = live_writes(&self.space);
        let same_bytes_on_server = |write: i64, writer: usize| {
            (self.devices[writer].engine.trees.local().iter()).any(|(id, node)| {
                let Some(local) = (node.shape.content.as_ref()).filter(|c| c.mtime_ns == write)
                else {
                    return false;
                };
                (nodes.iter()).any(|(other, remote)| {
                    other == id
                        && !remote.trashed
                        && (remote.shape.content.as_ref()).is_some_and(|c| same_content(c, local))
                })
            })
        };
        let kept_by_a_departed_writer = |write: i64, writer: usize| {
            let device = &self.devices[writer];
            device.left
                && (device.disk.entries().values())
                    .any(|entry| (entry.content.as_ref()).is_some_and(|c| c.mtime_ns == write))
        };
        let lost = (self.writers.iter()).find(|(write, writer)| {
            !live.contains_key(write)
                && !self.published.contains(write)
                && !self.removed[**writer].contains(write)
                && !self.discarded.contains(write)
                && !self.binned_copies.contains(write)
                && !same_bytes_on_server(**write, **writer)
                && !kept_by_a_departed_writer(**write, **writer)
        });
        match lost {
            Some((write, writer)) => Err(Failure::Diverged(format!(
                "write {write} of device {writer} was never in the server's tree, and its user never \
                 overwrote or deleted it"
            ))),
            None => Ok(()),
        }
    }
}
fn made_up_name<'a>(
    made_up: &[(usize, &'a Move)],
    answer: &CommitResult,
    held: &[NodeState],
    space: &SpaceDo,
) -> Option<&'a Move> {
    let position = |node: &NodeState| (node.parent_id.clone(), node.name.clone(), node.status);
    made_up
        .iter()
        .find(|(index, moved)| {
            let after = space.node(&moved.node_id);
            let before = held.iter().find(|node| node.node_id == moved.node_id);
            let refused = (answer.results.get(*index))
                .is_some_and(|result| result.code() == op_result::Code::Superseded)
                && before.map(position) == after.as_ref().map(position);
            !refused && after.is_some_and(|node| node.status == NodeStatus::Live as i32)
        })
        .map(|(_, moved)| *moved)
}

#[cfg(test)]
mod tests {
    use koidrive_engine::effects::{Base, Displaced, Location};
    use koidrive_engine::tree::{Kind, Shape};

    use super::*;

    #[test]
    fn a_move_answered_superseded_that_still_moved_the_node_is_a_made_up_name() {
        let mut space = SpaceDo::new(1, 0);
        let session = Session {
            device_id: "dev-a".into(),
            user_id: "user".into(),
            device_name: "Laptop A".into(),
            role: Role::Owner,
            max_format: 1,
        };
        let mut ids = koidrive_engine::ids::IdGen::new(2);
        let root = space.space_id().to_string();
        let dir = ids.uuid_v7(1);
        let mut commit = |space: &mut SpaceDo, op: op::Op| {
            let commit = Commit {
                commit_id: ids.uuid_v7(1),
                base_seq: space.head_seq(),
                ops: vec![koidrive_proto::v1::Op { op: Some(op) }],
                history_id: space.history_id().into(),
                device_seq: space.device_seq(&session.device_id) + 1,
                ..Default::default()
            };
            space.commit(&session, 0, &commit).unwrap().result
        };
        commit(
            &mut space,
            op::Op::CreateDir(koidrive_proto::v1::CreateDir {
                node_id: dir.clone(),
                parent_id: root.clone(),
                name: "a".into(),
                pending_for: String::new(),
            }),
        );
        let held = space.nodes();
        let moved = Move {
            node_id: dir,
            new_parent_id: root,
            new_name: "b".into(),
            pending_for: String::new(),
            from: None,
        };
        let mut answer = commit(&mut space, op::Op::Move(moved.clone()));
        let made_up = [(0, &moved)];
        assert_eq!(made_up_name(&made_up, &answer, &held, &space), Some(&moved));
        answer.results[0].code = op_result::Code::Superseded as i32;
        assert_eq!(made_up_name(&made_up, &answer, &held, &space), Some(&moved));
        assert_eq!(
            made_up_name(&made_up, &answer, &space.nodes(), &space),
            None
        );
    }

    #[test]
    fn a_restore_does_not_reopen_a_write_already_missing_from_retained_history() {
        let mut world = World::new(2);
        world.step(0, &Step::Backup).unwrap();
        world
            .phase(
                0,
                &[(
                    0,
                    Step::User(Action {
                        kind: 1,
                        target: 0,
                        parent: 0,
                        name: 0,
                        text: 0,
                    }),
                )],
            )
            .unwrap();
        world.restore(false);
        let exposed = world.exposed[&1].nodes.clone();
        world.devices[1]
            .disk
            .create(Place::Root, "b", Some(content(0, 0, 1)))
            .unwrap();
        world.step(0, &Step::Backup).unwrap();
        world.restore(false);
        assert!(world.exposed[&1].nodes == exposed);
        assert_eq!(world.exposed[&1].loose, 0);
    }

    #[test]
    fn an_edit_in_flight_when_the_server_reports_a_loss_stays_an_edit_of_its_file_after_any_refusal()
     {
        for code in [error::Code::UnsupportedVersion, error::Code::LimitExceeded] {
            let u = |kind, target, name, text| {
                Step::User(Action {
                    kind,
                    target,
                    parent: 0,
                    name,
                    text,
                })
            };
            let mut world = World::new(2);
            world
                .phase(0, &[(0, u(1, 0, 0, 0)), (0, u(1, 0, 2, 1))])
                .unwrap();
            world.run(1, &[(0, Step::Backup)]).unwrap();
            world
                .phase(1, &[(1, u(2, 0, 0, 2)), (1, u(2, 1, 0, 3))])
                .unwrap();
            world
                .run(
                    2,
                    &[
                        (0, u(2, 0, 0, 3)),
                        (0, u(2, 1, 0, 2)),
                        (0, Step::Scan),
                        (0, Step::Commit(Reply::Refused(code))),
                    ],
                )
                .unwrap();
            assert_eq!(world.devices[0].engine.commits.edited_in_flight().len(), 2);
            let link = world.devices[0].link.take();
            world.restore(false);
            world.devices[0].link = link;
            world.devices[0].engine.server_behind(Vec::new());
            world.devices[0].persist().unwrap();
            let answer = world.devices[0]
                .link
                .as_ref()
                .unwrap()
                .inbox
                .iter()
                .position(|m| matches!(m, Message::Answer { .. }))
                .unwrap();
            world.deliver(0, answer).unwrap();
            assert!(world.devices[0].engine.commits.in_flight().is_none());
            let listed = listing(&world.space);
            world.devices[0].link.as_mut().unwrap().inbox.push(listed);
            let at = world.devices[0].link.as_ref().unwrap().inbox.len() - 1;
            world.deliver(0, at).unwrap();
            world.commit(0, Reply::Served).unwrap();
            let ops = &world.devices[0]
                .engine
                .commits
                .in_flight()
                .unwrap()
                .commit
                .ops;
            assert!(
                ops.iter()
                    .all(|op| matches!(op.op, Some(op::Op::UpdateContent(_)))),
                "{code:?}: {ops:?}"
            );
        }
    }

    #[test]
    fn a_rejected_move_with_an_applied_edit_does_not_record_a_placement() {
        let mut world = World::new(1);
        let action = |kind, name| {
            Step::User(Action {
                kind,
                target: 0,
                parent: 0,
                name,
                text: 0,
            })
        };
        world.phase(0, &[(0, action(1, 0))]).unwrap();
        let placed = world.placements[0].clone();
        world
            .run(1, &[(0, action(2, 0)), (0, action(3, 2)), (0, Step::Scan)])
            .unwrap();
        world.commit(0, Reply::Served).unwrap();
        let mut state = world.devices[0].engine.commits.state();
        let flight = state.in_flight.as_mut().unwrap();
        let moved = flight
            .commit
            .ops
            .iter_mut()
            .find_map(|op| match &mut op.op {
                Some(op::Op::Move(moved)) => Some(moved),
                _ => None,
            })
            .unwrap();
        moved.new_parent_id = "00000000-0000-7000-8000-000000000001".into();
        world.devices[0].engine.commits = commit::Commits::from_state(state).unwrap();
        world.commit(0, Reply::Served).unwrap();
        let node = world
            .space
            .nodes()
            .into_iter()
            .find(|node| node.name == "a")
            .unwrap();
        assert_eq!(node.current.unwrap().content.unwrap().mtime_ns, 2);
        assert!(world.placements[0] == placed);
    }

    #[test]
    fn a_restore_matches_placement_commits_even_when_their_sequence_is_reused() {
        let mut world = World::new(2);
        let action = |kind, name| {
            Step::User(Action {
                kind,
                target: 0,
                parent: 0,
                name,
                text: 0,
            })
        };
        world.phase(0, &[(0, action(0, 0))]).unwrap();
        world.step(0, &Step::Backup).unwrap();
        world.phase(1, &[(0, action(3, 2))]).unwrap();
        world.step(0, &Step::Backup).unwrap();
        let head = world.space.head_seq();
        let token = world.space.token_at(head).unwrap();
        world.restore(true);
        world.phase(2, &[]).unwrap();
        assert_eq!(world.space.head_seq(), head);
        assert_ne!(world.space.token_at(head).unwrap(), token);
        let node = world
            .space
            .nodes()
            .into_iter()
            .find(|node| node.name == "b")
            .unwrap()
            .node_id;
        world.restore(false);
        assert_eq!(
            world.restored_names[0],
            BTreeMap::from([(node, BTreeSet::from(["b".into()]))])
        );
        assert!(world.restored_names[1].is_empty());
        world.phase(3, &[]).unwrap();
        assert!(world.restored_names.iter().all(BTreeMap::is_empty));
        world.restore(false);
        assert!(world.restored_names.iter().all(BTreeMap::is_empty));
    }

    #[test]
    fn a_recovery_copy_shares_its_source_lineage_only_for_the_write_it_copied() {
        let mut world = World::new(1);
        let disk = &mut world.devices[0].disk;
        let copied = content(0, 0, 1);
        let source = (disk.create(Place::Root, "a", Some(copied.clone()))).unwrap();
        let copies = ["b", "c.txt"]
            .map(|name| (disk.create(Place::Root, name, Some(copied.clone()))).unwrap());
        disk.overwrite(source, content(0, 1, 2)).unwrap();
        let mut steps = vec![
            (0, Step::Scan),
            (0, Step::Commit(Reply::Served)),
            (0, Step::Commit(Reply::Served)),
        ];
        steps.extend([(0, Step::Deliver(0)); 8]);
        world.run(0, &steps).unwrap();
        let id = |name: &str| {
            (world.space.nodes().into_iter())
                .find(|node| node.name == name)
                .unwrap()
                .node_id
        };
        let source = id("a");
        for name in ["b", "c.txt"] {
            let copy = RecoveryCopy {
                source: source.clone(),
                write: 1,
                device: 0,
                bound: 0,
                versions: vec![1],
            };
            world.recovery_copies.insert(id(name), copy);
        }
        world.phase(0, &[]).unwrap();
        for ino in copies {
            (world.devices[0].disk)
                .overwrite(ino, content(0, 2, 7))
                .unwrap();
        }
        world.removed[0].insert(1);
        let failure = world.phase(1, &[]).unwrap_err();
        assert!(
            failure
                .to_string()
                .contains("write 7 is in more than one file"),
            "{failure}"
        );
    }

    #[test]
    fn a_recovery_copy_of_a_write_its_device_knows_the_server_kept_is_refused() {
        let mut world = World::new(1);
        let disk = &mut world.devices[0].disk;
        let source = (disk.create(Place::Root, "a", Some(content(0, 0, 1)))).unwrap();
        (disk.create(Place::Root, "b", Some(content(0, 0, 1)))).unwrap();
        disk.overwrite(source, content(0, 1, 2)).unwrap();
        let mut steps = vec![
            (0, Step::Scan),
            (0, Step::Commit(Reply::Served)),
            (0, Step::Commit(Reply::Served)),
        ];
        steps.extend([(0, Step::Deliver(0)); 8]);
        world.run(0, &steps).unwrap();
        let id = |name: &str| {
            (world.space.nodes().into_iter())
                .find(|node| node.name == name)
                .unwrap()
                .node_id
        };
        let copy = RecoveryCopy {
            source: id("a"),
            write: 1,
            device: 0,
            bound: 2,
            versions: vec![1],
        };
        world.recovery_copies.insert(id("b"), copy);
        let failure = world.phase(0, &[]).unwrap_err();
        assert!(
            failure
                .to_string()
                .contains(r#"write 1 is in the recovery copy "b""#),
            "{failure}"
        );
    }

    #[test]
    fn only_a_restore_that_brings_back_commits_a_device_lost_lets_the_server_count_start_a_recovery()
     {
        let mut world = World::new(1);
        world.step(0, &Step::Backup).unwrap();
        (world.devices[0].disk)
            .create(Place::Root, "a", Some(content(0, 0, 1)))
            .unwrap();
        world.phase(0, &[]).unwrap();
        world.step(0, &Step::Backup).unwrap();
        world.restore(false);
        let recovery = |world: &mut World, trigger| world.recover(0, trigger, "test".into());
        let refused = recovery(&mut world, RecoveryTrigger::StoreRolledBack);
        assert!(matches!(refused, Err(Failure::Broken(_))), "{refused:?}");
        world.restore(true);
        world.restore(false);
        let refused = recovery(&mut world, RecoveryTrigger::SpaceRewound);
        assert!(matches!(refused, Err(Failure::Broken(_))), "{refused:?}");
        recovery(&mut world, RecoveryTrigger::StoreRolledBack).unwrap();
    }

    #[test]
    fn a_copy_bound_counts_only_commits_from_before_the_last_restore() {
        let held = BTreeSet::from([2, 5]);
        assert_eq!(copy_bound(&held, None), 5);
        assert_eq!(copy_bound(&held, Some(3)), 2);
        assert_eq!(copy_bound(&held, Some(1)), 0);
        let mut world = World::new(1);
        (world.devices[0].disk)
            .create(Place::Root, "a", Some(content(0, 0, 1)))
            .unwrap();
        world.phase(0, &[]).unwrap();
        world.step(0, &Step::Backup).unwrap();
        world.restore(false);
        (world.devices[0].disk)
            .create(Place::Root, "b", Some(content(0, 1, 2)))
            .unwrap();
        world.phase(1, &[]).unwrap();
        let held = world.held_commits[0].clone();
        assert_eq!(held.len(), 2, "{held:?}");
        let before = *held.first().unwrap();
        assert_eq!(copy_bound(&held, world.restored_to), before);
    }

    #[test]
    fn a_write_a_restore_took_is_excused_only_in_the_trash_of_the_device_whose_person_discarded_it()
    {
        let mut world = World::new(2);
        let write = content(0, 1, 5);
        let disk = &mut world.devices[0].disk;
        let ino = disk.create(Place::Root, "a", Some(write.clone())).unwrap();
        let record = EffectRecord {
            effect_id: "removal".into(),
            node: "a".into(),
            counter: 1,
            effect: Effect::RemoveFile {
                displaced: Displaced {
                    at: Location::At {
                        base: Base::Root,
                        dirs: Vec::new(),
                        name: "a".into(),
                    },
                    identity: disk::identity(ino),
                    content: write,
                },
            },
            after: None,
            recoveries: 0,
        };
        disk.apply(&record, Some(Cut::Done)).unwrap();
        assert!(disk.binned(5));
        world.exposed.insert(5, Exposure::default());
        let lost = |world: &World| {
            let kept = world.kept();
            assert!(
                matches!(&kept, Err(Failure::Broken(why)) if why.starts_with("a restore took write 5")),
                "{kept:?}"
            );
        };
        lost(&world);
        world.discards[1].insert(5);
        lost(&world);
        world.discards[0].insert(5);
        world.kept().unwrap();
    }

    #[test]
    fn restoring_a_newer_backup_restores_its_own_commit_bound() {
        let mut world = World::new(2);
        world.step(0, &Step::Backup).unwrap();
        world.devices[0]
            .disk
            .create(Place::Root, "a", None)
            .unwrap();
        world.phase(0, &[]).unwrap();
        let held = world.held_commits.clone();
        assert!(held[0].last().is_some());
        world.step(0, &Step::Backup).unwrap();
        world.restore(true);
        assert!(world.held_commits[0].is_empty());
        world.restore(false);
        assert_eq!(world.held_commits, held);
    }

    #[test]
    fn a_completed_phase_retires_only_live_devices_rollback_evidence() {
        let mut world = World::new(2);
        world.step(0, &Step::CopyStore).unwrap();
        world.step(0, &Step::RollBack).unwrap();
        world.devices[1].rolled_back = true;
        world.devices[1]
            .engine
            .commits
            .enter_recovery(RecoveryCause::of(RecoveryTrigger::StoreRolledBack));
        world.recovered();
        assert!(!world.devices[0].rolled_back);
        assert!(world.devices[1].rolled_back);
        assert!(matches!(
            world.recover(
                0,
                RecoveryTrigger::StoreRolledBack,
                "spurious recovery".into()
            ),
            Err(Failure::Broken(_))
        ));
    }

    #[test]
    fn a_stuck_pending_name_no_user_gave_fails_convergence() {
        let mut world = World::new(2);
        world.devices[0]
            .disk
            .create(Place::Root, "a (pending)", None)
            .unwrap();
        let stuck = world.quiesce().unwrap_err();
        assert!(matches!(&stuck, Failure::Diverged(why) if why.contains("keeps the pending name")));
    }

    /// A write a restore took from another file does not excuse an overwrite of a's change back
    /// to its rolled-back bytes.
    #[test]
    fn a_resend_over_a_revert_comes_from_the_rolled_back_file() {
        let (old, unseen, incoming) = (content(0, 0, 1), content(0, 0, 99), content(1, 1, 2));
        let at = Location::At {
            base: Base::Root,
            dirs: Vec::new(),
            name: "a".into(),
        };
        let mut disk = Disk::default();
        let ino = disk.create(Place::Root, "a", Some(unseen.clone())).unwrap();
        let record = EffectRecord {
            effect_id: "download".into(),
            node: "a".into(),
            counter: 1,
            effect: Effect::WriteFile {
                at: at.clone(),
                content: incoming.clone(),
                replaces: Some(Displaced {
                    at,
                    identity: disk::identity(ino),
                    content: unseen,
                }),
            },
            after: Some(Shape {
                parent: "root".into(),
                name: "a".into(),
                kind: Kind::File,
                content: Some(incoming),
            }),
            recoveries: 0,
        };
        let applied = disk.apply(&record, Some(Cut::Done)).unwrap();
        assert!(
            trashed_copy(
                &Held::new(),
                &BTreeSet::new(),
                None,
                &[],
                0,
                &record,
                &applied
            )
            .is_err()
        );
        let rolled_back = BTreeMap::from([("a".into(), old.manifest_digest)]);
        for file in ["a", "b"] {
            let exposed = BTreeMap::from([(
                2,
                Exposure {
                    nodes: BTreeMap::from([(file.into(), (1, true))]),
                    ..Exposure::default()
                },
            )]);
            let accepted = resent_over_revert(&exposed, &rolled_back, &record, &applied);
            assert_eq!(accepted, file == "a", "write 2 exposed at {file}");
        }
    }

    #[test]
    fn a_trashed_write_is_a_copy_only_of_synced_bytes_the_server_keeps() {
        let (trashed, other) = (content(0, 1, 11), content(0, 2, 12));
        let record = EffectRecord {
            effect_id: "download".into(),
            node: "a".into(),
            counter: 1,
            effect: Effect::WriteFile {
                at: Location::At {
                    base: Base::Root,
                    dirs: Vec::new(),
                    name: "a".into(),
                },
                content: other.clone(),
                replaces: None,
            },
            after: None,
            recoveries: 0,
        };
        let applied = Applied {
            outcome: Ok(None),
            trashed: Some(trashed.clone()),
        };
        let synced = |bytes: &Content| Shape {
            parent: "root".into(),
            name: "a".into(),
            kind: Kind::File,
            content: Some(bytes.clone()),
        };
        let (same, changed) = (synced(&trashed), synced(&other));
        let kept = vec![trashed.manifest_digest.clone()];
        let erased = vec![other.manifest_digest.clone()];
        for (synced, retained, copy) in [
            (Some(&same), &kept, true),
            (Some(&same), &erased, false),
            (Some(&changed), &kept, false),
            (None, &kept, false),
        ] {
            let found = trashed_copy(
                &Held::new(),
                &BTreeSet::new(),
                synced,
                retained,
                0,
                &record,
                &applied,
            );
            assert_eq!(
                found.ok(),
                copy.then_some(Some(11)),
                "{synced:?} {retained:?}"
            );
        }
    }

    #[test]
    fn a_write_in_the_local_trash_is_kept_only_as_a_copy_of_bytes_the_server_held() {
        let mut world = World::new(1);
        let write = content(0, 1, 5);
        let disk = &mut world.devices[0].disk;
        let ino = disk.create(Place::Root, "a", Some(write.clone())).unwrap();
        let record = EffectRecord {
            effect_id: "removal".into(),
            node: "a".into(),
            counter: 1,
            effect: Effect::RemoveFile {
                displaced: Displaced {
                    at: Location::At {
                        base: Base::Root,
                        dirs: Vec::new(),
                        name: "a".into(),
                    },
                    identity: disk::identity(ino),
                    content: write,
                },
            },
            after: None,
            recoveries: 0,
        };
        disk.apply(&record, Some(Cut::Done)).unwrap();
        assert!(disk.binned(5));
        world.writers.insert(5, 0);
        let kept = world.kept();
        assert!(
            matches!(&kept, Err(Failure::Broken(why)) if why.starts_with("write 5 left")),
            "{kept:?}"
        );
        let converged = world.converged();
        assert!(
            matches!(&converged, Err(Failure::Diverged(why)) if why.starts_with("write 5 of device 0")),
            "{converged:?}"
        );
        world.binned_copies.insert(5);
        world.kept().unwrap();
        world.converged().unwrap();
    }

    #[test]
    fn a_reload_keeps_an_effect_planned_after_a_recovery_applicable() {
        let mut engine = Engine::new("root", "history", 1);
        let cause = RecoveryCause::rewind(4, NOW);
        (engine.commits).recover(
            &mut engine.trees,
            0,
            &cause,
            &History::new(),
            &Marks::new(),
            &mut engine.ids,
            NOW as u64,
        );
        engine.trees.rules_scanned();
        let folder = RemoteNode {
            shape: Shape {
                parent: "root".into(),
                name: "d".into(),
                kind: Kind::Dir,
                content: None,
            },
            trashed: false,
            content_rev: 1,
            edit_rev: 1,
            meta_rev: 1,
            meta_edit_rev: 1,
            trash_group: String::new(),
            version_id: String::new(),
            pending_for: None,
            pending_by: None,
        };
        engine.trees.set_remote("d".into(), folder);
        engine.trees.settle();
        let planned = engine
            .effects
            .plan(
                &engine.trees,
                None,
                &mut engine.ids,
                NOW as u64,
                BatchLimit::ONE,
            )
            .to_vec();
        assert!(planned[0].applies(&engine.trees, None), "{planned:?}");
        let loaded = engine.reload().unwrap();
        assert!(loaded.effects.in_flight()[0].applies(&loaded.trees, None));
    }

    fn send(world: &mut World, d: usize) -> Result<(), Failure> {
        let mut steps = vec![
            (d, Step::Scan),
            (d, Step::Commit(Reply::Served)),
            (d, Step::Commit(Reply::Served)),
        ];
        steps.extend([(d, Step::Deliver(0)); 8]);
        world.run(0, &steps)
    }

    #[test]
    fn a_revert_to_bytes_a_restore_erased_from_the_server_is_not_excused_from_the_trash() {
        let user = |kind, text| {
            Step::User(Action {
                kind,
                target: 0,
                parent: 0,
                name: 0,
                text,
            })
        };
        let mut world = World::new(1);
        world.phase(0, &[(0, user(1, 1))]).unwrap();
        world.run(0, &[(0, Step::Backup)]).unwrap();
        world.phase(1, &[(0, user(2, 0))]).unwrap();
        let node = live_writes(&world.space).get(&2).unwrap().clone();
        let a = content(0, 0, 4);
        let save = |world: &mut World, text, write| {
            let disk = &mut world.devices[0].disk;
            let ino = disk.child(&Place::Root, "a").unwrap();
            let old = disk.overwrite(ino, content(0, text, write)).unwrap();
            world.removed[0].insert(old.mtime_ns);
            world.writers.insert(write, 0);
            world.writes = write;
        };
        save(&mut world, 1, 3);
        world.phase(2, &[(0, Step::Restore)]).unwrap();
        let versions = world
            .space
            .list_versions(&ListVersions {
                node_id: node.clone(),
                ..Default::default()
            })
            .unwrap();
        assert!(versions.versions.iter().all(|v| {
            v.content
                .as_ref()
                .is_none_or(|c| c.manifest_digest != a.manifest_digest)
        }));
        assert_eq!(world.held[&node][&2], a.manifest_digest);
        world.kept().unwrap();
        world.converged().unwrap();
        save(&mut world, 0, 4);
        world.scan(0).unwrap();
        let local = world.devices[0].engine.trees.local().get(&node).unwrap();
        let synced = world.devices[0].engine.trees.synced().live(&node).unwrap();
        assert!(!same_content(
            local.shape.content.as_ref().unwrap(),
            synced.content.as_ref().unwrap()
        ));
        assert!(!world.removed[0].contains(&4));
        assert!(!world.published.contains(&4));
        assert!(!world.discarded.contains(&4));
        let after = world.devices[0]
            .engine
            .trees
            .remote()
            .get(&node)
            .unwrap()
            .shape
            .clone();
        let b = after.content.clone().unwrap();
        let disk = &mut world.devices[0].disk;
        let ino = disk.child(&Place::Root, "a").unwrap();
        let at = Location::At {
            base: Base::Root,
            dirs: vec![],
            name: "a".into(),
        };
        let record = EffectRecord {
            effect_id: "review-bad-download".into(),
            node: node.clone(),
            counter: disk.marker() + 1,
            effect: Effect::WriteFile {
                at: at.clone(),
                content: b,
                replaces: Some(Displaced {
                    at,
                    identity: disk::identity(ino),
                    content: a,
                }),
            },
            after: Some(after),
            recoveries: 0,
        };
        assert!(record.applies(&world.devices[0].engine.trees, None));
        let mut state = world.devices[0].engine.effects.state();
        state.in_flight = vec![record];
        world.devices[0].engine.effects =
            koidrive_engine::effects::Effects::from_state(state).unwrap();
        world.devices[0].store = world.devices[0].engine.clone();
        let effect = world.effect(0, Outcome::Run);
        assert!(
            matches!(&effect, Err(Failure::Broken(why)) if why.contains("removed write 4")),
            "{effect:?}"
        );
    }

    fn connect(world: &mut World, d: usize) {
        let mut steps = vec![(d, Step::Connect)];
        steps.extend([(d, Step::Deliver(0)); 8]);
        world.run(0, &steps).unwrap();
    }

    #[test]
    fn deleting_a_concurrent_later_write_does_not_excuse_removing_an_unseen_earlier_one() {
        let user = |kind, text| {
            Step::User(Action {
                kind,
                target: 0,
                parent: 0,
                name: 0,
                text,
            })
        };
        let mut world = World::new(2);
        world.phase(0, &[(0, user(1, 0))]).unwrap();
        (world.run(1, &[(0, Step::Backup), (0, user(2, 1)), (1, user(2, 2))])).unwrap();
        send(&mut world, 1).unwrap();
        assert_eq!(
            live_writes(&world.space).into_keys().collect::<Vec<_>>(),
            [3]
        );
        world.run(1, &[(1, user(4, 0))]).unwrap();
        send(&mut world, 1).unwrap();
        assert!(world.removed[1].contains(&3));
        assert!(!world.removed[1].contains(&2));
        world.run(2, &[(0, Step::Restore)]).unwrap();
        connect(&mut world, 0);
        send(&mut world, 0).unwrap();
        assert_eq!(
            live_writes(&world.space).into_keys().collect::<Vec<_>>(),
            [2]
        );
        connect(&mut world, 1);
        let trees = &mut world.devices[1].engine.trees;
        let (id, shape) = (trees.remote().iter())
            .find(|(_, node)| !node.trashed)
            .map(|(id, node)| (id.clone(), node.shape.clone()))
            .unwrap();
        assert_eq!(shape.content.as_ref().unwrap().mtime_ns, 2);
        assert!(trees.local().get(&id).is_none());
        trees.set_synced(id, Some(shape));
        trees.settle();
        world.devices[1].persist().unwrap();
        let removed = send(&mut world, 1);
        assert!(
            matches!(&removed, Err(Failure::Broken(why)) if why.contains("write 2")),
            "{removed:?}"
        );
    }

    #[test]
    fn a_publish_after_a_rewind_spends_the_write_the_recovery_scan_read() {
        let user = |kind| {
            Step::User(Action {
                kind,
                target: 0,
                parent: 0,
                name: 0,
                text: 0,
            })
        };
        let mut world = World::new(2);
        world.phase(0, &[(0, user(1))]).unwrap();
        world
            .run(
                1,
                &[
                    (0, Step::Backup),
                    (1, user(2)),
                    (0, Step::Rewind),
                    (1, Step::Connect),
                    (1, Step::Deliver(0)),
                    (1, Step::Answer(0)),
                    (1, Step::Commit(Reply::Served)),
                    (1, Step::Commit(Reply::Served)),
                    (1, Step::Deliver(0)),
                ],
            )
            .unwrap();
        assert!(world.seen.contains_key("recovery classifies the trees"));
        assert!(world.spent.iter().any(|spent| spent.device == 1));
        assert!(world.touches[1].values().all(BTreeSet::is_empty));
    }

    #[test]
    fn a_stale_upload_over_a_restore_to_an_earlier_write_of_the_same_bytes_is_unasked() {
        fn user(kind: u8, text: u8) -> Step {
            Step::User(Action {
                kind,
                target: 0,
                parent: 0,
                name: 0,
                text,
            })
        }
        fn work(world: &mut World, d: usize) {
            let mut steps = vec![(d, Step::Connect)];
            steps.extend([(d, Step::Deliver(0)); 8]);
            steps.extend([(d, Step::Work(0)); 16]);
            world.run(2, &steps).unwrap();
        }
        let mut world = World::new(3);
        world.phase(0, &[(0, user(1, 1))]).unwrap();
        world.phase(1, &[(1, user(2, 0))]).unwrap();
        world.run(2, &[(0, user(2, 1))]).unwrap();
        work(&mut world, 0);
        world.run(2, &[(0, Step::Backup)]).unwrap();
        work(&mut world, 1);
        world.run(2, &[(1, user(2, 0))]).unwrap();
        work(&mut world, 1);
        work(&mut world, 0);
        world.run(2, &[(0, user(2, 1))]).unwrap();
        work(&mut world, 0);
        world.run(2, &[(0, Step::Restore)]).unwrap();
        connect(&mut world, 2);
        let trees = &mut world.devices[2].engine.trees;
        let (id, shape) = (trees.remote().iter())
            .find(|(_, node)| !node.trashed)
            .map(|(id, node)| (id.clone(), node.shape.clone()))
            .unwrap();
        assert_eq!(shape.content.as_ref().unwrap().mtime_ns, 3);
        assert_eq!(
            trees
                .synced()
                .live(&id)
                .unwrap()
                .content
                .as_ref()
                .unwrap()
                .mtime_ns,
            2
        );
        trees.set_synced(id, Some(shape));
        trees.settle();
        world.devices[2].persist().unwrap();
        send(&mut world, 2).unwrap();
        let stale = world.quiesce();
        assert!(
            matches!(&stale, Err(Failure::Diverged(why)) if why.contains("took write 3")),
            "{stale:?}"
        );
    }

    #[test]
    fn a_stale_upload_is_unasked_after_an_earlier_restore_republished_its_write() {
        fn user(kind: u8, text: u8) -> Step {
            Step::User(Action {
                kind,
                target: 0,
                parent: 0,
                name: 0,
                text,
            })
        }
        fn work(world: &mut World, d: usize) {
            let mut steps = vec![(d, Step::Connect)];
            steps.extend([(d, Step::Deliver(0)); 8]);
            steps.extend([(d, Step::Work(0)); 16]);
            world.run(3, &steps).unwrap();
        }
        let mut world = World::new(3);
        world.phase(0, &[(0, user(1, 1))]).unwrap();
        world.run(1, &[(0, Step::Backup)]).unwrap();
        world.phase(1, &[(1, user(2, 0))]).unwrap();
        world.phase(2, &[(0, Step::Restore)]).unwrap();
        world.run(3, &[(0, user(2, 1))]).unwrap();
        work(&mut world, 0);
        world.run(3, &[(0, Step::Backup)]).unwrap();
        work(&mut world, 1);
        world.run(3, &[(1, user(2, 0))]).unwrap();
        work(&mut world, 1);
        work(&mut world, 0);
        world.run(3, &[(0, user(2, 1))]).unwrap();
        work(&mut world, 0);
        world.run(3, &[(0, Step::Restore)]).unwrap();
        connect(&mut world, 2);
        let trees = &mut world.devices[2].engine.trees;
        let (id, shape) = (trees.remote().iter())
            .find(|(_, node)| !node.trashed)
            .map(|(id, node)| (id.clone(), node.shape.clone()))
            .unwrap();
        trees.set_synced(id, Some(shape));
        trees.settle();
        world.devices[2].persist().unwrap();
        send(&mut world, 2).unwrap();
        let stale = world.quiesce();
        assert!(
            matches!(&stale, Err(Failure::Diverged(why)) if why.contains("took write")),
            "{stale:?}"
        );
    }

    #[test]
    fn a_move_away_and_back_settled_while_another_device_moves_the_file_leaves_no_touch() {
        let user = |kind, name| {
            Step::User(Action {
                kind,
                target: 0,
                parent: 0,
                name,
                text: 0,
            })
        };
        let mut world = World::new(2);
        world.phase(0, &[(0, user(1, 0))]).unwrap();
        world.run(1, &[(1, user(3, 3))]).unwrap();
        send(&mut world, 1).unwrap();
        world
            .run(1, &[(0, user(3, 2)), (0, user(3, 0)), (0, user(2, 0))])
            .unwrap();
        send(&mut world, 0).unwrap();
        world.phase(1, &[]).unwrap();
        assert!(
            world.touches[0].values().all(BTreeSet::is_empty),
            "{:?}",
            world.touches[0]
        );
    }

    #[test]
    fn a_move_away_and_back_whose_edit_another_device_already_wrote_leaves_no_touch() {
        let user = |kind, name| {
            Step::User(Action {
                kind,
                target: 0,
                parent: 0,
                name,
                text: 0,
            })
        };
        let mut world = World::new(2);
        world.phase(0, &[(0, user(1, 0))]).unwrap();
        let steps = [
            (0, user(3, 2)),
            (0, user(3, 0)),
            (0, user(2, 0)),
            (0, Step::Scan),
            (0, Step::Commit(Reply::Served)),
        ];
        world.run(1, &steps).unwrap();
        let device = &world.devices[0];
        let ino = device.disk.visible()[0];
        let wanted = device.disk.entries()[&ino].content.clone().unwrap();
        let sent = device.engine.commits.in_flight().unwrap().commit.clone();
        world.run(1, &[(1, user(2, 0))]).unwrap();
        let other = &mut world.devices[1];
        let other_ino = other.disk.visible()[0];
        let mtime_ns = other.disk.entries()[&other_ino]
            .content
            .as_ref()
            .unwrap()
            .mtime_ns;
        let same_bytes = Content {
            mtime_ns,
            ..wanted.clone()
        };
        other.disk.overwrite(other_ino, same_bytes).unwrap();
        send(&mut world, 1).unwrap();
        world.run(1, &[(0, Step::Commit(Reply::Served))]).unwrap();
        let result = world.space.recorded_result(&sent.commit_id).unwrap();
        assert!(result.changes.is_empty());
        assert_eq!(result.results[0].code, op_result::Code::Applied as i32);
        world.run(1, &[(0, Step::Deliver(0)); 8]).unwrap();
        assert!(
            world.touches[0].values().all(BTreeSet::is_empty),
            "{:?}",
            world.touches[0]
        );
    }

    #[test]
    fn an_edit_a_split_commit_sends_after_its_move_brings_back_a_file_another_device_deleted() {
        let user = |kind, name| {
            Step::User(Action {
                kind,
                target: 0,
                parent: 0,
                name,
                text: 0,
            })
        };
        let mut world = World::new(2);
        world.phase(0, &[(0, user(1, 0))]).unwrap();
        let steps = [
            (0, user(3, 2)),
            (0, user(2, 0)),
            (0, Step::Scan),
            (0, Step::Commit(Reply::Served)),
            (0, Step::Commit(Reply::Refused(error::Code::LimitExceeded))),
            (0, Step::Deliver(0)),
            (0, Step::Commit(Reply::Served)),
            (0, Step::Commit(Reply::Served)),
            (0, Step::Deliver(0)),
            (1, Step::Work(0)),
            (1, Step::Work(0)),
            (1, Step::Work(0)),
            (1, Step::Work(0)),
            (1, Step::Work(0)),
            (1, user(4, 0)),
            (1, Step::Scan),
            (1, Step::Commit(Reply::Served)),
            (1, Step::Commit(Reply::Served)),
            (1, Step::Deliver(0)),
        ];
        world.phase(1, &steps).unwrap();
        assert!(live_writes(&world.space).contains_key(&2));
    }

    #[test]
    fn a_move_away_and_back_that_an_edit_commit_settles_leaves_no_touch() {
        let user = |kind, name| {
            Step::User(Action {
                kind,
                target: 0,
                parent: 0,
                name,
                text: 0,
            })
        };
        let mut world = World::new(2);
        world.phase(0, &[(0, user(1, 0))]).unwrap();
        world
            .phase(1, &[(0, user(3, 2)), (0, user(3, 0)), (0, user(2, 0))])
            .unwrap();
        assert!(
            world.touches[0].values().all(BTreeSet::is_empty),
            "{:?}",
            world.touches[0]
        );
    }
}
