// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use koidrive_engine::effects::{Effect, EffectRecord, Location};
use koidrive_engine::tree::{RemoteNode, same};
use koidrive_proto::v1::{
    ChangeRecord, Commit, CommitRecord, CommitResult, change_record, op, op_result,
};
use koidrive_telemetry::{
    InstallId, Recorder,
    schema::{FileCount, Measure},
};

use crate::status::FolderState;

pub(crate) type Bytes = Option<Arc<AtomicU64>>;

const MAX_DURATION_MS: u64 = 30 * 86_400_000;

pub(crate) fn add_bytes(bytes: &Bytes, count: u64) {
    if let Some(bytes) = bytes {
        let _ = bytes.try_update(Ordering::Relaxed, Ordering::Relaxed, |held| {
            Some(held.saturating_add(count))
        });
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Time {
    pub mono: Instant,
    pub unix_ms: i64,
}

impl Time {
    pub fn now() -> Self {
        Self {
            mono: Instant::now(),
            unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()
                .and_then(|time| time.as_millis().try_into().ok())
                .unwrap_or(0),
        }
    }

    fn hour(self) -> u32 {
        u32::try_from(self.unix_ms.max(0) / 3_600_000).unwrap_or(u32::MAX)
    }
}

pub(crate) struct Timing {
    recorder: Recorder,
    state: Live,
    first: Option<First>,
}

#[derive(Default)]
struct Live(Option<(u64, Box<State>)>);

impl Live {
    fn get(&mut self, recorder: &Recorder) -> Option<&mut State> {
        let Some(session) = recorder.session() else {
            self.0 = None;
            return None;
        };
        if self.0.as_ref().is_none_or(|(held, _)| *held != session) {
            self.0 = Some((session, Box::default()));
        }
        self.0.as_mut().map(|(_, state)| &mut **state)
    }
}

struct First {
    path: PathBuf,
    linked_ms: i64,
    install: String,
    session: Option<u64>,
    opened: Time,
}

#[derive(Default)]
struct State {
    syncing: Option<Instant>,
    download: Burst,
    upload: Burst,
    origins: BTreeMap<String, Origin>,
    effects: BTreeMap<String, Planned>,
}

#[derive(Default)]
struct Burst {
    start: Option<Instant>,
    files: BTreeSet<(String, String)>,
    bytes: Arc<AtomicU64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Origin {
    seq: u64,
    revision: Option<(u64, u64, bool)>,
    committed_ms: i64,
    received: Time,
}

struct Planned {
    node: String,
    wrote_file: bool,
    final_target: bool,
    origin: Option<Origin>,
    version: String,
}

impl Timing {
    pub fn initialize(store: &Path, linked_ms: i64, recorder: &Recorder) {
        let Some(install) = recorder.install_id() else {
            return;
        };
        let path = store.with_file_name("first-sync");
        let _ = (|| -> std::io::Result<()> {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&path)?;
            write!(file, "{linked_ms} {}", fingerprint(&install))?;
            file.sync_all()?;
            File::open(path.parent().unwrap())?.sync_all()
        })();
    }

    pub fn open(store: &Path, linked_ms: i64) -> Self {
        Self::open_at(store, linked_ms, Time::now())
    }

    fn open_at(store: &Path, linked_ms: i64, opened: Time) -> Self {
        let path = store.with_file_name("first-sync");
        let install = fs::read_to_string(&path).ok().and_then(|value| {
            let (linked, install) = value.split_once(' ')?;
            (linked.parse::<i64>().ok()? == linked_ms).then(|| install.trim().to_owned())
        });
        Self {
            recorder: Recorder::off(),
            state: Live::default(),
            first: install.map(|install| First {
                path,
                linked_ms,
                install,
                session: None,
                opened,
            }),
        }
    }

    pub fn enable(&mut self, recorder: Recorder) {
        self.state = Live::default();
        self.recorder = recorder;
        self.check_first();
    }

    fn check_first(&mut self) {
        let Some(first) = &mut self.first else { return };
        let session = self.recorder.session();
        let unbroken = session.is_some()
            && self.recorder.install_id().as_ref().map(fingerprint) == Some(first.install.clone())
            && first.session.is_none_or(|held| Some(held) == session);
        if unbroken {
            first.session = session;
        } else {
            let _ = fs::remove_file(&first.path);
            self.first = None;
        }
    }

    pub fn linked(&mut self, started: Option<Time>) {
        if let Some(started) = started
            && let Some(first) = &mut self.first
            && first.linked_ms == started.unix_ms
        {
            first.opened = started;
        }
    }

    pub fn is_on(&self) -> bool {
        self.recorder.is_on()
    }

    pub fn has_first(&self) -> bool {
        self.first.is_some()
    }

    pub fn first_pending(&mut self) -> bool {
        self.check_first();
        self.first.is_some()
    }

    pub fn first_sync(&mut self, files: u64, at: Time) {
        self.check_first();
        let Some(first) = self.first.take() else {
            return;
        };
        if fs::remove_file(&first.path).is_err() {
            return;
        }
        if File::open(first.path.parent().unwrap())
            .and_then(|dir| dir.sync_all())
            .is_err()
        {
            return;
        }
        let Some(before) = wall_duration(first.linked_ms, first.opened.unix_ms) else {
            return;
        };
        if wall_duration(first.linked_ms, at.unix_ms).is_none() {
            return;
        }
        let Some(elapsed) = at.mono.checked_duration_since(first.opened.mono) else {
            return;
        };
        let duration = before.saturating_add(millis(elapsed));
        if duration > MAX_DURATION_MS {
            return;
        }
        self.recorder.measure_on(
            at.hour(),
            Measure::FirstSync(FileCount::bucket(files)),
            duration,
        );
    }

    pub fn status(&mut self, status: FolderState, at: Time) {
        self.check_first();
        let Some(state) = self.state.get(&self.recorder) else {
            return;
        };
        let syncing = matches!(status, FolderState::Bootstrapping | FolderState::CatchingUp);
        if syncing {
            state.syncing.get_or_insert(at.mono);
        } else if let Some(start) = state.syncing.take()
            && status == FolderState::UpToDate
        {
            self.recorder.measure_on(
                at.hour(),
                Measure::SyncingMilliseconds,
                millis(at.mono.saturating_duration_since(start)).min(MAX_DURATION_MS),
            );
        }
    }

    pub fn drained(&mut self, at: Time) {
        let Some(state) = self.state.get(&self.recorder) else {
            return;
        };
        state.download.finish(
            &self.recorder,
            at,
            Measure::DownloadFilesPerSecond,
            Measure::DownloadBytesPerSecond,
        );
        state.upload.finish(
            &self.recorder,
            at,
            Measure::UploadFilesPerSecond,
            Measure::UploadBytesPerSecond,
        );
        state.effects.clear();
    }

    pub fn records(
        &mut self,
        records: &[CommitRecord],
        before: u64,
        through: u64,
        device: &str,
        at: Time,
    ) {
        if self.state.get(&self.recorder).is_none() {
            return;
        }
        let mut last = before;
        for record in records {
            if record.seq <= last {
                continue;
            }
            if record.seq > through || record.seq != last + 1 {
                break;
            }
            self.changes(
                &record.changes,
                record.seq,
                if record.device_id == device {
                    0
                } else {
                    record.committed_at_ms
                },
                at,
            );
            last = record.seq;
        }
    }

    pub fn changes(&mut self, changes: &[ChangeRecord], seq: u64, committed_ms: i64, at: Time) {
        let Some(state) = self.state.get(&self.recorder) else {
            return;
        };
        for change in changes {
            let (node, revision) = match &change.change {
                Some(change_record::Change::Node(node)) => (
                    &node.node_id,
                    Some((
                        node.content_rev,
                        node.meta_rev,
                        node.status == koidrive_proto::v1::NodeStatus::Trashed as i32,
                    )),
                ),
                Some(change_record::Change::Purged(node)) => (&node.node_id, None),
                _ => continue,
            };
            state.origins.remove(node);
            if wall_duration(committed_ms, at.unix_ms).is_some() {
                state.origins.insert(
                    node.clone(),
                    Origin {
                        seq,
                        revision,
                        committed_ms,
                        received: at,
                    },
                );
            }
        }
    }

    pub fn clear_origins(&mut self) {
        if let Some(state) = self.state.get(&self.recorder) {
            state.origins.clear();
        }
    }

    pub fn forget(&mut self, changes: &[ChangeRecord]) {
        let Some(state) = self.state.get(&self.recorder) else {
            return;
        };
        for change in changes {
            match &change.change {
                Some(change_record::Change::Node(node)) => {
                    state.origins.remove(&node.node_id);
                }
                Some(change_record::Change::Purged(node)) => {
                    state.origins.remove(&node.node_id);
                }
                _ => {}
            }
        }
    }

    pub fn planned(
        &mut self,
        record: &EffectRecord,
        remote: Option<&RemoteNode>,
        at: Time,
    ) -> Bytes {
        let state = self.state.get(&self.recorder)?;
        let revision = remote.map(|node| (node.content_rev, node.meta_rev, node.trashed));
        let origin = state
            .origins
            .get(&record.node)
            .filter(|origin| origin.revision == revision)
            .cloned();
        state.effects.insert(
            record.effect_id.clone(),
            Planned {
                node: record.node.clone(),
                wrote_file: matches!(record.effect, Effect::WriteFile { .. }),
                final_target: !matches!(
                    record.effect,
                    Effect::Move {
                        to: Location::Parked(_),
                        ..
                    }
                ) && same(
                    record.after.as_ref(),
                    remote.filter(|node| !node.trashed).map(|node| &node.shape),
                ),
                origin,
                version: remote
                    .map_or_else(|| record.effect_id.clone(), |node| node.version_id.clone()),
            },
        );
        if matches!(record.effect, Effect::WriteFile { .. }) {
            state.download.start.get_or_insert(at.mono);
            Some(state.download.bytes.clone())
        } else {
            None
        }
    }

    pub fn applied(&mut self, effect_id: &str, success: bool, at: Time) {
        let Some(state) = self.state.get(&self.recorder) else {
            return;
        };
        let Some(planned) = state.effects.remove(effect_id) else {
            return;
        };
        if !success {
            return;
        }
        if let Some(origin) = planned.origin
            && state.origins.get(&planned.node) == Some(&origin)
            && let Some(before) = wall_duration(origin.committed_ms, origin.received.unix_ms)
            && let Some(elapsed) = at.mono.checked_duration_since(origin.received.mono)
        {
            let duration = before.saturating_add(millis(elapsed));
            if duration <= MAX_DURATION_MS {
                self.recorder
                    .measure_on(at.hour(), Measure::RemoteApplyMilliseconds, duration);
            }
            if planned.final_target {
                state.origins.remove(&planned.node);
            }
        }
        if planned.wrote_file {
            state.download.files.insert((planned.node, planned.version));
        }
    }

    pub fn uploading(&mut self, commit: &Commit, at: Time) -> Bytes {
        let state = self.state.get(&self.recorder)?;
        if commit.ops.iter().any(|op| {
            matches!(
                op.op,
                Some(op::Op::CreateFile(_) | op::Op::UpdateContent(_))
            )
        }) {
            state.upload.start.get_or_insert(at.mono);
        }
        Some(state.upload.bytes.clone())
    }

    pub fn uploaded(&mut self, commit: &Commit, result: &CommitResult) {
        let Some(state) = self.state.get(&self.recorder) else {
            return;
        };
        if state.upload.start.is_none() {
            return;
        }
        for (index, (op, result)) in commit.ops.iter().zip(&result.results).enumerate() {
            if matches!(
                op.op,
                Some(op::Op::CreateFile(_) | op::Op::UpdateContent(_))
            ) && matches!(
                op_result::Code::try_from(result.code),
                Ok(op_result::Code::Applied
                    | op_result::Code::AppliedRenamed
                    | op_result::Code::ConflictCopy)
            ) {
                state
                    .upload
                    .files
                    .insert((commit.commit_id.clone(), index.to_string()));
            }
        }
    }
}

fn fingerprint(install: &InstallId) -> String {
    Sha256::digest(install.as_str().as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

impl Drop for Timing {
    fn drop(&mut self) {
        if self.is_on() {
            self.status(FolderState::Stopped, Time::now());
        }
    }
}

impl Burst {
    fn finish(&mut self, recorder: &Recorder, at: Time, files: Measure, bytes: Measure) {
        if let Some(start) = self.start.take() {
            let elapsed = at.mono.saturating_duration_since(start).as_nanos();
            let transferred = self.bytes.swap(0, Ordering::Relaxed);
            if !self.files.is_empty() && elapsed > 0 {
                recorder.measure_on(at.hour(), files, rate(self.files.len() as u64, elapsed));
                recorder.measure_on(at.hour(), bytes, rate(transferred, elapsed));
            }
            self.files.clear();
        }
    }
}

fn rate(count: u64, nanos: u128) -> u64 {
    u64::try_from(u128::from(count) * 1_000_000_000 / nanos.max(1)).unwrap_or(u64::MAX)
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn wall_duration(start: i64, end: i64) -> Option<u64> {
    if start <= 0 {
        return None;
    }
    u64::try_from(end.checked_sub(start)?)
        .ok()
        .filter(|elapsed| *elapsed <= MAX_DURATION_MS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use koidrive_proto::v1::{Content, NodeKind, NodeState, NodeStatus, Op, OpResult, VersionInfo};
    use serde_json::{Value, json};

    fn time() -> Time {
        Time {
            mono: Instant::now(),
            unix_ms: 2 * 3_600_000 - 30_000,
        }
    }

    fn later(at: Time, duration: Duration) -> Time {
        Time {
            mono: at.mono + duration,
            unix_ms: at.unix_ms + duration.as_millis() as i64,
        }
    }

    fn tracker(recorder: &Recorder) -> Timing {
        let mut timing = Timing {
            recorder: Recorder::off(),
            state: Live::default(),
            first: None,
        };
        timing.enable(recorder.clone());
        timing
    }

    #[test]
    fn a_folder_opened_before_reports_were_on_times_syncs_once_they_are() {
        let recorder = Recorder::off();
        let mut timing = tracker(&recorder);
        recorder.switch(true);
        let at = time();
        timing.status(FolderState::CatchingUp, at);
        timing.status(FolderState::UpToDate, later(at, Duration::from_secs(1)));
        assert!(
            !reports(&recorder)["1"]["histograms"]["syncing_ms"]["completed"].is_null(),
            "no syncing sample after reports were turned on"
        );
    }

    #[test]
    fn a_first_sync_is_timed_only_if_reports_stayed_on_since_the_link() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store.sqlite");
        let at = time();
        let first_sync = |timing: &mut Timing, recorder: &Recorder| {
            timing.enable(recorder.clone());
            timing.first_sync(1, later(at, Duration::from_secs(5)));
            recorder.take().hours.is_empty()
        };

        let off = Recorder::off();
        Timing::initialize(&store, at.unix_ms, &off);
        let on = Recorder::on();
        assert!(
            first_sync(&mut Timing::open_at(&store, at.unix_ms, at), &on),
            "linked while off"
        );

        Timing::initialize(&store, at.unix_ms, &on);
        let mut timing = Timing::open_at(&store, at.unix_ms, at);
        timing.enable(on.clone());
        on.switch(false);
        on.switch(true);
        assert!(first_sync(&mut timing, &on), "off and on in between");
        assert!(!Timing::open_at(&store, at.unix_ms, at).has_first());

        Timing::initialize(&store, at.unix_ms, &on);
        let restarted = Recorder::on();
        assert!(
            first_sync(&mut Timing::open_at(&store, at.unix_ms, at), &restarted),
            "another install id after a restart"
        );

        Timing::initialize(&store, at.unix_ms, &on);
        let marker = fs::read_to_string(store.with_file_name("first-sync")).unwrap();
        assert!(
            !marker.contains(on.install_id().unwrap().as_str()),
            "{marker}"
        );
        assert!(!first_sync(
            &mut Timing::open_at(&store, at.unix_ms, at),
            &on
        ));
    }

    #[test]
    fn a_span_that_reports_were_off_for_part_of_is_dropped() {
        let recorder = Recorder::on();
        let mut timing = tracker(&recorder);
        let at = time();
        timing.status(FolderState::CatchingUp, at);
        recorder.switch(false);
        recorder.switch(true);
        timing.status(FolderState::UpToDate, later(at, Duration::from_secs(10)));
        assert!(recorder.take().hours.is_empty());
    }

    #[test]
    fn an_upload_that_finished_while_reports_were_off_is_dropped() {
        let recorder = Recorder::on();
        let mut timing = tracker(&recorder);
        let at = time();
        let commit = commit();
        let bytes = timing.uploading(&commit, at);
        recorder.switch(false);
        add_bytes(&bytes, 10_000);
        timing.uploaded(
            &commit,
            &CommitResult {
                results: vec![OpResult {
                    code: op_result::Code::Applied as i32,
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
        recorder.switch(true);
        timing.drained(later(at, Duration::from_secs(10)));
        assert!(recorder.take().hours.is_empty());
    }

    fn reports(recorder: &Recorder) -> Value {
        serde_json::to_value(recorder.take().hours).unwrap()
    }

    fn assert_duration_bucket(histogram: &Value, bucket: usize) {
        let mut buckets = [0; 16];
        buckets[bucket] = 1;
        assert_eq!(
            histogram,
            &json!({ "shift": 10, "buckets": buckets, "count": 1 })
        );
    }

    fn node(revision: u64) -> RemoteNode {
        RemoteNode::from_state(&NodeState {
            parent_id: "root".into(),
            name: "secret-name".into(),
            kind: NodeKind::File as i32,
            status: NodeStatus::Live as i32,
            content_rev: revision,
            meta_rev: 1,
            current: Some(VersionInfo {
                version_id: format!("version-{revision}"),
                content: Some(Content::default()),
                ..Default::default()
            }),
            ..Default::default()
        })
        .unwrap()
        .1
    }

    fn effect(id: &str) -> EffectRecord {
        let remote = node(1);
        EffectRecord {
            effect_id: id.into(),
            node: "secret-node-id".into(),
            counter: 1,
            effect: Effect::WriteFile {
                at: Location::Parked("secret-node-id".into()),
                content: Content::default(),
                replaces: None,
            },
            after: Some(remote.shape),
        }
    }

    fn record(seq: u64, committed_ms: i64, revision: u64) -> CommitRecord {
        CommitRecord {
            seq,
            committed_at_ms: committed_ms,
            device_id: "other-device".into(),
            changes: vec![ChangeRecord {
                change: Some(change_record::Change::Node(NodeState {
                    node_id: "secret-node-id".into(),
                    content_rev: revision,
                    meta_rev: 1,
                    status: NodeStatus::Live as i32,
                    ..Default::default()
                })),
            }],
            ..Default::default()
        }
    }

    fn commit() -> Commit {
        Commit {
            commit_id: "private-commit".into(),
            ops: vec![Op {
                op: Some(op::Op::CreateFile(koidrive_proto::v1::CreateFile {
                    node_id: "secret-node-id".into(),
                    content: Some(Content::default()),
                    ..Default::default()
                })),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn first_sync_is_correlated_durable_once_and_legacy_links_are_excluded() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store.sqlite");
        let recorder = Recorder::on();
        let at = time();
        let mut legacy = Timing::open_at(&store, 1, at);
        legacy.enable(recorder.clone());
        legacy.first_sync(100_000, at);
        assert!(recorder.take().hours.is_empty());
        Timing::initialize(&store, at.unix_ms - 5000, &recorder);
        drop(Timing::open_at(&store, at.unix_ms - 5000, at));
        let mut timing = Timing::open_at(&store, at.unix_ms - 5000, at);
        timing.enable(recorder.clone());
        timing.first_sync(100_000, at);
        timing.first_sync(400_000, at);
        drop(timing);
        let mut reopened = Timing::open_at(&store, at.unix_ms - 5000, at);
        reopened.enable(recorder.clone());
        reopened.first_sync(400_000, later(at, Duration::from_secs(100)));
        let value = reports(&recorder);
        assert_duration_bucket(
            &value["1"]["histograms"]["first_sync_ms"]["65536_131071"],
            3,
        );
        assert_eq!(
            value["1"]["histograms"]["first_sync_ms"]
                .as_object()
                .unwrap()
                .len(),
            1
        );
        Timing::initialize(&store, at.unix_ms, &recorder);
        let mut relinked = Timing::open_at(&store, at.unix_ms, at);
        relinked.enable(recorder.clone());
        relinked.first_sync(400_000, later(at, Duration::from_secs(1)));
        assert_duration_bucket(
            &reports(&recorder)["1"]["histograms"]["first_sync_ms"]["262144_524287"],
            0,
        );
    }

    #[test]
    fn first_sync_io_failure_mismatched_link_and_off_do_not_record() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store.sqlite");
        let at = time();
        let recorder = Recorder::on();
        Timing::initialize(&store, at.unix_ms, &recorder);
        assert!(!Timing::open_at(&store, at.unix_ms + 1, at).has_first());
        let mut timing = Timing::open_at(&store, at.unix_ms, at);
        timing.enable(recorder.clone());
        fs::remove_file(store.with_file_name("first-sync")).unwrap();
        fs::create_dir(store.with_file_name("first-sync")).unwrap();
        timing.first_sync(1, at);
        assert!(recorder.take().hours.is_empty());
        fs::remove_dir(store.with_file_name("first-sync")).unwrap();
        Timing::initialize(&store, at.unix_ms, &recorder);
        let mut off = Timing::open_at(&store, at.unix_ms, at);
        off.first_sync(1, at);
        assert!(!Timing::open_at(&store, at.unix_ms, at).has_first());
        assert!(off.state.0.is_none());
    }

    #[test]
    fn syncing_spans_record_only_after_successful_completion() {
        let recorder = Recorder::on();
        let mut timing = tracker(&recorder);
        let at = time();
        timing.status(FolderState::Connecting, at);
        timing.status(FolderState::Bootstrapping, at);
        timing.status(FolderState::CatchingUp, later(at, Duration::from_secs(10)));
        timing.status(FolderState::CatchingUp, later(at, Duration::from_secs(60)));
        assert!(recorder.take().hours.is_empty());
        timing.status(FolderState::UpToDate, later(at, Duration::from_secs(90)));
        timing.status(FolderState::UpToDate, later(at, Duration::from_secs(100)));
        timing.status(FolderState::Stopped, later(at, Duration::from_secs(110)));
        assert_duration_bucket(
            &reports(&recorder)["2"]["histograms"]["syncing_ms"]["completed"],
            7,
        );
        let off = Recorder::off();
        let mut timing = tracker(&off);
        timing.status(FolderState::CatchingUp, at);
        timing.status(FolderState::CatchingUp, later(at, Duration::from_secs(600)));
        timing.status(FolderState::UpToDate, later(at, Duration::from_secs(900)));
        assert!(timing.state.0.is_none());
        assert!(off.take().hours.is_empty());
    }

    #[test]
    fn interrupted_sync_spans_do_not_record_completed_durations() {
        for syncing in [FolderState::Bootstrapping, FolderState::CatchingUp] {
            for interrupted in [
                FolderState::Connecting,
                FolderState::Offline,
                FolderState::Halted,
                FolderState::Stopped,
            ] {
                let recorder = Recorder::on();
                let mut timing = tracker(&recorder);
                let at = time();
                timing.status(syncing, at);
                timing.status(interrupted, later(at, Duration::from_secs(90)));
                assert!(recorder.take().hours.is_empty());
                timing.status(FolderState::UpToDate, later(at, Duration::from_secs(120)));
                assert!(recorder.take().hours.is_empty());
                timing.status(
                    FolderState::Bootstrapping,
                    later(at, Duration::from_secs(130)),
                );
                timing.status(FolderState::CatchingUp, later(at, Duration::from_secs(140)));
                timing.status(FolderState::UpToDate, later(at, Duration::from_secs(150)));
                timing.status(FolderState::UpToDate, later(at, Duration::from_secs(160)));
                assert_duration_bucket(
                    &reports(&recorder)["2"]["histograms"]["syncing_ms"]["completed"],
                    5,
                );
            }
        }
    }

    #[test]
    fn complete_report_histories_do_not_reveal_an_hour_crossing() {
        for (seconds, bucket) in [(90, 7), (3600, 12)] {
            let mut histories = Vec::new();
            for linked_ms in [
                20_001 * 86_400_000 - seconds * 500,
                20_001 * 86_400_000 + 12 * 3_600_000,
            ] {
                let dir = tempfile::tempdir().unwrap();
                let store = dir.path().join("store.sqlite");
                let recorder = Recorder::on();
                Timing::initialize(&store, linked_ms, &recorder);
                let at = Time {
                    mono: Instant::now(),
                    unix_ms: linked_ms,
                };
                let mut timing = Timing::open_at(&store, linked_ms, at);
                timing.enable(recorder.clone());
                let mut history = Vec::new();
                for elapsed in (0..seconds).step_by(5) {
                    timing.status(
                        FolderState::Bootstrapping,
                        later(at, Duration::from_secs(elapsed as u64)),
                    );
                    let value = reports(&recorder);
                    assert_eq!(value, json!({}));
                    history.push(value);
                }
                let finished = later(at, Duration::from_secs(seconds as u64));
                timing.status(FolderState::UpToDate, finished);
                timing.first_sync(100_000, finished);
                let value = reports(&recorder);
                for (metric, label) in [
                    ("first_sync_ms", "65536_131071"),
                    ("syncing_ms", "completed"),
                ] {
                    let histogram =
                        &value[finished.hour().to_string()]["histograms"][metric][label];
                    assert_duration_bucket(histogram, bucket);
                }
                let sums = (value.as_object().unwrap().values())
                    .map(|report| {
                        let mut report = report.clone();
                        report.as_object_mut().unwrap().remove("hour");
                        report
                    })
                    .collect::<Vec<_>>();
                history.push(json!(sums));
                histories.push(history);
            }
            assert_eq!(histories[0], histories[1]);
        }
    }

    #[test]
    fn broken_clocks_cannot_turn_durations_into_timestamps() {
        let now = Time::now();
        for (linked_ms, opened, finished) in [
            (
                1,
                Time { unix_ms: 1, ..now },
                later(now, Duration::from_secs(1)),
            ),
            (1, now, later(now, Duration::from_secs(1))),
            (
                now.unix_ms,
                now,
                Time {
                    unix_ms: now.unix_ms - 1,
                    ..later(now, Duration::from_secs(1))
                },
            ),
            (
                now.unix_ms,
                now,
                later(now, Duration::from_millis(MAX_DURATION_MS + 1)),
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let store = dir.path().join("store.sqlite");
            let recorder = Recorder::on();
            Timing::initialize(&store, linked_ms, &recorder);
            let mut timing = Timing::open_at(&store, linked_ms, opened);
            timing.enable(recorder.clone());
            timing.first_sync(1, finished);
            assert!(recorder.take().hours.is_empty());
            assert!(!Timing::open_at(&store, linked_ms, finished).has_first());
        }
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store.sqlite");
        let recorder = Recorder::on();
        Timing::initialize(&store, now.unix_ms, &recorder);
        let jumped = Time {
            unix_ms: now.unix_ms + 15 * 86_400_000,
            ..later(now, Duration::from_secs(1))
        };
        let mut first = Timing::open_at(&store, now.unix_ms, jumped);
        first.linked(Some(now));
        first.enable(recorder.clone());
        first.first_sync(1, jumped);
        let value = reports(&recorder);
        assert_duration_bucket(
            &value[jumped.hour().to_string()]["histograms"]["first_sync_ms"]["1"],
            0,
        );
        let recorder = Recorder::on();
        let mut timing = tracker(&recorder);
        timing.records(&[record(1, 1, 1)], 0, 1, "me", now);
        let effect = effect("clock");
        timing.planned(&effect, Some(&node(1)), now);
        timing.applied(&effect.effect_id, true, later(now, Duration::from_secs(1)));
        assert!(recorder.take().hours.is_empty());
        timing.records(&[record(2, now.unix_ms - 1000, 1)], 1, 2, "me", now);
        timing.planned(&effect, Some(&node(1)), now);
        let corrected = Time {
            unix_ms: now.unix_ms + 15 * 86_400_000,
            ..later(now, Duration::from_secs(1))
        };
        timing.applied(&effect.effect_id, true, corrected);
        let value = reports(&recorder);
        let histogram =
            &value[corrected.hour().to_string()]["histograms"]["remote_apply_ms"]["effect"];
        assert_duration_bucket(histogram, 1);
    }

    #[test]
    fn bursts_include_retries_empty_files_dedup_and_subsecond_denominators() {
        let recorder = Recorder::on();
        let mut timing = tracker(&recorder);
        let at = time();
        let record = effect("write");
        let bytes = timing.planned(&record, Some(&node(1)), at);
        add_bytes(&bytes, 17);
        timing.applied(
            &record.effect_id,
            false,
            later(at, Duration::from_millis(20)),
        );
        let bytes = timing.planned(
            &record,
            Some(&node(1)),
            later(at, Duration::from_millis(50)),
        );
        add_bytes(&bytes, 17);
        timing.applied(
            &record.effect_id,
            true,
            later(at, Duration::from_millis(70)),
        );
        timing.planned(
            &record,
            Some(&node(1)),
            later(at, Duration::from_millis(80)),
        );
        timing.applied(
            &record.effect_id,
            true,
            later(at, Duration::from_millis(90)),
        );
        let mut upload = commit();
        let mut update = koidrive_proto::v1::UpdateContent {
            node_id: "deduplicated-file".into(),
            ..Default::default()
        };
        update.content = Some(Content {
            size: 4096,
            ..Default::default()
        });
        upload.ops.push(Op {
            op: Some(op::Op::UpdateContent(update)),
        });
        timing.uploading(&upload, at);
        let accepted = CommitResult {
            results: vec![
                OpResult {
                    code: op_result::Code::Applied as i32,
                    ..Default::default()
                };
                2
            ],
            ..Default::default()
        };
        timing.uploaded(&upload, &accepted);
        timing.uploaded(&upload, &accepted);
        timing.status(FolderState::UpToDate, later(at, Duration::from_millis(100)));
        timing.drained(later(at, Duration::from_millis(100)));
        let value = reports(&recorder);
        let histograms = &value["1"]["histograms"];
        assert_eq!(histograms["download_per_second"]["files"]["sum"], 10);
        assert_eq!(histograms["download_per_second"]["bytes"]["sum"], 340);
        assert_eq!(histograms["upload_per_second"]["files"]["sum"], 20);
        assert_eq!(histograms["upload_per_second"]["bytes"]["sum"], 0);
        assert_eq!(rate(u64::MAX, 1), u64::MAX);
        assert_eq!(rate(1, 1_500_000_000), 0);
    }

    #[test]
    fn unsuccessful_and_zero_duration_bursts_do_not_invent_throughput() {
        let recorder = Recorder::on();
        let mut timing = tracker(&recorder);
        let at = time();
        let record = effect("failed");
        let bytes = timing.planned(&record, Some(&node(1)), at);
        add_bytes(&bytes, 99);
        timing.applied(&record.effect_id, false, at);
        let upload = commit();
        add_bytes(&timing.uploading(&upload, at), 99);
        timing.uploaded(
            &upload,
            &CommitResult {
                results: vec![OpResult {
                    code: op_result::Code::Rejected as i32,
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
        timing.status(FolderState::UpToDate, later(at, Duration::from_secs(1)));
        timing.drained(later(at, Duration::from_secs(1)));
        assert!(recorder.take().hours.is_empty());
        timing.planned(&record, Some(&node(1)), at);
        timing.applied(&record.effect_id, true, at);
        timing.status(FolderState::UpToDate, at);
        timing.drained(at);
        assert!(recorder.take().hours.is_empty());
    }

    #[test]
    fn accepting_a_resent_commit_without_an_upload_start_leaves_no_burst_files() {
        let recorder = Recorder::on();
        let mut timing = tracker(&recorder);
        let at = time();
        let mut upload = commit();
        let accepted = CommitResult {
            results: vec![OpResult {
                code: op_result::Code::Applied as i32,
                ..Default::default()
            }],
            ..Default::default()
        };
        timing.uploaded(&upload, &accepted);
        timing.drained(at);
        assert!(recorder.take().hours.is_empty());
        upload.commit_id = "later-upload".into();
        add_bytes(&timing.uploading(&upload, at), 1000);
        timing.uploaded(&upload, &accepted);
        timing.drained(later(at, Duration::from_secs(1)));
        let value = reports(&recorder);
        assert_eq!(
            value["1"]["histograms"]["upload_per_second"]["files"]["sum"],
            1
        );
        assert_eq!(
            value["1"]["histograms"]["upload_per_second"]["bytes"]["sum"],
            1000
        );
    }

    #[test]
    fn apply_latency_is_emitted_only_on_success_with_current_known_revision() {
        let recorder = Recorder::on();
        let mut timing = tracker(&recorder);
        let at = time();
        let remote = node(1);
        let record = effect("one");
        timing.records(&[self::record(1, at.unix_ms - 1000, 1)], 0, 1, "me", at);
        timing.planned(&record, Some(&remote), at);
        assert!(recorder.take().hours.is_empty());
        timing.applied(&record.effect_id, false, later(at, Duration::from_secs(1)));
        assert!(recorder.take().hours.is_empty());
        timing.planned(&record, Some(&remote), at);
        timing.applied(&record.effect_id, true, later(at, Duration::from_secs(2)));
        let value = reports(&recorder);
        assert_duration_bucket(&value["1"]["histograms"]["remote_apply_ms"]["effect"], 2);
        timing.applied(&record.effect_id, true, later(at, Duration::from_secs(3)));
        assert!(recorder.take().hours.is_empty());
        timing.planned(&record, Some(&remote), at);
        timing.records(&[self::record(2, at.unix_ms - 500, 2)], 1, 2, "me", at);
        timing.applied(&record.effect_id, true, at);
        let value = reports(&recorder);
        assert!(!value.to_string().contains("remote_apply_ms"));
    }

    #[test]
    fn old_gap_future_missing_and_snapshot_origins_are_omitted() {
        let recorder = Recorder::on();
        let at = time();
        for (records, before, through, revision) in [
            (vec![record(1, at.unix_ms - 1, 1)], 1, 1, 1),
            (vec![record(2, at.unix_ms - 1, 1)], 0, 0, 1),
            (vec![record(1, 0, 1)], 0, 1, 1),
            (vec![record(1, at.unix_ms + 1, 1)], 0, 1, 1),
            (vec![record(1, at.unix_ms - 1, 1)], 0, 1, 2),
            (vec![], 0, 0, 1),
        ] {
            let mut timing = tracker(&recorder);
            timing.records(&records, before, through, "me", at);
            let effect = effect("omitted");
            timing.planned(&effect, Some(&node(revision)), at);
            timing.applied(&effect.effect_id, true, at);
        }
        assert!(recorder.take().hours.is_empty());
        let mut timing = tracker(&recorder);
        timing.records(
            &[
                record(1, at.unix_ms - 20, 1),
                record(2, at.unix_ms - 10, 2),
                record(1, at.unix_ms - 20, 1),
            ],
            0,
            2,
            "me",
            at,
        );
        let effect = effect("newest");
        timing.planned(&effect, Some(&node(2)), at);
        timing.applied(&effect.effect_id, true, at);
        assert_duration_bucket(
            &reports(&recorder)["1"]["histograms"]["remote_apply_ms"]["effect"],
            0,
        );
    }

    #[test]
    #[ignore]
    fn timing_tracker_release_measurement() {
        for run in 0..5 {
            for on in if run % 2 == 0 {
                [false, true]
            } else {
                [true, false]
            } {
                let recorder = if on { Recorder::on() } else { Recorder::off() };
                let mut timing = tracker(&recorder);
                let effect = effect("measured");
                let remote = node(1);
                let records = [record(1, Time::now().unix_ms - 10, 1)];
                let commit = commit();
                let accepted = CommitResult {
                    results: vec![OpResult {
                        code: op_result::Code::Applied as i32,
                        ..Default::default()
                    }],
                    ..Default::default()
                };
                let started = Instant::now();
                for _ in 0..1000 {
                    if std::hint::black_box(&timing).is_on() {
                        timing.status(FolderState::CatchingUp, Time::now());
                    }
                    if std::hint::black_box(&timing).is_on() {
                        timing.records(&records, 0, 1, "me", Time::now());
                    }
                    if std::hint::black_box(&timing).is_on() {
                        add_bytes(&timing.planned(&effect, Some(&remote), Time::now()), 1024);
                    }
                    if std::hint::black_box(&timing).is_on() {
                        add_bytes(&timing.uploading(&commit, Time::now()), 2048);
                    }
                    if std::hint::black_box(&timing).is_on() {
                        timing.applied(&effect.effect_id, true, Time::now());
                        timing.uploaded(&commit, &accepted);
                    }
                    if std::hint::black_box(&timing).is_on() {
                        timing.status(FolderState::UpToDate, Time::now());
                        timing.drained(Time::now());
                    }
                }
                let elapsed = started.elapsed();
                let value = reports(&recorder);
                if on {
                    for (metric, label) in [
                        ("download_per_second", "files"),
                        ("download_per_second", "bytes"),
                        ("upload_per_second", "files"),
                        ("upload_per_second", "bytes"),
                        ("remote_apply_ms", "effect"),
                        ("syncing_ms", "completed"),
                    ] {
                        let count: u64 = value
                            .as_object()
                            .unwrap()
                            .values()
                            .map(|day| {
                                day["histograms"][metric][label]["count"]
                                    .as_u64()
                                    .unwrap_or(0)
                            })
                            .sum();
                        assert_eq!(count, 1000, "{metric}.{label}");
                    }
                } else {
                    assert_eq!(value, json!({}));
                }
                eprintln!(
                    "timing tracker run={run} on={on} bursts=1000 elapsed_ns={}",
                    elapsed.as_nanos()
                );
            }
        }
    }

    #[test]
    fn blocked_retries_keep_origins_and_bursts_until_the_direction_drains() {
        let recorder = Recorder::on();
        let mut timing = tracker(&recorder);
        let at = time();
        timing.records(&[record(1, at.unix_ms - 1000, 1)], 0, 1, "me", at);
        let mut parked = effect("park");
        parked.effect = Effect::Move {
            from: Location::Parked("before".into()),
            identity: koidrive_engine::tree::FileIdentity {
                dev: 1,
                ino: 1,
                birth_ns: None,
            },
            to: Location::Parked("secret-node-id".into()),
        };
        timing.planned(&parked, Some(&node(1)), at);
        timing.applied(
            &parked.effect_id,
            true,
            later(at, Duration::from_millis(100)),
        );
        assert_duration_bucket(
            &reports(&recorder)["1"]["histograms"]["remote_apply_ms"]["effect"],
            1,
        );
        let write = effect("write");
        add_bytes(&timing.planned(&write, Some(&node(1)), at), 13);
        timing.applied(
            &write.effect_id,
            false,
            later(at, Duration::from_millis(200)),
        );
        timing.status(FolderState::UpToDate, later(at, Duration::from_millis(400)));
        assert!(recorder.take().hours.is_empty());
        add_bytes(
            &timing.planned(
                &write,
                Some(&node(1)),
                later(at, Duration::from_millis(600)),
            ),
            13,
        );
        timing.applied(&write.effect_id, true, later(at, Duration::from_secs(1)));
        timing.drained(later(at, Duration::from_secs(1)));
        let value = reports(&recorder);
        assert_duration_bucket(&value["1"]["histograms"]["remote_apply_ms"]["effect"], 1);
        assert_eq!(
            value["1"]["histograms"]["download_per_second"]["files"]["sum"],
            1
        );
        assert_eq!(
            value["1"]["histograms"]["download_per_second"]["bytes"]["sum"],
            26
        );
    }
}
