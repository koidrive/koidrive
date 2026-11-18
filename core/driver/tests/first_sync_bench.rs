// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use koidrive_driver::{Driver, FolderState, Paths};
use koidrive_net::{Api, Auth, Credentials};
use koidrive_proto::v1::{ClaimRequest, CreateSpaceRequest};
use url::Url;

fn server_url() -> Url {
    std::env::var("KOIDRIVE_TEST_SERVER")
        .expect("KOIDRIVE_TEST_SERVER is not set")
        .parse()
        .unwrap()
}

fn api() -> Api {
    Api::local_development(server_url()).unwrap()
}

async fn claim() -> Credentials {
    let log =
        std::env::var("KOIDRIVE_TEST_SERVER_LOG").expect("KOIDRIVE_TEST_SERVER_LOG is not set");
    reqwest::get(server_url().join("setup").unwrap())
        .await
        .unwrap();
    let started = Instant::now();
    let code = loop {
        let text = fs::read_to_string(&log).unwrap_or_default();
        if let Some(code) = text.lines().rev().find_map(claim_code) {
            break code;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "no claim code in {log}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let request = ClaimRequest {
        code,
        user_name: "Owner".into(),
        device_name: "owner".into(),
        platform: "test".into(),
        setup_attempt: String::new(),
        request_id: String::new(),
    };
    Credentials::from(&api().claim(&request).await.unwrap())
}

fn claim_code(line: &str) -> Option<String> {
    if !line.to_ascii_lowercase().contains("claim code") {
        return None;
    }
    line.split_whitespace()
        .map(|word| word.trim_matches(|c: char| !c.is_ascii_alphanumeric()))
        .rfind(|word| word.contains('-') && word.len() >= 8)
        .map(str::to_owned)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the Worker under wrangler dev and KOIDRIVE_BENCH_TREE"]
async fn first_sync_of_a_large_folder() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .try_init();
    let tree = std::env::var("KOIDRIVE_BENCH_TREE").expect("KOIDRIVE_BENCH_TREE is not set");
    let limit = Duration::from_secs(
        std::env::var("KOIDRIVE_BENCH_SECONDS").map_or(1800, |s| s.parse().unwrap()),
    );
    let owner = Arc::new(Auth::new(api(), claim().await));
    let space = (owner.api())
        .create_space(
            &owner,
            &CreateSpaceRequest {
                name: "bench".into(),
                request_id: String::new(),
            },
        )
        .await
        .unwrap();
    let home = tempfile::tempdir().unwrap();
    let folder = home.path().join("folder");
    let copied = Command::new("cp")
        .arg("-R")
        .arg(Path::new(&tree))
        .arg(&folder)
        .status()
        .unwrap();
    assert!(copied.success());
    let driver = Driver::open(Paths::under(&home.path().join("profile"))).unwrap();
    let pairing = owner.api().pair_start(&owner).await.unwrap();
    driver
        .pair(server_url().as_str(), &pairing.code, "bench")
        .await
        .unwrap();
    let started = Instant::now();
    driver.link(&folder, &space.space_id).await.unwrap();
    driver.start();
    let mut last = None;
    let done = loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let status = driver.status().folders.remove(0);
        let line = serde_json::json!({
            "s": started.elapsed().as_secs(),
            "state": format!("{:?}", status.state),
            "remote_nodes": status.remote_nodes,
            "local_entries": status.local_entries,
            "progress": status.progress,
            "problem": status.problem,
        });
        println!("{line}");
        if status.state == FolderState::UpToDate
            && status.progress.is_none()
            && status.remote_nodes > 1
        {
            break Some(status);
        }
        if status.state == FolderState::Halted || started.elapsed() > limit {
            last = Some(status);
            break None;
        }
    };
    let elapsed = started.elapsed().as_secs_f64();
    let status = done.or(last).unwrap();
    println!(
        "{}",
        serde_json::json!({
            "summary": true,
            "finished": status.state == FolderState::UpToDate,
            "seconds": elapsed,
            "remote_nodes": status.remote_nodes,
            "nodes_per_second": status.remote_nodes as f64 / elapsed,
        })
    );
    driver.stop().await;
}

fn resident_bytes() -> u64 {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .unwrap()
        * 1024
}

fn scale_tree(root: &Path, projects: usize, packages: usize, distinct_projects: usize) -> u64 {
    let body = "module.exports = function () { return Object.freeze({ ok: true, value: 42 }); };\n"
        .repeat(4);
    let threads: Vec<_> = (0..projects)
        .map(|project| {
            let root = root.join(format!("project{project:02}/node_modules"));
            let body = body.clone();
            std::thread::spawn(move || {
                let mut entries = 2u64;
                for package in 0..packages {
                    let version = if project < distinct_projects {
                        project
                    } else {
                        distinct_projects
                    };
                    let dir = root.join(format!("pkg-{package:04}"));
                    let content =
                        |name: &str| format!("// pkg-{package:04}@{version} {name}\n{body}");
                    let mut files = vec![
                        "package.json".to_owned(),
                        "README.md".to_owned(),
                        "LICENSE".to_owned(),
                        "index.js".to_owned(),
                    ];
                    for a in 0..3 {
                        for b in 0..3 {
                            for file in 0..4 {
                                files.push(format!("lib/m{a}/s{b}/f{file}.js"));
                            }
                        }
                    }
                    for name in &files {
                        let path = dir.join(name);
                        fs::create_dir_all(path.parent().unwrap()).unwrap();
                        fs::write(&path, content(name)).unwrap();
                    }
                    entries += 1 + 1 + 3 + 9 + files.len() as u64;
                }
                entries
            })
        })
        .collect();
    threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .sum()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the Worker under wrangler dev and a few GB of disk"]
async fn a_first_sync_of_two_million_entries_keeps_moving() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .try_init();
    let seconds = |name, default| {
        Duration::from_secs(std::env::var(name).map_or(default, |s: String| s.parse().unwrap()))
    };
    let window = seconds("KOIDRIVE_SCALE_SECONDS", 600);
    let longest_allowed = seconds("KOIDRIVE_SCALE_MAX_STALL", 5);
    let base =
        std::env::var("KOIDRIVE_SCALE_DIR").map_or_else(|_| std::env::temp_dir(), Into::into);
    let home = tempfile::tempdir_in(base).unwrap();
    let folder = home.path().join("folder");
    let made = Instant::now();
    let entries = scale_tree(&folder, 40, 925, 4);
    println!(
        "{}",
        serde_json::json!({ "tree": true, "entries": entries, "seconds": made.elapsed().as_secs() })
    );
    let owner = Arc::new(Auth::new(api(), claim().await));
    let space = (owner.api())
        .create_space(
            &owner,
            &CreateSpaceRequest {
                name: "scale".into(),
                request_id: String::new(),
            },
        )
        .await
        .unwrap();
    let driver = Driver::open(Paths::under(&home.path().join("profile"))).unwrap();
    let pairing = owner.api().pair_start(&owner).await.unwrap();
    driver
        .pair(server_url().as_str(), &pairing.code, "scale")
        .await
        .unwrap();
    let started = Instant::now();
    driver.link(&folder, &space.space_id).await.unwrap();
    driver.start();
    let mut moved = None;
    let mut seen = (0, 0);
    let mut longest = Duration::ZERO;
    let mut problems = Vec::new();
    let mut peak = resident_bytes();
    let mut measured = Instant::now();
    while started.elapsed() < window {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if measured.elapsed() >= Duration::from_secs(5) {
            peak = peak.max(resident_bytes());
            measured = Instant::now();
        }
        let status = driver.status().folders.remove(0);
        if let Some(problem) = &status.problem {
            problems.push(problem.clone());
        }
        let now = (
            status.remote_nodes,
            status
                .progress
                .as_ref()
                .map_or(0, |progress| progress.bytes_done),
        );
        if status.state == FolderState::UpToDate {
            break;
        }
        if now != seen {
            if moved.is_none() {
                println!(
                    "{}",
                    serde_json::json!({ "first_move": started.elapsed().as_secs() })
                );
            }
            seen = now;
            moved = Some(Instant::now());
        } else if let Some(at) = moved {
            longest = longest.max(at.elapsed());
        }
    }
    let status = driver.status().folders.remove(0);
    let resident = resident_bytes();
    peak = peak.max(resident);
    println!(
        "{}",
        serde_json::json!({
            "summary": true,
            "peak_resident_bytes": peak,
            "resident_bytes": resident,
            "resident_bytes_per_synced_entry": resident.checked_div(status.remote_nodes),
            "seconds": started.elapsed().as_secs(),
            "state": format!("{:?}", status.state),
            "remote_nodes": status.remote_nodes,
            "bytes_done": status.progress.as_ref().map(|progress| progress.bytes_done),
            "longest_stall_ms": longest.as_millis() as u64,
            "problems": problems.len(),
        })
    );
    driver.stop().await;
    assert!(
        moved.is_some(),
        "the first sync never moved within {window:?}"
    );
    assert!(
        problems.is_empty(),
        "the folder showed problems: {:?}",
        &problems[..problems.len().min(5)]
    );
    assert!(
        longest <= longest_allowed,
        "the first sync stood still for {longest:?}, longer than {longest_allowed:?}"
    );
}
