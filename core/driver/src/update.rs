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
const RELEASES: &str = "repos/koidrive/koidrive/releases";
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
    #[serde(default)]
    pub retry_set_at: Option<i64>,
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
    pub fn due(&self, now: i64) -> i64 {
        let day = (self.checked_at)
            .filter(|at| *at <= now)
            .map_or(i64::MIN, |at| at.saturating_add(DAY_MS));
        let retry = (self.retry_after)
            .filter(|_| match self.retry_set_at {
                Some(at) => at <= now,
                None => self.checked_at.is_none_or(|at| at <= now),
            })
            .unwrap_or(i64::MIN);
        day.max(retry)
    }

    fn answered_at(&self) -> Option<i64> {
        self.successful_at
            .or_else(|| self.release.as_ref().and(self.checked_at))
    }

    fn answered_within_a_day(&self, now: i64) -> bool {
        self.answered_at()
            .is_some_and(|at| at > now.saturating_sub(DAY_MS) && at <= now)
    }

    pub fn after(mut self, answer: Answer, now: i64) -> Checked {
        self.asset = Some(this_asset());
        let successful_at = self.answered_at();
        match answer {
            Answer::Releases { etag, release } => Checked {
                asset: self.asset,
                checked_at: Some(now),
                successful_at: Some(now),
                etag,
                release,
                retry_after: None,
                retry_set_at: None,
            },
            Answer::NotModified => Checked {
                checked_at: Some(now),
                successful_at: successful_at.map(|_| now),
                retry_after: None,
                retry_set_at: None,
                ..self
            },
            Answer::Refused { until } => Checked {
                checked_at: Some(now),
                successful_at,
                retry_after: until.filter(|until| *until > now.saturating_add(DAY_MS)),
                retry_set_at: Some(now),
                ..self
            },
            Answer::Unreachable => Checked {
                successful_at,
                retry_after: Some(now.saturating_add(HOUR_MS)),
                retry_set_at: Some(now),
                ..self
            },
        }
    }

    pub fn notice(&self, runs: &Version) -> Update {
        if is_development_version(&runs.to_string()) {
            return Update::Off;
        }
        let Some(checked_at_ms) = self.answered_at() else {
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
    let mut url = base.join(RELEASES).ok()?;
    url.set_query(Some("per_page=100"));
    Some((url, https))
}

pub(crate) async fn ask(
    http: &reqwest::Client,
    url: Url,
    etag: Option<&str>,
    asset: &str,
) -> Answer {
    let mut answered = false;
    let asked = tokio::time::timeout(TIMEOUT, async {
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
                Ok(response) => {
                    answered = true;
                    response
                }
                Err(_) if !answered => return Answer::Unreachable,
                Err(_) => return Answer::Refused { until: None },
            };
            let (status, headers) = (response.status(), response.headers().clone());
            let next = match next_page(&url, &page, &headers) {
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
    .await;
    asked.unwrap_or(match answered {
        true => Answer::Refused { until: None },
        false => Answer::Unreachable,
    })
}

fn next_page(endpoint: &Url, page: &Url, headers: &HeaderMap) -> Result<Option<Url>, ()> {
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
            let url = page.join(target).map_err(|_| ())?;
            if url.origin() != endpoint.origin()
                || !lists_releases(endpoint, &url)
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

fn lists_releases(endpoint: &Url, url: &Url) -> bool {
    let Some(base) = endpoint.path().strip_suffix(RELEASES) else {
        return false;
    };
    let repository = (url.path().strip_prefix(base))
        .and_then(|path| path.strip_prefix("repositories/"))
        .and_then(|path| path.strip_suffix("/releases"));
    url.path() == endpoint.path()
        || repository
            .is_some_and(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
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
        Answer::Refused { .. } if checked.answered_within_a_day(now_ms()) => Ok(checked.release),
        Answer::Refused { .. } => Err(CheckError::Refused),
        Answer::Unreachable => Err(CheckError::Unreachable),
    }
}

pub(crate) fn load(path: &Path) -> Option<Checked> {
    let checked: Checked = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    (checked.asset.as_deref() == Some(this_asset().as_str())).then_some(checked)
}

fn save(path: &Path, checked: &Checked) -> io::Result<()> {
    let legacy = path.with_extension("json.new");
    koidrive_state_file::write_with_legacy(
        path,
        &serde_json::to_vec(checked)?,
        &[legacy.file_name().expect("the update cache has a name")],
    )
    .map(|written| written.warn_unless_durable(path))
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
            let now = now_ms();
            let wait = checked.due(now).saturating_sub(now);
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
        let now = now_ms();
        let failure = match answer {
            Answer::Unreachable => Some(CheckError::Unreachable),
            Answer::Refused { .. } if !checked.answered_within_a_day(now) => {
                Some(CheckError::Refused)
            }
            Answer::Refused { .. } | Answer::Releases { .. } | Answer::NotModified => None,
        };
        let checked = checked.after(answer, now);
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
mod tests;
