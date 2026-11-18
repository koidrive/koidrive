// SPDX-License-Identifier: Apache-2.0

use koidrive_cloudflare::{ApiError as CloudflareError, OauthError};
use koidrive_driver::update::CheckError;
use koidrive_driver::{ConfigError, CredentialError, DriverError, PathsError, SET_UP_AGAIN};
use koidrive_ipc::{Code, Next, Reason};
use koidrive_net::{ApiError, Resend, ServerError};
use koidrive_proto::v1::{Error as ProtoError, error::Code as ServerCode};

use crate::report::Done;

const LEAVE_LAST_DEVICE: [&str; 4] = ["koi", "device", "leave", "--last-device"];
const OWNED: &str = "this deployment already has an owner";
pub(crate) const RECOVERY_STEPS: &str =
    "https://github.com/koidrive/koidrive/blob/main/docs/deploy.md#if-you-lose-every-device";
const WRONG_CODE: [&str; 3] = [
    "wrong claim code",
    "no valid claim code; open /setup to issue one",
    "invalid or expired pairing code",
];

#[derive(Debug)]
pub struct CliError {
    pub code: Code,
    pub reason: Option<Reason>,
    pub message: String,
    pub human: Option<String>,
    pub presented: bool,
    pub next: Vec<Next>,
    /// What the command read before it failed, such as the spaces that answered when another one refused.
    pub partial: Option<Box<Done>>,
}

impl CliError {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        CliError {
            code,
            reason: None,
            message: message.into(),
            human: None,
            presented: false,
            next: Vec::new(),
            partial: None,
        }
    }

    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(Code::Usage, message)
    }

    pub fn human(mut self, message: impl Into<String>) -> Self {
        self.human = Some(message.into());
        self
    }

    pub fn caused(message: impl Into<String>, cause: impl Into<CliError>) -> Self {
        CliError {
            message: message.into(),
            ..cause.into()
        }
    }

    pub fn known_outcome(message: impl Into<String>, cause: impl Into<CliError>) -> Self {
        let message = message.into();
        CliError {
            human: Some(message.clone()),
            message,
            ..cause.into()
        }
    }

    pub fn context(mut self, context: impl std::fmt::Display) -> Self {
        self.message = format!("{context}: {}", self.message);
        self
    }

    pub fn because(self, reason: Reason) -> Self {
        CliError {
            reason: Some(reason),
            ..self
        }
    }

    pub fn then(mut self, argv: &[&str]) -> Self {
        self.next.push(Next::of(argv));
        self
    }
}

impl From<String> for CliError {
    fn from(message: String) -> Self {
        CliError::new(Code::Failed, message)
    }
}

impl From<&str> for CliError {
    fn from(message: &str) -> Self {
        CliError::new(Code::Failed, message)
    }
}

impl CliError {
    fn refused_credential() -> Self {
        CliError::new(
            Code::AuthRequired,
            format!(
                "the server refused this device's credential, so another device revoked it or the credential is \
                 wrong. To use this device again, {SET_UP_AGAIN}."
            ),
        )
        .human(format!(
            "The server no longer accepts this device's sign-in. To use it again, {SET_UP_AGAIN}."
        ))
    }

    pub fn code_refused(error: DriverError) -> Self {
        let refusal = match &error {
            DriverError::Api(api) => api
                .server()
                .filter(|server| server.code() == ServerCode::Forbidden)
                .map(|server| server.message()),
            _ => None,
        };
        match refusal {
            Some(message) if WRONG_CODE.contains(&message) => {
                CliError::new(Code::WrongCode, "the code is wrong or has expired")
                    .human("The code is wrong or has expired. Get a new code from the device or server you are joining.")
            }
            Some(OWNED) => CliError::retry_safe(error).because(Reason::Owned).human(format!(
                "This server already has an owner. If it is yours and no signed-in device is left, add this device with a recovery key: {RECOVERY_STEPS}"
            )),
            _ => CliError::retry_safe(error),
        }
    }

    pub fn rpc_refusal(error: ProtoError) -> Self {
        let error = ServerError(error);
        if error.code() == ServerCode::Unauthenticated {
            return CliError::refused_credential();
        }
        let code = match error.resend() {
            Resend::Allowed => Code::Unavailable,
            Resend::Never | Resend::ByCode => Code::Failed,
        };
        CliError::new(code, format!("the server refused: {}", error.message())).human(format!(
            "The server refused the request: {}",
            error.message()
        ))
    }
}

impl From<CheckError> for CliError {
    fn from(error: CheckError) -> Self {
        let reason = match error {
            CheckError::Unreachable => Some(Reason::ReleasesUnreachable),
            CheckError::Refused => Some(Reason::ReleasesRefused),
            CheckError::Url => None,
        };
        CliError {
            reason,
            ..CliError::new(Code::Unavailable, error.to_string())
        }
    }
}

impl From<ApiError> for CliError {
    fn from(error: ApiError) -> Self {
        Self::api(error, RetrySafety::Inspect)
    }
}

enum RetrySafety {
    Safe,
    Inspect,
}

impl CliError {
    pub fn api_retry_safe(error: ApiError) -> Self {
        Self::api(error, RetrySafety::Safe)
    }

    pub fn retry_safe(error: DriverError) -> Self {
        match error {
            DriverError::Api(error) => Self::api_retry_safe(error),
            other => Self::from(other),
        }
    }

    fn api(error: ApiError, retry: RetrySafety) -> Self {
        if error.is_unauthenticated() {
            return CliError::refused_credential();
        }
        let mut human = match &error {
            ApiError::Server { status: 500..=599, error } => {
                format!("The server reported a problem: {}", error.message())
            }
            ApiError::Server { error, .. } => {
                format!("The server refused the request: {}", error.message())
            }
            ApiError::Transport(_) => "Koidrive could not reach the server.".into(),
            ApiError::Status { status } | ApiError::Unreadable { status } => format!(
                "The server answered HTTP {status} without a readable cause."
            ),
            ApiError::TooLarge { .. } | ApiError::Decode(_) => "Koidrive could not read the server's answer.".into(),
            ApiError::Url(_) => "The server address cannot be used. Check the address you entered or the server used by this device.".into(),
            ApiError::PathSegment(_) => "The request contains an invalid ID. Nothing was sent. Use the ID shown by the command that lists the item.".into(),
            ApiError::Credential(_) => "Koidrive could not read this device's saved sign-in after the server refused it. Run koi diagnose to collect details before changing the device's setup.".into(),
        };
        let guidance = match (&error, retry) {
            (ApiError::Transport(_), RetrySafety::Safe) => {
                " Check your connection and the server address, then try again."
            }
            (ApiError::Transport(_), RetrySafety::Inspect) => {
                " Check your connection and the server address. Before repeating a change, inspect the affected item from a connected device to see whether it took effect."
            }
            (
                ApiError::Server { status: 429, .. }
                | ApiError::Status { status: 429 }
                | ApiError::Unreadable { status: 429 },
                _,
            ) => " Wait before trying this command again.",
            (
                ApiError::Server {
                    status: 500..=599, ..
                }
                | ApiError::Status { status: 500..=599 }
                | ApiError::Unreadable { status: 500..=599 },
                RetrySafety::Safe,
            ) => " Wait before trying this command again.",
            (
                ApiError::Server {
                    status: 500..=599, ..
                }
                | ApiError::Status { status: 500..=599 }
                | ApiError::Unreadable { status: 500..=599 }
                | ApiError::TooLarge { .. }
                | ApiError::Decode(_),
                RetrySafety::Inspect,
            ) => {
                " Before repeating a change, inspect the affected item from a connected device to see whether it took effect."
            }
            (
                ApiError::Status { .. }
                | ApiError::Unreadable { .. }
                | ApiError::TooLarge { .. }
                | ApiError::Decode(_),
                _,
            ) => {
                " Run this command with --verbose for details, or koi diagnose to collect a report."
            }
            _ => "",
        };
        if !guidance.is_empty() && !human.ends_with(['.', '!', '?']) {
            human.push('.');
        }
        human.push_str(guidance);
        let busy = match error
            .server()
            .map(|server| (server.code(), server.resend()))
        {
            Some((ServerCode::RetryLater | ServerCode::QuotaExceeded, resend)) => {
                resend == Resend::Allowed
            }
            _ => matches!(error.status(), Some(429 | 502 | 503 | 504)),
        };
        let code = match error {
            ApiError::Transport(_) | ApiError::TooLarge { .. } => Code::Unavailable,
            _ if busy => Code::Unavailable,
            _ => Code::Failed,
        };
        CliError::new(code, error.to_string()).human(human)
    }
}

impl From<CloudflareError> for CliError {
    fn from(error: CloudflareError) -> Self {
        let human = match &error {
            CloudflareError::Transport { call, .. } => format!(
                "Cloudflare did not answer while Koidrive tried to {call}. Check your connection. If this changed your server or storage, inspect it in the Cloudflare dashboard before repeating the command."
            ),
            CloudflareError::Failed { call, status, errors } => {
                let cause = errors.iter().map(|error| error.message.as_str()).collect::<Vec<_>>().join("; ");
                let action = match status {
                    401 | 403 => "Check the Cloudflare account and permissions used for this command.",
                    429 => "Cloudflare is limiting requests. Wait before making another request.",
                    _ => "If this changed your server or storage, inspect it in the Cloudflare dashboard before repeating the command.",
                };
                let cause = if cause.is_empty() { format!("HTTP {status}") } else { cause };
                format!("Cloudflare could not {call}. {cause}. {action}")
            }
            CloudflareError::Unreadable { call, .. } => format!(
                "Koidrive could not read Cloudflare's answer while trying to {call}. Inspect your server or storage in the Cloudflare dashboard before repeating a change."
            ),
            CloudflareError::Name { .. } => "The name cannot be used for this Cloudflare request. Choose a nonempty name that is neither a single dot nor two dots.".into(),
        };
        let code = match &error {
            CloudflareError::Transport { .. } => Code::Unavailable,
            CloudflareError::Failed { status, .. } => match status {
                401 | 403 => Code::AuthRequired,
                429 | 500..=599 => Code::Unavailable,
                _ => Code::Failed,
            },
            CloudflareError::Unreadable { .. } | CloudflareError::Name { .. } => Code::Failed,
        };
        CliError::new(code, error.to_string()).human(human)
    }
}

impl From<OauthError> for CliError {
    fn from(error: OauthError) -> Self {
        let human = match &error {
            OauthError::Listen { .. } | OauthError::Accept(_) => "Koidrive could not receive the browser's login result. Start the login again. If the browser callback still cannot connect, use --interactive --no-browser on the setup or server command.".into(),
            OauthError::NotCallback => "Paste the local page address the browser opened after you chose Authorize, not the Cloudflare login page.".into(),
            OauthError::StateMismatch => "This browser result belongs to another login attempt. Start the login again and use the page opened by that attempt.".into(),
            OauthError::MissingCode => "The browser result has no login code. Start the login again.".into(),
            OauthError::Denied { error, description } => format!(
                "Cloudflare did not authorize the login. {} Start the login again when you are ready to authorize Koidrive.", description.as_deref().unwrap_or(error)
            ),
            OauthError::TokenRefused { status, error, description } => format!(
                "Cloudflare refused the login exchange. {} (HTTP {status}). Start the login again.", description.as_deref().unwrap_or(error)
            ),
            OauthError::MissingScopes { .. } => "Cloudflare did not grant all required permissions. Start the login again and allow every permission on the consent page.".into(),
            OauthError::Spent => "This Cloudflare login can no longer be refreshed. Start the login again.".into(),
            OauthError::Transport(_) => "Koidrive could not reach Cloudflare to complete the login request. Check your connection. If you were signing out, inspect Koidrive under My Profile, Access Management, Connected Applications in the Cloudflare dashboard before repeating the command.".into(),
            OauthError::MalformedTokenResponse => "Koidrive could not read Cloudflare's login response. Start a new login rather than reusing the browser result.".into(),
            OauthError::RevokeRefused { .. } => "Cloudflare did not confirm that it revoked this login. Revoke Koidrive under My Profile, Access Management, Connected Applications in the Cloudflare dashboard.".into(),
        };
        let code = match &error {
            OauthError::NotCallback | OauthError::StateMismatch | OauthError::MissingCode => {
                Code::Usage
            }
            OauthError::Denied { .. }
            | OauthError::TokenRefused { .. }
            | OauthError::MissingScopes { .. }
            | OauthError::Spent => Code::AuthRequired,
            OauthError::Transport(_) => Code::Unavailable,
            OauthError::Listen { .. }
            | OauthError::Accept(_)
            | OauthError::MalformedTokenResponse
            | OauthError::RevokeRefused { .. } => Code::Failed,
        };
        CliError::new(code, error.to_string()).human(human)
    }
}

impl From<DriverError> for CliError {
    fn from(error: DriverError) -> Self {
        let human = driver_human(&error);
        let message = error.to_string();
        let mut result = match error {
            DriverError::Api(error) => CliError::from(error),
            DriverError::Refused(error) => CliError::rpc_refusal(error.0),
            DriverError::NotPaired => {
                CliError::new(Code::AuthRequired, message).then(&["koi", "setup"])
            }
            DriverError::NeedsConfirmation => CliError::new(Code::AuthRequired, message),
            DriverError::ShareTreeChanged => CliError::new(Code::Unavailable, message),
            DriverError::LastDevice => {
                CliError::new(Code::Failed, message).then(&LEAVE_LAST_DEVICE)
            }
            DriverError::NotRevoked(_) => {
                CliError::new(Code::Unavailable, message).then(&LEAVE_LAST_DEVICE)
            }
            DriverError::QuestionNotFound(_) | DriverError::AnsweredOtherwise { .. } => {
                CliError::new(
                    Code::StaleDecision,
                    format!("{message}; koi resolve lists the decisions waiting now"),
                )
                .then(&["koi", "resolve", "--json"])
            }
            DriverError::AnswerNotAllowed { .. } => {
                CliError::usage(message).then(&["koi", "resolve", "--json"])
            }
            DriverError::OutsideLinkedFolders(_) => {
                CliError::usage(message).because(Reason::NotInLinkedFolder)
            }
            DriverError::PathNotFound(_) => CliError::usage(message).because(Reason::NotFound),
            DriverError::NotOnServer(_) => CliError::usage(message).because(Reason::NotOnServer),
            DriverError::PasswordLength => CliError::usage(message).because(Reason::PasswordLength),
            DriverError::NotAShareLink | DriverError::ShareLinkNotFound => {
                CliError::usage(message).because(Reason::NoSuchLink)
            }
            DriverError::ShareLinkLimit => CliError::new(Code::Failed, message)
                .because(Reason::ShareLinkLimit)
                .then(&["koi", "share", "list"]),
            DriverError::SpaceUnplaced(space_id) => {
                CliError::usage(message).then(&["koi", "folder", "unlink", "--space", &space_id])
            }
            DriverError::UnlinkUnfinished(folder) => CliError::usage(message).then(&[
                "koi",
                "folder",
                "unlink",
                &folder.display().to_string(),
            ]),
            DriverError::EmptyName
            | DriverError::EmptyUserName
            | DriverError::SpaceNotFound(_)
            | DriverError::SpaceAmbiguous { .. }
            | DriverError::FolderNotFound(_)
            | DriverError::NotAFolder(_)
            | DriverError::FolderApprovalChanged(_)
            | DriverError::NotLinked(_)
            | DriverError::SpaceNotLinked(_)
            | DriverError::InvalidRestore(_)
            | DriverError::AlreadyPaired { .. }
            | DriverError::FolderHoldsProfile(_)
            | DriverError::FolderOverlaps(_)
            | DriverError::FolderOfOtherDevice { .. }
            | DriverError::LinkedByOtherDevice { .. }
            | DriverError::SpaceAlreadyLinked(_)
            | DriverError::MarkerOfOtherSpace(_)
            | DriverError::UnsupportedFileSystem
            | DriverError::NothingToLift
            | DriverError::RelativePath(_)
            | DriverError::ShareLinkAmbiguous(_)
            | DriverError::Paths(PathsError::BadSpaceId(_))
            | DriverError::Config(ConfigError::Invalid { .. }) => CliError::usage(message),
            _ => CliError::from(message),
        };
        if let Some(human) = human {
            result.human = Some(human);
        }
        result
    }
}

fn driver_human(error: &DriverError) -> Option<String> {
    let human = match error {
        DriverError::Api(_) | DriverError::Refused(_) => return None,
        DriverError::Busy(_) => {
            "Another Koidrive process is using this device's profile. Use the running app or service rather than starting a second one."
        }
        DriverError::NotPaired => "This device is not set up. Run koi setup to connect it.",
        DriverError::AlreadyPaired { .. } => {
            "This device is already set up. Run koi status to see its account and linked folders."
        }
        DriverError::EmptyName => "Enter a name for this device.",
        DriverError::EmptyUserName => "Enter a name for the account owner.",
        DriverError::LastDevice => {
            "This is your account's last active device. Signing out leaves only owner recovery to add a device again. Add another device first, or use koi device leave --last-device only if you accept that risk."
        }
        DriverError::NotRevoked(_) => {
            "The server did not confirm that this device was signed out. Check koi device list on another paired device before repeating the command. If this is your account's last active device, signing out leaves only owner recovery to add a device again. Use koi device leave --last-device only if you accept that risk."
        }
        DriverError::CredentialReplaced { .. } => {
            "This device's saved sign-in changed while the command was running, so Koidrive kept the new sign-in. Run koi status to inspect the current account before repeating a change."
        }
        DriverError::Paths(PathsError::NoHome) => {
            "Koidrive cannot find your home folder. Run koi from your normal user account with HOME set to an absolute path."
        }
        DriverError::Paths(PathsError::BadSpaceId(_)) => {
            "That space ID is not valid. Use an ID shown by koi space list."
        }
        DriverError::Credential(error) => match error {
            CredentialError::Io { source, .. } => {
                return Some(format!(
                    "Koidrive could not access this device's saved sign-in. {}",
                    local_io_human(source)?
                ));
            }
            CredentialError::Invalid { .. } => {
                "This device's saved sign-in is damaged or unreadable. Run koi diagnose to collect details before changing the device's setup."
            }
            CredentialError::Exposed { .. } => {
                "Other users can read this device's saved sign-in, so Koidrive will not use it. Run this command with --verbose to see the file path, restrict that file with chmod 600, then try again."
            }
            CredentialError::NotOwned { .. } => {
                "This device's saved sign-in belongs to another system user. Run Koidrive as the user who set it up, without copying their sign-in files."
            }
            CredentialError::Exists { .. } => {
                "This profile already has a saved sign-in. Run koi status to inspect it before setting up another account."
            }
            CredentialError::SignOutPending => {
                "A previous sign-out is waiting for server confirmation. Check koi device list on another paired device, then run koi device leave to finish signing out. If this is the last active device, only owner recovery can add a device again after sign-out."
            }
            CredentialError::NotExcludedFromBackups { .. } => {
                "Koidrive cannot keep this device's saved sign-in out of Time Machine backups, so it will not use it. Restore write access, keep the profile on storage that supports extended attributes, then restart Koidrive."
            }
        },
        DriverError::Config(ConfigError::Io { source, .. }) => {
            return Some(format!(
                "Koidrive could not read its configuration. {}",
                local_io_human(source)?
            ));
        }
        DriverError::Config(ConfigError::Invalid { .. }) => {
            "Koidrive's configuration is not valid. Run this command with --verbose to see which setting needs fixing, then check your changes to config.toml before starting sync again."
        }
        DriverError::SpaceNotFound(_) => {
            "No space matches that name. Run koi space list to see the available spaces."
        }
        DriverError::SpaceAmbiguous { .. } => {
            "Several spaces have that name. Run koi space list and use the ID of the space you want."
        }
        DriverError::FolderNotFound(path) | DriverError::PathNotFound(path) => {
            return Some(format!(
                "{} does not exist. Check its location and whether its drive is connected.",
                crate::output::path(path)
            ));
        }
        DriverError::NotAFolder(path) => {
            return Some(format!(
                "{} is not a folder. Choose a folder to sync.",
                crate::output::path(path)
            ));
        }
        DriverError::FolderApprovalChanged(_) => {
            "The folder changed after you approved it. Run koi setup again to review its files."
        }
        DriverError::FolderHoldsProfile(_) => {
            "That folder overlaps Koidrive's own local data. Choose a folder that neither contains nor sits inside Koidrive's profile."
        }
        DriverError::FolderOverlaps(_) => {
            "That folder overlaps a folder already being synced. Run koi status to see the linked folders and choose a separate folder."
        }
        DriverError::FolderInUse(_) => {
            "Another Koidrive process is syncing that folder. Stop syncing or unlink it in that process before using it here."
        }
        DriverError::FolderOfOtherDevice { .. } => {
            "That folder is linked by another device. Unlink it on that device before linking it here. Do not delete its Koidrive metadata to bypass this check."
        }
        DriverError::LinkedByOtherDevice { .. } => {
            "That folder was linked with a different sign-in on this device. Run koi folder unlink for that folder, then link it again with the current sign-in."
        }
        DriverError::SpaceAlreadyLinked(_) => {
            "That space is already linked on this device. Run koi status to find its folder."
        }
        DriverError::SpaceUnplaced(_) => {
            "That space is already linked on this device, but Koidrive can't tell where its folder is. Unlink it by its space ID, then link the folder again."
        }
        DriverError::MarkerOfOtherSpace(_) => {
            "That folder belongs to a different space. Unlink it from that space before linking it here."
        }
        DriverError::UnsupportedFileSystem => {
            "This drive cannot safely apply synced file changes. Choose a folder on a file system that supports renaming without replacing existing files."
        }
        DriverError::NotLinked(_) => {
            "That folder is not linked to Koidrive. Run koi status to see linked folders, or use koi folder link to link it."
        }
        DriverError::UnlinkUnfinished(_) => {
            "Unlinking that folder did not finish. Run koi status to inspect it, then repeat koi folder unlink for that folder to finish. Its files stay on this device."
        }
        DriverError::UnlinkBlocked { error, .. } => {
            return Some(format!(
                "This device was not signed out because a folder could not be unlinked. {} Check koi status before trying to sign out again.",
                local_io_human(error)?
            ));
        }
        DriverError::QuestionNotFound(_) => {
            "That decision is no longer waiting for an answer. Run koi resolve to see the decisions waiting now."
        }
        DriverError::AnsweredOtherwise { .. } => {
            "That decision already has a different answer. Run koi resolve to inspect the decisions waiting now before choosing another answer."
        }
        DriverError::AnswerNotAllowed { .. } => {
            "That answer is not available for this decision. Run koi resolve to see the choices it accepts."
        }
        DriverError::SpaceNotLinked(_) => {
            "That space has no linked folder on this device. Run koi status to see linked folders or koi space list to choose a space to link."
        }
        DriverError::InvalidRestore(_) => {
            "The restore contains an invalid ID. Use the IDs shown by koi history trash --json or koi history versions before requesting a restore."
        }
        DriverError::NothingToLift => {
            "That folder has no refused changes to retry. Run koi status to see its current sync state."
        }
        DriverError::RelativePath(_) => "Use the full path to the file or folder, starting with /.",
        DriverError::OutsideLinkedFolders(_) => {
            "That path is outside the folders linked on this device. Run koi status to see them."
        }
        DriverError::NotOnServer(_) => {
            "That file or folder has not synced to the server, or the folder's rules exclude it. Check koi status and the folder's sync rules before sharing or restoring it."
        }
        DriverError::PasswordLength => "Use a share-link password with 8 to 256 characters.",
        DriverError::NotAShareLink => "Enter a share-link ID or URL shown by koi share list.",
        DriverError::ShareLinkNotFound => {
            "No share link matches that ID or URL in this account. Run koi share list --all to inspect its links."
        }
        DriverError::ShareLinkLimit => {
            "This space has reached its active share-link limit. Run koi share list and revoke a link you no longer need before creating another."
        }
        DriverError::ShareLinkAmbiguous(_) => {
            "Several share links match that ID. Run koi share list and use the full URL of the link to revoke."
        }
        DriverError::ShareTreeChanged => {
            "The folder changed while Koidrive checked its files for sharing. Check koi share list before repeating the share command."
        }
        DriverError::NeedsConfirmation => {
            "The space's owner must confirm these folder changes, but Koidrive cannot request that confirmation yet. Leave the folder's files in place and ask the owner before changing its setup."
        }
        DriverError::Store { error, .. } => match error {
            koidrive_store::Error::UnknownMigration(_) => {
                "A newer Koidrive build updated this folder's sync state. Update Koidrive before opening it again. Keep the folder's files and saved sync data in place."
            }
            koidrive_store::Error::SchemaTooOld { .. } => {
                "This folder's saved sync state needs an update. Run koi service start to apply it, then repeat this command."
            }
            koidrive_store::Error::LogNotApplied(_) => {
                "Saved sync changes have not been applied to this folder's database. Run koi service start to apply them, then repeat this command."
            }
            koidrive_store::Error::DevelopmentSchema(_) => {
                "This folder's saved sync state comes from an unsupported development build. Keep its files in place and run koi diagnose before changing local sync data."
            }
            koidrive_store::Error::NotDurable(_) => {
                "The storage cannot safely save this folder's sync state. Keep its files in place and run koi diagnose before changing storage or local sync data."
            }
            koidrive_store::Error::Decode(_)
            | koidrive_store::Error::InvalidEngineState(_)
            | koidrive_store::Error::NotAStore => {
                "Koidrive cannot use its saved sync state because the saved data cannot be read safely. Keep the folder's files in place and run koi diagnose before changing local sync data."
            }
            koidrive_store::Error::Sqlite(_) => return None,
        },
        DriverError::Io { error, .. } => return local_io_human(error).map(str::to_owned),
    };
    Some(human.into())
}

pub(crate) fn local_io_human(error: &std::io::Error) -> Option<&'static str> {
    use std::io::ErrorKind;

    match error.kind() {
        ErrorKind::PermissionDenied => Some(if cfg!(target_os = "macos") {
            "Your account does not have permission to access the required local files. Check file permissions and Koidrive's access in System Settings > Privacy & Security."
        } else {
            "Your account does not have permission to access the required local files. Check file permissions and access for the user running Koidrive."
        }),
        ErrorKind::NotFound => Some(
            "A required local file or folder is missing. Check whether it was moved or its drive was disconnected before changing the setup.",
        ),
        ErrorKind::AlreadyExists => Some(
            "A file or folder already exists where Koidrive needs to write. Inspect it before moving or replacing anything.",
        ),
        ErrorKind::IsADirectory => Some(
            "A folder exists where Koidrive needs to read or write a file. Inspect it before moving or replacing anything.",
        ),
        ErrorKind::NotADirectory => Some(
            "A file exists where Koidrive needs a folder. Inspect it before moving or replacing anything.",
        ),
        ErrorKind::StorageFull => Some(
            "The drive is full. Free some space without deleting Koidrive's sync data, then check koi status before repeating a change.",
        ),
        ErrorKind::ReadOnlyFilesystem => Some(
            "The drive is read-only. Restore write access, then check koi status before repeating a change.",
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
