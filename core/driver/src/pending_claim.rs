// SPDX-License-Identifier: Apache-2.0

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::paths::Paths;

#[derive(Serialize, Deserialize, PartialEq, Eq)]
struct PendingClaim {
    server: String,
    code_sha256: String,
    request_id: String,
}

fn load(path: &Path) -> io::Result<Vec<PendingClaim>> {
    match fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).unwrap_or_default()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

fn save(path: &Path, claims: &[PendingClaim]) -> io::Result<()> {
    if claims.is_empty() {
        return clear(path);
    }
    let temp = path.with_file_name(format!(".pending-claim.json.{}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)?;
    file.write_all(&serde_json::to_vec(claims).expect("pending claims serialize"))?;
    file.sync_all()?;
    fs::rename(&temp, path)?;
    if let Some(dir) = path.parent() {
        File::open(dir)?.sync_all()?;
    }
    Ok(())
}

fn digest(code: &str) -> String {
    let normalized: String = (code.chars())
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_uppercase())
        .collect();
    (Sha256::digest(normalized.as_bytes()).iter())
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn same(claim: &PendingClaim, server: &Url, code: &str) -> bool {
    claim.server == server.as_str() && claim.code_sha256 == digest(code)
}

pub(crate) fn request_id(path: &Path, server: &Url, code: &str) -> io::Result<String> {
    let mut claims = load(path)?;
    if let Some(claim) = claims.iter().find(|claim| same(claim, server, code)) {
        return Ok(claim.request_id.clone());
    }
    let request_id = koidrive_net::request_id();
    claims.push(PendingClaim {
        server: server.to_string(),
        code_sha256: digest(code),
        request_id: request_id.clone(),
    });
    save(path, &claims)?;
    Ok(request_id)
}

pub(crate) fn forget(path: &Path, server: &Url, code: &str) -> io::Result<()> {
    let mut claims = load(path)?;
    claims.retain(|claim| !same(claim, server, code));
    save(path, &claims)
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
        .unwrap_or_default()
        .iter()
        .any(|claim| claim.server == server.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_the_server_reads_the_same_resumes_the_same_claim() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending-claim.json");
        let server = Url::parse("https://koidrive.alice.workers.dev/").unwrap();

        let first = request_id(&path, &server, "ABCD-EFGH-JKLM").unwrap();
        assert_eq!(request_id(&path, &server, " abcdefghjklm ").unwrap(), first);
        let other = request_id(&path, &server, "ABCD-EFGH-JKLN").unwrap();
        assert_ne!(other, first);

        forget(&path, &server, "abcd efgh jkln").unwrap();
        assert_eq!(request_id(&path, &server, "abcd-efgh-jklm").unwrap(), first);
        forget(&path, &server, "ABCD-EFGH-JKLM").unwrap();
        assert!(!path.exists());
    }
}
