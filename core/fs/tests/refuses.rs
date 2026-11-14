// SPDX-License-Identifier: Apache-2.0

mod common;

use common::{SCENARIOS, bind, prepare};
use koidrive_fs::{PRIVATE_DIR, QUARANTINE_DIR, Refusal, STAGING_DIR};
use std::fs;
use std::os::unix::fs::PermissionsExt;

#[test]
fn a_target_that_cannot_be_opened_still_refuses_a_write() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = prepare(&SCENARIOS[1], root);
    let space = bind(root);
    let docs = root.join("docs");
    fs::set_permissions(&docs, fs::Permissions::from_mode(0o555)).unwrap();
    assert_eq!(space.refuses(&effect.action).unwrap(), Some(Refusal::Here));
    fs::set_permissions(&docs, fs::Permissions::from_mode(0o000)).unwrap();
    let refusal = space.refuses(&effect.action).unwrap();
    fs::set_permissions(&docs, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(refusal, Some(Refusal::Here));
}

#[test]
fn a_private_folder_that_cannot_be_opened_is_named_as_refused() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = prepare(&SCENARIOS[1], root);
    let space = bind(root);
    let staging = root.join(PRIVATE_DIR).join(STAGING_DIR);
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o000)).unwrap();
    let refusal = space.refuses(&effect.action).unwrap();
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        refusal,
        Some(Refusal::Private(
            std::path::Path::new(PRIVATE_DIR).join(STAGING_DIR)
        ))
    );
}

#[test]
fn checking_refusal_does_not_create_quarantine() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = prepare(&SCENARIOS[0], root);
    let space = bind(root);
    let quarantine = root.join(PRIVATE_DIR).join(QUARANTINE_DIR);
    assert!(!quarantine.exists());
    assert_eq!(space.refuses(&effect.action).unwrap(), None);
    assert!(
        !quarantine.exists(),
        "a read-only preflight created quarantine"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn a_delete_acl_that_denies_replacement_is_a_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = prepare(&SCENARIOS[0], root);
    let space = bind(root);
    let path = root.join("docs/report.txt");
    assert!(
        std::process::Command::new("chmod")
            .args(["+a", "everyone deny delete"])
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    let refusal = space.refuses(&effect.action);
    let applied = common::apply_one(&space, &effect, &mut |_| {});
    assert!(
        std::process::Command::new("chmod")
            .arg("-N")
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    eprintln!("preflight={refusal:?}; apply={applied:?}");
    assert!(matches!(
        applied,
        Ok(koidrive_fs::Outcome::Failed {
            reason: koidrive_fs::Reason::Denied,
            ..
        })
    ));
    assert_eq!(refusal.unwrap(), Some(Refusal::Here));
}
