// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsString;
use std::io;
use std::time::Duration;

use serde_json::{Map, Value, json};
use url::Url;

use crate::consent::Mode;
use crate::schema::{Event, EventUuid, HourlyReport, InstallId, SCHEMA};
use crate::store::{Batch, OldBatch, Store};
use crate::system::Environment;

pub const ENDPOINT: &str = "https://tel.koidrive.com";
pub const PROJECT_TOKEN: &str = "phc_zS5sUnZrgWmsxoiodya3UeRdNfBkk5Pvdpsqiii4XqZ8";
pub const ENDPOINT_VARIABLE: &str = "KOIDRIVE_TELEMETRY_URL";
pub const TOKEN_VARIABLE: &str = "KOIDRIVE_TELEMETRY_TOKEN";

const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum UploadError {
    #[error("{0} is not an https address")]
    Endpoint(String),
    #[error("the HTTP client cannot start: {0}")]
    Client(reqwest::Error),
}

/// What became of a batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sent {
    /// The queue held nothing to send.
    Nothing,
    Accepted,
    /// Printed to stderr, in [`Mode::Debug`].
    Printed,
    /// It went back to the queue, to be sent later: no token, a network error, a 408, a 429 or a 5xx.
    Later,
    /// The endpoint refused it with this status. It is dropped, since sending it again would be refused again.
    Refused(u16),
}

pub struct Uploader {
    http: reqwest::Client,
    url: Url,
    token: String,
}

impl Uploader {
    /// The endpoint and token from `KOIDRIVE_TELEMETRY_URL` and `KOIDRIVE_TELEMETRY_TOKEN`, or the built-in ones. None
    /// without a token. Requests go to `batch/` under the endpoint's path. Plain HTTP is accepted only for a loopback
    /// address, for tests.
    pub fn from_env(var: impl Fn(&str) -> Option<OsString>) -> Result<Option<Self>, UploadError> {
        let text = |name: &str| {
            var(name)
                .and_then(|value| value.into_string().ok())
                .filter(|value| !value.is_empty())
        };
        let token = text(TOKEN_VARIABLE).unwrap_or_else(|| PROJECT_TOKEN.to_owned());
        if token.is_empty() {
            return Ok(None);
        }
        let endpoint = text(ENDPOINT_VARIABLE).unwrap_or_else(|| ENDPOINT.to_owned());
        let url = Url::parse(&endpoint)
            .and_then(|mut base| {
                if !base.path().ends_with('/') {
                    base.set_path(&format!("{}/", base.path()));
                }
                base.join("batch/")
            })
            .map_err(|_| UploadError::Endpoint(endpoint.clone()))?;
        let loopback = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
        if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
            return Err(UploadError::Endpoint(endpoint));
        }
        let http = reqwest::Client::builder()
            .user_agent(concat!("koidrive-telemetry/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::none())
            .timeout(TIMEOUT)
            .build()
            .map_err(UploadError::Client)?;
        Ok(Some(Uploader { http, url, token }))
    }

    pub fn token(&self) -> &str {
        &self.token
    }
}

/// Sends what the queue holds for hours before `hour`, with every event. A failure to send leaves it in the queue,
/// and so does cancelling the send before an answer; only a failure to read or write the queue is an error. A batch
/// that went again after a failure is followed by one with what was queued meanwhile.
pub async fn send(
    store: &Store,
    uploader: Option<&Uploader>,
    environment: &Environment,
    mode: Mode,
    hour: u32,
) -> io::Result<Sent> {
    if mode == Mode::Off {
        return Ok(Sent::Nothing);
    }
    let mut sent = Sent::Nothing;
    let token = uploader.map_or("", Uploader::token);
    for _ in 0..2 {
        let Some(sending) = store.take_old()? else {
            break;
        };
        let body = old_batch(
            token,
            &sending.id,
            environment,
            &sending.batch,
            &sending.uuids,
        );
        match deliver(uploader, mode, &body).await {
            Sent::Later => return Ok(Sent::Later),
            done => {
                store.sent_old(sending)?;
                sent = done;
            }
        }
    }
    for _ in 0..2 {
        match send_one(store, uploader, environment, mode, hour).await? {
            Sent::Nothing => break,
            next @ (Sent::Accepted | Sent::Printed) => sent = next,
            failed => return Ok(failed),
        }
    }
    Ok(sent)
}

async fn send_one(
    store: &Store,
    uploader: Option<&Uploader>,
    environment: &Environment,
    mode: Mode,
    hour: u32,
) -> io::Result<Sent> {
    let Some(sending) = store.take(hour)? else {
        return Ok(Sent::Nothing);
    };
    if sending.batch.is_empty() {
        return Ok(Sent::Nothing);
    }
    let token = uploader.map_or("", Uploader::token);
    let body = self::batch(
        token,
        &sending.id,
        environment,
        &sending.batch,
        &sending.uuids,
    );
    let sent = deliver(uploader, mode, &body).await;
    if sent == Sent::Later {
        store.give_back(sending)?;
    } else {
        store.sent(sending)?;
    }
    Ok(sent)
}

async fn deliver(uploader: Option<&Uploader>, mode: Mode, body: &Value) -> Sent {
    if mode == Mode::Debug {
        eprintln!(
            "[telemetry] {}",
            serde_json::to_string_pretty(body).unwrap_or_default()
        );
        return Sent::Printed;
    }
    let Some(uploader) = uploader else {
        return Sent::Later;
    };
    match uploader
        .http
        .post(uploader.url.clone())
        .json(body)
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => Sent::Accepted,
        Ok(response)
            if matches!(response.status().as_u16(), 408 | 429)
                || response.status().is_server_error() =>
        {
            Sent::Later
        }
        Ok(response) => Sent::Refused(response.status().as_u16()),
        Err(error) => {
            tracing::debug!(%error, "telemetry was not sent; trying again later");
            Sent::Later
        }
    }
}

/// The body of a PostHog batch request: one `hourly_report` event per hour, timed at the start of its UTC hour, and
/// one event per [`Event`], timed when it happened, each with the install id as its `distinct_id` and the
/// environment as properties. `uuids` holds one per hour and then one per event, as [`Sending`](crate::Sending) does; without them, as in a
/// preview, the events have no uuid.
pub fn batch(
    token: &str,
    id: &InstallId,
    environment: &Environment,
    batch: &Batch,
    uuids: &[EventUuid],
) -> Value {
    let mut events = Vec::new();
    for report in &batch.hours {
        let mut properties = common(id, environment);
        properties.extend(sums(report));
        events.push(json!({ "event": "hourly_report", "properties": properties, "timestamp": time(u64::from(report.hour) * 3600) }));
    }
    for event in &batch.events {
        let Value::Object(mut fields) = serde_json::to_value(event).unwrap_or_default() else {
            continue;
        };
        let name = fields.remove("type").unwrap_or_default();
        fields.remove("at");
        let mut properties = common(id, environment);
        properties.extend(fields);
        events.push(
            json!({ "event": name, "properties": properties, "timestamp": time(event.at()) }),
        );
    }
    if uuids.len() == events.len() {
        for (event, uuid) in events.iter_mut().zip(uuids) {
            event["uuid"] = json!(uuid.as_str());
        }
    }
    json!({ "api_key": token, "historical_migration": false, "batch": events })
}

/// The body an earlier release would send for `batch`, with only the reports and events its schema allows.
pub fn old_batch(
    token: &str,
    id: &InstallId,
    environment: &Environment,
    batch: &OldBatch,
    uuids: &[EventUuid],
) -> Value {
    let properties = || {
        let mut properties = common(id, environment);
        properties.insert("schema".into(), json!(1));
        properties.remove("koi_version");
        properties
    };
    let mut uuids = uuids.iter().map(|uuid| json!(uuid.as_str()));
    let mut events = Vec::new();
    for report in &batch.days {
        let uuid = uuids.next();
        let stored = json!({ "hour": 0, "counters": report.counters, "levels": report.levels, "histograms": report.histograms });
        let Ok(sums) =
            serde_json::from_value::<HourlyReport>(stored).map(|report| self::sums(&report))
        else {
            continue;
        };
        let mut properties = properties();
        properties.extend(sums);
        events.push(json!({ "event": "daily_report", "properties": properties, "timestamp": time(u64::from(report.day) * 86_400) }));
        if let Some(uuid) = uuid {
            events.last_mut().unwrap()["uuid"] = uuid;
        }
    }
    for event in &batch.events {
        let uuid = uuids.next();
        let mut stored = event.fields.clone();
        stored.insert("type".into(), json!(event.name));
        stored.insert("at".into(), json!(u64::from(event.day) * 86_400));
        let Ok(Value::Object(mut fields)) =
            serde_json::from_value::<Event>(Value::Object(stored)).map(|event| json!(event))
        else {
            continue;
        };
        fields.remove("type");
        fields.remove("at");
        let mut properties = properties();
        properties.extend(fields);
        events.push(json!({ "event": event.name, "properties": properties, "timestamp": time(u64::from(event.day) * 86_400) }));
        if let Some(uuid) = uuid {
            events.last_mut().unwrap()["uuid"] = uuid;
        }
    }
    json!({ "api_key": token, "historical_migration": false, "batch": events })
}

fn common(id: &InstallId, environment: &Environment) -> Map<String, Value> {
    let mut properties = Map::new();
    properties.insert("distinct_id".into(), json!(id));
    properties.insert("$process_person_profile".into(), json!(false));
    properties.insert("$geoip_disable".into(), json!(true));
    properties.insert("schema".into(), json!(SCHEMA));
    properties.insert("client".into(), json!(environment.client));
    properties.insert("koi_version".into(), json!(environment.koi_version));
    properties.insert("build".into(), json!(environment.build));
    properties.insert("os".into(), json!(environment.os));
    if let Some(version) = environment.os_version {
        properties.insert("os_major".into(), json!(version.major));
        properties.insert("os_minor".into(), json!(version.minor));
    }
    if let Some(distro) = environment.distro {
        properties.insert("distro".into(), json!(distro));
    }
    properties.insert("arch".into(), json!(environment.arch));
    properties
}

fn sums(report: &HourlyReport) -> Map<String, Value> {
    let mut properties = Map::new();
    for (metric, labels) in &report.counters {
        for (label, count) in labels {
            properties.insert(format!("c.{metric}.{label}"), json!(count));
        }
    }
    for (metric, labels) in &report.levels {
        for (label, value) in labels {
            properties.insert(format!("l.{metric}.{label}"), json!(value));
        }
    }
    for (metric, labels) in &report.histograms {
        for (label, histogram) in labels {
            properties.insert(format!("h.{metric}.{label}"), json!(histogram));
        }
    }
    properties
}

/// Seconds since 1970-01-01T00:00Z as ISO 8601 (the civil-from-days algorithm of
/// https://howardhinnant.github.io/date_algorithms.html).
fn time(seconds: u64) -> String {
    let (day, second) = (seconds / 86_400, seconds % 86_400);
    let z = i64::try_from(day).unwrap_or(i64::MAX - 719_468) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        second / 3600,
        second / 60 % 60,
        second % 60
    )
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;
    use crate::recorder::Recorder;
    use crate::schema::{
        Arch, Build, Client, Command, Counter, Distro, ErrorCode, Event, Failure, Halted, Level,
        Measure, Os, OsVersion, Outcome, Refusal, SetupFlow, SetupStep, SourceFile, Symbol, Synced,
        Version,
    };

    #[test]
    fn report_properties_are_pinned_to_the_schema_version() {
        let recorder = Recorder::on();
        for counter in Counter::all() {
            recorder.count_on(0, counter, 3);
        }
        for level in Level::all() {
            recorder.level_on(0, level, 5);
        }
        for measure in Measure::all() {
            recorder.measure_on(0, measure, 65_536);
        }
        for cloudflare_code in [Some(10042), None] {
            recorder.event(Event::SetupStep {
                at: 3_725,
                flow: SetupFlow::Cloudflare,
                step: SetupStep::R2Checkout,
                outcome: Outcome::Failed,
                cloudflare_code,
                seconds_magnitude: 7,
            });
        }
        recorder.event(Event::Panic {
            at: 3_725,
            file: SourceFile::from_location("core/driver/src/folder.rs"),
            line: 42,
            frames: ["koidrive_driver::folder::Folder::run", "koi::main"]
                .map(|name| Symbol::ours(name).unwrap())
                .to_vec(),
        });
        for code in ErrorCode::ALL {
            recorder.event(Event::CommandFailed {
                at: 3_726,
                command: Command::Folder,
                code: *code,
            });
        }
        for problem in Failure::all() {
            recorder.event(Event::Problem {
                at: 3_727,
                problem,
                count: 2,
            });
        }
        let pending = recorder.take();
        let reports = Batch {
            hours: pending.hours.into_values().collect(),
            events: pending.events,
        };
        let environment = Environment {
            client: Client::CliDaemon,
            koi_version: Version::parse("0.1.0").unwrap(),
            build: Build::Release,
            os: Os::Linux,
            os_version: Some(OsVersion { major: 6, minor: 8 }),
            distro: Some(Distro::Debian),
            arch: Arch::Arm64,
        };
        let id = InstallId::try_from("00000000000000000000000000000000".to_owned()).unwrap();
        let uuids: Vec<_> = (0..reports.hours.len() + reports.events.len())
            .map(|index| {
                EventUuid::try_from(format!("00000000-0000-4000-8000-{index:012x}")).unwrap()
            })
            .collect();
        let actual = json!({
            "populated": batch("schema-test-token", &id, &environment, &reports, &uuids),
            "minimal": batch(
                "schema-test-token",
                &id,
                &Environment { os_version: None, distro: None, ..environment },
                &Batch {
                    hours: vec![HourlyReport { hour: 25, ..HourlyReport::default() }],
                    events: vec![],
                },
                &[],
            ),
        });
        let fixture = match SCHEMA {
            2 => include_str!("../tests/fixtures/report-schema-v2.json"),
            version => panic!("Add a report-layout fixture for SCHEMA {version}"),
        };
        std::fs::write(
            "/tmp/koi619-v2.json",
            serde_json::to_string_pretty(&actual).unwrap(),
        )
        .unwrap();
        let expected: Value = serde_json::from_str(fixture).unwrap();
        assert_eq!(
            actual, expected,
            "Report properties changed; bump SCHEMA and add its versioned fixture"
        );
    }

    fn environment() -> Environment {
        Environment::detect(Client::CliDaemon, "0.1.0")
    }

    const HOUR: u32 = 20_404 * 24;

    fn queued(store: &Store, hour: u32) {
        store.set(true).unwrap();
        let recorder = Recorder::on();
        recorder.count_on(hour, Counter::Command(Command::Setup), 2);
        recorder.count_on(hour, Counter::Sync(Synced::BytesUploaded), 5_000_000);
        recorder.count_on(
            hour,
            Counter::Failure(Failure::Halted(Halted::StoreBehind)),
            1,
        );
        recorder.count_on(
            hour,
            Counter::Failure(Failure::Refused(Refusal::QuotaExceeded)),
            3,
        );
        recorder.level_on(hour, Level::Nodes, 1_500);
        recorder.level_on(hour, Level::Nodes, 900);
        recorder.event(Event::SetupStep {
            at: u64::from(hour) * 3600 + 125,
            flow: SetupFlow::Cloudflare,
            step: SetupStep::R2Checkout,
            outcome: Outcome::Failed,
            cloudflare_code: Some(10042),
            seconds_magnitude: 7,
        });
        recorder.event(Event::Panic {
            at: u64::from(hour) * 3600 + 125,
            file: SourceFile::from_location("/Users/alice/.cargo/registry/x.rs"),
            line: 3,
            frames: vec![
                Symbol::ours("koidrive_driver::folder::Folder::run::{{closure}}").unwrap(),
            ],
        });
        store.absorb_at(recorder.take(), hour + 1, true).unwrap();
    }

    #[test]
    fn seconds_since_the_epoch_become_times() {
        assert_eq!(time(0), "1970-01-01T00:00:00Z");
        assert_eq!(time(20_404 * 86_400), "2025-11-12T00:00:00Z");
        assert_eq!(time(11_016 * 86_400 + 86_399), "2000-02-29T23:59:59Z");
        assert_eq!(time(20_404 * 86_400 + 3_725), "2025-11-12T01:02:05Z");
    }

    #[test]
    fn a_batch_is_anonymous_posthog_events() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        queued(&store, HOUR);
        let id = store.install_id().unwrap();
        let body = batch("phc_test", &id, &environment(), &store.peek().unwrap(), &[]);
        assert_eq!(body["api_key"], "phc_test");
        let events = body["batch"].as_array().unwrap();
        assert_eq!(
            events
                .iter()
                .map(|event| event["event"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["hourly_report", "setup_step", "panic"]
        );
        for event in events {
            assert_eq!(event["properties"]["distinct_id"], id.as_str());
            assert_eq!(event["properties"]["$process_person_profile"], false);
            assert_eq!(event["properties"]["$geoip_disable"], true);
            assert!(event["properties"].get("at").is_none());
        }
        assert_eq!(events[0]["timestamp"], "2025-11-12T00:00:00Z");
        assert_eq!(events[1]["timestamp"], "2025-11-12T00:02:05Z");
        assert_eq!(events[2]["timestamp"], "2025-11-12T00:02:05Z");
        assert_eq!(events[0]["properties"]["c.command.setup"], 2);
        assert_eq!(events[0]["properties"]["c.sync.bytes_uploaded"], 5_000_000);
        assert_eq!(events[0]["properties"]["l.sync.nodes"], 1_500);
        assert_eq!(events[0]["properties"]["c.halted.store_behind"], 1);
        assert_eq!(events[0]["properties"]["c.refused.quota_exceeded"], 3);
        assert_eq!(events[1]["properties"]["cloudflare_code"], 10042);
        assert_eq!(events[2]["properties"]["file"], SourceFile::EXTERNAL);
    }

    #[test]
    fn a_batch_holds_no_free_text() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        queued(&store, HOUR);
        let id = store.install_id().unwrap();
        let sending = store.take(HOUR + 1).unwrap().unwrap();
        let body = batch(
            "phc_test",
            &id,
            &environment(),
            &sending.batch,
            &sending.uuids,
        );
        assert_eq!(body["batch"][0]["uuid"], sending.uuids[0].as_str());
        let mut strings = Vec::new();
        collect(&body, &mut strings);
        let closed = |s: &str| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'$'))
        };
        for string in strings {
            let allowed = string == id.as_str()
                || sending.uuids.iter().any(|uuid| uuid.as_str() == string)
                || string == "phc_test"
                || string == "2025-11-12T00:00:00Z"
                || string == "2025-11-12T00:02:05Z"
                || string == "0.1.0"
                || closed(&string)
                || (string.starts_with("cli/") || string.starts_with("core/"))
                    && closed(&string.replace('/', "."))
                || string == "koidrive_driver::folder::Folder::run::{{closure}}";
            assert!(allowed, "free text in a batch: {string:?}");
        }
    }

    #[tokio::test]
    async fn keys_outside_the_closed_set_never_reach_a_request() {
        let (address, bodies) = server(200).await;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        queued(&store, HOUR);
        let path = dir.path().join("telemetry/queue-v2.json");
        let mut queue: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let report = &mut queue["hours"]["489696"];
        report["counters"]["/home/fixture/private"] = json!({ "confidential.txt": 1 });
        report["counters"]["command"]["Alice Notes"] = json!(1);
        report["counters"]["sync"]["Alice Notes"] = json!(1);
        report["counters"]["halted"]["Alice Notes"] = json!(1);
        report["counters"]["refused"]["/home/fixture/private"] = json!(1);
        report["levels"]["/home/fixture/private"] = json!({ "confidential.txt": 1 });
        report["levels"]["sync"]["Alice Notes"] = json!(1);
        report["histograms"]["/home/fixture"] = json!({
            "notes.txt": { "shift": 0, "buckets": vec![0u64; 16], "count": 1, "sum": 1 }
        });
        std::fs::write(&path, serde_json::to_vec(&queue).unwrap()).unwrap();

        let sent = send(
            &store,
            Some(&uploader(&address)),
            &environment(),
            Mode::On,
            HOUR + 1,
        )
        .await
        .unwrap();
        assert_eq!(sent, Sent::Accepted);
        let request = serde_json::to_string(&bodies.lock().unwrap()[0]).unwrap();
        for secret in ["fixture", "private", "confidential", "Alice", "notes"] {
            assert!(!request.contains(secret), "{secret} in {request}");
        }
        assert!(request.contains("c.command.setup"));
        assert!(request.contains("l.sync.nodes"));
        assert!(request.contains("c.halted.store_behind"));
    }

    /// A batch that was not accepted goes again with the same uuids, so PostHog keeps one copy if both arrived.
    #[tokio::test]
    async fn a_retry_sends_the_same_uuids() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        queued(&store, HOUR);
        let mut uuids = Vec::new();
        for status in [503, 200] {
            let (address, bodies) = server(status).await;
            send(
                &store,
                Some(&uploader(&address)),
                &environment(),
                Mode::On,
                HOUR + 1,
            )
            .await
            .unwrap();
            let body = bodies.lock().unwrap()[0].clone();
            uuids.push(
                (body["batch"].as_array().unwrap().iter())
                    .map(|event| event["uuid"].as_str().unwrap().to_owned())
                    .collect::<Vec<_>>(),
            );
        }
        assert_eq!(uuids[0].len(), 3);
        assert_eq!(uuids[0], uuids[1]);
        assert!(store.peek().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_cancelled_send_keeps_the_batch() {
        let silent = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", silent.local_addr().unwrap());
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        queued(&store, HOUR);
        let waited = tokio::time::timeout(
            Duration::from_millis(300),
            send(
                &store,
                Some(&uploader(&address)),
                &environment(),
                Mode::On,
                HOUR + 1,
            ),
        )
        .await;
        assert!(waited.is_err());
        drop(silent);

        let (address, bodies) = server(200).await;
        let sent = send(
            &store,
            Some(&uploader(&address)),
            &environment(),
            Mode::On,
            HOUR + 1,
        )
        .await
        .unwrap();
        assert_eq!(sent, Sent::Accepted);
        assert_eq!(
            bodies.lock().unwrap()[0]["batch"].as_array().unwrap().len(),
            3
        );
        assert!(store.peek().unwrap().is_empty());
    }

    fn collect(value: &Value, strings: &mut Vec<String>) {
        match value {
            Value::String(s) => strings.push(s.clone()),
            Value::Array(items) => items.iter().for_each(|item| collect(item, strings)),
            Value::Object(fields) => fields.iter().for_each(|(key, item)| {
                strings.push(key.clone());
                collect(item, strings);
            }),
            _ => {}
        }
    }

    #[tokio::test]
    async fn a_retry_that_goes_through_also_sends_what_was_queued_meanwhile() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        queued(&store, HOUR);
        let (down, tried) = server(503).await;
        let down = uploader(&down);
        let sent = send(&store, Some(&down), &environment(), Mode::On, HOUR + 1).await;
        assert_eq!(sent.unwrap(), Sent::Later);
        let recorder = Recorder::on();
        recorder.count_on(HOUR, Counter::Sync(Synced::BytesUploaded), 7);
        recorder.event(Event::Panic {
            at: u64::from(HOUR) * 3600 + 126,
            file: SourceFile::from_location("cli/src/main.rs"),
            line: 9,
            frames: Vec::new(),
        });
        store.absorb_at(recorder.take(), HOUR + 1, true).unwrap();

        let (up, bodies) = server(200).await;
        let up = uploader(&up);
        let sent = send(&store, Some(&up), &environment(), Mode::On, HOUR + 1).await;
        assert_eq!(sent.unwrap(), Sent::Accepted);
        assert!(store.peek().unwrap().is_empty());
        let uuids = |body: &Value| {
            (body["batch"].as_array().unwrap().iter())
                .map(|event| event["uuid"].clone())
                .collect::<Vec<_>>()
        };
        let bodies = bodies.lock().unwrap();
        assert_eq!(bodies.len(), 2);
        assert_eq!(uuids(&bodies[0]), uuids(&tried.lock().unwrap()[0]));
        assert_eq!(
            bodies[1]["batch"][0]["properties"]["c.sync.bytes_uploaded"],
            7
        );
        assert_eq!(bodies[1]["batch"][1]["properties"]["line"], 9);
    }

    async fn server(status: u16) -> (String, Arc<Mutex<Vec<Value>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let kept = bodies.clone();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0u8; 8192];
                loop {
                    let read = socket.read(&mut buffer).await.unwrap();
                    request.extend_from_slice(&buffer[..read]);
                    let text = String::from_utf8_lossy(&request);
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length = (text.lines())
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            kept.lock()
                                .unwrap()
                                .push(serde_json::from_slice(&request[end + 4..]).unwrap());
                            break;
                        }
                    }
                    if read == 0 {
                        break;
                    }
                }
                let response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        (address, bodies)
    }

    fn uploader(address: &str) -> Uploader {
        let vars = [
            (ENDPOINT_VARIABLE, address.to_owned()),
            (TOKEN_VARIABLE, "phc_test".to_owned()),
        ];
        Uploader::from_env(|name| vars.iter().find(|(k, _)| *k == name).map(|(_, v)| v.into()))
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn an_accepted_batch_leaves_the_queue() {
        let (address, bodies) = server(200).await;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        queued(&store, HOUR);
        let sent = send(
            &store,
            Some(&uploader(&address)),
            &environment(),
            Mode::On,
            HOUR + 1,
        )
        .await
        .unwrap();
        assert_eq!(sent, Sent::Accepted);
        assert!(store.peek().unwrap().is_empty());
        assert_eq!(
            bodies.lock().unwrap()[0]["batch"].as_array().unwrap().len(),
            3
        );
    }

    #[tokio::test]
    async fn a_server_error_keeps_the_batch_and_a_refusal_drops_it() {
        for (status, expected, kept) in [
            (503, Sent::Later, true),
            (408, Sent::Later, true),
            (429, Sent::Later, true),
            (400, Sent::Refused(400), false),
        ] {
            let (address, _) = server(status).await;
            let dir = tempfile::tempdir().unwrap();
            let store = Store::new(dir.path());
            queued(&store, HOUR);
            let sent = send(
                &store,
                Some(&uploader(&address)),
                &environment(),
                Mode::On,
                HOUR + 1,
            )
            .await
            .unwrap();
            assert_eq!(sent, expected, "{status}");
            assert_eq!(!store.peek().unwrap().is_empty(), kept, "{status}");
        }
    }

    #[tokio::test]
    async fn an_unreachable_endpoint_keeps_the_batch() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        queued(&store, HOUR);
        let sent = send(
            &store,
            Some(&uploader(&address)),
            &environment(),
            Mode::On,
            HOUR + 1,
        )
        .await
        .unwrap();
        assert_eq!(sent, Sent::Later);
        assert_eq!(store.peek().unwrap().hours.len(), 1);
    }

    #[tokio::test]
    async fn without_a_token_nothing_is_sent_and_off_reads_nothing() {
        let built_in = Uploader::from_env(|_| None).unwrap().unwrap();
        assert_eq!(built_in.token(), PROJECT_TOKEN);
        assert_eq!(built_in.url.as_str(), "https://tel.koidrive.com/batch/");
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        queued(&store, HOUR);
        assert_eq!(
            send(&store, None, &environment(), Mode::On, HOUR + 1)
                .await
                .unwrap(),
            Sent::Later
        );
        assert_eq!(
            send(&store, None, &environment(), Mode::Off, HOUR + 1)
                .await
                .unwrap(),
            Sent::Nothing
        );
        assert_eq!(store.peek().unwrap().events.len(), 2);
    }

    #[test]
    fn only_https_or_a_loopback_endpoint_is_accepted() {
        let with = |url: &str| {
            let vars = [
                (ENDPOINT_VARIABLE, url.to_owned()),
                (TOKEN_VARIABLE, "phc_test".to_owned()),
            ];
            Uploader::from_env(|name| vars.iter().find(|(k, _)| *k == name).map(|(_, v)| v.into()))
                .map(|uploader| uploader.map(|u| u.url.to_string()))
        };
        assert_eq!(
            with("https://tel.koidrive.com").unwrap().unwrap(),
            "https://tel.koidrive.com/batch/"
        );
        assert!(with("http://127.0.0.1:9").unwrap().is_some());
        assert_eq!(
            with("https://example.com/posthog").unwrap().unwrap(),
            "https://example.com/posthog/batch/"
        );
        assert!(with("http://tel.koidrive.com").is_err());
    }
}
