// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::time::{Duration, Instant};

use koidrive_net::{Api, Credentials};
use koidrive_proto::v1::ClaimRequest;
use url::Url;

pub fn server_url() -> Url {
    let url = std::env::var("KOIDRIVE_TEST_SERVER").expect(
        "KOIDRIVE_TEST_SERVER is not set; run core/driver/scripts/integration-test.sh, which starts the Worker",
    );
    url.parse().expect("KOIDRIVE_TEST_SERVER is a URL")
}

pub fn api() -> Api {
    Api::local_development(server_url()).unwrap()
}

pub async fn claim() -> Credentials {
    let log =
        std::env::var("KOIDRIVE_TEST_SERVER_LOG").expect("KOIDRIVE_TEST_SERVER_LOG is not set");
    let setup = reqwest::get(server_url().join("setup").unwrap())
        .await
        .unwrap();
    assert!(
        setup.status().is_success(),
        "GET /setup answered {}",
        setup.status()
    );
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
    Credentials::from(&api().claim(&request).await.expect("the claim succeeds"))
}

fn claim_code(line: &str) -> Option<String> {
    if !line.to_ascii_lowercase().contains("claim code") {
        return None;
    }
    line.split_whitespace()
        .map(|word| word.trim_matches(|c: char| !c.is_ascii_alphanumeric()))
        .rfind(|word| {
            word.contains('-')
                && word.len() >= 8
                && word.split('-').all(|group| {
                    !group.is_empty() && group.chars().all(|c| c.is_ascii_alphanumeric())
                })
        })
        .map(str::to_owned)
}
