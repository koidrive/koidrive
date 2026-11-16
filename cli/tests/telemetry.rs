// SPDX-License-Identifier: Apache-2.0

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[test]
fn preview_round_trips_every_timing_field_and_drops_private_fixture_fields() {
    use koidrive_telemetry::{
        Recorder, Store,
        schema::{Counter, Measure},
    };
    use serde_json::{Value, json};

    let home = tempfile::tempdir().unwrap();
    stdout(&koi(home.path(), &["telemetry", "on"], &[]));
    let recorder = Recorder::on();
    let hour = koidrive_telemetry::this_hour();
    for measure in Measure::all() {
        recorder.measure_on(hour, measure, 1234);
        if matches!(
            measure,
            Measure::FirstSync(_) | Measure::RemoteApplyMilliseconds | Measure::SyncingMilliseconds
        ) {
            for millis in [60_000, 600_000, 5_280_000] {
                recorder.measure_on(hour, measure, millis);
            }
        }
    }
    recorder.count_on(hour, Counter::CpuMilliseconds, 4567);
    let pending = recorder.take();
    let expected = serde_json::to_value(&pending.hours[&hour]).unwrap();
    let store = Store::new(&home.path().join("state/koidrive"));
    store.absorb(pending).unwrap();
    let queue = home.path().join("state/koidrive/telemetry/queue-v2.json");
    let mut value: Value = serde_json::from_slice(&std::fs::read(&queue).unwrap()).unwrap();
    let report = &mut value["hours"][hour.to_string()];
    let secrets = [
        home.path().display().to_string(),
        "private-name.txt".into(),
        "private-space-id".into(),
        "1794527123456".into(),
    ];
    report["path"] = json!(secrets[0]);
    report["name"] = json!(secrets[1]);
    report["id"] = json!(secrets[2]);
    report["committed_at_ms"] = json!(1794527123456u64);
    report["counters"]["private-space-id"] = json!({ "private-name.txt": 1794527123456u64 });
    report["counters"]["sync"]["unfinished"] = json!(2);
    report["histograms"]["first_sync_ms"]["private-space-id"] =
        json!({"shift": 10, "buckets": vec![0;16], "count": 1, "sum": 1794527123456u64});
    report["histograms"]["syncing_ms"]["active_age"] =
        json!({"shift": 10, "buckets": vec![0;16], "count": 1, "sum": 1794527123456u64});
    for metric in ["first_sync_ms", "remote_apply_ms", "syncing_ms"] {
        for histogram in report["histograms"][metric]
            .as_object_mut()
            .unwrap()
            .values_mut()
        {
            histogram["sum"] = json!(1794527123456u64);
        }
    }
    std::fs::write(&queue, serde_json::to_vec(&value).unwrap()).unwrap();
    let preview = stdout(&koi(home.path(), &["telemetry", "preview"], &[]));
    let body: Value = serde_json::from_str(&preview).unwrap();
    let properties = &hourly_report_with(&body, "c.process.cpu_ms")["properties"];
    assert_eq!(properties["c.process.cpu_ms"], 4567);
    for rejected in ["c.sync.unfinished", "h.syncing_ms.active_age"] {
        assert!(!properties.as_object().unwrap().contains_key(rejected));
    }
    for metric in [
        "h.first_sync_ms.0",
        "h.remote_apply_ms.effect",
        "h.syncing_ms.completed",
    ] {
        let histogram = &properties[metric];
        assert_eq!(histogram["shift"], 10);
        for bucket in [1, 6, 10, 13] {
            assert_eq!(histogram["buckets"][bucket], 1);
        }
        assert_eq!(histogram["count"], 4);
        assert!(histogram.get("sum").is_none());
    }
    for (metric, labels) in expected["histograms"].as_object().unwrap() {
        for (label, histogram) in labels.as_object().unwrap() {
            assert_eq!(&properties[format!("h.{metric}.{label}")], histogram);
            if matches!(
                metric.as_str(),
                "first_sync_ms" | "remote_apply_ms" | "syncing_ms"
            ) {
                assert!(histogram.get("sum").is_none());
            } else {
                assert_eq!(histogram["sum"], 1234);
            }
        }
    }
    fn private_free(value: &Value, secrets: &[String]) {
        match value {
            Value::Object(fields) => {
                for (key, value) in fields {
                    assert!(secrets.iter().all(|secret| !key.contains(secret)));
                    private_free(value, secrets);
                }
            }
            Value::Array(values) => {
                for value in values {
                    private_free(value, secrets);
                }
            }
            Value::String(text) => assert!(secrets.iter().all(|secret| !text.contains(secret))),
            Value::Number(number) => assert_ne!(number.as_u64(), Some(1794527123456)),
            _ => {}
        }
    }
    private_free(&body, &secrets);
}

fn koi(home: &Path, args: &[&str], vars: &[(&str, &str)]) -> Output {
    command(home, args, vars).output().expect("run koi")
}

fn command(home: &Path, args: &[&str], vars: &[(&str, &str)]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_koi"));
    command
        .args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_RUNTIME_DIR", home.join("run"))
        .env_remove("DO_NOT_TRACK")
        .env_remove("KOIDRIVE_TELEMETRY")
        .env("KOIDRIVE_TELEMETRY_URL", "http://127.0.0.1:9")
        .env("KOIDRIVE_UPDATE_CHECK", "0")
        .env_remove("KOIDRIVE_TELEMETRY_TOKEN")
        .env_remove("CI");
    for (name, value) in vars {
        command.env(name, value);
    }
    command
}

fn stdout(output: &Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).unwrap()
}

#[test]
fn reports_are_off_until_someone_turns_them_on() {
    let home = tempfile::tempdir().unwrap();
    let status = stdout(&koi(home.path(), &["telemetry", "status"], &[]));
    assert!(
        status.contains("Usage reports: off (not asked yet)"),
        "{status}"
    );
    assert!(!home.path().join("state/koidrive/telemetry").exists());
}

#[test]
fn noninteractive_setup_never_asks_about_reports() {
    for (args, vars) in [
        (vec!["setup"], vec![]),
        (vec!["setup"], vec![("CI", "1")]),
        (vec!["setup", "--json"], vec![]),
    ] {
        let home = tempfile::tempdir().unwrap();
        let text = stdout(&koi(home.path(), &args, &vars));
        assert!(!text.contains("Send usage reports"), "{text}");
        assert!(!home.path().join("state/koidrive/telemetry").exists());
    }
}

#[test]
fn a_command_run_with_reports_on_is_counted_by_name_only() {
    let home = tempfile::tempdir().unwrap();
    stdout(&koi(home.path(), &["telemetry", "on"], &[]));
    stdout(&koi(home.path(), &["telemetry", "status"], &[]));
    let preview = stdout(&koi(home.path(), &["telemetry", "preview"], &[]));
    let body: serde_json::Value = serde_json::from_str(&preview).unwrap();
    let report = hourly_report_with(&body, "c.command.telemetry");
    assert_eq!(report["properties"]["c.command.telemetry"], 1);
    assert!(
        !preview.contains(&home.path().display().to_string()),
        "{preview}"
    );
}

#[test]
fn a_failed_command_is_an_event_with_its_group_and_code_only() {
    let home = tempfile::tempdir().unwrap();
    stdout(&koi(home.path(), &["telemetry", "on"], &[]));
    let failed = koi(home.path(), &["--json", "space", "list"], &[]);
    let envelope: serde_json::Value = serde_json::from_slice(&failed.stdout).unwrap();
    assert_eq!(envelope["error"]["code"], "auth_required", "{envelope}");
    let preview = stdout(&koi(home.path(), &["telemetry", "preview"], &[]));
    let body: serde_json::Value = serde_json::from_str(&preview).unwrap();
    let failures: Vec<_> = (body["batch"].as_array().unwrap().iter())
        .filter(|event| event["event"] == "command_failed")
        .collect();
    assert_eq!(failures.len(), 1, "{preview}");
    assert_eq!(failures[0]["properties"]["command"], "space");
    assert_eq!(failures[0]["properties"]["code"], "auth_required");
    let message = envelope["error"]["message"].as_str().unwrap();
    assert!(!preview.contains(message), "{preview}");
}

#[test]
fn a_daemon_that_cannot_start_is_counted_by_why() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempfile::tempdir().unwrap();
    stdout(&koi(home.path(), &["telemetry", "on"], &[]));
    let runtime = home.path().join("run/koidrive");
    std::fs::create_dir_all(&runtime).unwrap();
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();
    let daemon = koi(home.path(), &["_daemon"], &[]);
    assert!(!daemon.status.success());
    let preview = stdout(&koi(home.path(), &["telemetry", "preview"], &[]));
    let body: serde_json::Value = serde_json::from_str(&preview).unwrap();
    let report = &hourly_report_with(&body, "c.daemon_start_failed.socket")["properties"];
    assert_eq!(report["c.daemon_start_failed.socket"], 1, "{preview}");
    assert!(!preview.contains("run/koidrive"), "{preview}");
}

#[test]
fn off_deletes_the_id_and_what_waits_and_stays_off() {
    let home = tempfile::tempdir().unwrap();
    stdout(&koi(home.path(), &["telemetry", "on"], &[]));
    stdout(&koi(home.path(), &["telemetry", "status"], &[]));
    stdout(&koi(home.path(), &["telemetry", "off"], &[]));
    let status = stdout(&koi(home.path(), &["telemetry", "status"], &[]));
    assert!(
        status.contains("Usage reports: off (your choice)"),
        "{status}"
    );
    assert!(!status.contains("Install id"), "{status}");
    assert_eq!(
        stdout(&koi(home.path(), &["telemetry", "preview"], &[])).trim(),
        "Nothing waits to be sent."
    );
    assert!(
        !home
            .path()
            .join("state/koidrive/telemetry/queue-v2.json")
            .exists()
    );
}

#[test]
fn do_not_track_and_the_variable_override_the_choice() {
    let home = tempfile::tempdir().unwrap();
    stdout(&koi(home.path(), &["telemetry", "on"], &[]));
    let status = stdout(&koi(
        home.path(),
        &["telemetry", "status"],
        &[("DO_NOT_TRACK", "1")],
    ));
    assert!(
        status.contains("Usage reports: off (DO_NOT_TRACK is set)"),
        "{status}"
    );
    let status = stdout(&koi(
        home.path(),
        &["telemetry", "status"],
        &[("KOIDRIVE_TELEMETRY", "0")],
    ));
    assert!(
        status.contains("Usage reports: off (KOIDRIVE_TELEMETRY is set)"),
        "{status}"
    );
}

#[test]
fn the_telemetry_flag_of_a_command_that_never_asks_stores_the_choice() {
    let home = tempfile::tempdir().unwrap();
    let joined = koi(
        home.path(),
        &["join", "not-a-code", "--telemetry", "on"],
        &[],
    );
    assert!(!joined.status.success());
    let status = stdout(&koi(home.path(), &["telemetry", "status"], &[]));
    assert!(
        status.contains("Usage reports: on (your choice)"),
        "{status}"
    );
    let preview = stdout(&koi(home.path(), &["telemetry", "preview"], &[]));
    assert!(
        preview.contains("\"event\": \"pair\"") || preview.contains("\"step\": \"pair\""),
        "{preview}"
    );
}

#[test]
fn the_variable_turns_reports_on_without_a_stored_choice() {
    let home = tempfile::tempdir().unwrap();
    let on = [("KOIDRIVE_TELEMETRY", "1")];
    stdout(&koi(home.path(), &["telemetry", "status"], &on));
    let preview = stdout(&koi(home.path(), &["telemetry", "preview"], &on));
    assert!(preview.contains("\"c.command.telemetry\": 1"), "{preview}");
}

#[test]
fn reset_draws_a_new_id() {
    let home = tempfile::tempdir().unwrap();
    stdout(&koi(home.path(), &["telemetry", "on"], &[]));
    let id = |home: &Path| {
        let status = stdout(&koi(home, &["telemetry", "status"], &[]));
        status
            .lines()
            .find_map(|line| line.strip_prefix("Install id: ").map(str::to_owned))
            .unwrap()
    };
    let before = id(home.path());
    assert_eq!(before.len(), 32);
    stdout(&koi(home.path(), &["telemetry", "reset"], &[]));
    assert_ne!(id(home.path()), before);
}

#[test]
fn every_daemon_start_sends_the_reports_of_past_hours() {
    use koidrive_telemetry::schema::{Command as Name, Counter};
    use koidrive_telemetry::{Recorder, Store};

    let home = tempfile::tempdir().unwrap();
    stdout(&koi(home.path(), &["telemetry", "on"], &[]));
    let store = Store::new(&home.path().join("state/koidrive"));
    let (url, bodies) = posthog(0);
    let hour = koidrive_telemetry::this_hour();
    for (hour, name, property) in [
        (hour - 1, Name::Folder, "c.command.folder"),
        (hour - 2, Name::Space, "c.command.space"),
    ] {
        let recorder = Recorder::on();
        recorder.count_on(hour, Counter::Command(name), 1);
        store.absorb(recorder.take()).unwrap();
        let vars = [
            ("KOIDRIVE_TELEMETRY_URL", url.as_str()),
            ("KOIDRIVE_TELEMETRY_TOKEN", "phc_test"),
        ];
        let mut daemon = command(home.path(), &["_daemon"], &vars)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("run koi");
        let body = bodies.recv_timeout(Duration::from_secs(30));
        let queue = home.path().join("state/koidrive/telemetry/queue-v2.json");
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while std::fs::read_to_string(&queue).is_ok_and(|queue| queue.contains("\"out\""))
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        Command::new("kill")
            .args(["-TERM", &daemon.id().to_string()])
            .status()
            .unwrap();
        daemon.wait().unwrap();
        let body = body.expect("the daemon sent nothing as it started");
        let reports: Vec<_> = (body["batch"].as_array().unwrap().iter())
            .filter(|event| {
                ["c.command.folder", "c.command.space"]
                    .iter()
                    .any(|name| event["properties"].get(name).is_some())
            })
            .collect();
        assert_eq!(reports.len(), 1, "{body}");
        assert_eq!(reports[0]["event"], "hourly_report", "{body}");
        assert_eq!(reports[0]["properties"][property], 1, "{body}");
        assert_eq!(reports[0]["properties"]["client"], "cli_daemon", "{body}");
    }
}

#[test]
fn a_daemon_sends_again_a_tick_after_the_network_returns_and_sends_as_it_stops() {
    use koidrive_telemetry::schema::{Event, SourceFile};
    use koidrive_telemetry::{Pending, Store};

    let home = tempfile::tempdir().unwrap();
    stdout(&koi(home.path(), &["telemetry", "on"], &[]));
    let store = Store::new(&home.path().join("state/koidrive"));
    let today = koidrive_telemetry::now() / 86_400 * 86_400;
    let happen = |at: u64, line: u32| {
        let event = Event::Panic {
            at,
            file: SourceFile::from_location("cli/src/main.rs"),
            line,
            frames: Vec::new(),
        };
        let pending = Pending {
            events: vec![event],
            ..Pending::default()
        };
        store.absorb(pending).unwrap();
    };
    let queue = home.path().join("state/koidrive/telemetry/queue-v2.json");
    let settled = || {
        let deadline = Instant::now() + Duration::from_secs(30);
        while std::fs::read_to_string(&queue).is_ok_and(|queue| queue.contains("\"out\""))
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let panics = |body: &serde_json::Value| {
        (body["batch"].as_array().unwrap().iter())
            .filter(|event| event["event"] == "panic")
            .map(|event| {
                let line = event["properties"]["line"].as_u64().unwrap();
                let at = event["timestamp"].as_str().unwrap();
                (line, at[at.len() - 10..].to_owned())
            })
            .collect::<Vec<_>>()
    };
    let (url, bodies) = posthog(1);
    let next_with_panics = |timeout: Duration| {
        let deadline = Instant::now() + timeout;
        loop {
            let body = bodies
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .ok()?;
            if !panics(&body).is_empty() {
                return Some(body);
            }
        }
    };
    happen(today + 3_725, 1);
    let vars = [
        ("KOIDRIVE_TELEMETRY_URL", url.as_str()),
        ("KOIDRIVE_TELEMETRY_TOKEN", "phc_test"),
    ];
    let mut daemon = command(home.path(), &["_daemon"], &vars)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("run koi");
    let started = Instant::now();
    let body = next_with_panics(Duration::from_secs(90));
    let waited = started.elapsed();
    settled();
    happen(today + 3_726, 2);
    Command::new("kill")
        .args(["-TERM", &daemon.id().to_string()])
        .status()
        .unwrap();
    let stopping = next_with_panics(Duration::from_secs(30));
    daemon.wait().unwrap();

    let body = body.expect("the daemon did not send again after its first send failed");
    assert!(waited <= Duration::from_secs(75), "it waited {waited:?}");
    assert_eq!(panics(&body), [(1, "T01:02:05Z".to_owned())], "{body}");
    let stopping = stopping.expect("the daemon did not send as it stopped");
    assert_eq!(
        panics(&stopping),
        [(2, "T01:02:06Z".to_owned())],
        "{stopping}"
    );
}

#[test]
fn every_record_an_earlier_release_queued_arrives_once_through_a_failed_send_and_a_downgrade() {
    use koidrive_telemetry::schema::{Event, SourceFile};
    use koidrive_telemetry::{EventUuid, Pending, Store};
    use serde_json::{Value, json};

    let home = tempfile::tempdir().unwrap();
    stdout(&koi(home.path(), &["telemetry", "on"], &[]));
    let dir = home.path().join("state/koidrive/telemetry");
    let settings: Value =
        serde_json::from_slice(&std::fs::read(dir.join("settings.json")).unwrap()).unwrap();
    let id = settings["install_id"].clone();
    let uuid = |release: u64, n: u64| format!("00000000-0000-4000-8000-{release:06x}{n:06x}");
    let alpha5 = |release: u64| {
        json!({
            "days": {
                "20770": {
                    "day": 20770,
                    "counters": {
                        "command": { "status": release, "Alice Notes": 1 },
                        "/home/alice/private": { "confidential.txt": 1 },
                    },
                    "levels": { "/home/alice/private": { "confidential.txt": 1 } },
                },
                "20771": { "day": 20771, "counters": { "command": { "folder": release } } },
            },
            "events": [{
                "type": "panic", "day": 20771, "file": "cli/src/main.rs", "line": release,
                "message": "secret error text from /home/alice/private",
            }],
            "out": {
                "id": id,
                "batch": {
                    "days": [{ "day": 20769, "counters": { "command": { "space": release } } }],
                    "events": [{
                        "type": "setup_step", "day": 20769, "flow": "cloudflare", "step": "r2_checkout",
                        "outcome": "failed", "seconds_magnitude": release,
                    }],
                },
                "uuids": [uuid(release, 1), uuid(release, 2)],
            },
        })
    };
    let records = |release: u64| {
        let mut records = [
            (
                "daily_report",
                "c.command.space",
                "2026-11-12",
                Some(uuid(release, 1)),
            ),
            (
                "setup_step",
                "seconds_magnitude",
                "2026-11-12",
                Some(uuid(release, 2)),
            ),
            ("daily_report", "c.command.status", "2026-11-13", None),
            ("daily_report", "c.command.folder", "2026-11-14", None),
            ("panic", "line", "2026-11-14", None),
        ]
        .map(|(event, property, day, uuid)| {
            let at = format!("{day}T00:00:00Z");
            (event.to_owned(), property.to_owned(), release, at, uuid)
        })
        .to_vec();
        records.sort();
        records
    };
    let distinct = |requests: &[(u16, Value)]| {
        let mut copies = std::collections::HashMap::new();
        let mut events = Vec::new();
        for (_, body) in requests.iter().filter(|(status, _)| *status == 200) {
            for event in body["batch"].as_array().unwrap() {
                let Some(uuid) = event["uuid"].as_str() else {
                    events.push(event.clone());
                    continue;
                };
                match copies.entry(uuid.to_owned()) {
                    std::collections::hash_map::Entry::Occupied(first) => {
                        assert_eq!(first.get(), event, "two copies of {uuid} differ");
                    }
                    std::collections::hash_map::Entry::Vacant(slot) => {
                        slot.insert(event.clone());
                        events.push(event.clone());
                    }
                }
            }
        }
        events
    };
    let arrived = |requests: &[(u16, Value)]| {
        let mut arrived = Vec::new();
        for event in &distinct(requests) {
            let properties = event["properties"].as_object().unwrap();
            if properties["schema"] != 1 {
                continue;
            }
            assert!(!properties.contains_key("koi_version"), "{event}");
            let (property, release) = [
                "c.command.space",
                "c.command.status",
                "c.command.folder",
                "seconds_magnitude",
                "line",
            ]
            .into_iter()
            .find_map(|key| Some((key.to_owned(), properties.get(key)?.as_u64()?)))
            .unwrap();
            let uuid = event["uuid"].as_str().unwrap_or_default().to_owned();
            arrived.push((
                event["event"].as_str().unwrap().to_owned(),
                property,
                release,
                event["timestamp"].as_str().unwrap().to_owned(),
                uuid.starts_with("00000000-").then_some(uuid),
            ));
        }
        arrived.sort();
        arrived
    };
    let old_queue = dir.join("queue.json");
    let run = |failures: usize| {
        let (url, requests) = posthog_answering(failures);
        let vars = [
            ("KOIDRIVE_TELEMETRY_URL", url.as_str()),
            ("KOIDRIVE_TELEMETRY_TOKEN", "phc_test"),
        ];
        let mut daemon = command(home.path(), &["_daemon"], &vars)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("run koi");
        let deadline = Instant::now() + Duration::from_secs(120);
        while old_queue.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        Command::new("kill")
            .args(["-TERM", &daemon.id().to_string()])
            .status()
            .unwrap();
        daemon.wait().unwrap();
        assert!(
            !old_queue.exists(),
            "the earlier release's queue is still there"
        );
        let requests = requests.try_iter().collect::<Vec<_>>();
        for (_, body) in &requests {
            let text = body.to_string().to_lowercase();
            assert!(
                !text.contains("alice") && !text.contains("confidential"),
                "{body}"
            );
            for event in body["batch"].as_array().unwrap() {
                let uuid = event["uuid"].as_str().unwrap_or_default().to_owned();
                assert!(EventUuid::try_from(uuid).is_ok(), "{event}");
            }
        }
        requests
    };
    let uuids = |body: &Value| {
        (body["batch"].as_array().unwrap().iter())
            .map(|event| event["uuid"].clone())
            .collect::<Vec<_>>()
    };

    std::fs::write(&old_queue, serde_json::to_vec(&alpha5(1)).unwrap()).unwrap();
    let status: Value = serde_json::from_str(&stdout(&koi(
        home.path(),
        &["--json", "telemetry", "status"],
        &[],
    )))
    .unwrap();
    assert_eq!(
        (
            &status["data"]["waiting_days"],
            &status["data"]["waiting_events"]
        ),
        (&json!(3), &json!(2)),
        "{status}"
    );
    let preview: Value =
        serde_json::from_str(&stdout(&koi(home.path(), &["telemetry", "preview"], &[]))).unwrap();
    let mut previewed = arrived(&[(200, preview.clone())]);
    for record in &mut previewed {
        assert_eq!(record.4, None, "{preview}");
    }
    let mut expected = records(1);
    for record in &mut expected {
        record.4 = None;
    }
    expected.sort();
    previewed.sort();
    assert_eq!(previewed, expected);
    assert!(
        !preview.to_string().to_lowercase().contains("alice"),
        "{preview}"
    );
    let upgraded = run(1);
    assert_eq!(upgraded[0].0, 503);
    assert_eq!(uuids(&upgraded[0].1), uuids(&upgraded[1].1));
    assert_eq!(arrived(&upgraded), records(1));

    let pending = Pending {
        events: vec![Event::Panic {
            at: koidrive_telemetry::now(),
            file: SourceFile::from_location("cli/src/main.rs"),
            line: 99,
            frames: vec![],
        }],
        ..Pending::default()
    };
    Store::new(&home.path().join("state/koidrive"))
        .absorb(pending)
        .unwrap();
    std::fs::write(&old_queue, serde_json::to_vec(&alpha5(2)).unwrap()).unwrap();
    let upgraded_again = run(0);
    assert_eq!(arrived(&upgraded_again), records(2));
    let kept = (distinct(&upgraded_again).iter())
        .filter(|event| event["event"] == "panic" && event["properties"]["line"] == 99)
        .count();
    assert_eq!(kept, 1);
}

fn hourly_report_with<'a>(body: &'a serde_json::Value, property: &str) -> &'a serde_json::Value {
    (body["batch"].as_array().unwrap().iter())
        .find(|event| {
            event["event"] == "hourly_report" && event["properties"].get(property).is_some()
        })
        .unwrap_or_else(|| panic!("no hourly report carries {property}: {body}"))
}

fn posthog(dropped: usize) -> (String, mpsc::Receiver<serde_json::Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (sender, bodies) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming().skip(dropped) {
            let mut stream = stream.unwrap();
            let _ = sender.send(request_body(&stream));
            let response = "HTTP/1.1 200 OK\r\ncontent-length: 15\r\nconnection: close\r\n\r\n{\"status\":\"Ok\"}";
            stream.write_all(response.as_bytes()).unwrap();
        }
    });
    (url, bodies)
}

#[test]
fn the_next_command_sends_a_crash_queued_before_it_but_preview_sends_nothing() {
    use koidrive_telemetry::schema::{Event, SourceFile};
    use koidrive_telemetry::{Pending, Store};

    let home = tempfile::tempdir().unwrap();
    stdout(&koi(home.path(), &["telemetry", "on"], &[]));
    let store = Store::new(&home.path().join("state/koidrive"));
    let pending = Pending {
        events: vec![Event::Panic {
            at: koidrive_telemetry::now(),
            file: SourceFile::from_location("cli/src/main.rs"),
            line: 41,
            frames: vec![],
        }],
        ..Pending::default()
    };
    store.absorb(pending).unwrap();
    let (url, bodies) = posthog(0);
    let vars = [
        ("KOIDRIVE_TELEMETRY_URL", url.as_str()),
        ("KOIDRIVE_TELEMETRY_TOKEN", "phc_test"),
    ];
    stdout(&koi(home.path(), &["telemetry", "preview"], &vars));
    assert!(bodies.recv_timeout(Duration::from_secs(3)).is_err());
    stdout(&koi(home.path(), &["setup"], &vars));
    let body = bodies
        .recv_timeout(Duration::from_secs(10))
        .expect("the next command sent nothing");
    let panics = (body["batch"].as_array().unwrap().iter())
        .filter(|event| event["event"] == "panic" && event["properties"]["line"] == 41)
        .count();
    assert_eq!(panics, 1, "{body}");
    assert!(store.peek().unwrap().events.is_empty());
}

fn posthog_answering(failures: usize) -> (String, mpsc::Receiver<(u16, serde_json::Value)>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (sender, requests) = mpsc::channel();
    std::thread::spawn(move || {
        for (index, stream) in listener.incoming().enumerate() {
            let mut stream = stream.unwrap();
            let body = request_body(&stream);
            let status = if index < failures { 503 } else { 200 };
            let _ = sender.send((status, body));
            let response = format!(
                "HTTP/1.1 {status} X\r\ncontent-length: 15\r\nconnection: close\r\n\r\n{{\"status\":\"Ok\"}}"
            );
            stream.write_all(response.as_bytes()).unwrap();
        }
    });
    (url, requests)
}

fn request_body(stream: &std::net::TcpStream) -> serde_json::Value {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut length = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line.trim().is_empty() {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().unwrap();
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}
