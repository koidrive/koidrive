// SPDX-License-Identifier: Apache-2.0

use std::fmt;

use serde::{Deserialize, Serialize};

use super::Invalid;

/// Which program sends the report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Client {
    CliDaemon,
    CliCommand,
    MacosApp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Build {
    Release,
    Dev,
}

/// Which line of releases a version belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Dev,
    /// An alpha, beta or release candidate: a version with a pre-release part, such as `0.1.0-alpha.3`.
    Prerelease,
    Release,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Os {
    Macos,
    Linux,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Arch {
    Arm64,
    X86_64,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Distro {
    Alpine,
    Arch,
    Debian,
    Fedora,
    Nixos,
    Opensuse,
    Rhel,
    Ubuntu,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OsVersion {
    pub major: u16,
    pub minor: u16,
}

/// 128 random bits as 32 lowercase hex characters. It is drawn once per profile and says nothing about the machine,
/// the account or the Koidrive device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct InstallId(String);

impl InstallId {
    pub fn generate() -> Self {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).expect("the operating system provides random bytes");
        InstallId(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for InstallId {
    type Error = Invalid;

    fn try_from(value: String) -> Result<Self, Invalid> {
        let hex = value.len() == 32
            && value
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
        hex.then_some(InstallId(value)).ok_or(Invalid("install id"))
    }
}

impl From<InstallId> for String {
    fn from(id: InstallId) -> String {
        id.0
    }
}

impl fmt::Display for InstallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An event's uuid, drawn once when its batch is first taken and sent again with every retry, so PostHog keeps one copy
/// of a batch it got twice. Random, in the UUID version 4 layout.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct EventUuid(String);

impl EventUuid {
    pub fn generate() -> Result<Self, getrandom::Error> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes)?;
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        Ok(EventUuid(format!(
            "{}-{}-{}-{}-{}",
            &hex[..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..]
        )))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for EventUuid {
    type Error = Invalid;

    fn try_from(value: String) -> Result<Self, Invalid> {
        let valid = value.len() == 36
            && value.bytes().enumerate().all(|(i, byte)| match i {
                8 | 13 | 18 | 23 => byte == b'-',
                _ => matches!(byte, b'0'..=b'9' | b'a'..=b'f'),
            });
        valid
            .then_some(EventUuid(value))
            .ok_or(Invalid("event uuid"))
    }
}

impl From<EventUuid> for String {
    fn from(uuid: EventUuid) -> String {
        uuid.0
    }
}

/// A version number such as `0.1.0` or `0.1.0-dev+abc123`: a digit first, then at most 63 characters from
/// `0-9A-Za-z.+-`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Version(String);

impl Version {
    /// Whether this is the fixed local development version, with optional build metadata.
    pub fn is_development(&self) -> bool {
        self.0.split('+').next() == Some("0.0.0-dev")
    }

    pub fn channel(&self) -> Channel {
        match self.0.split('+').next().unwrap_or_default() {
            _ if self.is_development() => Channel::Dev,
            version if version.contains('-') => Channel::Prerelease,
            _ => Channel::Release,
        }
    }

    pub fn parse(value: &str) -> Result<Self, Invalid> {
        let valid = value.len() <= 64
            && value.starts_with(|c: char| c.is_ascii_digit())
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'+' | b'-'));
        valid
            .then(|| Version(value.to_owned()))
            .ok_or(Invalid("version"))
    }
}

impl TryFrom<String> for Version {
    type Error = Invalid;

    fn try_from(value: String) -> Result<Self, Invalid> {
        Version::parse(&value)
    }
}

impl From<Version> for String {
    fn from(version: Version) -> String {
        version.0
    }
}

/// A file of Koidrive's own source, as the compiler names it in a panic's location: `core/...` or `cli/...`, relative
/// to the repository. Any other location, such as a dependency's file under the home directory, becomes `external`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SourceFile(pub(super) String);

impl SourceFile {
    pub const EXTERNAL: &'static str = "external";

    pub fn from_location(file: &str) -> Self {
        let ours = (file.starts_with("core/") || file.starts_with("cli/"))
            && file.ends_with(".rs")
            && !file.contains("..")
            && file.len() <= 128
            && file
                .bytes()
                .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' | b'.' | b'/'));
        SourceFile(if ours { file } else { Self::EXTERNAL }.to_owned())
    }
}

impl TryFrom<String> for SourceFile {
    type Error = Invalid;

    fn try_from(value: String) -> Result<Self, Invalid> {
        let file = SourceFile::from_location(&value);
        (file.0 == value)
            .then_some(file)
            .ok_or(Invalid("source file"))
    }
}

impl From<SourceFile> for String {
    fn from(file: SourceFile) -> String {
        file.0
    }
}

labels! {
    /// How a failed `koi` command ended, as the `code` of its JSON error.
    pub enum ErrorCode {
        Failed => "failed",
        Usage => "usage",
        DecisionRequired => "decision_required",
        AuthRequired => "auth_required",
        Halted => "halted",
        Unavailable => "unavailable",
        StaleDecision => "stale_decision",
        WrongCode => "wrong_code",
        Interrupted => "interrupted",
    }
}

named!(
    ErrorCode,
    "error code",
    ErrorCode::ALL.iter().copied(),
    |code: ErrorCode| code.name().to_owned()
);
