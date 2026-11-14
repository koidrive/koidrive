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

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use koidrive_engine::classify::Class;
use koidrive_engine::commit::{Base, Failure, Halt as CommitHalt, Reconnect};
use koidrive_engine::effects::{self, Effect as EngineEffect, Stuck};
use koidrive_engine::ignore::LocalRules;
use koidrive_engine::tree::{Content, Kind, RemoteNode, Shape, Tree, same_content};
use koidrive_fs::{Snapshot, check_root};
use koidrive_net::{
    Auth, Backoff, CallError, ChunkError, Event, NotResent, ServerError, Session, SessionConfig,
    StopReason, Transfer, UploadGate,
};
use koidrive_proto::v1::error::Code;
use koidrive_proto::v1::resync_required::Reason;
use koidrive_proto::v1::{
    ChunkRef, Commit, CommitRecord, CommitResult, Error as ProtoError, Resume, TrashPage, TreePage,
    UploadReceipt, Welcome, op, op_result,
};
use koidrive_store::{Cursor, Link, Store, Tx};
use tokio::sync::{Notify, mpsc, watch};

use crate::claim::{self, Flock, Identity, Owner};
use crate::credential::SET_UP_AGAIN;
use crate::disk::{Batch, Disk, Pending, Sections, Settled, Staged, Stall, Stop, Upload};
use crate::driver::check_placement;
use crate::engine::{Applied, Engine, Staging};
use crate::failures::{self, Halted};
use crate::ignore::Matcher;
use crate::observe::{FsObserver, Observer};
use crate::policy::{self, Disposition};
use crate::progress::{Credit, Direction, Item, Tracker};
use crate::status::{FolderState, Wait, WaitKind, describe};
use crate::timing::{Time, Timing};
use koidrive_engine::question::{RecoveryCause, RecoveryTrigger};

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
/// How often a running folder checks that its path still names the folder it locked.
const CLAIM_RECHECK: Duration = Duration::from_secs(2);
/// The wait before a commit is built again after a file changed while its content was uploaded. The observer
/// reads the file again meanwhile.
const BUILD_AGAIN: Duration = Duration::from_secs(1);
/// The waits before a node whose disk effect failed is planned again: from about a second, doubling with each
/// failure of the node in a row, up to ten minutes.
const EFFECT_RETRY: Backoff = Backoff {
    initial: Duration::from_secs(1),
    max: Duration::from_secs(600),
};

/// How long calls of one kind may fail in a row before the status shows them, unless the server asks for a wait at
/// least this long.
const RETRY_SHOWN: Duration = Duration::from_secs(60);

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
    offline: bool,
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
    /// What the last disconnect was.
    note: Option<String>,
    /// The calls that failed and are sent again, by kind, until one of the kind goes through. The status shows
    /// them once they have failed for [`RETRY_SHOWN`], or at once when the wait is that long.
    retrying: BTreeMap<Call, Retrying>,
    /// The disk effects of each node that failed in a row in this task, for [`EFFECT_RETRY`], and why the last one
    /// failed.
    effect_failures: BTreeMap<String, (u32, String)>,
    /// The nodes whose last effect ended [`Settled::Stuck`]. A [`Work::RetryAfter`] leaves them blocked.
    stuck_effects: BTreeSet<String>,
    /// Why the server did not take a change of each node, for the nodes it may have left out of commits.
    refused_nodes: BTreeMap<String, String>,
    asked: BTreeSet<String>,
    progress: Arc<Mutex<Tracker>>,
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
    Uploading(Vec<PathBuf>),
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
    /// Read every page of the tree, then every page of the trash when `trash` is set. Answered with
    /// [`Folder::listed`].
    List {
        trash: bool,
    },
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
    trash: bool,
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

/// Calls of one kind that keep failing: since when, the latest failure and when the call is sent again.
#[derive(Debug)]
struct Retrying {
    since: Instant,
    why: String,
    wait: Duration,
    at: SystemTime,
    kind: WaitKind,
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
        let mut store = Store::open(store)?;
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
            offline: false,
            history_id: String::new(),
            device_seq: 0,
            head_seq: None,
            local_entries: None,
            remote_nodes,
            questions,
            failures: 0,
            backoff: Backoff::default(),
            note: None,
            retrying: BTreeMap::new(),
            effect_failures: BTreeMap::new(),
            stuck_effects: BTreeSet::new(),
            refused_nodes: BTreeMap::new(),
            asked,
            progress: Arc::default(),
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
                    ?reason,
                    ?retry_in,
                    "disconnected from the server"
                );
                self.offline = true;
                self.note = Some(format!(
                    "disconnected ({reason:?}); retrying in {retry_in:?}"
                ));
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
            return Ok(vec![Work::List {
                trash: self.lists_trash(),
            }]);
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
                self.retry_after(Call::List, why, wait, kind);
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
                && cursor.last_seq < staging.last_seq()
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
        self.disk
            .observed(batch, &mut self.engine, &mut self.store)?;
        let mut work = Vec::new();
        self.drive(&mut work)?;
        Ok(work)
    }

    /// The observer has scanned the folder once. Design §2.7 leaves a node the server refused an op for out of
    /// commits until the next rescan. This scan is that rescan, so the engine may plan every such node again from
    /// here on. Nothing else clears them.
    pub fn scanned(&mut self) -> Result<Vec<Work>, Stop> {
        self.scanned = true;
        let cleared = self.engine.commits.problems().clone();
        if !cleared.is_empty() {
            self.step(|engine, _| {
                for node in &cleared {
                    engine.commits.clear_problem(node);
                }
                Ok(())
            })?;
        }
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
            Err(Stall::Later { wait, why, kind }) => {
                self.retry_after(Call::Download, why, wait, kind);
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
            match settled {
                Settled::Done => {
                    self.effect_failures.remove(&node);
                    self.stuck_effects.remove(&node);
                }
                Settled::Failed(why) if self.engine.effects.blocked().contains_key(&node) => {
                    self.stuck_effects.remove(&node);
                    let (failures, last) = self.effect_failures.entry(node).or_default();
                    let wait = EFFECT_RETRY.delay(*failures);
                    *failures += 1;
                    last.clone_from(&why);
                    tracing::warn!(
                        space_id = self.link.space_id,
                        why,
                        ?wait,
                        "an effect failed; its node is planned again after the wait, or once what is planned for \
                         it changes"
                    );
                    let at = Instant::now() + wait;
                    if self.retry_at.is_none_or(|out| at < out) {
                        self.retry_at = Some(at);
                        work.push(Work::RetryAfter(wait));
                    }
                }
                Settled::Failed(why) => {
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

    /// A [`Work::RetryAfter`] ended. Every node whose effect failed is unblocked, so the engine plans its effect again
    /// even when it is the one that failed: the disk may have changed in a way the observer does not report, such as
    /// a name that was taken for a moment. A node that fails again waits longer the next time. A node whose last
    /// effect was stuck stays blocked, since no wait changes what keeps it.
    pub fn retry(&mut self) -> Result<Vec<Work>, Stop> {
        self.retry_at = None;
        let timed: Vec<String> = (self.engine.effects.blocked().keys())
            .filter(|node| !self.stuck_effects.contains(*node))
            .cloned()
            .collect();
        if !timed.is_empty() {
            self.step(|engine, _| {
                for node in &timed {
                    engine.effects.unblock(node);
                }
                Ok(())
            })?;
        }
        let mut work = Vec::new();
        self.drive(&mut work)?;
        Ok(work)
    }

    /// The upload of the content a commit adds ended. With its receipts the commit is sent. A file that changed
    /// since it was chunked, or that can no longer be read, abandons the commit, which the server holds nothing
    /// of (design §4.2), and the observer reads the commit's files again.
    pub fn uploaded(
        &mut self,
        receipts: Result<Vec<UploadReceipt>, ChunkError>,
    ) -> Result<Vec<Work>, Stop> {
        let mut work = Vec::new();
        let paths = match std::mem::replace(&mut self.flight, Flight::Idle) {
            Flight::Uploading(paths) => paths,
            other => {
                self.flight = other;
                return Ok(work);
            }
        };
        let error = match receipts {
            Ok(receipts) => {
                self.failures = 0;
                self.retrying.remove(&Call::Upload);
                if let Some(flight) = self.engine.commits.in_flight() {
                    let commit = Commit {
                        receipts,
                        ..flight.commit.clone()
                    };
                    self.sent_commit = Some(commit.commit_id.clone());
                    self.flight = Flight::Sending(commit.clone());
                    work.push(Work::Send(commit));
                }
                return Ok(work);
            }
            Err(error) => error,
        };
        failures::transfer_failed(&error, failures::Failure::Upload);
        let disposition = match &error {
            ChunkError::SourceChanged { .. } | ChunkError::Io { .. } => {
                tracing::info!(
                    space_id = self.link.space_id,
                    %error,
                    "a file changed while it was uploaded; planning its commit again"
                );
                self.step(|engine, _| Ok(engine.commits.abandon()))?;
                self.commit_ended();
                self.disk.rescan(&self.engine, paths);
                self.flight = Flight::Waiting(Next::Build);
                work.push(Work::ResendAfter(BUILD_AGAIN));
                return Ok(work);
            }
            ChunkError::Call(CallError::Stopped(_)) => {
                self.flight = Flight::Uploading(paths);
                return Ok(work);
            }
            ChunkError::Call(call) => policy::after_call_error(call),
            ChunkError::Refused {
                error: Some(refusal),
                ..
            } => policy::after_refusal(refusal),
            ChunkError::Refused { .. } | ChunkError::Unavailable { .. } => Disposition::Backoff,
            ChunkError::NoTarget { .. } | ChunkError::Corrupt { .. } => Disposition::Halt,
        };
        let wait = match disposition {
            Disposition::RetryAfter(wait) => wait,
            Disposition::Backoff => self.backoff.delay(self.failures),
            Disposition::Halt => {
                let why = format!("uploading a file's content failed: {error}");
                return Err(match &error {
                    ChunkError::Call(call) => Stop::after_call(call, why),
                    ChunkError::Refused { .. } => Stop::Refused(why),
                    _ => Stop::Protocol(why),
                });
            }
        };
        tracing::info!(space_id = self.link.space_id, %error, ?wait, "uploading again later");
        let kind = policy::wait_kind(match &error {
            ChunkError::Refused {
                error: Some(refusal),
                ..
            }
            | ChunkError::Call(CallError::Server(refusal)) => Some(refusal),
            _ => None,
        });
        self.retry_after(
            Call::Upload,
            format!("uploading a file's content failed: {error}"),
            wait,
            kind,
        );
        self.failures += 1;
        self.flight = Flight::Waiting(Next::Upload);
        work.push(Work::ResendAfter(wait));
        Ok(work)
    }

    /// `Session::commit` returned. A result is acknowledged once R has reached it. A refusal goes to the engine,
    /// which says whether the same commit is sent again, its changes are planned again, or committing stops. A
    /// commit the session did not send again because the server counts fewer commits than its slot accounts for
    /// waits for the `Welcome` that reported the count, which drops it ([`Folder::connected`]).
    pub fn sent(&mut self, result: Result<CommitResult, CallError>) -> Result<Vec<Work>, Stop> {
        let mut work = Vec::new();
        let Flight::Sending(commit) = std::mem::replace(&mut self.flight, Flight::Idle) else {
            return Ok(work);
        };
        match result {
            Ok(result) => {
                crate::volume::commit_sent();
                self.failures = 0;
                self.commit_ended();
                self.result(&commit.commit_id, result)?;
            }
            Err(CallError::Server(refusal)) => {
                self.refused(commit, NotTaken::Refused(refusal), &mut work)?
            }
            Err(CallError::Stopped(_)) => {
                self.flight = Flight::Unknown;
                return Ok(work);
            }
            Err(CallError::NotResent(NotResent::DeviceSeqMismatch { slot, welcome }))
                if welcome + 1 < slot =>
            {
                self.flight = Flight::Unknown;
                return Ok(work);
            }
            Err(CallError::NotResent(why)) => {
                tracing::warn!(space_id = self.link.space_id, %why, "a commit was not sent again after reconnecting");
                let trigger = match why {
                    NotResent::HistoryChanged { .. } => RecoveryTrigger::SpaceRewound,
                    _ => RecoveryTrigger::StoreRolledBack,
                };
                self.recover(RecoveryCause::of(trigger), &mut work)?;
                return Ok(work);
            }
            Err(CallError::TooLarge { bytes }) => {
                self.refused(commit, NotTaken::TooLarge { bytes }, &mut work)?
            }
            Err(error @ CallError::UnexpectedResult) => {
                return Err(Stop::Protocol(format!("sending a commit failed: {error}")));
            }
        }
        self.drive(&mut work)?;
        Ok(work)
    }

    /// A wait is over: upload or send the commit again, or build the next one.
    pub fn resend(&mut self) -> Result<Vec<Work>, Stop> {
        let mut work = Vec::new();
        match std::mem::replace(&mut self.flight, Flight::Idle) {
            Flight::Waiting(Next::Send(commit)) => {
                self.flight = Flight::Sending(commit.clone());
                work.push(Work::Send(commit));
            }
            Flight::Waiting(Next::Upload) => {
                if let Some(commit) = self.in_flight() {
                    self.start(commit, &mut work);
                }
            }
            Flight::Waiting(Next::Build) => {}
            other => self.flight = other,
        }
        self.drive(&mut work)?;
        Ok(work)
    }

    /// The commit in flight went through, or is not sent again as it is: the waits of its upload and its send
    /// leave the status.
    fn commit_ended(&mut self) {
        self.retrying.remove(&Call::Upload);
        self.retrying.remove(&Call::Commit);
    }

    /// Records that a call of kind `call` failed with `why` and is sent again after `wait`.
    fn retry_after(&mut self, call: Call, why: String, wait: Duration, kind: WaitKind) {
        let since = (self.retrying.get(&call)).map_or_else(Instant::now, |retrying| retrying.since);
        let at = SystemTime::now() + wait;
        let retrying = Retrying {
            since,
            why,
            wait,
            at,
            kind,
        };
        self.retrying.insert(call, retrying);
    }

    /// Whether a question changed since the last call, also when the number of questions did not.
    pub fn take_questions_changed(&mut self) -> bool {
        if !std::mem::take(&mut self.engine.asked) {
            return false;
        }
        let questions = self.engine.commits.questions();
        for question in questions
            .iter()
            .filter(|question| !self.asked.contains(&question.id))
        {
            failures::failed(failures::Failure::Asked(failures::asked(&question.kind)));
        }
        self.asked = (questions.iter())
            .map(|question| question.id.clone())
            .collect();
        true
    }

    /// The folder's status as the task knows it.
    pub fn view(&self) -> View {
        let trees = &self.engine.trees;
        let held = self.held();
        let state = match &self.stream {
            _ if self.offline => FolderState::Offline,
            Stream::Connecting => FolderState::Connecting,
            Stream::Listing { .. } | Stream::Replaying(_) => FolderState::Bootstrapping,
            Stream::Following { .. } if self.settled(&held) => FolderState::UpToDate,
            Stream::Following { .. } => FolderState::CatchingUp,
        };
        let quoted_item = |id: &String| {
            (self.item_path(id)).map_or_else(
                || format!("node {id}"),
                |path| koidrive_fs::quoted(&path).to_string(),
            )
        };
        let mut reasons: BTreeMap<&String, Vec<String>> = BTreeMap::new();
        for id in self.engine.effects.blocked().keys() {
            let why = (self.effect_failures.get(id)).map_or("it failed earlier", |(_, why)| why);
            let why = format!("the server's change could not be made here: {why}");
            reasons.entry(id).or_default().push(why);
        }
        for id in self.engine.commits.problems() {
            let why =
                (self.refused_nodes.get(id)).map_or("the server did not take it", String::as_str);
            reasons.entry(id).or_default().push(why.to_owned());
        }
        let kept_in = |id: &str| {
            (trees.local().get(id))
                .is_some_and(|node| node.parked)
                .then(|| {
                    let parked = Path::new(koidrive_fs::PRIVATE_DIR)
                        .join(koidrive_fs::PARKED_DIR)
                        .join(trees.parked_name(id));
                    format!("it is kept in {}", koidrive_fs::quoted(&parked))
                })
        };
        for (id, stuck) in &held {
            let kept = kept_in(id);
            let why = match stuck {
                Stuck::HeldBy(by) => {
                    let why = format!(
                        "its name here is taken by {}, which this device's rules exclude",
                        quoted_item(by)
                    );
                    match kept {
                        Some(kept) => format!("{why}; {kept}"),
                        None => why,
                    }
                }
                Stuck::Parked => format!("it has no free place here; {}", kept.unwrap_or_default()),
                Stuck::Behind(node) if held.get(node) == Some(&Stuck::Parked) => format!(
                    "waits for {}, which has no free place here; {}",
                    quoted_item(node),
                    kept_in(node).unwrap_or_default()
                ),
                Stuck::Behind(node) => format!(
                    "waits for {}, whose name here is taken by an entry this device's rules exclude",
                    quoted_item(node)
                ),
            };
            reasons.entry(id).or_default().push(why);
        }
        let mut stuck: Vec<String> = (reasons.into_iter())
            .map(|(id, whys)| format!("{} ({})", quoted_item(id), whys.join("; ")))
            .collect();
        stuck.sort();
        let shown = (self.retrying.values()).find(|retrying| {
            retrying.wait >= RETRY_SHOWN || retrying.since.elapsed() >= RETRY_SHOWN
        });
        let wait = (self.retrying.values())
            .max_by_key(|retrying| retrying.at)
            .map(|retrying| Wait {
                kind: retrying.kind,
                until_ms: (retrying.at.duration_since(UNIX_EPOCH))
                    .map_or(0, |at| at.as_millis() as i64),
            });
        let retrying =
            shown.map(|retrying| format!("{}; trying again at {}", retrying.why, utc(retrying.at)));
        let problem = match self.engine.commits.halted() {
            _ if self.note.is_some() => self.note.clone(),
            _ if retrying.is_some() => retrying,
            Some(CommitHalt::ReadOnly) => {
                Some("the server takes no changes from this device".into())
            }
            Some(CommitHalt::Policy { code }) => Some(format!(
                "the server refused this device's changes (code {code}); nothing more is sent{}",
                lift_hint(code)
            )),
            Some(CommitHalt::Recovery(cause)) => Some(describe(&cause)),
            None if !stuck.is_empty() => {
                let (count, fixed) = match stuck.len() {
                    1 => ("1 item".to_owned(), "it is"),
                    n => (format!("{n} items"), "they are"),
                };
                let more = stuck.len().saturating_sub(STUCK_SHOWN);
                stuck.truncate(STUCK_SHOWN);
                let more = match more {
                    0 => String::new(),
                    more => format!(", and {more} more"),
                };
                Some(format!(
                    "{count} could not be synced: {}{more}; run koi folder restart once {fixed} fixed",
                    stuck.join(", ")
                ))
            }
            None => None,
        };
        let last_seq = self.engine.cursor.as_ref().map(|c| c.last_seq);
        let recovery = match self.engine.commits.halted() {
            Some(CommitHalt::Recovery(cause)) => Some(cause),
            _ => (self.engine.commits.questions().iter())
                .find_map(koidrive_engine::question::Question::cause)
                .cloned(),
        };
        View {
            recovery,
            state,
            history_id: self.engine.cursor.as_ref().map(|c| c.history_id.clone()),
            last_seq,
            // The server reports its head only in a `Welcome` and a `CaughtUp`, and R may be past that since.
            head_seq: self.head_seq.max(last_seq),
            remote_nodes: self.remote_nodes,
            local_entries: self.local_entries,
            problem,
            wait,
            questions: self.questions,
        }
    }

    fn held(&self) -> BTreeMap<String, Stuck> {
        match (
            self.engine.effects.in_flight(),
            self.engine.commits.in_flight(),
        ) {
            ([], None) => effects::stuck(&self.engine.trees, &self.engine.effects, None),
            _ => BTreeMap::new(),
        }
    }

    pub fn track_progress(&mut self, state: FolderState) -> bool {
        match state {
            FolderState::Bootstrapping | FolderState::CatchingUp => {}
            FolderState::Connecting | FolderState::Offline => return false,
            FolderState::Stopped | FolderState::UpToDate | FolderState::Halted => {
                return self.progress.lock().unwrap().clear();
            }
        }
        let trees = &self.engine.trees;
        let held = self.held();
        let stays_out = self.stays_out(&held);
        fn file(shape: Option<&Shape>) -> Option<&Content> {
            (shape.filter(|shape| shape.kind == Kind::File))
                .and_then(|shape| shape.content.as_ref())
        }
        let other = |a: &Content, b: Option<&Content>| b.is_none_or(|b| !same_content(a, b));
        let mut wanted = BTreeMap::new();
        let mut want = |direction, id: &String, content: &Content| {
            let item = Item {
                direction,
                node: id.clone(),
                digest: content.manifest_digest.clone(),
            };
            wanted.insert(item, content.size);
        };
        for id in (trees.dirty().iter()).filter(|id| !stays_out(id) && !trees.held(id)) {
            let local = file(trees.local().live(id));
            let synced = file(trees.synced().live(id));
            let remote = file(trees.remote().live(id));
            let class = trees.class(id);
            if let Some(local) = local
                && matches!(class, Class::Local | Class::Concurrent)
                && other(local, remote)
                && (other(local, synced) || trees.unsent_edits().contains(id))
            {
                want(Direction::Upload, id, local);
            }
            if let Some(remote) = remote
                && matches!(class, Class::Remote | Class::Concurrent)
                && other(remote, synced)
                && other(remote, local)
            {
                want(Direction::Download, id, remote);
            }
        }
        let synced = |item: &Item| {
            let holds = match item.direction {
                Direction::Upload => file(trees.synced().live(&item.node)),
                Direction::Download => file(trees.local().live(&item.node)),
            };
            holds.is_some_and(|content| content.manifest_digest == item.digest)
        };
        self.progress.lock().unwrap().track(wanted, synced)
    }

    /// The path of a node relative to the root: where L holds it, or else where R does, as for a file whose download
    /// failed or that this device deleted.
    fn item_path(&self, id: &str) -> Option<PathBuf> {
        (self.engine.path_of(id)).or_else(|| remote_path(self.engine.trees.remote(), id))
    }

    /// True when nothing is left to move: R holds the server's head, the folder was scanned, no effect and no
    /// commit is in flight, no rules are awaited, every node the trees still differ on is one that cannot be synced
    /// as it is, that this device leaves out, a folder the user deleted that stays on the server for what this
    /// device leaves out, a folder to remove that L holds above a node that cannot be synced, or a node of `held`, which
    /// this device's rules keep from its place or from S ([`effects::stuck`]), and so is the node of every restore
    /// that waits.
    fn settled(&self, held: &BTreeMap<String, Stuck>) -> bool {
        let stuck = self.stays_out(held);
        let commits = &self.engine.commits;
        self.caught_up()
            && self.scanned
            && !self.disk.awaits_rules()
            && self.engine.effects.in_flight().is_empty()
            && matches!(self.flight, Flight::Idle)
            && self.engine.commits.in_flight().is_none()
            && self.engine.trees.dirty().iter().all(stuck)
            && commits.held_restores().count() == commits.restores().len()
    }

    fn stays_out<'a>(&'a self, held: &'a BTreeMap<String, Stuck>) -> impl Fn(&String) -> bool + 'a {
        let trees = &self.engine.trees;
        let mut above_blocked = BTreeSet::new();
        for node in (self.engine.effects.blocked().keys()).chain(held.keys()) {
            let mut current = node.as_str();
            for _ in 0..trees.local().len() {
                match trees.local().get(current) {
                    Some(local) if local.shape.parent != trees.local().root => {
                        current = &local.shape.parent;
                        above_blocked.insert(current);
                    }
                    _ => break,
                }
            }
        }
        let kept_for_excluded = trees.kept_for_excluded();
        move |id: &String| {
            self.engine.effects.blocked().contains_key(id)
                || self.engine.commits.problems().contains(id)
                || trees.excluded(id)
                || kept_for_excluded(id.as_str())
                || (above_blocked.contains(id.as_str()) && trees.remote().live(id).is_none())
                || held.contains_key(id)
        }
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
        self.offline = false;
        self.note = None;
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
                work.push(Work::List {
                    trash: self.lists_trash(),
                });
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

    /// The result of this device's commit `commit_id`, from the call or from the change stream, whichever comes
    /// first (design §2.7 and §4.4). Its records are applied when it is the commit after the cursor, and skipped
    /// when R already holds it. The engine keeps the result once, and acknowledges it when R has reached its seq.
    /// The second delivery finds no commit waiting for a result and changes nothing.
    fn result(&mut self, commit_id: &str, result: CommitResult) -> Result<(), Stop> {
        let ours = (self.engine.commits.in_flight())
            .is_some_and(|flight| flight.commit.commit_id == commit_id && flight.result.is_none());
        let flight = self.engine.commits.in_flight().filter(|_| ours);
        let next = match &self.stream {
            Stream::Replaying(staging) => result.seq == staging.last_seq() + 1,
            _ => self
                .engine
                .cursor
                .as_ref()
                .is_some_and(|cursor| result.seq == cursor.last_seq + 1),
        };
        if next {
            self.timing.forget(&result.changes);
        }
        if let Some(flight) = flight {
            self.timing.uploaded(&flight.commit, &result);
        }
        if let Some(refused) = flight.map(|flight| not_applied(&flight.commit, &result)) {
            let problems = self.engine.commits.problems();
            self.refused_nodes.retain(|id, _| problems.contains(id));
            for (node, why) in refused {
                tracing::info!(
                    space_id = self.link.space_id,
                    node,
                    path = ?self.item_path(&node),
                    why,
                    "the server did not apply a change"
                );
                self.refused_nodes.insert(node, why);
            }
        }
        let receive = |engine: &mut Engine, result: CommitResult| {
            (engine.commits.received(result)).map_err(|error| {
                Stop::Protocol(format!("the result of commit {commit_id}: {error:?}"))
            })
        };
        let at = match result.committed_at_ms {
            0 => Base::from(result.seq),
            committed_at_ms => Base {
                seq: result.seq,
                commit_id: commit_id.to_owned(),
                committed_at_ms,
            },
        };
        if let Stream::Replaying(staging) = &mut self.stream {
            if result.seq == staging.last_seq() + 1 {
                staging.replay_commit(at, &result.changes)?;
            } else {
                staging.remember(result.seq, &result.changes);
            }
            if ours {
                self.step(|engine, _| receive(engine, result))?;
            }
            return self.publish_remote();
        }
        self.remote_nodes = self.step(|engine, tx| {
            let next = (engine.cursor.as_ref()).is_some_and(|c| result.seq == c.last_seq + 1);
            if next {
                engine.apply_commit(tx, at.clone(), &result.changes)?;
            }
            if ours {
                receive(engine, result)?;
            }
            if let Some(cursor) = &engine.cursor {
                engine
                    .commits
                    .acknowledge(&mut engine.trees, cursor.last_seq);
            }
            Ok(tx.remote_node_count()?)
        })?;
        Ok(())
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
        let history = (staging.take_history())
            .map(|log| koidrive_engine::first_link::since(self.engine.trees.remote(), log))
            .unwrap_or_default();
        self.remote_nodes = self.step(|engine, tx| {
            engine.replace_remote(tx, staging)?;
            if recovering {
                (engine.commits).recover(
                    &mut engine.trees,
                    device_seq,
                    &cause,
                    &history,
                    &mut engine.ids,
                    now_ms,
                );
            } else if first_link {
                (engine.commits).first_link(&mut engine.trees, &mut engine.ids, now_ms);
                engine.recovery_cause = Some(crate::RecoveryCause {
                    trigger: crate::RecoveryTrigger::NoSyncState,
                    restore_point_ms: None,
                });
            }
            Ok(tx.remote_node_count()?)
        })?;
        Ok(())
    }

    fn refused(
        &mut self,
        commit: Commit,
        not_taken: NotTaken,
        work: &mut Vec<Work>,
    ) -> Result<(), Stop> {
        let refusal = &match &not_taken {
            NotTaken::Refused(refusal) => refusal.clone(),
            NotTaken::TooLarge { bytes } => ServerError(ProtoError {
                code: Code::LimitExceeded as i32,
                message: format!("the commit is {bytes} bytes, more than a message may hold"),
                ..ProtoError::default()
            }),
        };
        let before = self.engine.commits.problems().clone();
        let failure = self.step(|engine, tx| {
            let in_flight = engine.commits.edited_in_flight();
            let failure = (engine.commits.failed(&refusal.0))
                .map_err(|error| Stop::Protocol(format!("a refused commit: {error:?}")))?;
            if matches!(failure, Failure::Resync) {
                engine.server_behind(tx, in_flight)?;
            }
            Ok(failure)
        })?;
        let described = policy::describe(refusal);
        tracing::info!(
            space_id = self.link.space_id,
            refusal = described,
            ?failure,
            "a commit was refused"
        );
        let (counted, node_counted, why) = match not_taken {
            NotTaken::Refused(_) => {
                let code = failures::refusal(refusal);
                (
                    failures::Failure::Refused(code),
                    failures::Failure::ProblemNode(code),
                    format!("the server refused it: {described}"),
                )
            }
            NotTaken::TooLarge { .. } => (
                failures::Failure::TooLarge(failures::TooLarge::Commit),
                failures::Failure::TooLarge(failures::TooLarge::ProblemNode),
                format!("it is too large to send: {}", refusal.message()),
            ),
        };
        failures::failed(counted);
        for id in self.engine.commits.problems().difference(&before) {
            failures::failed(node_counted);
            self.refused_nodes.insert(id.clone(), why.clone());
        }
        match failure {
            Failure::Resend if refusal.code() == Code::MissingChunks => {
                if let Some(commit) = self.in_flight() {
                    self.start(commit, work);
                }
            }
            Failure::Resend => {
                let wait =
                    (refusal.retry_after()).unwrap_or_else(|| self.backoff.delay(self.failures));
                self.failures += 1;
                self.retry_after(
                    Call::Commit,
                    format!("the server did not take a commit: {described}"),
                    wait,
                    policy::wait_kind(Some(refusal)),
                );
                self.flight = Flight::Waiting(Next::Send(commit));
                work.push(Work::ResendAfter(wait));
            }
            Failure::Abandoned | Failure::Split | Failure::Halted(CommitHalt::ReadOnly) => {
                self.commit_ended();
            }
            Failure::Resync => {
                self.commit_ended();
                self.list_again(work);
            }
            Failure::Halted(CommitHalt::Recovery(cause)) => {
                tracing::warn!(
                    space_id = self.link.space_id,
                    described,
                    why = describe(&cause),
                    "a commit was refused; recovering through a new connection"
                );
                self.flight = Flight::Idle;
                self.stream = Stream::Connecting;
                work.push(Work::Reconnect);
            }
            Failure::Halted(CommitHalt::Policy { code }) => {
                return Err(Stop::Refused(format!(
                    "the server refused this device's changes: {described}{}",
                    lift_hint(code)
                )));
            }
        }
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
        work.push(Work::List {
            trash: self.lists_trash(),
        });
    }

    /// Whether a bootstrap reads the trash too: once the folder has synced, since L may then hold nodes another
    /// device trashed while this device missed the log. A node trashed since the server lost commits this device
    /// applied is then not taken for one the server lost and published again (RFC 0002 §6), and the mass-delete
    /// guard tells by each node's trash group which deletion its removal belongs to (design §6.1).
    fn lists_trash(&self) -> bool {
        self.engine.cursor.is_some()
    }

    /// Starts the upload of what `commit` adds, or its send when it adds no content.
    fn start(&mut self, commit: Commit, work: &mut Vec<Work>) {
        let mut upload = self.disk.upload(&commit, &self.engine);
        upload.credit = Credit::upload(&self.progress, &commit, |node| self.engine.path_of(node));
        if self.timing.is_on() {
            upload.timing_bytes = self.timing.uploading(&commit, Time::now());
        }
        if upload.chunks.is_empty() {
            self.sent_commit = Some(commit.commit_id.clone());
            self.flight = Flight::Sending(commit.clone());
            work.push(Work::Send(commit));
        } else {
            self.flight = Flight::Uploading(upload.paths());
            work.push(Work::Upload(upload));
        }
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
            && self.engine.commits.in_flight().is_none()
            && self.caught_up()
            && !self.offline
            && self.engine.commits.due(&self.engine.trees);
        if !may_build {
            return Ok(());
        }
        let now_ms = (SystemTime::now().duration_since(UNIX_EPOCH))
            .map_or(0, |since| since.as_millis() as u64);
        let built = self.step(|engine, _| {
            let cursor = engine.cursor.as_ref().expect("R follows the log");
            let built = engine.commits.build(
                &mut engine.trees,
                &mut engine.ids,
                now_ms,
                &cursor.history_id,
                cursor.base(),
            );
            Ok(built.cloned())
        })?;
        match built {
            Some(commit) => self.start(commit, work),
            None => {
                self.plan_effect(work)?;
            }
        }
        Ok(())
    }

    /// Plans the next batch of effects, if there is one, and asks for its staging.
    fn plan_effect(&mut self, work: &mut Vec<Work>) -> Result<bool, Stop> {
        let Some(mut pending) = self.disk.next(&mut self.engine, &mut self.store)? else {
            return Ok(false);
        };
        let remote = self.engine.trees.remote();
        let writes = (self.engine.effects.in_flight().iter())
            .filter(|record| matches!(record.effect, EngineEffect::WriteFile { .. }))
            .filter_map(|record| Some((record.node.as_str(), remote_path(remote, &record.node)?)));
        pending.credit = Credit::download(&self.progress, writes);
        if self.timing.is_on() {
            let at = Time::now();
            for record in self.engine.effects.in_flight() {
                let remote = self.engine.trees.remote().get(&record.node);
                let bytes = self.timing.planned(record, remote, at);
                pending.timing_bytes = pending.timing_bytes.take().or(bytes);
            }
        }
        work.push(Work::Stage(pending));
        Ok(true)
    }

    fn record_status(&mut self, state: FolderState) {
        if !self.timing.is_on() && !self.timing.has_first() {
            return;
        }
        if state == FolderState::UpToDate && self.retry_at.is_some() {
            return;
        }
        let at = Time::now();
        self.timing.status(state, at);
        if state == FolderState::UpToDate {
            self.timing.drained(at);
            let files = if self.timing.first_pending() {
                self.engine
                    .trees
                    .remote()
                    .iter()
                    .filter(|(_, node)| {
                        !node.trashed && node.shape.kind == koidrive_engine::tree::Kind::File
                    })
                    .count() as u64
            } else {
                0
            };
            self.timing.first_sync(files, at);
        }
    }
}

/// The questions in the store that wait for an answer.
fn unanswered(tx: &Tx<'_>) -> Result<u64, koidrive_store::Error> {
    let questions = tx.questions()?;
    Ok(questions.iter().filter(|q| q.answer.is_none()).count() as u64)
}

/// What the observer's thread sends to the task.
pub(crate) enum FromDisk {
    /// A stamped batch, with the number of entries the folder holds after it.
    Batch(Box<Batch>, u64),
    /// The first scan of the folder is done. Sent once, after the batch of that scan if it had one.
    Scanned,
    /// The folder cannot be observed at all.
    Failed(String),
}

pub(crate) struct FolderTask {
    pub linked_started: Option<Time>,
    pub telemetry: koidrive_telemetry::Recorder,
    pub link: Link,
    pub store: PathBuf,
    pub auth: Arc<Auth>,
    pub status: Arc<crate::lifecycle::Lifecycle>,
    pub matcher: Arc<dyn Matcher>,
    /// This device's `[sync]` list as the engine's trees take it, the same list `matcher` applies to the scans.
    pub local: LocalRules,
    /// The size cap in bytes of the folder's local trash.
    pub trash_cap: u64,
    /// The profile's one limit on chunk uploads in flight, which this folder shares with the others.
    pub upload_gate: UploadGate,
    /// The profile's private directories, which the folder must neither hold nor sit inside.
    pub private_dirs: Vec<PathBuf>,
    /// The roots of the profile's other linked folders.
    pub linked: Vec<PathBuf>,
    /// This profile's device, which must be the one that linked the folder.
    pub owner: Owner,
    /// The observer's thread, joined before the task ends so the folder's lock is released with it.
    pub observer: Mutex<Option<JoinHandle<()>>>,
    /// Marked whenever the folder's status changes (`Driver::watch`).
    pub changed: Arc<watch::Sender<()>>,
    pub progressed: Arc<watch::Sender<()>>,
    pub totals: Arc<Mutex<crate::progress::Sum>>,
    pub progress: Arc<Mutex<Tracker>>,
    /// Notified when `Driver::restore` has recorded a request in the folder's store.
    pub requests: Arc<Notify>,
    #[cfg(test)]
    pub observe_fault: Option<Arc<ObserveFault>>,
}

/// Fails the next observation of a folder's observer in a test. The pause after that failure lasts a day, so a stop
/// that waits it out never returns. `paused` hears when the pause begins.
#[cfg(test)]
pub(crate) struct ObserveFault {
    pub paused: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

#[cfg(test)]
thread_local! {
    pub(crate) static BEFORE_OPEN: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

/// Why the task stopped syncing before it was asked to.
#[derive(Debug, thiserror::Error)]
enum Halt {
    /// The folder at the path is not the one the task locked, or it fails its root check. The task syncs again on
    /// its own once the root passes the check, and only when the path leads to the folder it locked before.
    #[error("{1}")]
    Root(Halted, String),
    /// Only a person can fix it. The task waits for a restart.
    #[error("{1}")]
    Person(Halted, String),
    #[error("reconnecting")]
    Reconnect,
}

impl From<Stop> for Halt {
    fn from(stop: Stop) -> Self {
        let why = failures::halted(&stop);
        match stop {
            Stop::Withdrawn(_) | Stop::Root(_) | Stop::OtherRoot => {
                Halt::Root(why, stop.to_string())
            }
            Stop::Session(StopReason::CredentialsRefused | StopReason::DeviceRevoked) => {
                Halt::Person(why, format!("{stop}; {SET_UP_AGAIN}"))
            }
            stop => Halt::Person(why, stop.to_string()),
        }
    }
}

/// Work that is running, or nothing.
type Slot<'a, T> = Option<Pin<Box<dyn Future<Output = T> + Send + 'a>>>;

/// A `Resume` of `history_id` from `since`, with its commit token (RFC 0013).
fn resume(history_id: &str, since: &Base) -> Resume {
    Resume {
        history_id: history_id.to_owned(),
        since_seq: since.seq,
        since_commit_id: since.commit_id.clone(),
        since_committed_at_ms: since.committed_at_ms,
    }
}

/// What lifts a halt for a refusal with `code` (RFC 0006 section 1, point 5, and section 10).
fn lift_hint(code: i32) -> &'static str {
    if code == Code::ConfirmationRequired as i32 {
        "; the space's owner must confirm them, which Koidrive cannot ask for yet"
    } else {
        "; once the cause is fixed, run koi folder retry <folder> --resend to send them again, or --replan to plan them \
         again"
    }
}

/// The path of a node R holds, relative to the root, for a node L does not hold, such as one whose download failed.
fn remote_path(remote: &Tree<RemoteNode>, id: &str) -> Option<PathBuf> {
    let mut names = Vec::new();
    let mut current = id;
    while current != remote.root {
        let node = remote.get(current)?;
        names.push(node.shape.name.as_str());
        current = &node.shape.parent;
        if names.len() > remote.len() {
            return None;
        }
    }
    Some(names.into_iter().rev().collect())
}

/// The node of each op of `commit` that `result` says the server did not apply, with why. A `Restore` op is left out:
/// its node never stays out of commits.
fn not_applied(commit: &Commit, result: &CommitResult) -> Vec<(String, String)> {
    use op_result::Code as Outcome;
    (commit.ops.iter().zip(&result.results))
        .filter_map(|(sent, outcome)| {
            let code = Outcome::try_from(outcome.code).unwrap_or(Outcome::Unspecified);
            if matches!(
                code,
                Outcome::Applied
                    | Outcome::AppliedRenamed
                    | Outcome::ConflictCopy
                    | Outcome::MergedInto
            ) {
                return None;
            }
            let node = match sent.op.as_ref()? {
                op::Op::CreateDir(op) => &op.node_id,
                op::Op::CreateFile(op) => &op.node_id,
                op::Op::UpdateContent(op) => &op.node_id,
                op::Op::Move(op) => &op.node_id,
                op::Op::Delete(op) => &op.node_id,
                op::Op::Restore(_) => return None,
            };
            let why = match outcome.reject_reason.as_str() {
                "" => format!("the server did not apply it: {}", code.as_str_name()),
                reason => format!("the server rejected it: {reason}"),
            };
            Some((node.clone(), why))
        })
        .collect()
}

/// A finished `Session::commit`, after the session events `queued` before it. The session emits a connection's
/// `Connected` before it answers a commit on that connection, so the folder takes the `Welcome` before the result, and
/// the server's count in it is never older than a commit the folder acknowledged (design §2.8).
fn sent_in_order(
    folder: &mut Folder,
    queued: impl IntoIterator<Item = Event>,
    result: Result<CommitResult, CallError>,
) -> Result<Vec<Work>, Stop> {
    let mut work = Vec::new();
    for event in queued {
        work.extend(folder.event(event)?);
    }
    work.extend(folder.sent(result)?);
    Ok(work)
}

/// Waits for the work in `slot` and empties it. Never returns for an empty slot.
async fn finished<T>(slot: &mut Slot<'_, T>) -> T {
    match slot {
        Some(work) => {
            let out = work.as_mut().await;
            *slot = None;
            out
        }
        None => std::future::pending().await,
    }
}

impl FolderTask {
    /// Moves the folder's chunks. Its uploads wait for a place in the profile's gate.
    pub(crate) fn transfer(&self) -> Transfer {
        let mut transfer = Transfer::new(self.auth.api());
        transfer.gate = self.upload_gate.clone();
        transfer
    }

    /// Syncs the folder until `stop` is set. After a [`Halt::Root`] it syncs again, waiting longer after each one up
    /// to [`ROOT_RECHECK`]; after a [`Halt::Person`] it waits for `stop`. A sync after the first takes only the
    /// folder the first one locked: another one at the path, even with the space's marker, waits for a restart.
    pub async fn run(self, mut stop: watch::Receiver<bool>) {
        let space_id = self.link.space_id.clone();
        let mut again = ROOT_AGAIN;
        let mut locked = self.status.root();
        loop {
            let halted = tokio::select! {
                halted = self.sync(&mut locked) => Some(halted),
                _ = stop.wait_for(|stop| *stop) => None,
            };
            if self.status.lock().unwrap().progress.take().is_some() {
                self.changed.send_replace(());
            }
            let Some(halted) = halted else {
                self.join_observer().await;
                return;
            };
            if let Halt::Reconnect = halted {
                self.join_observer().await;
                continue;
            }
            tracing::warn!(space_id, reason = %halted, "sync of a folder halted");
            let (why, problem, recovery) = match &halted {
                Halt::Root(why, problem) => (
                    *why,
                    format!("{problem}; checking the folder again"),
                    crate::lifecycle::Recovery::Root,
                ),
                Halt::Person(why, problem) => {
                    (*why, problem.clone(), crate::lifecycle::Recovery::Restart)
                }
                Halt::Reconnect => unreachable!("handled above"),
            };
            self.status.halt(why, problem, recovery);
            self.changed.send_replace(());
            self.join_observer().await;
            if let Halt::Person(..) = halted {
                let _ = stop.wait_for(|stop| *stop).await;
                return;
            }
            tokio::select! {
                () = tokio::time::sleep(again) => {}
                _ = stop.wait_for(|stop| *stop) => return,
            }
            again = (again * 2).min(ROOT_RECHECK);
        }
    }

    /// Waits for the observer's thread, which ends soon after `sync` has dropped its receiver.
    async fn join_observer(&self) {
        let thread = self.observer.lock().unwrap().take();
        if let Some(thread) = thread {
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        }
    }

    /// Syncs the folder until something stops it. In order: the root check, the placement check, the claim, the
    /// store with the engine and the disk, the observer, the session, and then the loop that takes one input at a
    /// time. Dropping this future, which is how the task is stopped, drops every transfer with it. The store
    /// holds the effect and the commit in flight, so the next start goes on from them. `locked` is the folder an
    /// earlier sync of this task locked, which the claim requires and otherwise records.
    async fn sync(&self, locked: &mut Option<Identity>) -> Halt {
        self.wait_for_root().await;
        if let Err(error) = check_placement(&self.link.root, &self.private_dirs, &self.linked) {
            return Halt::Person(Halted::Placement, error.to_string());
        }
        let lock = match self.claim(locked) {
            Ok(lock) => Arc::new(lock),
            Err(problem) => return Halt::Person(Halted::Claim, problem),
        };
        if let Err(error) = self
            .status
            .claimed(locked.expect("a claimed folder has an identity"))
        {
            return Halt::Person(
                Halted::Io,
                format!("the folder's lifecycle cannot be saved: {error}"),
            );
        }
        #[cfg(test)]
        if let Some(before_open) = BEFORE_OPEN.with(|hook| hook.borrow_mut().take()) {
            before_open();
        }
        let local = self.local.clone();
        let mut folder = match Folder::open(self.link.clone(), &lock, &self.store, local) {
            Ok(folder) => folder,
            Err(stop) => return stop.into(),
        };
        folder.timing.linked(self.linked_started);
        folder.timing.enable(self.telemetry.clone());
        folder.progress = self.progress.clone();
        (folder.progress.lock().unwrap()).publish_to(
            self.status.clone(),
            self.progressed.clone(),
            self.totals.clone(),
            self.link.space_id.clone(),
        );
        let mut from_disk = self.observe(lock.clone(), folder.sections(), folder.snapshot());
        let mut config = SessionConfig::new(
            &self.link.space_id,
            "koidrive",
            env!("CARGO_PKG_VERSION"),
            MAX_FORMAT,
        );
        config.backoff = Backoff::default();
        config.retry_later_attempts = 0;
        let (session, mut events) = match Session::start(self.auth.clone(), config) {
            Ok(started) => started,
            Err(reason) => {
                return Halt::Person(failures::session_ended(&reason), reason.to_string());
            }
        };
        let transfer = self.transfer();
        self.publish(&mut folder);
        let stop = {
            let (session, transfer) = (&session, &transfer);
            let mut list: Slot<'_, Result<Listed, Stop>> = None;
            let mut stage: Slot<'_, (Pending, Result<Staged, Stall>)> = None;
            let mut upload: Slot<'_, Result<Vec<UploadReceipt>, ChunkError>> = None;
            let mut send: Slot<'_, Result<CommitResult, CallError>> = None;
            let mut wait: Slot<'_, ()> = None;
            let mut retry: Slot<'_, ()> = None;
            let mut recheck = tokio::time::interval(CLAIM_RECHECK);
            let mut reclaim = tokio::time::interval(RECLAIM_TRASH);
            reclaim.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            'session: loop {
                let step = tokio::select! {
                    event = events.next() => match event {
                        Some(event) => folder.event(event),
                        None => Err(Stop::Protocol("the session ended".into())),
                    },
                    Some(seen) = from_disk.recv() => match seen {
                        // The observer reaches the folder by its path, so a folder replaced before its
                        // first scan is scanned or fails there before the recheck below sees it.
                        FromDisk::Batch(batch, entries) => match folder.observed(*batch, entries) {
                            Err(Stop::OtherRoot) if !claim::still_locked(&lock, &self.link.root) => {
                                break Halt::Root(Halted::Replaced, REPLACED.to_owned());
                            }
                            step => step,
                        },
                        FromDisk::Scanned => folder.scanned(),
                        FromDisk::Failed(_) if !claim::still_locked(&lock, &self.link.root) => {
                            break Halt::Root(Halted::Replaced, REPLACED.to_owned());
                        }
                        FromDisk::Failed(why) => Err(Stop::Io(io::Error::other(why))),
                    },
                    pages = finished(&mut list) => folder.listed(pages),
                    (pending, staged) = finished(&mut stage) => folder.staged(pending, staged),
                    receipts = finished(&mut upload) => folder.uploaded(receipts),
                    result = finished(&mut send) => {
                        sent_in_order(&mut folder, std::iter::from_fn(|| events.try_next()), result)
                    }
                    () = finished(&mut wait) => folder.resend(),
                    () = finished(&mut retry) => folder.retry(),
                    () = self.requests.notified() => folder.requested(),
                    _ = reclaim.tick() => {
                        folder.reclaim_trash(self.trash_cap);
                        Ok(Vec::new())
                    }
                    _ = recheck.tick() => {
                        if !claim::still_locked(&lock, &self.link.root) {
                            break Halt::Root(Halted::Replaced, REPLACED.to_owned());
                        }
                        Ok(Vec::new())
                    }
                };
                let work = match step {
                    Ok(work) => work,
                    Err(stop) => break stop.into(),
                };
                for work in work {
                    match work {
                        Work::Subscribe(since) => session.subscribe(since),
                        Work::List { trash } => {
                            let listing = Listing {
                                trash,
                                ..Listing::default()
                            };
                            list = Some(Box::pin(list_tree(
                                session,
                                &self.link.space_id,
                                listing,
                                None,
                            )))
                        }
                        Work::ListAfter(wait, listing) => {
                            list = Some(Box::pin(list_tree(
                                session,
                                &self.link.space_id,
                                listing,
                                Some(wait),
                            )))
                        }
                        Work::Stage(pending) => {
                            stage = Some(Box::pin(stage_effect(session, transfer, pending, None)));
                        }
                        Work::StageAfter(wait, pending) => {
                            stage = Some(Box::pin(stage_effect(
                                session,
                                transfer,
                                pending,
                                Some(wait),
                            )));
                        }
                        Work::Upload(chunks) => {
                            upload = Some(Box::pin(upload_chunks(session, transfer, chunks)));
                        }
                        Work::Send(commit) => send = Some(Box::pin(session.commit(commit))),
                        Work::ResendAfter(after) => {
                            wait = Some(Box::pin(tokio::time::sleep(after)))
                        }
                        Work::RetryAfter(after) => {
                            retry = Some(Box::pin(tokio::time::sleep(after)))
                        }
                        Work::Reconnect => break 'session Halt::Reconnect,
                    }
                }
                self.publish(&mut folder);
            }
        };
        session.close().await;
        stop
    }

    /// Waits until the root passes the check of design §6.2: nothing is synced for a folder that is missing,
    /// unmounted or marked for another space.
    async fn wait_for_root(&self) {
        loop {
            match check_root(&self.link.root, &self.link.space_id, self.link.root_dev) {
                Ok(_) => {
                    return;
                }
                Err(problem) => {
                    tracing::warn!(space_id = self.link.space_id, %problem, "the folder fails its root check");
                    self.status.halt(
                        Halted::Root,
                        format!("{problem}; checking again in 30 seconds"),
                        crate::lifecycle::Recovery::Root,
                    );
                    self.changed.send_replace(());
                    tokio::time::sleep(ROOT_RECHECK).await;
                }
            }
        }
    }

    /// Takes the folder's lock and checks that this profile's device linked it, recording the device if no record
    /// is there. Another process that syncs the folder, or a record of another device, halts the folder. So does a
    /// folder other than `locked`, the one an earlier sync of this task locked, before anything is recorded in it.
    /// `locked` is then set to the folder this claim locked.
    fn claim(&self, locked: &mut Option<Identity>) -> Result<Flock, String> {
        let root = &self.link.root;
        let lock = claim::lock(root)
            .map_err(|error| format!("the folder's lock cannot be taken: {error}"))?
            .ok_or("another Koidrive process is syncing this folder; stop it, or unlink the folder there")?;
        let found = Identity::of(
            &lock
                .metadata()
                .map_err(|error| format!("the folder cannot be read: {error}"))?,
        );
        if locked.is_some_and(|locked| locked != found) {
            return Err(OTHER_FOLDER.to_owned());
        }
        *locked = Some(found);
        match claim::owner(root) {
            Ok(Some(owner)) if owner != self.owner => Err(format!(
                "the folder is linked by device {} of {}; unlink it there first, or remove its .koidrive/device.json",
                owner.device_id, owner.server
            )),
            Ok(Some(_)) => Ok(lock),
            Ok(None) => claim::set_owner(root, &self.owner)
                .map(|()| lock)
                .map_err(|error| format!("the folder's owner cannot be recorded: {error}")),
            Err(error) => Err(format!("the folder's owner record cannot be read: {error}")),
        }
    }

    /// Observes the disk on a thread of its own, starting from `previous`, one [`Sections::observe`] after another.
    /// The thread holds the folder's `lock`. Once the receiver is dropped, it stops reading files at the next chunk and
    /// ends.
    fn observe(
        &self,
        lock: Arc<Flock>,
        sections: Arc<Sections>,
        previous: Snapshot,
    ) -> mpsc::UnboundedReceiver<FromDisk> {
        let (sender, receiver) = mpsc::unbounded_channel();
        let link = self.link.clone();
        let matcher = self.matcher.clone();
        #[cfg(test)]
        let fault = self.observe_fault.clone();
        let thread = std::thread::spawn(move || {
            let _lock = lock;
            let closed = sender.clone();
            let started = FsObserver::start(
                &link.root,
                &link.space_id,
                link.root_dev,
                previous,
                matcher,
                move || closed.is_closed(),
            );
            let mut observer = match started {
                Ok(observer) => observer,
                Err(error) => {
                    tracing::warn!(space_id = link.space_id, %error, "the folder cannot be observed");
                    let _ = sender.send(FromDisk::Failed(format!(
                        "the folder cannot be observed: {error}"
                    )));
                    return;
                }
            };
            let mut scanned = false;
            while !sender.is_closed() {
                let observed = sections.observe(&mut observer, Duration::from_secs(1));
                #[cfg(test)]
                let paused = (fault.as_ref()).and_then(|fault| fault.paused.lock().unwrap().take());
                #[cfg(test)]
                let observed = match paused {
                    Some(_) => Err(koidrive_fs::Error::Io(io::Error::other(
                        "a failure the test injected",
                    ))),
                    None => observed,
                };
                match observed {
                    Ok(None) => {}
                    Ok(Some(batch)) => {
                        tracing::debug!(
                            space_id = link.space_id,
                            changes = batch.observation.changes.len(),
                            busy = batch.observation.busy.len(),
                            unreadable = batch.observation.unreadable.len(),
                            root_ok = batch.observation.root.is_ok(),
                            "observed the folder"
                        );
                        let entries = observer.snapshot().entries.len() as u64;
                        let _ = sender.send(FromDisk::Batch(Box::new(batch), entries));
                    }
                    Err(error) => {
                        tracing::warn!(space_id = link.space_id, %error, "observing the folder failed");
                        let again = Instant::now() + Duration::from_secs(5);
                        #[cfg(test)]
                        let again = match paused {
                            Some(paused) => {
                                let _ = paused.send(());
                                Instant::now() + Duration::from_secs(24 * 60 * 60)
                            }
                            None => again,
                        };
                        while !sender.is_closed() && Instant::now() < again {
                            std::thread::sleep(Duration::from_millis(50));
                        }
                    }
                }
                if !scanned && observer.scanned().is_some() {
                    scanned = true;
                    let _ = sender.send(FromDisk::Scanned);
                }
            }
        });
        *self.observer.lock().unwrap() = Some(thread);
        receiver
    }

    /// Shows what the folder knows in the status, and marks the status changed when it differs or a question changed.
    fn publish(&self, folder: &mut Folder) {
        let view = folder.view();
        folder.record_status(view.state);
        let began_or_ended = folder.track_progress(view.state);
        let asked = folder.take_questions_changed();
        let mut status = self.status.lock().unwrap();
        let shown = View {
            state: status.state,
            history_id: status.history_id.clone(),
            last_seq: status.last_seq,
            head_seq: status.head_seq,
            remote_nodes: status.remote_nodes,
            local_entries: status.local_entries,
            problem: status.problem.clone(),
            wait: status.wait,
            questions: status.questions,
            recovery: status.recovery,
        };
        if shown == view && !asked && !began_or_ended {
            return;
        }
        status.state = view.state;
        status.history_id = view.history_id;
        status.last_seq = view.last_seq;
        status.head_seq = view.head_seq;
        status.remote_nodes = view.remote_nodes;
        status.local_entries = view.local_entries;
        status.problem = view.problem;
        status.wait = view.wait;
        status.questions = view.questions;
        status.recovery = view.recovery;
        drop(status);
        self.changed.send_replace(());
    }
}

/// Reads the rest of the tree with `ListTree` from `listing`, after `wait` when there is one, page by page (spec
/// v0 section 7.5), then the trash with `ListTrash` when the listing asks for it (RFC 0002 §6). A page refused in a
/// way the server asks to retry ends it with the wait, so the folder can show it.
async fn list_tree(
    session: &Session,
    space_id: &str,
    mut listing: Listing,
    wait: Option<Duration>,
) -> Result<Listed, Stop> {
    if let Some(wait) = wait {
        tokio::time::sleep(wait).await;
    }
    while listing.pages.is_empty() || !listing.cursor.is_empty() {
        match session.list_tree(listing.cursor.clone(), TREE_PAGE).await {
            Ok(page) => {
                listing.failures = 0;
                listing.cursor.clone_from(&page.next_cursor);
                listing.pages.push(page);
            }
            Err(error) => return later(space_id, "tree", listing, &error),
        }
    }
    while listing.trash && (listing.trash_pages.is_empty() || !listing.trash_cursor.is_empty()) {
        match session
            .list_trash(listing.trash_cursor.clone(), TREE_PAGE)
            .await
        {
            Ok(page) => {
                listing.failures = 0;
                listing.trash_cursor.clone_from(&page.next_cursor);
                listing.trash_pages.push(page);
            }
            Err(error) => return later(space_id, "trash", listing, &error),
        }
    }
    Ok(Listed::Done {
        pages: listing.pages,
        trash: listing.trash_pages,
    })
}

/// How a listing ends after a page of the space's `what`, tree or trash, failed with `error`: later with a wait, or
/// with a stop when the server refuses it for good.
fn later(
    space_id: &str,
    what: &str,
    mut listing: Listing,
    error: &CallError,
) -> Result<Listed, Stop> {
    failures::call_failed(error);
    let why = match error {
        CallError::Server(refusal) => policy::describe(refusal),
        other => other.to_string(),
    };
    let wait = match policy::after_call_error(error) {
        Disposition::RetryAfter(wait) => wait,
        Disposition::Backoff => Backoff::default().delay(listing.failures),
        Disposition::Halt => {
            return Err(Stop::after_call(
                error,
                format!("reading the space's {what} failed: {why}"),
            ));
        }
    };
    tracing::info!(space_id, %error, ?wait, "reading the {what} again later");
    listing.failures += 1;
    Ok(Listed::Later {
        listing,
        wait,
        why: format!("reading the space's {what} failed: {why}"),
        kind: policy::wait_kind(match error {
            CallError::Server(refusal) => Some(refusal),
            _ => None,
        }),
    })
}

/// The time of day of `at` in UTC, such as `00:00 UTC`.
fn utc(at: SystemTime) -> String {
    let seconds = at
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
        % 86_400;
    format!("{:02}:{:02} UTC", seconds / 3_600, seconds / 60 % 60)
}

/// Stages `pending`, after `wait` when there is one, and gives it back with how the staging ended.
async fn stage_effect(
    session: &Session,
    transfer: &Transfer,
    pending: Pending,
    wait: Option<Duration>,
) -> (Pending, Result<Staged, Stall>) {
    if let Some(wait) = wait {
        tokio::time::sleep(wait).await;
    }
    let staged = pending.stage(&(session, transfer)).await;
    (pending, staged)
}

/// Uploads the chunks of `upload` the server lacks, and adds the bytes of each to [`crate::volume`] once it is stored.
async fn upload_chunks(
    session: &Session,
    transfer: &Transfer,
    upload: Upload,
) -> Result<Vec<UploadReceipt>, ChunkError> {
    let bytes = upload.timing_bytes.clone();
    upload_counted(session, transfer, upload, |count| {
        crate::volume::uploaded(count);
        crate::timing::add_bytes(&bytes, count);
    })
    .await
}

/// Uploads the chunks of `upload` the server lacks, each read on a thread that may block, and passes the size of each
/// to `stored` once it is stored.
async fn upload_counted(
    session: &Session,
    transfer: &Transfer,
    upload: Upload,
    mut stored: impl FnMut(u64),
) -> Result<Vec<UploadReceipt>, ChunkError> {
    let upload = Arc::new(upload);
    let read = |chunk: &ChunkRef| {
        let (upload, chunk) = (upload.clone(), chunk.clone());
        async move {
            tokio::task::spawn_blocking(move || upload.read(&chunk))
                .await
                .map_err(io::Error::other)?
        }
    };
    let credit = upload.credit.clone();
    let stored = |chunk: &ChunkRef| {
        stored(u64::from(chunk.size));
        credit.chunk(&chunk.hash);
    };
    let present = |chunk: &ChunkRef| credit.chunk(&chunk.hash);
    (transfer.upload_observed(session, &upload.chunks, read, stored, present)).await
}

#[cfg(test)]
mod tests;
