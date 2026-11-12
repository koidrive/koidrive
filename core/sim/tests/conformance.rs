// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeSet, HashMap};

use koidrive_engine::ids::IdGen;
use koidrive_engine::names::MAX_NAME_BYTES;
use koidrive_proto::v1::{
    ChangeRecord, ChunkRef, Commit, CommitResult, Content, CreateDir, CreateFile, Delete, Error,
    ListVersions, Move, NodeKind, NodeRevisions, NodeState, NodeStatus, Op, OpResult, Place,
    PrepareUpload, Restore, RpcRequest, UpdateContent, UploadReceipt, VersionInfo, change_record,
    chunk_plan, error, op, op_result, rpc_request, rpc_response,
};
use koidrive_sim::space::{Role, Session, SpaceDo, node_states};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

const FILE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../spec/testdata/scenarios/space-conformance-v1.json"
);

const NUMBERS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../spec/testdata/scenarios/runner-numbers-v1.json"
);

const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

const MAX_REPEAT: u64 = 65_536;

const SEQUENCES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../spec/testdata/scenarios/space-commit-sequences-v1.json"
);

type Outcome = Result<(), String>;

fn main() {
    let doc = load(FILE);
    let generated = sequences(&doc).unwrap_or_else(|e| {
        println!("generating {SEQUENCES} ... FAILED: {e}");
        std::process::exit(1);
    });
    if std::env::args().any(|arg| arg == "--write") {
        std::fs::write(SEQUENCES, render(&generated)).expect("write the commit sequences");
    }
    let sequences = load(SEQUENCES);
    let mut failed = run_file(FILE, &doc) + run_file(SEQUENCES, &sequences);
    let check = format!("{} is what the reference returns", file_name(SEQUENCES));
    if sequences == generated {
        println!("{check} ... ok");
    } else {
        failed += 1;
        println!(
            "{check} ... FAILED: regenerate it with cargo test -p koidrive-sim --test conformance -- --write"
        );
    }
    let self_tests: Vec<(String, Outcome)> = self_tests(&doc)
        .into_iter()
        .map(|(name, outcome)| (name.to_owned(), outcome))
        .chain(number_tests(&doc))
        .collect();
    for (name, outcome) in &self_tests {
        match outcome {
            Ok(()) => println!("self-test {name} ... ok"),
            Err(e) => {
                failed += 1;
                println!("self-test {name} ... FAILED: {e}");
            }
        }
    }
    println!("runner self-tests: {} run", self_tests.len());
    if failed > 0 {
        std::process::exit(1);
    }
}

fn load(path: &str) -> Value {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    parse_doc(&text).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn parse_doc(text: &str) -> Result<Value, String> {
    plain_integers(text)?;
    serde_json::from_str(text).map_err(|e| e.to_string())
}

fn plain_integers(text: &str) -> Outcome {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
            }
            b'-' | b'+' | b'0'..=b'9' => {
                let start = i;
                while i < bytes.len() && b"+-.eE0123456789".contains(&bytes[i]) {
                    i += 1;
                }
                let token = &text[start..i];
                let digits = token.strip_prefix('-').unwrap_or(token);
                let plain = !digits.is_empty()
                    && digits.bytes().all(|b| b.is_ascii_digit())
                    && (digits == "0" || !digits.starts_with('0'))
                    && token != "-0";
                if !plain {
                    return Err(format!(
                        "numbers must be written as plain integers, got {token}"
                    ));
                }
                if digits.parse::<u64>().map_or(true, |n| n > MAX_SAFE_INTEGER) {
                    return Err(format!(
                        "numbers must be integers from -{MAX_SAFE_INTEGER} to {MAX_SAFE_INTEGER}, got {token}"
                    ));
                }
            }
            _ => i += 1,
        }
    }
    Ok(())
}

fn number_tests(doc: &Value) -> Vec<(String, Outcome)> {
    let fixture = std::fs::read_to_string(FILE)
        .expect("read the conformance file")
        .replace("\"repeat\": 1024\n", "\"repeat\": 1024.0\n");
    let float_in_fixture = match parse_doc(&fixture) {
        Err(e) if e.contains("numbers must be written as plain integers, got 1024.0") => Ok(()),
        other => Err(format!("got {other:?}")),
    };
    let mut tests = vec![(
        "the-fixture-with-repeat-1024.0-fails".to_owned(),
        float_in_fixture,
    )];
    tests.extend(cases(doc));
    tests
}

fn cases(doc: &Value) -> Vec<(String, Outcome)> {
    load(NUMBERS)["cases"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|case| {
            let token = case["repeat"].as_str().unwrap_or_default();
            let text = format!(
                r#"{{"name": "repeat", "devices": {{"a": {{"name": "A"}}}}, "contents": {{"c": {{"chunks": [{{"text": "x", "repeat": {token}}}]}}}}, "steps": []}}"#
            );
            let got = parse_doc(&text).and_then(|scenario| run(doc, &scenario));
            let outcome = match (got, case["error"].as_str()) {
                (Ok(()), None) => Ok(()),
                (Err(e), Some(want)) if e.contains(want) => Ok(()),
                (Ok(()), Some(want)) => Err(format!("passed, expected {want}")),
                (Err(e), _) => Err(format!("failed with {e}")),
            };
            (format!("repeat-{token}"), outcome)
        })
        .collect()
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn run_file(path: &str, doc: &Value) -> usize {
    let path = file_name(path);
    if let Err(e) = check_doc(doc) {
        println!("{path} ... FAILED: {e}");
        return 1;
    }
    let scenarios = doc["scenarios"].as_array().expect("checked above");
    let mut failed = 0;
    for scenario in scenarios {
        let name = scenario["name"].as_str().unwrap_or("?");
        let tag = if scenario.get("reference_only").is_some() {
            " (reference only)"
        } else {
            ""
        };
        match run(doc, scenario) {
            Ok(()) => println!("scenario {name}{tag} ... ok"),
            Err(e) => {
                failed += 1;
                println!("scenario {name}{tag} ... FAILED: {e}");
            }
        }
    }
    println!(
        "{path}: {} passed; {failed} failed",
        scenarios.len() - failed
    );
    failed
}

const SEEDS: u64 = 16;
const COMMITS: usize = 24;
const NAMES: &[&str] = &[
    "a",
    "A",
    "\u{df}",
    "ss",
    "SS",
    "Stra\u{df}e",
    "strasse",
    "x.txt",
    "X.TXT",
    "Cafe\u{301}",
    "caf\u{e9}",
    ".hidden",
];

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        ((z ^ (z >> 31)) % n as u64) as usize
    }
}

fn sequences(hand: &Value) -> Result<Value, String> {
    let mut doc = json!({
        "format": "koidrive.space-conformance",
        "version": 1,
        "spec": hand["spec"],
        "unicode": hand["unicode"],
        "now_ms": hand["now_ms"],
        "mtime_ns": hand["mtime_ns"],
        "conventions": [
            "The conventions of space-conformance-v1.json apply, except that no expected value here is written by hand.",
            "Each scenario is a sequence of commits from three devices whose base_seq trails the head by up to three, with ops drawn from a fixed seed. Expected values are what the reference server in core/sim returned, and cargo test -p koidrive-sim --test conformance -- --write regenerates this file. The server must return the same.",
        ],
    });
    let scenarios = (1..=SEEDS)
        .map(|seed| sequence(&doc, seed))
        .collect::<Result<Vec<_>, _>>()?;
    doc["scenarios"] = scenarios.into();
    Ok(doc)
}

fn sequence(doc: &Value, seed: u64) -> Result<Value, String> {
    let contents: Map<String, Value> = (1..=4)
        .map(|i| {
            let chunks = json!({ "chunks": [{ "text": format!("version {i}") }] });
            (format!("c{i}"), chunks)
        })
        .collect();
    let mut scenario = json!({
        "name": format!("commit-sequence-{seed:02}"),
        "devices": { "a": { "name": "Laptop" }, "b": { "name": "Desk" }, "c": { "name": "Phone" } },
        "contents": contents,
    });
    let mut run = Run::new(doc, &scenario)?;
    let mut rng = Rng(seed);
    let mut nodes = Vec::new();
    let mut steps = Vec::new();
    for _ in 0..COMMITS {
        let head = run.space("main")?.head_seq();
        let mut pool = Pool::of(&run, &nodes)?;
        let ops: Vec<Value> = (0..[1, 1, 1, 2, 3][rng.below(5)])
            .map(|_| random_op(&mut rng, &mut pool, &mut nodes))
            .collect();
        let device = ["a", "b", "c"][rng.below(3)];
        let base_seq = head.saturating_sub(rng.below(4) as u64);
        let mut step = json!({ "device": device, "commit": { "base_seq": base_seq, "ops": ops } });
        let result = run.send("main", run.now_ms, &step)?;
        let (expect, changed) = run.recorded("main", &result)?;
        step["expect"] = expect;
        steps.push(step);
        for symbol in changed {
            if symbol != "root" && !nodes.contains(&symbol) {
                nodes.push(symbol);
            }
        }
        steps.push(json!({ "head": run.space("main")?.head_seq() }));
    }
    let root = run.space("main")?.space_id().to_owned();
    let mut live = Vec::new();
    for node in run.space("main")?.nodes() {
        if node.status == NodeStatus::Live as i32 && node.node_id != root {
            let symbol = run
                .values
                .get(&node.node_id)
                .ok_or_else(|| format!("live node {} is named by no op or result", node.node_id))?;
            live.push(symbol.clone());
        }
    }
    live.sort();
    let entries = nodes
        .iter()
        .map(|symbol| run.recorded_node("main", symbol))
        .collect::<Result<Vec<_>, _>>()?;
    steps.push(json!({ "live": live, "nodes": entries }));
    scenario["steps"] = steps.into();
    Ok(scenario)
}

struct Pool {
    dirs: Vec<String>,
    files: Vec<String>,
    trashed: Vec<String>,
    all: Vec<String>,
}

impl Pool {
    fn of(run: &Run, nodes: &[String]) -> Result<Self, String> {
        let mut pool = Pool {
            dirs: vec!["root".into()],
            files: Vec::new(),
            trashed: Vec::new(),
            all: nodes.to_vec(),
        };
        for symbol in nodes {
            let Some(node) = run.space("main")?.node(&run.known("main", symbol)?) else {
                continue;
            };
            let list = match (
                NodeStatus::try_from(node.status),
                NodeKind::try_from(node.kind),
            ) {
                (Ok(NodeStatus::Live), Ok(NodeKind::Dir)) => &mut pool.dirs,
                (Ok(NodeStatus::Live), _) => &mut pool.files,
                _ => &mut pool.trashed,
            };
            list.push(symbol.clone());
        }
        Ok(pool)
    }

    fn pick(&self, rng: &mut Rng, lists: &[&Vec<String>]) -> String {
        let candidates: Vec<&String> = lists.iter().copied().flatten().collect();
        if candidates.is_empty() || rng.below(10) == 0 {
            let all: Vec<&String> = self.all.iter().chain(&self.dirs[..1]).collect();
            return all[rng.below(all.len())].clone();
        }
        candidates[rng.below(candidates.len())].clone()
    }
}

fn random_op(rng: &mut Rng, pool: &mut Pool, nodes: &mut Vec<String>) -> Value {
    let short = NAMES[rng.below(NAMES.len())];
    let name = if rng.below(10) == 0 {
        "n".repeat(MAX_NAME_BYTES - 1) + short
    } else {
        short.to_owned()
    };
    let content = format!("c{}", 1 + rng.below(4));
    let parent = pool.pick(rng, &[&pool.dirs]);
    let mut fresh = |list: fn(&mut Pool) -> &mut Vec<String>, pool: &mut Pool| {
        let symbol = format!("n{}", nodes.len() + 1);
        nodes.push(symbol.clone());
        pool.all.push(symbol.clone());
        list(pool).push(symbol.clone());
        symbol
    };
    match rng.below(20) {
        0..=3 => {
            let node = fresh(|pool| &mut pool.dirs, pool);
            json!({ "create_dir": { "node": node, "parent": parent, "name": name } })
        }
        4..=7 => {
            let node = fresh(|pool| &mut pool.files, pool);
            json!({ "create_file": { "node": node, "parent": parent, "name": name, "content": content } })
        }
        8..=10 => {
            let node = pool.pick(rng, &[&pool.files, &pool.files, &pool.trashed]);
            json!({ "update_content": { "node": node, "content": content } })
        }
        11..=14 => {
            let node = pool.pick(rng, &[&pool.files, &pool.dirs[1..].to_vec()]);
            json!({ "move": { "node": node, "parent": parent, "name": name } })
        }
        15..=17 => {
            let node = pool.pick(rng, &[&pool.files, &pool.dirs[1..].to_vec()]);
            json!({ "delete": { "node": node } })
        }
        _ => {
            let node = pool.pick(rng, &[&pool.trashed]);
            if rng.below(2) == 0 {
                json!({ "restore": { "node": node, "parent": parent, "name": name } })
            } else {
                json!({ "restore": { "node": node } })
            }
        }
    }
}

fn render(doc: &Value) -> String {
    let line = |value: &Value| serde_json::to_string(value).expect("a JSON value");
    let list = |values: &Value, indent: &str, item: &dyn Fn(&Value) -> String| {
        let items: Vec<String> = values
            .as_array()
            .into_iter()
            .flatten()
            .map(|value| format!("{indent}{}", item(value)))
            .collect();
        items.join(",\n")
    };
    let mut out = String::from("{\n");
    for key in ["format", "version", "spec", "unicode", "now_ms", "mtime_ns"] {
        out += &format!("  \"{key}\": {},\n", line(&doc[key]));
    }
    out += &format!(
        "  \"conventions\": [\n{}\n  ],\n",
        list(&doc["conventions"], "    ", &line)
    );
    let scenario = |scenario: &Value| {
        let mut out = String::from("{\n");
        for key in ["name", "devices", "contents"] {
            out += &format!("      \"{key}\": {},\n", line(&scenario[key]));
        }
        out += &format!(
            "      \"steps\": [\n{}\n      ]\n    }}",
            list(&scenario["steps"], "        ", &line)
        );
        out
    };
    out += &format!(
        "  \"scenarios\": [\n{}\n  ]\n}}\n",
        list(&doc["scenarios"], "    ", &scenario)
    );
    out
}

const LONG_MISSING_LIST: &str = "a-long-missing-chunk-list-is-capped-and-the-resent-commit-applies";

fn self_tests(doc: &Value) -> Vec<(&'static str, Outcome)> {
    let base = &doc["scenarios"][0];
    let long = doc["scenarios"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|s| s["name"] == LONG_MISSING_LIST)
        .unwrap_or(&Value::Null);
    let mutate = |scenario: &Value, edit: &dyn Fn(&mut Value), reason: &str| -> Outcome {
        let mut scenario = scenario.clone();
        edit(&mut scenario);
        match run(doc, &scenario) {
            Ok(()) => Err("the mutated scenario passed".into()),
            Err(e) if e.contains(reason) => Ok(()),
            Err(e) => Err(format!("failed for another reason: {e}")),
        }
    };
    vec![
        (
            "a-literal-id-in-an-expectation-is-compared",
            mutate(
                base,
                &|s| s["steps"][2]["nodes"][0]["parent"] = "=not-the-root".into(),
                "step 3: f1.parent: expected not-the-root",
            ),
        ),
        (
            "an-assertion-of-the-wrong-type-fails",
            mutate(
                base,
                &|s| s["steps"][0]["expect"]["seq"] = "999".into(),
                "step 1: seq must be a non-negative integer",
            ),
        ),
        (
            "an-unknown-assertion-key-fails",
            mutate(
                base,
                &|s| {
                    let expect = s["steps"][0]["expect"].as_object_mut().unwrap();
                    let seq = expect.remove("seq").unwrap();
                    expect.insert("sqe".into(), seq);
                },
                "step 1: unknown field sqe in expect",
            ),
        ),
        (
            "a-field-for-the-other-branch-fails",
            mutate(
                base,
                &|s| s["steps"][0]["expect"]["missing"] = 123.into(),
                "step 1: unknown field missing in expect",
            ),
        ),
        (
            "a-wrong-numeric-assertion-fails",
            mutate(
                base,
                &|s| s["steps"][0]["expect"]["seq"] = 999.into(),
                "step 1: seq 1, expected 999",
            ),
        ),
        (
            "a-total-that-is-only-the-listed-count-fails",
            mutate(
                long,
                &|s| s["steps"][0]["expect"]["missing_some"]["total"] = 1000.into(),
                "does not give the total 1000 first",
            ),
        ),
        (
            "an-unknown-scenario-field-fails",
            mutate(
                base,
                &|s| s["covres"] = json!(["R1"]),
                "unknown field covres in a scenario",
            ),
        ),
        (
            "an-unknown-step-field-fails",
            mutate(
                base,
                &|s| s["steps"][0]["devcie"] = "a".into(),
                "step 1: unknown field devcie in a step",
            ),
        ),
        (
            "an-unknown-commit-field-fails",
            mutate(
                base,
                &|s| s["steps"][0]["commit"]["base"] = 0.into(),
                "step 1: unknown field base in commit",
            ),
        ),
        (
            "an-unknown-op-field-fails",
            mutate(
                base,
                &|s| s["steps"][0]["commit"]["ops"][0]["create_file"]["mode"] = "x".into(),
                "step 1: unknown field mode in create_file",
            ),
        ),
        (
            "an-unknown-result-field-fails",
            mutate(
                base,
                &|s| s["steps"][0]["expect"]["results"][0]["nmae"] = "x".into(),
                "step 1: result 1: unknown field nmae in a result",
            ),
        ),
        (
            "an-unknown-node-field-fails",
            mutate(
                base,
                &|s| s["steps"][2]["nodes"][0]["stauts"] = "LIVE".into(),
                "step 3: unknown field stauts in a node",
            ),
        ),
        ("the-unmutated-scenario-passes", run(doc, base)),
    ]
}

fn run(doc: &Value, scenario: &Value) -> Outcome {
    Run::new(doc, scenario)?.steps(scenario)
}

fn object<'a>(value: &'a Value, what: &str) -> Result<&'a Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| format!("{what} must be an object, got {value}"))
}

fn keys(value: &Value, what: &str, allowed: &[&str]) -> Outcome {
    for key in object(value, what)?.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(format!("unknown field {key} in {what}"));
        }
    }
    Ok(())
}

fn opt<'a, T>(
    value: &'a Value,
    field: &str,
    kind: &str,
    read: impl Fn(&'a Value) -> Option<T>,
) -> Result<Option<T>, String> {
    match value.get(field) {
        None => Ok(None),
        Some(v) => read(v)
            .map(Some)
            .ok_or_else(|| format!("{field} must be {kind}, got {v}")),
    }
}

fn opt_str<'a>(value: &'a Value, field: &str) -> Result<Option<&'a str>, String> {
    opt(value, field, "a string", Value::as_str)
}

fn opt_u64(value: &Value, field: &str) -> Result<Option<u64>, String> {
    opt(value, field, "a non-negative integer", |v| {
        v.as_u64().filter(|n| *n <= MAX_SAFE_INTEGER)
    })
}

fn opt_i64(value: &Value, field: &str) -> Result<Option<i64>, String> {
    opt(value, field, "an integer", |v| {
        v.as_i64().filter(|n| n.unsigned_abs() <= MAX_SAFE_INTEGER)
    })
}

fn opt_u32(value: &Value, field: &str) -> Result<Option<u32>, String> {
    opt(value, field, "an integer from 0 to 4294967295", |v| {
        v.as_u64().and_then(|n| u32::try_from(n).ok())
    })
}

fn opt_bool(value: &Value, field: &str) -> Result<Option<bool>, String> {
    opt(value, field, "a boolean", Value::as_bool)
}

fn opt_list<'a>(value: &'a Value, field: &str) -> Result<Option<&'a Vec<Value>>, String> {
    opt(value, field, "a list", Value::as_array)
}

fn req<T>(found: Result<Option<T>, String>, field: &str) -> Result<T, String> {
    found?.ok_or_else(|| format!("missing field {field}"))
}

fn every(value: &Value, fields: &[&str]) -> Outcome {
    match fields.iter().find(|field| value.get(**field).is_none()) {
        Some(field) => Err(format!("missing field {field}")),
        None => Ok(()),
    }
}

const NODE_FIELDS: &[&str] = &[
    "node",
    "parent",
    "name",
    "kind",
    "status",
    "content_rev",
    "meta_rev",
    "trash_group",
    "current",
    "pending_for",
    "pending_by",
];

const CURRENT_FIELDS: &[&str] = &[
    "version",
    "seq",
    "content",
    "chunks_inline",
    "device",
    "user",
    "created_at_ms",
];

fn strings<'a>(list: &'a [Value], what: &str) -> Result<Vec<&'a str>, String> {
    list.iter()
        .map(|v| {
            v.as_str()
                .ok_or_else(|| format!("{what} must list strings, got {v}"))
        })
        .collect()
}

fn check_doc(doc: &Value) -> Outcome {
    keys(
        doc,
        "the file",
        &[
            "format",
            "version",
            "spec",
            "unicode",
            "now_ms",
            "mtime_ns",
            "conventions",
            "scenarios",
        ],
    )?;
    if doc["format"] != "koidrive.space-conformance" || doc["version"] != 1 {
        return Err("unsupported format or version".into());
    }
    req(opt_list(doc, "scenarios"), "scenarios")?;
    Ok(())
}

fn expectation<'a>(
    step: &'a Value,
    on_error: &[&str],
    on_success: &[&str],
) -> Result<&'a Value, String> {
    let expect = step
        .get("expect")
        .ok_or("a call step needs expect, even if empty")?;
    if opt_str(expect, "error")?.is_some() {
        keys(expect, "an error expect", &[&["error"], on_error].concat())
    } else {
        keys(expect, "expect", on_success)
    }?;
    Ok(expect)
}

fn chunk_list(chunks: &[Value]) -> Result<Vec<ChunkRef>, String> {
    let mut out = Vec::new();
    for chunk in chunks {
        keys(
            chunk,
            "a chunk",
            &["text", "counter_le32", "size", "repeat"],
        )?;
        let pieces: Vec<Vec<u8>> = match (opt_str(chunk, "text")?, opt_u32(chunk, "counter_le32")?)
        {
            (Some(text), None) => vec![text.as_bytes().to_vec()],
            (None, Some(n)) => (0..n).map(|i| i.to_le_bytes().to_vec()).collect(),
            _ => {
                return Err(format!(
                    "a chunk needs exactly one of text and counter_le32: {chunk}"
                ));
            }
        };
        let size = opt_u32(chunk, "size")?;
        let listed: Vec<ChunkRef> = pieces
            .iter()
            .map(|bytes| ChunkRef {
                hash: Sha256::digest(bytes).to_vec(),
                size: size.unwrap_or(bytes.len() as u32),
                encoding: 0,
            })
            .collect();
        let repeat = opt(
            chunk,
            "repeat",
            &format!("an integer from 1 to {MAX_REPEAT}"),
            |v| v.as_u64().filter(|n| (1..=MAX_REPEAT).contains(n)),
        )?;
        for _ in 0..repeat.unwrap_or(1) {
            out.extend(listed.iter().cloned());
        }
    }
    Ok(out)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn error_name(code: i32) -> String {
    error::Code::try_from(code)
        .map(|c| c.as_str_name().trim_start_matches("CODE_").to_owned())
        .unwrap_or_else(|_| format!("{code}"))
}

fn result_code(code: i32) -> &'static str {
    op_result::Code::try_from(code)
        .map(|c| c.as_str_name().trim_start_matches("CODE_"))
        .unwrap_or("?")
}

fn kind_name(kind: i32) -> &'static str {
    NodeKind::try_from(kind)
        .map(|k| k.as_str_name().trim_start_matches("NODE_KIND_"))
        .unwrap_or("?")
}

fn status_name(status: i32) -> &'static str {
    NodeStatus::try_from(status)
        .map(|s| s.as_str_name().trim_start_matches("NODE_STATUS_"))
        .unwrap_or("?")
}

fn check_error(error: &Error, code: &str) -> Outcome {
    let got = error_name(error.code);
    if got == code {
        Ok(())
    } else {
        Err(format!("error {got} ({}), expected {code}", error.message))
    }
}

struct Run {
    now_ms: i64,
    mtime_ns: i64,
    spaces: HashMap<String, SpaceDo>,
    devices: HashMap<String, Session>,
    contents: HashMap<String, Content>,
    wallets: HashMap<(String, String), Vec<UploadReceipt>>,
    slots: HashMap<(String, String), u64>,
    sent_slots: HashMap<(String, String, String), u64>,
    top_seq: HashMap<String, u64>,
    last_missing: HashMap<(String, String), Vec<Vec<u8>>>,
    symbols: HashMap<String, String>,
    values: HashMap<String, String>,
    cursors: HashMap<String, String>,
    ids: IdGen,
    req_id: u64,
}

impl Run {
    fn new(doc: &Value, scenario: &Value) -> Result<Self, String> {
        keys(
            scenario,
            "a scenario",
            &[
                "name",
                "covers",
                "reference_only",
                "devices",
                "spaces",
                "contents",
                "steps",
            ],
        )?;
        req(opt_str(scenario, "name"), "name")?;
        opt_str(scenario, "reference_only")?;
        opt_list(scenario, "covers")?;
        let now_ms = req(opt_i64(doc, "now_ms"), "now_ms")?;
        let mtime_ns: i64 = req(opt_str(doc, "mtime_ns"), "mtime_ns")?
            .parse()
            .map_err(|_| "mtime_ns must be an int64 written as a string")?;
        let mut spaces = HashMap::new();
        let names = match opt_list(scenario, "spaces")? {
            Some(list) => strings(list, "spaces")?,
            None => vec!["main"],
        };
        for (i, name) in names.into_iter().enumerate() {
            spaces.insert(name.to_owned(), SpaceDo::new(1 + i as u64, now_ms));
        }
        let mut devices = HashMap::new();
        for (symbol, device) in object(&scenario["devices"], "devices")? {
            keys(device, "a device", &["name", "role", "max_format"])?;
            let role = match opt_str(device, "role")?.unwrap_or("owner") {
                "owner" => Role::Owner,
                "editor" => Role::Editor,
                "viewer" => Role::Viewer,
                other => return Err(format!("unknown role {other}")),
            };
            devices.insert(
                symbol.clone(),
                Session {
                    device_id: format!("device-{symbol}"),
                    user_id: "user".into(),
                    device_name: req(opt_str(device, "name"), "name")?.to_owned(),
                    role,
                    max_format: opt_u32(device, "max_format")?.unwrap_or(1),
                },
            );
        }
        let mut contents = HashMap::new();
        if let Some(map) = scenario.get("contents") {
            for (symbol, def) in object(map, "contents")? {
                keys(def, "a content", &["chunks", "size"])?;
                let chunks = chunk_list(req(opt_list(def, "chunks"), "chunks")?)?;
                let manifest: Vec<u8> = chunks
                    .iter()
                    .flat_map(|c| c.hash.iter().copied().chain(c.size.to_le_bytes()))
                    .collect();
                let size = match opt_u64(def, "size")? {
                    Some(size) => size,
                    None => chunks.iter().map(|c| u64::from(c.size)).sum(),
                };
                contents.insert(
                    symbol.clone(),
                    Content {
                        size,
                        mtime_ns,
                        executable: false,
                        chunk_count: chunks.len() as u32,
                        manifest_digest: Sha256::digest(manifest).to_vec(),
                        chunker: "fastcdc2020-1m-4m-16m".into(),
                        chunks,
                    },
                );
            }
        }
        Ok(Self {
            now_ms,
            mtime_ns,
            spaces,
            devices,
            contents,
            wallets: HashMap::new(),
            slots: HashMap::new(),
            sent_slots: HashMap::new(),
            top_seq: HashMap::new(),
            last_missing: HashMap::new(),
            symbols: HashMap::new(),
            values: HashMap::new(),
            cursors: HashMap::new(),
            ids: IdGen::new(7),
            req_id: 0,
        })
    }

    fn bind(&mut self, symbol: &str, value: &str) -> Outcome {
        if let Some(bound) = self.symbols.get(symbol) {
            return if bound == value {
                Ok(())
            } else {
                Err(format!("{symbol} is {bound}, got {value}"))
            };
        }
        if let Some(other) = self.values.get(value) {
            return Err(format!("{symbol} got {value}, which is already {other}"));
        }
        self.symbols.insert(symbol.to_owned(), value.to_owned());
        self.values.insert(value.to_owned(), symbol.to_owned());
        Ok(())
    }

    fn fixed(&self, space: &str, symbol: &str) -> Result<Option<String>, String> {
        if symbol == "root" {
            return Ok(Some(self.space(space)?.space_id().to_owned()));
        }
        Ok(symbol.strip_prefix('=').map(str::to_owned))
    }

    fn id(&mut self, space: &str, symbol: &str) -> Result<String, String> {
        if let Some(value) = self.fixed(space, symbol)? {
            return Ok(value);
        }
        if let Some(value) = self.symbols.get(symbol) {
            return Ok(value.clone());
        }
        let value = self.ids.uuid_v7(self.now_ms as u64);
        self.bind(symbol, &value)?;
        Ok(value)
    }

    fn known(&self, space: &str, symbol: &str) -> Result<String, String> {
        if let Some(value) = self.fixed(space, symbol)? {
            return Ok(value);
        }
        self.symbols
            .get(symbol)
            .cloned()
            .ok_or_else(|| format!("unbound symbol {symbol}"))
    }

    fn expect_id(&mut self, space: &str, symbol: &str, value: &str) -> Outcome {
        match self.fixed(space, symbol)? {
            Some(fixed) if fixed == value => Ok(()),
            Some(fixed) => Err(format!("expected {fixed}, got {value}")),
            None => self.bind(symbol, value),
        }
    }

    fn known_set(
        &self,
        space: &str,
        list: &[Value],
        what: &str,
    ) -> Result<BTreeSet<String>, String> {
        strings(list, what)?
            .into_iter()
            .map(|symbol| self.known(space, symbol))
            .collect()
    }

    fn space(&self, name: &str) -> Result<&SpaceDo, String> {
        self.spaces
            .get(name)
            .ok_or_else(|| format!("unknown space {name}"))
    }

    fn content(&self, symbol: &str) -> Result<Content, String> {
        self.contents
            .get(symbol)
            .cloned()
            .ok_or_else(|| format!("unknown content {symbol}"))
    }

    fn contents_of(&self, list: &[Value]) -> Result<Vec<ChunkRef>, String> {
        let mut chunks = Vec::new();
        for symbol in strings(list, "contents")? {
            chunks.extend(self.content(symbol)?.chunks);
        }
        Ok(chunks)
    }

    fn session(&self, step: &Value) -> Result<Session, String> {
        let device = req(opt_str(step, "device"), "device")?;
        self.devices
            .get(device)
            .cloned()
            .ok_or_else(|| format!("unknown device {device}"))
    }

    fn rpc(
        &mut self,
        space: &str,
        session: &Session,
        at_ms: i64,
        call: rpc_request::Call,
    ) -> Result<rpc_response::Result, String> {
        self.req_id += 1;
        let request = RpcRequest {
            req_id: self.req_id,
            call: Some(call),
        };
        let space = self
            .spaces
            .get_mut(space)
            .ok_or_else(|| format!("unknown space {space}"))?;
        let (response, _) = space.rpc(session, at_ms, &request);
        response.result.ok_or_else(|| "empty response".to_owned())
    }

    fn steps(&mut self, scenario: &Value) -> Outcome {
        for (i, step) in req(opt_list(scenario, "steps"), "steps")?
            .iter()
            .enumerate()
        {
            self.step(step)
                .map_err(|e| format!("step {}: {e}", i + 1))?;
        }
        Ok(())
    }

    fn step(&mut self, step: &Value) -> Outcome {
        let space = opt_str(step, "space")?.unwrap_or("main").to_owned();
        let at_ms = opt_i64(step, "at_ms")?.unwrap_or(self.now_ms);
        opt_str(step, "reference_only")?;
        const CALL: &[&str] = &["device", "space", "at_ms", "reference_only", "expect"];
        let with = |extra: &'static str| [CALL, &[extra]].concat();
        if step.get("commit").is_some() {
            keys(step, "a step", &with("commit"))?;
            return self.commit(&space, at_ms, step);
        }
        if let Some(upload) = step.get("upload") {
            keys(
                step,
                "a step",
                &["device", "space", "at_ms", "reference_only", "upload"],
            )?;
            keys(upload, "upload", &["contents", "last_missing"])?;
            let session = self.session(step)?;
            let chunks = match (
                opt_list(upload, "contents")?,
                opt_bool(upload, "last_missing")?,
            ) {
                (Some(list), None) => self.contents_of(list)?,
                (None, Some(true)) => {
                    let listed = self
                        .last_missing
                        .get(&(session.device_id.clone(), space.clone()))
                        .ok_or("no MISSING_CHUNKS error was listed yet")?;
                    let known: HashMap<&[u8], &ChunkRef> = self
                        .contents
                        .values()
                        .flat_map(|content| &content.chunks)
                        .map(|chunk| (chunk.hash.as_slice(), chunk))
                        .collect();
                    listed
                        .iter()
                        .map(|hash| {
                            known
                                .get(hash.as_slice())
                                .map(|chunk| (*chunk).clone())
                                .ok_or_else(|| {
                                    format!("listed hash {} is no known chunk", hex(hash))
                                })
                        })
                        .collect::<Result<_, _>>()?
                }
                _ => {
                    return Err(
                        "upload needs exactly one of contents and last_missing: true".into(),
                    );
                }
            };
            let receipts = self.upload(&space, &session, at_ms, chunks)?;
            self.wallets
                .entry((session.device_id.clone(), space))
                .or_default()
                .extend(receipts);
            return Ok(());
        }
        if step.get("prepare_upload").is_some() {
            keys(step, "a step", &with("prepare_upload"))?;
            return self.prepare_upload(&space, at_ms, step);
        }
        if step.get("list_versions").is_some() {
            keys(step, "a step", &with("list_versions"))?;
            return self.list_versions(&space, at_ms, step);
        }
        keys(
            step,
            "a step",
            &["space", "reference_only", "head", "nodes", "live"],
        )?;
        self.check_state(&space, step)
    }

    fn upload(
        &mut self,
        space: &str,
        session: &Session,
        at_ms: i64,
        chunks: Vec<ChunkRef>,
    ) -> Result<Vec<UploadReceipt>, String> {
        let mut distinct = Vec::new();
        let mut seen = BTreeSet::new();
        for chunk in chunks {
            if seen.insert((chunk.hash.clone(), chunk.size)) {
                distinct.push(chunk);
            }
        }
        let mut receipts = Vec::new();
        for part in distinct.chunks(1000) {
            let result = self.rpc(
                space,
                session,
                at_ms,
                rpc_request::Call::PrepareUpload(PrepareUpload {
                    chunks: part.to_vec(),
                }),
            )?;
            let rpc_response::Result::PrepareUpload(plan) = result else {
                return Err(format!("PrepareUpload failed: {result:?}"));
            };
            let space = self.spaces.get_mut(space).ok_or("space")?;
            for (planned, chunk) in plan.chunks.iter().zip(part) {
                if planned.action == chunk_plan::Action::Upload as i32 {
                    receipts.push(space.put_chunk(&chunk.hash, chunk.size, at_ms));
                }
            }
        }
        Ok(receipts)
    }

    fn prepare_upload(&mut self, space: &str, at_ms: i64, step: &Value) -> Outcome {
        let session = self.session(step)?;
        let body = &step["prepare_upload"];
        keys(body, "prepare_upload", &["chunks", "contents"])?;
        let chunks = match (opt_list(body, "contents")?, opt_list(body, "chunks")?) {
            (Some(list), None) => self.contents_of(list)?,
            (None, Some(list)) => chunk_list(list)?,
            _ => return Err("prepare_upload needs exactly one of chunks and contents".into()),
        };
        let expect = expectation(step, &[], &["actions"])?;
        let result = self.rpc(
            space,
            &session,
            at_ms,
            rpc_request::Call::PrepareUpload(PrepareUpload { chunks }),
        )?;
        match (result, opt_str(expect, "error")?) {
            (rpc_response::Result::Error(e), Some(code)) => check_error(&e, code),
            (rpc_response::Result::Error(e), None) => {
                Err(format!("PrepareUpload failed: {}", error_name(e.code)))
            }
            (rpc_response::Result::PrepareUpload(plan), None) => {
                let actions: Vec<String> = plan
                    .chunks
                    .iter()
                    .map(|c| {
                        chunk_plan::Action::try_from(c.action)
                            .map(|a| a.as_str_name().trim_start_matches("ACTION_").to_owned())
                            .unwrap_or_default()
                    })
                    .collect();
                let expected = strings(req(opt_list(expect, "actions"), "actions")?, "actions")?;
                if actions == expected {
                    Ok(())
                } else {
                    Err(format!("actions {actions:?}, expected {expected:?}"))
                }
            }
            (_, Some(code)) => Err(format!("expected error {code}, got a result")),
            _ => Err("unexpected response".into()),
        }
    }

    fn ops(&mut self, space: &str, list: &[Value]) -> Result<(Vec<Op>, Vec<String>), String> {
        let mut ops = Vec::new();
        let mut contents = Vec::new();
        for entry in list {
            if entry.get("repeat").is_some() {
                keys(entry, "repeat", &["repeat", "op"])?;
                let n = req(opt_u64(entry, "repeat"), "repeat")?;
                let (one, used) = self.ops(space, std::slice::from_ref(&entry["op"]))?;
                for _ in 0..n {
                    ops.extend(one.iter().cloned());
                }
                contents.extend(used);
                continue;
            }
            let fields = object(entry, "an op")?;
            let [(kind, body)] = fields.iter().collect::<Vec<_>>()[..] else {
                return Err(format!("an op must have exactly one kind: {entry}"));
            };
            if kind == "create_dir_chain" {
                keys(body, kind, &["prefix", "parent", "count"])?;
                let prefix = req(opt_str(body, "prefix"), "prefix")?;
                let mut parent = self.id(space, req(opt_str(body, "parent"), "parent")?)?;
                for i in 1..=req(opt_u64(body, "count"), "count")? {
                    let symbol = format!("{prefix}{i}");
                    let node_id = self.id(space, &symbol)?;
                    ops.push(Op {
                        op: Some(op::Op::CreateDir(CreateDir {
                            node_id: node_id.clone(),
                            parent_id: parent,
                            name: symbol,
                            pending_for: String::new(),
                        })),
                    });
                    parent = node_id;
                }
                continue;
            }
            let allowed: &[&str] = match kind.as_str() {
                "create_dir" => &["node", "parent", "name", "pending_for"],
                "move" => &["node", "parent", "name", "pending_for", "from"],
                "create_file" => &["node", "parent", "name", "content"],
                "update_content" => &["node", "content", "resend"],
                "delete" => &["node", "requires"],
                "restore" => &["node", "version", "parent", "name"],
                "empty" => &[],
                other => return Err(format!("unknown op {other}")),
            };
            keys(body, kind, allowed)?;
            if kind == "empty" {
                ops.push(Op { op: None });
                continue;
            }
            let node = self.id(space, req(opt_str(body, "node"), "node")?)?;
            let parent = match opt_str(body, "parent")? {
                Some(p) => self.id(space, p)?,
                None => String::new(),
            };
            let name = opt_str(body, "name")?.unwrap_or_default().to_owned();
            let pending_for = opt_str(body, "pending_for")?.unwrap_or_default().to_owned();
            let mut content = || -> Result<Option<Content>, String> {
                let Some(symbol) = opt_str(body, "content")? else {
                    return Ok(None);
                };
                contents.push(symbol.to_owned());
                self.content(symbol).map(Some)
            };
            let op = match kind.as_str() {
                "create_dir" => op::Op::CreateDir(CreateDir {
                    node_id: node,
                    parent_id: parent,
                    name,
                    pending_for,
                }),
                "create_file" => op::Op::CreateFile(CreateFile {
                    node_id: node,
                    parent_id: parent,
                    name,
                    content: Some(req(content(), "content")?),
                }),
                "update_content" => op::Op::UpdateContent(UpdateContent {
                    node_id: node,
                    content: Some(req(content(), "content")?),
                    resend: opt_bool(body, "resend")?.unwrap_or(false),
                }),
                "move" => op::Op::Move(Move {
                    node_id: node,
                    new_parent_id: parent,
                    new_name: name,
                    from: match body.get("from") {
                        None => None,
                        Some(from) => {
                            keys(from, "from", &["parent", "name"])?;
                            Some(Place {
                                parent_id: self
                                    .id(space, req(opt_str(from, "parent"), "parent")?)?,
                                name: req(opt_str(from, "name"), "name")?.to_owned(),
                            })
                        }
                    },
                    pending_for,
                }),
                "delete" => op::Op::Delete(Delete {
                    node_id: node,
                    requires: match body.get("requires") {
                        None => None,
                        Some(requires) => {
                            keys(requires, "requires", &["node", "content_rev", "meta_rev"])?;
                            Some(NodeRevisions {
                                node_id: self.id(space, req(opt_str(requires, "node"), "node")?)?,
                                content_rev: req(opt_u64(requires, "content_rev"), "content_rev")?,
                                meta_rev: req(opt_u64(requires, "meta_rev"), "meta_rev")?,
                            })
                        }
                    },
                }),
                _ => op::Op::Restore(Restore {
                    node_id: node,
                    version_id: match opt_str(body, "version")? {
                        Some(v) => self.id(space, v)?,
                        None => String::new(),
                    },
                    target_parent_id: parent,
                    name,
                }),
            };
            ops.push(Op { op: Some(op) });
        }
        Ok((ops, contents))
    }

    fn commit(&mut self, space: &str, at_ms: i64, step: &Value) -> Outcome {
        let expect = expectation(
            step,
            &["missing", "missing_some"],
            &["seq", "results", "changed", "response"],
        )?;
        let result = self.send(space, at_ms, step)?;
        self.check_commit(space, expect, result)
    }

    fn send(
        &mut self,
        space: &str,
        at_ms: i64,
        step: &Value,
    ) -> Result<rpc_response::Result, String> {
        let session = self.session(step)?;
        let body = &step["commit"];
        keys(
            body,
            "commit",
            &[
                "commit_id",
                "base_seq",
                "history_id",
                "device_seq",
                "require_head",
                "ops",
                "upload",
                "receipts_from",
                "receipt_size_delta",
            ],
        )?;
        let (ops, used) = self.ops(space, req(opt_list(body, "ops"), "ops")?)?;
        let commit_id = match opt_str(body, "commit_id")? {
            Some(symbol) => self.id(space, symbol)?,
            None => self.ids.uuid_v7(self.now_ms as u64),
        };
        let upload = opt_bool(body, "upload")?.unwrap_or(true);
        if upload
            && (body.get("receipts_from").is_some() || body.get("receipt_size_delta").is_some())
        {
            return Err("receipts_from and receipt_size_delta apply only with upload false".into());
        }
        let receipts = if !upload {
            let from = opt_str(body, "receipts_from")?.unwrap_or(space).to_owned();
            let delta = opt_u32(body, "receipt_size_delta")?.unwrap_or(0);
            self.wallets
                .get(&(session.device_id.clone(), from))
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|r| UploadReceipt {
                    size: r.size + delta,
                    ..r
                })
                .collect()
        } else {
            let mut chunks = Vec::new();
            for symbol in &used {
                chunks.extend(self.content(symbol)?.chunks);
            }
            self.upload(space, &session, at_ms, chunks)?
        };
        let history_id = match opt_str(body, "history_id")? {
            Some(symbol) => self.id(space, symbol)?,
            None => self.space(space)?.history_id().to_owned(),
        };
        let slot = (session.device_id.clone(), space.to_owned());
        let sent = (slot.0.clone(), slot.1.clone(), commit_id.clone());
        let device_seq = match opt_u64(body, "device_seq")? {
            Some(device_seq) => device_seq,
            None => match self.sent_slots.get(&sent) {
                Some(&device_seq) => device_seq,
                None => self.slots.get(&slot).copied().unwrap_or(0) + 1,
            },
        };
        self.sent_slots.entry(sent).or_insert(device_seq);
        let commit = Commit {
            commit_id,
            base_seq: req(opt_u64(body, "base_seq"), "base_seq")?,
            ops,
            receipts,
            history_id,
            device_seq,
            require_head: opt_bool(body, "require_head")?.unwrap_or(false),
            run_id: String::new(),
            base_commit_id: String::new(),
            base_committed_at_ms: 0,
        };
        let result = self.rpc(space, &session, at_ms, rpc_request::Call::Commit(commit))?;
        match &result {
            rpc_response::Result::Error(e) if !e.missing_chunks.is_empty() => {
                self.last_missing.insert(
                    (session.device_id.clone(), space.to_owned()),
                    e.missing_chunks.clone(),
                );
            }
            rpc_response::Result::Commit(result) => {
                let top = self.top_seq.entry(space.to_owned()).or_default();
                if result.seq > *top {
                    *top = result.seq;
                    self.slots.insert(slot, device_seq);
                }
            }
            _ => {}
        }
        Ok(result)
    }

    fn symbol(&mut self, space: &str, id: &str, prefix: &str) -> Result<String, String> {
        if id == self.space(space)?.space_id() {
            return Ok("root".into());
        }
        if id.is_empty() {
            return Ok("=".into());
        }
        if let Some(symbol) = self.values.get(id) {
            return Ok(symbol.clone());
        }
        let symbol = format!("{prefix}{}", self.symbols.len() + 1);
        self.bind(&symbol, id)?;
        Ok(symbol)
    }

    fn recorded(
        &mut self,
        space: &str,
        result: &rpc_response::Result,
    ) -> Result<(Value, Vec<String>), String> {
        let result = match result {
            rpc_response::Result::Commit(result) => result,
            other => return Err(format!("a generated commit got {other:?}")),
        };
        let CommitResult {
            seq,
            results,
            changes,
            committed_at_ms,
        } = result;
        let mut listed = Vec::new();
        for got in results {
            let OpResult {
                code,
                node_id,
                name,
                reject_reason,
            } = got;
            listed.push(json!({
                "code": result_code(*code),
                "node": self.symbol(space, node_id, "copy")?,
                "name": name,
                "reason": reject_reason,
            }));
        }
        let mut changed = Vec::new();
        let mut states = Vec::new();
        for change in changes {
            let Some(change_record::Change::Node(node)) = &change.change else {
                return Err(format!("a generated commit returned {change:?}"));
            };
            if self.space(space)?.space_id() != node.node_id
                && !self.values.contains_key(&node.node_id)
            {
                return Err(format!(
                    "changed node {} is named by no result",
                    node.node_id
                ));
            }
            let state = self.recorded_state(space, node)?;
            changed.push(state["node"].as_str().unwrap_or_default().to_owned());
            states.push(json!({ "node": state }));
        }
        let response = json!({
            "seq": seq,
            "committed_at_ms": committed_at_ms,
            "results": listed,
            "changes": states,
        });
        Ok((json!({ "response": response }), changed))
    }

    fn recorded_node(&mut self, space: &str, symbol: &str) -> Result<Value, String> {
        let id = self.known(space, symbol)?;
        match self.space(space)?.node(&id) {
            None => Ok(json!({ "node": symbol, "absent": true })),
            Some(node) => self.recorded_state(space, &node),
        }
    }

    fn recorded_state(&mut self, space: &str, node: &NodeState) -> Result<Value, String> {
        let NodeState {
            node_id,
            parent_id,
            name,
            kind,
            status,
            content_rev,
            meta_rev,
            meta_edit_rev: _,
            trash_group,
            current,
            edit_rev: _,
            pending_for,
            pending_by,
        } = node;
        let trash_group = if trash_group.is_empty() {
            Value::Null
        } else {
            self.symbol(space, trash_group, "group")?.into()
        };
        let current = match current {
            None => Value::Null,
            Some(version) => self.recorded_current(space, version)?,
        };
        Ok(json!({
            "node": self.symbol(space, node_id, "copy")?,
            "parent": self.symbol(space, parent_id, "copy")?,
            "name": name,
            "kind": kind_name(*kind),
            "status": status_name(*status),
            "content_rev": content_rev,
            "meta_rev": meta_rev,
            "trash_group": trash_group,
            "current": current,
            "pending_for": match pending_for.as_str() {
                "" => Value::Null,
                name => name.into(),
            },
            "pending_by": match pending_by.as_str() {
                "" => Value::Null,
                device_id => self.device_symbol(device_id)?.into(),
            },
        }))
    }

    fn recorded_current(&mut self, space: &str, version: &VersionInfo) -> Result<Value, String> {
        let VersionInfo {
            version_id,
            seq,
            content,
            device_id,
            user_id,
            created_at_ms,
        } = version;
        let mut devices: Vec<(&String, &Session)> = self.devices.iter().collect();
        devices.sort_by_key(|(symbol, _)| *symbol);
        let device = self.device_symbol(device_id)?;
        let user = devices
            .iter()
            .find(|(_, s)| s.user_id == *user_id)
            .map(|(symbol, _)| symbol.to_string())
            .ok_or_else(|| format!("user {user_id} is no scenario device's user"))?;
        let (content, inline) = match content {
            None => (Value::Null, false),
            Some(content) => {
                let (symbol, _) = self
                    .contents
                    .iter()
                    .find(|(_, c)| c.manifest_digest == content.manifest_digest)
                    .ok_or("a version holds a content no scenario lists")?;
                (symbol.clone().into(), !content.chunks.is_empty())
            }
        };
        Ok(json!({
            "version": self.symbol(space, version_id, "version")?,
            "seq": seq,
            "content": content,
            "chunks_inline": inline,
            "device": device,
            "user": user,
            "created_at_ms": created_at_ms,
        }))
    }

    fn check_commit(
        &mut self,
        space: &str,
        expect: &Value,
        result: rpc_response::Result,
    ) -> Outcome {
        match (result, opt_str(expect, "error")?) {
            (rpc_response::Result::Error(e), Some(code)) => {
                check_error(&e, code)?;
                if let Some(some) = expect.get("missing_some") {
                    keys(some, "missing_some", &["contents", "count", "total"])?;
                    let from: BTreeSet<Vec<u8>> = self
                        .contents_of(req(opt_list(some, "contents"), "contents")?)?
                        .into_iter()
                        .map(|c| c.hash)
                        .collect();
                    let count = req(opt_u64(some, "count"), "count")?;
                    if e.missing_chunks.len() as u64 != count {
                        return Err(format!(
                            "{} missing chunks listed, expected {count}",
                            e.missing_chunks.len()
                        ));
                    }
                    if let Some(hash) = e.missing_chunks.iter().find(|h| !from.contains(*h)) {
                        return Err(format!("missing chunk {} is not in contents", hex(hash)));
                    }
                    let total = req(opt_u64(some, "total"), "total")?.to_string();
                    let first = e
                        .message
                        .split(|c: char| !c.is_ascii_digit())
                        .find(|number| !number.is_empty());
                    if first != Some(total.as_str()) {
                        return Err(format!(
                            "message {:?} does not give the total {total} first",
                            e.message
                        ));
                    }
                }
                if let Some(missing) = opt_list(expect, "missing")? {
                    let want: BTreeSet<Vec<u8>> = self
                        .contents_of(missing)?
                        .into_iter()
                        .map(|c| c.hash)
                        .collect();
                    let got: BTreeSet<Vec<u8>> = e.missing_chunks.iter().cloned().collect();
                    if got != want {
                        return Err(format!("missing_chunks differ: {} listed", got.len()));
                    }
                }
                Ok(())
            }
            (rpc_response::Result::Error(e), None) => Err(format!(
                "commit failed with {}: {}",
                error_name(e.code),
                e.message
            )),
            (rpc_response::Result::Commit(result), None) => {
                if let Some(response) = expect.get("response") {
                    keys(expect, "a full expect", &["response"])?;
                    return self.check_response(space, response, &result);
                }
                if let Some(seq) = opt_u64(expect, "seq")?
                    && result.seq != seq
                {
                    return Err(format!("seq {}, expected {seq}", result.seq));
                }
                if let Some(expected) = opt_list(expect, "results")? {
                    if expected.len() != result.results.len() {
                        return Err(format!(
                            "{} results, expected {}",
                            result.results.len(),
                            expected.len()
                        ));
                    }
                    for (i, (want, got)) in expected.iter().zip(&result.results).enumerate() {
                        self.check_result(space, want, got, false)
                            .map_err(|e| format!("result {}: {e}", i + 1))?;
                    }
                }
                if let Some(changed) = opt_list(expect, "changed")? {
                    let want = self.known_set(space, changed, "changed")?;
                    let got: BTreeSet<String> = node_states(&result.changes)
                        .map(|n| n.node_id.clone())
                        .collect();
                    if got != want {
                        return Err(format!(
                            "changed {:?}, expected {:?}",
                            self.names(&got),
                            self.names(&want)
                        ));
                    }
                }
                Ok(())
            }
            (_, Some(code)) => Err(format!("expected error {code}, got a result")),
            _ => Err("unexpected response".into()),
        }
    }

    fn check_response(&mut self, space: &str, want: &Value, got: &CommitResult) -> Outcome {
        const FIELDS: &[&str] = &["seq", "committed_at_ms", "results", "changes"];
        keys(want, "response", FIELDS)?;
        every(want, FIELDS)?;
        let CommitResult {
            seq,
            results,
            changes,
            committed_at_ms,
        } = got;
        let want_seq = req(opt_u64(want, "seq"), "seq")?;
        if *seq != want_seq {
            return Err(format!("seq {seq}, expected {want_seq}"));
        }
        let at = req(opt_i64(want, "committed_at_ms"), "committed_at_ms")?;
        if *committed_at_ms != at {
            return Err(format!("committed_at_ms {committed_at_ms}, expected {at}"));
        }
        let want_results = req(opt_list(want, "results"), "results")?;
        if want_results.len() != results.len() {
            return Err(format!(
                "{} results, expected {}",
                results.len(),
                want_results.len()
            ));
        }
        for (i, (want, got)) in want_results.iter().zip(results).enumerate() {
            self.check_result(space, want, got, true)
                .map_err(|e| format!("result {}: {e}", i + 1))?;
        }
        let want_changes = req(opt_list(want, "changes"), "changes")?;
        if want_changes.len() != changes.len() {
            return Err(format!(
                "{} changes, expected {}",
                changes.len(),
                want_changes.len()
            ));
        }
        for (i, (want, got)) in want_changes.iter().zip(changes).enumerate() {
            self.check_change(space, want, got)
                .map_err(|e| format!("change {}: {e}", i + 1))?;
        }
        Ok(())
    }

    fn check_change(&mut self, space: &str, want: &Value, got: &ChangeRecord) -> Outcome {
        keys(want, "a change", &["node"])?;
        let ChangeRecord { change } = got;
        let found = match change {
            Some(change_record::Change::Node(node)) => {
                let want = want.get("node").ok_or("missing field node")?;
                return self.check_node_state(space, want, node, true);
            }
            Some(change_record::Change::Purged(_)) => "a purge",
            Some(change_record::Change::Space(_)) => "a space update",
            Some(change_record::Change::Member(_)) => "a member update",
            None => "an empty change",
        };
        Err(format!("{found}, expected a node"))
    }

    fn check_result(
        &mut self,
        space: &str,
        want: &Value,
        got: &OpResult,
        complete: bool,
    ) -> Outcome {
        const FIELDS: &[&str] = &["code", "node", "name", "reason"];
        keys(want, "a result", FIELDS)?;
        if complete {
            every(want, FIELDS)?;
        }
        let OpResult {
            code,
            node_id,
            name,
            reject_reason,
        } = got;
        let code = result_code(*code);
        let want_code = req(opt_str(want, "code"), "code")?;
        if code != want_code {
            return Err(format!("{code} {reject_reason:?}, expected {want_code}"));
        }
        if let Some(want_name) = opt_str(want, "name")?
            && name != want_name
        {
            return Err(format!("name {name:?}, expected {want_name:?}"));
        }
        if let Some(reason) = opt_str(want, "reason")?
            && reject_reason != reason
        {
            return Err(format!("reason {reject_reason:?}, expected {reason}"));
        }
        if let Some(node) = opt_str(want, "node")? {
            self.expect_id(space, node, node_id)?;
        }
        Ok(())
    }

    fn names(&self, ids: &BTreeSet<String>) -> Vec<String> {
        ids.iter()
            .map(|id| self.values.get(id).cloned().unwrap_or_else(|| id.clone()))
            .collect()
    }

    fn check_content(&self, got: Option<&Content>, symbol: &str) -> Outcome {
        let want = self.content(symbol)?;
        let got = got.ok_or_else(|| format!("no content, expected {symbol}"))?;
        if got.size == want.size
            && got.manifest_digest == want.manifest_digest
            && got.executable == want.executable
            && got.mtime_ns == self.mtime_ns
        {
            Ok(())
        } else {
            Err(format!("content differs from {symbol}"))
        }
    }

    fn check_version(&mut self, space: &str, got: &VersionInfo, want: &Value) -> Outcome {
        keys(want, "a version", &["version", "seq", "content"])?;
        if let Some(seq) = opt_u64(want, "seq")?
            && got.seq != seq
        {
            return Err(format!("version seq {}, expected {seq}", got.seq));
        }
        if let Some(symbol) = opt_str(want, "version")? {
            self.expect_id(space, symbol, &got.version_id)?;
        }
        if let Some(content) = opt_str(want, "content")? {
            self.check_content(got.content.as_ref(), content)?;
        }
        Ok(())
    }

    fn list_versions(&mut self, space: &str, at_ms: i64, step: &Value) -> Outcome {
        let session = self.session(step)?;
        let body = &step["list_versions"];
        keys(body, "list_versions", &["node", "cursor", "limit"])?;
        let cursor = match body.get("cursor") {
            None => String::new(),
            Some(Value::String(s)) => s
                .strip_prefix('=')
                .ok_or_else(|| format!("a cursor string must be a = literal, got {s:?}"))?
                .to_owned(),
            Some(pair) => {
                keys(pair, "cursor", &["returned"])?;
                let name = req(opt_str(pair, "returned"), "returned")?;
                self.cursors
                    .get(name)
                    .cloned()
                    .ok_or_else(|| format!("no returned cursor {name}"))?
            }
        };
        let request = ListVersions {
            node_id: self.id(space, req(opt_str(body, "node"), "node")?)?,
            cursor,
            limit: opt_u32(body, "limit")?.unwrap_or(0),
        };
        let expect = expectation(step, &[], &["count", "versions", "next_cursor"])?;
        let result = self.rpc(
            space,
            &session,
            at_ms,
            rpc_request::Call::ListVersions(request),
        )?;
        match (result, opt_str(expect, "error")?) {
            (rpc_response::Result::Error(e), Some(code)) => check_error(&e, code),
            (rpc_response::Result::Error(e), None) => {
                Err(format!("ListVersions failed: {}", error_name(e.code)))
            }
            (rpc_response::Result::Versions(list), None) => {
                if let Some(count) = opt_u64(expect, "count")?
                    && list.versions.len() as u64 != count
                {
                    return Err(format!(
                        "{} versions, expected {count}",
                        list.versions.len()
                    ));
                }
                if let Some(want) = opt_list(expect, "versions")? {
                    if want.len() != list.versions.len() {
                        return Err(format!(
                            "{} versions, expected {}",
                            list.versions.len(),
                            want.len()
                        ));
                    }
                    for (got, want) in list.versions.iter().zip(want) {
                        self.check_version(space, got, want)?;
                    }
                }
                match expect.get("next_cursor") {
                    None => {}
                    Some(Value::String(s)) if s.is_empty() => {
                        if !list.next_cursor.is_empty() {
                            return Err(format!(
                                "next_cursor {:?}, expected the last page",
                                list.next_cursor
                            ));
                        }
                    }
                    Some(pair) => {
                        keys(pair, "next_cursor", &["bind"])?;
                        let name = req(opt_str(pair, "bind"), "bind")?;
                        if list.next_cursor.is_empty() {
                            return Err("next_cursor is empty, expected another page".into());
                        }
                        if self.cursors.contains_key(name) {
                            return Err(format!("cursor {name} is bound twice"));
                        }
                        self.cursors.insert(name.to_owned(), list.next_cursor);
                    }
                }
                Ok(())
            }
            (_, Some(code)) => Err(format!("expected error {code}, got a result")),
            _ => Err("unexpected response".into()),
        }
    }

    fn check_state(&mut self, space: &str, step: &Value) -> Outcome {
        let mut known = false;
        if let Some(head) = opt_u64(step, "head")? {
            known = true;
            let got = self.space(space)?.head_seq();
            if got != head {
                return Err(format!("head_seq {got}, expected {head}"));
            }
        }
        if let Some(nodes) = opt_list(step, "nodes")? {
            known = true;
            for want in nodes {
                self.check_node(space, want)?;
            }
        }
        if let Some(live) = opt_list(step, "live")? {
            known = true;
            let root = self.space(space)?.space_id().to_owned();
            let got: BTreeSet<String> = self
                .space(space)?
                .nodes()
                .into_iter()
                .filter(|n| n.status == NodeStatus::Live as i32 && n.node_id != root)
                .map(|n| n.node_id)
                .collect();
            let want = self.known_set(space, live, "live")?;
            if got != want {
                return Err(format!(
                    "live nodes {:?}, expected {:?}",
                    self.names(&got),
                    self.names(&want)
                ));
            }
        }
        if known {
            Ok(())
        } else {
            Err(format!("a step does nothing: {step}"))
        }
    }

    fn check_node(&mut self, space: &str, want: &Value) -> Outcome {
        let symbol = req(opt_str(want, "node"), "node")?;
        let id = self.known(space, symbol)?;
        let got: Option<NodeState> = self.space(space)?.node(&id);
        if let Some(absent) = opt_bool(want, "absent")? {
            if !absent {
                return Err(format!("{symbol}: absent may only be true"));
            }
            keys(want, "an absent node", &["node", "absent"])?;
            return match got {
                None => Ok(()),
                Some(_) => Err(format!("{symbol} exists, expected it absent")),
            };
        }
        let got = got.ok_or_else(|| format!("{symbol} does not exist"))?;
        self.check_node_state(space, want, &got, want.get("current").is_some())
    }

    fn check_node_state(
        &mut self,
        space: &str,
        want: &Value,
        got: &NodeState,
        complete: bool,
    ) -> Outcome {
        if complete {
            keys(
                want,
                "a node",
                &[NODE_FIELDS, &["edit_rev", "meta_edit_rev"]].concat(),
            )?;
            every(want, NODE_FIELDS)?;
        } else {
            keys(
                want,
                "a node",
                &[
                    NODE_FIELDS,
                    &["absent", "content", "edit_rev", "meta_edit_rev"],
                ]
                .concat(),
            )?;
        }
        let NodeState {
            node_id,
            parent_id,
            name,
            kind,
            status,
            content_rev,
            meta_rev,
            meta_edit_rev,
            trash_group,
            current,
            edit_rev,
            pending_for,
            pending_by,
        } = got;
        let symbol = req(opt_str(want, "node"), "node")?;
        self.expect_id(space, symbol, node_id)
            .map_err(|e| format!("node: {e}"))?;
        let differs = |field: &str, got: &dyn std::fmt::Debug| {
            format!("{symbol}.{field} is {got:?}, expected {}", want[field])
        };
        if let Some(parent) = opt_str(want, "parent")? {
            self.expect_id(space, parent, parent_id)
                .map_err(|e| format!("{symbol}.parent: {e}"))?;
        }
        if let Some(want_name) = opt_str(want, "name")?
            && name != want_name
        {
            return Err(differs("name", name));
        }
        if let Some(want_kind) = opt_str(want, "kind")?
            && kind_name(*kind) != want_kind
        {
            return Err(differs("kind", &kind_name(*kind)));
        }
        if let Some(want_status) = opt_str(want, "status")?
            && status_name(*status) != want_status
        {
            return Err(differs("status", &status_name(*status)));
        }
        if let Some(rev) = opt_u64(want, "content_rev")?
            && *content_rev != rev
        {
            return Err(differs("content_rev", content_rev));
        }
        if let Some(rev) = opt_u64(want, "meta_rev")?
            && *meta_rev != rev
        {
            return Err(differs("meta_rev", meta_rev));
        }
        if let Some(rev) = opt_u64(want, "edit_rev")?
            && *edit_rev != rev
        {
            return Err(differs("edit_rev", edit_rev));
        }
        if let Some(rev) = opt_u64(want, "meta_edit_rev")?
            && *meta_edit_rev != rev
        {
            return Err(differs("meta_edit_rev", meta_edit_rev));
        }
        match want.get("trash_group") {
            None => {}
            Some(Value::Null) => {
                if !trash_group.is_empty() {
                    return Err(differs("trash_group", trash_group));
                }
            }
            Some(Value::String(group)) => self
                .expect_id(space, group, trash_group)
                .map_err(|e| format!("{symbol}.trash_group: {e}"))?,
            Some(other) => {
                return Err(format!("trash_group must be a string or null, got {other}"));
            }
        }
        match want.get("pending_for") {
            None => {}
            Some(Value::Null) if pending_for.is_empty() => {}
            Some(Value::String(name)) if pending_for == name => {}
            Some(Value::Null | Value::String(_)) => {
                return Err(differs("pending_for", pending_for));
            }
            Some(other) => {
                return Err(format!("pending_for must be a string or null, got {other}"));
            }
        }
        match want.get("pending_by") {
            None => {}
            Some(Value::Null) if pending_by.is_empty() => {}
            Some(Value::String(device)) if *pending_by == self.session_of(device)?.device_id => {}
            Some(Value::Null | Value::String(_)) => {
                return Err(differs("pending_by", pending_by));
            }
            Some(other) => {
                return Err(format!("pending_by must be a device or null, got {other}"));
            }
        }
        if let Some(content) = opt_str(want, "content")? {
            self.check_content(current.as_ref().and_then(|v| v.content.as_ref()), content)
                .map_err(|e| format!("{symbol}: {e}"))?;
        }
        match (want.get("current"), current) {
            (None, _) | (Some(Value::Null), None) => Ok(()),
            (Some(Value::Null), Some(_)) => {
                Err(format!("{symbol} has a current version, expected none"))
            }
            (Some(_), None) => Err(format!("{symbol} has no current version, expected one")),
            (Some(want), Some(got)) => self
                .check_current(space, want, got)
                .map_err(|e| format!("{symbol}.current: {e}")),
        }
    }

    fn check_current(&mut self, space: &str, want: &Value, got: &VersionInfo) -> Outcome {
        keys(want, "a current version", CURRENT_FIELDS)?;
        every(want, CURRENT_FIELDS)?;
        let VersionInfo {
            version_id,
            seq,
            content,
            device_id,
            user_id,
            created_at_ms,
        } = got;
        self.expect_id(space, req(opt_str(want, "version"), "version")?, version_id)
            .map_err(|e| format!("version: {e}"))?;
        let want_seq = req(opt_u64(want, "seq"), "seq")?;
        if *seq != want_seq {
            return Err(format!("seq {seq}, expected {want_seq}"));
        }
        let at = req(opt_i64(want, "created_at_ms"), "created_at_ms")?;
        if *created_at_ms != at {
            return Err(format!("created_at_ms {created_at_ms}, expected {at}"));
        }
        let device = self.session_of(req(opt_str(want, "device"), "device")?)?;
        if *device_id != device.device_id {
            return Err(format!(
                "device_id {device_id}, expected {}'s",
                device.device_name
            ));
        }
        let user = self.session_of(req(opt_str(want, "user"), "user")?)?;
        if *user_id != user.user_id {
            return Err(format!("user_id {user_id}, expected {}", user.user_id));
        }
        let inline = req(opt_bool(want, "chunks_inline"), "chunks_inline")?;
        match (&want["content"], content) {
            (Value::Null, None) => Ok(()),
            (Value::String(symbol), Some(got)) => self.check_full_content(got, symbol, inline),
            (Value::Null, Some(_)) => Err("a content, expected none".into()),
            (want, None) => Err(format!("no content, expected {want}")),
            (other, Some(_)) => Err(format!("content must be a string or null, got {other}")),
        }
    }

    fn device_symbol(&self, device_id: &str) -> Result<String, String> {
        let mut devices: Vec<(&String, &Session)> = self.devices.iter().collect();
        devices.sort_by_key(|(symbol, _)| *symbol);
        devices
            .iter()
            .find(|(_, s)| s.device_id == device_id)
            .map(|(symbol, _)| symbol.to_string())
            .ok_or_else(|| format!("device {device_id} is no scenario device"))
    }

    fn session_of(&self, symbol: &str) -> Result<Session, String> {
        self.devices
            .get(symbol)
            .cloned()
            .ok_or_else(|| format!("unknown device {symbol}"))
    }

    fn check_full_content(&self, got: &Content, symbol: &str, inline: bool) -> Outcome {
        let want = self.content(symbol)?;
        let Content {
            size,
            mtime_ns,
            executable,
            chunks,
            chunk_count,
            manifest_digest,
            chunker,
        } = got;
        let listed: &[ChunkRef] = if inline { &want.chunks } else { &[] };
        let same_chunks = chunks.len() == listed.len()
            && chunks.iter().zip(listed).all(|(got, want)| {
                let ChunkRef {
                    hash,
                    size,
                    encoding,
                } = got;
                *hash == want.hash && *size == want.size && *encoding == want.encoding
            });
        let fields = [
            ("size", *size == want.size),
            ("mtime_ns", *mtime_ns == want.mtime_ns),
            ("executable", *executable == want.executable),
            ("chunks", same_chunks),
            ("chunk_count", *chunk_count == want.chunk_count),
            ("manifest_digest", *manifest_digest == want.manifest_digest),
            ("chunker", *chunker == want.chunker),
        ];
        let differ: Vec<&str> = fields
            .iter()
            .filter(|(_, same)| !same)
            .map(|(field, _)| *field)
            .collect();
        if differ.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "content differs from {symbol} in {}",
                differ.join(", ")
            ))
        }
    }
}
