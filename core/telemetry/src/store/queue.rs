// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::io;

use koidrive_state_file::Flock;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::consent;
use crate::recorder::Pending;
use crate::schema::{Event, EventUuid, HourlyReport, InstallId, Origin, Version};

use super::{KEEP_HOURS, MAX_APP_CRASH_REPORTS, MAX_EVENTS, QUEUE, Store, write};

/// What the app writes into its crash reports each time it reads the setting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppCrashStamp {
    /// Reports are on for the reading process, its environment included.
    pub on: bool,
    pub generation: String,
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
    _lock: Flock,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub(super) struct Queue {
    #[serde(default)]
    pub(super) hours: BTreeMap<u32, HourlyReport>,
    #[serde(default, deserialize_with = "known_events")]
    pub(super) events: Vec<Queued>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) out: Option<Out>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    admitted: BTreeMap<u32, BTreeMap<String, usize>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    app_crash_reports: Vec<String>,
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
pub(super) struct Out {
    id: InstallId,
    pub(super) batch: Batch,
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
    pub(super) fn put(
        &mut self,
        hours: impl IntoIterator<Item = HourlyReport>,
        events: Vec<Queued>,
    ) {
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

    pub(super) fn trim(&mut self, hour: u32) {
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

impl Store {
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

    pub(super) fn absorb_for(
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

    pub fn app_crash_stamp(
        &self,
        var: impl Fn(&str) -> Option<std::ffi::OsString>,
    ) -> io::Result<AppCrashStamp> {
        let settings = self.settings()?;
        Ok(AppCrashStamp {
            on: consent::resolve(var, settings.enabled).mode != consent::Mode::Off,
            generation: settings.generation.unwrap_or_default(),
        })
    }

    /// Queues the crash in `report`, a crash report KSCrash 2.6 wrote. Only the crash's kind, Mach exception, signal and
    /// time are read, with the build UUID of the executable that crashed, the offsets of the crashed thread's frames in it
    /// and what the app stamped into `user`: `koidrive_version`, which the event goes under, and the
    /// [`AppCrashStamp`] as `reports_on` and `reports_generation`; paths, names, messages, memory and everything else
    /// in the report are never read. The crash is queued, once by its report id, only when the stamp says reports were
    /// on, its generation is still in force and reports are on now under `var`.
    pub fn absorb_app_crash(
        &self,
        report: &Value,
        var: impl Fn(&str) -> Option<std::ffi::OsString>,
    ) -> io::Result<()> {
        let stamped = (report["user"]["reports_on"] == true)
            .then(|| report["user"]["reports_generation"].as_str())
            .flatten();
        let (Some(event), Some(stamped)) = (crate::app_crash::app_crash(report), stamped) else {
            return Ok(());
        };
        let id = report["report"]["id"].as_str().map(str::to_owned);
        let _lock = self.lock(true)?;
        let mut settings = self.settings()?;
        if consent::resolve(&var, settings.enabled).mode == consent::Mode::Off
            || settings.generation.as_deref().unwrap_or_default() != stamped
        {
            return Ok(());
        }
        if settings.install_id.is_none() {
            self.remove_queues()?;
            settings.install_id = Some(InstallId::generate());
            write(&self.dir.join("settings.json"), &settings)?;
        }
        let mut queue = self.queue()?;
        if id
            .as_ref()
            .is_some_and(|id| queue.app_crash_reports.contains(id))
        {
            return Ok(());
        }
        let crashed = report["user"]["koidrive_version"]
            .as_str()
            .and_then(|version| Version::parse(version).ok());
        let origin = (self.origin.clone()).map(|origin| Origin {
            koi_version: crashed.unwrap_or(origin.koi_version),
            ..origin
        });
        queue.put(std::iter::empty(), vec![Queued { event, origin }]);
        queue.app_crash_reports.extend(id);
        let over = queue
            .app_crash_reports
            .len()
            .saturating_sub(MAX_APP_CRASH_REPORTS);
        queue.app_crash_reports.drain(..over);
        queue.trim(crate::this_hour());
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
}
