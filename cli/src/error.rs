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
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn refused(status: u16, code: ServerCode) -> ApiError {
        refused_with(status, code, "refused")
    }

    fn refused_with(status: u16, code: ServerCode, message: &str) -> ApiError {
        ApiError::Server {
            status,
            error: ServerError(koidrive_proto::v1::Error {
                code: code as i32,
                message: message.into(),
                ..Default::default()
            }),
        }
    }

    #[test]
    fn human_text_is_optional_and_context_keeps_it_separate() {
        let raw = "unknown cause: /private/state\n{debug: true}";
        let plain = CliError::from(raw);
        assert_eq!(plain.message.as_bytes(), raw.as_bytes());
        assert!(plain.human.is_none());
        let wrapped = CliError::caused(
            "the original contextual message",
            plain.human("The selected folder cannot be read."),
        )
        .context("raw context with internal-id")
        .because(Reason::NotInLinkedFolder)
        .then(&["koi", "status"]);
        assert_eq!(wrapped.code, Code::Failed);
        assert_eq!(wrapped.reason, Some(Reason::NotInLinkedFolder));
        assert_eq!(wrapped.next, [Next::of(&["koi", "status"])]);
        assert_eq!(
            wrapped.message,
            "raw context with internal-id: the original contextual message"
        );
        assert_eq!(
            wrapped.human.as_deref(),
            Some("The selected folder cannot be read.")
        );
        let raw = CliError::caused("outer", CliError::from("inner")).context("context");
        assert_eq!(raw.message, "context: outer");
        assert!(raw.human.is_none());
    }

    #[test]
    fn human_mapping_keeps_the_original_json_bytes() {
        let stale = CliError::from(DriverError::QuestionNotFound("q1".into()));
        assert!(stale.human.as_deref().unwrap().contains("koi resolve"));
        assert!(!stale.human.as_deref().unwrap().contains("q1"));
        let json =
            serde_json::to_vec(&crate::report::Envelope::failed("resolve", stale, None)).unwrap();
        assert_eq!(
            json,
            br#"{"schema_version":1,"command":"resolve","outcome":"error","data":null,"error":{"code":"stale_decision","message":"no unanswered question has the id \"q1\"; koi resolve lists the decisions waiting now","retryable":false},"decisions":[],"next_commands":[{"argv":["koi","resolve","--json"]}]}"#
        );
    }

    #[test]
    fn typed_server_causes_survive_without_protocol_tags() {
        let cause = "this deployment already has an owner";
        let api = CliError::from(refused_with(403, ServerCode::Forbidden, cause));
        assert_eq!(api.code, Code::Failed);
        assert_eq!(
            api.message,
            "the server answered HTTP 403: CODE_FORBIDDEN: this deployment already has an owner"
        );
        assert!(api.next.is_empty());
        assert_eq!(
            api.human.as_deref(),
            Some("The server refused the request: this deployment already has an owner")
        );
        let rpc = CliError::from(DriverError::Refused(ServerError(ProtoError {
            code: ServerCode::Forbidden as i32,
            message: cause.into(),
            ..Default::default()
        })));
        assert_eq!(rpc.code, Code::Failed);
        assert_eq!(rpc.message, format!("the server refused: {cause}"));
        assert_eq!(rpc.human, api.human);
        assert!(rpc.next.is_empty());
    }

    #[test]
    fn retry_safe_network_guidance_does_not_require_an_existing_sign_in() {
        for (error, action) in [
            (
                ApiError::Transport(reqwest::Client::new().get("not a url").build().unwrap_err()),
                "Check your connection and the server address, then try again",
            ),
            (
                ApiError::Status { status: 503 },
                "Wait before trying this command again",
            ),
            (
                ApiError::Unreadable { status: 429 },
                "Wait before trying this command again",
            ),
        ] {
            let raw = error.to_string();
            let shown = CliError::api_retry_safe(error);
            assert_eq!(shown.message, raw);
            let human = shown.human.unwrap();
            assert!(human.contains(action), "{human}");
            assert!(!human.contains("inspect"), "{human}");
            assert!(!human.contains("koi device list"), "{human}");
            assert!(!human.contains("koi space list"), "{human}");
        }
    }

    #[test]
    fn known_outer_outcomes_keep_their_own_recovery_guidance() {
        let message =
            "The saved sign-in could not be checked. Nothing was changed; run the command again";
        let cause = CliError::from(ApiError::Status { status: 503 });
        assert!(cause.human.as_ref().unwrap().contains("Before repeating"));
        let shown = CliError::known_outcome(message, cause);
        assert_eq!(shown.message, message);
        assert_eq!(shown.code, Code::Unavailable);
        assert_eq!(shown.human.as_deref(), Some(message));
    }

    #[test]
    fn typed_server_failures_keep_the_retry_safety_of_the_operation() {
        for status in [500, 503, 599, 429] {
            for safe in [false, true] {
                let error = refused(status, ServerCode::Internal);
                let raw = error.to_string();
                let shown = if safe {
                    CliError::api_retry_safe(error)
                } else {
                    CliError::from(error)
                };
                assert_eq!(shown.message, raw);
                assert_eq!(
                    shown.code,
                    if matches!(status, 429 | 503) {
                        Code::Unavailable
                    } else {
                        Code::Failed
                    }
                );
                assert!(shown.next.is_empty());
                let human = shown.human.unwrap();
                assert!(human.contains("refused"), "{human}");
                if status >= 500 {
                    assert!(
                        human.starts_with("The server reported a problem:"),
                        "{human}"
                    );
                }
                if safe || status == 429 {
                    assert!(human.contains("Wait before trying"), "{human}");
                    assert!(!human.contains("inspect"), "{human}");
                } else {
                    assert!(human.contains("Before repeating a change"), "{human}");
                    assert!(human.contains("inspect the affected item"), "{human}");
                }
            }
        }
    }

    #[test]
    fn unreadable_answers_require_inspection_before_repeating_changes() {
        let transport = || reqwest::Client::new().get("not a url").build().unwrap_err();
        for error in [
            ApiError::Transport(transport()),
            ApiError::Status { status: 503 },
            ApiError::Unreadable { status: 502 },
            ApiError::TooLarge { limit: 1 },
            ApiError::Decode("InternalParserType"),
        ] {
            let original = error.to_string();
            let shown = CliError::from(error);
            assert_eq!(shown.message.as_bytes(), original.as_bytes());
            assert!(shown.next.is_empty());
            let human = shown.human.unwrap();
            assert!(human.contains("Before repeating a change"), "{human}");
            assert!(human.contains("inspect the affected item"), "{human}");
            assert!(!human.contains("InternalParserType"), "{human}");
            assert!(!human.contains("Nothing was sent"), "{human}");
        }
        for error in [
            CloudflareError::Transport {
                call: "upload the Worker",
                source: transport(),
            },
            CloudflareError::Unreadable {
                call: "upload the Worker",
                detail: "parser/debug payload",
            },
        ] {
            let original = error.to_string();
            let shown = CliError::from(error);
            assert_eq!(shown.message.as_bytes(), original.as_bytes());
            assert!(shown.next.is_empty());
            let human = shown.human.unwrap();
            assert!(human.contains("Cloudflare dashboard"), "{human}");
            assert!(human.contains("before repeating"), "{human}");
            assert!(!human.contains("parser/debug"), "{human}");
        }
    }

    #[test]
    fn local_typed_errors_hide_internal_paths_ids_and_parser_payloads() {
        let path = || PathBuf::from("/private/koidrive/state/credential.json");
        let cases = [
            (
                DriverError::Busy(path()),
                "Another Koidrive process",
                Code::Failed,
            ),
            (
                DriverError::FolderHoldsProfile(path()),
                "overlaps Koidrive's own local data",
                Code::Usage,
            ),
            (
                DriverError::Config(ConfigError::Invalid {
                    path: path(),
                    reason: "parser/debug payload".into(),
                }),
                "configuration is not valid",
                Code::Usage,
            ),
            (
                DriverError::Config(ConfigError::Io {
                    path: path(),
                    source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
                }),
                "could not read its configuration",
                Code::Failed,
            ),
            (
                DriverError::Credential(CredentialError::Io {
                    path: path(),
                    source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
                }),
                "could not access this device's saved sign-in",
                Code::Failed,
            ),
            (
                DriverError::Credential(CredentialError::Invalid {
                    path: path(),
                    reason: "parser/debug payload".into(),
                }),
                "saved sign-in is damaged",
                Code::Failed,
            ),
            (
                DriverError::Credential(CredentialError::Exposed {
                    path: path(),
                    mode: 0o644,
                }),
                "Other users can read",
                Code::Failed,
            ),
            (
                DriverError::Credential(CredentialError::NotOwned { path: path() }),
                "another system user",
                Code::Failed,
            ),
            (
                DriverError::Credential(CredentialError::Exists { path: path() }),
                "already has a saved sign-in",
                Code::Failed,
            ),
            (
                DriverError::Credential(CredentialError::NotExcludedFromBackups {
                    path: path(),
                    source: std::io::Error::other("parser/debug payload"),
                }),
                "Time Machine backups",
                Code::Failed,
            ),
            (
                DriverError::AlreadyPaired {
                    device_id: "internal-id".into(),
                },
                "already set up",
                Code::Usage,
            ),
            (
                DriverError::CredentialReplaced {
                    device_id: "internal-id".into(),
                },
                "saved sign-in changed",
                Code::Failed,
            ),
            (
                DriverError::SpaceAmbiguous {
                    name: "Notes".into(),
                    ids: vec!["internal-id".into()],
                },
                "koi space list",
                Code::Usage,
            ),
            (
                DriverError::MarkerOfOtherSpace("internal-id".into()),
                "different space",
                Code::Usage,
            ),
            (
                DriverError::FolderOfOtherDevice {
                    device_id: "internal-id".into(),
                    server: "https://server.example".into(),
                },
                "another device",
                Code::Usage,
            ),
            (
                DriverError::LinkedByOtherDevice {
                    device_id: "internal-id".into(),
                    server: "https://server.example".into(),
                },
                "different sign-in",
                Code::Usage,
            ),
            (
                DriverError::SpaceNotLinked("internal-id".into()),
                "no linked folder",
                Code::Usage,
            ),
            (
                DriverError::Paths(PathsError::BadSpaceId("internal-id".into())),
                "space ID is not valid",
                Code::Usage,
            ),
            (
                DriverError::Paths(PathsError::NoHome),
                "home folder",
                Code::Failed,
            ),
            (
                DriverError::UnsupportedFileSystem,
                "cannot safely apply",
                Code::Usage,
            ),
            (
                DriverError::Store {
                    path: path(),
                    error: koidrive_store::Error::InvalidEngineState("parser/debug payload".into()),
                },
                "cannot use its saved sync state",
                Code::Failed,
            ),
        ];
        for (error, expected, code) in cases {
            let original = error.to_string();
            let shown = CliError::from(error);
            assert_eq!(shown.message.as_bytes(), original.as_bytes());
            assert_eq!(shown.code, code);
            assert!(shown.next.is_empty());
            let human = shown.human.unwrap();
            assert!(human.contains(expected), "{human}");
            for hidden in [
                "/private/",
                "internal-id",
                "parser/debug",
                "credential.json",
            ] {
                assert!(!human.contains(hidden), "{human}");
            }
        }
    }

    #[test]
    fn known_filesystem_causes_have_actions_and_unknown_causes_stay_raw() {
        for (kind, cause) in [
            (std::io::ErrorKind::PermissionDenied, "permission"),
            (std::io::ErrorKind::NotFound, "missing"),
            (std::io::ErrorKind::AlreadyExists, "already exists"),
            (std::io::ErrorKind::StorageFull, "drive is full"),
            (std::io::ErrorKind::ReadOnlyFilesystem, "read-only"),
        ] {
            let error = DriverError::Io {
                path: PathBuf::from("/private/state"),
                error: std::io::Error::new(kind, "parser/debug payload"),
            };
            let original = error.to_string();
            let shown = CliError::from(error);
            assert_eq!(shown.message.as_bytes(), original.as_bytes());
            assert_eq!(shown.code, Code::Failed);
            assert!(shown.next.is_empty());
            let human = shown.human.unwrap();
            assert!(human.contains(cause), "{human}");
            assert!(!human.contains("/private/"), "{human}");
            assert!(!human.contains("parser/debug"), "{human}");
        }
        let unknown = CliError::from(DriverError::Io {
            path: PathBuf::from("/x"),
            error: std::io::Error::other("a new filesystem failure"),
        });
        assert!(unknown.human.is_none());
        assert!(unknown.message.contains("a new filesystem failure"));
        assert_eq!(unknown.code, Code::Failed);
    }

    #[test]
    fn sign_out_presentations_keep_last_device_recovery_warnings() {
        for error in [
            DriverError::LastDevice,
            DriverError::NotRevoked("parser/debug payload".into()),
            DriverError::Credential(CredentialError::SignOutPending),
        ] {
            let original = error.to_string();
            let shown = CliError::from(error);
            assert_eq!(shown.message.as_bytes(), original.as_bytes());
            let human = shown.human.unwrap();
            assert!(human.contains("last active device"), "{human}");
            assert!(human.contains("owner recovery"), "{human}");
            assert!(!human.contains("parser/debug"), "{human}");
        }
    }

    #[test]
    fn pairing_and_unlinking_presentations_keep_original_actions() {
        let unpaired = CliError::from(DriverError::NotPaired);
        assert_eq!(unpaired.code, Code::AuthRequired);
        assert_eq!(
            unpaired.message,
            "this device is not set up; run koi setup first"
        );
        assert_eq!(unpaired.next, [Next::of(&["koi", "setup"])]);
        assert!(unpaired.human.unwrap().contains("koi setup"));
        let unlinked = CliError::from(DriverError::NotLinked(PathBuf::from("/Documents")));
        assert_eq!(unlinked.code, Code::Usage);
        assert!(unlinked.next.is_empty());
        assert!(unlinked.human.unwrap().contains("koi folder link"));
        let unfinished = DriverError::UnlinkUnfinished(PathBuf::from("/Documents"));
        let original = unfinished.to_string();
        let unfinished = CliError::from(unfinished);
        assert_eq!(unfinished.code, Code::Usage);
        assert_eq!(unfinished.message.as_bytes(), original.as_bytes());
        assert_eq!(
            unfinished.next,
            [Next::of(&["koi", "folder", "unlink", "/Documents"])]
        );
        assert!(unfinished.human.unwrap().contains("Its files stay"));
        let wrong = CliError::code_refused(DriverError::Api(refused_with(
            403,
            ServerCode::Forbidden,
            WRONG_CODE[0],
        )));
        assert_eq!(wrong.code, Code::WrongCode);
        assert_eq!(wrong.message, "the code is wrong or has expired");
        assert!(wrong.next.is_empty());
        assert!(wrong.human.unwrap().contains("Get a new code"));
        let revoked = CliError::from(refused(401, ServerCode::Unauthenticated));
        assert_eq!(revoked.code, Code::AuthRequired);
        assert!(revoked.human.unwrap().contains(SET_UP_AGAIN));
    }

    #[test]
    fn cloudflare_refusals_keep_the_cause_and_login_permissions_have_actions() {
        let error = CloudflareError::Failed {
            call: "create the bucket",
            status: 403,
            errors: vec![koidrive_cloudflare::CloudflareMessage {
                code: 10042,
                message: "Enable R2 before creating a bucket".into(),
            }],
        };
        let original = error.to_string();
        let shown = CliError::from(error);
        assert_eq!(shown.message.as_bytes(), original.as_bytes());
        assert_eq!(shown.code, Code::AuthRequired);
        assert!(shown.next.is_empty());
        let human = shown.human.unwrap();
        assert!(human.contains("Enable R2 before creating a bucket"));
        assert!(!human.contains("10042"));
        for (error, expected, code) in [
            (OauthError::NotCallback, "local page address", Code::Usage),
            (
                OauthError::StateMismatch,
                "another login attempt",
                Code::Usage,
            ),
            (OauthError::MissingCode, "no login code", Code::Usage),
            (
                OauthError::MissingScopes {
                    missing: "internal-scope".into(),
                },
                "allow every permission",
                Code::AuthRequired,
            ),
            (
                OauthError::Denied {
                    error: "access_denied".into(),
                    description: Some("You declined access".into()),
                },
                "You declined access",
                Code::AuthRequired,
            ),
            (
                OauthError::TokenRefused {
                    status: 400,
                    error: "invalid_grant".into(),
                    description: Some("The login code expired".into()),
                },
                "The login code expired",
                Code::AuthRequired,
            ),
            (
                OauthError::RevokeRefused {
                    token: "refresh_token",
                    status: 503,
                },
                "My Profile, Access Management, Connected Applications",
                Code::Failed,
            ),
        ] {
            let original = error.to_string();
            let shown = CliError::from(error);
            assert_eq!(shown.message.as_bytes(), original.as_bytes());
            assert_eq!(shown.code, code);
            assert!(shown.next.is_empty());
            let human = shown.human.unwrap();
            assert!(human.contains(expected), "{human}");
            assert!(!human.contains("internal-scope"), "{human}");
            assert!(!human.contains("refresh_token"), "{human}");
        }
    }

    #[test]
    fn a_refused_claim_or_pairing_code_is_the_codes_fault() {
        for message in WRONG_CODE {
            let wrong = CliError::code_refused(DriverError::Api(refused_with(
                403,
                ServerCode::Forbidden,
                message,
            )));
            assert_eq!(wrong.code, Code::WrongCode);
            assert_eq!(wrong.code.exit(), 8);
            assert_eq!(wrong.message, "the code is wrong or has expired");
            assert!(wrong.next.is_empty());
        }
        let owned = CliError::code_refused(DriverError::Api(refused_with(
            403,
            ServerCode::Forbidden,
            "this deployment already has an owner",
        )));
        assert_ne!(owned.code, Code::WrongCode);
        assert_eq!(owned.reason, Some(Reason::Owned));
        assert!(
            owned.message.contains("already has an owner"),
            "{}",
            owned.message
        );
        assert!(
            owned.human.as_deref().unwrap().contains(RECOVERY_STEPS),
            "{:?}",
            owned.human
        );
        let too_many = CliError::code_refused(DriverError::Api(refused_with(
            429,
            ServerCode::RetryLater,
            "too many wrong codes from this address; try again in a few minutes",
        )));
        assert_eq!(
            too_many.human.as_deref(),
            Some(
                "The server refused the request: too many wrong codes from this address; try again in a few minutes. Wait before trying this command again."
            )
        );
        let throttled =
            CliError::code_refused(DriverError::Api(refused(400, ServerCode::RetryLater)));
        assert_eq!(throttled.code, Code::Unavailable);
        let offline = CliError::code_refused(DriverError::Api(ApiError::Status { status: 503 }));
        assert_eq!(offline.code, Code::Unavailable);
        assert_eq!(
            CliError::code_refused(DriverError::NotPaired).code,
            CliError::from(DriverError::NotPaired).code
        );
    }

    #[test]
    fn server_refusals_are_classified_by_their_cause() {
        let classify = |error: ApiError| CliError::from(error).code;
        assert_eq!(
            classify(refused(401, ServerCode::Unauthenticated)),
            Code::AuthRequired
        );
        assert_eq!(
            classify(refused(429, ServerCode::Unspecified)),
            Code::Unavailable
        );
        assert_eq!(
            classify(ApiError::Status { status: 503 }),
            Code::Unavailable
        );
        assert_eq!(
            classify(refused(400, ServerCode::RetryLater)),
            Code::Unavailable
        );
        assert_eq!(
            classify(refused(403, ServerCode::QuotaExceeded)),
            Code::Failed
        );
        let mut waiting = refused(429, ServerCode::QuotaExceeded);
        if let ApiError::Server { error, .. } = &mut waiting {
            error.0.retry_after_ms = 1_000;
        }
        assert_eq!(classify(waiting), Code::Unavailable);
        assert_eq!(classify(ApiError::TooLarge { limit: 1 }), Code::Unavailable);
        assert_eq!(classify(ApiError::Status { status: 404 }), Code::Failed);
        assert_eq!(classify(ApiError::Decode("Status")), Code::Failed);
    }

    #[test]
    fn an_rpc_refusal_keeps_the_meaning_of_its_code() {
        let rpc = |code: ServerCode, retry_after_ms: u64| {
            CliError::rpc_refusal(ProtoError {
                code: code as i32,
                message: "refused".into(),
                retry_after_ms,
                ..Default::default()
            })
        };
        let revoked = rpc(ServerCode::Unauthenticated, 0);
        assert_eq!(revoked.code, Code::AuthRequired);
        assert!(revoked.message.contains(SET_UP_AGAIN), "{revoked:?}");
        assert_eq!(rpc(ServerCode::RetryLater, 0).code, Code::Unavailable);
        assert_eq!(
            rpc(ServerCode::QuotaExceeded, 1_000).code,
            Code::Unavailable
        );
        assert_eq!(rpc(ServerCode::QuotaExceeded, 0).code, Code::Failed);
        let other = rpc(ServerCode::NotFound, 0);
        assert_eq!(other.code, Code::Failed);
        assert_eq!(other.message, "the server refused: refused");
    }

    #[test]
    fn driver_errors_are_classified_and_name_the_real_next_command() {
        let stale = CliError::from(DriverError::QuestionNotFound("q1".into()));
        assert_eq!(stale.code, Code::StaleDecision);
        assert_eq!(stale.next, [Next::of(&["koi", "resolve", "--json"])]);
        let leave = [Next::of(&["koi", "device", "leave", "--last-device"])];
        let last = CliError::from(DriverError::LastDevice);
        assert_eq!((last.code, &last.next[..]), (Code::Failed, &leave[..]));
        let unconfirmed = CliError::from(DriverError::NotRevoked("no answer".into()));
        assert_eq!(
            (unconfirmed.code, &unconfirmed.next[..]),
            (Code::Unavailable, &leave[..])
        );
        let unpaired = CliError::from(DriverError::NotPaired);
        assert_eq!(unpaired.code, Code::AuthRequired);
        assert_eq!(unpaired.next, [Next::of(&["koi", "setup"])]);
        assert_eq!(
            CliError::from(DriverError::NotLinked(PathBuf::from("/x"))).code,
            Code::Usage
        );
        assert_eq!(
            CliError::from(DriverError::Api(ApiError::Status { status: 503 })).code,
            Code::Unavailable
        );
        let other = CliError::from(DriverError::Io {
            path: PathBuf::from("/x"),
            error: std::io::Error::other("the disk failed"),
        });
        assert_eq!(other.code, Code::Failed);
        assert!(other.next.is_empty());
    }

    #[test]
    fn argument_errors_are_usage_and_a_change_that_waits_for_the_owner_needs_auth() {
        let usage = [
            DriverError::FolderOverlaps(PathBuf::from("/home/ada/Docs")),
            DriverError::FolderHoldsProfile(PathBuf::from("/home/ada/.local/state/koidrive")),
            DriverError::FolderOfOtherDevice {
                device_id: "d2".into(),
                server: "https://files.example.com".into(),
            },
            DriverError::LinkedByOtherDevice {
                device_id: "d1".into(),
                server: "https://files.example.com".into(),
            },
            DriverError::SpaceAlreadyLinked(PathBuf::from("/home/ada/Docs")),
            DriverError::MarkerOfOtherSpace("s2".into()),
            DriverError::UnsupportedFileSystem,
            DriverError::AlreadyPaired {
                device_id: "d1".into(),
            },
            DriverError::NothingToLift,
            DriverError::Paths(PathsError::BadSpaceId("../x".into())),
            DriverError::Config(ConfigError::Invalid {
                path: PathBuf::from("/home/ada/.config/koidrive/config.toml"),
                reason: "unknown key".into(),
            }),
        ];
        for error in usage {
            let shown = error.to_string();
            assert_eq!(CliError::from(error).code, Code::Usage, "{shown}");
        }
        assert_eq!(
            CliError::from(DriverError::NeedsConfirmation).code,
            Code::AuthRequired
        );
    }

    #[test]
    fn cloudflare_and_login_errors_are_classified_by_their_cause() {
        let transport = || reqwest::Client::new().get("not a url").build().unwrap_err();
        let failed = |status| CloudflareError::Failed {
            call: "list the account's Workers",
            status,
            errors: Vec::new(),
        };
        let classify = |error: CloudflareError| CliError::from(error).code;
        assert_eq!(
            classify(CloudflareError::Transport {
                call: "list the account's Workers",
                source: transport(),
            }),
            Code::Unavailable
        );
        assert_eq!(classify(failed(503)), Code::Unavailable);
        assert_eq!(classify(failed(429)), Code::Unavailable);
        assert_eq!(classify(failed(403)), Code::AuthRequired);
        assert_eq!(classify(failed(400)), Code::Failed);

        let login = |error: OauthError| CliError::from(error).code;
        assert_eq!(login(OauthError::NotCallback), Code::Usage);
        assert_eq!(login(OauthError::StateMismatch), Code::Usage);
        assert_eq!(
            login(OauthError::Denied {
                error: "access_denied".into(),
                description: None,
            }),
            Code::AuthRequired
        );
        assert_eq!(
            login(OauthError::TokenRefused {
                status: 400,
                error: "invalid_grant".into(),
                description: None,
            }),
            Code::AuthRequired
        );
        assert_eq!(login(OauthError::Transport(transport())), Code::Unavailable);

        let wrapped = CliError::caused(
            "cannot list the account's Workers: no answer; run the command again",
            failed(502),
        );
        assert_eq!(wrapped.code, Code::Unavailable);
        assert!(wrapped.message.ends_with("run the command again"));
        let prefixed = CliError::from(failed(401)).context("cannot read the bucket fb");
        assert_eq!(prefixed.code, Code::AuthRequired);
        assert!(
            prefixed
                .message
                .starts_with("cannot read the bucket fb: could not")
        );
    }

    #[test]
    fn each_share_refusal_names_its_reason() {
        let path = || PathBuf::from("/home/ada/Documents/report.docx");
        for (error, reason) in [
            (
                DriverError::OutsideLinkedFolders(path()),
                Reason::NotInLinkedFolder,
            ),
            (DriverError::PathNotFound(path()), Reason::NotFound),
            (DriverError::NotOnServer(path()), Reason::NotOnServer),
            (DriverError::PasswordLength, Reason::PasswordLength),
            (DriverError::ShareLinkNotFound, Reason::NoSuchLink),
            (DriverError::NotAShareLink, Reason::NoSuchLink),
        ] {
            let message = error.to_string();
            let refused = CliError::from(error);
            assert_eq!(
                (refused.code, refused.reason, refused.message),
                (Code::Usage, Some(reason), message)
            );
        }
        let full = CliError::from(DriverError::ShareLinkLimit);
        assert!(full.human.as_deref().unwrap().contains("koi share list"));
        assert!(full.human.as_deref().unwrap().contains("revoke"));
        assert_eq!(
            (full.code, full.reason),
            (Code::Failed, Some(Reason::ShareLinkLimit))
        );
        let ambiguous = CliError::from(DriverError::ShareLinkAmbiguous("0123456789".into()));
        assert_eq!((ambiguous.code, ambiguous.reason), (Code::Usage, None));
    }

    #[test]
    fn permission_guidance_matches_the_platform() {
        let error = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let human = local_io_human(&error).unwrap();
        assert_eq!(human.contains("System Settings"), cfg!(target_os = "macos"));
        assert!(human.contains("file permissions"));
    }

    #[test]
    fn human_store_errors_keep_the_specific_recovery_action() {
        for (error, expected) in [
            (
                koidrive_store::Error::UnknownMigration("future-migration".into()),
                "Update Koidrive",
            ),
            (
                koidrive_store::Error::SchemaTooOld {
                    missing: "migration".into(),
                },
                "koi service start",
            ),
            (
                koidrive_store::Error::LogNotApplied("/private/state/wal".into()),
                "koi service start",
            ),
        ] {
            let error = CliError::from(DriverError::Store {
                path: "/private/state/store".into(),
                error,
            });
            assert!(
                error.human.as_deref().unwrap().contains(expected),
                "{error:?}"
            );
        }
    }
}
