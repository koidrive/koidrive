// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use koidrive_engine::classify::Class;
use koidrive_engine::commit::Halt as CommitHalt;
use koidrive_engine::effects::{self, Stuck};
use koidrive_engine::tree::{Content, Kind, NodeId, Shape, same_content};
use koidrive_fs::Detached;
use koidrive_net::{Cause, DisconnectReason};

use crate::clock;
use crate::driver::PROGRESS_INTERVAL;
use crate::failures;
use crate::progress::{Direction, Item};
use crate::status::{
    Activity, FolderState, HeldDisk, HeldDiskKind, Unreachable, Wait, WaitKind, describe,
};
use crate::timing::Time;

use super::commit::lift_hint;
use super::{
    Disconnected, Flight, Folder, QUICK_RECONNECT, RECOUNT_SPACING, RETRY_SHOWN, Retrying,
    STUCK_SHOWN, Stream, View,
};

impl Folder {
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
        let now = clock::system();
        let reconnect = (self.disconnected.as_ref())
            .filter(|disconnected| disconnected.shown && disconnected.retry_at > now);
        let waiting: Vec<&Retrying> = (self.retrying.values())
            .filter(|retrying| retrying.at > now)
            .collect();
        let unreachable_call = (waiting.iter()).find(|retrying| retrying.unreachable.is_some());
        let reconnecting = (self.disconnected.as_ref()).filter(|disconnected| {
            disconnected.retry_at <= now
                && disconnected.reason.unreachable()
                && (disconnected.shown
                    || clock::instant().saturating_duration_since(disconnected.since)
                        >= QUICK_RECONNECT)
        });
        let state = match &self.stream {
            _ if reconnect.is_some_and(|disconnected| disconnected.reason.unreachable())
                || unreachable_call.is_some()
                || reconnecting.is_some() =>
            {
                FolderState::Offline
            }
            _ if self.disconnected.is_some() => FolderState::Connecting,
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
        let mut restart = (self.engine.effects.blocked().keys())
            .any(|node| !self.denied.contains_key(node))
            || !held.is_empty();
        for id in self.engine.effects.blocked().keys() {
            let why = (self.effect_failures.get(id)).map_or("it failed earlier", |(_, why)| why);
            let why = format!("the server's change could not be made here: {why}");
            reasons.entry(id).or_default().push(why);
        }
        for id in self.engine.commits.problems() {
            let left_out = (self.unreadable.get(id))
                .and_then(|unreadable| Some((unreadable, unreadable.left_out.as_ref()?)));
            let why = match left_out {
                Some((unreadable, left_out)) => format!(
                    "it could not be read: {}; trying again at {}",
                    unreadable.why,
                    utc(left_out.at)
                ),
                None => {
                    restart = true;
                    (self.refused_nodes.get(id))
                        .map_or("the server did not take it", String::as_str)
                        .to_owned()
                }
            };
            reasons.entry(id).or_default().push(why);
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
        let shown = (waiting.iter()).find(|retrying| {
            retrying.wait >= RETRY_SHOWN
                || clock::instant().saturating_duration_since(retrying.since) >= RETRY_SHOWN
        });
        let unix_ms =
            |at: SystemTime| (at.duration_since(UNIX_EPOCH)).map_or(0, |at| at.as_millis() as i64);
        let call = |retrying: &&Retrying| {
            let unreachable = (retrying.unreachable).map(|cause| Unreachable {
                why: cause.to_string(),
                no_internet: cause == Cause::NoInternet,
            });
            (retrying.kind, Some(retrying.at), unreachable)
        };
        let session = |disconnected: &Disconnected, at| {
            let unreachable = disconnected_unreachable(&disconnected.reason);
            (WaitKind::Retry, at, unreachable)
        };
        let wait = (reconnect.filter(|disconnected| disconnected.reason.unreachable()))
            .map(|disconnected| session(disconnected, Some(disconnected.retry_at)))
            .or_else(|| unreachable_call.map(call))
            .or_else(|| reconnecting.map(|disconnected| session(disconnected, None)))
            .or_else(|| {
                reconnect.map(|disconnected| session(disconnected, Some(disconnected.retry_at)))
            })
            .or_else(|| (waiting.iter().max_by_key(|retrying| retrying.at)).map(call));
        let offline = (wait.as_ref())
            .filter(|_| state == FolderState::Offline)
            .and_then(|(_, at, unreachable)| {
                let when = at.map_or_else(|| "now".to_owned(), |at| format!("at {}", utc(at)));
                Some((unreachable.as_ref()?, when))
            })
            .map(|(unreachable, when)| match unreachable.no_internet {
                true => format!("no internet connection; trying again {when}"),
                false => format!(
                    "can't connect to the server: {}; trying again {when}",
                    unreachable.why
                ),
            });
        let refused = reconnect
            .filter(|disconnected| !disconnected.reason.unreachable())
            .map(|disconnected| {
                format!(
                    "{}; trying again at {}",
                    disconnected.reason,
                    utc(disconnected.retry_at)
                )
            });
        let wait = wait.map(|(kind, at, unreachable)| Wait {
            kind,
            until_ms: at.map(unix_ms),
            unreachable,
        });
        let retrying =
            shown.map(|retrying| format!("{}; trying again at {}", retrying.why, utc(retrying.at)));
        let problem = match self.engine.commits.halted() {
            _ if offline.is_some() => offline,
            _ if refused.is_some() => refused,
            _ if retrying.is_some() => retrying,
            _ if self.disk.unwritable().is_some() => self.disk.unwritable().map(|why| {
                format!(
                    "changes made here wait to be uploaded until this folder can be written: {why}"
                )
            }),
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
                let restart = match restart {
                    true => format!("; run koi folder restart once {fixed} fixed"),
                    false => String::new(),
                };
                Some(format!(
                    "{count} could not be synced: {}{more}{restart}",
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
            held_disks: (self.detached.iter())
                .map(|(path, why)| HeldDisk {
                    path: path.to_string_lossy().into_owned().into(),
                    kind: match why {
                        Detached::Away => HeldDiskKind::NotConnected,
                        Detached::Covered => HeldDiskKind::MountedOver,
                    },
                })
                .collect(),
            kept_entries: (self.engine.trees.kept_entry_paths().into_iter())
                .map(|names| names.join("/").into())
                .collect(),
        }
    }

    /// How long until a wait the status shows ends, so the status can be shown again then.
    pub fn next_change(&self) -> Option<Duration> {
        let now = clock::system();
        let grace = (self.disconnected.iter()).filter_map(|disconnected| {
            QUICK_RECONNECT
                .checked_sub(clock::instant().saturating_duration_since(disconnected.since))
        });
        (self
            .disconnected
            .iter()
            .map(|disconnected| disconnected.retry_at))
        .chain(self.retrying.values().map(|retrying| retrying.at))
        .filter_map(|at| at.duration_since(now).ok())
        .chain(grace)
        .chain(self.recount_due())
        .min()
    }

    fn recount_due(&self) -> Option<Duration> {
        let at = self.recount_at.filter(|_| self.recount_waits)?;
        Some(at.saturating_duration_since(Instant::now()))
    }

    pub fn recount_progress(&mut self, state: FolderState) -> bool {
        let counting = matches!(state, FolderState::Bootstrapping | FolderState::CatchingUp);
        let started = Instant::now();
        if counting && self.recount_at.is_some_and(|at| started < at) {
            self.recount_waits = true;
            return false;
        }
        let changed = self.track_progress(state);
        self.recount_waits = false;
        self.recount_at = counting
            .then(|| Instant::now() + PROGRESS_INTERVAL.max(started.elapsed() * RECOUNT_SPACING));
        changed
    }

    fn held(&self) -> BTreeMap<String, Stuck> {
        match (
            self.engine.effects.in_flight(),
            self.engine.commits.in_flight(),
        ) {
            ([], None) => {
                let now_ms = (SystemTime::now().duration_since(UNIX_EPOCH))
                    .map_or(0, |since| since.as_millis() as u64);
                let renaming = self.engine.commits.renaming(&self.engine.trees, now_ms);
                effects::stuck(&self.engine.trees, &self.engine.effects, None, &renaming)
            }
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
        let asked: BTreeSet<&NodeId> = (self.engine.commits.questions().iter())
            .flat_map(|question| question.holds())
            .collect();
        let mut other_changes = 0;
        let mut waiting = BTreeSet::new();
        for id in (trees.dirty().iter()).filter(|id| !stays_out(id)) {
            if trees.held(id) {
                if asked.contains(id) {
                    waiting.insert(id);
                }
                continue;
            }
            let local = file(trees.local().live(id));
            let synced = file(trees.synced().live(id));
            let remote = file(trees.remote().live(id));
            let class = trees.class(id);
            let mut moves = false;
            if let Some(local) = local
                && matches!(class, Class::Local | Class::Concurrent)
                && other(local, remote)
                && (other(local, synced) || trees.unsent_edits().contains(id))
            {
                want(Direction::Upload, id, local);
                moves = true;
            }
            if let Some(remote) = remote
                && matches!(class, Class::Remote | Class::Concurrent)
                && other(remote, synced)
                && other(remote, local)
            {
                want(Direction::Download, id, remote);
                moves = true;
            }
            match (moves, asked.contains(id)) {
                (true, _) => {}
                (false, true) => {
                    waiting.insert(id);
                }
                (false, false) => other_changes += 1,
            }
        }
        let questions_holding_changes = (self.engine.commits.questions().iter())
            .filter(|question| question.holds().any(|id| waiting.contains(id)))
            .count() as u64;
        drop(stays_out);
        let synced = |item: &Item| {
            let holds = match item.direction {
                Direction::Upload => file(trees.synced().live(&item.node)),
                Direction::Download => file(trees.local().live(&item.node)),
            };
            holds.is_some_and(|content| content.manifest_digest == item.digest)
        };
        let began_or_ended = self.progress.lock().unwrap().track(wanted, synced);
        self.other_changes = other_changes;
        self.questions_holding_changes = questions_holding_changes;
        began_or_ended
    }

    pub fn activity(&self, state: FolderState) -> Option<Activity> {
        if !matches!(state, FolderState::Bootstrapping | FolderState::CatchingUp) {
            return None;
        }
        if !self.scanned {
            return Some(Activity::Scanning { files: 0 });
        }
        if matches!(self.stream, Stream::Listing { .. } | Stream::Replaying(_)) {
            return Some(Activity::Listing);
        }
        if let (files @ 1.., bytes) = self.unread {
            return Some(Activity::Checking { files, bytes });
        }
        if self.progress.lock().unwrap().progress().is_some() {
            return None;
        }
        match (self.other_changes, self.questions_holding_changes) {
            (0, 0) => Some(Activity::CatchingUp),
            (0, questions) => Some(Activity::AwaitingAnswers { questions }),
            (changes, _) => Some(Activity::Applying { changes }),
        }
    }

    /// The path of a node relative to the root: where L holds it, or else where R does, as for a file whose download
    /// failed or that this device deleted.
    pub(super) fn item_path(&self, id: &str) -> Option<PathBuf> {
        (self.engine.path_of(id)).or_else(|| self.engine.trees.remote().path(id))
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
            && !self.disk.awaits_scan()
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

    pub(super) fn record_status(&mut self, state: FolderState) {
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

/// The time of day of `at` in UTC, such as `00:00 UTC`.
fn utc(at: SystemTime) -> String {
    let seconds = at
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
        % 86_400;
    format!("{:02}:{:02} UTC", seconds / 3_600, seconds / 60 % 60)
}

fn disconnected_unreachable(reason: &DisconnectReason) -> Option<Unreachable> {
    reason.unreachable().then(|| Unreachable {
        why: reason.to_string(),
        no_internet: matches!(reason, DisconnectReason::Transport(transport) if transport.cause == Cause::NoInternet),
    })
}
