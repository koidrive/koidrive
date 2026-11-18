// SPDX-License-Identifier: Apache-2.0

//! The task that syncs one linked folder.
//!
//! [`Folder`] is the folder while it syncs. It owns the engine, the store and the disk, and it is their only
//! writer. Each of its public methods takes one input: a session event, an observation batch, or the result of work
//! it asked for. A method runs its store transactions with no await and returns the [`Work`] to start. After every
//! input it decides what runs next, in the order of the engine's reference loop (`core/sim/tests/engine_sync.rs`):
//! an effect first, then a commit, and never an effect while a commit is in flight.
//!
//! [`FolderTask`] is the shell around it. It checks the root, claims the folder, starts the observer's thread and the
//! session, and then runs one `select!` that hands each input to the folder and runs the returned work as futures.
//! A future owns what it needs and borrows only the session and the transfer client, so none holds the store or
//! the engine across an await.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use koidrive_engine::commit::{Base, Halt as CommitHalt, Reconnect};
use koidrive_engine::first_link::PriorSync;
use koidrive_engine::ignore::LocalRules;
use koidrive_engine::tree::{FileIdentity, LocalNode, NodeId};
use koidrive_fs::{Detached, Refusal, Snapshot};
use koidrive_net::{Backoff, Cause, DisconnectReason, Event, ServerError};
use koidrive_proto::v1::resync_required::Reason;
use koidrive_proto::v1::{Commit, CommitRecord, Resume, TrashPage, TreePage, Welcome};
use koidrive_store::{Cursor, Link, Store, Tx};

use crate::clock;
use crate::disk::{Batch, Disk, Pending, Sections, Settled, Sources, Staged, Stall, Stop, Upload};
use crate::engine::{Applied, Engine, Staging};
use crate::progress::Tracker;
use crate::status::{FolderState, HeldDisk, Wait, WaitKind, describe};
use crate::timing::{Time, Timing};
use koidrive_engine::question::{RecoveryCause, RecoveryTrigger};

mod commit;
mod effect;
mod task;
mod view;
mod work;

#[cfg(test)]
use crate::claim::{self, Identity};
#[cfg(test)]
use crate::disk::{self, Adopted};
#[cfg(test)]
use crate::observe::{FsObserver, Observer, Observing};
#[cfg(test)]
use crate::status::{Activity, HeldDiskKind, Unreachable};
#[cfg(test)]
use commit::sent_in_order;
#[cfg(test)]
use koidrive_net::{CallError, ChunkError, NotResent, StopReason, Transfer, UploadGate};
#[cfg(test)]
use koidrive_proto::v1::error::Code;
#[cfg(test)]
use koidrive_proto::v1::{ChunkRef, CommitResult, Error as ProtoError, UploadReceipt, op_result};
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
pub(crate) use task::FolderTask;
#[cfg(test)]
use task::reporter;
#[cfg(test)]
pub(crate) use task::{BEFORE_OPEN, ObserveFault};
#[cfg(test)]
use tokio::sync::watch;
#[cfg(test)]
use work::{later, upload_chunks, upload_counted};

/// The highest space format this build reads (spec v0 section 9.2).
pub const MAX_FORMAT: u32 = 1;
/// Nodes per `ListTree` page, the most spec v0 section 13 allows.
const TREE_PAGE: u32 = 5_000;
/// How often a folder that failed its root check is checked again.
const ROOT_RECHECK: Duration = Duration::from_secs(30);
/// The first wait before a folder that halted for its root syncs again. Each such halt doubles it, up to
/// [`ROOT_RECHECK`].
const ROOT_AGAIN: Duration = Duration::from_secs(1);
/// How often a running folder reclaims its local trash, after the first time when it starts.
const RECLAIM_TRASH: Duration = Duration::from_secs(24 * 60 * 60);

const RECOUNT_SPACING: u32 = 9;
/// How often a running folder checks that its path still names the folder it locked.
const CLAIM_RECHECK: Duration = Duration::from_secs(2);
/// The wait before a commit is built again after a file changed while its content was uploaded. The observer
/// reads the file again meanwhile.
const BUILD_AGAIN: Duration = Duration::from_secs(1);
const LEFT_OUT_AFTER: u32 = 3;
const PERMISSION_RETRY: Backoff = Backoff {
    initial: Duration::from_secs(30),
    max: Duration::from_secs(1800),
};
/// The waits before a node whose disk effect failed is planned again: from about a second, doubling with each
/// failure of the node in a row, up to ten minutes.
const EFFECT_RETRY: Backoff = Backoff {
    initial: Duration::from_secs(1),
    max: Duration::from_secs(600),
};

/// How long calls of one kind may fail in a row before the status shows them, unless the server asks for a wait at
/// least this long.
const RETRY_SHOWN: Duration = Duration::from_secs(60);
const QUICK_RECONNECT: Duration = Duration::from_secs(5);

/// How many of the items that could not be synced the status names.
const STUCK_SHOWN: usize = 5;

/// Why a folder halts once its path no longer leads to the folder its task locked.
const REPLACED: &str = "the folder at this path is no longer the folder this task locked (it was moved, replaced or \
                        removed)";

/// Why a folder halts when the folder at its path after a root halt is not the one the task locked before.
const OTHER_FOLDER: &str = "the folder at this path is not the one this device synced before (it was replaced); run \
                            koi folder restart to sync the folder that is there now";

/// One linked folder while it syncs.
pub(crate) struct Folder {
    timing: Timing,
    link: Link,
    engine: Engine,
    store: Store,
    disk: Disk,
    stream: Stream,
    flight: Flight,
    sent_commit: Option<String>,
    /// The observer has scanned the folder once. Nothing is planned before, because L does not describe the disk
    /// until then.
    scanned: bool,
    disconnected: Option<Disconnected>,
    /// The history the server reported in its last `Welcome`.
    history_id: String,
    /// The server's count of this device's commits in that history, from the same `Welcome`.
    device_seq: u64,
    head_seq: Option<u64>,
    local_entries: Option<u64>,
    remote_nodes: u64,
    /// The questions in the store that wait for an answer.
    questions: u64,
    /// Commit sends and uploads that failed in a row, for the backoff.
    failures: u32,
    backoff: Backoff,
    unreadable: BTreeMap<String, Unreadable>,
    detached: BTreeMap<PathBuf, Detached>,
    /// The calls that failed and are sent again, by kind, until one of the kind goes through. The status shows
    /// them once they have failed for [`RETRY_SHOWN`], or at once when the wait is that long.
    retrying: BTreeMap<Call, Retrying>,
    /// The disk effects of each node that failed in a row in this task, for [`EFFECT_RETRY`], and why the last one
    /// failed.
    effect_failures: BTreeMap<String, (u32, String)>,
    denied: BTreeMap<String, Denied>,
    /// The nodes whose last effect ended [`Settled::Stuck`]. A [`Work::RetryAfter`] leaves them blocked.
    stuck_effects: BTreeSet<String>,
    /// Why the server did not take a change of each node, for the nodes it may have left out of commits.
    refused_nodes: BTreeMap<String, String>,
    asked: BTreeSet<String>,
    progress: Arc<Mutex<Tracker>>,
    recount_at: Option<Instant>,
    recount_waits: bool,
    unread: (u64, u64),
    other_changes: u64,
    questions_holding_changes: u64,
    /// When the [`Work::RetryAfter`] that is out ends.
    retry_at: Option<Instant>,
    /// Tests stop the task after this many more store transactions, as a crash right after one would.
    #[cfg(test)]
    crash_after: Option<usize>,
}

/// Where R stands against the session's change stream (design §4.4). It is not stored: the next `Welcome` and the
/// store's cursor give it again.
enum Stream {
    /// No `Welcome` yet.
    Connecting,
    /// The tree is being listed for a bootstrap. R is untouched. `superseded` is set when something calls for a new
    /// listing while this one is out, such as a `Welcome` from a server that lost commits: the pages may have been
    /// read from the server before it, so they are dropped and the tree is listed again.
    Listing { superseded: bool },
    /// The listing follows the log until it may replace R.
    Replaying(Staging),
    /// R follows the log. `head` is the server's head as of the last `CaughtUp` of this connection.
    Following { head: Option<u64> },
}

/// What this process is doing with the commit in flight. The commit itself is the engine's and is in the store.
enum Flight {
    /// No commit in flight, or one whose result waits until R has reached it.
    Idle,
    /// The store held a commit in flight when the task started. The server may have it, so it is only ever sent
    /// again unchanged, once a `Welcome` allows that (design §2.8).
    Unknown,
    /// The content the commit adds is being uploaded from these files. The server holds nothing of the commit.
    Uploading(Sources),
    /// `Session::commit` is out with this copy of the commit, which carries the upload's receipts. The session
    /// resends it after a reconnect, so the folder starts no second call.
    Sending(Commit),
    /// A [`Work::ResendAfter`] is out.
    Waiting(Next),
}

/// What follows a wait.
enum Next {
    /// Upload the commit's content again. Only for a commit the server holds nothing of.
    Upload,
    /// Send the same commit again.
    Send(Commit),
    /// The commit was abandoned. Build the next one.
    Build,
}

/// Work a step asks the shell to start. Each owns what it needs.
pub(crate) enum Work {
    /// `Session::subscribe`.
    Subscribe(Resume),
    /// Read every page of the tree, then every page of the trash. Answered with [`Folder::listed`].
    List,
    /// Read on from `Listing` after a wait.
    ListAfter(Duration, Listing),
    Reconnect,
    /// [`Pending::stage`]. Answered with [`Folder::staged`].
    Stage(Pending),
    /// The same after a wait.
    StageAfter(Duration, Pending),
    /// `Transfer::upload` of the chunks a commit adds. Answered with [`Folder::uploaded`].
    Upload(Upload),
    /// `Session::commit`. Answered with [`Folder::sent`].
    Send(Commit),
    /// Answered with [`Folder::resend`] after the wait.
    ResendAfter(Duration),
    /// Answered with [`Folder::retry`] after the wait. A later one replaces it, and always ends earlier.
    RetryAfter(Duration),
}

/// The pages of the tree read so far, the cursor of the next page and how many times in a row a page failed. When
/// `trash` is set, the trash is read after the tree, with its own pages and cursor.
#[derive(Debug, Default)]
pub(crate) struct Listing {
    pages: Vec<TreePage>,
    cursor: String,
    trash_pages: Vec<TrashPage>,
    trash_cursor: String,
    failures: u32,
}

enum NotTaken {
    Refused(ServerError),
    TooLarge { bytes: usize },
}

/// How reading the tree ended.
#[derive(Debug)]
pub(crate) enum Listed {
    Done {
        pages: Vec<TreePage>,
        trash: Vec<TrashPage>,
    },
    /// A page failed for now with `why`. Read on from `listing` after `wait`.
    Later {
        listing: Listing,
        wait: Duration,
        why: String,
        kind: WaitKind,
    },
}

/// The calls a folder sends again after they fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Call {
    List,
    Download,
    Upload,
    Commit,
}

#[derive(Debug)]
struct Denied {
    due: Instant,
    foreseen: bool,
}

struct Unreadable {
    failures: u32,
    why: String,
    left_out: Option<LeftOut>,
}

#[derive(Debug)]
struct LeftOut {
    at: SystemTime,
    due: Instant,
    seen: Option<LocalNode>,
}

/// Calls of one kind that keep failing: since when, the latest failure and when the call is sent again.
#[derive(Debug)]
struct Retrying {
    since: Instant,
    why: String,
    wait: Duration,
    at: SystemTime,
    kind: WaitKind,
    unreachable: Option<Cause>,
}

struct Disconnected {
    reason: DisconnectReason,
    retry_at: SystemTime,
    since: Instant,
    shown: bool,
}

/// The part of a folder's status the task knows.
#[derive(Debug, PartialEq)]
pub(crate) struct View {
    pub state: FolderState,
    pub history_id: Option<String>,
    pub last_seq: Option<u64>,
    pub head_seq: Option<u64>,
    pub remote_nodes: u64,
    pub local_entries: Option<u64>,
    pub problem: Option<String>,
    pub wait: Option<Wait>,
    pub questions: u64,
    pub recovery: Option<RecoveryCause>,
    pub held_disks: Vec<HeldDisk>,
}

impl Folder {
    /// Opens the folder's store, loads the engine and starts the disk through `lock`, the descriptor the task
    /// locked. [`Disk::start`] recovers an effect that was in flight before anything is observed or planned. The
    /// engine then takes the restores asked for while no task ran. Every node whose effect failed is unblocked, so each
    /// is planned again once after the first scan of this start. The trees take `local`, this device's `[sync]` list,
    /// which the store does not hold; the rules files follow with the observer's first batch, and nothing is planned
    /// before that (design §5.10).
    pub fn open(link: Link, lock: &File, store: &Path, local: LocalRules) -> Result<Folder, Stop> {
        let timing = Timing::open(store, link.linked_at_ms);
        let mut store = Store::open_existing(store)?;
        let mut engine = store.transaction(|tx| Engine::load(tx, &link.space_id))?;
        engine.trees.set_local_rules(local);
        let disk = Disk::start(lock, &link, &mut engine, &mut store)?;
        for node in engine.effects.blocked().clone().into_keys() {
            engine.effects.unblock(&node);
        }
        let (remote_nodes, questions) = store.transaction(|tx| {
            engine.accept_restores(tx)?;
            engine.accept_answers(tx)?;
            engine.checkpoint(tx)?;
            Ok::<_, koidrive_store::Error>((tx.remote_node_count()?, unanswered(tx)?))
        })?;
        let flight = match engine.commits.in_flight() {
            Some(flight) if flight.result.is_none() => Flight::Unknown,
            _ => Flight::Idle,
        };
        let sent_commit =
            (engine.commits.in_flight()).map(|flight| flight.commit.commit_id.clone());
        let asked = (engine.commits.questions().iter())
            .map(|question| question.id.clone())
            .collect();
        Ok(Folder {
            timing,
            link,
            engine,
            store,
            disk,
            stream: Stream::Connecting,
            flight,
            sent_commit,
            scanned: false,
            disconnected: None,
            history_id: String::new(),
            device_seq: 0,
            head_seq: None,
            local_entries: None,
            remote_nodes,
            questions,
            failures: 0,
            backoff: Backoff::default(),
            unreadable: BTreeMap::new(),
            detached: BTreeMap::new(),
            retrying: BTreeMap::new(),
            effect_failures: BTreeMap::new(),
            denied: BTreeMap::new(),
            stuck_effects: BTreeSet::new(),
            refused_nodes: BTreeMap::new(),
            asked,
            progress: Arc::default(),
            recount_at: None,
            recount_waits: false,
            unread: (0, 0),
            other_changes: 0,
            questions_holding_changes: 0,
            retry_at: None,
            #[cfg(test)]
            crash_after: None,
        })
    }

    /// What the observer's thread shares with the effect sections.
    pub fn sections(&self) -> Arc<Sections> {
        self.disk.sections()
    }

    /// The snapshot the observer starts from: the disk as L describes it.
    pub fn snapshot(&self) -> Snapshot {
        self.engine.snapshot()
    }

    /// Reclaims the local trash, evicting the oldest entries while it holds more than `cap` bytes. An entry written
    /// after it entered comes back at the root as a recovered file, which the observer reports. A failure is logged,
    /// and the next pass tries again.
    pub fn reclaim_trash(&self, cap: u64) {
        let space_id = &self.link.space_id;
        match self.disk.reclaim_trash(SystemTime::now(), cap) {
            Ok(reclaimed) => {
                for name in reclaimed.recovered {
                    let name = name.to_string_lossy();
                    tracing::warn!(space_id, %name, "a local trash entry written after it entered was recovered");
                }
            }
            Err(error) => {
                tracing::warn!(space_id, %error, "the local trash could not be reclaimed")
            }
        }
    }

    /// One event of the session.
    pub fn event(&mut self, event: Event) -> Result<Vec<Work>, Stop> {
        let mut work = Vec::new();
        match event {
            Event::Connected(welcome) => self.connected(&welcome, &mut work)?,
            Event::Changes(records) => self.changes(&records, &mut work)?,
            Event::Committed(committed) => self.result(&committed.commit_id, committed.result)?,
            Event::CaughtUp(caught_up) => {
                let head_seq = caught_up.head_seq;
                self.head_seq = Some(head_seq);
                match &mut self.stream {
                    Stream::Replaying(staging) => staging.caught_up(head_seq),
                    Stream::Following { head } => {
                        *head = Some(head_seq);
                        self.step(|engine, tx| engine.caught_up(tx, &caught_up))?;
                    }
                    Stream::Connecting | Stream::Listing { .. } => {}
                }
                self.publish_remote()?;
            }
            Event::ResyncRequired(resync) => match Reason::try_from(resync.reason) {
                Ok(Reason::LogTruncated)
                    if let Stream::Replaying(staging) = &mut self.stream
                        && let Some(last) = staging.forget_history() =>
                {
                    work.push(Work::Subscribe(resume(&self.history_id, last)));
                }
                Ok(reason @ (Reason::LogTruncated | Reason::AheadOfServer)) => {
                    tracing::info!(
                        space_id = self.link.space_id,
                        ?reason,
                        "the server's log does not continue from the store; reading the tree again"
                    );
                    if reason == Reason::AheadOfServer {
                        self.step(|engine, tx| engine.server_behind(tx, Vec::new()))?;
                    }
                    self.list_again(&mut work);
                }
                Ok(Reason::HistoryChanged) => {
                    let recovery =
                        RecoveryCause::rewind(resync.restore_seq, resync.restore_point_ms);
                    tracing::warn!(
                        space_id = self.link.space_id,
                        why = describe(&recovery),
                        "recovering from the Welcome of a new connection"
                    );
                    self.step(|engine, _| {
                        engine.commits.enter_recovery(recovery);
                        Ok(())
                    })?;
                    self.flight = Flight::Idle;
                    self.stream = Stream::Connecting;
                    work.push(Work::Reconnect);
                }
                Ok(Reason::Unspecified) | Err(_) => {
                    return Err(Stop::Protocol(format!(
                        "the server asked for a resync for an unknown reason ({})",
                        resync.reason
                    )));
                }
            },
            Event::Notice(notice) => {
                tracing::info!(
                    space_id = self.link.space_id,
                    kind = notice.kind,
                    retry_after_ms = notice.retry_after_ms,
                    message = notice.message,
                    "notice from the server"
                );
            }
            Event::Disconnected { reason, retry_in } => {
                tracing::info!(
                    space_id = self.link.space_id,
                    %reason,
                    kind = %reason.kind(),
                    detail = %reason.detail(),
                    ?retry_in,
                    "disconnected from the server"
                );
                let shown = self.disconnected.is_some()
                    || matches!(self.stream, Stream::Connecting)
                    || retry_in >= QUICK_RECONNECT;
                let since = (self.disconnected.as_ref())
                    .map_or_else(clock::instant, |disconnected| disconnected.since);
                self.disconnected = Some(Disconnected {
                    reason,
                    retry_at: clock::system() + retry_in,
                    since,
                    shown,
                });
            }
            Event::Stopped(reason) => return Err(Stop::Session(reason)),
        }
        self.drive(&mut work)?;
        Ok(work)
    }

    /// The pages of a bootstrap, read in order. The log is replayed onto them from the first page's `head_seq`
    /// before they replace R.
    pub fn listed(&mut self, listed: Result<Listed, Stop>) -> Result<Vec<Work>, Stop> {
        if let Stream::Listing { superseded } = &mut self.stream
            && *superseded
        {
            *superseded = false;
            return Ok(vec![Work::List]);
        }
        let listed = listed?;
        if !matches!(self.stream, Stream::Listing { .. }) {
            return Ok(Vec::new());
        }
        let (pages, trash) = match listed {
            Listed::Done { pages, trash } => {
                self.retrying.remove(&Call::List);
                (pages, trash)
            }
            Listed::Later {
                listing,
                wait,
                why,
                kind,
            } => {
                self.retry_after(Call::List, why, wait, kind, None);
                return Ok(vec![Work::ListAfter(wait, listing)]);
            }
        };
        let mut staging = Staging::new(self.history_id.clone(), pages, trash);
        tracing::info!(
            space_id = self.link.space_id,
            seq = staging.last_seq(),
            "read the space's tree"
        );
        let recovering = matches!(self.engine.commits.halted(), Some(CommitHalt::Recovery(_)));
        let since = (self.engine.cursor.as_ref()).filter(|cursor| {
            recovering
                && cursor.history_id == self.history_id
                && !cursor.ahead_of_server
                && cursor.last_seq <= staging.last_seq()
        });
        let from = match since {
            Some(cursor) => staging.read_history_since(cursor.base()),
            None => staging.last().clone(),
        };
        let work = vec![Work::Subscribe(resume(&self.history_id, &from))];
        self.stream = Stream::Replaying(staging);
        Ok(work)
    }

    /// One observation batch, with the number of entries the folder holds after it.
    pub fn observed(&mut self, batch: Batch, entries: u64) -> Result<Vec<Work>, Stop> {
        self.local_entries = Some(entries);
        self.unread = batch.unread;
        let detached = batch.observation.detached.clone();
        self.disk
            .observed(batch, &mut self.engine, &mut self.store)?;
        self.detached = detached;
        let allowed: Vec<String> = (self.denied.iter())
            .filter(|(node, denied)| {
                denied.foreseen
                    && self.engine.effects.blocked().contains_key(*node)
                    && self.allowed(node)
            })
            .map(|(node, _)| node.clone())
            .collect();
        if !allowed.is_empty() {
            self.step(|engine, _| {
                for node in &allowed {
                    engine.effects.unblock(node);
                }
                Ok(())
            })?;
            for node in &allowed {
                self.denied.remove(node);
            }
        }
        let problems = self.engine.commits.problems();
        let changed: Vec<String> = (self.unreadable.iter())
            .filter(|(node, unreadable)| {
                problems.contains(*node)
                    && (unreadable.left_out.as_ref()).is_some_and(|left_out| {
                        self.engine.trees.local().get(node.as_str()) != left_out.seen.as_ref()
                    })
            })
            .map(|(node, _)| node.clone())
            .collect();
        self.let_back_in(&changed)?;
        let mut work = Vec::new();
        self.drive(&mut work)?;
        Ok(work)
    }

    /// The observer has scanned the folder once. Design §2.7 leaves a node the server refused an op for out of
    /// commits until the next rescan. This scan is that rescan, so the engine may plan every such node again from
    /// here on. Nothing else clears them.
    pub fn scanned(&mut self) -> Result<Vec<Work>, Stop> {
        self.scanned = true;
        let cleared: Vec<String> = self.engine.commits.problems().iter().cloned().collect();
        self.let_back_in(&cleared)?;
        self.refused_nodes.retain(|id, _| !cleared.contains(id));
        self.publish_remote()?;
        let mut work = Vec::new();
        self.drive(&mut work)?;
        Ok(work)
    }

    /// `Driver::restore` recorded a request, or `Driver::answer` an answer. The engine takes what the inbox holds,
    /// and the next commit carries its `Restore` op (design §6.3), and it takes the answers to its questions (§6.1).
    pub fn requested(&mut self) -> Result<Vec<Work>, Stop> {
        self.step(|engine, tx| {
            engine.accept_restores(tx)?;
            Ok(engine.accept_answers(tx)?)
        })?;
        let mut work = Vec::new();
        self.drive(&mut work)?;
        Ok(work)
    }

    /// The staging of `pending` ended. A staged batch is applied here, in the effect section. A transfer that
    /// failed for now is staged again from the same `pending` after its wait, since nothing else can stage the
    /// batch in flight.
    pub fn staged(
        &mut self,
        pending: Pending,
        staged: Result<Staged, Stall>,
    ) -> Result<Vec<Work>, Stop> {
        let staged = match staged {
            Ok(staged) => {
                self.retrying.remove(&Call::Download);
                staged
            }
            Err(Stall::Later {
                wait,
                why,
                kind,
                unreachable,
            }) => {
                self.retry_after(Call::Download, why, wait, kind, unreachable);
                return Ok(vec![Work::StageAfter(wait, pending)]);
            }
            Err(Stall::Halt(stop)) => return Err(stop),
        };
        let effect_ids: Vec<String> = (self.engine.effects.in_flight().iter())
            .filter(|_| self.timing.is_on())
            .map(|record| record.effect_id.clone())
            .collect();
        let settled = self.disk.apply(staged, &mut self.engine, &mut self.store)?;
        let at = Time::now();
        for (effect_id, (_, settled)) in effect_ids.iter().zip(&settled) {
            self.timing
                .applied(effect_id, *settled == Settled::Done, at);
        }
        if self.engine.asked {
            self.questions = self.store.transaction(unanswered)?;
        }
        let mut work = Vec::new();
        for (node, settled) in settled {
            let (backoff, denied) = match settled {
                Settled::Denied(_) => (PERMISSION_RETRY, true),
                _ => (EFFECT_RETRY, false),
            };
            let refusal = (self.engine.effects.blocked().get(&node))
                .filter(|_| denied)
                .and_then(|effect| self.disk.refuses(effect));
            let foreseen = refusal.is_some();
            let settled = match (settled, refusal) {
                (Settled::Denied(_), Some(Refusal::Private(path))) => Settled::Denied(format!(
                    "this device does not allow Koidrive to write its own folder {}; Koidrive tries again by \
                     itself once it can",
                    koidrive_fs::quoted(&path)
                )),
                (settled, _) => settled,
            };
            self.denied.remove(&node);
            match settled {
                Settled::Done => {
                    self.effect_failures.remove(&node);
                    self.stuck_effects.remove(&node);
                }
                Settled::Failed(why) | Settled::Denied(why)
                    if self.engine.effects.blocked().contains_key(&node) =>
                {
                    self.stuck_effects.remove(&node);
                    let (failures, last) = self.effect_failures.entry(node.clone()).or_default();
                    let wait = backoff.delay(*failures);
                    *failures += 1;
                    last.clone_from(&why);
                    tracing::warn!(
                        space_id = self.link.space_id,
                        why,
                        ?wait,
                        "an effect failed; its node is planned again after the wait, or once what is planned for \
                         it changes"
                    );
                    let at = clock::instant() + wait;
                    if denied {
                        self.denied.insert(node, Denied { due: at, foreseen });
                    }
                    if self.retry_at.is_none_or(|out| at < out) {
                        self.retry_at = Some(at);
                        work.push(Work::RetryAfter(wait));
                    }
                }
                Settled::Failed(why) | Settled::Denied(why) => {
                    self.stuck_effects.remove(&node);
                    tracing::info!(
                        space_id = self.link.space_id,
                        why,
                        "an effect failed and is planned again"
                    );
                }
                Settled::Stuck(why) => {
                    tracing::warn!(
                        space_id = self.link.space_id,
                        why,
                        "an effect failed; its node is planned again once what is planned for it changes or the folder \
                         restarts"
                    );
                    self.effect_failures.entry(node.clone()).or_default().1 = why;
                    self.stuck_effects.insert(node);
                }
            }
        }
        self.drive(&mut work)?;
        Ok(work)
    }

    /// True once R holds everything the server had at the last `CaughtUp` of this connection (design §4.4).
    fn caught_up(&self) -> bool {
        match (&self.stream, &self.engine.cursor) {
            (Stream::Following { head: Some(head) }, Some(cursor)) => cursor.last_seq >= *head,
            _ => false,
        }
    }

    fn in_flight(&self) -> Option<Commit> {
        (self.engine.commits.in_flight()).map(|flight| flight.commit.clone())
    }

    /// Runs one change of the engine and its checkpoint in one store transaction.
    fn step<T>(
        &mut self,
        change: impl FnOnce(&mut Engine, &Tx<'_>) -> Result<T, Stop>,
    ) -> Result<T, Stop> {
        let engine = &mut self.engine;
        let space_id = &self.link.space_id;
        let (out, questions) = self.store.transaction(|tx| -> Result<_, Stop> {
            let out = change(engine, tx)?;
            engine.ask(tx, space_id)?;
            engine.checkpoint(tx)?;
            Ok((out, unanswered(tx)?))
        })?;
        self.questions = questions;
        let granted = self.engine.commits.take_granted();
        self.mark_granted(granted);
        #[cfg(test)]
        if let Some(left) = &mut self.crash_after {
            if *left == 0 {
                return Err(Stop::Protocol("the test crashed the task".into()));
            }
            *left -= 1;
        }
        Ok(out)
    }

    /// A `Welcome` (design §4.4 and §2.8). A cursor of another history stops the task before anything is replaced.
    /// The engine then compares the server's count of this device's commits with the store. A folder that was
    /// never synced lists the tree, and any other subscribes from its cursor once. After a reconnect the session
    /// resumes by itself.
    ///
    /// A store that never synced (no cursor, no commit in flight and none acknowledged) holds no commit of its own
    /// to account for. When the server counts commits of this device at such a store's first `Welcome`, the device
    /// synced the space with a store that is gone, as after an unlink and a second link, and the new store continues
    /// from the server's count (design §6.4, step 2). RFC 0002 section 2 sends every count but the store's own to
    /// recovery, and every other store still goes there.
    ///
    /// A server whose count is below the store's lost commits of this device that it had accepted, as when its
    /// storage was restored from a backup (design §2.8). The store then continues from the server's count, drops
    /// the commit in flight, and lists the tree again as for `AHEAD_OF_SERVER`, so that what the server lost is sent
    /// again.
    fn connected(&mut self, welcome: &Welcome, work: &mut Vec<Work>) -> Result<(), Stop> {
        self.disconnected = None;
        self.head_seq = Some(welcome.head_seq);
        self.history_id.clone_from(&welcome.history_id);
        self.device_seq = welcome.device_seq;
        if let Some(cursor) = &self.engine.cursor
            && cursor.history_id != welcome.history_id
        {
            let recovery = RecoveryCause::rewind(welcome.restore_seq, welcome.restore_point_ms);
            return self.recover(recovery, work);
        }
        let commits = &self.engine.commits;
        let never_synced = self.engine.cursor.is_none()
            && commits.in_flight().is_none()
            && commits.accepted() == 0;
        let adopt =
            matches!(self.stream, Stream::Connecting) && never_synced && welcome.device_seq != 0;
        if adopt {
            tracing::info!(
                space_id = self.link.space_id,
                device_seq = welcome.device_seq,
                "this device synced the space with a store that is gone; continuing from the server's count"
            );
        }
        let reconnect = self.step(|engine, tx| {
            if adopt {
                engine.commits.adopt(welcome.device_seq);
            }
            let in_flight = engine.commits.edited_in_flight();
            let reconnect = engine.commits.connected(welcome);
            if reconnect == Reconnect::AheadOfServer {
                engine.server_behind(tx, in_flight)?;
            }
            Ok(reconnect)
        })?;
        match reconnect {
            Reconnect::Recovery => {
                let Some(CommitHalt::Recovery(cause)) = self.engine.commits.halted() else {
                    unreachable!("connected halts for a recovery");
                };
                self.recover(cause, work)?;
            }
            Reconnect::AheadOfServer => {
                tracing::info!(
                    space_id = self.link.space_id,
                    device_seq = welcome.device_seq,
                    "the server lost commits of this device; reading the tree again"
                );
                self.flight = Flight::Idle;
                self.list_again(work);
            }
            Reconnect::Resend => {
                if let (Flight::Unknown, Some(commit)) = (&self.flight, self.in_flight()) {
                    self.flight = Flight::Sending(commit.clone());
                    work.push(Work::Send(commit));
                }
            }
            Reconnect::Idle => {
                let waits =
                    (self.engine.commits.in_flight()).is_some_and(|flight| flight.result.is_none());
                if matches!(self.flight, Flight::Unknown) && !waits {
                    self.flight = Flight::Idle;
                }
            }
        }
        match (&mut self.stream, &self.engine.cursor) {
            (
                Stream::Connecting,
                None
                | Some(Cursor {
                    ahead_of_server: true,
                    ..
                }),
            ) => {
                self.stream = Stream::Listing { superseded: false };
                work.push(Work::List);
            }
            (Stream::Connecting, Some(cursor)) => {
                work.push(Work::Subscribe(resume(&cursor.history_id, &cursor.base())));
                self.stream = Stream::Following { head: None };
            }
            (Stream::Following { head }, _) => *head = None,
            (Stream::Listing { .. } | Stream::Replaying(_), _) => {}
        }
        Ok(())
    }

    /// Commits of the log. A gap subscribes again from the last commit applied.
    fn changes(&mut self, records: &[CommitRecord], work: &mut Vec<Work>) -> Result<(), Stop> {
        let before = match &self.stream {
            Stream::Replaying(staging) => staging.last_seq(),
            _ => self
                .engine
                .cursor
                .as_ref()
                .map_or(0, |cursor| cursor.last_seq),
        };
        let (applied, since) = match &mut self.stream {
            Stream::Connecting | Stream::Listing { .. } => return Ok(()),
            Stream::Replaying(staging) => (
                staging.replay(records)?,
                resume(staging.history_id(), staging.last()),
            ),
            Stream::Following { .. } => {
                let (applied, nodes) = self.step(|engine, tx| {
                    Ok((engine.apply_records(tx, records)?, tx.remote_node_count()?))
                })?;
                self.remote_nodes = nodes;
                let cursor = self.engine.cursor.as_ref().expect("R follows the log");
                (applied, resume(&cursor.history_id, &cursor.base()))
            }
        };
        let through = match &self.stream {
            Stream::Replaying(staging) => staging.last_seq(),
            _ => self
                .engine
                .cursor
                .as_ref()
                .map_or(0, |cursor| cursor.last_seq),
        };
        if self.timing.is_on() {
            self.timing
                .records(records, before, through, &self.link.device_id, Time::now());
        }
        crate::volume::commits_received(records.len());
        if applied == Applied::Gap {
            tracing::warn!(
                space_id = self.link.space_id,
                "the change stream skipped a seq; resuming"
            );
            work.push(Work::Subscribe(since));
        }
        self.publish_remote()
    }

    fn mark_granted(&self, granted: Vec<NodeId>) {
        let trees = &self.engine.trees;
        let entries = (granted.into_iter())
            .filter(|node| trees.local().get(node).is_some())
            .map(|node| (node.clone(), node));
        self.disk.mark(trees, entries);
    }

    /// Replaces R with the bootstrap's tree once the log has caught it up (design §4.4). On the first link of the
    /// folder, with no cursor in the store, R waits for the first scan, and the engine then matches L against it
    /// (§6.4): what both hold alike is adopted, and the rest of L is held until the person publishes or discards it.
    fn publish_remote(&mut self) -> Result<(), Stop> {
        let Stream::Replaying(staging) = &self.stream else {
            return Ok(());
        };
        let commits = &self.engine.commits;
        let sent_unanswered = (commits.in_flight()).is_some_and(|flight| {
            flight.result.is_none() && self.sent_commit.as_ref() == Some(&flight.commit.commit_id)
        });
        if !staging.ready() || (sent_unanswered && commits.halted().is_none()) {
            return Ok(());
        }
        let first_link = self.engine.cursor.is_none();
        let recovering = matches!(self.engine.commits.halted(), Some(CommitHalt::Recovery(_)));
        if (first_link || recovering) && !self.scanned {
            return Ok(());
        }
        let head = Some(staging.last_seq());
        let Stream::Replaying(mut staging) =
            std::mem::replace(&mut self.stream, Stream::Following { head })
        else {
            unreachable!("checked above");
        };
        let now_ms = (SystemTime::now().duration_since(UNIX_EPOCH))
            .map_or(0, |since| since.as_millis() as u64);
        let cause = match self.engine.commits.halted() {
            Some(CommitHalt::Recovery(cause)) => cause,
            _ => RecoveryCause::of(RecoveryTrigger::StoreRolledBack),
        };
        let device_seq = self.device_seq;
        let prior = match device_seq > 0 || self.link.found_marker {
            true => PriorSync::Shown,
            false => PriorSync::Fresh,
        };
        let history = (staging.take_history())
            .map(|log| koidrive_engine::first_link::since(self.engine.trees.remote(), log))
            .unwrap_or_default();
        let trees = &self.engine.trees;
        let unheld: BTreeMap<NodeId, FileIdentity> = (trees.local().iter())
            .filter(|(id, _)| recovering && trees.synced().get(id).is_none())
            .map(|(id, node)| (id.clone(), node.identity))
            .collect();
        let marks = self.disk.marks(trees, unheld.keys());
        let unheld: HashSet<FileIdentity> = unheld.into_values().collect();
        self.remote_nodes = self.step(|engine, tx| {
            engine.replace_remote(tx, staging)?;
            if recovering {
                (engine.commits).recover(
                    &mut engine.trees,
                    device_seq,
                    &cause,
                    &history,
                    &marks,
                    &mut engine.ids,
                    now_ms,
                );
            } else if first_link {
                (engine.commits).first_link(&mut engine.trees, &mut engine.ids, now_ms, prior);
                engine.recovery_cause = Some(crate::RecoveryCause {
                    trigger: crate::RecoveryTrigger::NoSyncState,
                    restore_point_ms: None,
                });
            }
            Ok(tx.remote_node_count()?)
        })?;
        if recovering {
            self.unreadable.clear();
        }
        let trees = &self.engine.trees;
        let adopted = (trees.local().iter())
            .filter(|(id, node)| {
                trees.synced().get(id).is_some() && unheld.contains(&node.identity)
            })
            .map(|(id, _)| (id.clone(), id.clone()))
            .collect::<Vec<_>>();
        self.disk.mark(trees, adopted);
        Ok(())
    }

    /// Starts a recovery (design §6.4) for `why`: nothing is committed and the commit in flight is never sent again,
    /// and the tree of the server's current history is listed. Once it has replaced R and the folder is scanned, the
    /// engine classifies every node against S ([`Folder::publish_remote`]).
    fn recover(&mut self, recovery: RecoveryCause, work: &mut Vec<Work>) -> Result<(), Stop> {
        tracing::warn!(
            space_id = self.link.space_id,
            why = describe(&recovery),
            "recovering the folder"
        );
        self.step(|engine, _| {
            engine.commits.enter_recovery(recovery);
            Ok(())
        })?;
        self.flight = Flight::Idle;
        self.commit_ended();
        self.list_again(work);
        Ok(())
    }

    /// Bootstraps again (design §4.4): the tree is listed and replayed into a staging tree that replaces R, while L
    /// and S stay.
    fn list_again(&mut self, work: &mut Vec<Work>) {
        self.timing.clear_origins();
        if let Stream::Listing { superseded } = &mut self.stream {
            *superseded = true;
            return;
        }
        self.stream = Stream::Listing { superseded: false };
        work.push(Work::List);
    }

    fn drive(&mut self, work: &mut Vec<Work>) -> Result<(), Stop> {
        self.publish_remote()?;
        let staging = !self.engine.effects.in_flight().is_empty();
        let held = !self.scanned || staging || self.disk.awaits_rules();
        if held || !matches!(self.stream, Stream::Following { .. }) {
            return Ok(());
        }
        let commits = &self.engine.commits;
        if commits.in_flight().is_some() && commits.halted().is_none() {
            return Ok(());
        }
        if self.plan_effect(work)? {
            return Ok(());
        }
        let may_build = matches!(self.flight, Flight::Idle)
            && !self.disk.awaits_rescan()
            && self.engine.commits.in_flight().is_none()
            && self.caught_up()
            && self.disconnected.is_none()
            && self
                .engine
                .commits
                .due(&self.engine.trees, &self.link.device_id);
        if !may_build {
            return Ok(());
        }
        let now_ms = (SystemTime::now().duration_since(UNIX_EPOCH))
            .map_or(0, |since| since.as_millis() as u64);
        let device_id = self.link.device_id.clone();
        let rotated = self.disk.rotate_before_build(&mut self.store)?;
        let built = self.step(|engine, tx| {
            let cursor = engine.cursor.as_ref().expect("R follows the log");
            let built = engine.commits.build(
                &mut engine.trees,
                &mut engine.ids,
                now_ms,
                &cursor.history_id,
                &device_id,
                cursor.base(),
            );
            if let (Some(_), Some(tokens)) = (built, &rotated) {
                tx.set_root_tokens(tokens)?;
            }
            Ok(built.cloned())
        })?;
        match built {
            Some(commit) => {
                self.disk.built(rotated);
                self.start(commit, work)
            }
            None => {
                self.plan_effect(work)?;
            }
        }
        Ok(())
    }
}

/// The questions in the store that wait for an answer.
fn unanswered(tx: &Tx<'_>) -> Result<u64, koidrive_store::Error> {
    let questions = tx.questions()?;
    Ok(questions.iter().filter(|q| q.answer.is_none()).count() as u64)
}

/// A `Resume` of `history_id` from `since`, with its commit token (RFC 0013).
fn resume(history_id: &str, since: &Base) -> Resume {
    Resume {
        history_id: history_id.to_owned(),
        since_seq: since.seq,
        since_commit_id: since.commit_id.clone(),
        since_committed_at_ms: since.committed_at_ms,
    }
}

#[cfg(test)]
mod tests;
