// SPDX-License-Identifier: Apache-2.0

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use koidrive_driver::{Driver, FolderState, Paths, api_for};
use koidrive_net::{Api, Auth, Credentials};
use koidrive_proto::v1::{ClaimRequest, CreateSpaceRequest};
use serde::Deserialize;
use url::Url;

fn server_url() -> Url {
    std::env::var("KOIDRIVE_BENCH_URL")
        .or_else(|_| std::env::var("KOIDRIVE_TEST_SERVER"))
        .expect("neither KOIDRIVE_BENCH_URL nor KOIDRIVE_TEST_SERVER is set")
        .parse()
        .unwrap()
}

fn api() -> Api {
    api_for(server_url().as_str()).unwrap()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Owner {
    user_id: String,
    device_id: String,
    refresh_credential: String,
}

async fn owner() -> Credentials {
    let Ok(raw) = std::env::var("KOIDRIVE_BENCH_OWNER") else {
        return claim().await;
    };
    let owner: Owner =
        serde_json::from_str(&raw).unwrap_or_else(|_| panic!("invalid claimed owner credentials"));
    Credentials {
        user_id: owner.user_id,
        device_id: owner.device_id,
        refresh_credential: owner.refresh_credential,
    }
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
#[ignore = "needs a Worker and KOIDRIVE_BENCH_TREE"]
async fn first_sync_of_a_large_folder() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .try_init();
    let tree = std::env::var("KOIDRIVE_BENCH_TREE").expect("KOIDRIVE_BENCH_TREE is not set");
    let limit = Duration::from_secs(
        std::env::var("KOIDRIVE_BENCH_SECONDS").map_or(1800, |s| s.parse().unwrap()),
    );
    let owner = Arc::new(Auth::new(api(), owner().await));
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
    let nodes = 1 + entries(&folder);
    let driver = Driver::open(Paths::under(&home.path().join("profile"))).unwrap();
    let pairing = owner.api().pair_start(&owner).await.unwrap();
    driver
        .pair(server_url().as_str(), &pairing.code, "bench")
        .await
        .unwrap();
    let started = Instant::now();
    driver.link(&folder, &space.space_id).await.unwrap();
    driver.start();
    let (finished, status) = loop {
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
        let finished = status.state == FolderState::UpToDate
            && status.progress.is_none()
            && status.remote_nodes == nodes;
        if finished || status.state == FolderState::Halted || started.elapsed() > limit {
            break (finished, status);
        }
    };
    let elapsed = started.elapsed().as_secs_f64();
    println!(
        "{}",
        serde_json::json!({
            "summary": true,
            "finished": finished,
            "seconds": elapsed,
            "remote_nodes": status.remote_nodes,
            "folder_nodes": nodes,
            "nodes_per_second": status.remote_nodes as f64 / elapsed,
        })
    );
    if let Ok(path) = std::env::var("GITHUB_STEP_SUMMARY") {
        let mut summary = OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .expect("open job summary");
        writeln!(
            summary,
            "| First sync finished | Seconds | Nodes on the server | Nodes in the folder | Nodes/s |\n|---|---|---|---|---|\n| {} | {:.0} | {} | {} | {:.0} |",
            if finished { "yes" } else { "no" },
            elapsed,
            status.remote_nodes,
            nodes,
            status.remote_nodes as f64 / elapsed,
        )
        .expect("write job summary");
    }
    driver.stop().await;
    assert!(
        finished,
        "the first sync did not finish: {} of {nodes} nodes on the server, {:?}",
        status.remote_nodes, status.problem
    );
}

fn entries(dir: &Path) -> u64 {
    fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            match kind.is_dir() {
                true => 1 + entries(&entry.path()),
                false => u64::from(kind.is_file()),
            }
        })
        .sum()
}
