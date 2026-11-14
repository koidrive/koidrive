// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, Once, PoisonError, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};

use koidrive_driver::{self as driver, Status, api_for};
use koidrive_ipc::{Code, Response};
use koidrive_net::{Api, PROTO_MAX, PROTO_MIN};
use tokio::runtime::Runtime;
use tokio::sync::{Notify, watch};

use crate::core::{Core, Events, pairing_code};
use crate::helper::{
    AccountData, DevicesData, Done, FolderData, Helper, InvitationData, Outcome, Refusal,
    RequestsData, RestoreAskedData, SpaceData, SpacesData, TrashData, UsagesData, VersionsData,
};
use crate::profile::Profile;
use crate::state::{self, Halted};
use crate::types::{
    Account, Answer, Device, FileVersions, LinkedFolder, PairingCode, RestoreAsked, RestoreRequest,
    ServerInfo, SessionError, SessionEvent, SessionState, Space, SpaceUsage, SyncStatus,
    TelemetrySetting, Trash,
};

const LOOK_AGAIN: Duration = Duration::from_secs(1);

const ANSWER: Duration = Duration::from_secs(5);

pub(crate) struct DaemonCore {
    me: Weak<DaemonCore>,
    helper: Helper,
    paths: driver::Paths,
    runtime: Option<Runtime>,
    events: Events,
    inner: Mutex<Inner>,
    reading: Mutex<u64>,
    wake: Arc<Notify>,
    watcher: Mutex<Option<Watcher>>,
    reconciled: Once,
}

struct Inner {
    started: bool,
    server: Option<Api>,
    status: Status,
    questions: Vec<driver::Question>,
    halted: Halted,
    sent: SessionState,
}

struct Watcher {
    stop: watch::Sender<bool>,
    thread: JoinHandle<()>,
}

impl DaemonCore {
    pub(crate) fn open(
        helper: PathBuf,
        profile: Profile,
        events: Events,
    ) -> Result<Arc<Self>, SessionError> {
        if !helper.is_file() {
            return Err(SessionError::Failed {
                reason: format!("koi is missing at {}", koidrive_fs::quoted(&helper)),
            });
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("koidrive-daemon")
            .enable_all()
            .build()
            .map_err(|error| SessionError::Failed {
                reason: format!("the app's runtime cannot start: {error}"),
            })?;
        let core = Arc::new_cyclic(|me| DaemonCore {
            me: me.clone(),
            helper: Helper {
                path: helper,
                env: profile.env,
            },
            paths: profile.paths,
            runtime: Some(runtime),
            events,
            inner: Mutex::new(Inner {
                started: false,
                server: None,
                status: Status::default(),
                questions: Vec::new(),
                halted: Halted::new(),
                sent: SessionState {
                    status: SyncStatus::Stopped,
                    progress: None,
                    account: None,
                    folders: Vec::new(),
                    recent_changes: Vec::new(),
                    questions: Vec::new(),
                    problems: Vec::new(),
                    notices: Vec::new(),
                },
            }),
            reading: Mutex::default(),
            wake: Arc::default(),
            watcher: Mutex::new(None),
            reconciled: Once::new(),
        });
        core.reload();
        Ok(core)
    }

    fn runtime(&self) -> &Runtime {
        self.runtime
            .as_ref()
            .expect("the runtime lives as long as the core")
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn reload(&self) {
        let mut reading = self.reading.lock().unwrap_or_else(PoisonError::into_inner);
        *reading = reading.wrapping_add(1);
        match self.read() {
            Ok((status, questions)) => self.observe(status, questions),
            Err(error) => tracing::warn!(%error, "the sync state cannot be read"),
        }
    }

    fn read(&self) -> Result<(Status, Vec<driver::Question>), SessionError> {
        if let Some(state) = self.runtime().block_on(daemon_state(self.paths.socket())) {
            return Ok(state);
        }
        self.helper.run(&["status"])?.status()
    }

    fn observe(&self, status: Status, questions: Vec<driver::Question>) {
        let mut inner = self.lock();
        inner.status = status;
        inner.questions = questions;
        self.send(&mut inner, false);
    }

    fn daemon_gone(&self) {
        let mut inner = self.lock();
        inner.status.running = false;
        self.send(&mut inner, false);
    }

    fn send(&self, inner: &mut Inner, ask_again: bool) {
        let next = state::session_state(
            &inner.status,
            &inner.questions,
            inner.started,
            &mut inner.halted,
            &self.helper,
        );
        let sent = &inner.sent;
        let mut events = Vec::new();
        if next.account != sent.account {
            events.push(SessionEvent::Account {
                account: next.account.clone(),
            });
        }
        if next.folders != sent.folders {
            events.push(SessionEvent::Folders {
                folders: next.folders.clone(),
            });
        }
        if ask_again || next.questions != sent.questions {
            events.push(SessionEvent::Questions {
                questions: next.questions.clone(),
            });
        }
        if next.problems != sent.problems {
            events.push(SessionEvent::Problems {
                problems: next.problems.clone(),
            });
        }
        if next.notices != sent.notices {
            events.push(SessionEvent::Notices {
                notices: next.notices.clone(),
            });
        }
        if next.status != sent.status {
            events.push(SessionEvent::Status {
                status: next.status,
            });
        }
        if next.progress != sent.progress {
            events.push(SessionEvent::Progress {
                progress: next.progress.clone(),
            });
        }
        inner.sent = next;
        for event in events {
            self.events.send(event);
        }
    }

    fn change(&self, args: &[&str]) -> Result<Done, Refusal> {
        self.change_with_input(args, None)
    }

    fn change_with_input(&self, args: &[&str], input: Option<&str>) -> Result<Done, Refusal> {
        let done = self.helper.run_with_input(args, input);
        self.reload();
        self.wake.notify_one();
        done
    }

    fn server(&self) -> Result<String, SessionError> {
        let inner = self.lock();
        let server = inner.server.as_ref().ok_or(SessionError::NoServer)?;
        Ok(server.base().as_str().to_owned())
    }

    fn watch(&self) {
        let mut watcher = self.watcher.lock().unwrap_or_else(PoisonError::into_inner);
        if watcher.is_some() {
            return;
        }
        let (stop, stopped) = watch::channel(false);
        let (core, runtime) = (self.me.clone(), self.runtime().handle().clone());
        let spawned = thread::Builder::new()
            .name("koidrive-watch".into())
            .spawn(move || runtime.block_on(follow(core, stopped)));
        match spawned {
            Ok(thread) => *watcher = Some(Watcher { stop, thread }),
            Err(error) => tracing::error!(%error, "the daemon's state cannot be followed"),
        }
    }

    fn unwatch(&self) {
        let watcher = (self.watcher.lock().unwrap_or_else(PoisonError::into_inner)).take();
        let Some(watcher) = watcher else { return };
        let _ = watcher.stop.send(true);
        if watcher.thread.thread().id() != thread::current().id() {
            let _ = watcher.thread.join();
        }
    }

    fn reconcile(&self) {
        let (helper, wake) = (self.helper.clone(), self.wake.clone());
        let spawned = thread::Builder::new()
            .name("koidrive-reconcile".into())
            .spawn(move || {
                if let Err(refusal) = helper.run(&["_reconcile"]) {
                    tracing::warn!(message = refusal.message, "koi left its service as it was");
                }
                wake.notify_one();
            });
        if let Err(error) = spawned {
            tracing::error!(%error, "the service cannot be reconciled");
        }
    }
}

impl Drop for DaemonCore {
    fn drop(&mut self) {
        self.close();
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

fn owner() -> u32 {
    rustix::process::getuid().as_raw()
}

async fn daemon_state(socket: PathBuf) -> Option<(Status, Vec<driver::Question>)> {
    let first = async {
        let mut lines = koidrive_ipc::watch(&socket, owner()).await.ok()??;
        match lines.next().await.ok()?? {
            Response::State { status, questions } => Some((status, questions)),
            _ => None,
        }
    };
    tokio::time::timeout(ANSWER, first).await.ok()?
}

type ProfileFiles = Vec<(PathBuf, Option<(SystemTime, u64)>)>;

fn profile_files(paths: &driver::Paths) -> ProfileFiles {
    let stores = (fs::read_dir(paths.spaces()).into_iter().flatten().flatten())
        .filter_map(|entry| paths.store(&entry.file_name().to_string_lossy()).ok())
        .flat_map(|store| {
            let mut wal = store.clone().into_os_string();
            wal.push("-wal");
            [store, PathBuf::from(wal)]
        });
    let mut files: ProfileFiles = [
        paths.socket(),
        paths.credential(),
        paths.state_dir.join("leaving"),
        paths.paused(),
        paths.config_file.clone(),
    ]
    .into_iter()
    .chain(stores)
    .map(|path| {
        let seen = fs::metadata(&path)
            .ok()
            .and_then(|metadata| Some((metadata.modified().ok()?, metadata.len())));
        (path, seen)
    })
    .collect();
    files.sort();
    files
}

async fn follow(core: Weak<DaemonCore>, mut stopped: watch::Receiver<bool>) {
    let mut seen = None;
    loop {
        let Some((paths, wake, helper)) = core
            .upgrade()
            .map(|core| (core.paths.clone(), core.wake.clone(), core.helper.clone()))
        else {
            return;
        };
        if let Ok(Some(mut lines)) = koidrive_ipc::watch(&paths.socket(), owner()).await {
            seen = None;
            loop {
                let line = tokio::select! {
                    line = lines.next() => line,
                    _ = stopped.changed() => return,
                };
                let Some(core) = core.upgrade() else { return };
                match line {
                    Ok(Some(Response::State { status, questions })) => {
                        let _reading = core.reading.lock().unwrap_or_else(PoisonError::into_inner);
                        core.observe(status, questions)
                    }
                    Ok(Some(Response::Error { message, .. })) => {
                        tracing::warn!(message, "the daemon cannot report its state");
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => {
                        core.daemon_gone();
                        break;
                    }
                }
            }
        } else {
            let now = profile_files(&paths);
            if seen.as_ref() != Some(&now) {
                let Some(revision) = core.upgrade().map(|core| {
                    let reading = core.reading.lock().unwrap_or_else(PoisonError::into_inner);
                    *reading
                }) else {
                    return;
                };
                let read = tokio::select! {
                    read = tokio::task::spawn_blocking(move || {
                        helper.run(&["status"])
                            .map_err(SessionError::from)
                            .and_then(|done| done.status())
                    }) => read,
                    _ = stopped.changed() => return,
                };
                let Some(core) = core.upgrade() else { return };
                match read {
                    Ok(Ok((status, questions))) => {
                        let reading = core.reading.lock().unwrap_or_else(PoisonError::into_inner);
                        if *reading == revision {
                            core.observe(status, questions);
                            seen = Some(now);
                        }
                    }
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "the sync state cannot be read");
                    }
                    Err(error) => {
                        tracing::warn!(%error, "the status worker cannot finish");
                    }
                }
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(LOOK_AGAIN) => {}
            _ = wake.notified() => {}
            _ = stopped.changed() => return,
        }
    }
}

impl Core for DaemonCore {
    fn state(&self) -> SessionState {
        self.lock().sent.clone()
    }

    fn start(&self) {
        {
            let mut inner = self.lock();
            if inner.started {
                return;
            }
            inner.started = true;
            self.send(&mut inner, true);
        }
        self.watch();
        self.reconciled.call_once(|| self.reconcile());
    }

    fn stop(&self) {
        self.unwatch();
        let mut inner = self.lock();
        inner.started = false;
        self.send(&mut inner, false);
    }

    fn pause(&self) -> Result<(), SessionError> {
        if !self.lock().started {
            return Ok(());
        }
        self.change(&["pause"])?;
        Ok(())
    }

    fn resume(&self) -> Result<(), SessionError> {
        self.change(&["resume"])?;
        Ok(())
    }

    fn connect(&self, url: &str) -> Result<ServerInfo, SessionError> {
        let api = api_for(url.trim()).map_err(|_| SessionError::InvalidServerUrl)?;
        let info = self
            .runtime()
            .block_on(api.server_info())
            .map_err(|error| match error {
                koidrive_net::ApiError::Transport(source) => SessionError::ServerUnreachable {
                    reason: error_chain(&source),
                },
                other => SessionError::Failed {
                    reason: other.to_string(),
                },
            })?;
        if info.proto_max < PROTO_MIN || info.proto_min > PROTO_MAX {
            return Err(SessionError::ServerIncompatible {
                server_version: info.server_version,
            });
        }
        let server = ServerInfo {
            url: api.base().as_str().trim_end_matches('/').to_owned(),
            server_version: info.server_version,
            claimed: info.claimed,
        };
        self.lock().server = Some(api);
        Ok(server)
    }

    fn claim(
        &self,
        code: &str,
        user_name: &str,
        device_name: &str,
    ) -> Result<Account, SessionError> {
        let server = self.server()?;
        let args = [
            "server",
            "claim",
            "--user",
            user_name,
            "--name",
            device_name,
            "--code-stdin",
            "--",
            &server,
        ];
        let done = self.change_with_input(&args, Some(code.trim()))?;
        Ok(done.data::<AccountData>()?.account())
    }

    fn pair(&self, code: &str, device_name: &str) -> Result<Account, SessionError> {
        let server = self.server()?;
        let code = pairing_code(code, &server)?;
        let args = [
            "join",
            "--server",
            &server,
            "--name",
            device_name,
            "--code-stdin",
        ];
        let done = self.change_with_input(&args, Some(code))?;
        Ok(done.data::<AccountData>()?.account())
    }

    fn sign_out(&self, confirm_last_device: bool) -> Result<(), SessionError> {
        #[derive(serde::Deserialize)]
        struct Left {
            #[serde(default)]
            not_removed: Vec<driver::CleanupFailure>,
        }

        let leave = ["device", "leave", "--last-device"];
        let args = match confirm_last_device {
            true => &leave[..],
            false => &leave[..2],
        };
        let done = self.change(args).map_err(|refusal| match refusal.code {
            Code::Failed if refusal.names("--last-device") => SessionError::LastDevice,
            Code::Unavailable if refusal.names("--last-device") => SessionError::NotRevoked {
                reason: refusal.message,
            },
            _ => refusal.into(),
        })?;
        let left = done.data::<Left>()?.not_removed;
        if !left.is_empty() {
            return Err(SessionError::Failed {
                reason: format!(
                    "Signed out, but these paths could not be removed. Run koi device leave again to finish.\n{}",
                    left.iter()
                        .map(|failure| format!(
                            "{}: {}",
                            koidrive_fs::quoted(&failure.path),
                            failure.error
                        ))
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
            });
        }
        Ok(())
    }

    fn devices(&self) -> Result<Vec<Device>, SessionError> {
        let done = self.helper.run(&["device", "list"])?;
        Ok(done.data::<DevicesData>()?.active())
    }

    fn revoke_device(&self, device_id: &str) -> Result<(), SessionError> {
        let issued = device_id.len() == 36
            && (device_id.bytes()).all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f' | b'-'));
        if !issued {
            return Err(SessionError::DeviceNotFound);
        }
        let this = self
            .lock()
            .sent
            .account
            .as_ref()
            .map(|account| account.device_id.clone());
        if this.as_deref() == Some(device_id) {
            return Err(SessionError::ThisDevice);
        }
        match self
            .helper
            .run(&["device", "revoke", "--", device_id])?
            .outcome
        {
            Outcome::Unchanged => Err(SessionError::DeviceNotFound),
            _ => Ok(()),
        }
    }

    fn start_pairing(&self) -> Result<PairingCode, SessionError> {
        let done = self.helper.run(&["device", "invite"])?;
        Ok(done.data::<InvitationData>()?.code())
    }

    fn spaces(&self) -> Result<Vec<Space>, SessionError> {
        let done = self.helper.run(&["space", "list"])?;
        let spaces = done.data::<SpacesData>()?.spaces;
        Ok(spaces.into_iter().map(SpaceData::space).collect())
    }

    fn create_space(&self, name: &str) -> Result<Space, SessionError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(SessionError::EmptyName);
        }
        let done = self.helper.run(&["space", "create", "--", name])?;
        Ok(done.data::<SpaceData>()?.space())
    }

    fn space_usage(&self, space_id: &str) -> Result<SpaceUsage, SessionError> {
        let done = self.helper.run(&["space", "usage"])?;
        done.data::<UsagesData>()?.of(space_id)
    }

    fn link_folder(&self, space_id: &str, path: &str) -> Result<LinkedFolder, SessionError> {
        let done = self.change(&["folder", "link", "--space", space_id, "--", path])?;
        Ok(done.data::<FolderData>()?.folder())
    }

    fn unlink_folder(&self, space_id: &str) -> Result<(), SessionError> {
        let path = (self.lock().sent.folders.iter())
            .find(|folder| folder.space_id == space_id)
            .map(|folder| folder.path.clone())
            .ok_or(SessionError::FolderNotLinked)?;
        match self.change(&["folder", "unlink", "--", &path])?.outcome {
            Outcome::Unchanged => Err(SessionError::FolderNotLinked),
            _ => Ok(()),
        }
    }

    fn trash(&self, path: &str) -> Result<Trash, SessionError> {
        let done = (self
            .helper
            .run(&["history", "trash", "--", absolute(path)?]))
        .map_err(|refusal| refusal.at(path))?;
        Ok(done.data::<TrashData>()?.trash())
    }

    fn versions(&self, path: &str) -> Result<FileVersions, SessionError> {
        let done = (self
            .helper
            .run(&["history", "versions", "--", absolute(path)?]))
        .map_err(|refusal| refusal.at(path))?;
        Ok(done.data::<VersionsData>()?.versions())
    }

    fn restore_deleted(&self, path: &str, node_id: &str) -> Result<RestoreAsked, SessionError> {
        let args = [
            "history",
            "restore",
            "--node",
            node_id,
            "--",
            absolute(path)?,
        ];
        let done = self.change(&args).map_err(|refusal| refusal.at(path))?;
        Ok(done.data::<RestoreAskedData>()?.asked())
    }

    fn restore_version(&self, path: &str, version_id: &str) -> Result<RestoreAsked, SessionError> {
        let args = [
            "history",
            "restore",
            "--version",
            version_id,
            "--",
            absolute(path)?,
        ];
        let done = self.change(&args).map_err(|refusal| refusal.at(path))?;
        Ok(done.data::<RestoreAskedData>()?.asked())
    }

    fn restore_requests(&self, path: &str) -> Result<Vec<RestoreRequest>, SessionError> {
        let done = (self
            .helper
            .run(&["history", "requests", "--", absolute(path)?]))
        .map_err(|refusal| refusal.at(path))?;
        Ok(done.data::<RequestsData>()?.requests())
    }

    fn answer(&self, question_id: &str, answer: Answer) -> Result<(), SessionError> {
        let done = (self.change(&["resolve", "--", question_id, state::word(answer)])).map_err(
            |refusal| match refusal.code {
                Code::StaleDecision => SessionError::QuestionNotFound,
                Code::Usage => SessionError::AnswerNotAllowed,
                _ => refusal.into(),
            },
        )?;
        match done.outcome {
            Outcome::Unchanged => Err(SessionError::QuestionNotFound),
            _ => Ok(()),
        }
    }

    fn dismiss_notice(&self, notice_id: &str) -> Result<(), SessionError> {
        let done =
            (self.change(&["resolve", "--", notice_id, "dismiss"])).map_err(
                |refusal| match refusal.code {
                    Code::StaleDecision | Code::Usage => SessionError::NoticeNotFound,
                    _ => refusal.into(),
                },
            )?;
        match done.outcome {
            Outcome::Unchanged => Err(SessionError::NoticeNotFound),
            _ => Ok(()),
        }
    }

    fn telemetry(&self) -> TelemetrySetting {
        let store = koidrive_telemetry::Store::new(&self.paths.state_dir);
        let settings = (store.settings())
            .inspect_err(|error| tracing::warn!(%error, "the telemetry settings cannot be read"))
            .unwrap_or_default();
        let status = koidrive_telemetry::resolve(|name| std::env::var_os(name), settings.enabled);
        TelemetrySetting {
            enabled: status.mode != koidrive_telemetry::Mode::Off,
            decided: settings.enabled.is_some(),
            locked: matches!(
                status.source,
                koidrive_telemetry::Source::DoNotTrack | koidrive_telemetry::Source::Variable
            ),
        }
    }

    fn set_telemetry(&self, enabled: bool) -> Result<(), SessionError> {
        let choice = match enabled {
            true => "on",
            false => "off",
        };
        (self.helper.run(&["telemetry", choice]))
            .map(drop)
            .map_err(|refusal| SessionError::NotSaved {
                reason: refusal.message,
            })
    }

    fn close(&self) {
        self.unwatch();
    }
}

fn absolute(path: &str) -> Result<&str, SessionError> {
    match Path::new(path).is_absolute() {
        true => Ok(path),
        false => Err(SessionError::PathNotInLinkedFolder {
            path: path.to_owned(),
        }),
    }
}

fn error_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod server;
