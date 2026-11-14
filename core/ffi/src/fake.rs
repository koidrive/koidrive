// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::path::{Component, Path};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::core::{Core, Events, pairing_code};
use crate::types::{
    Account, Answer, ChangeKind, DeletedItem, Device, Direction, FileVersion, FileVersions,
    FolderPath, FolderState, HeldItem, ItemDetail, LinkedFolder, MassDeleteLimit, NodeKind, Notice,
    NoticeKind, PairingCode, Problem, ProblemKind, Question, QuestionKind, RECENT_CHANGES,
    RecentChange, RecoveryCause, RecoveryGroup, RecoveryTrigger, RestoreAsked, RestoreRequest,
    RestoreState, RestoreTarget, ServerInfo, SessionError, SessionEvent, SessionState, Space,
    SpaceUsage, SyncProgress, SyncStatus, TelemetrySetting, Trash,
};

const CLAIM_CODE_LEN: usize = 12;
const PAIRING_CODE_LEN: usize = 8;
const PAIRING_CODE_MS: i64 = 10 * 60_000;
const NOT_WRITABLE_ID: &str = "problem-state-not-writable";

/// What a running, set-up fake session is doing, which decides its [`SyncStatus`] unless it is
/// paused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum FakeActivity {
    Connecting,
    Syncing,
    Idle,
    Offline,
}

/// A whole state the fake can switch to at once. Every scenario but [`FakeScenario::FirstRun`]
/// is signed in to `https://box.example.com` with two linked folders, Documents and Photos, from
/// one of the account's two devices.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum FakeScenario {
    /// A fresh install: no server, no account, no folders, and the server is not claimed yet.
    FirstRun,
    /// Up to date, with a few recent changes. Documents has three deletions in its trash and a file with three
    /// versions, `Reports/Annual Report.docx`.
    UpToDate,
    /// Syncing, with progress and recent changes.
    Syncing,
    /// Paused by the person.
    Paused,
    /// The server cannot be reached.
    Offline,
    /// One mass-delete question in Documents: 14 held deletions of 4,553 files in a space of
    /// 6,000, over half of the space.
    MassDelete,
    /// One ambiguous-disappearance question in Documents with 4 files.
    AmbiguousDisappearance,
    /// Photos is recovering after a rewind: a recovery notice and one question for each of the
    /// five recovery groups.
    Recovery,
    /// Photos recovered after a rewind with nothing held: a recovery notice and no questions.
    RecoveredWithoutQuestions,
    /// Another device deleted 1,200 files from Photos, more than the guard's limit; a notice
    /// reports it and nothing is asked.
    RemoteMassDelete,
    /// Long lists: a mass delete of 1,500 files, the five recovery groups with 300 items each,
    /// and 25 ambiguous-disappearance questions: 31 questions in all.
    ManyQuestions,
    /// One problem of every kind. Photos is halted because its folder is missing.
    Problems,
}

/// An answer the fake received, for tests.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct GivenAnswer {
    pub question_id: String,
    pub answer: Answer,
}

#[derive(Clone, Debug, Default)]
struct Saved {
    server: Option<ServerInfo>,
    claimed: bool,
    account: Option<Account>,
    devices: Vec<Device>,
    revoked: Vec<String>,
    spaces: Vec<Space>,
    folders: Vec<LinkedFolder>,
    questions: Vec<Question>,
    problems: Vec<Problem>,
    notices: Vec<Notice>,
    paused: bool,
    next_id: u64,
    history: BTreeMap<String, History>,
}

#[derive(Clone, Debug, Default)]
struct History {
    trash: Vec<DeletedItem>,
    versions: Vec<FileVersions>,
    requests: Vec<RestoreRequest>,
}

struct State {
    saved: Saved,
    running: bool,
    activity: FakeActivity,
    progress: Option<SyncProgress>,
    recent: Vec<RecentChange>,
    answers: Vec<GivenAnswer>,
    fail_next: Option<SessionError>,
    usage_refusals: Vec<(String, SessionError)>,
    fail_saves: bool,
    not_writable: Option<Problem>,
    status: SyncStatus,
    telemetry: TelemetrySetting,
}

impl Saved {
    fn next_id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}-{}", self.next_id)
    }

    fn account(&self) -> Result<&Account, SessionError> {
        self.account.as_ref().ok_or(SessionError::NotSignedIn)
    }

    fn ask_restore(
        &mut self,
        space_id: String,
        folder: String,
        path: String,
        target: RestoreTarget,
    ) -> RestoreAsked {
        let id = self.next_id("restore");
        self.history
            .entry(space_id)
            .or_default()
            .requests
            .push(RestoreRequest {
                id: id.clone(),
                path: Some(path.clone()),
                asked_at_ms: now_ms(),
                target,
                state: RestoreState::Pending {
                    held_by_problem: false,
                },
            });
        RestoreAsked {
            request_id: id,
            folder,
            path,
        }
    }
}

impl State {
    fn derived_status(&self) -> SyncStatus {
        if !self.running {
            SyncStatus::Stopped
        } else if self.saved.account.is_none() || self.saved.folders.is_empty() {
            SyncStatus::NeedsSetup
        } else if self.saved.paused {
            SyncStatus::Paused
        } else if self
            .saved
            .folders
            .iter()
            .any(|folder| folder.state == FolderState::Halted)
        {
            SyncStatus::Halted
        } else {
            match self.activity {
                FakeActivity::Connecting => SyncStatus::Connecting,
                FakeActivity::Syncing => SyncStatus::Syncing,
                FakeActivity::Idle => SyncStatus::Idle,
                FakeActivity::Offline => SyncStatus::Offline,
            }
        }
    }

    fn problems(&self) -> Vec<Problem> {
        let mut problems = self.saved.problems.clone();
        problems.extend(self.not_writable.clone());
        problems
    }

    fn set_save_failure(&mut self, reason: Option<String>) -> bool {
        let next = reason.map(|reason| Problem {
            id: NOT_WRITABLE_ID.to_owned(),
            space_id: None,
            path: None,
            kind: ProblemKind::StateNotWritable { reason },
            since_ms: self
                .not_writable
                .as_ref()
                .map_or_else(now_ms, |p| p.since_ms),
        });
        let changed = next != self.not_writable;
        self.not_writable = next;
        changed
    }

    fn take_failure(&mut self) -> Result<(), SessionError> {
        self.fail_next.take().map_or(Ok(()), Err)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    Progress,
    Account,
    Folders,
    Recent,
    Questions,
    Problems,
    Notices,
}

const ALL_PARTS: [Part; 7] = [
    Part::Progress,
    Part::Account,
    Part::Folders,
    Part::Recent,
    Part::Questions,
    Part::Problems,
    Part::Notices,
];

/// The fake core. Swift scripts it through [`SyncSession::fake_core`](crate::SyncSession::fake_core).
#[derive(uniffi::Object)]
pub struct FakeCore {
    state: Mutex<State>,
    events: Events,
}

impl FakeCore {
    pub fn new(events: Events) -> Self {
        Self {
            state: Mutex::new(State {
                saved: Saved::default(),
                running: false,
                activity: FakeActivity::Idle,
                progress: None,
                recent: Vec::new(),
                answers: Vec::new(),
                fail_next: None,
                usage_refusals: Vec::new(),
                fail_saves: false,
                not_writable: None,
                status: SyncStatus::Stopped,
                telemetry: TelemetrySetting::default(),
            }),
            events,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn commit(&self, state: &mut State, next: Saved, parts: &[Part]) -> Result<(), SessionError> {
        let cleared = self.store(state, next)?;
        self.publish(state, parts, cleared);
        Ok(())
    }

    fn store(&self, state: &mut State, next: Saved) -> Result<bool, SessionError> {
        if state.fail_saves {
            let reason = "saving is scripted to fail".to_owned();
            let problems = state.set_save_failure(Some(reason.clone()));
            self.publish(state, &[], problems);
            return Err(SessionError::NotSaved { reason });
        }
        state.saved = next;
        Ok(state.set_save_failure(None))
    }

    fn update<T>(
        &self,
        state: &mut State,
        parts: &[Part],
        change: impl FnOnce(&mut Saved) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        let mut next = state.saved.clone();
        let value = change(&mut next)?;
        self.commit(state, next, parts)?;
        Ok(value)
    }

    fn publish(&self, state: &mut State, parts: &[Part], problems: bool) {
        let extra = (problems && !parts.contains(&Part::Problems)).then_some(Part::Problems);
        for part in parts.iter().copied().chain(extra) {
            self.events.send(match part {
                Part::Progress => SessionEvent::Progress {
                    progress: state.progress.clone(),
                },
                Part::Account => SessionEvent::Account {
                    account: state.saved.account.clone(),
                },
                Part::Folders => SessionEvent::Folders {
                    folders: state.saved.folders.clone(),
                },
                Part::Recent => SessionEvent::RecentChanges {
                    changes: state.recent.clone(),
                },
                Part::Questions => SessionEvent::Questions {
                    questions: state.saved.questions.clone(),
                },
                Part::Problems => SessionEvent::Problems {
                    problems: state.problems(),
                },
                Part::Notices => SessionEvent::Notices {
                    notices: state.saved.notices.clone(),
                },
            });
        }
        let status = state.derived_status();
        if status != state.status {
            state.status = status;
            self.events.send(SessionEvent::Status { status });
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn normalize_code(code: &str) -> String {
    code.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

fn this_device(account: &Account) -> Device {
    Device {
        device_id: account.device_id.clone(),
        name: account.device_name.clone(),
        platform: "macos".to_owned(),
        this_device: true,
        added_at_ms: now_ms(),
    }
}

fn located<'a>(
    folders: &'a [LinkedFolder],
    path: &str,
) -> Result<(&'a LinkedFolder, String), SessionError> {
    let path = Path::new(path);
    folders
        .iter()
        .find_map(|folder| {
            let names: Option<Vec<&str>> = (path.strip_prefix(&folder.path).ok()?.components())
                .map(|component| match component {
                    Component::Normal(name) => name.to_str(),
                    _ => None,
                })
                .collect();
            Some((folder, names?.join("/")))
        })
        .ok_or_else(|| SessionError::PathNotInLinkedFolder {
            path: path.to_string_lossy().into_owned(),
        })
}

fn located_inside<'a>(
    folders: &'a [LinkedFolder],
    path: &str,
) -> Result<(&'a LinkedFolder, String), SessionError> {
    let (folder, below) = located(folders, path)?;
    match below.is_empty() {
        true => Err(SessionError::PathIsLinkedFolder),
        false => Ok((folder, below)),
    }
}

fn required(name: &str) -> Result<String, SessionError> {
    let name = name.trim();
    if name.is_empty() {
        Err(SessionError::EmptyName)
    } else {
        Ok(name.to_owned())
    }
}

#[uniffi::export]
impl FakeCore {
    /// Replaces the whole state with a scenario and sends every part. A running session keeps
    /// running.
    pub fn load_scenario(&self, scenario: FakeScenario) -> Result<(), SessionError> {
        let mut state = self.lock();
        let script = scenario::build(scenario, now_ms());
        let cleared = self.store(&mut state, script.saved)?;
        state.activity = script.activity;
        state.progress = script.progress;
        state.recent = script.recent;
        state.answers.clear();
        state.fail_next = None;
        state.usage_refusals.clear();
        state.telemetry = TelemetrySetting {
            enabled: scenario != FakeScenario::FirstRun,
            decided: scenario != FakeScenario::FirstRun,
            locked: false,
        };
        self.publish(&mut state, &ALL_PARTS, cleared);
        Ok(())
    }

    /// Sets the usage-report setting as it is, such as a locked one, which [`Core::set_telemetry`] cannot make.
    pub fn set_telemetry_setting(&self, setting: TelemetrySetting) {
        self.lock().telemetry = setting;
    }

    pub fn set_activity(&self, activity: FakeActivity) {
        let mut state = self.lock();
        state.activity = activity;
        self.publish(&mut state, &[], false);
    }

    pub fn set_progress(&self, progress: Option<SyncProgress>) {
        let mut state = self.lock();
        state.progress = progress;
        self.publish(&mut state, &[Part::Progress], false);
    }

    /// Adds a change at the top of the recent changes, keeping the newest
    /// [`RECENT_CHANGES`](crate::RECENT_CHANGES).
    pub fn add_recent_change(&self, change: RecentChange) {
        let mut state = self.lock();
        state.recent.insert(0, change);
        state.recent.truncate(RECENT_CHANGES);
        self.publish(&mut state, &[Part::Recent], false);
    }

    /// Sets whether the server reports itself claimed, which decides between claiming and
    /// pairing in the first-run setup.
    pub fn set_server_claimed(&self, claimed: bool) -> Result<(), SessionError> {
        self.update(&mut self.lock(), &[], |saved| {
            saved.claimed = claimed;
            if let Some(server) = &mut saved.server {
                server.claimed = claimed;
            }
            Ok(())
        })
    }

    /// Sets the devices the server lists for the account.
    pub fn set_devices(&self, devices: Vec<Device>) -> Result<(), SessionError> {
        self.update(&mut self.lock(), &[], |saved| {
            saved.devices = devices;
            Ok(())
        })
    }

    pub fn set_folder_state(
        &self,
        space_id: String,
        folder_state: FolderState,
    ) -> Result<(), SessionError> {
        self.update(&mut self.lock(), &[Part::Folders], |saved| {
            for folder in &mut saved.folders {
                if folder.space_id == space_id {
                    folder.state = folder_state;
                }
            }
            Ok(())
        })
    }

    /// Asks a question and returns its ID. The answers follow from the kind.
    pub fn ask(
        &self,
        space_id: String,
        kind: QuestionKind,
        items: Vec<HeldItem>,
    ) -> Result<String, SessionError> {
        self.update(&mut self.lock(), &[Part::Questions], |saved| {
            let id = saved.next_id("question");
            let answers = kind.answers();
            saved.questions.push(Question {
                id: id.clone(),
                space_id,
                asked_at_ms: now_ms(),
                kind,
                items,
                answers,
            });
            Ok(id)
        })
    }

    /// Withdraws a question without an answer, as the engine does when its held set changes or
    /// a certain case now applies (design §6.1, §6.4).
    pub fn withdraw_question(&self, question_id: String) -> Result<(), SessionError> {
        self.update(&mut self.lock(), &[Part::Questions], |saved| {
            saved.questions.retain(|q| q.id != question_id);
            Ok(())
        })
    }

    /// Adds a notice and returns its ID.
    pub fn notify(&self, space_id: String, kind: NoticeKind) -> Result<String, SessionError> {
        self.update(&mut self.lock(), &[Part::Notices], |saved| {
            let id = saved.next_id("notice");
            saved.notices.push(Notice {
                id: id.clone(),
                space_id,
                at_ms: now_ms(),
                kind,
            });
            Ok(id)
        })
    }

    /// Reports a problem and returns its ID.
    pub fn report_problem(
        &self,
        space_id: Option<String>,
        path: Option<String>,
        kind: ProblemKind,
    ) -> Result<String, SessionError> {
        self.update(&mut self.lock(), &[Part::Problems], |saved| {
            let id = saved.next_id("problem");
            saved.problems.push(Problem {
                id: id.clone(),
                space_id,
                path,
                kind,
                since_ms: now_ms(),
            });
            Ok(id)
        })
    }

    pub fn resolve_problem(&self, problem_id: String) -> Result<(), SessionError> {
        self.update(&mut self.lock(), &[Part::Problems], |saved| {
            saved.problems.retain(|p| p.id != problem_id);
            Ok(())
        })
    }

    /// Replaces what the trash of the space holds.
    pub fn set_trash(&self, space_id: String, items: Vec<DeletedItem>) -> Result<(), SessionError> {
        self.update(&mut self.lock(), &[], |saved| {
            let trash = &mut saved.history.entry(space_id).or_default().trash;
            *trash = items;
            trash.sort_by(|a, b| a.path.cmp(&b.path));
            Ok(())
        })
    }

    /// Replaces the versions the server keeps of the file at `versions.path` in the space.
    pub fn set_versions(
        &self,
        space_id: String,
        versions: FileVersions,
    ) -> Result<(), SessionError> {
        self.update(&mut self.lock(), &[], |saved| {
            let kept = &mut saved.history.entry(space_id).or_default().versions;
            kept.retain(|file| file.path != versions.path);
            kept.push(versions);
            Ok(())
        })
    }

    /// Ends the restore `request_id` with `state`, after the restores that ended before it. A restore from the trash
    /// that brought its item back takes the item out of the trash.
    pub fn finish_restore(
        &self,
        request_id: String,
        state: RestoreState,
    ) -> Result<(), SessionError> {
        self.update(&mut self.lock(), &[], |saved| {
            for history in saved.history.values_mut() {
                let Some(index) =
                    (history.requests.iter()).position(|request| request.id == request_id)
                else {
                    continue;
                };
                let mut request = history.requests.remove(index);
                if let RestoreTarget::Deleted { node_id } = &request.target
                    && matches!(
                        state,
                        RestoreState::Restored | RestoreState::RestoredAs { .. }
                    )
                {
                    history.trash.retain(|item| &item.node_id != node_id);
                }
                request.state = state;
                let ended = (history.requests.iter())
                    .take_while(|request| !matches!(request.state, RestoreState::Pending { .. }))
                    .count();
                history.requests.insert(ended, request);
                break;
            }
            Ok(())
        })
    }

    /// Makes the next call that can fail return `error` instead.
    pub fn fail_next(&self, error: SessionError) {
        self.lock().fail_next = Some(error);
    }

    /// Makes every usage read of the space fail with `error`, as for a space a rewind holds closed.
    pub fn refuse_space_usage(&self, space_id: String, error: SessionError) {
        self.lock().usage_refusals.push((space_id, error));
    }

    /// Makes every save fail, as an unwritable store would, until called with false.
    pub fn fail_saves(&self, fail: bool) {
        self.lock().fail_saves = fail;
    }

    /// The answers given so far, in order. An answer that could not be saved is not among them.
    pub fn given_answers(&self) -> Vec<GivenAnswer> {
        self.lock().answers.clone()
    }
}

impl Core for FakeCore {
    fn state(&self) -> SessionState {
        let state = self.lock();
        SessionState {
            status: state.status,
            progress: state.progress.clone(),
            account: state.saved.account.clone(),
            folders: state.saved.folders.clone(),
            recent_changes: state.recent.clone(),
            questions: state.saved.questions.clone(),
            problems: state.problems(),
            notices: state.saved.notices.clone(),
        }
    }

    fn start(&self) {
        let mut state = self.lock();
        if state.running {
            return;
        }
        state.running = true;
        self.publish(&mut state, &[Part::Questions, Part::Notices], false);
    }

    fn stop(&self) {
        let mut state = self.lock();
        state.running = false;
        self.publish(&mut state, &[], false);
    }

    fn pause(&self) -> Result<(), SessionError> {
        let mut state = self.lock();
        if !state.running || state.saved.paused {
            return Ok(());
        }
        self.update(&mut state, &[], |saved| {
            saved.paused = true;
            Ok(())
        })
    }

    fn resume(&self) -> Result<(), SessionError> {
        let mut state = self.lock();
        if !state.saved.paused {
            return Ok(());
        }
        self.update(&mut state, &[], |saved| {
            saved.paused = false;
            Ok(())
        })
    }

    fn connect(&self, url: &str) -> Result<ServerInfo, SessionError> {
        let mut state = self.lock();
        state.take_failure()?;
        let url = url.trim().trim_end_matches('/');
        let host = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))
            .ok_or(SessionError::InvalidServerUrl)?;
        if host.is_empty() || host.contains(char::is_whitespace) {
            return Err(SessionError::InvalidServerUrl);
        }
        self.update(&mut state, &[], |saved| {
            let server = ServerInfo {
                url: url.to_owned(),
                server_version: env!("CARGO_PKG_VERSION").to_owned(),
                claimed: saved.claimed,
            };
            saved.server = Some(server.clone());
            Ok(server)
        })
    }

    fn claim(
        &self,
        code: &str,
        user_name: &str,
        device_name: &str,
    ) -> Result<Account, SessionError> {
        let mut state = self.lock();
        state.take_failure()?;
        self.update(&mut state, &[Part::Account], |saved| {
            let server = saved.server.clone().ok_or(SessionError::NoServer)?;
            if saved.claimed {
                return Err(SessionError::AlreadyClaimed);
            }
            if normalize_code(code).len() != CLAIM_CODE_LEN {
                return Err(SessionError::InvalidCode);
            }
            let (user_name, device_name) = (required(user_name)?, required(device_name)?);
            let account = Account {
                server_url: server.url,
                user_id: saved.next_id("user"),
                user_name,
                device_id: saved.next_id("device"),
                device_name,
            };
            saved.claimed = true;
            saved.devices = vec![this_device(&account)];
            saved.account = Some(account.clone());
            Ok(account)
        })
    }

    fn pair(&self, code: &str, device_name: &str) -> Result<Account, SessionError> {
        let mut state = self.lock();
        state.take_failure()?;
        self.update(&mut state, &[Part::Account], |saved| {
            let server = saved.server.clone().ok_or(SessionError::NoServer)?;
            if !saved.claimed {
                return Err(SessionError::NotClaimed);
            }
            let code = pairing_code(code, &server.url)?;
            if normalize_code(code).len() != PAIRING_CODE_LEN {
                return Err(SessionError::InvalidCode);
            }
            let account = Account {
                server_url: server.url,
                user_id: saved.next_id("user"),
                user_name: String::new(),
                device_id: saved.next_id("device"),
                device_name: required(device_name)?,
            };
            if saved.spaces.is_empty() {
                saved.spaces = vec![Space {
                    space_id: saved.next_id("space"),
                    name: "Shared".to_owned(),
                }];
            }
            let pairer = Device {
                device_id: saved.next_id("device"),
                name: "Other device".to_owned(),
                platform: "macos".to_owned(),
                this_device: false,
                added_at_ms: now_ms(),
            };
            saved.devices = vec![pairer, this_device(&account)];
            saved.account = Some(account.clone());
            Ok(account)
        })
    }

    fn sign_out(&self, confirm_last_device: bool) -> Result<(), SessionError> {
        let mut state = self.lock();
        state.take_failure()?;
        let last = (state.saved.devices.iter()).all(|device| device.this_device);
        if last && !confirm_last_device && state.saved.account.is_some() {
            return Err(SessionError::LastDevice);
        }
        let progress = state.progress.take();
        let recent = std::mem::take(&mut state.recent);
        let result = self.update(&mut state, &ALL_PARTS, |saved| {
            saved.account()?;
            *saved = Saved {
                server: saved.server.take(),
                claimed: saved.claimed,
                next_id: saved.next_id,
                ..Saved::default()
            };
            Ok(())
        });
        if result.is_err() {
            state.progress = progress;
            state.recent = recent;
        }
        result
    }

    fn devices(&self) -> Result<Vec<Device>, SessionError> {
        let mut state = self.lock();
        state.take_failure()?;
        state.saved.account()?;
        Ok(state.saved.devices.clone())
    }

    fn revoke_device(&self, device_id: &str) -> Result<(), SessionError> {
        let mut state = self.lock();
        state.take_failure()?;
        self.update(&mut state, &[], |saved| {
            if saved.account()?.device_id == device_id {
                return Err(SessionError::ThisDevice);
            }
            if saved.revoked.iter().any(|revoked| revoked == device_id) {
                return Ok(());
            }
            let listed = saved.devices.len();
            saved.devices.retain(|device| device.device_id != device_id);
            if saved.devices.len() == listed {
                return Err(SessionError::DeviceNotFound);
            }
            saved.revoked.push(device_id.to_owned());
            Ok(())
        })
    }

    fn start_pairing(&self) -> Result<PairingCode, SessionError> {
        let mut state = self.lock();
        state.take_failure()?;
        self.update(&mut state, &[], |saved| {
            saved.account()?;
            let n = saved.next_id;
            saved.next_id += 1;
            let code = (0..PAIRING_CODE_LEN)
                .map(|i| {
                    let alphabet = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
                    alphabet[(n as usize * 7 + i * 13) % alphabet.len()] as char
                })
                .collect();
            Ok(PairingCode {
                code,
                expires_at_ms: now_ms() + PAIRING_CODE_MS,
            })
        })
    }

    fn spaces(&self) -> Result<Vec<Space>, SessionError> {
        let mut state = self.lock();
        state.take_failure()?;
        state.saved.account()?;
        Ok(state.saved.spaces.clone())
    }

    fn create_space(&self, name: &str) -> Result<Space, SessionError> {
        let mut state = self.lock();
        state.take_failure()?;
        self.update(&mut state, &[], |saved| {
            saved.account()?;
            let space = Space {
                name: required(name)?,
                space_id: saved.next_id("space"),
            };
            saved.spaces.push(space.clone());
            Ok(space)
        })
    }

    fn space_usage(&self, space_id: &str) -> Result<SpaceUsage, SessionError> {
        let mut state = self.lock();
        state.take_failure()?;
        state.saved.account()?;
        if let Some((_, error)) = state.usage_refusals.iter().find(|(id, _)| id == space_id) {
            return Err(error.clone());
        }
        let n = state
            .saved
            .spaces
            .iter()
            .position(|space| space.space_id == space_id)
            .ok_or(SessionError::SpaceNotFound)? as u64
            + 1;
        Ok(SpaceUsage {
            live_files: 1_204 * n,
            live_bytes: 3_200_000_000 * n,
            trashed_files: 12 * n,
            trash_bytes: 40_100_000 * n,
            old_versions: 30 * n,
            old_version_bytes: 120_000_000 * n,
            unreferenced_bytes: 1_500_000 * n,
        })
    }

    fn link_folder(&self, space_id: &str, path: &str) -> Result<LinkedFolder, SessionError> {
        let mut state = self.lock();
        state.take_failure()?;
        self.update(&mut state, &[Part::Folders], |saved| {
            saved.account()?;
            let space = saved
                .spaces
                .iter()
                .find(|s| s.space_id == space_id)
                .cloned()
                .ok_or(SessionError::SpaceNotFound)?;
            let folder = Path::new(path);
            if !folder.is_absolute() || !folder.is_dir() {
                return Err(SessionError::FolderNotFound);
            }
            if saved.folders.iter().any(|f| f.space_id == space_id) {
                return Err(SessionError::SpaceAlreadyLinked);
            }
            if let Some(other) = saved.folders.iter().find(|f| {
                let other = Path::new(&f.path);
                folder.starts_with(other) || other.starts_with(folder)
            }) {
                return Err(SessionError::FolderOverlaps {
                    other: other.path.clone(),
                });
            }
            let linked = LinkedFolder {
                space_id: space.space_id,
                space_name: space.name,
                path: folder.to_string_lossy().into_owned(),
                state: FolderState::UpToDate,
            };
            saved.folders.push(linked.clone());
            Ok(linked)
        })
    }

    fn unlink_folder(&self, space_id: &str) -> Result<(), SessionError> {
        let mut state = self.lock();
        state.take_failure()?;
        let parts = [
            Part::Folders,
            Part::Questions,
            Part::Problems,
            Part::Notices,
        ];
        self.update(&mut state, &parts, |saved| {
            saved.account()?;
            if !saved.folders.iter().any(|f| f.space_id == space_id) {
                return Err(SessionError::FolderNotLinked);
            }
            saved.folders.retain(|f| f.space_id != space_id);
            saved.questions.retain(|q| q.space_id != space_id);
            saved.notices.retain(|n| n.space_id != space_id);
            saved
                .problems
                .retain(|p| p.space_id.as_deref() != Some(space_id));
            Ok(())
        })
    }

    fn trash(&self, path: &str) -> Result<Trash, SessionError> {
        let mut state = self.lock();
        state.take_failure()?;
        state.saved.account()?;
        let (folder, below) = located(&state.saved.folders, path)?;
        let inside = |item: &&DeletedItem| Path::new(&item.path).starts_with(&below);
        let items = (state.saved.history.get(&folder.space_id))
            .map(|history| history.trash.iter().filter(inside).cloned().collect())
            .unwrap_or_default();
        Ok(Trash {
            folder: folder.path.clone(),
            items,
        })
    }

    fn versions(&self, path: &str) -> Result<FileVersions, SessionError> {
        let mut state = self.lock();
        state.take_failure()?;
        state.saved.account()?;
        let (folder, below) = located_inside(&state.saved.folders, path)?;
        (state.saved.history.get(&folder.space_id))
            .and_then(|history| history.versions.iter().find(|file| file.path == below))
            .cloned()
            .ok_or(SessionError::NothingAtPath)
    }

    fn restore_deleted(&self, path: &str, node_id: &str) -> Result<RestoreAsked, SessionError> {
        let mut state = self.lock();
        state.take_failure()?;
        self.update(&mut state, &[], |saved| {
            saved.account()?;
            let (folder, below) = located_inside(&saved.folders, path)?;
            let (space_id, folder) = (folder.space_id.clone(), folder.path.clone());
            let deleted = (saved.history.get(&space_id).into_iter())
                .flat_map(|history| &history.trash)
                .any(|item| item.node_id == node_id && item.path == below);
            if !deleted {
                return Err(SessionError::DeletedItemNotFound);
            }
            let target = RestoreTarget::Deleted {
                node_id: node_id.to_owned(),
            };
            Ok(saved.ask_restore(space_id, folder, below, target))
        })
    }

    fn restore_version(&self, path: &str, version_id: &str) -> Result<RestoreAsked, SessionError> {
        let mut state = self.lock();
        state.take_failure()?;
        self.update(&mut state, &[], |saved| {
            saved.account()?;
            let (folder, below) = located_inside(&saved.folders, path)?;
            let (space_id, folder) = (folder.space_id.clone(), folder.path.clone());
            let file = (saved.history.get(&space_id))
                .and_then(|history| history.versions.iter().find(|file| file.path == below))
                .ok_or(SessionError::NothingAtPath)?;
            let version = (file.versions.iter())
                .find(|version| version.version_id == version_id)
                .ok_or(SessionError::VersionNotFound)?;
            if version.current {
                return Err(SessionError::VersionIsCurrent);
            }
            let target = RestoreTarget::Version {
                node_id: file.node_id.clone(),
                version_id: version_id.to_owned(),
            };
            Ok(saved.ask_restore(space_id, folder, below, target))
        })
    }

    fn restore_requests(&self, path: &str) -> Result<Vec<RestoreRequest>, SessionError> {
        let mut state = self.lock();
        state.take_failure()?;
        let (folder, _) = located(&state.saved.folders, path)?;
        Ok((state.saved.history.get(&folder.space_id))
            .map(|history| history.requests.clone())
            .unwrap_or_default())
    }

    fn answer(&self, question_id: &str, answer: Answer) -> Result<(), SessionError> {
        let mut state = self.lock();
        state.take_failure()?;
        let parts = [Part::Questions, Part::Folders];
        self.update(&mut state, &parts, |saved| {
            let index = saved
                .questions
                .iter()
                .position(|q| q.id == question_id)
                .ok_or(SessionError::QuestionNotFound)?;
            if !saved.questions[index].answers.contains(&answer) {
                return Err(SessionError::AnswerNotAllowed);
            }
            let space_id = saved.questions.remove(index).space_id;
            let recovering = saved
                .questions
                .iter()
                .any(|q| q.space_id == space_id && matches!(q.kind, QuestionKind::Recovery { .. }));
            if !recovering {
                for folder in &mut saved.folders {
                    if folder.space_id == space_id && folder.state == FolderState::Recovering {
                        folder.state = FolderState::UpToDate;
                    }
                }
            }
            Ok(())
        })?;
        state.answers.push(GivenAnswer {
            question_id: question_id.to_owned(),
            answer,
        });
        Ok(())
    }

    fn telemetry(&self) -> TelemetrySetting {
        self.lock().telemetry
    }

    fn set_telemetry(&self, enabled: bool) -> Result<(), SessionError> {
        let mut state = self.lock();
        state.take_failure()?;
        if state.fail_saves {
            return Err(SessionError::NotSaved {
                reason: "saving is scripted to fail".to_owned(),
            });
        }
        let telemetry = &mut state.telemetry;
        telemetry.decided = true;
        if !telemetry.locked {
            telemetry.enabled = enabled;
        }
        Ok(())
    }

    fn dismiss_notice(&self, notice_id: &str) -> Result<(), SessionError> {
        let mut state = self.lock();
        state.take_failure()?;
        self.update(&mut state, &[Part::Notices], |saved| {
            if !saved.notices.iter().any(|n| n.id == notice_id) {
                return Err(SessionError::NoticeNotFound);
            }
            saved.notices.retain(|n| n.id != notice_id);
            Ok(())
        })
    }
}

mod scenario;

#[cfg(test)]
mod tests {
    use super::scenario::{DOCUMENTS, PHOTOS};
    use super::*;
    use crate::core::SessionListener;
    use std::fs;
    use std::sync::Arc;

    struct Recorder(Arc<Mutex<Vec<SessionEvent>>>);

    impl SessionListener for Recorder {
        fn on_event(&self, event: SessionEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    struct Harness {
        core: FakeCore,
        seen: Arc<Mutex<Vec<SessionEvent>>>,
        _listener: Arc<Box<dyn SessionListener>>,
    }

    impl Harness {
        fn new() -> Self {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let listener: Arc<Box<dyn SessionListener>> =
                Arc::new(Box::new(Recorder(seen.clone())));
            let core = FakeCore::new(Events::new(&listener));
            Self {
                core,
                seen,
                _listener: listener,
            }
        }

        fn take(&self) -> Vec<SessionEvent> {
            std::mem::take(&mut *self.seen.lock().unwrap())
        }

        fn statuses(&self) -> Vec<SyncStatus> {
            self.take()
                .into_iter()
                .filter_map(|e| match e {
                    SessionEvent::Status { status } => Some(status),
                    _ => None,
                })
                .collect()
        }

        fn question_ids(&self) -> Vec<String> {
            self.core
                .state()
                .questions
                .into_iter()
                .map(|q| q.id)
                .collect()
        }
    }

    fn folder_of(core: &FakeCore, space: &str) -> LinkedFolder {
        core.state()
            .folders
            .into_iter()
            .find(|f| f.space_id == space)
            .unwrap()
    }

    #[test]
    fn lifecycle_reports_each_status_once() {
        let h = Harness::new();
        assert_eq!(h.core.state().status, SyncStatus::Stopped);
        h.core.start();
        h.core.start();
        assert_eq!(h.statuses(), [SyncStatus::NeedsSetup]);

        h.core.load_scenario(FakeScenario::UpToDate).unwrap();
        h.core.pause().unwrap();
        h.core.pause().unwrap();
        h.core.resume().unwrap();
        h.core.set_activity(FakeActivity::Syncing);
        h.core.set_activity(FakeActivity::Offline);
        h.core.stop();
        h.core.stop();
        assert_eq!(
            h.statuses(),
            [
                SyncStatus::Idle,
                SyncStatus::Paused,
                SyncStatus::Idle,
                SyncStatus::Syncing,
                SyncStatus::Offline,
                SyncStatus::Stopped,
            ]
        );
    }

    #[test]
    fn telemetry_starts_undecided_and_a_locked_setting_keeps_its_state() {
        let h = Harness::new();
        assert_eq!(h.core.telemetry(), TelemetrySetting::default());
        h.core.set_telemetry(true).unwrap();
        assert_eq!(
            h.core.telemetry(),
            TelemetrySetting {
                enabled: true,
                decided: true,
                locked: false
            }
        );
        h.core.load_scenario(FakeScenario::FirstRun).unwrap();
        assert!(!h.core.telemetry().decided);
        h.core.load_scenario(FakeScenario::UpToDate).unwrap();
        assert!(h.core.telemetry().enabled);

        let locked = TelemetrySetting {
            enabled: false,
            decided: false,
            locked: true,
        };
        h.core.fail_saves(true);
        assert!(matches!(
            h.core.set_telemetry(false),
            Err(SessionError::NotSaved { .. })
        ));
        assert!(h.core.telemetry().enabled);
        h.core.fail_saves(false);

        h.core.set_telemetry_setting(locked);
        h.core.set_telemetry(true).unwrap();
        assert_eq!(
            h.core.telemetry(),
            TelemetrySetting {
                decided: true,
                ..locked
            }
        );
    }

    #[test]
    fn pause_needs_a_running_session_and_outlives_a_restart() {
        let h = Harness::new();
        h.core.load_scenario(FakeScenario::UpToDate).unwrap();
        h.core.pause().unwrap();
        h.core.start();
        assert_eq!(h.core.state().status, SyncStatus::Idle);

        h.core.pause().unwrap();
        h.core.stop();
        h.core.start();
        assert_eq!(h.core.state().status, SyncStatus::Paused);
    }

    #[test]
    fn claiming_signs_in_the_owner() {
        let h = Harness::new();
        h.core.start();
        assert_eq!(
            h.core.claim("ABCD-EFGH-JKLM", "Ada", "Mac"),
            Err(SessionError::NoServer)
        );
        assert_eq!(
            h.core.connect("ftp://box"),
            Err(SessionError::InvalidServerUrl)
        );
        assert_eq!(
            h.core.connect("https://"),
            Err(SessionError::InvalidServerUrl)
        );
        let server = h.core.connect(" https://box.example.com/ ").unwrap();
        assert_eq!(server.url, "https://box.example.com");
        assert!(!server.claimed);

        assert_eq!(
            h.core.pair("ABCD-EFGH", "Mac"),
            Err(SessionError::NotClaimed)
        );
        assert_eq!(
            h.core.claim("ABCD", "Ada", "Mac"),
            Err(SessionError::InvalidCode)
        );
        assert_eq!(
            h.core.claim("abcd-efgh-jklm", " ", "Mac"),
            Err(SessionError::EmptyName)
        );
        let account = h.core.claim("abcd-efgh-jklm", "Ada", "Mac").unwrap();
        assert_eq!(account.server_url, "https://box.example.com");
        assert_eq!(account.user_name, "Ada");
        assert_eq!(h.core.state().account, Some(account.clone()));
        let devices = h.core.devices().unwrap();
        assert_eq!(
            (devices.iter())
                .map(|device| (device.device_id.as_str(), device.this_device))
                .collect::<Vec<_>>(),
            [(account.device_id.as_str(), true)]
        );
        assert!(h.take().contains(&SessionEvent::Account {
            account: Some(account)
        }));
        assert!(h.core.connect("https://box.example.com").unwrap().claimed);
    }

    #[test]
    fn pairing_signs_in_another_device() {
        let h = Harness::new();
        h.core.set_server_claimed(true).unwrap();
        h.core.connect("https://box.example.com").unwrap();
        assert_eq!(
            h.core.claim("ABCD-EFGH-JKLM", "Ada", "Mac"),
            Err(SessionError::AlreadyClaimed)
        );
        assert_eq!(h.core.start_pairing(), Err(SessionError::NotSignedIn));
        assert_eq!(
            h.core.pair("ABCDEFGHJ", "Mac"),
            Err(SessionError::InvalidCode)
        );
        assert_eq!(
            h.core.pair("ABCD-EFGH@koidrive.alice.workers.dev", "Mac"),
            Err(SessionError::CodeForOtherServer {
                host: "koidrive.alice.workers.dev".to_owned(),
                server: "box.example.com".to_owned(),
            })
        );
        let account = h.core.pair(" abcd efgh@Box.Example.com ", "Mac").unwrap();
        let devices = h.core.devices().unwrap();
        assert_eq!(
            (devices.iter())
                .filter(|device| device.this_device)
                .map(|device| device.device_id.as_str())
                .collect::<Vec<_>>(),
            [account.device_id.as_str()]
        );
        assert_eq!(devices.len(), 2);
        assert_eq!(h.core.spaces().unwrap().len(), 1);

        let code = h.core.start_pairing().unwrap();
        assert_eq!(normalize_code(&code.code).len(), PAIRING_CODE_LEN);
        assert!(code.expires_at_ms > now_ms());
    }

    #[test]
    fn scripted_failures_apply_to_the_next_call_only() {
        let h = Harness::new();
        let unreachable = SessionError::ServerUnreachable {
            reason: "timed out".to_owned(),
        };
        h.core.fail_next(unreachable.clone());
        assert_eq!(h.core.connect("https://box.example.com"), Err(unreachable));
        assert!(h.core.connect("https://box.example.com").is_ok());
    }

    #[test]
    fn linking_checks_the_space_and_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        let inside = dir.path().join("inside");
        fs::create_dir(&inside).unwrap();
        let h = Harness::new();
        h.core.start();
        assert_eq!(h.core.spaces(), Err(SessionError::NotSignedIn));
        h.core.connect("https://box.example.com").unwrap();
        h.core.claim("ABCDEFGHJKLM", "Ada", "Mac").unwrap();
        assert_eq!(h.core.create_space(""), Err(SessionError::EmptyName));
        let one = h.core.create_space("One").unwrap();
        let two = h.core.create_space("Two").unwrap();
        assert_eq!(h.core.spaces().unwrap(), [one.clone(), two.clone()]);
        assert!(
            h.core.space_usage(&two.space_id).unwrap().live_bytes
                > h.core.space_usage(&one.space_id).unwrap().live_bytes
        );
        assert_eq!(
            h.core.space_usage("no-such-space"),
            Err(SessionError::SpaceNotFound)
        );

        let path = dir.path().to_str().unwrap();
        assert_eq!(
            h.core.link_folder("nope", path),
            Err(SessionError::SpaceNotFound)
        );
        assert_eq!(
            h.core.link_folder(&one.space_id, "relative"),
            Err(SessionError::FolderNotFound)
        );
        assert_eq!(
            h.core
                .link_folder(&one.space_id, &format!("{path}/missing")),
            Err(SessionError::FolderNotFound)
        );
        h.take();
        let linked = h.core.link_folder(&one.space_id, path).unwrap();
        assert_eq!(linked.space_name, "One");
        assert_eq!(h.core.state().status, SyncStatus::Idle);
        assert_eq!(
            h.take(),
            [
                SessionEvent::Folders {
                    folders: vec![linked]
                },
                SessionEvent::Status {
                    status: SyncStatus::Idle
                },
            ]
        );
        assert_eq!(
            h.core.link_folder(&one.space_id, inside.to_str().unwrap()),
            Err(SessionError::SpaceAlreadyLinked)
        );
        assert_eq!(
            h.core.link_folder(&two.space_id, inside.to_str().unwrap()),
            Err(SessionError::FolderOverlaps {
                other: path.to_owned()
            })
        );

        assert_eq!(
            h.core.unlink_folder(&two.space_id),
            Err(SessionError::FolderNotLinked)
        );
        h.core.unlink_folder(&one.space_id).unwrap();
        assert!(h.core.state().folders.is_empty());
        assert_eq!(h.core.state().status, SyncStatus::NeedsSetup);
    }

    #[test]
    fn signing_out_unlinks_everything() {
        let h = Harness::new();
        h.core.load_scenario(FakeScenario::MassDelete).unwrap();
        h.core.start();
        h.core.sign_out(false).unwrap();
        let state = h.core.state();
        assert_eq!(state.status, SyncStatus::NeedsSetup);
        assert!(state.account.is_none() && state.folders.is_empty() && state.questions.is_empty());
        assert_eq!(h.core.sign_out(false), Err(SessionError::NotSignedIn));
    }

    #[test]
    fn every_question_kind_takes_only_its_own_answers() {
        let h = Harness::new();
        h.core.load_scenario(FakeScenario::ManyQuestions).unwrap();
        let questions = h.core.state().questions;
        assert_eq!(questions.len(), 31);
        for question in &questions {
            assert_eq!(question.answers.len(), 2);
            let wrong = [
                Answer::Delete,
                Answer::Restore,
                Answer::Keep,
                Answer::Discard,
                Answer::Publish,
                Answer::Repeat,
                Answer::Cancel,
            ]
            .into_iter()
            .find(|a| !question.answers.contains(a))
            .unwrap();
            assert_eq!(
                h.core.answer(&question.id, wrong),
                Err(SessionError::AnswerNotAllowed)
            );
        }
        assert_eq!(h.core.state().questions.len(), 31);

        for question in &questions {
            h.core.answer(&question.id, question.answers[1]).unwrap();
            assert_eq!(
                h.core.answer(&question.id, question.answers[1]),
                Err(SessionError::QuestionNotFound)
            );
        }
        assert!(h.core.state().questions.is_empty());
        assert_eq!(h.core.given_answers().len(), 31);
        assert_eq!(h.core.given_answers()[0].answer, Answer::Restore);
    }

    #[test]
    fn scenarios_hold_long_lists() {
        let h = Harness::new();
        h.core.load_scenario(FakeScenario::ManyQuestions).unwrap();
        let questions = h.core.state().questions;
        assert_eq!(questions[0].items.len(), 1_500);
        let recovery: Vec<_> = questions
            .iter()
            .filter_map(|q| match &q.kind {
                QuestionKind::Recovery { group, .. } => Some((*group, q.items.len())),
                _ => None,
            })
            .collect();
        assert_eq!(recovery.len(), 5);
        assert!(recovery.iter().all(|(_, n)| *n == 300));
        assert_eq!(
            questions
                .iter()
                .filter(|q| q.kind == QuestionKind::AmbiguousDisappearance)
                .count(),
            25
        );
    }

    #[test]
    fn answering_the_last_recovery_question_ends_the_recovery() {
        let h = Harness::new();
        h.core.load_scenario(FakeScenario::Recovery).unwrap();
        assert_eq!(folder_of(&h.core, PHOTOS).state, FolderState::Recovering);
        let ids = h.question_ids();
        assert_eq!(ids.len(), 5);
        for question in h.core.state().questions {
            let QuestionKind::Recovery { cause, .. } = &question.kind else {
                panic!("not a recovery question");
            };
            assert_eq!(cause.trigger, RecoveryTrigger::SpaceRewound);
            assert!(cause.restore_point_ms.is_some());
        }
        for (i, question) in h.core.state().questions.into_iter().enumerate() {
            assert_eq!(
                folder_of(&h.core, PHOTOS).state,
                FolderState::Recovering,
                "{i}"
            );
            h.core.answer(&question.id, question.answers[0]).unwrap();
        }
        assert_eq!(folder_of(&h.core, PHOTOS).state, FolderState::UpToDate);
    }

    #[test]
    fn a_replaced_question_refuses_answers() {
        let h = Harness::new();
        h.core.load_scenario(FakeScenario::UpToDate).unwrap();
        let kind = QuestionKind::AmbiguousDisappearance;
        let item = |p: &str| HeldItem {
            node_id: p.to_owned(),
            path: p.to_owned(),
            kind: NodeKind::File,
            files: 1,
            detail: ItemDetail::Plain,
        };
        let old = h
            .core
            .ask(DOCUMENTS.to_owned(), kind.clone(), vec![item("a")])
            .unwrap();
        h.core.withdraw_question(old.clone()).unwrap();
        let new = h
            .core
            .ask(DOCUMENTS.to_owned(), kind, vec![item("a"), item("b")])
            .unwrap();
        assert_eq!(
            h.core.answer(&old, Answer::Delete),
            Err(SessionError::QuestionNotFound)
        );
        assert_eq!(h.question_ids(), std::slice::from_ref(&new));
        h.core.answer(&new, Answer::Delete).unwrap();
        assert_eq!(
            h.core.given_answers(),
            [GivenAnswer {
                question_id: new,
                answer: Answer::Delete
            }]
        );
    }

    #[test]
    fn unanswered_questions_are_asked_again_after_a_restart() {
        let h = Harness::new();
        h.core.load_scenario(FakeScenario::Recovery).unwrap();
        h.core.start();
        h.core.pause().unwrap();
        let answered = h.core.state().questions[0].clone();
        h.core.answer(&answered.id, answered.answers[0]).unwrap();
        let open = h.core.state().questions;
        assert_eq!(open.len(), 4);
        h.core.stop();
        h.take();

        assert_eq!(h.core.state().status, SyncStatus::Stopped);
        h.core.start();
        assert_eq!(
            h.take(),
            [
                SessionEvent::Questions {
                    questions: open.clone()
                },
                SessionEvent::Notices {
                    notices: h.core.state().notices
                },
                SessionEvent::Status {
                    status: SyncStatus::Paused
                },
            ]
        );
        assert_eq!(folder_of(&h.core, PHOTOS).state, FolderState::Recovering);
    }

    #[test]
    fn recent_changes_keep_the_newest() {
        let h = Harness::new();
        for i in 0..(RECENT_CHANGES + 5) {
            h.core.add_recent_change(RecentChange {
                space_id: DOCUMENTS.to_owned(),
                path: format!("f{i}"),
                kind: NodeKind::File,
                change: ChangeKind::Added,
                direction: Direction::Uploaded,
                at_ms: i as i64,
            });
        }
        let recent = h.core.state().recent_changes;
        assert_eq!(recent.len(), RECENT_CHANGES);
        assert_eq!(recent[0].path, format!("f{}", RECENT_CHANGES + 4));
    }

    #[test]
    fn problems_scenario_has_every_kind_and_halts_the_missing_folder() {
        let h = Harness::new();
        h.core.load_scenario(FakeScenario::Problems).unwrap();
        let problems = h.core.state().problems;
        assert_eq!(problems.len(), 19);
        assert_eq!(folder_of(&h.core, PHOTOS).state, FolderState::Halted);
        let id = problems[0].id.clone();
        h.core.resolve_problem(id.clone()).unwrap();
        assert!(h.core.state().problems.iter().all(|p| p.id != id));

        let reported = h
            .core
            .report_problem(None, None, ProblemKind::SignInRequired)
            .unwrap();
        assert_eq!(h.core.state().problems.last().unwrap().id, reported);
    }

    #[test]
    fn events_stop_after_the_listener_is_gone() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let listener: Arc<Box<dyn SessionListener>> = Arc::new(Box::new(Recorder(seen.clone())));
        let core = FakeCore::new(Events::new(&listener));
        core.start();
        drop(listener);
        core.load_scenario(FakeScenario::UpToDate).unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            [
                SessionEvent::Questions { questions: vec![] },
                SessionEvent::Notices { notices: vec![] },
                SessionEvent::Status {
                    status: SyncStatus::NeedsSetup
                },
            ]
        );
    }

    #[test]
    fn another_device_is_revoked_but_not_this_one() {
        let h = Harness::new();
        h.core.load_scenario(FakeScenario::UpToDate).unwrap();
        let listed = |h: &Harness| {
            (h.core.devices().unwrap().into_iter())
                .map(|device| device.device_id)
                .collect::<Vec<_>>()
        };
        assert_eq!(listed(&h), ["device-1", "device-2"]);
        assert_eq!(
            h.core.revoke_device("device-1"),
            Err(SessionError::ThisDevice)
        );
        h.core.revoke_device("device-2").unwrap();
        assert_eq!(listed(&h), ["device-1"]);
        h.core.revoke_device("device-2").unwrap();
        assert_eq!(
            h.core.revoke_device("device-9"),
            Err(SessionError::DeviceNotFound)
        );
    }

    #[test]
    fn the_last_device_signs_out_only_once_confirmed() {
        let h = Harness::new();
        h.core.load_scenario(FakeScenario::UpToDate).unwrap();
        let device = |device_id: &str, this_device: bool| Device {
            device_id: device_id.to_owned(),
            name: "Mac".to_owned(),
            platform: "macos".to_owned(),
            this_device,
            added_at_ms: 1,
        };
        h.core.set_devices(vec![device("device-1", true)]).unwrap();
        assert_eq!(h.core.sign_out(false), Err(SessionError::LastDevice));
        assert!(h.core.state().account.is_some());
        h.core.sign_out(true).unwrap();
        assert_eq!(h.core.state().account, None);

        h.core.load_scenario(FakeScenario::UpToDate).unwrap();
        h.core
            .set_devices(vec![device("device-1", true), device("device-2", false)])
            .unwrap();
        h.core.sign_out(false).unwrap();
        assert_eq!(h.core.state().account, None);
    }

    #[test]
    fn a_recovery_without_questions_still_has_a_notice() {
        let h = Harness::new();
        h.core
            .load_scenario(FakeScenario::RecoveredWithoutQuestions)
            .unwrap();
        let state = h.core.state();
        assert!(state.questions.is_empty());
        let [notice] = state.notices.as_slice() else {
            panic!("expected one notice: {:?}", state.notices);
        };
        assert_eq!(notice.space_id, PHOTOS);
        let NoticeKind::Recovery { cause, held_files } = &notice.kind else {
            panic!("not a recovery notice");
        };
        assert_eq!(cause.trigger, RecoveryTrigger::SpaceRewound);
        assert_eq!(*held_files, 0);

        h.core.load_scenario(FakeScenario::Recovery).unwrap();
        let notices = h.core.state().notices;
        assert!(matches!(
            notices[0].kind,
            NoticeKind::Recovery { held_files: 30, .. }
        ));
    }

    #[test]
    fn a_remote_mass_delete_is_reported_without_asking() {
        let h = Harness::new();
        h.core
            .load_scenario(FakeScenario::RemoteMassDelete)
            .unwrap();
        let state = h.core.state();
        assert!(state.questions.is_empty());
        let notice = state.notices[0].clone();
        assert_eq!(
            notice.kind,
            NoticeKind::RemoteMassDelete {
                deletions: 3,
                affected_files: 1_200,
                space_files: 2_000,
                exceeded: vec![MassDeleteLimit::ShareOfSpace { percent: 50 }],
            }
        );
        h.take();
        h.core.dismiss_notice(&notice.id).unwrap();
        assert!(h.core.state().notices.is_empty());
        assert_eq!(h.take(), [SessionEvent::Notices { notices: vec![] }]);
        assert_eq!(
            h.core.dismiss_notice(&notice.id),
            Err(SessionError::NoticeNotFound)
        );
        let id = h
            .core
            .notify(
                DOCUMENTS.to_owned(),
                NoticeKind::Recovery {
                    cause: RecoveryCause {
                        trigger: RecoveryTrigger::NoSyncState,
                        restore_point_ms: None,
                    },
                    held_files: 2,
                },
            )
            .unwrap();
        assert_eq!(h.core.state().notices[0].id, id);
    }

    fn not_writable(core: &FakeCore) -> Option<ProblemKind> {
        core.state()
            .problems
            .into_iter()
            .find(|p| p.id == NOT_WRITABLE_ID)
            .map(|p| p.kind)
    }

    #[test]
    fn an_answer_that_cannot_be_saved_is_not_taken() {
        let h = Harness::new();
        h.core.load_scenario(FakeScenario::MassDelete).unwrap();
        h.core.start();
        let question = h.core.state().questions[0].clone();
        h.take();

        h.core.fail_saves(true);
        let reason = "saving is scripted to fail".to_owned();
        assert_eq!(
            h.core.answer(&question.id, Answer::Delete),
            Err(SessionError::NotSaved {
                reason: reason.clone()
            })
        );
        assert_eq!(h.core.state().questions, std::slice::from_ref(&question));
        assert!(h.core.given_answers().is_empty());
        assert_eq!(
            not_writable(&h.core),
            Some(ProblemKind::StateNotWritable { reason })
        );
        assert!(matches!(
            h.take().as_slice(),
            [SessionEvent::Problems { .. }]
        ));
        assert!(matches!(h.core.pause(), Err(SessionError::NotSaved { .. })));
        assert_eq!(h.core.state().status, SyncStatus::Idle);
        assert!(matches!(
            h.core.dismiss_notice("none"),
            Err(SessionError::NoticeNotFound)
        ));

        h.core.fail_saves(false);
        h.core.answer(&question.id, Answer::Delete).unwrap();
        assert_eq!(not_writable(&h.core), None);
        assert!(h.core.state().questions.is_empty());
    }

    #[test]
    fn a_scripted_change_that_cannot_be_saved_changes_nothing() {
        let h = Harness::new();
        h.core.load_scenario(FakeScenario::UpToDate).unwrap();
        h.take();
        h.core.fail_saves(true);
        let kind = QuestionKind::AmbiguousDisappearance;
        assert!(matches!(
            h.core.ask(DOCUMENTS.to_owned(), kind.clone(), vec![]),
            Err(SessionError::NotSaved { .. })
        ));
        assert!(h.question_ids().is_empty());
        assert!(not_writable(&h.core).is_some());
        let since = h.core.state().problems.last().unwrap().since_ms;
        assert!(
            h.core
                .report_problem(None, None, ProblemKind::FileBusy)
                .is_err()
        );
        assert!(h.core.load_scenario(FakeScenario::MassDelete).is_err());
        assert_eq!(h.core.state().problems.len(), 1);
        assert_eq!(h.core.state().problems[0].since_ms, since);
        assert!(h.core.state().questions.is_empty());
        assert!(
            h.take()
                .iter()
                .all(|e| matches!(e, SessionEvent::Problems { .. })),
            "only the problem list may change"
        );

        h.core.fail_saves(false);
        h.core.ask(DOCUMENTS.to_owned(), kind, vec![]).unwrap();
        assert_eq!(not_writable(&h.core), None);
        assert_eq!(h.question_ids().len(), 1);
    }

    #[test]
    fn scenario_and_sign_out_events_follow_the_state() {
        let h = Harness::new();
        h.core.start();
        h.take();
        h.core.load_scenario(FakeScenario::Syncing).unwrap();
        let events = h.take();
        assert_eq!(events.len(), ALL_PARTS.len() + 1, "{events:?}");
        assert_eq!(
            events.last(),
            Some(&SessionEvent::Status {
                status: SyncStatus::Syncing
            })
        );

        h.core.sign_out(false).unwrap();
        let events = h.take();
        assert!(events.contains(&SessionEvent::Progress { progress: None }));
        assert!(events.contains(&SessionEvent::RecentChanges { changes: vec![] }));
        assert_eq!(
            events.last(),
            Some(&SessionEvent::Status {
                status: SyncStatus::NeedsSetup
            })
        );
        assert_eq!(events.len(), ALL_PARTS.len() + 1, "{events:?}");
    }

    #[test]
    fn a_sign_out_that_cannot_be_saved_keeps_progress_and_recent_changes() {
        let h = Harness::new();
        h.core.load_scenario(FakeScenario::Syncing).unwrap();
        h.core.fail_saves(true);
        assert!(h.core.sign_out(false).is_err());
        let state = h.core.state();
        assert!(state.account.is_some() && state.progress.is_some());
        assert!(!state.recent_changes.is_empty());
    }

    #[test]
    fn scenario_lists_keep_their_documented_order() {
        let h = Harness::new();
        h.core.load_scenario(FakeScenario::ManyQuestions).unwrap();
        for question in h.core.state().questions {
            assert!(
                question.items.is_sorted_by(|a, b| a.path <= b.path),
                "{}",
                question.id
            );
        }
        h.core.load_scenario(FakeScenario::Problems).unwrap();
        assert!(h.core.state().problems.is_sorted_by_key(|p| p.since_ms));
    }

    #[test]
    fn history_needs_a_path_in_a_linked_folder() {
        let h = Harness::new();
        h.core.load_scenario(FakeScenario::UpToDate).unwrap();
        let documents = folder_of(&h.core, DOCUMENTS).path;
        let at = |below: &str| format!("{documents}/{below}");
        let report = at("Reports/Annual Report.docx");
        let outside = |path: &str| {
            Err(SessionError::PathNotInLinkedFolder {
                path: path.to_owned(),
            })
        };

        assert_eq!(h.core.trash("/elsewhere").map(drop), outside("/elsewhere"));
        assert_eq!(h.core.trash("Documents").map(drop), outside("Documents"));
        assert_eq!(
            h.core.restore_requests("Documents").map(drop),
            outside("Documents")
        );
        assert_eq!(
            h.core.versions(&documents),
            Err(SessionError::PathIsLinkedFolder)
        );
        assert_eq!(
            h.core.restore_deleted(&documents, "node-budget"),
            Err(SessionError::PathIsLinkedFolder)
        );
        assert_eq!(
            h.core.versions(&at("Notes.txt")),
            Err(SessionError::NothingAtPath)
        );
        assert_eq!(
            h.core.restore_deleted(&at("Q3 Budget.xlsx"), "node-letter"),
            Err(SessionError::DeletedItemNotFound)
        );
        assert_eq!(
            h.core.restore_version(&report, "version-3"),
            Err(SessionError::VersionIsCurrent)
        );
        assert_eq!(
            h.core.restore_version(&report, "version-9"),
            Err(SessionError::VersionNotFound)
        );

        let trash = h.core.trash(&documents).unwrap();
        let paths: Vec<&str> = trash.items.iter().map(|item| item.path.as_str()).collect();
        assert_eq!(trash.folder, documents);
        assert_eq!(
            paths,
            [
                "Drafts/Letter to the landlord.docx",
                "Old Slides",
                "Q3 Budget.xlsx"
            ]
        );
        let drafts = h.core.trash(&at("Drafts")).unwrap();
        assert_eq!(drafts.items.len(), 1);
        assert_eq!(h.core.versions(&report).unwrap().versions.len(), 3);
        assert!(h.core.restore_requests(&documents).unwrap().is_empty());
    }

    #[test]
    fn a_restore_waits_until_it_is_finished_and_a_restored_item_leaves_the_trash() {
        let h = Harness::new();
        h.core.load_scenario(FakeScenario::UpToDate).unwrap();
        let documents = folder_of(&h.core, DOCUMENTS).path;
        let at = |below: &str| format!("{documents}/{below}");
        let in_trash = |node: &str| {
            (h.core.trash(&documents).unwrap().items.iter()).any(|item| item.node_id == node)
        };

        h.core.fail_saves(true);
        assert!(matches!(
            h.core.restore_deleted(&at("Q3 Budget.xlsx"), "node-budget"),
            Err(SessionError::NotSaved { .. })
        ));
        h.core.fail_saves(false);
        assert!(h.core.restore_requests(&documents).unwrap().is_empty());

        let budget = h
            .core
            .restore_deleted(&at("Q3 Budget.xlsx"), "node-budget")
            .unwrap();
        let slides = h
            .core
            .restore_deleted(&at("Old Slides"), "node-old-slides")
            .unwrap();
        let report = (h.core)
            .restore_version(&at("Reports/Annual Report.docx"), "version-1")
            .unwrap();
        assert_eq!(
            (budget.folder.as_str(), budget.path.as_str()),
            (documents.as_str(), "Q3 Budget.xlsx")
        );
        let listed = |path: &str| -> Vec<(String, RestoreState)> {
            (h.core.restore_requests(path).unwrap().into_iter())
                .map(|request| (request.id, request.state))
                .collect()
        };
        let pending = RestoreState::Pending {
            held_by_problem: false,
        };
        assert_eq!(
            listed(&at("Reports")),
            [
                (budget.request_id.clone(), pending.clone()),
                (slides.request_id.clone(), pending.clone()),
                (report.request_id.clone(), pending.clone()),
            ]
        );

        let copy = RestoreState::RestoredAsCopy {
            node_id: "node-copy".into(),
            name: "Annual Report (restored).docx".into(),
        };
        h.core
            .finish_restore(report.request_id.clone(), copy.clone())
            .unwrap();
        h.core
            .finish_restore(slides.request_id.clone(), RestoreState::TooLarge)
            .unwrap();
        assert_eq!(
            listed(&documents),
            [
                (report.request_id.clone(), copy.clone()),
                (slides.request_id.clone(), RestoreState::TooLarge),
                (budget.request_id.clone(), pending),
            ]
        );
        assert!(in_trash("node-budget") && in_trash("node-old-slides"));

        h.core
            .finish_restore(budget.request_id.clone(), RestoreState::Restored)
            .unwrap();
        assert!(!in_trash("node-budget"));
        assert!(in_trash("node-old-slides"));
        assert_eq!(
            listed(&documents)[2],
            (budget.request_id, RestoreState::Restored)
        );
    }
}
