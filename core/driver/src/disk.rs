// SPDX-License-Identifier: Apache-2.0

//! Carrying out the engine's disk effects in the folder the task locked (design §2.5, §2.8, §5.2 and §5.3), and
//! putting observations of that folder into L without undoing an effect.
//!
//! [`Disk::start`] runs before the observer. It binds a [`Space`] to a duplicate of the locked descriptor, reads the
//! root marker's counter *m* before anything writes it, and compares it with the store's effect counter *e*. It
//! recovers the effects in flight and acknowledges them, requires *m* = *e* afterwards, and deletes staged downloads
//! no effect owns. Then each batch of effects takes three calls:
//!
//! 1. [`Disk::next`] plans the next batch of independent effects and commits it with the ID generator's state. A
//!    [`Pending`] exists only after that commit, so the store holds every effect before the disk sees it.
//! 2. [`Pending::stage`] downloads the files the batch writes into staging, which no scan reports, fetching each
//!    distinct chunk once. It borrows neither the store nor the engine, so the task keeps handling other inputs
//!    meanwhile.
//! 3. [`Disk::apply`] is the effect section. With no await in between, it writes the batch's counter to the
//!    marker, applies each effect with the quarantine protocol and acknowledges the outcomes in one store transaction.
//!
//! Observations reach L through [`Disk::observed`]. It refuses a batch scanned from another folder than the locked
//! one, and drops the changes a batch reports at or below the paths of an effect section that ran while the batch
//! was scanned or waited. Such a change can predate the effect, and applying it would undo the acknowledged L.
//! After each section, and for every path of a dropped change, the observer resets its snapshot there to L's view and
//! scans the path again ([`Sections::take_resets`]). The next batch then reports how the disk differs from L there,
//! removals and moves included, and carries the content of every file L holds there, read from the disk again.
//!
//! A [`Stop`] ends the task. The engine in memory may then be ahead of the store; the next start loads it again and
//! recovers the effects in flight. A root that is no longer at its linked path is never treated as a missing parent.
//! `core/fs` refuses every rename, `mkdir` and marker write once the path names another folder, and that refusal
//! stops the task with the flight kept. The check and the rename that follows it are not one atomic step, so another
//! process that moves the still locked root in between can still receive that one rename (design §5.2 accepts the
//! same window for parents). Nothing is ever written into a folder that replaced the root, since every write goes
//! through the locked descriptor.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use koidrive_chunk::ChunkerConfig;
use koidrive_engine::classify::Trees;
use koidrive_engine::effects::{
    Base as EngineBase, BatchLimit, Dir as EngineDir, Displaced as EngineDisplaced,
    Effect as EngineEffect, EffectRecord, Effects, Location as EngineLocation, location_of,
};
use koidrive_engine::first_link::{self, Marks};
use koidrive_engine::ignore::is_rules_file;
use koidrive_engine::question::{RecoveryCause, RecoveryTrigger};
use koidrive_engine::tree::{Content, Kind, LocalNode, NodeId};
use koidrive_fs::{
    Action, Base, BroughtBack, Contents, Ctime, Dir, Displaced, Effect, Entry, EntryKind, FileId,
    Known, Location, Marker, Minted, Outcome, Reason, Reclaimed, Refusal, RootAt, RootProblem,
    RootTokens, Rotation, Space, Step, Unresolved, check_root_at, open_root_folder, root_at,
};
use koidrive_net::{CallError, ChunkError, Session, StopReason, Transfer};
use koidrive_proto::v1::error::Code;
use koidrive_proto::v1::{ChunkRef, Commit, VersionInfo, op};
use koidrive_store::{Link, Store, Tx};
use prost::Message;
use rustix::fs::{Mode, OFlags};

use crate::engine::{Engine, file_id, file_identity, pending};
use crate::failures::{self, Failure};
use crate::observe::{FsObserver, Observation, Observed, Observer, Reset};
use crate::policy::{self, Disposition};
use crate::progress::{Credit, Direction, Item};
use crate::status::Remedy;
use crate::status::WaitKind;

/// How many effect sections [`Disk`] remembers for batches that span them. A batch older than all of them is dropped
/// whole, and the observer rescans the whole tree.
const SPANS: usize = 1024;
/// The wait before staging again after a transfer failed for now, when the server named none.
const STAGE_AGAIN: Duration = Duration::from_secs(30);
/// The most effects one batch holds: as many as one call for download targets takes chunks, with the contents of its
/// writes after the first within 64 MiB.
pub(crate) const BATCH: BatchLimit = BatchLimit {
    effects: koidrive_net::MAX_CHUNKS_PER_CALL,
    bytes: 64 << 20,
};

/// Why the task stops. The effects in flight, if any, stay in the store for the next start to recover.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Stop {
    #[error("{} no longer names the folder this task locked (it was moved, replaced or removed)", koidrive_fs::quoted(.0))]
    Withdrawn(PathBuf),
    #[error("the folder fails its root check: {0}")]
    Root(RootProblem),
    #[error("the folder's file system has no rename that refuses to replace its target")]
    NoReplace,
    #[error(
        "the root marker counts {marker} effects, fewer than the {cleared} the store cleared: the folder was restored \
         from an older state"
    )]
    DiskBehind { marker: u64, cleared: u64 },
    #[error("an observation was scanned from another folder than the one this task locked")]
    OtherRoot,
    #[error("effect {effect_id} cannot be carried out: {what}")]
    Record { effect_id: String, what: String },
    #[error("the folder's sync state: {0}")]
    Store(#[from] koidrive_store::Error),
    #[error("the folder: {0}")]
    Io(#[from] io::Error),
    #[error("{0}")]
    Session(StopReason),
    /// The server refused a call for good.
    #[error("{0}")]
    Refused(String),
    #[error("{problem}")]
    Policy { problem: String, needs_owner: bool },
    /// The server sent something this build cannot apply.
    #[error("{0}")]
    Protocol(String),
}

impl Stop {
    pub(crate) fn remedy(&self) -> Remedy {
        match self {
            Stop::Root(RootProblem::Copy) | Stop::DiskBehind { .. } => Remedy::Relink,
            Stop::Withdrawn(_) | Stop::Root(_) | Stop::OtherRoot => Remedy::Root,
            Stop::Session(StopReason::CredentialsRefused | StopReason::DeviceRevoked) => {
                Remedy::SetUpAgain
            }
            Stop::Session(StopReason::Forbidden | StopReason::NotFound) => Remedy::Membership,
            Stop::Policy {
                needs_owner: true, ..
            } => Remedy::Owner,
            Stop::Policy {
                needs_owner: false, ..
            } => Remedy::Retry,
            _ => Remedy::Restart,
        }
    }

    pub(crate) fn after_call(error: &CallError, why: String) -> Stop {
        match error {
            CallError::Server(_) => Stop::Refused(why),
            CallError::Stopped(reason) => Stop::Session(reason.clone()),
            CallError::NotResent(_) | CallError::TooLarge { .. } | CallError::UnexpectedResult => {
                Stop::Protocol(why)
            }
        }
    }
}

/// Why [`Pending::stage`] returned without staging the batch. Its effects stay in flight.
#[derive(Debug)]
pub(crate) enum Stall {
    /// The transfer failed for now with `why`. Stage again after `wait`.
    Later {
        wait: Duration,
        why: String,
        kind: WaitKind,
        unreachable: Option<koidrive_net::Cause>,
    },
    /// Staging cannot go on: the root was withdrawn, a staged file could not be written, or the session ended.
    Halt(Stop),
}

/// How an effect ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Settled {
    Done,
    /// The node stays blocked until the effect planned for it changes, except after an interrupted effect or one L
    /// no longer takes, which can be planned again unchanged.
    Failed(String),
    /// Failed, and planning the effect again cannot help until the person changes the disk: the folder it removes
    /// holds entries L does not know. The node stays blocked with no wait to end it.
    Stuck(String),
    Denied(String),
    AwaitsScan(String),
}

/// What the effect sections share with the observer's thread.
#[derive(Debug, Default)]
pub(crate) struct Sections {
    /// Odd while an effect section runs. Each section adds one when it starts and one when it ends.
    epoch: AtomicU64,
    /// Scopes the observer resets to L's view and scans again before its next batch, by request in the order
    /// requested, and how many requests there have been.
    resets: Mutex<(Requested, u64)>,
    open: Mutex<Vec<(Requests, Vec<PathBuf>)>>,
    recorded: Mutex<Option<BTreeMap<PathBuf, Known>>>,
    marks: koidrive_fs::MarkLog,
}

type Requested = Vec<(Requests, Vec<Reset>)>;

/// A count of the reset requests of the effect sections. The count a request returns names that request and every
/// one before it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Requests(u64);

#[derive(Clone, Debug, Default)]
pub(crate) struct Answers {
    pub through: Requests,
    pub open: Vec<Requests>,
}

impl Answers {
    fn covers(&self, request: Requests) -> bool {
        request <= self.through && !self.open.contains(&request)
    }
}

/// The epoch read before a batch's scans began.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stamp(u64);

impl Sections {
    /// Read before each [`Observer::observe`](crate::observe::Observer::observe), so the stamp covers the batch's
    /// scans and content reads.
    pub fn stamp(&self) -> Stamp {
        Stamp(self.epoch.load(Ordering::SeqCst))
    }

    pub fn marks(&self) -> koidrive_fs::MarkLog {
        self.marks.clone()
    }

    /// The resets to pass to [`FsObserver::reset`] before the next observation, by request, with the count of every
    /// request made so far.
    pub fn take_resets(&self) -> (Requested, Requests) {
        let mut resets = self.resets.lock().unwrap();
        (std::mem::take(&mut resets.0), Requests(resets.1))
    }

    pub(crate) fn request(&self, resets: Vec<Reset>) -> Requests {
        let mut requested = self.resets.lock().unwrap();
        requested.1 += 1;
        let request = Requests(requested.1);
        requested.0.push((request, resets));
        request
    }

    /// One pass of the observer's thread. It resets the scopes the effect sections asked for, then takes the stamp,
    /// so the stamp covers the batch's scans and content reads. After a reset it does not wait for a change, since
    /// the task may plan nothing until that scan is reported; otherwise it waits up to `idle`.
    pub fn observe(
        &self,
        observer: &mut FsObserver,
        idle: Duration,
    ) -> Result<Option<Batch>, koidrive_fs::Error> {
        let (resets, through) = self.take_resets();
        let wait = if resets.iter().all(|(_, resets)| resets.is_empty()) {
            idle
        } else {
            Duration::ZERO
        };
        let mut open = self.open.lock().unwrap();
        for (request, resets) in resets {
            open.push((
                request,
                resets.iter().map(|reset| reset.scope.clone()).collect(),
            ));
            observer.reset(resets);
        }
        if let Some(recorded) = self.recorded.lock().unwrap().take() {
            observer.seek(recorded);
        }
        let stamp = self.stamp();
        let observation = observer.observe(wait)?;
        Ok(observation.map(|observation| {
            let unanswered = observer.unanswered();
            open.retain_mut(|(_, scopes)| {
                scopes.retain(|scope| unanswered.contains(scope));
                !scopes.is_empty()
            });
            Batch {
                stamp,
                answered: Answers {
                    through,
                    open: open.iter().map(|(request, _)| *request).collect(),
                },
                observation,
                unread: observer.unread(),
            }
        }))
    }
}

#[derive(Debug, Default)]
struct Unscanned {
    batch: Vec<PathBuf>,
    effects: Vec<(Requests, Vec<PathBuf>)>,
    dropped: Vec<(Requests, Vec<PathBuf>)>,
}

impl Unscanned {
    fn all(&self, more: &[PathBuf]) -> Vec<PathBuf> {
        let requested = (self.effects.iter()).chain(&self.dropped);
        (self.batch.iter())
            .chain(requested.flat_map(|(_, paths)| paths))
            .chain(more)
            .cloned()
            .collect()
    }
}

/// One observation with the stamp taken before it was scanned.
#[derive(Debug)]
pub(crate) struct Batch {
    pub stamp: Stamp,
    /// The reset requests this batch answers: the observer was given the resets of each one, and a scan made after
    /// that settled every scope of them, in this batch or in one sent before it. A stamp cannot say this: it tells
    /// when the scans of a batch began, not which scopes they read.
    pub answered: Answers,
    pub observation: Observation,
    pub unread: (u64, u64),
}

/// The places a crash test stops an effect at, besides the steps of `core/fs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cut {
    /// Inside the transaction that records the planned effect, before it commits.
    BeforePlan,
    /// After that transaction committed.
    AfterPlan,
    Step(Step),
    /// Inside the transaction that acknowledges the effect, before it commits.
    BeforeAck,
    /// After that transaction committed.
    AfterAck,
    TokenPended,
    TokenWritten,
    TokenCommitted,
}

type Hook = Box<dyn FnMut(Cut) + Send>;

/// The locked folder, as effects and observations reach it.
pub(crate) struct Disk {
    space: Arc<Space>,
    path: PathBuf,
    space_id: String,
    sections: Arc<Sections>,
    recorded: BTreeMap<PathBuf, Known>,
    /// The paths of the latest effect sections, each with the epoch its section ended at, oldest first.
    spans: VecDeque<(u64, Vec<PathBuf>)>,
    /// The end epoch of the latest section that `spans` no longer holds.
    forgotten: u64,
    unscanned: Unscanned,
    rescan_after: Vec<Requests>,
    /// The most each batch of effects holds: [`BATCH`] outside tests.
    batch: BatchLimit,
    hook: Hook,
    brought_back: BroughtBack,
    current: Option<String>,
    next: Option<NextToken>,
    unwritable: Option<String>,
    #[cfg(test)]
    flush_fails: bool,
}

struct NextToken {
    token: Minted,
    rotation: Rotation,
    for_build: bool,
    flushed: bool,
}

/// A planned batch of effects the store holds, in the order they were planned.
pub(crate) struct Pending {
    pub timing_bytes: crate::timing::Bytes,
    pub credit: Credit,
    space: Arc<Space>,
    effects: Vec<Planned>,
}

#[derive(Clone)]
struct Planned {
    record: EffectRecord,
    effect: Effect,
    contents: Expected,
    /// The version whose chunk list a write downloads when its content omits the list, from the store's R.
    version: Option<String>,
}

/// A batch ready to apply: each write's file is staged and flushed, or holds the reason it cannot be fetched.
pub(crate) struct Staged {
    effects: Vec<(Planned, Option<String>)>,
}

/// Where chunk lists and chunks come from: the session and [`Transfer`] in the folder task.
pub(crate) trait Fetch {
    /// Every page of the chunk list of `version_id`, in order.
    async fn version_chunks(&self, version_id: &str) -> Result<Vec<ChunkRef>, ChunkError>;

    /// Hands each distinct chunk of `chunks` to `write` once, checked against its hash and size, as
    /// [`Transfer::download`] does.
    async fn download(
        &self,
        chunks: &[ChunkRef],
        write: &mut (dyn FnMut(&ChunkRef, &[u8]) -> io::Result<()> + Send),
    ) -> Result<(), ChunkError>;
}

impl Fetch for (&Session, &Transfer) {
    async fn version_chunks(&self, version_id: &str) -> Result<Vec<ChunkRef>, ChunkError> {
        let mut chunks = Vec::new();
        let mut page = 0;
        loop {
            let found = self.0.get_version_chunks(version_id, page).await?;
            chunks.extend(found.chunks);
            page += 1;
            if page >= found.page_count {
                return Ok(chunks);
            }
        }
    }

    async fn download(
        &self,
        chunks: &[ChunkRef],
        write: &mut (dyn FnMut(&ChunkRef, &[u8]) -> io::Result<()> + Send),
    ) -> Result<(), ChunkError> {
        self.1
            .download(self.0, chunks, |chunk, bytes| {
                crate::volume::downloaded(bytes.len());
                std::future::ready(write(chunk, &bytes))
            })
            .await
    }
}

impl Disk {
    /// Startup for the folder `link` names, through `lock`, the descriptor the task locked. In order:
    ///
    /// 1. The root check of design §6.2 through that descriptor: the linked path still names it, it is on the linked
    ///    device, and its marker names the space and, unless it counts more effects than the store (step 2), carries a
    ///    token the store recorded ([`RootProblem::Copy`] otherwise). Nothing is written when it fails.
    /// 2. The counters: with *e* the store's effect counter and *upper* that plus the number of effects in flight, a
    ///    marker above *upper* means the store is older than the folder. The disk carried out every effect the
    ///    marker counts, so the store continues from it and from the marker's token, and the folder recovers (§6.4).
    ///    A marker below *e* is [`Stop::DiskBehind`], which leaves the marker, staging and quarantine as they are.
    /// 3. The no-replace probe.
    /// 4. The marker gets a new token: the store records it as pending, the marker takes it, and the store makes it
    ///    current. Each commit this device builds rotates the token the same way before it is sent. A marker that
    ///    cannot be written keeps its token until the next marker write.
    /// 5. The effects in flight are recovered in one section without staging anything again, and acknowledged in one
    ///    transaction. An interrupted one is planned again; one that met another state on disk stays blocked, and so
    ///    does one that recovery found done where L shows another entry
    ///    ([`koidrive_engine::effects::Effects::confirms`]).
    /// 6. Staged files without an effect are deleted.
    ///
    /// The observer then resets the paths of the recovered effects to L, so it reads a file an effect placed before
    /// the crash once, whatever the snapshot it starts from says about that file.
    pub fn start(
        lock: &File,
        link: &Link,
        engine: &mut Engine,
        store: &mut Store,
    ) -> Result<Disk, Stop> {
        Self::start_with_hook(lock, link, engine, store, Box::new(|_| {}))
    }

    fn start_with_hook(
        lock: &File,
        link: &Link,
        engine: &mut Engine,
        store: &mut Store,
        hook: Hook,
    ) -> Result<Disk, Stop> {
        let bound =
            (lock.try_clone()).and_then(|root| Space::bind(OwnedFd::from(root), &link.root));
        let space = match bound {
            Err(error) if error.raw_os_error() == Some(rustix::io::Errno::BADF.raw_os_error()) => {
                return Err(Stop::Withdrawn(link.root.clone()));
            }
            bound => bound?,
        };
        let sections = Arc::new(Sections {
            marks: space.marks(),
            ..Sections::default()
        });
        let mut disk = Disk {
            space: Arc::new(space),
            path: link.root.clone(),
            space_id: link.space_id.clone(),
            sections,
            recorded: BTreeMap::new(),
            spans: VecDeque::new(),
            forgotten: 0,
            unscanned: Unscanned::default(),
            rescan_after: Vec::new(),
            batch: BATCH,
            hook,
            brought_back: BroughtBack::Entries(HashSet::new()),
            current: None,
            next: None,
            unwritable: None,
            #[cfg(test)]
            flush_fails: false,
        };
        let (found, opened) = marker_standing(&disk.space, link, &engine.effects, store)?;
        let recovered = matches!(opened, Standing::StoreOlder);
        if recovered {
            tracing::warn!(
                space_id = link.space_id,
                marker = found.effects,
                cleared = engine.effects.cleared(),
                "the store is older than the folder; recovering"
            );
            bring_back_all(&disk.space, &engine.trees)?;
            store.transaction(|tx| -> Result<_, Stop> {
                engine.effects.catch_up(found.effects);
                engine
                    .commits
                    .enter_recovery(RecoveryCause::of(RecoveryTrigger::StoreOlderThanDisk));
                engine.checkpoint(tx)?;
                tx.set_root_tokens(&RootTokens {
                    current: found.token.clone(),
                    pending: None,
                })?;
                Ok(())
            })?;
        }
        if let Err(err) = disk.space.repair_brought_back() {
            tracing::warn!(space_id = link.space_id, %err, "could not repair what a recovery brought back");
        }
        disk.brought_back = disk.space.brought_back()?;
        if !disk.space.probe_no_replace()? {
            return Err(Stop::NoReplace);
        }
        disk.rotate(found, recovered, store)?;
        let records = engine.effects.in_flight().to_vec();
        if let Some(first) = records.first() {
            let mut translated = (records.iter().map(translate)).collect::<Result<Vec<_>, _>>()?;
            let marker = disk.counter(first.counter);
            let hook = &mut disk.hook;
            let mut step = |step| hook(Cut::Step(step));
            let mut section = disk.space.section(&marker, &mut step)?;
            let now = SystemTime::now();
            for (effect, contents) in &mut translated {
                section.recover(effect, contents, now)?;
            }
            let outcomes: Vec<_> = (records.iter().zip(section.finish()?))
                .map(|(record, outcome)| (record, confirmed(outcome, record, engine)))
                .collect();
            acknowledge(
                &outcomes,
                true,
                &link.space_id,
                pending(&disk.unscanned.all(&[]), &BTreeMap::new()),
                engine,
                store,
                hook,
            )?;
            disk.finish_rotation(store)?;
            if let Standing::StoreOlder =
                standing(&disk.marker()?, &disk.tokens(), &engine.effects, || false)?
            {
                return Err(Stop::Root(RootProblem::BadMarker(io::Error::other(
                    "it changed while the folder's effects were recovered",
                ))));
            }
            let scopes = (translated.iter().zip(&outcomes))
                .flat_map(|((effect, _), (_, outcome))| scopes(&effect.action, outcome));
            disk.sections.request(engine.resets(scopes));
        }
        disk.space.clear_staging(None)?;
        disk.record(engine);
        Ok(disk)
    }

    fn record(&mut self, engine: &Engine) {
        let recorded = engine.recorded();
        if recorded != self.recorded {
            *self.sections.recorded.lock().unwrap() = Some(recorded.clone());
            self.recorded = recorded;
        }
    }

    /// What the observer's thread shares with the effect sections. The observer starts only once [`Disk::start`]
    /// returned, since this is where it gets its stamps.
    pub fn sections(&self) -> Arc<Sections> {
        self.sections.clone()
    }

    /// Reclaims the local trash at `now` with a cap of `cap` bytes ([`Space::reclaim_trash`]). A trashed file is
    /// compared with the content its effect recorded for it.
    pub fn reclaim_trash(&self, now: SystemTime, cap: u64) -> io::Result<Reclaimed> {
        self.space.reclaim_trash(now, cap, &mut |file, content| {
            let Ok(content) = Content::decode(content) else {
                return Ok(false);
            };
            holds(file, Some(&content), false)
        })
    }

    /// Plans the next batch of effects ([`BATCH`]) and records it, with the ID generator, in one store transaction.
    /// Returns `None` when nothing is planned, including while a batch is in flight. The nodes planning unblocks are
    /// stored unblocked also then. A batch with a record that cannot become a `core/fs` effect (a name that is not a
    /// single file name) is never committed, and stops the task.
    pub fn next(
        &mut self,
        engine: &mut Engine,
        store: &mut Store,
    ) -> Result<Option<Pending>, Stop> {
        let now_ms = crate::clock::unix_ms();
        let (space, batch) = (&self.space, self.batch);
        let hook = &mut self.hook;
        let pending = store.transaction(|tx| -> Result<_, Stop> {
            let blocked = engine.effects.blocked().len();
            let listed = list_kept_folders(space, &mut engine.trees)?;
            let records = (engine.effects)
                .plan(
                    &engine.trees,
                    engine.commits.in_flight(),
                    &mut engine.ids,
                    now_ms,
                    batch,
                )
                .to_vec();
            if records.is_empty() {
                if listed || engine.effects.blocked().len() != blocked {
                    engine.checkpoint(tx)?;
                }
                return Ok(None);
            }
            let mut versions = None;
            let effects = (records.into_iter())
                .map(|record| {
                    let (effect, contents) = translate(&record)?;
                    let version = version_of(tx, &mut versions, &record)?;
                    Ok(Planned {
                        record,
                        effect,
                        contents,
                        version,
                    })
                })
                .collect::<Result<Vec<_>, Stop>>()?;
            engine.checkpoint(tx)?;
            hook(Cut::BeforePlan);
            Ok(Some(Pending {
                timing_bytes: None,
                credit: Credit::default(),
                space: space.clone(),
                effects,
            }))
        })?;
        if pending.is_some() {
            hook(Cut::AfterPlan);
        }
        Ok(pending)
    }

    /// The effect section, which has no await. It checks that `staged` is the batch in flight and that the root is
    /// held, applies every effect of the batch in one `core/fs` section with a marker holding the batch's counter,
    /// acknowledges the outcomes in one transaction, and asks the observer to reset the paths the effects acted on to
    /// L as acknowledged. Returns how each effect ended, with its node, in the order of the batch. A write whose
    /// content could not be fetched only drops its staged file, and fails. So does an effect that L no longer takes
    /// ([`EffectRecord::applies`]), because a batch changed L after the plan. Nothing else under the root is touched
    /// for them.
    pub fn apply(
        &mut self,
        staged: Staged,
        engine: &mut Engine,
        store: &mut Store,
    ) -> Result<Vec<(String, Settled)>, Stop> {
        let Staged { mut effects } = staged;
        let records = effects.iter().map(|(planned, _)| &planned.record);
        if !records.eq(engine.effects.in_flight()) {
            return Err(Stop::Record {
                effect_id: (effects.first())
                    .map(|(planned, _)| planned.record.effect_id.clone())
                    .unwrap_or_default(),
                what: "it is not the batch of effects in flight".into(),
            });
        }
        let Some(counter) = effects.first().map(|(planned, _)| planned.record.counter) else {
            return Ok(Vec::new());
        };
        if !self.space.held()? {
            return Err(Stop::Withdrawn(self.path.clone()));
        }
        let marker = self.counter(counter);
        self.sections.epoch.fetch_add(1, Ordering::SeqCst);
        let hook = &mut self.hook;
        let mut step = |step| hook(Cut::Step(step));
        let now = SystemTime::now();
        let mut section = self.space.section(&marker, &mut step)?;
        let mut applied = Vec::with_capacity(effects.len());
        for (planned, unfetchable) in &mut effects {
            let applies = unfetchable.is_none()
                && (planned.record).applies(&engine.trees, engine.commits.in_flight());
            match applies {
                true => section.apply(&planned.effect, &mut planned.contents, now)?,
                false => section.recover(&planned.effect, &mut planned.contents, now)?,
            }
            applied.push(applies);
        }
        let outcomes: Vec<_> = (effects.iter().zip(section.finish()?).zip(applied))
            .map(|(((planned, _), outcome), applied)| match applied {
                true => (&planned.record, outcome),
                false => (&planned.record, confirmed(outcome, &planned.record, engine)),
            })
            .collect();
        let rules_file = |scope: &PathBuf| {
            (scope.file_name()).is_some_and(|name| is_rules_file(&name.to_string_lossy()))
        };
        let mut scopes_of_batch = Vec::new();
        let mut rules_changed = false;
        for ((planned, _), (_, outcome)) in effects.iter().zip(&outcomes) {
            let scopes = scopes(&planned.effect.action, outcome);
            let moved_folder = matches!(planned.effect.action, Action::Move { .. })
                && (engine.trees.local().get(&planned.record.node))
                    .is_some_and(|node| node.shape.kind == Kind::Dir);
            rules_changed |= !scopes.is_empty() && (moved_folder || scopes.iter().any(rules_file));
            scopes_of_batch.extend(scopes);
        }
        let unscanned = match rules_changed {
            true => scopes_of_batch.clone(),
            false => Vec::new(),
        };
        let pending = pending(&self.unscanned.all(&unscanned), &BTreeMap::new());
        let settled = acknowledge(
            &outcomes,
            false,
            &self.space_id,
            pending,
            engine,
            store,
            hook,
        )?;
        self.finish_rotation(store)?;
        let end = self.sections.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        let requested = (self.sections).request(engine.resets(scopes_of_batch.iter().cloned()));
        if rules_changed
            || (settled.iter()).any(|settled| matches!(settled, Settled::AwaitsScan(_)))
        {
            self.unscanned.effects.push((requested, unscanned));
        }
        self.spans.push_back((end, scopes_of_batch));
        while self.spans.len() > SPANS {
            if let Some((end, _)) = self.spans.pop_front() {
                self.forgotten = end;
            }
        }
        Ok((effects.into_iter().zip(settled))
            .map(|((planned, unfetchable), settled)| {
                let settled = match (settled, unfetchable) {
                    (Settled::Failed(_), Some(why)) => Settled::Failed(why),
                    (settled, _) => settled,
                };
                (planned.record.node, settled)
            })
            .collect())
    }

    /// Puts an observation batch into L in one transaction, after giving the trees the rules files it lists. Refuses
    /// a batch scanned from another folder than the locked one, a root that is no longer held, and a batch whose root
    /// check failed. The rules of a batch are taken even when its changes are dropped: the scans report a rules file
    /// only when it changed. Drops the changes at or below
    /// the paths of every effect section that ended after the batch's stamp, and a whole batch older than the
    /// sections remembered. A change at or below a path that a dropped change names is dropped with it: the removals a
    /// scan reports below a folder it saw move belong to that move. The observer then resets every path a dropped change names, both ends of a rename, or
    /// the whole tree for a dropped batch, to L as it is after the batch, so a later batch reports what was lost.
    pub fn observed(
        &mut self,
        batch: Batch,
        engine: &mut Engine,
        store: &mut Store,
    ) -> Result<(), Stop> {
        if let Some(next) = &mut self.next {
            next.for_build = false;
        }
        let Batch {
            stamp,
            answered,
            observation,
            unread: _,
        } = batch;
        if observation.scanned != Some(self.space.id()) {
            return Err(Stop::OtherRoot);
        }
        if !self.space.held()? {
            return Err(Stop::Withdrawn(self.path.clone()));
        }
        observation.root.map_err(Stop::Root)?;
        engine.rules(
            observation.rules,
            observation.tracked,
            observation.all_rules,
        );
        self.rescan_after
            .retain(|requested| !answered.covers(*requested));
        (self.unscanned.effects).retain(|(requested, _)| !answered.covers(*requested));
        (self.unscanned.dropped).retain(|(requested, _)| !answered.covers(*requested));
        if stamp.0 < self.forgotten {
            tracing::debug!(
                space_id = self.space_id.as_str(),
                changes = observation.changes.len(),
                "dropped an observation older than the effects the folder remembers, and reset the whole folder"
            );
            engine.read(&observation.read, &observation.unreadable);
            let root = vec![PathBuf::new()];
            engine.listed(
                &[],
                pending(&self.unscanned.all(&root), &observation.waiting),
            );
            let requested = self.sections.request(engine.resets(root.iter().cloned()));
            self.unscanned.dropped.push((requested, root));
            return Ok(());
        }
        let mut scopes: BTreeSet<PathBuf> = (self.spans.iter())
            .filter(|(end, _)| *end > stamp.0)
            .flat_map(|(_, scopes)| scopes.iter().cloned())
            .collect();
        let mut stale = vec![false; observation.changes.len()];
        let mut grew = true;
        while grew {
            grew = false;
            for (change, stale) in observation.changes.iter().zip(&mut stale) {
                if !*stale
                    && paths(change).any(|path| path.ancestors().any(|at| scopes.contains(at)))
                {
                    *stale = true;
                    grew = true;
                    scopes.extend(paths(change).map(Path::to_path_buf));
                }
            }
        }
        let (read, stale_read): (Vec<PathBuf>, Vec<PathBuf>) = (observation.read.into_iter())
            .partition(|path| !path.ancestors().any(|at| scopes.contains(at)));
        let dropped: BTreeSet<PathBuf> = (observation.changes.iter().zip(&stale))
            .filter(|(_, stale)| **stale)
            .flat_map(|(change, _)| paths(change).map(Path::to_path_buf))
            .chain(stale_read)
            .collect();
        let changes: Vec<Observed> = (observation.changes.into_iter().zip(stale))
            .filter(|(_, stale)| !stale)
            .map(|(change, _)| change)
            .collect();
        let listed: Vec<PathBuf> = (read.iter())
            .filter(|folder| {
                !scopes
                    .iter()
                    .any(|at| at.parent() == Some(folder.as_path()))
            })
            .cloned()
            .collect();
        let mounts = (observation.mounts != engine.mounts).then_some(observation.mounts);
        let mut ctimes = observation.ctimes;
        ctimes.retain(|path, _| !dropped.contains(path));
        self.unscanned.batch = observation.pending;
        let dropped: Vec<PathBuf> = dropped.into_iter().collect();
        let pending = pending(&self.unscanned.all(&dropped), &observation.waiting);
        if !changes.is_empty()
            || mounts.is_some()
            || !ctimes.is_empty()
            || engine.sees_unobserved(&listed, &pending)
        {
            let now_ms = crate::clock::unix_ms();
            store.transaction(|tx| -> Result<(), Stop> {
                if let Some(mounts) = mounts {
                    tx.set_volume_mounts(&mounts)?;
                    engine.mounts = mounts;
                }
                engine.observe(&changes, now_ms);
                engine.read(&read, &observation.unreadable);
                engine.listed(&listed, pending);
                engine.read_ctimes(ctimes);
                Ok(engine.checkpoint(tx)?)
            })?;
        } else {
            engine.read(&read, &observation.unreadable);
        }
        let relist = engine.relist();
        if !relist.is_empty() {
            self.sections.request(engine.resets(relist));
        }
        if !dropped.is_empty() {
            tracing::debug!(
                space_id = self.space_id.as_str(),
                ?dropped,
                "dropped the changes an effect made after the observation began, and reset their paths"
            );
            let requested = self
                .sections
                .request(engine.resets(dropped.iter().cloned()));
            self.unscanned.dropped.push((requested, dropped));
        }
        self.record(engine);
        Ok(())
    }

    /// True from an effect that can have changed which rules apply where until a batch has passed the rules of a scan
    /// of that effect's paths: an effect that wrote, moved or removed a rules file, or that moved a folder. The caller
    /// plans nothing meanwhile (design §5.10). Otherwise what a rules file that was just downloaded excludes would be
    /// downloaded first, and a remote change would be written into a file that the rules file of a moved folder
    /// excludes, because the trees hold each rules file by the path of its folder. A removal of a folder that an entry
    /// L places in it had left waits the same way ([`Settled::AwaitsScan`]): the scan tells L where the entry went,
    /// and until then the same removal would be planned again. The wait ends with the batch that answers the reset the
    /// effect requested ([`Batch::answered`]), never with a batch that only began later: the observer may have taken
    /// its resets before the effect ended, or may have scanned other paths. The observer answers a reset with a batch
    /// also when the scan found nothing, so the wait ends without a change on disk.
    pub fn awaits_scan(&self) -> bool {
        !self.unscanned.effects.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn set_batch(&mut self, batch: BatchLimit) {
        self.batch = batch;
    }

    pub fn refuses(&self, effect: &EngineEffect) -> Option<Refusal> {
        let (action, _) = action(effect).ok()?;
        self.space.refuses(&action).ok().flatten()
    }

    pub fn marks<'a>(&self, trees: &Trees, ids: impl IntoIterator<Item = &'a NodeId>) -> Marks {
        let brought_back = |identity: &_| self.brought_back.holds(file_id(*identity));
        (ids.into_iter())
            .filter(|id| !first_link::at_or_under(trees, id, brought_back))
            .filter_map(|id| {
                let (at, expected) = self.place_of(trees, id)?;
                match self.space.mark_of(&at, expected) {
                    Ok(node) => Some((id.clone(), node?)),
                    Err(err) => {
                        tracing::warn!(space_id = self.space_id, id, %err, "could not read a node mark");
                        None
                    }
                }
            })
            .collect()
    }

    pub fn mark(&self, trees: &Trees, entries: impl IntoIterator<Item = (NodeId, NodeId)>) {
        for (id, node) in entries {
            let Some((at, expected)) = self.place_of(trees, &id) else {
                continue;
            };
            if let Err(err) = self.space.mark(&at, expected, &node) {
                tracing::warn!(space_id = self.space_id, node, %err, "could not mark an entry");
            }
        }
    }

    fn place_of(&self, trees: &Trees, id: &str) -> Option<(Location, FileId)> {
        let at = location(&location_of(trees, id)?).ok()?;
        Some((at, file_id(trees.local().get(id)?.identity)))
    }

    /// Asks the observer to reset `paths` to L and scan them again.
    pub fn rescan(&mut self, engine: &Engine, paths: Vec<PathBuf>) {
        self.rescan_after
            .push(self.sections.request(engine.resets(paths)));
    }

    pub fn awaits_rescan(&self) -> bool {
        !self.rescan_after.is_empty()
    }

    /// The chunks `commit` adds, with where the bytes of each are: for every content the commit creates or
    /// updates, the file of its node in L and the offset of each chunk in it.
    pub fn upload(&self, commit: &Commit, engine: &Engine) -> Upload {
        let mut upload = Upload {
            timing_bytes: None,
            credit: Credit::default(),
            space: self.space.clone(),
            chunks: Vec::new(),
            sources: HashMap::new(),
        };
        for op in &commit.ops {
            let (node, content) = match &op.op {
                Some(op::Op::CreateFile(create)) => (&create.node_id, &create.content),
                Some(op::Op::UpdateContent(update)) => (&update.node_id, &update.content),
                _ => continue,
            };
            let Some(content) = content else { continue };
            let path = engine.path_of(node);
            let at = location_of(&engine.trees, node).and_then(|at| location(&at).ok());
            let identity = (engine.trees.local().get(node)).map(|held| file_id(held.identity));
            let mut offset = 0;
            for chunk in &content.chunks {
                upload.sources.entry(chunk.hash.clone()).or_insert(Source {
                    node: node.clone(),
                    path: path.clone(),
                    at: at.clone(),
                    identity,
                    offset,
                });
                upload.chunks.push(chunk.clone());
                offset += u64::from(chunk.size);
            }
        }
        upload
    }

    fn counter(&self, effects: u64) -> Marker {
        let marker = Marker {
            space_id: self.space_id.clone(),
            effects,
            token: self.current.clone(),
        };
        match &self.next {
            Some(next) => self.space.log_token(&marker, next.token, next.rotation),
            None => marker,
        }
    }

    fn tokens(&self) -> RootTokens {
        RootTokens {
            current: self.current.clone(),
            pending: self.next.as_ref().map(|next| next.token.to_string()),
        }
    }

    fn rotate(&mut self, found: Marker, recovered: bool, store: &mut Store) -> Result<(), Stop> {
        let (token, rotation) = pend_token(&found, recovered, store)?;
        if let Err(error) = self.space.write_rotation(&found, token, rotation) {
            tracing::warn!(space_id = found.space_id, %error, "the folder's marker keeps its token until the next marker write");
            self.current = found.token;
            self.next = Some(NextToken {
                token,
                rotation,
                for_build: false,
                flushed: false,
            });
            return Ok(());
        }
        commit_token(token, store)?;
        self.current = Some(token.to_string());
        Ok(())
    }

    pub fn rotate_before_build(&mut self, store: &mut Store) -> Result<Option<Minted>, Stop> {
        let marker = self.marker()?;
        let next = match &mut self.next {
            Some(next) if next.for_build => next,
            unbuilt => {
                let restart = (unbuilt.as_ref()).is_some_and(|next| {
                    let taken = marker.token == Some(next.token.to_string());
                    next.rotation == Rotation::Restart && !next.flushed && !taken
                });
                let (token, rotation) = pend_token(&marker, restart, store)?;
                (self.hook)(Cut::TokenPended);
                self.current = marker.token.clone();
                unbuilt.insert(NextToken {
                    token,
                    rotation,
                    for_build: true,
                    flushed: false,
                })
            }
        };
        if !next.flushed {
            let written = self
                .space
                .write_rotation(&marker, next.token, next.rotation);
            #[cfg(test)]
            let written = written.and_then(|()| match self.flush_fails {
                true => Err(io::Error::other("the test failed the marker's flush")),
                false => Ok(()),
            });
            if let Err(error) = written {
                if self.unwritable.is_none() {
                    tracing::warn!(space_id = self.space_id, %error, "nothing is uploaded until the folder's marker can be written");
                }
                self.unwritable = Some(error.to_string());
                return Ok(None);
            }
            next.flushed = true;
            (self.hook)(Cut::TokenWritten);
        }
        self.unwritable = None;
        Ok(Some(next.token))
    }

    pub fn built(&mut self, token: Minted) {
        self.next = None;
        self.current = Some(token.to_string());
        (self.hook)(Cut::TokenCommitted);
    }

    #[cfg(test)]
    pub(crate) fn fail_marker_flush(&mut self, fails: bool) {
        self.flush_fails = fails;
    }

    pub fn unwritable(&self) -> Option<&str> {
        self.unwritable.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn set_hook(&mut self, hook: Hook) {
        self.hook = hook;
    }

    fn finish_rotation(&mut self, store: &mut Store) -> Result<(), Stop> {
        if let Some(next) = self.next.take() {
            commit_token(next.token, store)?;
            self.current = Some(next.token.to_string());
        }
        Ok(())
    }

    /// The marker, read through the locked descriptor, for a marker that names this space.
    fn marker(&self) -> Result<Marker, Stop> {
        marker_of(&self.space, &self.space_id)
    }
}

enum Standing {
    Current,
    StoreOlder,
}

fn standing(
    found: &Marker,
    tokens: &RootTokens,
    effects: &Effects,
    descends: impl FnOnce() -> bool,
) -> Result<Standing, Stop> {
    let (marker, cleared) = (found.effects, effects.cleared());
    if marker > cleared + effects.in_flight().len() as u64 {
        return Ok(Standing::StoreOlder);
    }
    match tokens.admits(found) {
        Ok(()) if marker < cleared => Err(Stop::DiskBehind { marker, cleared }),
        Ok(()) => Ok(Standing::Current),
        Err(_) if marker >= cleared && descends() => Ok(Standing::StoreOlder),
        Err(problem) => Err(Stop::Root(problem)),
    }
}

fn marker_standing(
    space: &Space,
    link: &Link,
    effects: &Effects,
    store: &mut Store,
) -> Result<(Marker, Standing), Stop> {
    if !space.held()? {
        return Err(Stop::Withdrawn(link.root.clone()));
    }
    link.root_at()
        .admits(&space.root_at()?)
        .map_err(Stop::Root)?;
    let (tokens, minted) =
        store.read(|tx| Ok::<_, Stop>((tx.root_tokens()?, tx.root_minted()?)))?;
    let found = marker_of(space, &link.space_id)?;
    let own = [minted.as_deref(), tokens.pending.as_deref()];
    let standing = standing(&found, &tokens, effects, || descends(space, &own, &found))?;
    Ok((found, standing))
}

fn descends(space: &Space, own: &[Option<&str>], found: &Marker) -> bool {
    (space.lineage()).is_ok_and(|lineage| lineage.descends(own, found))
}

fn bring_back_all(space: &Space, trees: &Trees) -> io::Result<()> {
    let name = |held: &OsStr, found: FileId| held_name(trees, held, found);
    space.bring_back_all(name, SystemTime::now())?;
    Ok(())
}

fn marker_of(space: &Space, space_id: &str) -> Result<Marker, Stop> {
    let marker = space
        .marker()
        .map_err(|error| Stop::Root(RootProblem::BadMarker(error)))?
        .ok_or(Stop::Root(RootProblem::NoMarker))?;
    if marker.space_id != space_id {
        return Err(Stop::Root(RootProblem::OtherSpace(marker.space_id)));
    }
    Ok(marker)
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum RelinkError {
    #[error("the folder is not halted as an older copy of the folder this link synced")]
    NotACopy,
    #[error(transparent)]
    Stop(#[from] Stop),
}

pub(crate) fn relink(lock: &File, link: &Link, store: &mut Store) -> Result<u64, RelinkError> {
    relink_with_hook(lock, link, store, Box::new(|_| {}))
}

pub(crate) fn relink_with_hook(
    lock: &File,
    link: &Link,
    store: &mut Store,
    mut hook: Hook,
) -> Result<u64, RelinkError> {
    let space = Space::bind(
        OwnedFd::from(lock.try_clone().map_err(Stop::from)?),
        &link.root,
    )
    .map_err(Stop::from)?;
    if !space.held().map_err(Stop::from)? {
        return Err(Stop::Withdrawn(link.root.clone()).into());
    }
    let (stored, before, tokens, minted) = store.transaction(|tx| -> Result<_, Stop> {
        Ok((
            tx.link()?,
            Engine::load(tx, &link.space_id)?,
            tx.root_tokens()?,
            tx.root_minted()?,
        ))
    })?;
    (stored.as_ref().unwrap_or(link).root_at())
        .admits(&space.root_at().map_err(Stop::from)?)
        .map_err(Stop::Root)?;
    let marker = marker_of(&space, &link.space_id)?;
    let own = [minted.as_deref(), tokens.pending.as_deref()];
    if standing(&marker, &tokens, &before.effects, || {
        descends(&space, &own, &marker)
    })
    .is_ok()
    {
        return Err(RelinkError::NotACopy);
    }
    bring_back_all(&space, &before.trees).map_err(Stop::from)?;
    let next = Minted::mint();
    let questions = store.transaction(|tx| -> Result<_, Stop> {
        let current = tx.root_tokens()?.current;
        let answered = tx.questions()?;
        tx.forget_sync_state()?;
        if let Some(stored) = tx.link()? {
            tx.set_link(&Link {
                found_marker: true,
                ..stored
            })?;
        }
        let mut engine = Engine::load(tx, &link.space_id)?;
        engine.effects.catch_up(marker.effects);
        let now_ms = crate::clock::unix_ms();
        (engine.commits).ask_unsettled_restores(&before.commits, &mut engine.ids, now_ms);
        engine.ask(tx, &link.space_id)?;
        for row in &answered {
            if let Some(given) = &row.answer {
                tx.answer_question(&row.question_id, given)?;
            }
        }
        engine.checkpoint(tx)?;
        tx.set_root_tokens(&RootTokens {
            current,
            pending: Some(next.to_string()),
        })?;
        let unanswered = tx
            .questions()?
            .into_iter()
            .filter(|question| question.answer.is_none());
        Ok(unanswered.count() as u64)
    })?;
    hook(Cut::TokenPended);
    (space.write_rotation(&marker, next, Rotation::Restart)).map_err(Stop::from)?;
    hook(Cut::TokenWritten);
    commit_token(next, store)?;
    hook(Cut::TokenCommitted);
    Ok(questions)
}

pub(crate) fn held_name(trees: &Trees, held: &OsStr, found: FileId) -> OsString {
    let id = held.to_string_lossy();
    let same = |node: &&LocalNode| {
        let identity = &node.identity;
        (identity.ino, identity.birth_ns) == (found.ino, found.birth_ns)
    };
    (trees.local().iter().map(|(_, node)| node))
        .find(same)
        .or_else(|| trees.local().get(&id))
        .map(|node| node.shape.name.clone())
        .or_else(|| trees.synced().live(&id).map(|shape| shape.name.clone()))
        .or_else(|| trees.remote().get(&id).map(|node| node.shape.name.clone()))
        .map_or_else(|| held.to_owned(), OsString::from)
}

pub(crate) fn pend_token(
    marker: &Marker,
    restart: bool,
    store: &mut Store,
) -> Result<(Minted, Rotation), Stop> {
    let next = Minted::mint();
    let tokens = RootTokens {
        current: marker.token.clone(),
        pending: Some(next.to_string()),
    };
    let ours = store.transaction(|tx| -> Result<_, Stop> {
        let own = [tx.root_minted()?, tx.root_tokens()?.pending];
        if let Some(taken) = &marker.token {
            tx.commit_pending_as_minted(taken)?;
        }
        tx.set_root_tokens(&tokens)?;
        Ok(marker.token.is_some() && own.contains(&marker.token))
    })?;
    let rotation = match ours && !restart {
        true => Rotation::Extend,
        false => Rotation::Restart,
    };
    Ok((next, rotation))
}

fn commit_token(token: Minted, store: &mut Store) -> Result<(), Stop> {
    store.transaction(|tx| tx.commit_root_token(token))?;
    Ok(())
}

pub(crate) type Sources = BTreeMap<Vec<u8>, (String, Option<PathBuf>)>;

/// The chunks a commit adds to the server and where the bytes of each are in the folder.
pub(crate) struct Upload {
    pub timing_bytes: crate::timing::Bytes,
    pub credit: Credit,
    space: Arc<Space>,
    /// Every chunk of every content the commit creates or updates, in the order of its ops.
    pub chunks: Vec<ChunkRef>,
    sources: HashMap<Vec<u8>, Source>,
}

struct Source {
    node: String,
    /// `None` when L no longer holds the node under the root.
    path: Option<PathBuf>,
    at: Option<Location>,
    identity: Option<FileId>,
    offset: u64,
}

impl Upload {
    /// The bytes of `chunk`, read from the file L holds for its node, through the locked descriptor and without
    /// following a link. Another file at that path is an error. The caller checks the bytes against the chunk's hash.
    pub fn read(&self, chunk: &ChunkRef) -> io::Result<Bytes> {
        let gone = || {
            io::Error::new(
                io::ErrorKind::NotFound,
                "the file is no longer in the folder",
            )
        };
        let source = self.sources.get(&chunk.hash).ok_or_else(gone)?;
        let at = source.at.as_ref().ok_or_else(gone)?;
        let (folder, name) = self.space.parent(at)?;
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let file = File::from(rustix::fs::openat(folder.fd(), name, flags, Mode::empty())?);
        let meta = file.metadata()?;
        let live = koidrive_fs::id_of(file.as_fd())?;
        let same = |id: FileId| meta.is_file() && id.same(&live);
        if !source.identity.is_some_and(same) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "another file is at its path now",
            ));
        }
        let mut bytes = vec![0; chunk.size as usize];
        file.read_exact_at(&mut bytes, source.offset)?;
        Ok(Bytes::from(bytes))
    }

    pub fn sources(&self) -> Sources {
        (self.sources.iter())
            .map(|(hash, source)| (hash.clone(), (source.node.clone(), source.path.clone())))
            .collect()
    }
}

impl Pending {
    /// Downloads the files the batch writes into staging. Every chunk is checked against its hash and size, and each
    /// chunk list against its manifest digest before anything is fetched. The distinct chunks of the whole batch are
    /// fetched in one transfer, so a chunk that several files or offsets hold is downloaded once, and written at every
    /// offset of every file that lists it. A file whose chunks have all arrived is cut to its content's size, given
    /// its modification time and executable bit, and flushed. Other effects stage nothing.
    ///
    /// A content that cannot be fetched does not stall the batch. That is a chunk the server refused as gone or
    /// whose bytes never matched, a malformed manifest, or a content that omits its chunk list when the store's R has
    /// no version with that content. Each write that lists such a chunk fails when the batch is applied, with that
    /// reason, and the transfer goes on for the others. A transfer the server asks to retry later stalls the whole
    /// batch, which is staged again from the start.
    pub async fn stage(&self, fetch: &impl Fetch) -> Result<Staged, Stall> {
        let mut unfetchable: Vec<Option<String>> = vec![None; self.effects.len()];
        let mut writes: Vec<(usize, &Content, Manifest)> = Vec::new();
        for (index, planned) in self.effects.iter().enumerate() {
            let EngineEffect::WriteFile { content, .. } = &planned.record.effect else {
                continue;
            };
            let chunks = if content.chunks.is_empty() && content.chunk_count > 0 {
                let Some(version) = &planned.version else {
                    unfetchable[index] = Some(
                        "its content omits the chunk list, and the remote tree has no version with that content"
                            .into(),
                    );
                    continue;
                };
                match fetch.version_chunks(version).await {
                    Ok(chunks) => chunks,
                    Err(error) => {
                        let id = &planned.record.effect_id;
                        unfetchable[index] = Some(failed_transfer(id, error, false)?);
                        continue;
                    }
                }
            } else {
                content.chunks.clone()
            };
            match Manifest::parse(content, chunks) {
                Ok(manifest) => writes.push((index, content, manifest)),
                Err(why) => unfetchable[index] = Some(why),
            }
        }
        let items: Vec<Item> = (writes.iter())
            .map(|(index, content, _)| Item {
                direction: Direction::Download,
                node: self.effects[*index].record.node.clone(),
                digest: content.manifest_digest.clone(),
            })
            .collect();
        (self.credit).register(
            (items.iter().cloned()).zip(writes.iter().map(|(_, content, _)| content.size)),
        );
        let mut targets: HashMap<ChunkKey, Vec<(usize, &[u64])>> = HashMap::new();
        let mut distinct: Vec<&ChunkRef> = Vec::new();
        for (write, (_, _, manifest)) in writes.iter().enumerate() {
            for chunk in &manifest.distinct {
                let holders = targets.entry(key(chunk)).or_default();
                if holders.is_empty() {
                    distinct.push(chunk);
                }
                holders.push((write, &manifest.offsets[&chunk.hash]));
            }
        }
        let mut missing: Vec<usize> = (writes.iter())
            .map(|(_, _, manifest)| manifest.distinct.len())
            .collect();
        let mut created: HashSet<usize> = HashSet::new();
        let mut received: HashSet<ChunkKey> = HashSet::new();
        let open = |unfetchable: &[Option<String>], missing: &[usize], write: usize| {
            unfetchable[writes[write].0].is_none() && missing[write] > 0
        };
        loop {
            let wanted: Vec<ChunkRef> = (distinct.iter())
                .filter(|chunk| !received.contains(&key(chunk)))
                .filter(|chunk| {
                    (targets[&key(chunk)].iter())
                        .any(|(write, _)| open(&unfetchable, &missing, *write))
                })
                .map(|chunk| (*chunk).clone())
                .collect();
            if wanted.is_empty() {
                break;
            }
            let mut corrupt = None;
            let mut local = None;
            let fetched = fetch
                .download(&wanted, &mut |chunk, bytes| {
                    if !koidrive_net::matches(chunk, bytes) {
                        corrupt = Some(chunk.hash.clone());
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "the bytes do not match the chunk's hash and size",
                        ));
                    }
                    let chunk = key(chunk);
                    self.credit.moved(bytes.len() as u64);
                    for (write, offsets) in targets.get(&chunk).into_iter().flatten() {
                        let (index, content, _) = &writes[*write];
                        if unfetchable[*index].is_some() {
                            continue;
                        }
                        let id = &self.effects[*index].record.effect_id;
                        let file = match created.insert(*write) {
                            true => self.space.stage(id),
                            false => self.space.open_staged(id),
                        };
                        let file = match file {
                            Ok(file) => file,
                            Err(error) => return Err(local.insert(error).kind().into()),
                        };
                        for offset in *offsets {
                            file.write_all_at(bytes, *offset)?;
                        }
                        let written = bytes.len() as u64 * offsets.len() as u64;
                        self.credit.item(&items[*write], &chunk.0, written);
                        missing[*write] -= 1;
                        if missing[*write] == 0 {
                            match finish_file(&file, content) {
                                Ok(why) => unfetchable[*index] = why,
                                Err(error) => return Err(local.insert(error).kind().into()),
                            }
                        }
                    }
                    crate::timing::add_bytes(&self.timing_bytes, bytes.len() as u64);
                    received.insert(chunk);
                    Ok(())
                })
                .await;
            let Err(error) = fetched else {
                break;
            };
            if let Some(error) = local {
                return Err(halt(error));
            }
            let named = match &corrupt {
                Some(hash) => Some(vec![hash.clone()]),
                None => chunk_hashes(&error),
            };
            let holds = |write: usize| {
                named.as_ref().is_none_or(|hashes| {
                    (hashes.iter()).any(|hash| writes[write].2.offsets.contains_key(hash))
                })
            };
            let failing: Vec<usize> = (0..writes.len())
                .filter(|write| open(&unfetchable, &missing, *write) && holds(*write))
                .collect();
            let Some(first) = failing.first() else {
                break;
            };
            let id = &self.effects[writes[*first].0].record.effect_id;
            let why = failed_transfer(id, error, corrupt.is_some())?;
            for write in failing {
                unfetchable[writes[write].0] = Some(why.clone());
            }
        }
        for (write, (index, content, manifest)) in writes.iter().enumerate() {
            if unfetchable[*index].is_some() {
                continue;
            }
            let id = &self.effects[*index].record.effect_id;
            if manifest.distinct.is_empty() {
                let file = self.space.stage(id).map_err(halt)?;
                unfetchable[*index] = finish_file(&file, content).map_err(halt)?;
            } else if missing[write] > 0 {
                let all = manifest.distinct.len();
                unfetchable[*index] = Some(format!(
                    "the transfer handed over {} of its {all} distinct chunks",
                    all - missing[write]
                ));
            }
            if unfetchable[*index].is_none() {
                self.space
                    .finish_staged(id, content.executable)
                    .map_err(halt)?;
            }
        }
        Ok(Staged {
            effects: self.effects.iter().cloned().zip(unfetchable).collect(),
        })
    }
}

/// A chunk by its hash and size.
type ChunkKey = (Vec<u8>, u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Adopted {
    Volume,
    Children(usize),
}

struct LinkedRoot {
    dir: OwnedFd,
    found: RootAt,
    renumbered: bool,
}

fn open_linked_root(link: &Link) -> Result<LinkedRoot, RootProblem> {
    let dir = open_root_folder(&link.root)?;
    let found = root_at(dir.as_fd()).map_err(RootProblem::Unreadable)?;
    let renumbered = match check_root_at(dir.as_fd(), &link.space_id, &link.root_at()) {
        Ok(_) => false,
        Err(RootProblem::Renumbered) if found.volume.is_some() => {
            check_root_at(dir.as_fd(), &link.space_id, &found)?;
            true
        }
        Err(problem) => return Err(problem),
    };
    Ok(LinkedRoot {
        dir,
        found,
        renumbered,
    })
}

struct FoundRoot {
    space: Space,
    recorded: RootAt,
    adopted: Option<Adopted>,
}

fn find_root(link: &mut Link, store: &mut Store) -> Result<FoundRoot, Stop> {
    if let Some(stored) = store.read(|tx| tx.link())? {
        link.root_dev = stored.root_dev;
        link.root_volume = stored.root_volume;
        link.found_marker = stored.found_marker;
    }
    let recorded = link.root_at();
    let unreadable = |error| Stop::Root(RootProblem::Unreadable(error));
    let LinkedRoot {
        dir,
        found,
        renumbered,
    } = open_linked_root(link).map_err(Stop::Root)?;
    let space = Space::bind(dir, &link.root).map_err(unreadable)?;
    let adopted = if renumbered {
        let engine = store.read(|tx| Engine::load(tx, &link.space_id))?;
        match matching_children(&space, &engine.trees).map_err(unreadable)? {
            0 => return Err(Stop::Root(RootProblem::Renumbered)),
            matched => Some(Adopted::Children(matched)),
        }
    } else {
        (found.dev != recorded.dev).then_some(Adopted::Volume)
    };
    link.root_dev = found.dev;
    link.root_volume = found.volume;
    Ok(FoundRoot {
        space,
        recorded,
        adopted,
    })
}

pub(crate) fn admitted(link: &Link, store: &mut Store) -> Result<Space, Stop> {
    let mut link = link.clone();
    let FoundRoot { space, .. } = find_root(&mut link, store)?;
    let effects = store.read(|tx| crate::engine::effects_of(tx.engine_state()?.effects))?;
    marker_standing(&space, &link, &effects, store)?;
    Ok(space)
}

pub(crate) fn admit_root(link: &mut Link, store: &mut Store) -> Result<Option<Adopted>, Stop> {
    let FoundRoot {
        recorded, adopted, ..
    } = find_root(link, store)?;
    if link.root_at() == recorded {
        return Ok(None);
    }
    store.transaction(|tx| tx.set_link(link))?;
    if let Some(adopted) = adopted {
        tracing::warn!(
            space_id = link.space_id,
            old_dev = recorded.dev,
            new_dev = link.root_dev,
            ?adopted,
            "the folder's disk is mounted under another device number; syncing it again"
        );
    }
    Ok(adopted)
}

fn matching_children(space: &Space, trees: &Trees) -> io::Result<usize> {
    let local = trees.local();
    let mut matched = 0;
    for child in trees.local_children(&local.root) {
        let Some(node) = local.get(child).filter(|node| !node.parked) else {
            continue;
        };
        match space.child_id(OsStr::new(&node.shape.name))? {
            None => {}
            Some(found) if found.same(&file_id(node.identity)) => matched += 1,
            Some(_) => return Ok(0),
        }
    }
    Ok(matched)
}

fn key(chunk: &ChunkRef) -> ChunkKey {
    (chunk.hash.clone(), chunk.size)
}

fn chunk_hashes(error: &ChunkError) -> Option<Vec<Vec<u8>>> {
    match error {
        ChunkError::Call(CallError::Server(refusal))
            if refusal.code() == Code::MissingChunks && !refusal.0.missing_chunks.is_empty() =>
        {
            Some(refusal.0.missing_chunks.clone())
        }
        ChunkError::Call(_) => None,
        ChunkError::SourceChanged { hash }
        | ChunkError::Corrupt { hash }
        | ChunkError::Refused { hash, .. }
        | ChunkError::Unavailable { hash, .. }
        | ChunkError::NoTarget { hash }
        | ChunkError::Io { hash, .. } => Some(vec![hash.clone()]),
    }
}

/// Cuts a staged file whose chunks are all written to its content's size and gives it the content's modification
/// time. Returns why the effect fails when the file does not then have that size.
fn finish_file(file: &File, content: &Content) -> io::Result<Option<String>> {
    file.set_len(content.size)?;
    if file.metadata()?.size() != content.size {
        return Ok(Some(
            "the staged file does not have the content's size".into(),
        ));
    }
    let mtime = rustix::fs::Timespec {
        tv_sec: content.mtime_ns.div_euclid(1_000_000_000),
        tv_nsec: content.mtime_ns.rem_euclid(1_000_000_000) as _,
    };
    rustix::fs::futimens(
        file,
        &rustix::fs::Timestamps {
            last_access: mtime,
            last_modification: mtime,
        },
    )?;
    Ok(None)
}

/// What a failed transfer means for the writes that list the chunk it names, or for every write of the batch still
/// waiting for chunks when it names none: a chunk the server no longer has, or whose bytes never matched, fails them
/// with the reason returned; a refusal the server asks to retry stalls the batch; anything else halts the task with
/// the batch in flight, naming `effect_id`.
fn failed_transfer(effect_id: &str, error: ChunkError, corrupt: bool) -> Result<String, Stall> {
    match corrupt {
        true => failures::failed(Failure::Download(failures::Transfer::Corrupt)),
        false => failures::transfer_failed(&error, Failure::Download),
    }
    let disposition = match &error {
        _ if corrupt => None,
        ChunkError::Corrupt { .. } | ChunkError::NoTarget { .. } => None,
        ChunkError::Refused {
            status: 404 | 410, ..
        } => None,
        ChunkError::Refused {
            error: Some(refusal),
            ..
        } => Some(policy::after_refusal(refusal)),
        ChunkError::Call(call)
            if matches!(call.code(), Some(Code::NotFound | Code::MissingChunks)) =>
        {
            None
        }
        ChunkError::Call(call) => Some(policy::after_call_error(call)),
        ChunkError::Refused { .. } | ChunkError::Unavailable { .. } => Some(Disposition::Backoff),
        ChunkError::SourceChanged { .. } | ChunkError::Io { .. } => Some(Disposition::Halt),
    };
    let why = match &error {
        ChunkError::Refused {
            error: Some(refusal),
            ..
        }
        | ChunkError::Call(CallError::Server(refusal)) => policy::describe(refusal),
        other => other.to_string(),
    };
    let kind = policy::wait_kind(match &error {
        ChunkError::Refused {
            error: Some(refusal),
            ..
        }
        | ChunkError::Call(CallError::Server(refusal)) => Some(refusal),
        _ => None,
    });
    let unreachable = policy::unreachable(&error);
    let later = |wait| Stall::Later {
        wait,
        why: format!("downloading a file's content failed: {why}"),
        kind,
        unreachable,
    };
    match disposition {
        None => Ok(format!("its content cannot be downloaded: {error}")),
        Some(Disposition::RetryAfter(wait)) => Err(later(wait)),
        Some(Disposition::Backoff) => Err(later(STAGE_AGAIN)),
        Some(Disposition::Halt) => {
            let what = format!("its content could not be downloaded: {error}");
            Err(Stall::Halt(match &error {
                ChunkError::Call(call) => Stop::after_call(call, what),
                ChunkError::Refused { .. } => Stop::Refused(what),
                _ => Stop::Record {
                    effect_id: effect_id.to_owned(),
                    what,
                },
            }))
        }
    }
}

fn halt(error: io::Error) -> Stall {
    Stall::Halt(Stop::Io(error))
}

/// Records the outcome of each effect of the batch in flight, in the batch's order, in one transaction: done with the
/// identity of the entry it created, or failed. The mass-delete guard counts a removal done for another device's
/// deletion, and a notice it opens or updates is stored in the same transaction. With `retry`, an interrupted effect
/// is unblocked in the same transaction, so it is planned again. An outcome that L no longer takes
/// ([`EffectRecord::applies`]) is never written to L, whatever the disk holds. The effect fails and is unblocked, and
/// the planner leaves it out until L changes.
fn acknowledge(
    outcomes: &[(&EffectRecord, Outcome)],
    retry: bool,
    space_id: &str,
    pending: koidrive_engine::classify::Pending,
    engine: &mut Engine,
    store: &mut Store,
    hook: &mut Hook,
) -> Result<Vec<Settled>, Stop> {
    let settled = store.transaction(|tx| -> Result<_, Stop> {
        let settled = (outcomes.iter())
            .map(|(record, outcome)| settle(record, outcome, retry, engine))
            .collect::<Result<Vec<_>, Stop>>()?;
        engine.ask(tx, space_id)?;
        engine.listed(&[], pending);
        engine.checkpoint(tx)?;
        hook(Cut::BeforeAck);
        Ok(settled)
    })?;
    hook(Cut::AfterAck);
    Ok(settled)
}

/// Gives the engine the outcome of one effect of the batch in flight.
fn settle(
    record: &EffectRecord,
    outcome: &Outcome,
    retry: bool,
    engine: &mut Engine,
) -> Result<Settled, Stop> {
    let refused = |error| Stop::Record {
        effect_id: record.effect_id.clone(),
        what: format!("the engine refused its outcome: {error:?}"),
    };
    {
        let settled = match outcome {
            _ if !record.applies(&engine.trees, engine.commits.in_flight()) => {
                engine
                    .effects
                    .failed(&mut engine.trees, &record.effect_id)
                    .map_err(refused)?;
                engine.effects.unblock(&record.node);
                Settled::Failed("L changed since it was planned".into())
            }
            Outcome::Done { .. } if !record.left_nothing_behind(&engine.trees) => {
                engine
                    .effects
                    .failed(&mut engine.trees, &record.effect_id)
                    .map_err(refused)?;
                let why = "an entry left the folder before it was removed".to_owned();
                match &record.effect {
                    EngineEffect::RemoveDir {
                        at:
                            EngineLocation::At {
                                base: EngineBase::Root,
                                ..
                            },
                        ..
                    } => {
                        engine.effects.unblock(&record.node);
                        Settled::AwaitsScan(why)
                    }
                    _ => Settled::Stuck(why),
                }
            }
            Outcome::Done { identity } => {
                let removal = (engine.effects)
                    .done(
                        &mut engine.trees,
                        engine.commits.in_flight(),
                        &record.effect_id,
                        identity.map(file_identity),
                    )
                    .map_err(refused)?;
                if let Some(removal) = removal {
                    let now_ms = crate::clock::unix_ms();
                    let Engine {
                        trees,
                        commits,
                        ids,
                        cursor,
                        ..
                    } = engine;
                    let last_seq = cursor.as_ref().map_or(0, |cursor| cursor.last_seq);
                    commits.count_remote(removal, trees, last_seq, ids, now_ms);
                }
                Settled::Done
            }
            Outcome::Failed {
                reason: Reason::Holds(names),
                ..
            } if (engine.effects).keeps(&engine.trees, &record.effect_id, &held(names)) => {
                let held = held(names);
                (engine.effects)
                    .kept(&mut engine.trees, &record.effect_id, &held)
                    .map_err(refused)?;
                tracing::info!(
                    entries = names.len(),
                    "a folder another device deleted stays here with the entries this device does not sync"
                );
                Settled::Done
            }
            Outcome::Failed { reason, .. } => {
                match reason {
                    Reason::Overtaken => {
                        (engine.effects).overtaken(&mut engine.trees, &record.effect_id)
                    }
                    _ => engine.effects.failed(&mut engine.trees, &record.effect_id),
                }
                .map_err(refused)?;
                if retry && *reason == Reason::Interrupted {
                    engine.effects.unblock(&record.node);
                }
                let failed = |why: &str| Settled::Failed(why.into());
                match reason {
                    Reason::Moved => failed("a folder on its path is missing or was replaced"),
                    Reason::Missing => failed("the entry is not there"),
                    Reason::Changed => failed("the entry changed on this device"),
                    Reason::Taken => failed("another entry holds its name"),
                    Reason::Interrupted => failed("it was interrupted"),
                    Reason::Denied => Settled::Denied(
                        "this device does not allow the change: the folder or the file is read-only or locked; \
                         Koidrive tries again by itself once it can write there"
                            .into(),
                    ),
                    Reason::Overtaken => {
                        failed("the file it replaced is gone and its name changed on this device")
                    }
                    Reason::Holds(names) => {
                        let quoted: Vec<_> = (names.iter())
                            .map(|(name, _)| koidrive_fs::quoted(name).to_string())
                            .collect();
                        Settled::Stuck(format!(
                            "the folder holds entries this device does not sync: {}",
                            quoted.join(", ")
                        ))
                    }
                }
            }
        };
        Ok(settled)
    }
}

fn list_kept_folders(space: &Space, trees: &mut Trees) -> Result<bool, Stop> {
    let folders = trees.unlisted_kept_folders();
    for folder in &folders {
        let Some(identity) = trees.local().get(folder).map(|node| node.identity) else {
            continue;
        };
        let Some(EngineLocation::At {
            base,
            dirs,
            name: last,
        }) = location_of(trees, folder)
        else {
            continue;
        };
        let dirs = (dirs.iter().chain([&EngineDir {
            name: last,
            identity,
        }]))
        .map(|dir| {
            Ok(Dir {
                name: name(&dir.name)?,
                id: file_id(dir.identity),
            })
        })
        .collect::<Result<Vec<_>, String>>();
        let base = match base {
            EngineBase::Root => Ok(Base::Root),
            EngineBase::Parked { node, identity } => name(&node).map(|name| Base::Parked {
                name,
                id: file_id(identity),
            }),
        };
        let (Ok(base), Ok(dirs)) = (base, dirs) else {
            continue;
        };
        let entries = match space.entries(&base, &dirs) {
            Ok(entries) => entries,
            Err(
                Unresolved::Missing(_) | Unresolved::NotAFolder(_) | Unresolved::Replaced { .. },
            ) => Vec::new(),
            Err(unresolved) => return Err(Stop::Io(unresolved.into())),
        };
        let entries = (entries.into_iter()).filter_map(|(name, kind)| {
            let kind = kind.map(|kind| match kind {
                EntryKind::File => Kind::File,
                EntryKind::Dir => Kind::Dir,
            });
            Some((name.into_string().ok()?, kind))
        });
        trees.list_kept_folder(folder, entries);
    }
    Ok(!folders.is_empty())
}

fn held(names: &[(OsString, Option<EntryKind>)]) -> Vec<(&str, Option<Kind>)> {
    (names.iter())
        .filter_map(|(name, kind)| {
            let kind = kind.map(|kind| match kind {
                EntryKind::File => Kind::File,
                EntryKind::Dir => Kind::Dir,
            });
            Some((name.to_str()?, kind))
        })
        .collect()
}

/// `outcome` as recovery reports it for `record`, an effect in flight, unless L refutes a done
/// ([`koidrive_engine::effects::Effects::confirms`]). Then the effect fails as it would have on the entry recovery
/// found: a removal finds its entry missing, and a write or a new folder finds its name taken.
fn confirmed(outcome: Outcome, record: &EffectRecord, engine: &Engine) -> Outcome {
    let Outcome::Done { identity } = &outcome else {
        return outcome;
    };
    let identity = identity.map(file_identity);
    if (engine.effects).confirms(&engine.trees, &record.effect_id, identity.as_ref()) {
        return outcome;
    }
    let reason = match &record.effect {
        EngineEffect::RemoveDir { .. } | EngineEffect::RemoveFile { .. } => Reason::Missing,
        _ => Reason::Taken,
    };
    Outcome::Failed {
        reason,
        recovered: None,
    }
}

/// The version of a write whose content omits its chunk list: the version R holds for the node, if it has that
/// content. `versions` keeps the current version of each node of the store's R once a write of the batch read them.
fn version_of(
    tx: &Tx<'_>,
    versions: &mut Option<HashMap<String, VersionInfo>>,
    record: &EffectRecord,
) -> Result<Option<String>, Stop> {
    let EngineEffect::WriteFile { content, .. } = &record.effect else {
        return Ok(None);
    };
    if !content.chunks.is_empty() || content.chunk_count == 0 {
        return Ok(None);
    }
    if versions.is_none() {
        let current = (tx.remote_nodes()?.into_iter())
            .filter_map(|node| Some((node.node_id, node.current?)))
            .collect();
        *versions = Some(current);
    }
    Ok((versions.as_ref())
        .and_then(|versions| versions.get(&record.node))
        .filter(|version| {
            (version.content.as_ref())
                .is_some_and(|found| found.manifest_digest == content.manifest_digest)
        })
        .map(|version| version.version_id.clone()))
}

/// The paths an effect acted on, relative to the root: its target, the source of a move or of a replacement that
/// also moves the node, and a recovered file. Paths inside the private folder are left out, since no scan reports
/// them.
fn scopes(action: &Action, outcome: &Outcome) -> Vec<PathBuf> {
    let locations = match action {
        Action::CreateDir { at } => vec![at],
        Action::RemoveDir { at, along, .. } => std::iter::once(at)
            .chain(along.iter().map(|file| &file.at))
            .collect(),
        Action::WriteFile { at, replaces } => std::iter::once(at)
            .chain(replaces.iter().map(|d| &d.at))
            .collect(),
        Action::Move { from, to, .. } => vec![from, to],
        Action::RemoveFile { displaced } => vec![&displaced.at],
    };
    let mut scopes: Vec<PathBuf> = locations
        .into_iter()
        .filter_map(|location| match location {
            Location::At {
                base: Base::Root,
                dirs,
                name,
            } => Some(
                dirs.iter()
                    .map(|dir| dir.name.as_os_str())
                    .chain([name.as_os_str()])
                    .collect(),
            ),
            Location::At { .. } | Location::Parked(_) => None,
        })
        .collect();
    if let Outcome::Failed {
        recovered: Some(path),
        ..
    } = outcome
    {
        scopes.push(path.clone());
    }
    scopes.dedup();
    scopes
}

/// The paths a change names.
fn paths(change: &Observed) -> impl Iterator<Item = &Path> {
    let (first, second) = match change {
        Observed::Created { path, .. }
        | Observed::Modified { path, .. }
        | Observed::Removed { path, .. }
        | Observed::Missing { path, .. }
        | Observed::Reidentified { path, .. } => (path, None),
        Observed::Renamed { from, to, .. } => (from, Some(to)),
    };
    std::iter::once(first.as_path()).chain(second.map(PathBuf::as_path))
}

/// The record as a `core/fs` effect, with the contents it expects. One match covers every effect, so a new kind
/// fails to build here. Names keep their exact bytes; a name that is not a single file name is refused. Expected
/// entries come from the identities and contents the record holds, never from the disk.
fn translate(record: &EffectRecord) -> Result<(Effect, Expected), Stop> {
    let bad = |what: String| Stop::Record {
        effect_id: record.effect_id.clone(),
        what,
    };
    name(&record.effect_id).map_err(bad)?;
    let (action, contents) = action(&record.effect).map_err(bad)?;
    let effect = Effect {
        id: record.effect_id.clone(),
        node: record.node.clone(),
        action,
    };
    Ok((effect, contents))
}

fn action(effect: &EngineEffect) -> Result<(Action, Expected), String> {
    Ok(match effect {
        EngineEffect::CreateDir { at } => {
            (Action::CreateDir { at: location(at)? }, Expected::default())
        }
        EngineEffect::WriteFile {
            at,
            content,
            replaces,
        } => (
            Action::WriteFile {
                at: location(at)?,
                replaces: replaces.as_ref().map(displaced).transpose()?,
            },
            Expected {
                displaced: replaces.as_ref().map(|d| d.content.clone()),
                written: Some(content.clone()),
                along: Vec::new(),
            },
        ),
        EngineEffect::Move { from, identity, to } => (
            Action::Move {
                from: location(from)?,
                id: file_id(*identity),
                to: location(to)?,
            },
            Expected::default(),
        ),
        EngineEffect::RemoveFile { displaced: removed } => (
            Action::RemoveFile {
                displaced: displaced(removed)?,
            },
            Expected {
                displaced: Some(removed.content.clone()),
                written: None,
                along: Vec::new(),
            },
        ),
        EngineEffect::RemoveDir {
            at,
            identity,
            along,
        } => (
            Action::RemoveDir {
                at: location(at)?,
                id: file_id(*identity),
                along: along.iter().map(displaced).collect::<Result<_, _>>()?,
            },
            Expected {
                along: along.iter().map(|file| file.content.clone()).collect(),
                ..Expected::default()
            },
        ),
    })
}

fn location(location: &EngineLocation) -> Result<Location, String> {
    Ok(match location {
        EngineLocation::Parked(node) => Location::Parked(name(node)?),
        EngineLocation::At {
            base,
            dirs,
            name: last,
        } => Location::At {
            base: match base {
                EngineBase::Root => Base::Root,
                EngineBase::Parked { node, identity } => Base::Parked {
                    name: name(node)?,
                    id: file_id(*identity),
                },
            },
            dirs: dirs
                .iter()
                .map(|dir| {
                    Ok(Dir {
                        name: name(&dir.name)?,
                        id: file_id(dir.identity),
                    })
                })
                .collect::<Result<_, String>>()?,
            name: name(last)?,
        },
    })
}

/// The entry a file effect displaces, as the record expects it.
fn displaced(displaced: &EngineDisplaced) -> Result<Displaced, String> {
    Ok(Displaced {
        at: location(&displaced.at)?,
        entry: Entry {
            kind: EntryKind::File,
            id: file_id(displaced.identity),
            size: displaced.content.size,
            mtime_ns: i128::from(displaced.content.mtime_ns),
            ctime: Ctime::Settled { at: 0, mode: 0 },
            executable: displaced.content.executable,
            linked: false,
            mount: None,
        },
    })
}

/// `value` as a file name with the same bytes, if it is a single one.
fn name(value: &str) -> Result<OsString, String> {
    if matches!(value, "" | "." | "..") || value.contains(['/', '\0']) {
        return Err(format!("{value:?} is not a single file name"));
    }
    Ok(OsString::from(value))
}

/// A written content's chunk list, checked: a known chunker, as many 32-byte raw chunks as the content counts,
/// one size per hash, sizes that add up to the content's size, and the content's manifest digest.
struct Manifest {
    /// Each hash once, in order of first appearance.
    distinct: Vec<ChunkRef>,
    /// Where each hash appears.
    offsets: BTreeMap<Vec<u8>, Vec<u64>>,
}

impl Manifest {
    fn parse(content: &Content, chunks: Vec<ChunkRef>) -> Result<Manifest, String> {
        ChunkerConfig::from_name(&content.chunker).map_err(|error| {
            format!(
                "its chunker {:?} is not supported: {error}",
                content.chunker
            )
        })?;
        if chunks.len() != content.chunk_count as usize {
            return Err(format!(
                "its content counts {} chunks and lists {}",
                content.chunk_count,
                chunks.len()
            ));
        }
        if koidrive_chunk::manifest_digest(&chunks).as_slice() != content.manifest_digest {
            return Err("its chunk list does not match its manifest digest".into());
        }
        let mut sizes: BTreeMap<&[u8], u32> = BTreeMap::new();
        let mut offsets: BTreeMap<Vec<u8>, Vec<u64>> = BTreeMap::new();
        let mut distinct = Vec::new();
        let mut size = 0u64;
        for chunk in &chunks {
            if chunk.hash.len() != 32 || chunk.encoding != 0 || chunk.size == 0 {
                return Err("it lists a chunk that is not a raw chunk with a 32-byte hash".into());
            }
            if *sizes.entry(chunk.hash.as_slice()).or_insert(chunk.size) != chunk.size {
                return Err("it lists one chunk hash with two sizes".into());
            }
            let at = offsets.entry(chunk.hash.clone()).or_default();
            if at.is_empty() {
                distinct.push(chunk.clone());
            }
            at.push(size);
            size = size
                .checked_add(u64::from(chunk.size))
                .ok_or("its chunk sizes overflow")?;
        }
        if size != content.size {
            return Err(format!(
                "its chunks add up to {size} bytes, not to its size of {}",
                content.size
            ));
        }
        Ok(Manifest { distinct, offsets })
    }
}

/// The contents an effect expects: the displaced file's and the written file's, compared by size and manifest
/// digest, chunked with each content's own chunker. A written file must also have the content's executable bit.
#[derive(Clone, Debug, Default)]
struct Expected {
    displaced: Option<Content>,
    written: Option<Content>,
    along: Vec<Content>,
}

impl Contents for Expected {
    fn displaced(&mut self, file: &File) -> io::Result<bool> {
        holds(file, self.displaced.as_ref(), false)
    }

    fn written(&mut self, file: &File) -> io::Result<bool> {
        holds(file, self.written.as_ref(), true)
    }

    fn along(&mut self, index: usize, file: &File) -> io::Result<bool> {
        holds(file, self.along.get(index), false)
    }

    fn displaced_content(&self) -> Option<Vec<u8>> {
        let displaced = self.displaced.as_ref()?;
        let content = Content {
            size: displaced.size,
            manifest_digest: displaced.manifest_digest.clone(),
            chunker: displaced.chunker.clone(),
            ..Content::default()
        };
        Some(content.encode_to_vec())
    }
}

/// Whether `file` holds `expected`, read from its start. A file that changes while it is read does not.
fn holds(file: &File, expected: Option<&Content>, executable: bool) -> io::Result<bool> {
    let Some(expected) = expected else {
        return Ok(false);
    };
    let Ok(config) = ChunkerConfig::from_name(&expected.chunker) else {
        return Ok(false);
    };
    let before = file.metadata()?;
    if before.size() != expected.size
        || (executable && (before.mode() & 0o111 != 0) != expected.executable)
    {
        return Ok(false);
    }
    let chunks = koidrive_chunk::chunks(Positioned { file, at: 0 }, config)
        .map(|chunk| chunk.map(|chunk| chunk.chunk_ref()))
        .collect::<io::Result<Vec<ChunkRef>>>()?;
    let after = file.metadata()?;
    let same = |meta: &std::fs::Metadata| {
        (
            meta.size(),
            meta.mtime(),
            meta.mtime_nsec(),
            meta.ctime(),
            meta.ctime_nsec(),
        )
    };
    Ok(same(&before) == same(&after)
        && koidrive_chunk::manifest_digest(&chunks).as_slice() == expected.manifest_digest)
}

/// Reads a file from `at` with positional reads, whatever its descriptor's offset.
struct Positioned<'a> {
    file: &'a File,
    at: u64,
}

impl io::Read for Positioned<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.file.read_at(buf, self.at)?;
        self.at += read as u64;
        Ok(read)
    }
}

#[cfg(test)]
pub(crate) mod tests;
