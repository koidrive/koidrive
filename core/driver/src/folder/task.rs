// SPDX-License-Identifier: Apache-2.0

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use koidrive_engine::ignore::LocalRules;
use koidrive_fs::Snapshot;
use koidrive_net::{
    Auth, Backoff, CallError, ChunkError, Session, SessionConfig, StopReason, Transfer, UploadGate,
};
use koidrive_proto::v1::{CommitResult, UploadReceipt};
use koidrive_state_file::Flock;
use koidrive_store::{Link, Store};
use koidrive_telemetry::RootProof;
use koidrive_telemetry::schema::magnitude;
use tokio::sync::{Notify, mpsc, watch};

use crate::claim::{self, Identity, Owner};
use crate::credential::SET_UP_AGAIN;
use crate::disk::{self, Adopted, Batch, Pending, Sections, Staged, Stall, Stop};
use crate::driver::check_placement;
use crate::failures::{self, Halted};
use crate::ignore::Matcher;
use crate::observe::{FsObserver, Observer, Observing};
use crate::progress::Tracker;
use crate::timing::Time;

use super::commit::sent_in_order;
use super::work::{Slot, finished, list_tree, stage_effect, upload_chunks};
use super::{
    CLAIM_RECHECK, Folder, Listed, Listing, MAX_FORMAT, OTHER_FOLDER, RECLAIM_TRASH, REPLACED,
    ROOT_AGAIN, ROOT_RECHECK, View, Work,
};

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
    pub observing: Arc<Mutex<Option<Observing>>>,
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
            let ended = self.status.lock().unwrap().end_transfers();
            if ended {
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
        let (link, adopted) = match self.admit_root().await {
            Ok(admitted) => admitted,
            Err(halt) => return halt,
        };
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
        let mut folder = match Folder::open(link, &lock, &self.store, local) {
            Ok(folder) => folder,
            Err(stop) => return stop.into(),
        };
        if let Some(adopted) = adopted {
            let (proof, children) = match adopted {
                Adopted::Volume => (RootProof::Volume, None),
                Adopted::Children(matched) => {
                    (RootProof::Children, Some(magnitude(matched as u64)))
                }
            };
            self.telemetry
                .event(koidrive_telemetry::Event::RootAdopted {
                    at: koidrive_telemetry::now(),
                    proof,
                    children_magnitude: children,
                });
        }
        folder.timing.linked(self.linked_started);
        folder.timing.enable(self.telemetry.clone());
        folder.progress = self.progress.clone();
        (folder.progress.lock().unwrap()).publish_to(
            self.status.clone(),
            self.progressed.clone(),
            self.totals.clone(),
            self.link.space_id.clone(),
        );
        let mut from_disk = self.observe(
            folder.link.clone(),
            lock.clone(),
            folder.sections(),
            folder.snapshot(),
        );
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
            let mut expire: Slot<'_, ()> = None;
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
                    () = finished(&mut expire) => Ok(Vec::new()),
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
                        Work::List => {
                            let listing = Listing::default();
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
                expire = match folder.next_change() {
                    Some(after) => Some(Box::pin(tokio::time::sleep(after))),
                    None => None,
                };
            }
        };
        session.close().await;
        stop
    }

    /// Waits until the root passes the check of design §6.2: nothing is synced for a folder that is missing,
    /// unmounted or marked for another space.
    async fn admit_root(&self) -> Result<(Link, Option<Adopted>), Halt> {
        loop {
            let mut link = self.link.clone();
            let admitted = Store::open_existing(&self.store)
                .map_err(Stop::from)
                .and_then(|mut store| disk::admit_root(&mut link, &mut store));
            match admitted {
                Ok(adopted) => return Ok((link, adopted)),
                Err(Stop::Root(problem)) => {
                    tracing::warn!(space_id = self.link.space_id, %problem, "the folder fails its root check");
                    self.status.halt(
                        Halted::Root,
                        format!("{problem}; checking again in 30 seconds"),
                        crate::lifecycle::Recovery::Root,
                    );
                    self.changed.send_replace(());
                    tokio::time::sleep(ROOT_RECHECK).await;
                }
                Err(Stop::Store(error)) => {
                    return Err(Halt::Person(
                        Halted::Store,
                        crate::lifecycle::unopened(&self.store, &error),
                    ));
                }
                Err(stop) => return Err(stop.into()),
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
        link: Link,
        lock: Arc<Flock>,
        sections: Arc<Sections>,
        previous: Snapshot,
    ) -> mpsc::UnboundedReceiver<FromDisk> {
        let (sender, receiver) = mpsc::unbounded_channel();
        let matcher = self.matcher.clone();
        *self.observing.lock().unwrap() = None;
        let walked = Arc::new(AtomicBool::new(false));
        let report = reporter(
            self.observing.clone(),
            self.progressed.clone(),
            walked.clone(),
            crate::driver::PROGRESS_INTERVAL,
        );
        #[cfg(test)]
        let fault = self.observe_fault.clone();
        let thread = std::thread::spawn(move || {
            let _lock = lock;
            let closed = sender.clone();
            let started = FsObserver::start(
                &link.root,
                &link.space_id,
                link.root_at(),
                previous,
                matcher,
                move || closed.is_closed(),
                report,
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
                    walked.store(true, Ordering::Relaxed);
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
        (self.status).set_pending(folder.engine.trees.dirty().len() as u64);
        folder.record_status(view.state);
        let began_or_ended = folder.recount_progress(view.state);
        let activity = folder.activity(view.state);
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
            wait: status.wait.clone(),
            questions: status.questions,
            recovery: status.recovery,
            held_disks: status.held_disks.clone(),
        };
        if shown == view && status.activity == activity && !asked && !began_or_ended {
            return;
        }
        status.activity = activity;
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
        status.held_disks = view.held_disks;
        drop(status);
        self.changed.send_replace(());
    }
}

pub(super) fn reporter(
    observing: Arc<Mutex<Option<Observing>>>,
    progressed: Arc<watch::Sender<()>>,
    walked: Arc<AtomicBool>,
    interval: Duration,
) -> impl Fn(Observing) + Send + Sync + 'static {
    let last_shown: Mutex<Option<Instant>> = Mutex::default();
    move |report: Observing| {
        let mut shown = observing.lock().unwrap();
        let report = match (report, *shown) {
            (Observing::Walking { files }, Some(Observing::Walking { files: before })) => {
                Observing::Walking {
                    files: files.max(before),
                }
            }
            _ => report,
        };
        let phase = shown.map(|shown| std::mem::discriminant(&shown));
        *shown = Some(report);
        drop(shown);
        if matches!(report, Observing::Walking { .. }) && walked.load(Ordering::Relaxed) {
            return;
        }
        let mut last = last_shown.lock().unwrap();
        let due = last.is_none_or(|at| at.elapsed() >= interval);
        if due || phase != Some(std::mem::discriminant(&report)) {
            *last = Some(Instant::now());
            progressed.send_replace(());
        }
    }
}
