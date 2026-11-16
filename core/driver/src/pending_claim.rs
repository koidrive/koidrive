// SPDX-License-Identifier: Apache-2.0

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::paths::Paths;

#[derive(Serialize, Deserialize)]
struct PendingClaim {
    server: String,
    code_sha256: String,
    request_id: String,
}

fn load(path: &Path) -> io::Result<Option<PendingClaim>> {
    match fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn digest(code: &str) -> String {
    (Sha256::digest(code.as_bytes()).iter())
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn request_id(path: &Path, server: &Url, code: &str) -> io::Result<String> {
    let code_sha256 = digest(code);
    if let Some(pending) = load(path)?
        && pending.server == server.as_str()
        && pending.code_sha256 == code_sha256
    {
        return Ok(pending.request_id);
    }
    let pending = PendingClaim {
        server: server.to_string(),
        code_sha256,
        request_id: koidrive_net::request_id(),
    };
    let temp = path.with_extension("json.new");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)?;
    file.write_all(&serde_json::to_vec(&pending).expect("a pending claim serializes"))?;
    file.sync_all()?;
    fs::rename(&temp, path)?;
    if let Some(dir) = path.parent() {
        File::open(dir)?.sync_all()?;
    }
    Ok(pending.request_id)
}

pub(crate) fn clear(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Whether this profile sent a claim to `server` that ended without an answer. Claiming again with the same code
/// within the code's hour finishes it.
pub fn claim_pending(paths: &Paths, server: &Url) -> bool {
    load(&paths.pending_claim())
        .ok()
        .flatten()
        .is_some_and(|pending| pending.server == server.as_str())
}
