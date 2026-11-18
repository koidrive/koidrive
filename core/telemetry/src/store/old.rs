// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::io;

use koidrive_state_file::Flock;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::schema::{EventUuid, InstallId};

use super::{OLD_QUEUE, Store, read, remove, write};

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
    _lock: Flock,
}

impl Store {
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
}
