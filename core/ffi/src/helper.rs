// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use koidrive_driver::{self as driver, FolderState, Status};
use koidrive_ipc::{Code, Next, Reason};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::types::{
    Account, DeletedItem, Device, FileVersion, FileVersions, LinkedFolder, NodeKind, PairingCode,
    RestoreAsked, RestoreRefusal, RestoreRequest, RestoreState, RestoreTarget, SessionError, Space,
    SpaceUsage, Trash,
};

#[derive(Clone)]
pub(crate) struct Helper {
    pub(crate) path: PathBuf,
    pub(crate) env: Vec<(&'static str, Option<OsString>)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Outcome {
    Ok,
    Accepted,
    Unchanged,
    Error,
}

#[derive(Debug, Deserialize)]
struct Envelope {
    outcome: Outcome,
    #[serde(default)]
    data: Value,
    error: Option<ErrorBody>,
    #[serde(default)]
    decisions: Vec<DecisionData>,
    #[serde(default)]
    next_commands: Vec<Next>,
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    code: Code,
    #[serde(default, deserialize_with = "known")]
    reason: Option<Reason>,
    message: String,
}

fn known<'de, D: Deserializer<'de>>(reason: D) -> Result<Option<Reason>, D::Error> {
    Ok(Option::<Value>::deserialize(reason)?.and_then(|reason| Reason::deserialize(reason).ok()))
}

#[derive(Debug)]
pub(crate) struct Done {
    pub(crate) outcome: Outcome,
    data: Value,
    decisions: Vec<DecisionData>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Refusal {
    pub(crate) code: Code,
    pub(crate) reason: Option<Reason>,
    pub(crate) message: String,
    pub(crate) next: Vec<Next>,
}

impl Refusal {
    fn failed(message: String) -> Self {
        Refusal {
            code: Code::Failed,
            reason: None,
            message,
            next: Vec::new(),
        }
    }

    pub(crate) fn names(&self, flag: &str) -> bool {
        (self.next.iter()).any(|next| next.argv.iter().any(|arg| arg == flag))
    }

    pub(crate) fn at(self, path: &str) -> SessionError {
        match self.reason {
            Some(Reason::NotInLinkedFolder) => SessionError::PathNotInLinkedFolder {
                path: path.to_owned(),
            },
            Some(Reason::LinkedFolderItself) => SessionError::PathIsLinkedFolder,
            Some(Reason::NotFound) => SessionError::NothingAtPath,
            Some(Reason::IsFolder) => SessionError::PathIsFolder,
            Some(Reason::NotDeleted) => SessionError::NotDeleted,
            Some(Reason::NotInTrash) => SessionError::DeletedItemNotFound,
            Some(Reason::NoSuchVersion) => SessionError::VersionNotFound,
            Some(Reason::AlreadyCurrent) => SessionError::VersionIsCurrent,
            Some(Reason::SeveralDeletions) | None => self.into(),
        }
    }
}

impl From<Refusal> for SessionError {
    fn from(refusal: Refusal) -> Self {
        match refusal.code {
            Code::AuthRequired => SessionError::NotSignedIn,
            Code::WrongCode => SessionError::InvalidCode,
            Code::Unavailable => SessionError::ServerUnreachable {
                reason: refusal.message,
            },
            _ => SessionError::Failed {
                reason: refusal.message,
            },
        }
    }
}

impl Helper {
    pub(crate) fn action_command(&self, args: &[&str]) -> String {
        let env = [
            "HOME",
            "XDG_CONFIG_HOME",
            "XDG_STATE_HOME",
            "XDG_RUNTIME_DIR",
        ]
        .map(|name| {
            let value = match self.env.iter().find(|(set, _)| *set == name) {
                Some((_, value)) => value.clone(),
                None => std::env::var_os(name),
            };
            (name, value)
        });
        let mut words = vec!["/usr/bin/env".to_owned()];
        for (name, value) in &env {
            if value.is_none() {
                words.extend(["-u".to_owned(), shell_word(name)]);
            }
        }
        for (name, value) in &env {
            if let Some(value) = value {
                let assignment = [name, "=", value.to_string_lossy().as_ref()].concat();
                words.push(shell_word(&assignment));
            }
        }
        words.push(shell_word(&self.path.to_string_lossy()));
        words.extend(args.iter().map(|arg| shell_word(arg)));
        words.join(" ")
    }

    pub(crate) fn run(&self, args: &[&str]) -> Result<Done, Refusal> {
        self.run_with_input(args, None)
    }

    pub(crate) fn run_with_input(
        &self,
        args: &[&str],
        input: Option<&str>,
    ) -> Result<Done, Refusal> {
        let mut command = Command::new(&self.path);
        command
            .arg("--json")
            .args(args)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (name, value) in &self.env {
            match value {
                Some(value) => command.env(name, value),
                None => command.env_remove(name),
            };
        }
        let shown = koidrive_fs::quoted(&self.path);
        let mut child = command
            .spawn()
            .map_err(|error| Refusal::failed(format!("{shown} cannot run: {error}")))?;
        let timeout = match args {
            ["status"] | ["device", "list"] | ["space", "list" | "usage"] => Duration::from_secs(5),
            _ => Duration::from_secs(60),
        };
        let output = finish(&mut child, input, timeout)
            .map_err(|error| Refusal::failed(format!("{shown} cannot finish: {error}")))?;
        let envelope: Envelope = serde_json::from_slice(&output.stdout).map_err(|error| {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Refusal::failed(format!(
                "{shown} printed no result ({error}): {}",
                stderr.trim()
            ))
        })?;
        match envelope.error {
            Some(error) => Err(Refusal {
                code: error.code,
                reason: error.reason,
                message: error.message,
                next: envelope.next_commands,
            }),
            None => Ok(Done {
                outcome: envelope.outcome,
                data: envelope.data,
                decisions: envelope.decisions,
            }),
        }
    }
}

fn finish(child: &mut Child, input: Option<&str>, timeout: Duration) -> io::Result<Output> {
    let result = (|| {
        let deadline = Instant::now() + timeout;
        let mut stdin = child.stdin.take();
        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        if let Some(stdin) = &stdin {
            rustix::io::ioctl_fionbio(stdin, true)?;
        }
        if let Some(stdout) = &stdout {
            rustix::io::ioctl_fionbio(stdout, true)?;
        }
        if let Some(stderr) = &stderr {
            rustix::io::ioctl_fionbio(stderr, true)?;
        }
        let mut input = input.unwrap_or_default().as_bytes();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut status = None;
        loop {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("koi timed out after {} seconds", timeout.as_secs_f64()),
                ));
            }
            if input.is_empty() {
                stdin = None;
            }
            if let Some(pipe) = &mut stdin {
                match pipe.write(input) {
                    Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                    Ok(written) => input = &input[written..],
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => return Err(error),
                }
            }
            collect(&mut stdout, &mut out)?;
            collect(&mut stderr, &mut err)?;
            if status.is_none() {
                status = child.try_wait()?;
            }
            if let Some(status) = status
                && input.is_empty()
                && stdout.is_none()
                && stderr.is_none()
            {
                return Ok(Output {
                    status,
                    stdout: out,
                    stderr: err,
                });
            }
            thread::sleep(Duration::from_millis(5));
        }
    })();
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

fn collect(pipe: &mut Option<impl Read>, bytes: &mut Vec<u8>) -> io::Result<()> {
    let Some(reader) = pipe else { return Ok(()) };
    let mut buffer = [0; 65536];
    match reader.read(&mut buffer) {
        Ok(0) => *pipe = None,
        Ok(read) => bytes.extend_from_slice(&buffer[..read]),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
        Err(error) => return Err(error),
    }
    Ok(())
}

impl Done {
    pub(crate) fn data<T: DeserializeOwned>(&self) -> Result<T, SessionError> {
        T::deserialize(&self.data).map_err(|error| SessionError::Failed {
            reason: format!("koi printed a result this app cannot read: {error}"),
        })
    }

    pub(crate) fn status(&self) -> Result<(Status, Vec<driver::Question>), SessionError> {
        let status: StatusData = self.data()?;
        let status = Status {
            account: status.account.map(AccountData::driver),
            running: status.running,
            paused: status.paused,
            version: None,
            update: driver::Update::Unknown,
            folders: status.folders.into_iter().map(FolderData::driver).collect(),
            progress: None,
        };
        let questions = self
            .decisions
            .iter()
            .filter_map(DecisionData::driver)
            .collect();
        Ok((status, questions))
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct AccountData {
    server: String,
    user_id: String,
    #[serde(default)]
    user_name: Option<String>,
    device_id: String,
    device_name: String,
}

impl AccountData {
    fn driver(self) -> driver::Account {
        driver::Account {
            server: self.server,
            user_id: self.user_id,
            user_name: self.user_name,
            device_id: self.device_id,
            device_name: self.device_name,
        }
    }

    pub(crate) fn account(self) -> Account {
        Account {
            server_url: self.server.trim_end_matches('/').to_owned(),
            user_id: self.user_id,
            user_name: self.user_name.unwrap_or_default(),
            device_id: self.device_id,
            device_name: self.device_name,
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct FolderData {
    path: String,
    space_id: String,
    space_name: String,
    state: FolderState,
    #[serde(default)]
    problem: Option<String>,
}

impl FolderData {
    fn driver(self) -> driver::FolderStatus {
        driver::FolderStatus {
            recovery: None,
            space_id: self.space_id,
            space_name: self.space_name,
            path: self.path,
            state: self.state,
            history_id: None,
            last_seq: None,
            head_seq: None,
            remote_nodes: 0,
            local_entries: None,
            questions: 0,
            problem: self.problem,
            wait: None,
            progress: None,
        }
    }

    pub(crate) fn folder(self) -> LinkedFolder {
        crate::state::folder(&self.driver())
    }
}

#[derive(Debug, Deserialize)]
struct StatusData {
    account: Option<AccountData>,
    running: bool,
    #[serde(default)]
    paused: bool,
    folders: Vec<FolderData>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DecisionKind {
    MassDelete,
    RemoteMassDelete,
    AmbiguousDisappearance,
    Conflict,
    Recovery,
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct DecisionData {
    id: String,
    space_id: String,
    kind: DecisionKind,
    #[serde(default)]
    group: Option<driver::RecoveryGroup>,
    #[serde(default)]
    recovery_cause: Option<driver::RecoveryCause>,
    #[serde(default)]
    conflict: Option<driver::QuestionKind>,
    asked_at_ms: i64,
    #[serde(default)]
    counts: Option<Counts>,
    #[serde(default)]
    items: Vec<ItemData>,
}

#[derive(Debug, Deserialize)]
struct Counts {
    deletions: u64,
    affected_files: u64,
    space_files: u64,
    exceeded: Vec<driver::Limit>,
}

#[derive(Debug, Deserialize)]
struct ItemData {
    node_id: String,
    path: String,
    files: u64,
}

impl DecisionData {
    fn driver(&self) -> Option<driver::Question> {
        let counts = || {
            let counts = self.counts.as_ref()?;
            Some((
                counts.deletions,
                counts.affected_files,
                counts.space_files,
                counts.exceeded.clone(),
            ))
        };
        let kind = match self.kind {
            DecisionKind::MassDelete => {
                let (deletions, affected_files, space_files, exceeded) = counts()?;
                driver::QuestionKind::MassDelete {
                    deletions,
                    affected_files,
                    space_files,
                    exceeded,
                }
            }
            DecisionKind::RemoteMassDelete => {
                let (deletions, affected_files, space_files, exceeded) = counts()?;
                driver::QuestionKind::RemoteMassDelete {
                    deletions,
                    affected_files,
                    space_files,
                    exceeded,
                }
            }
            DecisionKind::AmbiguousDisappearance => driver::QuestionKind::AmbiguousDisappearance,
            DecisionKind::Conflict => match self.conflict.as_ref()? {
                kind @ driver::QuestionKind::Conflict { .. } => kind.clone(),
                _ => return None,
            },
            DecisionKind::Recovery => driver::QuestionKind::Recovery {
                group: self.group?,
                sent_in: None,
                cause: None,
            },
            DecisionKind::Other => return None,
        };
        Some(driver::Question {
            id: self.id.clone(),
            lineage: self.id.clone(),
            space_id: self.space_id.clone(),
            asked_at_ms: self.asked_at_ms,
            kind,
            recovery_cause: self.recovery_cause.clone(),
            items: (self.items.iter())
                .map(|item| driver::HeldItem {
                    node_id: item.node_id.clone(),
                    path: item.path.clone(),
                    files: item.files,
                    device_id: None,
                })
                .collect(),
        })
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct DevicesData {
    devices: Vec<DeviceData>,
}

#[derive(Debug, Deserialize)]
struct DeviceData {
    device_id: String,
    name: String,
    platform: String,
    created_at_ms: i64,
    revoked_at_ms: i64,
    current: bool,
}

impl DevicesData {
    pub(crate) fn active(self) -> Vec<Device> {
        (self.devices.into_iter())
            .filter(|device| device.revoked_at_ms == 0)
            .map(|device| Device {
                device_id: device.device_id,
                name: device.name,
                platform: device.platform,
                this_device: device.current,
                added_at_ms: device.created_at_ms,
            })
            .collect()
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct InvitationData {
    join_argv: Vec<String>,
    expires_at_ms: i64,
}

impl InvitationData {
    pub(crate) fn code(self) -> PairingCode {
        let words: Vec<String> = (self.join_argv.iter().skip(2))
            .map(|word| shell_word(word))
            .collect();
        PairingCode {
            code: words.join(" "),
            expires_at_ms: self.expires_at_ms,
        }
    }
}

fn shell_word(text: &str) -> String {
    let bare = !text.is_empty()
        && (text.chars()).all(|c| c.is_ascii_alphanumeric() || "@%+=:,./_-".contains(c));
    match bare {
        true => text.to_owned(),
        false => format!("'{}'", text.replace('\'', r"'\''")),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct SpacesData {
    pub(crate) spaces: Vec<SpaceData>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SpaceData {
    space_id: String,
    name: String,
}

impl SpaceData {
    pub(crate) fn space(self) -> Space {
        Space {
            space_id: self.space_id,
            name: self.name,
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct UsagesData {
    spaces: Vec<UsageData>,
    refused: Vec<RefusedSpace>,
}

#[derive(Debug, Deserialize)]
struct UsageData {
    space_id: String,
    live_files: u64,
    live_bytes: u64,
    trashed_files: u64,
    trash_bytes: u64,
    old_versions: u64,
    old_version_bytes: u64,
    unreferenced_bytes: u64,
}

#[derive(Debug, Deserialize)]
struct RefusedSpace {
    space_id: String,
    message: String,
}

impl UsagesData {
    pub(crate) fn of(self, space_id: &str) -> Result<SpaceUsage, SessionError> {
        if let Some(usage) = self
            .spaces
            .into_iter()
            .find(|usage| usage.space_id == space_id)
        {
            return Ok(SpaceUsage {
                live_files: usage.live_files,
                live_bytes: usage.live_bytes,
                trashed_files: usage.trashed_files,
                trash_bytes: usage.trash_bytes,
                old_versions: usage.old_versions,
                old_version_bytes: usage.old_version_bytes,
                unreferenced_bytes: usage.unreferenced_bytes,
            });
        }
        match self
            .refused
            .into_iter()
            .find(|refused| refused.space_id == space_id)
        {
            Some(refused) => Err(SessionError::Failed {
                reason: refused.message,
            }),
            None => Err(SessionError::SpaceNotFound),
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct TrashData {
    folder: String,
    entries: Vec<TrashEntryData>,
}

#[derive(Debug, Deserialize)]
struct TrashEntryData {
    node_id: String,
    path: String,
    kind: EntryKind,
    files: u64,
    bytes: u64,
    #[serde(default)]
    modified_at_ms: Option<i64>,
    #[serde(default)]
    deleted_at_ms: Option<i64>,
    #[serde(default)]
    deleted_by_device_id: Option<String>,
    #[serde(default)]
    deleted_by_device_name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum EntryKind {
    File,
    Folder,
    Symlink,
}

impl TrashData {
    pub(crate) fn trash(self) -> Trash {
        Trash {
            folder: self.folder,
            items: (self.entries.into_iter())
                .map(|entry| DeletedItem {
                    node_id: entry.node_id,
                    path: entry.path,
                    kind: match entry.kind {
                        EntryKind::Folder => NodeKind::Folder,
                        EntryKind::File | EntryKind::Symlink => NodeKind::File,
                    },
                    files: entry.files,
                    bytes: entry.bytes,
                    modified_at_ms: entry.modified_at_ms,
                    deleted_at_ms: entry.deleted_at_ms,
                    deleted_by_device_id: entry.deleted_by_device_id,
                    deleted_by_device_name: entry.deleted_by_device_name,
                })
                .collect(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct VersionsData {
    path: String,
    node_id: String,
    versions: Vec<VersionData>,
}

#[derive(Debug, Deserialize)]
struct VersionData {
    version_id: String,
    size: u64,
    device_id: String,
    #[serde(default)]
    device_name: Option<String>,
    created_at_ms: i64,
    current: bool,
}

impl VersionsData {
    pub(crate) fn versions(self) -> FileVersions {
        FileVersions {
            path: self.path,
            node_id: self.node_id,
            versions: (self.versions.into_iter())
                .map(|version| FileVersion {
                    version_id: version.version_id,
                    size: version.size,
                    saved_at_ms: version.created_at_ms,
                    device_id: version.device_id,
                    device_name: version.device_name,
                    current: version.current,
                })
                .collect(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct RestoreAskedData {
    request_id: String,
    folder: String,
    path: String,
}

impl RestoreAskedData {
    pub(crate) fn asked(self) -> RestoreAsked {
        RestoreAsked {
            request_id: self.request_id,
            folder: self.folder,
            path: self.path,
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct RequestsData {
    requests: Vec<RequestData>,
}

#[derive(Debug, Deserialize)]
struct RequestData {
    id: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    asked_at_ms: i64,
    request: RequestedData,
    state: RequestStateData,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RequestedData {
    FromTrash {
        node_id: String,
    },
    Version {
        node_id: String,
        version_id: String,
        #[serde(default)]
        saved_at_ms: Option<i64>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RequestStateData {
    Pending {
        #[serde(default)]
        waits_for: Option<String>,
    },
    Restored,
    Renamed {
        name: String,
    },
    Copied {
        node_id: String,
        name: String,
    },
    Rejected {
        reason: String,
    },
    DoesNotFit,
}

impl RequestsData {
    pub(crate) fn requests(self) -> Vec<RestoreRequest> {
        (self.requests.into_iter())
            .map(|request| RestoreRequest {
                id: request.id,
                path: request.path,
                asked_at_ms: request.asked_at_ms,
                target: match request.request {
                    RequestedData::FromTrash { node_id } => RestoreTarget::Deleted { node_id },
                    RequestedData::Version {
                        node_id,
                        version_id,
                        saved_at_ms,
                    } => RestoreTarget::Version {
                        node_id,
                        version_id,
                        saved_at_ms,
                    },
                },
                state: match request.state {
                    RequestStateData::Pending { waits_for } => RestoreState::Pending {
                        held_by_problem: waits_for.is_some(),
                    },
                    RequestStateData::Restored => RestoreState::Restored,
                    RequestStateData::Renamed { name } => RestoreState::RestoredAs { name },
                    RequestStateData::Copied { node_id, name } => {
                        RestoreState::RestoredAsCopy { node_id, name }
                    }
                    RequestStateData::Rejected { reason } => RestoreState::Refused {
                        reason: match reason.as_str() {
                            "not_found" => RestoreRefusal::Gone,
                            "not_file" => RestoreRefusal::NotAFile,
                            "invalid_name" => RestoreRefusal::InvalidName,
                            "parent_not_dir" => RestoreRefusal::DestinationNotAFolder,
                            "cycle" => RestoreRefusal::DestinationInsideItself,
                            "root" => RestoreRefusal::Root,
                            "read_only" => RestoreRefusal::ReadOnly,
                            _ => RestoreRefusal::Other { reason },
                        },
                    },
                    RequestStateData::DoesNotFit => RestoreState::TooLarge,
                },
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
