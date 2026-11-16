// SPDX-License-Identifier: Apache-2.0

//! Whether a newer Koidrive release is out. The sync host asks GitHub's release list and keeps
//! the answer in the profile's `update-check.json`. Every reader turns that file into an [`Update`] for the version
//! it runs, with no request of its own. `koi update` asks again at once with [`check_now`].

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::{ACCEPT, HeaderMap, IF_NONE_MATCH};
use reqwest::{StatusCode, redirect};
use semver::Version;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio::task::AbortHandle;
use url::Url;

use crate::config::{Config, UpdateCheck};
use crate::paths::Paths;

pub const VARIABLE: &str = "KOIDRIVE_UPDATE_CHECK";

pub const URL_VARIABLE: &str = "KOIDRIVE_UPDATE_CHECK_URL";

const API: &str = "https://api.github.com/";
const RELEASES: &str = "repos/koidrive/koidrive/releases?per_page=20";
const TIMEOUT: Duration = Duration::from_secs(10);
const PAGE_LIMIT: usize = 10;
const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 24 * HOUR_MS;

/// Whether a newer Koidrive release is out than the version the reporting process runs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Update {
    Available {
        latest: String,
        url: String,
        checked_at_ms: i64,
    },
    UpToDate {
        checked_at_ms: i64,
    },
    /// No check has had an answer from GitHub yet, or the daemon is too old to say.
    #[default]
    Unknown,
    /// `[updates] check = false` in config.toml or [`VARIABLE`] turned the check off.
    Off,
}

/// The release of the device's track: the newest stable release once one exists, else the newest prerelease. A
/// release counts once it lists the tarball for this target.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    pub version: String,
    /// The release's page on GitHub.
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CheckError {
    #[error("GitHub cannot be reached")]
    Unreachable,
    #[error("GitHub did not send the list of Koidrive releases; try again later")]
    Refused,
    #[error("{URL_VARIABLE} is not an https URL or an http URL on a loopback address")]
    Url,
}

/// Whether a version identifies a local development build rather than a release.
pub fn is_development_version(version: &str) -> bool {
    koidrive_telemetry::schema::Version::parse(version)
        .is_ok_and(|version| version.is_development())
}

/// Where `koi update` writes the commands it ran and what they printed: `~/Library/Logs/Koidrive/update.log` on macOS,
/// `update.log` in the state folder elsewhere.
pub fn log_path(paths: &Paths) -> PathBuf {
    let home = (std::env::var_os("HOME").map(PathBuf::from)).filter(|home| home.is_absolute());
    match (cfg!(target_os = "macos"), home) {
        (true, Some(home)) => home.join("Library/Logs/Koidrive/update.log"),
        _ => paths.state_dir.join("update.log"),
    }
}

/// Whether the update check is on for the profile at `paths`: `[updates]` in its config.toml, unless [`VARIABLE`]
/// says otherwise. A config.toml that cannot be read leaves it on.
pub fn is_on(paths: &Paths) -> bool {
    if is_development_version(env!("CARGO_PKG_VERSION")) {
        return false;
    }
    let config = Config::load(&paths.config_file).map(|config| config.updates);
    switched_on(
        &config.unwrap_or_default(),
        std::env::var(VARIABLE).ok().as_deref(),
    )
}

pub(crate) fn switched_on(config: &UpdateCheck, variable: Option<&str>) -> bool {
    match variable
        .map(|value| value.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("0" | "off" | "false") => false,
        Some("1" | "on" | "true") => true,
        _ => config.check,
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Checked {
    #[serde(default)]
    pub asset: Option<String>,
    #[serde(default)]
    pub checked_at: Option<i64>,
    #[serde(default)]
    pub successful_at: Option<i64>,
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub release: Option<Release>,
    #[serde(default)]
    pub retry_after: Option<i64>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Answer {
    Releases {
        etag: Option<String>,
        release: Option<Release>,
    },
    NotModified,
    Refused {
        until: Option<i64>,
    },
    Unreachable,
}

impl Checked {
    pub fn due(&self) -> i64 {
        let day = (self.checked_at).map_or(i64::MIN, |at| at.saturating_add(DAY_MS));
        day.max(self.retry_after.unwrap_or(i64::MIN))
    }

    pub fn after(mut self, answer: Answer, now: i64) -> Checked {
        self.asset = Some(this_asset());
        let successful_at = self
            .successful_at
            .or_else(|| self.release.as_ref().and(self.checked_at));
        match answer {
            Answer::Releases { etag, release } => Checked {
                asset: self.asset,
                checked_at: Some(now),
                successful_at: Some(now),
                etag,
                release,
                retry_after: None,
            },
            Answer::NotModified => Checked {
                checked_at: Some(now),
                successful_at: successful_at.map(|_| now),
                retry_after: None,
                ..self
            },
            Answer::Refused { until } => Checked {
                checked_at: Some(now),
                successful_at,
                retry_after: until.filter(|until| *until > now.saturating_add(DAY_MS)),
                ..self
            },
            Answer::Unreachable => Checked {
                successful_at,
                retry_after: Some(now.saturating_add(HOUR_MS)),
                ..self
            },
        }
    }

    pub fn notice(&self, runs: &Version) -> Update {
        if is_development_version(&runs.to_string()) {
            return Update::Off;
        }
        let Some(checked_at_ms) = self
            .successful_at
            .or_else(|| self.release.as_ref().and(self.checked_at))
        else {
            return Update::Unknown;
        };
        let newer = (self.release.as_ref())
            .and_then(|release| Some((Version::parse(&release.version).ok()?, release)))
            .filter(|(latest, _)| latest.cmp_precedence(runs).is_gt());
        match newer {
            Some((latest, release)) => Update::Available {
                latest: latest.to_string(),
                url: release.url.clone(),
                checked_at_ms,
            },
            None => Update::UpToDate { checked_at_ms },
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct Listed {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
}

pub(crate) fn track(listed: &[Listed], asset: &str) -> Option<Release> {
    let published: Vec<(Version, &Listed)> = (listed.iter())
        .filter(|release| !release.draft)
        .filter_map(|release| {
            let version = Version::parse(release.tag_name.strip_prefix('v')?).ok()?;
            Some((version, release))
        })
        .collect();
    let stable = published.iter().any(|(version, _)| version.pre.is_empty());
    published
        .into_iter()
        .filter(|(version, _)| !stable || version.pre.is_empty())
        .filter(|(_, release)| release.assets.iter().any(|listed| listed.name == asset))
        .max_by(|(a, _), (b, _)| a.cmp_precedence(b))
        .map(|(version, release)| Release {
            version: version.to_string(),
            url: release.html_url.clone(),
        })
}

pub(crate) fn this_asset() -> String {
    let os = match std::env::consts::OS {
        "macos" => "apple-darwin",
        _ => "unknown-linux-musl",
    };
    format!("koidrive-{}-{os}.tar.gz", std::env::consts::ARCH)
}

pub(crate) fn answer(
    status: StatusCode,
    headers: &HeaderMap,
    body: &[u8],
    asset: &str,
    now: i64,
) -> Answer {
    let header = |name: &str| {
        (headers.get(name))
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
    };
    let seconds = |name: &str| header(name).and_then(|value| value.parse::<i64>().ok());
    match status.as_u16() {
        200 => match serde_json::from_slice::<Vec<Listed>>(body) {
            Ok(listed) => Answer::Releases {
                etag: header("etag").map(str::to_owned),
                release: track(&listed, asset),
            },
            Err(error) => {
                tracing::warn!(%error, "GitHub's list of Koidrive releases cannot be read");
                Answer::Refused { until: None }
            }
        },
        304 => Answer::NotModified,
        403 | 429 => {
            let retry =
                seconds("retry-after").map(|after| now.saturating_add(after.saturating_mul(1000)));
            let reset = (header("x-ratelimit-remaining") == Some("0"))
                .then(|| seconds("x-ratelimit-reset"))
                .flatten()
                .map(|reset| reset.saturating_mul(1000));
            Answer::Refused {
                until: retry.max(reset),
            }
        }
        _ => Answer::Refused { until: None },
    }
}

pub(crate) fn releases_url(base: Option<&str>) -> Option<(Url, bool)> {
    let mut base = Url::parse(base.unwrap_or(API)).ok()?;
    if !base.username().is_empty() || base.password().is_some() {
        return None;
    }
    if !base.path().ends_with('/') {
        base.path_segments_mut().ok()?.push("");
    }
    let loopback = matches!(base.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    let https = match base.scheme() {
        "https" => true,
        "http" if loopback => false,
        _ => return None,
    };
    Some((base.join(RELEASES).ok()?, https))
}

pub(crate) async fn ask(
    http: &reqwest::Client,
    url: Url,
    etag: Option<&str>,
    asset: &str,
) -> Answer {
    tokio::time::timeout(TIMEOUT, async {
        let mut page = url.clone();
        let mut conditional = etag;
        let mut visited = HashSet::new();
        let mut listed = Vec::new();
        let mut single_etag = None;
        let mut requests = 0;
        loop {
            if requests == PAGE_LIMIT {
                return Answer::Refused { until: None };
            }
            requests += 1;
            if !visited.insert(page.clone()) {
                return Answer::Refused { until: None };
            }
            let mut request = (http.get(page.clone()))
                .header(ACCEPT, "application/vnd.github+json")
                .header("x-github-api-version", "2022-11-28");
            if let Some(etag) = conditional {
                request = request.header(IF_NONE_MATCH, etag);
            }
            let response = match request.send().await {
                Ok(response) => response,
                Err(_) if visited.len() == 1 => return Answer::Unreachable,
                Err(_) => return Answer::Refused { until: None },
            };
            let (status, headers) = (response.status(), response.headers().clone());
            let next = match next_page(&url, &headers) {
                Ok(next) => next,
                Err(()) => return Answer::Refused { until: None },
            };
            if status == StatusCode::NOT_MODIFIED {
                if visited.len() != 1 || conditional.is_none() {
                    return Answer::Refused { until: None };
                }
                if next.is_none() {
                    return Answer::NotModified;
                }
                conditional = None;
                visited.clear();
                continue;
            }
            if status != StatusCode::OK {
                return answer(status, &headers, &[], asset, now_ms());
            }
            let body = match response.bytes().await {
                Ok(body) => body,
                Err(_) => return Answer::Refused { until: None },
            };
            let Ok(mut releases) = serde_json::from_slice::<Vec<Listed>>(&body) else {
                return Answer::Refused { until: None };
            };
            listed.append(&mut releases);
            if visited.len() == 1 && next.is_none() {
                single_etag = headers
                    .get("etag")
                    .and_then(|value| value.to_str().ok())
                    .map(|value| value.trim().to_owned());
            }
            let Some(next) = next else {
                return Answer::Releases {
                    etag: single_etag,
                    release: track(&listed, asset),
                };
            };
            page = next;
            conditional = None;
        }
    })
    .await
    .unwrap_or(Answer::Refused { until: None })
}

fn next_page(endpoint: &Url, headers: &HeaderMap) -> Result<Option<Url>, ()> {
    let mut next = None;
    for header in headers.get_all("link") {
        for link in header.to_str().map_err(|_| ())?.split(',') {
            let mut parts = link.split(';');
            let target = parts.next().ok_or(())?.trim();
            let is_next = parts.any(|part| {
                part.trim().split_once('=').is_some_and(|(key, value)| {
                    key.trim() == "rel"
                        && value
                            .trim()
                            .trim_matches('"')
                            .split_whitespace()
                            .any(|rel| rel == "next")
                })
            });
            if !is_next {
                continue;
            }
            let target = target
                .strip_prefix('<')
                .and_then(|target| target.strip_suffix('>'))
                .ok_or(())?;
            let url = endpoint.join(target).map_err(|_| ())?;
            if url.origin() != endpoint.origin()
                || url.path() != endpoint.path()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.fragment().is_some()
                || next.is_some()
            {
                return Err(());
            }
            next = Some(url);
        }
    }
    Ok(next)
}

pub(crate) fn client(https_only: bool) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent("koidrive")
        .https_only(https_only)
        .redirect(redirect::Policy::none())
        .timeout(TIMEOUT)
        .build()
}

fn endpoint() -> Result<(reqwest::Client, Url), CheckError> {
    let base = std::env::var(URL_VARIABLE).ok();
    let (url, https) = releases_url(base.as_deref()).ok_or(CheckError::Url)?;
    let http = client(https).map_err(|_| CheckError::Unreachable)?;
    Ok((http, url))
}

/// Asks GitHub for the track's release now, whatever the time of the last check, with the request of the daily check.
/// A list that GitHub answers is unchanged since the profile's last check gives that check's release. Nothing is
/// saved.
pub async fn check_now(paths: &Paths) -> Result<Option<Release>, CheckError> {
    if is_development_version(env!("CARGO_PKG_VERSION")) {
        return Ok(None);
    }
    let (http, url) = endpoint()?;
    let checked = load(&paths.update_check()).unwrap_or_default();
    match ask(&http, url, checked.etag.as_deref(), &this_asset()).await {
        Answer::Releases { release, .. } => Ok(release),
        Answer::NotModified => Ok(checked.release),
        Answer::Refused { .. } => Err(CheckError::Refused),
        Answer::Unreachable => Err(CheckError::Unreachable),
    }
}

pub(crate) fn load(path: &Path) -> Option<Checked> {
    let checked: Checked = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    (checked.asset.as_deref() == Some(this_asset().as_str())).then_some(checked)
}

fn save(path: &Path, checked: &Checked) -> io::Result<()> {
    koidrive_state_file::write(path, &serde_json::to_vec(checked)?)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

fn this_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("the crate's version is SemVer")
}

pub(crate) struct Updates {
    on: bool,
    version: Version,
    path: PathBuf,
    checked: Mutex<Option<Checked>>,
    task: Mutex<Option<AbortHandle>>,
    asking: tokio::sync::Mutex<Option<Result<Update, CheckError>>>,
    answers: AtomicU64,
}

impl Updates {
    pub fn open(on: bool, path: PathBuf) -> Updates {
        Self::for_version(on, path, this_version())
    }

    pub(super) fn for_version(on: bool, path: PathBuf, version: Version) -> Updates {
        let on = on && !is_development_version(&version.to_string());
        let checked = on.then(|| load(&path)).flatten();
        Updates {
            on,
            version,
            path,
            checked: Mutex::new(checked),
            task: Mutex::new(None),
            asking: tokio::sync::Mutex::new(None),
            answers: AtomicU64::new(0),
        }
    }

    pub fn notice(&self) -> Update {
        match (self.on, self.checked.lock().unwrap().as_ref()) {
            (false, _) => Update::Off,
            (true, None) => Update::Unknown,
            (true, Some(checked)) => checked.notice(&self.version),
        }
    }

    pub fn start(self: &Arc<Self>, changed: Arc<watch::Sender<()>>) {
        if !self.on {
            match fs::remove_file(&self.path) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => {
                    tracing::warn!(%error, "the update check's file cannot be removed");
                }
                _ => {}
            }
            return;
        }
        let mut task = self.task.lock().unwrap();
        if task.as_ref().is_some_and(|task| !task.is_finished()) {
            return;
        }
        let (http, url) = match endpoint() {
            Ok(endpoint) => endpoint,
            Err(error) => {
                tracing::warn!(%error, "the update check cannot start");
                return;
            }
        };
        let updates = self.clone();
        *task = Some(tokio::spawn(updates.keep_checking(http, url, changed)).abort_handle());
    }

    pub fn stop(&self) {
        if let Some(task) = self.task.lock().unwrap().take() {
            task.abort();
        }
    }

    pub fn is_running(&self) -> bool {
        self.task
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|task| !task.is_finished())
    }

    async fn keep_checking(
        self: Arc<Self>,
        http: reqwest::Client,
        url: Url,
        changed: Arc<watch::Sender<()>>,
    ) {
        loop {
            let checked = self.checked.lock().unwrap().clone().unwrap_or_default();
            let wait = checked.due().saturating_sub(now_ms());
            if wait > 0 {
                tokio::time::sleep(Duration::from_millis(wait.min(HOUR_MS) as u64)).await;
                continue;
            }
            let _ = self.ask_once(&http, url.clone(), &changed).await;
        }
    }

    pub async fn check_now(&self, changed: &watch::Sender<()>) -> Result<Update, CheckError> {
        if is_development_version(&self.version.to_string()) {
            return Ok(Update::Off);
        }
        let (http, url) = endpoint()?;
        self.ask_once(&http, url, changed).await
    }

    async fn ask_once(
        &self,
        http: &reqwest::Client,
        url: Url,
        changed: &watch::Sender<()>,
    ) -> Result<Update, CheckError> {
        let answers = self.answers.load(Ordering::SeqCst);
        let mut last = self.asking.lock().await;
        if let Some(last) = last.as_ref()
            && self.answers.load(Ordering::SeqCst) != answers
        {
            return last.clone();
        }
        let checked = match self.on {
            true => self.checked.lock().unwrap().clone().unwrap_or_default(),
            false => Checked::default(),
        };
        let answer = ask(http, url, checked.etag.as_deref(), &this_asset()).await;
        let failure = match answer {
            Answer::Unreachable => Some(CheckError::Unreachable),
            Answer::Refused { .. } => Some(CheckError::Refused),
            Answer::Releases { .. } | Answer::NotModified => None,
        };
        let checked = checked.after(answer, now_ms());
        if self.on {
            if let Err(error) = save(&self.path, &checked) {
                tracing::warn!(%error, "the update check's answer cannot be saved");
            }
            *self.checked.lock().unwrap() = Some(checked.clone());
            changed.send_replace(());
        }
        let result = failure.map_or_else(|| Ok(checked.notice(&self.version)), Err);
        *last = Some(result.clone());
        self.answers.fetch_add(1, Ordering::SeqCst);
        result
    }
}

#[cfg(test)]
mod tests {
    use reqwest::header::HeaderValue;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    const ASSET: &str = "koidrive-aarch64-apple-darwin.tar.gz";

    fn listed(tag: &str, draft: bool, assets: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "tag_name": tag,
            "html_url": format!("https://github.com/koidrive/koidrive/releases/tag/{tag}"),
            "draft": draft,
            "prerelease": tag.contains('-'),
            "assets": assets.iter().map(|name| serde_json::json!({ "name": name })).collect::<Vec<_>>(),
        })
    }

    fn picked(releases: &[serde_json::Value]) -> Option<String> {
        let listed: Vec<Listed> =
            serde_json::from_value(serde_json::Value::from(releases.to_vec())).unwrap();
        track(&listed, ASSET).map(|release| release.version)
    }

    #[test]
    fn the_track_is_the_newest_stable_release_once_one_exists_else_the_newest_prerelease() {
        let full = [ASSET, "SHA256SUMS"];
        let alphas = [
            listed("v0.1.0-alpha.2", false, &full),
            listed("v0.1.0-alpha.10", false, &full),
            listed("v0.1.0-alpha.9", false, &full),
        ];
        assert_eq!(picked(&alphas).as_deref(), Some("0.1.0-alpha.10"));

        let mixed = [
            listed("v0.2.0-alpha.1", false, &full),
            listed("v0.1.0", false, &full),
            listed("v0.1.0-rc.1", false, &full),
        ];
        assert_eq!(picked(&mixed).as_deref(), Some("0.1.0"));

        let backport = [
            listed("v0.1.3", false, &full),
            listed("v0.2.0", false, &full),
            listed("v0.1.2", false, &full),
        ];
        assert_eq!(picked(&backport).as_deref(), Some("0.2.0"));
    }

    #[test]
    fn drafts_tags_that_do_not_parse_and_releases_without_this_targets_tarball_do_not_count() {
        let full = [ASSET];
        assert_eq!(
            picked(&[
                listed("v0.3.0", true, &full),
                listed("0.4.0", false, &full),
                listed("v0.5", false, &full),
                listed("nightly", false, &full),
                listed(
                    "v0.2.0",
                    false,
                    &["koidrive-x86_64-unknown-linux-musl.tar.gz"]
                ),
                listed("v0.1.1", false, &full),
            ])
            .as_deref(),
            Some("0.1.1")
        );
        assert_eq!(
            picked(&[
                listed("v0.1.0", false, &["SHA256SUMS"]),
                listed("v0.1.0-alpha.3", false, &full),
            ]),
            None,
            "a stable release whose build is missing does not send the device back to prereleases"
        );
        assert_eq!(picked(&[listed("v0.1.0", true, &full)]), None);
        assert_eq!(picked(&[]), None);
    }

    fn release(version: &str) -> Option<Release> {
        Some(Release {
            version: version.to_owned(),
            url: format!("https://github.com/koidrive/koidrive/releases/tag/v{version}"),
        })
    }

    fn release_updates(on: bool, path: PathBuf) -> Updates {
        Updates::for_version(on, path, Version::parse("0.1.0").unwrap())
    }

    #[tokio::test]
    async fn development_builds_disable_checks_even_when_the_switch_is_on() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update-check.json");
        for version in ["0.0.0-dev", "0.0.0-dev+abc123"] {
            let cached = Checked {
                asset: Some(this_asset()),
                checked_at: Some(1_000),
                release: release("99.0.0"),
                ..Checked::default()
            };
            save(&path, &cached).unwrap();
            let updates = Arc::new(Updates::for_version(
                true,
                path.clone(),
                Version::parse(version).unwrap(),
            ));
            assert_eq!(updates.notice(), Update::Off);
            updates.start(Arc::new(watch::Sender::new(())));
            assert!(!updates.is_running());
            assert!(!path.exists());
        }
    }

    #[test]
    fn development_versions_never_offer_a_cached_release() {
        let checked = Checked {
            checked_at: Some(1_000),
            release: release("99.0.0"),
            ..Checked::default()
        };
        for version in ["0.0.0-dev", "0.0.0-dev+abc123"] {
            assert_eq!(
                checked.notice(&Version::parse(version).unwrap()),
                Update::Off
            );
            assert_eq!(
                Checked::default().notice(&Version::parse(version).unwrap()),
                Update::Off
            );
        }
    }

    #[test]
    fn a_device_is_told_only_about_a_release_above_the_version_it_runs() {
        let checked = |version: &str| Checked {
            checked_at: Some(1_000),
            release: release(version),
            ..Checked::default()
        };
        let runs = |version: &str| Version::parse(version).unwrap();
        let notice = |cached: &str, running: &str| checked(cached).notice(&runs(running));
        assert_eq!(
            notice("0.1.2", "0.1.0"),
            Update::Available {
                latest: "0.1.2".into(),
                url: "https://github.com/koidrive/koidrive/releases/tag/v0.1.2".into(),
                checked_at_ms: 1_000,
            }
        );
        assert_eq!(
            notice("0.1.0", "0.1.0"),
            Update::UpToDate {
                checked_at_ms: 1_000
            }
        );
        assert_eq!(
            notice("0.1.0", "0.2.0-alpha.1"),
            Update::UpToDate {
                checked_at_ms: 1_000
            }
        );
        assert!(matches!(
            notice("0.1.0", "0.1.0-alpha.4"),
            Update::Available { .. }
        ));
        assert!(matches!(
            notice("0.2.0-alpha.2", "0.2.0-alpha.1"),
            Update::Available { .. }
        ));
        assert_eq!(Checked::default().notice(&runs("0.1.0")), Update::Unknown);
        let unanswered = Checked::default().after(Answer::Unreachable, 5);
        assert_eq!(unanswered.notice(&runs("0.1.0")), Update::Unknown);
    }

    #[test]
    fn a_check_is_due_a_day_after_githubs_last_answer_an_hour_after_no_answer_and_not_before_a_rate_limit_resets()
     {
        let now = 10 * DAY_MS;
        let first = Checked::default();
        assert!(
            first.due() <= now,
            "a profile that never checked checks at once"
        );

        let answered = first.clone().after(
            Answer::Releases {
                etag: Some("\"e1\"".into()),
                release: release("0.1.2"),
            },
            now,
        );
        assert_eq!(answered.due(), now + DAY_MS);
        assert_eq!(answered.etag.as_deref(), Some("\"e1\""));

        let later = now + DAY_MS;
        let same = answered.clone().after(Answer::NotModified, later);
        assert_eq!(
            (same.checked_at, same.release.clone(), same.etag.clone()),
            (Some(later), release("0.1.2"), Some("\"e1\"".into()))
        );
        assert_eq!(same.due(), later + DAY_MS);

        let offline = answered.clone().after(Answer::Unreachable, later);
        assert_eq!(offline.checked_at, Some(now), "no answer is no day's check");
        assert_eq!(
            offline.release,
            release("0.1.2"),
            "the last good answer stays"
        );
        assert_eq!(offline.due(), later + HOUR_MS);
        let back = offline.after(Answer::NotModified, later + HOUR_MS);
        assert_eq!(back.due(), later + HOUR_MS + DAY_MS);

        let refused = answered
            .clone()
            .after(Answer::Refused { until: None }, later);
        assert_eq!(
            (refused.due(), refused.release.clone()),
            (later + DAY_MS, release("0.1.2"))
        );

        let soon = answered.clone().after(
            Answer::Refused {
                until: Some(later + HOUR_MS),
            },
            later,
        );
        assert_eq!(
            soon.due(),
            later + DAY_MS,
            "a rate limit waits at least a day"
        );
        let reset = later + DAY_MS + 5 * HOUR_MS;
        let limited = answered.after(Answer::Refused { until: Some(reset) }, later);
        assert_eq!(limited.due(), reset);
        assert_eq!(
            limited.after(Answer::NotModified, reset).due(),
            reset + DAY_MS
        );
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(*name, HeaderValue::from_str(value).unwrap());
        }
        headers
    }

    #[test]
    fn failed_checks_back_off_without_claiming_a_successful_check() {
        let runs = Version::parse("0.1.0").unwrap();
        for (status, body) in [(500, b"".as_slice()), (403, b""), (200, b"not json")] {
            let failure = || {
                answer(
                    StatusCode::from_u16(status).unwrap(),
                    &HeaderMap::new(),
                    body,
                    ASSET,
                    100,
                )
            };
            let failed = Checked::default().after(failure(), 100);
            assert_eq!(failed.notice(&runs), Update::Unknown, "{status}");
            assert_eq!(failed.due(), 100 + DAY_MS);
            for (version, expected) in [
                (None, Update::UpToDate { checked_at_ms: 200 }),
                (release("0.1.0"), Update::UpToDate { checked_at_ms: 200 }),
                (
                    release("0.1.2"),
                    Update::Available {
                        latest: "0.1.2".into(),
                        url: "https://github.com/koidrive/koidrive/releases/tag/v0.1.2".into(),
                        checked_at_ms: 200,
                    },
                ),
            ] {
                let success = failed.clone().after(
                    Answer::Releases {
                        etag: Some("e1".into()),
                        release: version,
                    },
                    200,
                );
                assert_eq!(success.notice(&runs), expected);
                let failed_again = success.after(failure(), 300);
                assert_eq!(failed_again.notice(&runs), expected);
                assert_eq!(failed_again.due(), 300 + DAY_MS);
            }
        }
    }

    #[test]
    fn not_modified_without_a_successful_cache_does_not_claim_up_to_date() {
        let runs = Version::parse("0.1.0").unwrap();
        let checked = Checked::default().after(Answer::NotModified, 100);
        assert_eq!(checked.notice(&runs), Update::Unknown);
        assert_eq!(checked.due(), 100 + DAY_MS);
    }

    #[test]
    fn legacy_caches_with_a_release_keep_their_success_time_after_failure() {
        let runs = Version::parse("0.1.0").unwrap();
        let legacy: Checked = serde_json::from_str(
            r#"{"checked_at":100,"release":{"version":"0.1.0","url":"https://example.com"}}"#,
        )
        .unwrap();
        let failed = legacy.after(Answer::Refused { until: None }, 200);
        assert_eq!(
            failed.notice(&runs),
            Update::UpToDate { checked_at_ms: 100 }
        );
        let restored: Checked =
            serde_json::from_slice(&serde_json::to_vec(&failed).unwrap()).unwrap();
        assert_eq!(
            restored.notice(&runs),
            Update::UpToDate { checked_at_ms: 100 }
        );
        let same = restored.after(Answer::NotModified, 300);
        assert_eq!(same.notice(&runs), Update::UpToDate { checked_at_ms: 300 });
        let empty: Checked = serde_json::from_str(r#"{"checked_at":100,"release":null}"#).unwrap();
        assert_eq!(empty.notice(&runs), Update::Unknown);
        let answered = empty.after(
            Answer::Releases {
                etag: None,
                release: None,
            },
            400,
        );
        let restored: Checked =
            serde_json::from_slice(&serde_json::to_vec(&answered).unwrap()).unwrap();
        assert_eq!(
            restored.after(Answer::NotModified, 500).notice(&runs),
            Update::UpToDate { checked_at_ms: 500 }
        );
    }

    #[test]
    fn every_answer_from_github_is_the_days_check_and_a_rate_limit_waits_for_its_reset() {
        let now = 1_000_000;
        let body = serde_json::to_vec(&[listed("v0.1.2", false, &[ASSET])]).unwrap();
        let answered = |status: u16, pairs: &[(&'static str, &str)], body: &[u8]| {
            answer(
                StatusCode::from_u16(status).unwrap(),
                &headers(pairs),
                body,
                ASSET,
                now,
            )
        };
        assert_eq!(
            answered(200, &[("etag", "W/\"abc\"")], &body),
            Answer::Releases {
                etag: Some("W/\"abc\"".into()),
                release: release("0.1.2"),
            }
        );
        assert_eq!(answered(304, &[], b""), Answer::NotModified);
        assert_eq!(
            answered(200, &[], b"<html>"),
            Answer::Refused { until: None }
        );
        assert_eq!(
            answered(
                403,
                &[
                    ("x-ratelimit-remaining", "0"),
                    ("x-ratelimit-reset", "5000")
                ],
                b""
            ),
            Answer::Refused {
                until: Some(5_000_000)
            }
        );
        assert_eq!(
            answered(
                403,
                &[
                    ("x-ratelimit-remaining", "12"),
                    ("x-ratelimit-reset", "5000")
                ],
                b""
            ),
            Answer::Refused { until: None },
            "a 403 with requests left is not a rate limit"
        );
        assert_eq!(
            answered(429, &[("retry-after", "60")], b""),
            Answer::Refused {
                until: Some(now + 60_000)
            }
        );
        for status in [301, 404, 500, 502] {
            assert_eq!(
                answered(status, &[], b""),
                Answer::Refused { until: None },
                "{status}"
            );
        }
    }

    #[test]
    fn only_zero_off_or_false_in_the_variable_turns_the_check_off_and_it_wins_over_the_file() {
        let on = UpdateCheck { check: true };
        let off = UpdateCheck { check: false };
        assert!(switched_on(&on, None));
        assert!(!switched_on(&off, None));
        for value in ["0", "off", "OFF", "false", " 0 "] {
            assert!(!switched_on(&on, Some(value)), "{value}");
        }
        for value in ["1", "on", "true"] {
            assert!(switched_on(&off, Some(value)), "{value}");
        }
        assert!(switched_on(&on, Some("")));
        assert!(!switched_on(&off, Some("yes")));
    }

    #[test]
    fn the_api_base_is_https_or_http_on_a_loopback_address() {
        let (url, https) = releases_url(None).unwrap();
        assert_eq!(
            url.as_str(),
            "https://api.github.com/repos/koidrive/koidrive/releases?per_page=20"
        );
        assert!(https);
        let (url, https) = releases_url(Some("http://127.0.0.1:8080/api")).unwrap();
        assert_eq!(
            url.as_str(),
            "http://127.0.0.1:8080/api/repos/koidrive/koidrive/releases?per_page=20"
        );
        assert!(!https);
        assert_eq!(releases_url(Some("http://example.com")), None);
        assert_eq!(releases_url(Some("file:///tmp")), None);
    }

    async fn served(listener: &TcpListener, response: &str) -> String {
        let (headers, body) = response.split_once("\r\n\r\n").unwrap();
        let response = format!("{headers}\r\nconnection: close\r\n\r\n{body}");
        let (mut stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 4096];
        while !request.ends_with(b"\r\n\r\n") {
            let read = stream.read(&mut buffer).await.unwrap();
            assert!(read > 0, "the request ended early");
            request.extend_from_slice(&buffer[..read]);
        }
        stream.write_all(response.as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();
        String::from_utf8(request).unwrap()
    }

    #[tokio::test]
    async fn the_single_response_fixture_declares_connection_close_and_preserves_its_body() {
        for (status, body) in [
            ("200 OK", "first\r\n\r\nlast"),
            ("304 Not Modified", ""),
            ("500 Error", ""),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            );
            let receive = async {
                let mut stream = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
                    .await
                    .unwrap();
                stream
                    .write_all(b"GET / HTTP/1.1\r\nhost: fixture\r\n\r\n")
                    .await
                    .unwrap();
                let mut wire = Vec::new();
                tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut wire))
                    .await
                    .unwrap()
                    .unwrap();
                String::from_utf8(wire).unwrap()
            };
            let (request, wire) = tokio::join!(served(&listener, &response), receive);
            assert_eq!(request, "GET / HTTP/1.1\r\nhost: fixture\r\n\r\n");
            let (headers, received) = wire.split_once("\r\n\r\n").unwrap();
            assert!(
                headers
                    .lines()
                    .any(|line| line.eq_ignore_ascii_case("connection: close")),
                "{headers}"
            );
            assert_eq!(received, body);
        }
    }

    fn header_lines(request: &str) -> Vec<String> {
        let mut lines: Vec<String> = (request.lines().skip(1))
            .filter(|line| !line.is_empty())
            .map(|line| line.to_ascii_lowercase())
            .collect();
        lines.sort();
        lines
    }

    fn page(body: serde_json::Value, next: Option<&str>, etag: &str) -> String {
        let body = body.to_string();
        let link = next.map_or(String::new(), |next| {
            format!("link: <{next}>; rel=\"next\"\r\n")
        });
        format!(
            "HTTP/1.1 200 OK\r\netag: {etag}\r\n{link}content-length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    #[tokio::test]
    async fn pagination_selects_the_track_and_highest_version_across_every_page() {
        for (first, second, expected) in [
            ("v0.2.0-alpha.1", "v0.1.0", "0.1.0"),
            ("v0.1.3", "v0.2.0", "0.2.0"),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}/", listener.local_addr().unwrap());
            let (url, https) = releases_url(Some(&base)).unwrap();
            let next = format!("{}&page=2", url);
            let first = page(
                serde_json::json!([listed(first, false, &[ASSET])]),
                Some(&next),
                "\"first\"",
            );
            let second = page(
                serde_json::json!([listed(second, false, &[ASSET])]),
                None,
                "\"second\"",
            );
            let server = async {
                served(&listener, &first).await;
                served(&listener, &second).await
            };
            let http = client(https).unwrap();
            let (_, answer) = tokio::join!(server, ask(&http, url, None, ASSET));
            assert_eq!(
                answer,
                Answer::Releases {
                    etag: None,
                    release: release(expected)
                }
            );
        }
    }

    #[tokio::test]
    async fn a_failed_later_page_keeps_the_last_good_notice_and_waits_a_day() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let (url, https) = releases_url(Some(&base)).unwrap();
        let first = page(
            serde_json::json!([listed("v99.0.0", false, &[ASSET])]),
            Some(&format!("{}&page=2", url)),
            "\"first\"",
        );
        let server = async {
            served(&listener, &first).await;
            served(&listener, "HTTP/1.1 500 Error\r\ncontent-length: 0\r\n\r\n").await;
        };
        let http = client(https).unwrap();
        let (_, answer) = tokio::join!(server, ask(&http, url, None, ASSET));
        assert_eq!(answer, Answer::Refused { until: None });
        let runs = Version::parse("0.1.0").unwrap();
        for previous in [
            Checked::default(),
            Checked {
                checked_at: Some(100),
                successful_at: Some(100),
                release: release("0.1.2"),
                ..Checked::default()
            },
        ] {
            let notice = previous.notice(&runs);
            let failed = previous.after(Answer::Refused { until: None }, 200);
            assert_eq!(failed.notice(&runs), notice);
            assert_eq!(failed.due(), 200 + DAY_MS);
        }
    }

    #[tokio::test]
    async fn multipage_lists_do_not_cache_page_one_etags_or_hide_later_changes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let (url, https) = releases_url(Some(&base)).unwrap();
        let http = client(https).unwrap();
        let next = format!("{}&page=2", url);
        let first = page(
            serde_json::json!([listed("v0.1.0", false, &[ASSET])]),
            Some(&next),
            "\"first\"",
        );
        let mut checked = Checked {
            etag: Some("\"old-single-page\"".into()),
            ..Checked::default()
        };
        for version in ["0.2.0", "0.3.0"] {
            let second = page(
                serde_json::json!([listed(&format!("v{version}"), false, &[ASSET])]),
                None,
                "\"second\"",
            );
            let cached = checked.etag.clone();
            let server = async {
                let request = served(&listener, &first).await;
                assert_eq!(
                    request.to_ascii_lowercase().contains("if-none-match:"),
                    cached.is_some()
                );
                let request = served(&listener, &second).await;
                assert!(!request.to_ascii_lowercase().contains("if-none-match:"));
            };
            let (_, answer) = tokio::join!(
                server,
                ask(&http, url.clone(), checked.etag.as_deref(), ASSET)
            );
            checked = checked.after(answer, 100);
            assert_eq!(checked.release, release(version));
            assert_eq!(checked.etag, None);
        }
    }

    #[tokio::test]
    async fn a_not_modified_page_with_a_new_next_link_fetches_the_whole_list() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let (url, https) = releases_url(Some(&base)).unwrap();
        let next = format!("{}&page=2", url);
        let not_modified = format!(
            "HTTP/1.1 304 Not Modified\r\nlink: <{next}>; rel=\"next\"\r\ncontent-length: 0\r\n\r\n"
        );
        let first = page(
            serde_json::json!([listed("v0.1.0", false, &[ASSET])]),
            Some(&next),
            "\"first\"",
        );
        let second = page(
            serde_json::json!([listed("v0.2.0", false, &[ASSET])]),
            None,
            "\"second\"",
        );
        let server = async {
            served(&listener, &not_modified).await;
            let request = served(&listener, &first).await;
            assert!(!request.to_ascii_lowercase().contains("if-none-match:"));
            served(&listener, &second).await;
        };
        let http = client(https).unwrap();
        let (_, answer) = tokio::join!(server, ask(&http, url, Some("\"first\""), ASSET));
        assert_eq!(
            answer,
            Answer::Releases {
                etag: None,
                release: release("0.2.0")
            }
        );
    }

    #[tokio::test]
    async fn pagination_refuses_links_outside_the_release_endpoint_and_cycles() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let (url, https) = releases_url(Some(&base)).unwrap();
        let http = client(https).unwrap();
        let mut credentials = url.clone();
        credentials.set_username("account").unwrap();
        credentials.set_password(Some("password")).unwrap();
        let mut other_host = url.clone();
        other_host.set_host(Some("localhost")).unwrap();
        let mut other_port = url.clone();
        other_port.set_port(Some(1)).unwrap();
        let mut other_scheme = url.clone();
        other_scheme.set_scheme("https").unwrap();
        for next in [
            credentials,
            other_host,
            other_port,
            other_scheme,
            url.join("/arbitrary").unwrap(),
            url.clone(),
        ] {
            let first = page(
                serde_json::json!([listed("v99.0.0", false, &[ASSET])]),
                Some(next.as_str()),
                "\"first\"",
            );
            let server = async {
                served(&listener, &first).await;
                assert!(
                    tokio::time::timeout(Duration::from_millis(50), listener.accept())
                        .await
                        .is_err(),
                    "followed {next}"
                );
            };
            let (_, answer) = tokio::join!(server, ask(&http, url.clone(), None, ASSET));
            assert_eq!(answer, Answer::Refused { until: None }, "{next}");
        }
    }

    #[test]
    fn next_links_accept_relative_urls_and_relation_lists_but_not_ambiguous_targets() {
        let (url, _) = releases_url(None).unwrap();
        let mut links = headers(&[("link", "<?per_page=20&page=2>; rel=\"next last\"")]);
        let expected = url.join("?per_page=20&page=2").unwrap();
        assert_eq!(next_page(&url, &links), Ok(Some(expected)));
        links.append("link", HeaderValue::from_static("<?page=3>; rel=\"next\""));
        assert_eq!(next_page(&url, &links), Err(()));
        assert_eq!(
            next_page(&url, &headers(&[("link", "not-a-link; rel=next")])),
            Err(())
        );
    }

    #[tokio::test]
    async fn a_release_check_refuses_more_than_ten_page_requests_without_using_a_partial_list() {
        for endless in [true, false] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}/", listener.local_addr().unwrap());
            let (url, https) = releases_url(Some(&base)).unwrap();
            let http = client(https).unwrap();
            let server = async {
                for number in 1..=10 {
                    let next = format!("{}&page={}", url, number + 1);
                    served(
                        &listener,
                        &page(
                            serde_json::json!([listed("v99.0.0", false, &[ASSET])]),
                            (endless || number < 10).then_some(next.as_str()),
                            "\"page\"",
                        ),
                    )
                    .await;
                }
                assert!(
                    tokio::time::timeout(Duration::from_millis(100), listener.accept())
                        .await
                        .is_err(),
                    "a release check sent an eleventh request"
                );
            };
            let (_, answered) = tokio::join!(server, ask(&http, url.clone(), None, ASSET));
            assert_eq!(
                answered,
                if endless {
                    Answer::Refused { until: None }
                } else {
                    Answer::Releases {
                        etag: None,
                        release: release("99.0.0"),
                    }
                }
            );
        }
    }

    #[tokio::test]
    async fn all_pages_and_their_bodies_share_one_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let (url, https) = releases_url(Some(&base)).unwrap();
        let http = client(https).unwrap();
        let first = page(
            serde_json::json!([listed("v99.0.0", false, &[ASSET])]),
            Some(&format!("{}&page=2", url)),
            "\"first\"",
        );
        let server = async {
            tokio::time::sleep(Duration::from_secs(6)).await;
            served(&listener, &first).await;
            let (_socket, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        };
        let started = std::time::Instant::now();
        tokio::pin!(server);
        let answer = tokio::select! {
            _ = &mut server => panic!("the stub must keep page two open"),
            answer = ask(&http, url, None, ASSET) => answer,
        };
        assert_eq!(answer, Answer::Refused { until: None });
        assert!(started.elapsed() < Duration::from_secs(12));
    }

    #[tokio::test]
    async fn the_request_carries_only_the_headers_github_needs_and_the_etag_of_the_last_answer() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let (url, https) = releases_url(Some(&base)).unwrap();
        let http = client(https).unwrap();
        let body = serde_json::to_string(&[listed("v0.1.2", false, &[ASSET])]).unwrap();
        let ok = format!(
            "HTTP/1.1 200 OK\r\netag: \"e1\"\r\nset-cookie: id=1\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        );
        let (request, answered) =
            tokio::join!(served(&listener, &ok), ask(&http, url.clone(), None, ASSET));
        assert!(
            request.starts_with("GET /repos/koidrive/koidrive/releases?per_page=20 HTTP/1.1\r\n"),
            "{request}"
        );
        let host = format!("host: {}", listener.local_addr().unwrap());
        assert_eq!(
            header_lines(&request),
            [
                "accept: application/vnd.github+json".to_owned(),
                host.clone(),
                "user-agent: koidrive".to_owned(),
                "x-github-api-version: 2022-11-28".to_owned(),
            ]
        );
        assert_eq!(
            answered,
            Answer::Releases {
                etag: Some("\"e1\"".into()),
                release: Some(Release {
                    version: "0.1.2".into(),
                    url: "https://github.com/koidrive/koidrive/releases/tag/v0.1.2".into(),
                }),
            }
        );

        let not_modified = "HTTP/1.1 304 Not Modified\r\ncontent-length: 0\r\n\r\n";
        let (request, answered) = tokio::join!(
            served(&listener, not_modified),
            ask(&http, url.clone(), Some("\"e1\""), ASSET)
        );
        assert_eq!(answered, Answer::NotModified);
        assert_eq!(
            header_lines(&request),
            [
                "accept: application/vnd.github+json".to_owned(),
                host,
                "if-none-match: \"e1\"".to_owned(),
                "user-agent: koidrive".to_owned(),
                "x-github-api-version: 2022-11-28".to_owned(),
            ],
            "the cookie of the first answer is not sent back"
        );

        let elsewhere =
            "HTTP/1.1 302 Found\r\nlocation: http://127.0.0.1:1/\r\ncontent-length: 0\r\n\r\n";
        let (_, answered) =
            tokio::join!(served(&listener, elsewhere), ask(&http, url, None, ASSET));
        assert_eq!(
            answered,
            Answer::Refused { until: None },
            "a redirect is not followed"
        );

        drop(listener);
        let (closed, _) = releases_url(Some(&base)).unwrap();
        assert_eq!(ask(&http, closed, None, ASSET).await, Answer::Unreachable);
    }

    #[tokio::test]
    async fn a_check_switched_off_removes_the_last_answer_and_sends_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update-check.json");
        let answered = Checked {
            asset: Some(this_asset()),
            checked_at: Some(1),
            release: release("9.0.0"),
            ..Checked::default()
        };
        save(&path, &answered).unwrap();
        assert_eq!(load(&path), Some(answered));

        let off = Arc::new(release_updates(false, path.clone()));
        assert_eq!(off.notice(), Update::Off);
        off.start(Arc::new(watch::Sender::new(())));
        assert!(!path.exists());
        assert!(off.task.lock().unwrap().is_none());

        save(&path, &Checked::default()).unwrap();
        let on = release_updates(true, path.clone());
        assert_eq!(on.notice(), Update::Unknown);
    }

    fn newest_release_page() -> String {
        let asset = this_asset();
        page(
            serde_json::json!([listed("v0.2.0", false, &[asset.as_str()])]),
            None,
            "\"e1\"",
        )
    }

    async fn stub_github() -> (TcpListener, reqwest::Client, Url) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let (url, https) = releases_url(Some(&base)).unwrap();
        (listener, client(https).unwrap(), url)
    }

    #[tokio::test]
    async fn checking_now_keeps_the_answer_and_a_second_call_while_it_waits_shares_its_request() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update-check.json");
        let (listener, http, url) = stub_github().await;
        let updates = release_updates(true, path.clone());
        let changed = watch::Sender::new(());
        let mut seen = changed.subscribe();
        let newest = newest_release_page();
        let (_, first, second) = tokio::join!(
            served(&listener, &newest),
            updates.ask_once(&http, url.clone(), &changed),
            updates.ask_once(&http, url.clone(), &changed),
        );
        let Ok(available @ Update::Available { .. }) = first.clone() else {
            panic!("{first:?}");
        };
        assert_eq!(second, first);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "the second call sent a request of its own"
        );
        assert!(seen.has_changed().unwrap());
        assert_eq!(updates.notice(), available);
        assert_eq!(load(&path).unwrap().release, release("0.2.0"));

        seen.mark_unchanged();
        let not_modified = "HTTP/1.1 304 Not Modified\r\ncontent-length: 0\r\n\r\n";
        let (request, again) = tokio::join!(
            served(&listener, not_modified),
            updates.ask_once(&http, url, &changed)
        );
        assert!(
            request
                .to_ascii_lowercase()
                .contains("if-none-match: \"e1\""),
            "{request}"
        );
        assert!(
            matches!(&again, Ok(Update::Available { latest, .. }) if latest == "0.2.0"),
            "{again:?}"
        );
        assert!(seen.has_changed().unwrap());
    }

    #[tokio::test]
    async fn checking_now_with_the_check_off_answers_once_and_keeps_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update-check.json");
        let (listener, http, url) = stub_github().await;
        let updates = release_updates(false, path.clone());
        let changed = watch::Sender::new(());
        let seen = changed.subscribe();
        let newest = newest_release_page();
        let (request, answered) = tokio::join!(
            served(&listener, &newest),
            updates.ask_once(&http, url, &changed)
        );
        assert!(!request.to_ascii_lowercase().contains("if-none-match"));
        assert!(
            matches!(&answered, Ok(Update::Available { latest, .. }) if latest == "0.2.0"),
            "{answered:?}"
        );
        assert_eq!(updates.notice(), Update::Off);
        assert!(!path.exists());
        assert!(!seen.has_changed().unwrap());
    }

    #[tokio::test]
    async fn a_failed_check_now_says_why_and_keeps_the_last_answer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update-check.json");
        let cached = Checked {
            asset: Some(this_asset()),
            checked_at: Some(100),
            successful_at: Some(100),
            release: release("0.1.0"),
            ..Checked::default()
        };
        save(&path, &cached).unwrap();
        let (listener, http, url) = stub_github().await;
        let updates = release_updates(true, path.clone());
        let changed = watch::Sender::new(());
        assert_eq!(updates.notice(), Update::UpToDate { checked_at_ms: 100 });
        let refused = "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\n\r\n";
        let (_, answered) = tokio::join!(
            served(&listener, refused),
            updates.ask_once(&http, url.clone(), &changed)
        );
        assert_eq!(answered, Err(CheckError::Refused));
        assert_eq!(updates.notice(), Update::UpToDate { checked_at_ms: 100 });
        drop(listener);
        assert_eq!(
            updates.ask_once(&http, url, &changed).await,
            Err(CheckError::Unreachable)
        );
        assert_eq!(updates.notice(), Update::UpToDate { checked_at_ms: 100 });
    }

    #[tokio::test]
    async fn a_development_build_answers_off_without_asking() {
        let dir = tempfile::tempdir().unwrap();
        let updates = Updates::for_version(
            true,
            dir.path().join("update-check.json"),
            Version::parse("0.0.0-dev").unwrap(),
        );
        assert_eq!(
            updates.check_now(&watch::Sender::new(())).await,
            Ok(Update::Off)
        );
    }

    #[test]
    fn foreign_or_legacy_caches_are_unknown_and_due_for_a_fresh_check() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update-check.json");
        for asset in [Some("koidrive-foreign-target.tar.gz"), None] {
            for release in [None, release("99.0.0")] {
                let mut cached = serde_json::json!({
                    "checked_at": i64::MAX - 1000,
                    "successful_at": 100,
                    "etag": "\"foreign\"",
                    "release": release,
                    "retry_after": i64::MAX - 1000,
                });
                if let Some(asset) = asset {
                    cached["asset"] = asset.into();
                }
                fs::write(&path, cached.to_string()).unwrap();
                assert_eq!(
                    release_updates(true, path.clone()).notice(),
                    Update::Unknown,
                    "{asset:?}"
                );
                let checked = load(&path).unwrap_or_default();
                assert!(checked.due() <= now_ms());
                assert_eq!(checked.etag, None);
                assert_eq!(checked.release, None);
                assert_eq!(checked.successful_at, None);
            }
        }
    }

    #[test]
    fn same_target_caches_keep_the_last_answer_and_conditional_validator() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update-check.json");
        let cached = serde_json::json!({
            "asset": this_asset(),
            "checked_at": i64::MAX - 1000,
            "successful_at": 100,
            "etag": "\"same\"",
            "release": release("99.0.0"),
        });
        fs::write(&path, cached.to_string()).unwrap();
        let expected = Update::Available {
            latest: "99.0.0".into(),
            url: "https://github.com/koidrive/koidrive/releases/tag/v99.0.0".into(),
            checked_at_ms: 100,
        };
        assert_eq!(release_updates(true, path.clone()).notice(), expected);
        let checked = load(&path).unwrap();
        assert!(checked.due() > now_ms());
        assert_eq!(checked.etag.as_deref(), Some("\"same\""));
        assert_eq!(checked.release, release("99.0.0"));
    }

    #[test]
    fn successful_caches_persist_the_current_target_even_without_a_release() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update-check.json");
        for release in [None, release("99.0.0")] {
            let checked = Checked::default().after(
                Answer::Releases {
                    etag: Some("\"same\"".into()),
                    release,
                },
                100,
            );
            save(&path, &checked).unwrap();
            let cached: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            assert_eq!(cached["asset"], this_asset());
            assert_eq!(load(&path), Some(checked));
        }
    }

    #[test]
    fn this_targets_tarball_is_named_like_the_release_assets() {
        let asset = this_asset();
        assert!(
            [
                "koidrive-aarch64-apple-darwin.tar.gz",
                "koidrive-x86_64-apple-darwin.tar.gz",
                "koidrive-aarch64-unknown-linux-musl.tar.gz",
                "koidrive-x86_64-unknown-linux-musl.tar.gz",
            ]
            .contains(&asset.as_str()),
            "{asset}"
        );
    }
}
