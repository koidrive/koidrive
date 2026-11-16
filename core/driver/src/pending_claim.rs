// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::claim::Flock;
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

fn save(lease: &Flock, path: &Path, claims: &[PendingClaim]) -> io::Result<()> {
    if claims.is_empty() {
        return clear(lease, path);
    }
    let stage = koidrive_state_file::AtomicFile::prepare(path)?;
    stage
        .as_file()
        .write_all(&serde_json::to_vec(claims).expect("pending claims serialize"))?;
    stage
        .finish_with_legacy_matching(koidrive_state_file::Publish::Replace, |name| {
            legacy_stage(lease, name)
        })
        .map(drop)
}

fn legacy_stage(_profile_lease: &Flock, name: &OsStr) -> bool {
    name.to_str()
        .and_then(|name| name.strip_prefix(".pending-claim.json."))
        .and_then(|name| name.strip_suffix(".tmp"))
        .is_some_and(|pid| {
            pid.parse::<u32>()
                .is_ok_and(|number| number > 0 && number.to_string() == pid)
        })
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

pub(crate) fn request_id(
    lease: &Flock,
    path: &Path,
    server: &Url,
    code: &str,
) -> io::Result<String> {
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
    save(lease, path, &claims)?;
    Ok(request_id)
}

pub(crate) fn forget(lease: &Flock, path: &Path, server: &Url, code: &str) -> io::Result<()> {
    let mut claims = load(path)?;
    claims.retain(|claim| !same(claim, server, code));
    save(lease, path, &claims)
}

pub(crate) fn clear(lease: &Flock, path: &Path) -> io::Result<()> {
    koidrive_state_file::remove_with_legacy_matching(path, |name| legacy_stage(lease, name))
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

    #[cfg(target_os = "macos")]
    #[test]
    fn immutable_legacy_stages_do_not_block_saving_forgetting_or_clearing_claims() {
        use std::process::Command;

        let dir = tempfile::tempdir().unwrap();
        let lease = lease(dir.path());
        let path = dir.path().join("pending-claim.json");
        let server = Url::parse("https://koidrive.alice.workers.dev/").unwrap();
        let other = Url::parse("https://koidrive.bob.workers.dev/").unwrap();
        let stale = dir.path().join(".pending-claim.json.123.tmp");
        fs::write(&stale, b"obsolete claim").unwrap();
        assert!(
            Command::new("chflags")
                .arg("uchg")
                .arg(&stale)
                .status()
                .unwrap()
                .success()
        );

        let result = std::panic::catch_unwind(|| -> io::Result<_> {
            let first = request_id(&lease, &path, &server, "ABCD-EFGH-JKLM")?;
            let retained = request_id(&lease, &path, &other, "BCDE-FGHJ-KLMN")?;
            let resumed = request_id(&lease, &path, &server, "ABCD-EFGH-JKLM")?;
            forget(&lease, &path, &server, "ABCD-EFGH-JKLM")?;
            let claims = load(&path)?;
            let retained_again = request_id(&lease, &path, &other, "BCDE-FGHJ-KLMN")?;
            clear(&lease, &path)?;
            let absent = !path.exists();
            clear(&lease, &path)?;
            let obsolete = fs::read(&stale)?;
            let protected = fs::remove_file(&stale);
            Ok((
                first,
                retained,
                resumed,
                claims,
                retained_again,
                absent,
                obsolete,
                protected,
            ))
        });

        assert!(
            Command::new("chflags")
                .arg("nouchg")
                .arg(&stale)
                .status()
                .unwrap()
                .success()
        );
        let (first, retained, resumed, claims, retained_again, absent, obsolete, protected) =
            result.unwrap().unwrap();
        assert_eq!(first, resumed);
        assert_eq!(claims.len(), 1);
        assert!(same(&claims[0], &other, "BCDE-FGHJ-KLMN"));
        assert_eq!(claims[0].request_id, retained);
        assert_eq!(retained_again, retained);
        assert!(absent);
        assert_eq!(obsolete, b"obsolete claim");
        assert_eq!(
            protected.unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    fn lease(dir: &Path) -> Flock {
        Flock::take(fs::File::create(dir.join("lock")).unwrap())
            .unwrap()
            .unwrap()
    }

    #[test]
    fn forgetting_a_claim_recovers_pid_stages_without_changing_other_claims() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let lease = lease(dir.path());
        let path = dir.path().join("pending-claim.json");
        let server = Url::parse("https://koidrive.alice.workers.dev/").unwrap();
        let other = Url::parse("https://koidrive.bob.workers.dev/").unwrap();
        request_id(&lease, &path, &server, "ABCD-EFGH-JKLM").unwrap();
        let retained = request_id(&lease, &path, &other, "BCDE-FGHJ-KLMN").unwrap();
        let stale = dir.path().join(".pending-claim.json.123.tmp");
        fs::write(&stale, fs::read(&path).unwrap()).unwrap();
        let untouched = [
            ".pending-claim.json.0.tmp",
            ".pending-claim.json.0123.tmp",
            ".pending-claim.json.+123.tmp",
            ".pending-claim.json.4294967296.tmp",
            ".pending-claim.json.123.tmp.backup",
            ".pending-claim.json.abc.tmp",
            "pending-claim.json.123.tmp",
        ];
        for name in untouched {
            fs::write(dir.path().join(name), b"not a writer stage").unwrap();
        }

        forget(&lease, &path, &server, "ABCD-EFGH-JKLM").unwrap();

        assert!(!stale.exists());
        let claims = load(&path).unwrap();
        assert_eq!(claims.len(), 1);
        assert!(same(&claims[0], &other, "BCDE-FGHJ-KLMN"));
        assert_eq!(claims[0].request_id, retained);
        assert_eq!(
            request_id(&lease, &path, &other, "BCDE-FGHJ-KLMN").unwrap(),
            retained
        );
        let bytes = fs::read(&path).unwrap();
        for code in ["ABCD-EFGH-JKLM", "BCDE-FGHJ-KLMN", "BCDEFGHJKLMN"] {
            assert!(!String::from_utf8_lossy(&bytes).contains(code));
        }
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        for name in untouched {
            assert_eq!(
                fs::read(dir.path().join(name)).unwrap(),
                b"not a writer stage"
            );
        }
    }

    #[test]
    fn clearing_claims_recovers_pid_stages_without_removing_other_state() {
        let dir = tempfile::tempdir().unwrap();
        let lease = lease(dir.path());
        let path = dir.path().join("pending-claim.json");
        let server = Url::parse("https://koidrive.alice.workers.dev/").unwrap();
        request_id(&lease, &path, &server, "ABCD-EFGH-JKLM").unwrap();
        let stale = dir.path().join(".pending-claim.json.123.tmp");
        fs::write(&stale, fs::read(&path).unwrap()).unwrap();
        let other = dir.path().join("credential.json");
        fs::write(&other, b"another saved record").unwrap();
        let link = dir.path().join(".pending-claim.json.456.tmp");
        std::os::unix::fs::symlink(&other, &link).unwrap();
        let folder = dir.path().join(".pending-claim.json.789.tmp");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("kept"), b"not a stage").unwrap();

        clear(&lease, &path).unwrap();

        assert!(!path.exists());
        assert!(!stale.exists());
        assert!(!link.exists());
        assert_eq!(fs::read(folder.join("kept")).unwrap(), b"not a stage");
        assert_eq!(fs::read(&other).unwrap(), b"another saved record");
        fs::write(&stale, b"abandoned before publication").unwrap();
        clear(&lease, &path).unwrap();
        assert!(!stale.exists());
        clear(&lease, &dir.path().join("missing/pending-claim.json")).unwrap();
    }

    #[test]
    fn pending_status_does_not_recover_stages_or_rewrite_claims() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        fs::create_dir_all(&paths.state_dir).unwrap();
        let lease = lease(&paths.state_dir);
        let server = Url::parse("https://koidrive.alice.workers.dev/").unwrap();
        request_id(&lease, &paths.pending_claim(), &server, "ABCD-EFGH-JKLM").unwrap();
        let before = fs::read(paths.pending_claim()).unwrap();
        let stale = paths.state_dir.join(".pending-claim.json.123.tmp");
        fs::write(&stale, b"unfinished").unwrap();

        assert!(claim_pending(&paths, &server));

        assert_eq!(fs::read(&stale).unwrap(), b"unfinished");
        assert_eq!(fs::read(paths.pending_claim()).unwrap(), before);
        let empty = Paths::under(&dir.path().join("missing"));
        assert!(!claim_pending(&empty, &server));
        assert!(!empty.state_dir.exists());
    }

    #[test]
    fn a_code_the_server_reads_the_same_resumes_the_same_claim() {
        let dir = tempfile::tempdir().unwrap();
        let lease = lease(dir.path());
        let path = dir.path().join("pending-claim.json");
        let server = Url::parse("https://koidrive.alice.workers.dev/").unwrap();

        let first = request_id(&lease, &path, &server, "ABCD-EFGH-JKLM").unwrap();
        assert_eq!(
            request_id(&lease, &path, &server, " abcdefghjklm ").unwrap(),
            first
        );
        let other = request_id(&lease, &path, &server, "ABCD-EFGH-JKLN").unwrap();
        assert_ne!(other, first);

        forget(&lease, &path, &server, "abcd efgh jkln").unwrap();
        assert_eq!(
            request_id(&lease, &path, &server, "abcd-efgh-jklm").unwrap(),
            first
        );
        forget(&lease, &path, &server, "ABCD-EFGH-JKLM").unwrap();
        assert!(!path.exists());
    }
}
