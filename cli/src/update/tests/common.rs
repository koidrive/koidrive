// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;

use std::ffi::OsStr;

use std::fs::{self, File};

use std::path::{Path, PathBuf};

use std::process::Stdio;

use std::time::{Duration, SystemTime};

use koidrive_driver::Paths;

use koidrive_driver::update;

use koidrive_ipc::Code;

use semver::Version;

use tokio::process::Command;

use url::Url;

use crate::report::{UpdateResult, Via};

use crate::service;

use super::handoff::scan;

use super::installer::installer;

use super::inventory::{Install, Installs, Method, command_text, homebrew_link};

use super::process::{Child, Echo};

use super::reconcile::{
    ReconcileError, UpdateBinary, app_not_updated, finish_reconciliation, reconcile_updated,
    update_problem, updated,
};

#[cfg(target_os = "macos")]
use super::reconcile::{discover_app_version, select_app_helper};

use super::refuse_root;

use super::release_source::RECONCILE;

use std::os::unix::process::CommandExt;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use tokio::net::TcpListener;

fn executable(path: &Path, script: &str) {
    super::fixture::executable(path, script.as_bytes());
}

fn echo(dir: &Path) -> Echo {
    Echo::open(&dir.join("logs/update.log"), true, false)
}

fn app_fixture(
    dir: &Path,
    actual: &str,
    change: crate::app::Change,
    problem: Option<String>,
) -> crate::app::Prepared {
    let path = dir.join("user Applications/Koidrive.app");
    let binary = path.join("Contents/Helpers/koi");
    executable(
        &binary,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\ncase \"$1\" in\n--version) echo 'koi {actual}' ;;\n_reconcile) echo '{{\"data\":{{\"service\":{{\"manager\":\"launchd\",\"name\":\"fixture\",\"state\":\"running\"}},\"app\":{{\"state\":\"skipped\",\"reason\":\"bundled_helper\"}}}}}}' ;;\nesac\n"
        ),
    );
    crate::app::Prepared::fixture(crate::app::Report::Present {
        committed: Vec::new(),
        path,
        version: "99.0.0".into(),
        change,
        launch: if problem.is_some() {
            crate::app::Launch::Failed
        } else {
            crate::app::Launch::NotRequested
        },
        problem,
        retired: Vec::new(),
        residue: Vec::new(),
        preserved: Vec::new(),
    })
}

async fn serve_once(listener: TcpListener, body: &'static str) {
    let (mut stream, _) = listener.accept().await.unwrap();
    let mut request = Vec::new();
    let mut buffer = [0; 4096];
    while !request.ends_with(b"\r\n\r\n") {
        let read = stream.read(&mut buffer).await.unwrap();
        assert!(read > 0);
        request.extend_from_slice(&buffer[..read]);
    }
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await.unwrap();
}
