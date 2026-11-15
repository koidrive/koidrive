use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime};

use koidrive_driver::Paths;
use koidrive_driver::update;
use semver::Version;
use tokio::process::Command;

use crate::service;

use super::inventory::{Installs, homebrew_link};
use super::release_source::RECONCILE;

const SCAN: Duration = Duration::from_secs(60 * 60);

pub(super) fn scan(
    seen: &mut HashMap<PathBuf, (SystemTime, Option<Version>)>,
    candidates: &[PathBuf],
    runs: &Version,
) -> Option<PathBuf> {
    if update::is_development_version(&runs.to_string()) {
        return None;
    }
    (candidates.iter())
        .filter_map(|koi| {
            let Ok(modified) = fs::metadata(koi).and_then(|metadata| metadata.modified()) else {
                seen.remove(koi);
                return None;
            };
            let cached = seen
                .entry(koi.clone())
                .or_insert_with(|| (modified, service::version_of(koi)));
            if cached.0 != modified {
                *cached = (modified, service::version_of(koi));
            }
            Some((cached.1.clone()?, koi))
        })
        .filter(|(version, _)| version.cmp_precedence(runs).is_gt())
        .max_by(|(a, _), (b, _)| a.cmp_precedence(b))
        .map(|(_, koi)| koi.clone())
}

fn take_service(koi: &Path) {
    let mut command = Command::new(koi);
    command
        .arg("_reconcile")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            tracing::warn!(koi = %koi.display(), %error, "a newer koi is installed, but it did not run");
            return;
        }
    };
    tracing::info!(koi = %koi.display(), "a newer koi is installed; it takes the background service");
    let koi = koi.to_path_buf();
    tokio::spawn(async move {
        match tokio::time::timeout(RECONCILE, child.wait()).await {
            Ok(Ok(status)) if status.success() => {}
            Ok(Ok(status)) => {
                tracing::warn!(koi = %koi.display(), %status, "the newer koi did not take the background service")
            }
            Ok(Err(error)) => {
                tracing::warn!(koi = %koi.display(), %error, "the newer koi did not take the background service")
            }
            Err(_) => {
                tracing::warn!(koi = %koi.display(), "the newer koi did not take the background service within seven minutes")
            }
        }
    });
}

pub async fn follow_newest(paths: Paths) {
    if update::is_development_version(env!("CARGO_PKG_VERSION")) {
        return;
    }
    let mut seen = HashMap::new();
    let mut every = tokio::time::interval(SCAN);
    loop {
        every.tick().await;
        let paths = paths.clone();
        let looked = tokio::task::spawn_blocking(move || {
            let manager = service::system();
            let installed = (service::this_service(&paths).ok())
                .and_then(|service| manager.installed(&service))
                .map(PathBuf::from);
            let newer = installed.and_then(|installed| {
                let mut candidates = Installs::of_this_machine().all();
                let mut running = vec![installed];
                running.extend(std::env::current_exe().ok());
                candidates.extend(running.iter().filter_map(|koi| homebrew_link(koi)));
                candidates.extend(running);
                scan(&mut seen, &candidates, &service::this_version())
            });
            (seen, newer)
        })
        .await;
        let Ok((kept, newer)) = looked else { return };
        seen = kept;
        if let Some(koi) = newer {
            take_service(&koi);
        }
    }
}
