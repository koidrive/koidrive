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
