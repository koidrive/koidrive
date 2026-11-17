// SPDX-License-Identifier: Apache-2.0

#[path = "server/support/mod.rs"]
mod support;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use koidrive_net::{Api, Auth, Credentials, Session, SessionConfig, Transfer, UploadGate};
use koidrive_proto::v1::{ChunkRef, Content, CreateFile, CreateSpaceRequest, Op, op, op_result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Metadata, Subscriber};

use crate::support::{commit, connected, owner, uuidv7};

const CHUNK_BYTES: usize = 4 << 20;

fn measurement_transfer(api: &Api, n: usize) -> Transfer {
    let mut transfer = Transfer::new(api);
    transfer.gate = UploadGate::new(n);
    transfer
}

struct Config {
    ns: Vec<usize>,
    repetitions: usize,
    mib: usize,
}

impl Config {
    fn parse(ns: &str, repetitions: &str, mib: &str) -> Result<Self, &'static str> {
        let ns: Vec<usize> = ns
            .split(',')
            .map(|n| {
                n.trim()
                    .parse()
                    .map_err(|_| "Ns must be comma-separated integers")
            })
            .collect::<Result<_, _>>()?;
        if ns.is_empty() || ns.len() > 4 || ns.iter().collect::<HashSet<_>>().len() != ns.len() {
            return Err("Ns must contain one to four distinct values");
        }
        if ns.iter().any(|&n| !matches!(n, 1 | 2 | 4 | 8)) {
            return Err("N must be one of 1,2,4,8");
        }
        let repetitions = repetitions
            .parse()
            .map_err(|_| "repetitions must be 1 to 3")?;
        if !(1..=3).contains(&repetitions) {
            return Err("repetitions must be 1 to 3");
        }
        let mib = mib
            .parse()
            .map_err(|_| "MiB must be a multiple of 4, from 4 to 1024")?;
        if !(4..=1024).contains(&mib) || mib % 4 != 0 {
            return Err("MiB must be a multiple of 4, from 4 to 1024");
        }
        Ok(Self {
            ns,
            repetitions,
            mib,
        })
    }

    fn order(&self) -> Vec<(usize, usize)> {
        (0..self.repetitions)
            .flat_map(|rep| {
                (0..self.ns.len()).map(move |i| (rep + 1, self.ns[(i + rep) % self.ns.len()]))
            })
            .collect()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Owner {
    user_id: String,
    device_id: String,
    refresh_credential: String,
}

#[derive(Clone, Default)]
struct Retries {
    chunks: HashSet<String>,
    statuses: BTreeMap<String, usize>,
}

impl Retries {
    fn record(&mut self, error: &str) {
        let hash = error
            .split("chunk ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next());
        if let Some(hash) =
            hash.filter(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            self.chunks.insert(hash.to_owned());
        }
        let status = error
            .split("HTTP ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .filter(|status| status.len() == 3 && status.bytes().all(|b| b.is_ascii_digit()))
            .unwrap_or("transport/timeout");
        *self.statuses.entry(status.to_owned()).or_default() += 1;
    }

    fn text(&self) -> String {
        let counts = self
            .statuses
            .iter()
            .map(|(status, n)| format!("{status}: {n}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{} chunks; {} retry events{}",
            self.chunks.len(),
            self.statuses.values().sum::<usize>(),
            if counts.is_empty() {
                String::new()
            } else {
                format!(" ({counts})")
            }
        )
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Requests {
    sent: usize,
    chunks: usize,
}

struct RetryCounter {
    retries: Arc<Mutex<Retries>>,
    protocols: Arc<Mutex<HashSet<&'static str>>>,
    uploading: Arc<AtomicBool>,
    requests: Arc<Mutex<Requests>>,
}

impl Subscriber for RetryCounter {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        (metadata.target() == "koidrive_net::chunks" && *metadata.level() >= tracing::Level::DEBUG)
            || (metadata.target() == "hyper_util::client::legacy::client"
                && *metadata.level() == tracing::Level::TRACE)
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        #[derive(Default)]
        struct Fields {
            message: String,
            error: String,
            chunks: u64,
        }
        impl Visit for Fields {
            fn record_u64(&mut self, field: &Field, value: u64) {
                if field.name() == "chunks" {
                    self.chunks = value;
                }
            }
            fn record_i64(&mut self, field: &Field, value: i64) {
                self.record_u64(field, value.try_into().unwrap_or_default());
            }
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                match field.name() {
                    "message" => self.message = format!("{value:?}"),
                    "error" => self.error = format!("{value:?}"),
                    _ => {}
                }
            }
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        if fields.message == "koidrive chunk transfer will be retried" {
            self.retries.lock().unwrap().record(&fields.error);
        }
        if fields.message == "koidrive chunk upload request" {
            let mut requests = self.requests.lock().unwrap();
            requests.sent += 1;
            requests.chunks += fields.chunks as usize;
        }
        if self.uploading.load(Ordering::Relaxed) {
            let protocol = match fields.message.as_str() {
                "http2 handshake complete, spawning background dispatcher task" => Some("HTTP/2"),
                "http1 handshake complete, spawning background dispatcher task" => Some("HTTP/1.1"),
                _ => None,
            };
            if let Some(protocol) = protocol {
                self.protocols.lock().unwrap().insert(protocol);
            }
        }
    }
}

fn generated(mib: usize) -> (Vec<ChunkRef>, HashMap<Vec<u8>, Bytes>) {
    let mut chunks = Vec::new();
    let mut source = HashMap::new();
    for _ in 0..mib / 4 {
        let mut bytes = vec![0; CHUNK_BYTES];
        fastrand::fill(&mut bytes);
        let chunk = ChunkRef {
            hash: Sha256::digest(&bytes).to_vec(),
            size: CHUNK_BYTES as u32,
            encoding: 0,
        };
        source.insert(chunk.hash.clone(), Bytes::from(bytes));
        chunks.push(chunk);
    }
    assert_eq!(chunks.len(), source.len(), "every chunk must be fresh");
    (chunks, source)
}

fn content(chunks: &[ChunkRef]) -> Content {
    let mut digest = Sha256::new();
    for chunk in chunks {
        digest.update(&chunk.hash);
        digest.update(chunk.size.to_le_bytes());
    }
    Content {
        size: chunks.iter().map(|c| u64::from(c.size)).sum(),
        chunks: chunks.to_vec(),
        chunk_count: chunks.len() as u32,
        manifest_digest: digest.finalize().to_vec(),
        chunker: "fastcdc2020-1m-4m-16m".into(),
        ..Default::default()
    }
}

fn emit(markdown: &str) {
    println!("{markdown}");
    if let Ok(path) = std::env::var("GITHUB_STEP_SUMMARY") {
        writeln!(
            OpenOptions::new()
                .append(true)
                .create(true)
                .open(path)
                .expect("open job summary"),
            "{markdown}"
        )
        .expect("write job summary");
    }
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    }
}

async fn deployment() -> (Api, Arc<Auth>) {
    let deployed = std::env::var("KOIDRIVE_BENCH_URL").ok();
    let api = if let Some(url) = &deployed {
        Api::new(url.parse().expect("benchmark URL")).expect("HTTPS benchmark origin")
    } else {
        crate::support::api()
    };
    let credentials = if deployed.is_some() {
        let raw = std::env::var("KOIDRIVE_BENCH_OWNER").expect("claimed owner is missing");
        let owner: Owner = serde_json::from_str(&raw)
            .unwrap_or_else(|_| panic!("invalid claimed owner credentials"));
        Credentials {
            user_id: owner.user_id,
            device_id: owner.device_id,
            refresh_credential: owner.refresh_credential,
        }
    } else {
        owner().await
    };
    let auth = Arc::new(Auth::new(api.clone(), credentials));
    (api, auth)
}

fn commit_sha() -> String {
    std::env::var("GITHUB_SHA")
        .ok()
        .filter(|sha| sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()))
        .unwrap_or_else(|| "local checkout".into())
}

#[tokio::test]
#[ignore = "deploy-test bench mode, or a local Worker with KOIDRIVE_BENCH_MIB=64"]
async fn upload_benchmark() {
    let setting = |key: &str, default: &str| std::env::var(key).unwrap_or_else(|_| default.into());
    let config = Config::parse(
        &setting("KOIDRIVE_BENCH_NS", "1,2,4,8"),
        &setting("KOIDRIVE_BENCH_REPETITIONS", "3"),
        &setting("KOIDRIVE_BENCH_MIB", "1024"),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let (api, auth) = deployment().await;
    let retries = Arc::new(Mutex::new(Retries::default()));
    let protocols = Arc::new(Mutex::new(HashSet::new()));
    let uploading = Arc::new(AtomicBool::new(false));
    tracing::subscriber::set_global_default(RetryCounter {
        retries: retries.clone(),
        protocols: protocols.clone(),
        uploading: uploading.clone(),
        requests: Arc::default(),
    })
    .expect("install retry counter");

    let probe = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let mut rtts = Vec::new();
    let mut versions = HashSet::new();
    let mut colos = HashSet::new();
    for i in 0..10 {
        let started = Instant::now();
        let response = probe
            .get(api.base().join("v1/server").unwrap())
            .send()
            .await
            .expect("Worker RTT probe");
        assert!(
            response.status().is_success(),
            "Worker RTT probe HTTP {}",
            response.status()
        );
        versions.insert(format!("{:?}", response.version()));
        if let Some(ray) = response
            .headers()
            .get("cf-ray")
            .and_then(|v| v.to_str().ok())
            && let Some(colo) = ray
                .rsplit('-')
                .next()
                .filter(|colo| colo.len() == 3 && colo.bytes().all(|b| b.is_ascii_uppercase()))
        {
            colos.insert(colo.to_owned());
        }
        response.bytes().await.expect("complete RTT probe body");
        if i > 0 {
            rtts.push(started.elapsed().as_secs_f64() * 1000.0);
        }
    }
    let region = ["RUNNER_REGION", "AZURE_REGION"]
        .iter()
        .find_map(|key| std::env::var(key).ok())
        .filter(|s| {
            s.len() <= 80
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_ ".contains(&b))
        })
        .unwrap_or_else(|| "not exposed by runner environment".into());
    let mut versions = versions.into_iter().collect::<Vec<_>>();
    versions.sort();
    let mut colos = colos.into_iter().collect::<Vec<_>>();
    colos.sort();
    let sha = commit_sha();
    emit(&format!(
        "### Chunk upload benchmark\n\nCommit `{}`. {} MiB per measurement in 4 MiB chunks. Region: {}.\n\nWorker probe negotiated {}; cf-ray colo {}; median warm small-request RTT {:.2} ms, 9 requests after one warmup.\n\nPUT duration includes the Worker and R2 write; it does not isolate R2 latency.\n\n| Rep | N | upload() seconds | MB/s | Receipts | Observed retries |\n|---|---|---|---|---|---|",
        sha,
        config.mib,
        region,
        versions.join(", "),
        if colos.is_empty() {
            "unavailable".into()
        } else {
            colos.join(", ")
        },
        median(&mut rtts)
    ));
    let mut results: BTreeMap<usize, Vec<f64>> = BTreeMap::new();
    for (rep, n) in config.order() {
        let (chunks, source) = generated(config.mib);
        let space = api
            .create_space(
                &auth,
                &CreateSpaceRequest {
                    name: format!("Upload benchmark {rep} N={n}"),
                    request_id: String::new(),
                },
            )
            .await
            .unwrap();
        let (session, mut events) = Session::start(
            auth.clone(),
            SessionConfig::new(
                &space.space_id,
                "upload benchmark",
                env!("CARGO_PKG_VERSION"),
                1,
            ),
        )
        .unwrap();
        let history = connected(&mut events).await.history_id;
        let transfer = measurement_transfer(&api, n);
        let n = transfer.gate.concurrency();
        *retries.lock().unwrap() = Retries::default();
        uploading.store(true, Ordering::Relaxed);
        let started = Instant::now();
        let outcome = transfer
            .upload(&session, &chunks, |chunk| {
                std::future::ready(
                    source
                        .get(&chunk.hash)
                        .cloned()
                        .ok_or_else(|| std::io::Error::other("unknown benchmark chunk")),
                )
            })
            .await;
        let seconds = started.elapsed().as_secs_f64();
        uploading.store(false, Ordering::Relaxed);
        let observed = retries.lock().unwrap().clone();
        let receipts = match outcome {
            Ok(receipts) => receipts,
            Err(error) => {
                emit(&format!(
                    "| {rep} | {n} | {seconds:.3} | FAILED | 0 | {} |\n\nUpload failed: {error}",
                    observed.text()
                ));
                panic!("benchmark upload failed");
            }
        };
        assert_eq!(
            receipts.len(),
            chunks.len(),
            "the relay must acknowledge every fresh chunk, with no deduplication"
        );
        let mbps = config.mib as f64 * 1_048_576.0 / 1_000_000.0 / seconds;
        emit(&format!(
            "| {rep} | {n} | {seconds:.3} | {mbps:.2} | {} | {} |",
            receipts.len(),
            observed.text()
        ));
        let mut request = commit(
            &history,
            0,
            1,
            vec![Op {
                op: Some(op::Op::CreateFile(CreateFile {
                    made_over: Vec::new(),
                    source_version_id: String::new(),
                    node_id: uuidv7(),
                    parent_id: space.space_id,
                    name: "payload.bin".into(),
                    content: Some(content(&chunks)),
                })),
            }],
        );
        request.receipts = receipts;
        let accepted = session
            .commit(request)
            .await
            .expect("commit uploaded bytes outside timed region");
        assert!(
            accepted.results.len() == 1
                && accepted.results[0].code == op_result::Code::Applied as i32,
            "uploaded file must commit"
        );
        session.close().await;
        results.entry(n).or_default().push(mbps);
    }
    emit("\n| N | Measurements | Median MB/s | Min MB/s | Max MB/s |\n|---|---|---|---|---|");
    for (n, mut values) in results {
        let mid = median(&mut values);
        emit(&format!(
            "| {n} | {} | {mid:.2} | {:.2} | {:.2} |",
            values.len(),
            values[0],
            values[values.len() - 1]
        ));
    }
    let mut negotiated = protocols
        .lock()
        .unwrap()
        .iter()
        .copied()
        .collect::<Vec<_>>();
    negotiated.sort();
    assert!(
        !negotiated.is_empty(),
        "no upload connection handshake observed; check the pinned hyper-util trace messages"
    );
    emit(&format!(
        "\nUpload connections negotiated {}. Observed hyper-util handshake events only while upload() was running, without changing Transfer or its HTTP client configuration.",
        negotiated.join(", ")
    ));
    emit(
        "\nEach measurement uses a fresh space so earlier pending chunks and metadata cannot affect its plan. Generation, initial hashing, space creation, session setup and commit are outside the timer. Transfer's hash validation, PrepareUpload, PUTs and retry waits are inside. MB/s uses decimal megabytes. Retries count existing chunk-transfer debug events, grouped by status and distinct hash. Expired-ticket renewals and the final failed attempt are not emitted by Transfer and are not counted. These are not zero-error guarantees. This benchmark does not identify the throughput limiter or rank differences smaller than the run-to-run spread.",
    );
}

#[derive(Debug, PartialEq)]
struct SmallConfig {
    files: usize,
    kib: usize,
}

impl SmallConfig {
    fn parse(files: &str, kib: &str) -> Result<Self, &'static str> {
        let files = files
            .parse()
            .map_err(|_| "files must be an integer from 32 to 50000")?;
        if !(32..=50_000).contains(&files) {
            return Err("files must be an integer from 32 to 50000");
        }
        let kib = kib
            .parse()
            .map_err(|_| "KiB must be an integer from 1 to 255")?;
        if !(1..=255).contains(&kib) {
            return Err("KiB must be an integer from 1 to 255");
        }
        Ok(Self { files, kib })
    }
}

fn small_files(config: &SmallConfig) -> (Vec<ChunkRef>, HashMap<Vec<u8>, Bytes>) {
    let mut chunks = Vec::new();
    let mut source = HashMap::new();
    for _ in 0..config.files {
        let mut bytes = vec![0; config.kib << 10];
        fastrand::fill(&mut bytes);
        let chunk = ChunkRef {
            hash: Sha256::digest(&bytes).to_vec(),
            size: bytes.len() as u32,
            encoding: 0,
        };
        source.insert(chunk.hash.clone(), Bytes::from(bytes));
        chunks.push(chunk);
    }
    assert_eq!(chunks.len(), source.len(), "every chunk must be fresh");
    (chunks, source)
}

fn epoch_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

#[tokio::test]
#[ignore = "deploy-test bench-small mode, or a local Worker with KOIDRIVE_BENCH_SMALL_FILES=320"]
async fn small_file_benchmark() {
    let setting = |key: &str, default: &str| std::env::var(key).unwrap_or_else(|_| default.into());
    let config = SmallConfig::parse(
        &setting("KOIDRIVE_BENCH_SMALL_FILES", "20000"),
        &setting("KOIDRIVE_BENCH_SMALL_KIB", "16"),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let (api, auth) = deployment().await;
    let retries = Arc::new(Mutex::new(Retries::default()));
    let requests = Arc::new(Mutex::new(Requests::default()));
    tracing::subscriber::set_global_default(RetryCounter {
        retries: retries.clone(),
        protocols: Arc::default(),
        uploading: Arc::default(),
        requests: requests.clone(),
    })
    .expect("install request counter");
    emit(&format!(
        "### Small-file upload benchmark\n\nCommit `{}`. {} fresh files of {} KiB per side, one chunk each, default upload concurrency.\n\n| Side | upload() seconds | Files/s | Upload requests | Chunks per request | Receipts | Observed retries |\n|---|---|---|---|---|---|---|",
        commit_sha(),
        config.files,
        config.kib
    ));
    let mut windows = Vec::new();
    for (side, bundles) in [("bundles", true), ("puts", false)] {
        let (chunks, source) = small_files(&config);
        let space = api
            .create_space(
                &auth,
                &CreateSpaceRequest {
                    name: format!("Small-file benchmark {side}"),
                    request_id: String::new(),
                },
            )
            .await
            .unwrap();
        let (session, mut events) = Session::start(
            auth.clone(),
            SessionConfig::new(
                &space.space_id,
                "small-file benchmark",
                env!("CARGO_PKG_VERSION"),
                1,
            ),
        )
        .unwrap();
        let welcome = connected(&mut events).await;
        assert!(
            !bundles
                || welcome
                    .features
                    .iter()
                    .any(|f| f == koidrive_net::CHUNK_BUNDLES),
            "the deployed Worker does not offer chunk bundles"
        );
        let mut transfer = Transfer::new(&api);
        transfer.bundles = bundles;
        *retries.lock().unwrap() = Retries::default();
        *requests.lock().unwrap() = Requests::default();
        let start_ms = epoch_ms();
        let started = Instant::now();
        let outcome = transfer
            .upload(&session, &chunks, |chunk| {
                std::future::ready(
                    source
                        .get(&chunk.hash)
                        .cloned()
                        .ok_or_else(|| std::io::Error::other("unknown benchmark chunk")),
                )
            })
            .await;
        let seconds = started.elapsed().as_secs_f64();
        let end_ms = epoch_ms();
        let observed = retries.lock().unwrap().clone();
        let sent = *requests.lock().unwrap();
        session.close().await;
        let receipts = match outcome {
            Ok(receipts) => receipts,
            Err(error) => {
                emit(&format!(
                    "| {side} | {seconds:.3} | FAILED | {} | | 0 | {} |\n\nUpload failed: {error}",
                    sent.sent,
                    observed.text()
                ));
                panic!("small-file benchmark upload failed");
            }
        };
        assert_eq!(
            receipts.len(),
            chunks.len(),
            "every fresh chunk needs a receipt"
        );
        emit(&format!(
            "| {side} | {seconds:.3} | {:.0} | {} | {:.1} | {} | {} |",
            config.files as f64 / seconds,
            sent.sent,
            sent.chunks as f64 / sent.sent.max(1) as f64,
            receipts.len(),
            observed.text()
        ));
        windows.push(serde_json::json!({
            "side": side,
            "startMs": start_ms as u64,
            "endMs": end_ms as u64,
            "requests": sent.sent,
            "chunks": sent.chunks,
        }));
    }
    if let Ok(path) = std::env::var("KOIDRIVE_BENCH_WINDOWS") {
        std::fs::write(path, serde_json::to_string(&windows).unwrap())
            .expect("write benchmark windows");
    }
    emit(
        "\nEach side uses a fresh space and fresh random bytes. Generation, hashing, space creation and session setup are outside the timer. PrepareUpload, upload requests and retry waits are inside. Upload requests count every chunk PUT and bundle POST the client sent, retries included.",
    );
}

#[test]
fn small_benchmark_accepts_only_bounded_sizes_before_network() {
    for (files, kib) in [
        ("31", "16"),
        ("50001", "16"),
        ("x", "16"),
        ("320", "0"),
        ("320", "256"),
        ("320", "x"),
    ] {
        assert!(
            SmallConfig::parse(files, kib).is_err(),
            "{files} files of {kib} KiB"
        );
    }
    assert_eq!(
        SmallConfig::parse("20000", "16"),
        Ok(SmallConfig {
            files: 20000,
            kib: 16
        })
    );
}

#[test]
fn benchmark_accepts_only_supported_concurrency_before_network() {
    for ns in ["0", "3", "16", "1,1", "1,2,4,8,1", ""] {
        assert!(Config::parse(ns, "3", "1024").is_err());
    }
    for ns in ["1", "2", "4", "8", "1,2,4,8", "8,1"] {
        assert!(Config::parse(ns, "3", "1024").is_ok());
    }
}

#[test]
fn measurements_set_the_gate_and_report_its_concurrency() {
    let api = Api::local_development("http://127.0.0.1:9".parse().unwrap()).unwrap();
    for n in [1, 2, 4, 8] {
        let transfer = measurement_transfer(&api, n);
        assert_eq!(transfer.gate.concurrency(), n);
    }
}

#[test]
fn benchmark_bounds_memory_and_repetitions() {
    for reps in ["0", "4", "-1", "x"] {
        assert!(Config::parse("1", reps, "64").is_err());
    }
    for size in ["0", "5", "1028", "x"] {
        assert!(Config::parse("1", "3", size).is_err());
    }
    assert!(Config::parse("1", "3", "64").is_ok());
}

#[test]
fn repetitions_rotate_the_concurrency_order() {
    let config = Config {
        ns: vec![1, 2, 4, 8],
        repetitions: 3,
        mib: 1024,
    };
    assert_eq!(
        config.order(),
        vec![
            (1, 1),
            (1, 2),
            (1, 4),
            (1, 8),
            (2, 2),
            (2, 4),
            (2, 8),
            (2, 1),
            (3, 4),
            (3, 8),
            (3, 1),
            (3, 2)
        ]
    );
}

#[test]
fn retry_counter_groups_statuses_without_retaining_tickets() {
    let mut retries = Retries::default();
    let hash = "ab".repeat(32);
    retries.record(&format!(
        "the transfer of chunk {hash} was refused with HTTP 429"
    ));
    retries.record(&format!(
        "the transfer of chunk {hash} was refused with HTTP 503"
    ));
    retries.record(&format!(
        "the transfer of chunk {hash} kept failing: no complete answer within 120s"
    ));
    assert_eq!(
        retries.text(),
        "1 chunks; 3 retry events (429: 1, 503: 1, transport/timeout: 1)"
    );
}

#[test]
fn spread_uses_the_median_and_range() {
    let mut samples = [9.0, 1.0, 3.0];
    assert_eq!(median(&mut samples), 3.0);
    assert_eq!(samples, [1.0, 3.0, 9.0]);
    assert_eq!(median(&mut [4.0, 2.0]), 3.0);
}

#[test]
fn trace_observer_counts_retries_and_only_active_upload_handshakes() {
    let retries = Arc::new(Mutex::new(Retries::default()));
    let protocols = Arc::new(Mutex::new(HashSet::new()));
    let uploading = Arc::new(AtomicBool::new(false));
    let requests = Arc::new(Mutex::new(Requests::default()));
    let counter = RetryCounter {
        retries: retries.clone(),
        protocols: protocols.clone(),
        uploading: uploading.clone(),
        requests: requests.clone(),
    };
    tracing::subscriber::with_default(counter, || {
        tracing::trace!(target: "hyper_util::client::legacy::client", "http2 handshake complete, spawning background dispatcher task");
        assert!(protocols.lock().unwrap().is_empty());
        uploading.store(true, Ordering::Relaxed);
        tracing::trace!(target: "hyper_util::client::legacy::client", "http2 handshake complete, spawning background dispatcher task");
        let error = koidrive_net::ChunkError::Refused {
            hash: vec![0xab; 32],
            status: 429,
            error: None,
            cloudflare_error_code: None,
        };
        tracing::debug!(target: "koidrive_net::chunks", %error, "koidrive chunk transfer will be retried");
        tracing::trace!(target: "koidrive_net::chunks", chunks = 32, "koidrive chunk upload request");
        tracing::trace!(target: "koidrive_net::chunks", chunks = 1, "koidrive chunk upload request");
    });
    assert_eq!(
        *requests.lock().unwrap(),
        Requests {
            sent: 2,
            chunks: 33
        }
    );
    assert_eq!(
        retries.lock().unwrap().text(),
        "1 chunks; 1 retry events (429: 1)"
    );
    assert_eq!(*protocols.lock().unwrap(), HashSet::from(["HTTP/2"]));
}
