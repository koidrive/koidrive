// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use rustix::fs::{FlockOperation, flock};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::consent::{self, Status};
use crate::recorder::Pending;
use crate::schema::{Event, EventUuid, HourlyReport, InstallId, Origin};

pub const KEEP_HOURS: u32 = 14 * 24;
pub const MAX_EVENTS: usize = 500;
const QUEUE: &str = "queue-v2.json";
const OLD_QUEUE: &str = "queue.json";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// The person's choice; none until asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_id: Option<InstallId>,
}

/// Reports and events taken from the queue to send.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Batch {
    #[serde(default)]
    pub hours: Vec<HourlyReport>,
    #[serde(default, deserialize_with = "known_events")]
    pub events: Vec<Queued>,
}

/// An event in the queue, with the release and process that recorded it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Queued {
    #[serde(flatten)]
    pub event: Event,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<Origin>,
}

impl Batch {
    pub fn is_empty(&self) -> bool {
        self.hours.is_empty() && self.events.is_empty()
    }
}

/// A batch [`Store::take`] handed out. It stays in the queue until [`Store::sent`] removes it, and holds `send.lock`
/// meanwhile, so one send runs at a time. Given back, or dropped as when its send is cancelled or its process ends,
/// the next take hands out the same batch with the same uuids, so PostHog keeps one copy if an earlier try arrived.
#[derive(Debug)]
pub struct Sending {
    pub id: InstallId,
    pub batch: Batch,
    pub uuids: Vec<EventUuid>,
    _lock: File,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Queue {
    #[serde(default)]
    hours: BTreeMap<u32, HourlyReport>,
    #[serde(default, deserialize_with = "known_events")]
    events: Vec<Queued>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    out: Option<Out>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    admitted: BTreeMap<u32, BTreeMap<String, usize>>,
}

fn known_events<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<Queued>, D::Error> {
    let values = Vec::<Value>::deserialize(deserializer)?;
    Ok((values.into_iter())
        .filter_map(|value| serde_json::from_value(value).ok())
        .collect())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(from = "StoredOut")]
struct Out {
    id: InstallId,
    batch: Batch,
    uuids: Vec<EventUuid>,
}

#[derive(Deserialize)]
struct StoredOut {
    id: InstallId,
    batch: StoredBatch,
    uuids: Vec<EventUuid>,
}

#[derive(Deserialize)]
struct StoredBatch {
    #[serde(default)]
    hours: Vec<HourlyReport>,
    #[serde(default)]
    events: Vec<Value>,
}

impl From<StoredOut> for Out {
    fn from(stored: StoredOut) -> Self {
        let hours = stored.batch.hours.len();
        let mut uuids = stored.uuids;
        let event_uuids = match uuids.len() == hours + stored.batch.events.len() {
            true => uuids.split_off(hours),
            false => Vec::new(),
        };
        let mut out = Out {
            id: stored.id,
            batch: Batch {
                hours: stored.batch.hours,
                events: Vec::new(),
            },
            uuids,
        };
        for (value, uuid) in stored.batch.events.into_iter().zip(event_uuids) {
            if let Ok(event) = serde_json::from_value(value) {
                out.batch.events.push(event);
                out.uuids.push(uuid);
            }
        }
        out
    }
}

impl Out {
    fn keep_events(&mut self, mut keep: impl FnMut(&Queued) -> bool) {
        let uuids = self.uuids.split_off(self.batch.hours.len());
        let events = std::mem::take(&mut self.batch.events);
        for (event, uuid) in events.into_iter().zip(uuids) {
            if keep(&event) {
                self.batch.events.push(event);
                self.uuids.push(uuid);
            }
        }
    }

    fn new(id: InstallId, batch: Batch) -> io::Result<Self> {
        let uuids = (0..batch.hours.len() + batch.events.len())
            .map(|_| EventUuid::generate().map_err(io::Error::other))
            .collect::<io::Result<_>>()?;
        Ok(Out { id, batch, uuids })
    }

    fn trimmed(self, hour: u32) -> Option<Self> {
        if self.uuids.len() != self.batch.hours.len() + self.batch.events.len() {
            return None;
        }
        let oldest = hour.saturating_sub(KEEP_HOURS);
        let mut uuids = self.uuids.into_iter();
        let mut out = Out {
            id: self.id,
            batch: Batch::default(),
            uuids: Vec::new(),
        };
        for (report, uuid) in self.batch.hours.into_iter().zip(uuids.by_ref()) {
            if report.hour >= oldest {
                out.batch.hours.push(report);
                out.uuids.push(uuid);
            }
        }
        out.uuids.extend(uuids);
        out.batch.events = self.batch.events;
        out.keep_events(|event| event.event.hour() >= oldest);
        (!out.batch.is_empty()).then_some(out)
    }
}

impl Queue {
    fn put(&mut self, hours: impl IntoIterator<Item = HourlyReport>, events: Vec<Queued>) {
        self.put_hours(hours);
        for event in events {
            let (kind, group, label) = event.event.kind();
            let admitted = (self.admitted.entry(event.event.day()).or_default())
                .entry(format!("{kind}.{group}.{label}"))
                .or_default();
            if *admitted < Event::PER_KIND_PER_DAY {
                *admitted += 1;
                self.events.push(event);
            }
        }
    }

    fn put_hours(&mut self, hours: impl IntoIterator<Item = HourlyReport>) {
        for report in hours {
            let hour = report.hour;
            self.hours
                .entry(hour)
                .or_insert_with(|| HourlyReport {
                    hour,
                    ..HourlyReport::default()
                })
                .merge(report);
        }
    }

    fn out_events(&self) -> impl Iterator<Item = &Queued> {
        self.out.iter().flat_map(|out| &out.batch.events)
    }

    fn trim(&mut self, hour: u32) {
        self.out = self.out.take().and_then(|out| out.trimmed(hour));
        let oldest = hour.saturating_sub(KEEP_HOURS);
        self.hours
            .retain(|hour, report| *hour >= oldest && !report.is_empty());
        self.events.retain(|event| event.event.hour() >= oldest);
        self.admitted = self.admitted.split_off(&(oldest / 24));
        let mut over = (self.events.len() + self.out_events().count()).saturating_sub(MAX_EVENTS);
        let mut keep = |queued: &Queued| {
            let evicted =
                over > 0 && !matches!(queued.event, Event::Panic { .. } | Event::AppCrash { .. });
            over -= usize::from(evicted);
            !evicted
        };
        if let Some(out) = &mut self.out {
            out.keep_events(&mut keep);
        }
        self.out = self.out.take().filter(|out| !out.batch.is_empty());
        self.events.retain(|queued| keep(queued));
        self.events.drain(..over.min(self.events.len()));
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct OldQueue {
    #[serde(default)]
    days: BTreeMap<u32, OldReport>,
    #[serde(default)]
    events: Vec<OldEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    out: Option<OldOut>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OldOut {
    id: InstallId,
    batch: OldBatch,
    uuids: Vec<EventUuid>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OldBatch {
    #[serde(default)]
    pub days: Vec<OldReport>,
    #[serde(default)]
    pub events: Vec<OldEvent>,
}

impl OldBatch {
    pub fn is_empty(&self) -> bool {
        self.days.is_empty() && self.events.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OldReport {
    pub day: u32,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub counters: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub levels: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub histograms: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OldEvent {
    #[serde(rename = "type")]
    pub name: String,
    pub day: u32,
    #[serde(flatten)]
    pub fields: Map<String, Value>,
}

/// A batch an earlier release queued, as [`Store::take_old`] hands it out.
#[derive(Debug)]
pub struct SendingOld {
    pub id: InstallId,
    pub batch: OldBatch,
    pub uuids: Vec<EventUuid>,
    _lock: File,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Store {
    dir: PathBuf,
    origin: Option<Origin>,
}

impl Store {
    /// The telemetry files of the profile whose state directory is `state_dir`.
    pub fn new(state_dir: &Path) -> Self {
        Store {
            dir: state_dir.join("telemetry"),
            origin: None,
        }
    }

    /// The same files, with events this store queues marked as recorded by `origin`.
    pub fn recorded_by(self, origin: Origin) -> Self {
        Store {
            origin: Some(origin),
            ..self
        }
    }

    /// A missing file is the default: never asked, no id.
    pub fn settings(&self) -> io::Result<Settings> {
        read(&self.dir.join("settings.json"))
    }

    pub fn prompt_pending(&self) -> io::Result<bool> {
        read(&self.dir.join("prompt.json"))
    }

    pub fn begin_prompt(&self) -> io::Result<bool> {
        let _lock = self.lock(true)?;
        if self.settings()?.enabled.is_some() {
            return Ok(false);
        }
        write(&self.dir.join("prompt.json"), &true)?;
        Ok(true)
    }

    /// The mode with this profile's stored choice and the process's environment.
    pub fn status(&self, var: impl Fn(&str) -> Option<std::ffi::OsString>) -> io::Result<Status> {
        Ok(consent::resolve(var, self.settings()?.enabled))
    }

    /// Stores the person's choice. Turning it on keeps the install id or draws one. Turning it off deletes the id and
    /// everything waiting to be sent. A `settings.json` that cannot be parsed is replaced. Whenever no id is kept, the
    /// queue goes too.
    pub fn set(&self, on: bool) -> io::Result<Settings> {
        let _lock = self.lock(true)?;
        let mut settings = match self.settings() {
            Err(error) if error.kind() == io::ErrorKind::InvalidData => Settings::default(),
            settings => settings?,
        };
        settings.enabled = Some(on);
        settings.install_id = settings.install_id.filter(|_| on);
        if settings.install_id.is_none() {
            self.remove_queues()?;
        }
        if on {
            settings.install_id.get_or_insert_with(InstallId::generate);
        }
        write(&self.dir.join("settings.json"), &settings)?;
        remove(&self.dir.join("prompt.json"))?;
        Ok(settings)
    }

    /// Deletes everything waiting to be sent and then draws a new install id.
    /// Without an id there is nothing to reset.
    pub fn reset(&self) -> io::Result<Option<InstallId>> {
        let _lock = self.lock(true)?;
        self.remove_queues()?;
        let mut settings = self.settings()?;
        if settings.install_id.is_none() {
            return Ok(None);
        }
        let id = InstallId::generate();
        settings.install_id = Some(id.clone());
        write(&self.dir.join("settings.json"), &settings)?;
        Ok(Some(id))
    }

    /// The install id, drawn and stored when telemetry is on through `KOIDRIVE_TELEMETRY` and none exists yet.
    pub fn install_id(&self) -> io::Result<InstallId> {
        let _lock = self.lock(true)?;
        let mut settings = self.settings()?;
        if let Some(id) = &settings.install_id {
            return Ok(id.clone());
        }
        self.remove_queues()?;
        let id = InstallId::generate();
        settings.install_id = Some(id.clone());
        write(&self.dir.join("settings.json"), &settings)?;
        Ok(id)
    }

    pub fn absorb(&self, pending: Pending) -> io::Result<()> {
        self.absorb_at(pending, crate::this_hour(), true)
    }

    pub fn try_absorb(&self, pending: Pending) -> io::Result<()> {
        self.absorb_at(pending, crate::this_hour(), false)
    }

    /// Like [`Store::absorb`], but only while the profile's install id is still `id`.
    pub fn absorb_as(&self, pending: Pending, id: &InstallId) -> io::Result<()> {
        self.absorb_for(pending, crate::this_hour(), true, Some(id))
    }

    pub(crate) fn absorb_at(&self, pending: Pending, hour: u32, wait: bool) -> io::Result<()> {
        self.absorb_for(pending, hour, wait, None)
    }

    fn absorb_for(
        &self,
        pending: Pending,
        hour: u32,
        wait: bool,
        expected: Option<&InstallId>,
    ) -> io::Result<()> {
        if pending.hours.values().all(HourlyReport::is_empty) && pending.events.is_empty() {
            return Ok(());
        }
        let _lock = self.lock(wait)?;
        match (self.settings()?.install_id, expected) {
            (None, _) => return Ok(()),
            (Some(id), Some(expected)) if &id != expected => return Ok(()),
            _ => {}
        }
        let mut queue = self.queue()?;
        let events = (pending.events.into_iter())
            .map(|event| Queued {
                event,
                origin: self.origin.clone(),
            })
            .collect();
        queue.put(pending.hours.into_values(), events);
        queue.trim(hour);
        write(&self.dir.join(QUEUE), &queue)
    }

    pub fn take(&self, hour: u32) -> io::Result<Option<Sending>> {
        let sending = match self.send_lock() {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
            Err(error) => return Err(error),
        };
        let _lock = self.lock(true)?;
        let Some(id) = self.settings()?.install_id else {
            return Ok(None);
        };
        let mut queue = self.queue()?;
        let held = (queue.hours.len(), queue.events.len(), queue.out.is_some());
        queue.trim(hour);
        let trimmed = held != (queue.hours.len(), queue.events.len(), queue.out.is_some());
        let left = queue.out.take().filter(|out| out.id == id);
        let out = match left {
            Some(out) => out,
            None => {
                let later = queue.hours.split_off(&hour);
                let batch = Batch {
                    hours: std::mem::replace(&mut queue.hours, later)
                        .into_values()
                        .collect(),
                    events: std::mem::take(&mut queue.events),
                };
                Out::new(id.clone(), batch)?
            }
        };
        if !out.batch.is_empty() {
            queue.out = Some(out.clone());
        }
        if trimmed || held.2 || queue.out.is_some() {
            write(&self.dir.join(QUEUE), &queue)?;
        }
        Ok(Some(Sending {
            id,
            batch: out.batch,
            uuids: out.uuids,
            _lock: sending,
        }))
    }

    pub fn sent(&self, sending: Sending) -> io::Result<()> {
        self.settle(sending, true)
    }

    pub fn give_back(&self, sending: Sending) -> io::Result<()> {
        self.settle(sending, false)
    }

    fn settle(&self, sending: Sending, sent: bool) -> io::Result<()> {
        let _lock = self.lock(true)?;
        let id = self.settings()?.install_id;
        let mut queue = self.queue()?;
        let ours = (queue.out.as_ref()).is_some_and(|out| {
            out.id == sending.id && out.uuids.iter().all(|uuid| sending.uuids.contains(uuid))
        });
        if !ours || (!sent && id.as_ref() == Some(&sending.id)) {
            return Ok(());
        }
        queue.out = None;
        write(&self.dir.join(QUEUE), &queue)
    }

    /// Everything in the queue, this hour and a batch that is out included, without taking it.
    pub fn peek(&self) -> io::Result<Batch> {
        let mut queue = self.queue()?;
        let mut events = Vec::new();
        if let Some(out) = queue.out.take() {
            queue.put_hours(out.batch.hours);
            events = out.batch.events;
        }
        events.extend(queue.events);
        Ok(Batch {
            hours: queue.hours.into_values().collect(),
            events,
        })
    }

    /// Hands out what an earlier release left in `queue.json`: first the batch it had out, with the uuids it drew, then
    /// everything else it queued. The file keeps the batch, in that release's layout, until [`Store::sent_old`].
    pub fn take_old(&self) -> io::Result<Option<SendingOld>> {
        let sending = match self.send_lock() {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
            Err(error) => return Err(error),
        };
        let _lock = self.lock(true)?;
        let Some(id) = self.settings()?.install_id else {
            return Ok(None);
        };
        let path = self.dir.join(OLD_QUEUE);
        let mut queue: OldQueue = match read(&path) {
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                tracing::debug!(%error, "dropping an earlier release's telemetry queue that cannot be parsed");
                OldQueue::default()
            }
            read => read?,
        };
        let left = (queue.out.take()).filter(|out| {
            out.id == id && out.uuids.len() == out.batch.days.len() + out.batch.events.len()
        });
        let out = match left {
            Some(out) => out,
            None => {
                let batch = OldBatch {
                    days: std::mem::take(&mut queue.days).into_values().collect(),
                    events: std::mem::take(&mut queue.events),
                };
                let uuids = (0..batch.days.len() + batch.events.len())
                    .map(|_| EventUuid::generate().map_err(io::Error::other))
                    .collect::<io::Result<_>>()?;
                OldOut {
                    id: id.clone(),
                    batch,
                    uuids,
                }
            }
        };
        if out.batch.is_empty() {
            remove(&path)?;
            return Ok(None);
        }
        queue.out = Some(out.clone());
        write(&path, &queue)?;
        Ok(Some(SendingOld {
            id,
            batch: out.batch,
            uuids: out.uuids,
            _lock: sending,
        }))
    }

    /// Removes a batch [`Store::take_old`] handed out, and deletes `queue.json` once nothing else is left in it.
    pub fn sent_old(&self, sending: SendingOld) -> io::Result<()> {
        let _lock = self.lock(true)?;
        let path = self.dir.join(OLD_QUEUE);
        let mut queue: OldQueue = read(&path)?;
        if (queue.out.as_ref()).is_some_and(|out| out.uuids == sending.uuids) {
            queue.out = None;
        }
        if queue.days.is_empty() && queue.events.is_empty() && queue.out.is_none() {
            remove(&path)
        } else {
            write(&path, &queue)
        }
    }

    /// What an earlier release left in `queue.json`, its batch out included, without taking it.
    pub fn peek_old(&self) -> io::Result<OldBatch> {
        let queue: OldQueue = match read(&self.dir.join(OLD_QUEUE)) {
            Err(error) if error.kind() == io::ErrorKind::InvalidData => OldQueue::default(),
            read => read?,
        };
        let mut batch = queue.out.map(|out| out.batch).unwrap_or_default();
        batch.days.extend(queue.days.into_values());
        batch.events.extend(queue.events);
        Ok(batch)
    }

    fn remove_queues(&self) -> io::Result<()> {
        remove(&self.dir.join(QUEUE))?;
        remove(&self.dir.join(OLD_QUEUE))
    }

    fn queue(&self) -> io::Result<Queue> {
        match read(&self.dir.join(QUEUE)) {
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                tracing::debug!(%error, "dropping a telemetry queue that cannot be parsed");
                Ok(Queue::default())
            }
            read => read,
        }
    }

    fn lock(&self, wait: bool) -> io::Result<File> {
        self.flock("lock", wait)
    }

    fn send_lock(&self) -> io::Result<File> {
        self.flock("send.lock", false)
    }

    fn flock(&self, name: &str, wait: bool) -> io::Result<File> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join(name))?;
        let operation = if wait {
            FlockOperation::LockExclusive
        } else {
            FlockOperation::NonBlockingLockExclusive
        };
        flock(&file, operation)?;
        Ok(file)
    }
}

fn read<T: Default + for<'de> Deserialize<'de>>(path: &Path) -> io::Result<T> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error),
    }
}

fn write<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec(value)?)?;
    fs::rename(&temporary, path)
}

fn remove(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Recorder;
    use crate::schema::{Command, Counter, Failure, SourceFile};

    #[test]
    fn an_earlier_release_queue_stays_when_it_cannot_be_read_after_its_batch_went() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, store) = store();
        store.set(true).unwrap();
        let old = store.dir.join(OLD_QUEUE);
        fs::write(&old, br#"{"days":{"1":{"day":1}},"events":[{"type":"panic","day":2,"file":"cli/src/main.rs","line":1}]}"#).unwrap();
        let sending = store.take_old().unwrap().unwrap();
        fs::set_permissions(&old, fs::Permissions::from_mode(0o000)).unwrap();
        assert!(store.sent_old(sending).is_err());
        fs::set_permissions(&old, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(store.peek_old().unwrap().events.len(), 1);
    }

    #[test]
    fn an_earlier_release_queue_goes_when_reports_turn_off_and_is_dropped_when_unreadable() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        let old = store.dir.join(OLD_QUEUE);
        fs::write(
            &old,
            br#"{"days":{"1":{"day":1,"counters":{"command":{"status":1}}}}}"#,
        )
        .unwrap();
        store.set(false).unwrap();
        assert!(!old.exists());
        store.set(true).unwrap();
        fs::write(&old, b"{").unwrap();
        assert!(store.take_old().unwrap().is_none());
        assert!(!old.exists());
    }

    #[test]
    fn unanswered_prompts_persist_without_consent_or_an_identity() {
        for on in [false, true] {
            let (dir, store) = store();
            assert!(!store.prompt_pending().unwrap());
            assert!(store.begin_prompt().unwrap());
            assert!(store.prompt_pending().unwrap());
            assert_eq!(store.settings().unwrap(), Settings::default());
            assert!(matches!(
                store.status(|_| None).unwrap().mode,
                crate::Mode::Off
            ));
            assert!(!dir.path().join("telemetry/settings.json").exists());
            assert!(!dir.path().join("telemetry/queue-v2.json").exists());
            store.set(on).unwrap();
            assert!(!store.prompt_pending().unwrap());
            assert!(!store.begin_prompt().unwrap());
            assert_eq!(store.settings().unwrap().enabled, Some(on));
        }
    }

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        (dir, store)
    }

    fn panic_on(hour: u32) -> Event {
        Event::Panic {
            at: u64::from(hour) * 3600,
            file: SourceFile::from_location("cli/src/main.rs"),
            line: 7,
            frames: Vec::new(),
        }
    }

    fn pending(hour: u32, status: u64) -> Pending {
        let recorder = Recorder::on();
        recorder.count_on(hour, Counter::Command(Command::Status), status);
        recorder.event(panic_on(hour));
        recorder.take()
    }

    #[test]
    fn a_new_profile_was_never_asked() {
        let (_dir, store) = store();
        assert_eq!(store.settings().unwrap(), Settings::default());
    }

    #[test]
    fn turning_it_on_draws_an_id_once_and_off_deletes_the_id_and_the_queue() {
        let (_dir, store) = store();
        let on = store.set(true).unwrap();
        let id = on.install_id.clone().unwrap();
        assert_eq!(store.set(true).unwrap().install_id, Some(id.clone()));
        store.absorb_at(pending(100, 1), 100, true).unwrap();
        assert!(!store.peek().unwrap().is_empty());

        let off = store.set(false).unwrap();
        assert_eq!(
            off,
            Settings {
                enabled: Some(false),
                install_id: None
            }
        );
        assert!(store.peek().unwrap().is_empty());
        assert_ne!(store.set(true).unwrap().install_id, Some(id));
    }

    #[test]
    fn a_broken_settings_file_does_not_stop_a_choice() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        store.absorb_at(pending(100, 1), 100, true).unwrap();
        fs::write(store.dir.join("settings.json"), b"{").unwrap();
        assert!(store.settings().is_err());
        assert_eq!(store.set(false).unwrap().enabled, Some(false));
        assert!(store.peek().unwrap().is_empty());
        assert_eq!(store.settings().unwrap().install_id, None);
    }

    #[test]
    fn turning_it_on_over_a_broken_settings_file_drops_the_old_queue() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        store.absorb_at(pending(99, 1), 100, true).unwrap();
        drop(store.take(100).unwrap());
        store.absorb_at(pending(100, 1), 100, true).unwrap();
        fs::write(store.dir.join("settings.json"), b"{").unwrap();
        assert!(store.set(true).unwrap().install_id.is_some());
        assert!(store.peek().unwrap().is_empty());
        assert!(store.take(101).unwrap().unwrap().batch.is_empty());
    }

    #[test]
    fn a_new_id_over_a_deleted_settings_file_drops_the_old_queue() {
        for draw in [
            |store: &Store| drop(store.set(true).unwrap()),
            |store: &Store| drop(store.install_id().unwrap()),
        ] {
            let (_dir, store) = store();
            store.set(true).unwrap();
            store.absorb_at(pending(99, 1), 100, true).unwrap();
            drop(store.take(100).unwrap());
            store.absorb_at(pending(100, 1), 100, true).unwrap();
            fs::remove_file(store.dir.join("settings.json")).unwrap();
            draw(&store);
            assert!(store.peek().unwrap().is_empty());
        }
    }

    #[test]
    fn reset_draws_a_new_id_and_empties_the_queue() {
        let (_dir, store) = store();
        assert_eq!(store.reset().unwrap(), None);
        let id = store.set(true).unwrap().install_id.unwrap();
        store.absorb_at(pending(100, 1), 100, true).unwrap();
        let new = store.reset().unwrap().unwrap();
        assert_ne!(new, id);
        assert_eq!(store.settings().unwrap().install_id, Some(new));
        assert!(store.peek().unwrap().is_empty());
    }

    #[test]
    fn a_new_id_is_drawn_only_once_the_old_queue_is_gone() {
        for (on, draw) in [
            (
                true,
                Store::reset as fn(&Store) -> io::Result<Option<InstallId>>,
            ),
            (false, |store: &Store| store.set(true).map(|s| s.install_id)),
            (false, |store: &Store| store.install_id().map(Some)),
        ] {
            let (_dir, store) = store();
            store.set(on).unwrap();
            let before = store.settings().unwrap().install_id;
            fs::create_dir(store.dir.join(QUEUE)).unwrap();
            assert!(draw(&store).is_err());
            assert_eq!(store.settings().unwrap().install_id, before);
        }
    }

    #[test]
    fn sums_from_several_processes_add_up() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        store.absorb_at(pending(100, 1), 100, true).unwrap();
        store.absorb_at(pending(100, 2), 100, true).unwrap();
        let queued = store.peek().unwrap();
        assert_eq!(queued.hours.len(), 1);
        assert_eq!(queued.hours[0].counters["command"]["status"], 3);
        assert_eq!(queued.events.len(), 2);
    }

    fn hours(queue: &Queue) -> Vec<u32> {
        queue.hours.keys().copied().collect()
    }

    #[test]
    fn take_leaves_this_hour_and_give_back_restores_the_rest() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        store.absorb_at(pending(99, 1), 100, true).unwrap();
        store.absorb_at(pending(100, 5), 100, true).unwrap();
        let sending = store.take(100).unwrap().unwrap();
        assert_eq!(
            (sending.batch.hours.iter())
                .map(|report| report.hour)
                .collect::<Vec<_>>(),
            [99]
        );
        assert_eq!(sending.batch.events.len(), 2);
        let left = store.queue().unwrap();
        assert_eq!(hours(&left), [100]);
        assert!(left.events.is_empty());
        assert_eq!(left.out.unwrap().batch, sending.batch);
        assert_eq!(store.peek().unwrap().hours.len(), 2);

        let (batch, uuids) = (sending.batch.clone(), sending.uuids.clone());
        assert_eq!(uuids.len(), 3);
        store.give_back(sending).unwrap();
        store.absorb_at(pending(99, 4), 100, true).unwrap();
        let again = store.take(101).unwrap().unwrap();
        assert_eq!((again.batch, again.uuids), (batch, uuids));
    }

    #[test]
    fn take_drops_what_expired() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        store.absorb_at(pending(99, 1), 100, true).unwrap();
        assert!(
            store
                .take(99 + KEEP_HOURS + 1)
                .unwrap()
                .unwrap()
                .batch
                .is_empty()
        );
        store.absorb_at(pending(99, 1), 100, true).unwrap();
        let sending = store.take(100).unwrap().unwrap();
        store.give_back(sending).unwrap();
        assert!(
            store
                .take(99 + KEEP_HOURS + 1)
                .unwrap()
                .unwrap()
                .batch
                .is_empty()
        );
        assert!(store.peek().unwrap().is_empty());
    }

    #[test]
    fn a_take_with_nothing_due_still_drops_what_expired() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        store.absorb_at(pending(99, 1), 100, true).unwrap();
        let late = 99 + KEEP_HOURS + 1;
        drop(store.take(late).unwrap());
        assert!(store.queue().unwrap().hours.is_empty());
        assert!(store.queue().unwrap().events.is_empty());
    }

    #[test]
    fn an_expired_batch_out_makes_no_room_for_itself() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        store.absorb_at(pending(99, 1), 100, true).unwrap();
        let sending = store.take(100).unwrap().unwrap();
        store.give_back(sending).unwrap();
        let late = 99 + KEEP_HOURS + 1;
        store.absorb_at(pending(late, 1), late, true).unwrap();
        let queue = store.queue().unwrap();
        assert!(queue.out.is_none());
        assert_eq!(queue.events.len(), 1);
    }

    #[test]
    fn events_out_count_toward_the_cap() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        let panics = |n| Pending {
            hours: BTreeMap::new(),
            events: vec![panic_on(99); n],
        };
        store
            .absorb_at(panics(Event::PER_KIND_PER_DAY), 100, true)
            .unwrap();
        let sending = store.take(100).unwrap().unwrap();
        store.give_back(sending).unwrap();
        store.absorb_at(panics(5), 100, true).unwrap();
        assert!(store.queue().unwrap().events.is_empty());
    }

    #[test]
    fn the_daily_cap_holds_across_sends() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        let problem = Failure::all().next().unwrap();
        let mut sent = 0;
        for minute in 0..100 {
            let pending = Pending {
                hours: BTreeMap::new(),
                events: vec![Event::Problem {
                    at: 100 * 3600 + minute,
                    problem,
                    count: 1,
                }],
            };
            store.absorb_at(pending, 100, true).unwrap();
            let sending = store.take(101).unwrap().unwrap();
            sent += sending.batch.events.len();
            store.sent(sending).unwrap();
        }
        assert_eq!(sent, Event::PER_KIND_PER_DAY);
    }

    #[test]
    fn a_full_queue_drops_other_events_before_a_panic() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        let now = 1_000;
        let oldest = now - 13 * 24;
        let panic = Pending {
            hours: BTreeMap::new(),
            events: vec![panic_on(oldest)],
        };
        store.absorb_at(panic, now, true).unwrap();
        for day in 0..13 {
            let events = (Failure::all().take(3))
                .flat_map(|problem| {
                    (0..Event::PER_KIND_PER_DAY).map(move |n| Event::Problem {
                        at: u64::from(oldest + day * 24) * 3600 + n as u64,
                        problem,
                        count: 1,
                    })
                })
                .collect();
            let pending = Pending {
                hours: BTreeMap::new(),
                events,
            };
            store.absorb_at(pending, now, true).unwrap();
        }
        let queued = store.peek().unwrap();
        assert_eq!(queued.events.len(), MAX_EVENTS);
        assert!((queued.events.iter()).any(|queued| matches!(queued.event, Event::Panic { .. })));
    }

    fn panics(at_hour: u32, n: usize) -> Pending {
        Pending {
            hours: BTreeMap::new(),
            events: vec![panic_on(at_hour); n],
        }
    }

    #[test]
    fn a_batch_given_back_still_shows_in_full() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        store
            .absorb_at(panics(99, Event::PER_KIND_PER_DAY), 100, true)
            .unwrap();
        drop(store.take(100).unwrap().unwrap());
        assert_eq!(store.peek().unwrap().events.len(), Event::PER_KIND_PER_DAY);
    }

    #[test]
    fn an_unknown_event_in_a_batch_out_leaves_the_rest_with_their_uuids() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        let mut pending = panics(99, 2);
        if let Event::Panic { line, .. } = &mut pending.events[1] {
            *line = 8;
        }
        store.absorb_at(pending, 100, true).unwrap();
        let sending = store.take(100).unwrap().unwrap();
        let uuids = sending.uuids.clone();
        drop(sending);
        let path = store.dir.join(QUEUE);
        let mut file: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        file["out"]["batch"]["events"][0] =
            serde_json::json!({ "type": "from_a_later_release", "at": 1 });
        fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
        let again = store.take(100).unwrap().unwrap();
        assert_eq!(again.uuids, uuids[1..]);
        assert!(matches!(
            again.batch.events[..],
            [Queued {
                event: Event::Panic { line: 8, .. },
                ..
            }]
        ));
    }

    #[test]
    fn a_full_batch_out_makes_room_for_a_new_panic() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        let now = 1_000;
        for day in 0..13 {
            let events = (Failure::all().take(3))
                .flat_map(|problem| {
                    (0..Event::PER_KIND_PER_DAY).map(move |n| Event::Problem {
                        at: u64::from(now - 13 * 24 + day * 24) * 3600 + n as u64,
                        problem,
                        count: 1,
                    })
                })
                .collect();
            let pending = Pending {
                hours: BTreeMap::new(),
                events,
            };
            store.absorb_at(pending, now, true).unwrap();
        }
        drop(store.take(now).unwrap().unwrap());
        store.absorb_at(panics(now, 1), now, true).unwrap();
        let queued = store.peek().unwrap();
        assert_eq!(queued.events.len(), MAX_EVENTS);
        assert!((queued.events.iter()).any(|queued| matches!(queued.event, Event::Panic { .. })));
        let sending = store.take(now).unwrap().unwrap();
        assert_eq!(
            sending.uuids.len(),
            sending.batch.hours.len() + sending.batch.events.len()
        );
    }

    #[test]
    fn an_event_this_build_does_not_know_drops_only_itself() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        fs::write(
            store.dir.join(QUEUE),
            br#"{"events":[{"type":"panic","at":3600,"file":"cli/src/main.rs","line":7},{"type":"from_a_later_release","at":3600}],"later_field":1}"#,
        )
        .unwrap();
        let queued = store.peek().unwrap();
        assert_eq!(queued.events.len(), 1);
        assert!(matches!(
            queued.events[0].event,
            Event::Panic { line: 7, .. }
        ));
    }

    #[test]
    fn a_batch_partly_expired_while_out_still_settles() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        store.absorb_at(pending(98, 1), 100, true).unwrap();
        store.absorb_at(pending(99, 1), 100, true).unwrap();
        let sending = store.take(100).unwrap().unwrap();
        assert_eq!(sending.batch.hours.len(), 2);
        store
            .absorb_at(pending(100, 1), 98 + KEEP_HOURS + 1, true)
            .unwrap();
        assert_eq!(store.queue().unwrap().out.unwrap().batch.hours.len(), 1);
        store.sent(sending).unwrap();
        assert!(store.queue().unwrap().out.is_none());
    }

    #[test]
    fn a_sending_of_another_profile_settles_nothing() {
        let (_dir, store) = store();
        let (_other_dir, other) = self::store();
        for store in [&store, &other] {
            store.set(true).unwrap();
            store.absorb_at(pending(99, 1), 100, true).unwrap();
        }
        let theirs = other.take(100).unwrap().unwrap();
        let _ours = store.take(100).unwrap().unwrap();
        store.sent(theirs).unwrap();
        assert!(store.queue().unwrap().out.is_some());
    }

    #[test]
    fn a_sent_batch_leaves_the_queue() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        store.absorb_at(pending(99, 1), 100, true).unwrap();
        let sending = store.take(100).unwrap().unwrap();
        store.sent(sending).unwrap();
        assert!(store.peek().unwrap().is_empty());
    }

    #[test]
    fn one_send_at_a_time_and_a_dropped_one_loses_nothing() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        store.absorb_at(pending(99, 1), 100, true).unwrap();
        let sending = store.take(100).unwrap().unwrap();
        assert!(store.take(100).unwrap().is_none());
        let (batch, uuids) = (sending.batch.clone(), sending.uuids.clone());
        drop(sending);

        store.absorb_at(pending(99, 2), 100, true).unwrap();
        let again = store.take(100).unwrap().unwrap();
        assert_eq!((&again.batch, &again.uuids), (&batch, &uuids));
        store.sent(again).unwrap();
        let next = store.take(100).unwrap().unwrap();
        assert_eq!(next.batch.hours[0].counters["command"]["status"], 2);
        assert_ne!(next.uuids, uuids);
    }

    #[test]
    fn a_batch_out_while_telemetry_is_reset_or_turned_off_is_not_given_back() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        store.absorb_at(pending(99, 1), 100, true).unwrap();
        let sending = store.take(100).unwrap().unwrap();
        store.reset().unwrap();
        store.give_back(sending).unwrap();
        assert!(store.peek().unwrap().is_empty());

        store.absorb_at(pending(99, 1), 100, true).unwrap();
        let sending = store.take(100).unwrap().unwrap();
        store.set(false).unwrap();
        store.give_back(sending).unwrap();
        assert!(store.peek().unwrap().is_empty());
        assert!(store.take(100).unwrap().is_none());
        assert_eq!(store.settings().unwrap().install_id, None);

        let id = store.set(true).unwrap().install_id.unwrap();
        let new = store.reset().unwrap().unwrap();
        store
            .absorb_for(pending(99, 1), 100, true, Some(&id))
            .unwrap();
        assert!(store.peek().unwrap().is_empty());
        store
            .absorb_for(pending(99, 1), 100, true, Some(&new))
            .unwrap();
        assert!(!store.peek().unwrap().is_empty());
    }

    #[test]
    fn nothing_is_kept_without_an_install_id() {
        let (_dir, store) = store();
        store.absorb_at(pending(100, 1), 100, true).unwrap();
        assert!(store.peek().unwrap().is_empty());
        store.set(true).unwrap();
        store.set(false).unwrap();
        store.absorb_at(pending(100, 1), 100, true).unwrap();
        assert!(store.peek().unwrap().is_empty());
    }

    #[test]
    fn an_unreadable_queue_is_dropped_instead_of_stopping_later_writes() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        fs::write(store.dir.join(QUEUE), b"").unwrap();
        store.absorb_at(pending(100, 1), 100, true).unwrap();
        assert_eq!(store.peek().unwrap().hours.len(), 1);
    }

    #[test]
    fn a_queue_that_cannot_be_read_is_left_alone() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        fs::create_dir(store.dir.join(QUEUE)).unwrap();
        assert!(store.absorb_at(pending(100, 1), 100, true).is_err());
        assert!(store.dir.join(QUEUE).is_dir());
    }

    #[test]
    fn the_directory_is_the_owner_s_alone() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, store) = store();
        store.set(true).unwrap();
        let mode = fs::metadata(&store.dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn old_hours_and_events_are_dropped() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        store
            .absorb_at(pending(400 - KEEP_HOURS - 1, 1), 400 - KEEP_HOURS - 1, true)
            .unwrap();
        store.absorb_at(pending(400, 1), 400, true).unwrap();
        let queued = store.peek().unwrap();
        assert_eq!(
            queued
                .hours
                .iter()
                .map(|report| report.hour)
                .collect::<Vec<_>>(),
            [400]
        );
        assert!(
            queued
                .events
                .iter()
                .all(|queued| queued.event.hour() == 400)
        );
    }

    #[test]
    fn the_queue_holds_at_most_20_events_of_a_kind_a_day() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        for _ in 0..30 {
            store
                .absorb_at(
                    Pending {
                        hours: BTreeMap::new(),
                        events: vec![panic_on(100)],
                    },
                    100,
                    true,
                )
                .unwrap();
        }
        assert_eq!(store.peek().unwrap().events.len(), Event::PER_KIND_PER_DAY);
    }

    #[test]
    fn a_busy_queue_is_skipped_instead_of_waited_for() {
        let (_dir, store) = store();
        let _held = store.lock(true).unwrap();
        let error = store.absorb_at(pending(100, 1), 100, false).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    }
}
