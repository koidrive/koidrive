// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;

use koidrive_telemetry::Recorder;
use koidrive_telemetry::schema::{Age, Halted, Level};

pub(crate) struct Health {
    pub halt: Option<Halting>,
    pub synced_ago: Option<u64>,
    pub pending: u64,
}

pub(crate) struct Halting {
    pub why: Option<Halted>,
    pub seconds: u64,
}

pub(crate) fn record(recorder: &Recorder, now: u64, paused: bool, folders: &[Health]) {
    if folders.is_empty() {
        return;
    }
    let hour = u32::try_from(now / 3_600).unwrap_or(u32::MAX);
    let level = |level, value| recorder.level_on(hour, level, value);
    let halts: Vec<&Halting> = folders
        .iter()
        .filter_map(|folder| folder.halt.as_ref())
        .collect();
    level(Level::Halted, halts.len() as u64);
    level(
        Level::OldestHaltMinutes,
        (halts.iter())
            .map(|halt| halt.seconds / 60)
            .max()
            .unwrap_or(0),
    );
    let mut reasons: BTreeMap<Halted, u64> = BTreeMap::new();
    for why in halts.iter().filter_map(|halt| halt.why) {
        *reasons.entry(why).or_default() += 1;
    }
    for (why, folders) in reasons {
        level(Level::HaltedBy(why), folders);
    }
    let mut ages: BTreeMap<Age, u64> = BTreeMap::new();
    for folder in folders {
        *ages
            .entry(folder.synced_ago.map_or(Age::Unknown, Age::of))
            .or_default() += 1;
    }
    for (age, folders) in ages {
        level(Level::LastSynced(age), folders);
    }
    level(
        Level::PendingNodes,
        folders.iter().map(|folder| folder.pending).sum(),
    );
    level(Level::Paused, u64::from(paused));
}

#[cfg(test)]
pub(crate) mod tests {
    use std::fs;

    use koidrive_telemetry::schema::Client;
    use koidrive_telemetry::{Batch, Environment, InstallId, Pending, Queued};
    use serde_json::{Value, json};

    use super::*;
    use crate::claim::Identity;
    use crate::lifecycle::Lifecycle;
    use crate::status::FolderStatus;

    pub(crate) fn posthog(pending: Pending) -> Vec<Value> {
        let batch = Batch {
            hours: pending.hours.into_values().collect(),
            events: (pending.events.into_iter())
                .map(|event| Queued {
                    event,
                    origin: None,
                })
                .collect(),
        };
        let environment = Environment::detect(Client::CliDaemon, "0.1.0");
        let body =
            koidrive_telemetry::batch("token", &InstallId::generate(), &environment, &batch, &[]);
        body["batch"].as_array().unwrap().clone()
    }

    pub(crate) fn named<'a>(events: &'a [Value], name: &str) -> Vec<&'a Value> {
        (events.iter())
            .filter(|event| event["event"] == name)
            .map(|event| &event["properties"])
            .collect()
    }

    pub(crate) fn with_prefix<'a>(properties: &'a Value, prefix: &str) -> Vec<(&'a str, u64)> {
        (properties.as_object().unwrap().iter())
            .filter(|(key, _)| key.starts_with(prefix))
            .map(|(key, value)| (key.as_str(), value.as_u64().unwrap()))
            .collect()
    }

    fn stopped() -> FolderStatus {
        serde_json::from_value(json!({
            "space_id": "space-of-notes", "space_name": "Alice Notes", "path": "/Users/alice/Notes",
            "state": "stopped", "history_id": null, "last_seq": null, "head_seq": null,
            "remote_nodes": 3198, "local_entries": null, "questions": 0, "problem": null,
        }))
        .unwrap()
    }

    #[test]
    fn a_28_hour_halt_shows_in_every_hourly_report_and_its_end_in_a_cleared_event() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store.sqlite");
        let cleared_at = koidrive_telemetry::now();
        let halted_at = cleared_at - 28 * 3_600;
        let stored = json!({
            "halt": {
                "problem": "the folder at /Users/alice/Notes is not the one linked; checking again in 30 seconds",
                "recovery": "Root",
                "why": "root",
                "since": halted_at,
            },
            "root": null,
            "synced": halted_at - 60,
        });
        fs::write(store.with_file_name("lifecycle.json"), stored.to_string()).unwrap();
        let recorder = Recorder::on();
        let lifecycle = Lifecycle::open(&store, stopped(), recorder.clone());
        for hour in 0..=28 {
            let at = halted_at + hour * 3_600;
            record(&recorder, at, false, &[lifecycle.health(at)]);
        }
        lifecycle
            .claimed(Identity::of(&fs::metadata(dir.path()).unwrap()))
            .unwrap();
        let later = cleared_at + 3_600;
        record(&recorder, later, false, &[lifecycle.health(later)]);

        let events = posthog(recorder.take());
        let reports = named(&events, "hourly_report");
        assert_eq!(reports.len(), 30);
        let halted: Vec<_> = (reports.iter())
            .filter(|report| report["l.sync.halted"].as_u64().unwrap() > 0)
            .collect();
        assert_eq!(halted.len(), 29);
        for report in &halted {
            assert_eq!(with_prefix(report, "l.halted."), [("l.halted.root", 1)]);
        }
        let longest = (halted.iter())
            .map(|report| report["l.sync.oldest_halt_minutes"].as_u64().unwrap())
            .max();
        assert_eq!(longest, Some(28 * 60));
        assert_eq!(halted[28]["l.last_synced.1d_3d"], 1);
        assert_eq!(halted[0]["l.last_synced.under_1h"], 1);
        let after = reports[29];
        assert_eq!(after["l.sync.halted"], 0);
        assert_eq!(after["l.sync.oldest_halt_minutes"], 0);
        assert!(with_prefix(after, "l.halted.").is_empty(), "{after}");
        let cleared = named(&events, "problem_cleared");
        assert_eq!(cleared.len(), 1);
        assert_eq!(cleared[0]["problem"], "halted.root");
        assert_eq!(cleared[0]["lasted"], "1d_3d");
        let text = Value::Array(events).to_string();
        for private in ["alice", "Alice", "/Users", "space-of-notes"] {
            assert!(!text.contains(private), "{private} in {text}");
        }
    }

    #[test]
    fn health_counts_folders_per_reason_and_age_and_sums_their_pending_nodes() {
        let recorder = Recorder::on();
        let halt = |why, seconds| Some(Halting { why, seconds });
        record(
            &recorder,
            0,
            true,
            &[
                Health {
                    halt: halt(Some(Halted::Root), 90 * 60),
                    synced_ago: Some(2 * 3_600),
                    pending: 3,
                },
                Health {
                    halt: halt(Some(Halted::Root), 60),
                    synced_ago: None,
                    pending: 0,
                },
                Health {
                    halt: halt(None, 7_200 * 60),
                    synced_ago: None,
                    pending: 0,
                },
                Health {
                    halt: None,
                    synced_ago: Some(0),
                    pending: 4,
                },
            ],
        );
        let report = &named(&posthog(recorder.take()), "hourly_report")[0].clone();
        let mut levels = with_prefix(report, "l.");
        levels.sort();
        assert_eq!(
            levels,
            [
                ("l.halted.root", 2),
                ("l.last_synced.1h_6h", 1),
                ("l.last_synced.under_1h", 1),
                ("l.last_synced.unknown", 2),
                ("l.sync.halted", 3),
                ("l.sync.oldest_halt_minutes", 7_200),
                ("l.sync.paused", 1),
                ("l.sync.pending_nodes", 7),
            ]
        );
    }

    #[test]
    fn no_folder_records_no_health() {
        let recorder = Recorder::on();
        record(&recorder, 0, false, &[]);
        assert_eq!(recorder.take(), Pending::default());
    }
}
