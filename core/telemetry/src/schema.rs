// SPDX-License-Identifier: Apache-2.0

//! The JSON a report holds. Every string in it is an enum's name, a key built from enum names, a version number or
//! the install id. Nothing here can hold a path, a name, file contents or an error message.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

pub const SCHEMA: u16 = 2;

/// What one install recorded in one UTC hour. Its keys are always those of a [`Counter`], a [`Level`] or a [`Measure`]: only a
/// [`Recorder`](crate::Recorder) fills it, and reading one from JSON keeps only the keys of the closed set.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(from = "StoredReport")]
pub struct HourlyReport {
    /// Hours since 1970-01-01T00:00Z.
    pub hour: u32,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) counters: BTreeMap<String, BTreeMap<String, u64>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) levels: BTreeMap<String, BTreeMap<String, u64>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) histograms: BTreeMap<String, BTreeMap<String, Histogram>>,
}

#[derive(Deserialize)]
struct StoredReport {
    hour: u32,
    #[serde(default)]
    counters: BTreeMap<String, BTreeMap<String, u64>>,
    #[serde(default)]
    levels: BTreeMap<String, BTreeMap<String, u64>>,
    #[serde(default)]
    histograms: BTreeMap<String, BTreeMap<String, Histogram>>,
}

impl From<StoredReport> for HourlyReport {
    fn from(stored: StoredReport) -> Self {
        let mut report = HourlyReport {
            hour: stored.hour,
            ..HourlyReport::default()
        };
        for (metric, label) in Counter::all().map(Counter::key) {
            if let Some(&count) = stored
                .counters
                .get(metric)
                .and_then(|labels| labels.get(label))
            {
                (report.counters.entry(metric.to_owned()).or_default())
                    .insert(label.to_owned(), count);
            }
        }
        for (metric, label) in Level::all().map(Level::key) {
            if let Some(&value) = stored
                .levels
                .get(metric)
                .and_then(|labels| labels.get(label))
            {
                (report.levels.entry(metric.to_owned()).or_default())
                    .insert(label.to_owned(), value);
            }
        }
        for measure in Measure::all() {
            let (metric, label, shift) = measure.key();
            if let Some(histogram) = (stored.histograms.get(metric))
                .and_then(|labels| labels.get(label))
                .filter(|histogram| histogram.shift == shift)
            {
                let mut kept = measure.histogram();
                kept.merge(histogram);
                (report.histograms.entry(metric.to_owned()).or_default())
                    .insert(label.to_owned(), kept);
            }
        }
        report
    }
}

impl HourlyReport {
    pub fn merge(&mut self, other: HourlyReport) {
        for (metric, labels) in other.counters {
            let into = self.counters.entry(metric).or_default();
            for (label, count) in labels {
                let sum = into.entry(label).or_default();
                *sum = sum.saturating_add(count);
            }
        }
        for (metric, labels) in other.levels {
            let into = self.levels.entry(metric).or_default();
            for (label, value) in labels {
                let highest = into.entry(label).or_default();
                *highest = (*highest).max(value);
            }
        }
        for (metric, labels) in other.histograms {
            let into = self.histograms.entry(metric).or_default();
            for (label, histogram) in labels {
                into.entry(label)
                    .or_insert_with(|| Histogram::new(histogram.shift))
                    .merge(&histogram);
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.counters.is_empty() && self.levels.is_empty() && self.histograms.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Histogram {
    pub shift: u8,
    pub buckets: [u64; 16],
    pub count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sum: Option<u64>,
}

impl Histogram {
    pub fn new(shift: u8) -> Self {
        Histogram {
            shift,
            buckets: [0; 16],
            count: 0,
            sum: Some(0),
        }
    }

    pub fn record(&mut self, value: u64) {
        let scaled = value >> self.shift.min(63);
        let bucket = (64 - scaled.leading_zeros()).min(15) as usize;
        self.buckets[bucket] += 1;
        self.count += 1;
        self.sum = self.sum.map(|sum| sum.saturating_add(value));
    }

    pub fn merge(&mut self, other: &Histogram) {
        if other.shift != self.shift {
            return;
        }
        for (into, from) in self.buckets.iter_mut().zip(other.buckets) {
            *into = into.saturating_add(from);
        }
        self.count = self.count.saturating_add(other.count);
        self.sum = self
            .sum
            .zip(other.sum)
            .map(|(sum, other)| sum.saturating_add(other));
    }
}

/// A count as its bit length: 0 for 0, 1 for 1, 2 for 2 and 3, 3 for 4 to 7, and so on. Reports send this instead of
/// an exact number that could tell one setup from another.
pub fn magnitude(value: u64) -> u8 {
    (64 - value.leading_zeros()) as u8
}

/// Something that happened once, sent on its own rather than summed into the hour.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A step of `koi setup` or `koi server ...` ended.
    SetupStep {
        at: u64,
        flow: SetupFlow,
        step: SetupStep,
        outcome: Outcome,
        /// Cloudflare's numeric API error code, when a Cloudflare call failed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cloudflare_code: Option<u32>,
        /// The step's duration in seconds, as a [`magnitude`].
        seconds_magnitude: u8,
    },
    /// The process panicked here. The panic's message is never kept, since it can hold a path.
    Panic {
        at: u64,
        file: SourceFile,
        line: u32,
    },
}

impl Event {
    pub fn at(&self) -> u64 {
        match self {
            Event::SetupStep { at, .. } | Event::Panic { at, .. } => *at,
        }
    }

    pub fn hour(&self) -> u32 {
        u32::try_from(self.at() / 3600).unwrap_or(u32::MAX)
    }

    pub fn day(&self) -> u32 {
        u32::try_from(self.at() / 86_400).unwrap_or(u32::MAX)
    }

    /// The events of one kind an install keeps per day; later ones are dropped, so a loop cannot fill the queue.
    pub const PER_KIND_PER_DAY: usize = 20;

    pub fn kind(&self) -> &'static str {
        match self {
            Event::SetupStep { .. } => "setup_step",
            Event::Panic { .. } => "panic",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupFlow {
    /// `koi setup`, logging in with Cloudflare.
    Cloudflare,
    /// `koi setup`, entering a pairing code.
    Pairing,
    Join,
    Deploy,
    AddDevice,
    Claim,
    Upgrade,
    Uninstall,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupStep {
    Login,
    R2Checkout,
    Bucket,
    Script,
    Serving,
    Claim,
    Pair,
    Upgrade,
    Remove,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok,
    Failed,
    /// The person stopped it, for example with Ctrl-C or by answering no.
    Aborted,
}

/// An hourly sum: a metric and the label it is counted under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Counter {
    CpuMilliseconds,
    /// A CLI command ran. Only its name; never its arguments.
    Command(Command),
    /// What the daemon synced, over all its linked folders.
    Sync(Synced),
    /// Something failed, by its kind. Never its message.
    Failure(Failure),
}

impl Counter {
    pub fn all() -> impl Iterator<Item = Counter> {
        (Command::ALL.into_iter().map(Counter::Command))
            .chain(Synced::ALL.map(Counter::Sync))
            .chain(Failure::all().map(Counter::Failure))
            .chain([Counter::CpuMilliseconds])
    }

    pub(crate) fn key(self) -> (&'static str, &'static str) {
        match self {
            Counter::CpuMilliseconds => ("process", "cpu_ms"),
            Counter::Command(command) => ("command", command.name()),
            Counter::Sync(synced) => ("sync", synced.name()),
            Counter::Failure(failure) => failure.key(),
        }
    }
}

/// A kind of failure, counted each time it happens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Failure {
    /// A folder went from syncing to halted.
    Halted(Halted),
    /// An upload of a commit's chunks failed.
    Upload(Transfer),
    /// A download of a file's chunks failed.
    Download(Transfer),
    /// The server refused a call.
    Refused(Refusal),
    /// The server refused an op of a commit, and the folder shows its node as a problem.
    ProblemNode(Refusal),
    /// A commit was larger than a message may hold, so this device did not send it.
    TooLarge(TooLarge),
    /// A question for the person was asked, also when one is asked again.
    Asked(Asked),
    /// The daemon could not start.
    DaemonStart(DaemonStart),
}

impl Failure {
    pub fn all() -> impl Iterator<Item = Failure> {
        (Halted::ALL.iter().copied().map(Failure::Halted))
            .chain(Transfer::ALL.iter().copied().map(Failure::Upload))
            .chain(Transfer::ALL.iter().copied().map(Failure::Download))
            .chain(Refusal::ALL.iter().copied().map(Failure::Refused))
            .chain(Refusal::ALL.iter().copied().map(Failure::ProblemNode))
            .chain(TooLarge::ALL.iter().copied().map(Failure::TooLarge))
            .chain(Asked::ALL.iter().copied().map(Failure::Asked))
            .chain(DaemonStart::ALL.iter().copied().map(Failure::DaemonStart))
    }

    fn key(self) -> (&'static str, &'static str) {
        match self {
            Failure::Halted(why) => ("halted", why.name()),
            Failure::Upload(why) => ("upload_failed", why.name()),
            Failure::Download(why) => ("download_failed", why.name()),
            Failure::Refused(code) => ("refused", code.name()),
            Failure::ProblemNode(code) => ("problem_node", code.name()),
            Failure::TooLarge(what) => ("too_large", what.name()),
            Failure::Asked(kind) => ("asked", kind.name()),
            Failure::DaemonStart(why) => ("daemon_start_failed", why.name()),
        }
    }
}

macro_rules! labels {
    ($(#[$meta:meta])* pub enum $name:ident { $($(#[$doc:meta])* $variant:ident => $label:literal,)* }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
        pub enum $name {
            $($(#[$doc])* $variant,)*
        }

        impl $name {
            pub const ALL: &[$name] = &[$($name::$variant,)*];

            pub fn name(self) -> &'static str {
                match self {
                    $($name::$variant => $label,)*
                }
            }
        }
    };
}

labels! {
    /// Why a folder halted.
    pub enum Halted {
        /// The path no longer leads to the folder the task locked.
        Withdrawn => "withdrawn",
        /// The folder fails its root check: it is missing, unmounted or marked for another space.
        Root => "root",
        /// Another folder took the place of the one the task locked.
        Replaced => "replaced",
        /// An observation came from another folder than the one the task locked.
        OtherRoot => "other_root",
        /// The folder is inside another linked folder or a private directory.
        Placement => "placement",
        /// Another process syncs the folder, or another device linked it.
        Claim => "claim",
        /// The folder's file system cannot rename without replacing.
        NoReplace => "no_replace",
        /// The folder's sync state is older than the folder.
        StoreBehind => "store_behind",
        /// The folder was restored from an older state.
        DiskBehind => "disk_behind",
        /// An effect on the folder cannot be carried out.
        Record => "record",
        /// The folder's sync state cannot be read or written.
        Store => "store",
        /// Reading or writing the folder failed.
        Io => "io",
        /// The session cannot start, for example for a bad server URL or TLS.
        SessionStart => "session_start",
        /// The session ended on its own.
        SessionClosed => "session_closed",
        /// The server refused this device's credentials.
        CredentialsRefused => "credentials_refused",
        /// The server revoked this device.
        DeviceRevoked => "device_revoked",
        /// The server does not support this client's protocol versions.
        UnsupportedVersion => "unsupported_version",
        /// This device is not a member of the space.
        NotAMember => "not_a_member",
        /// The server does not know the space.
        SpaceNotFound => "space_not_found",
        /// The server refused the connection for good.
        ConnectionRefused => "connection_refused",
        /// The store and the server disagree in a way only recovery resolves.
        Recovery => "recovery",
        /// The server refused a call for good.
        Refused => "refused",
        /// The server sent something this build cannot apply.
        Protocol => "protocol",
        /// This device is signed out.
        SignedOut => "signed_out",
        /// The folder's task cannot start, for example because another device linked it.
        Start => "start",
        /// Unlinking the folder failed.
        Unlink => "unlink",
    }
}

labels! {
    /// How a transfer of chunks failed.
    pub enum Transfer {
        /// A call to the server failed.
        Call => "call",
        /// The file changed after it was chunked.
        SourceChanged => "source_changed",
        /// The bytes received never matched the chunk.
        Corrupt => "corrupt",
        /// The storage refused the transfer.
        Refused => "refused",
        /// The transfer kept failing, for example on the network.
        Unavailable => "unavailable",
        /// The server gave no usable target.
        NoTarget => "no_target",
        /// Reading or writing the chunk on this device failed.
        Io => "io",
    }
}

labels! {
    /// The code of a server refusal.
    pub enum Refusal {
        Unauthenticated => "unauthenticated",
        Forbidden => "forbidden",
        InvalidArgument => "invalid_argument",
        MissingChunks => "missing_chunks",
        LimitExceeded => "limit_exceeded",
        QuotaExceeded => "quota_exceeded",
        ReadOnly => "read_only",
        RetryLater => "retry_later",
        UnsupportedVersion => "unsupported_version",
        Internal => "internal",
        HistoryMismatch => "history_mismatch",
        StaleDeviceState => "stale_device_state",
        HeadMoved => "head_moved",
        AheadOfServer => "ahead_of_server",
        NotFound => "not_found",
        ConfirmationRequired => "confirmation_required",
        /// A code this build does not know, or none.
        Unknown => "unknown",
    }
}

labels! {
    /// What a commit too large to send cost.
    pub enum TooLarge {
        /// The commit, which is split and sent again.
        Commit => "commit",
        /// A node whose op alone is too large, which the folder shows as a problem.
        ProblemNode => "problem_node",
    }
}

labels! {
    /// The kind of a question for the person.
    pub enum Asked {
        /// Deletions on this device crossed a limit of the mass-delete guard.
        MassDelete => "mass_delete",
        /// Deletions from other devices crossed a limit of the mass-delete guard.
        RemoteMassDelete => "remote_mass_delete",
        /// Files disappeared while changes were not tracked.
        AmbiguousDisappearance => "ambiguous_disappearance",
        /// A recovery holds a group of files.
        Recovery => "recovery",
        /// A write of this device became a conflict copy or took a conflict name.
        Conflict => "conflict",
    }
}

labels! {
    /// Why the daemon could not start.
    pub enum DaemonStart {
        /// Another daemon or command uses the profile.
        Busy => "busy",
        /// The profile cannot be opened.
        Profile => "profile",
        /// The daemon cannot listen on its socket.
        Socket => "socket",
        /// The daemon cannot watch for signals.
        Signals => "signals",
    }
}

/// The sync totals of an hour, summed over every linked folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Synced {
    /// Commits of this device the server accepted.
    CommitsSent,
    /// Commit records of the server's log that reached this device, apart from results of its own commits.
    CommitsReceived,
    /// File content uploaded, in bytes: the chunks the server did not hold yet.
    BytesUploaded,
    /// File content downloaded, in bytes.
    BytesDownloaded,
}

impl Synced {
    pub const ALL: [Synced; 4] = [
        Synced::CommitsSent,
        Synced::CommitsReceived,
        Synced::BytesUploaded,
        Synced::BytesDownloaded,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Synced::CommitsSent => "commits_sent",
            Synced::CommitsReceived => "commits_received",
            Synced::BytesUploaded => "bytes_uploaded",
            Synced::BytesDownloaded => "bytes_downloaded",
        }
    }
}

/// A reading kept as the hour's highest: a metric and its label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// Folders linked to a space.
    Spaces,
    /// Nodes of the spaces' remote trees, files and folders, live and trashed.
    Nodes,
}

impl Level {
    pub const ALL: [Level; 2] = [Level::Spaces, Level::Nodes];

    pub fn all() -> impl Iterator<Item = Level> {
        Level::ALL.into_iter()
    }

    pub(crate) fn key(self) -> (&'static str, &'static str) {
        match self {
            Level::Spaces => ("sync", "spaces"),
            Level::Nodes => ("sync", "nodes"),
        }
    }
}

/// An hourly distribution of durations or sizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Measure {
    FirstSync(FileCount),
    DownloadFilesPerSecond,
    DownloadBytesPerSecond,
    UploadFilesPerSecond,
    UploadBytesPerSecond,
    RemoteApplyMilliseconds,
    SyncingMilliseconds,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileCount(u8);

impl FileCount {
    pub fn bucket(files: u64) -> Self {
        Self(magnitude(files).min(21))
    }

    fn name(self) -> &'static str {
        [
            "0",
            "1",
            "2_3",
            "4_7",
            "8_15",
            "16_31",
            "32_63",
            "64_127",
            "128_255",
            "256_511",
            "512_1023",
            "1024_2047",
            "2048_4095",
            "4096_8191",
            "8192_16383",
            "16384_32767",
            "32768_65535",
            "65536_131071",
            "131072_262143",
            "262144_524287",
            "524288_1048575",
            "1048576_plus",
        ][usize::from(self.0)]
    }
}

impl Measure {
    pub(crate) fn histogram(self) -> Histogram {
        let (_, _, shift) = self.key();
        Histogram {
            sum: match self {
                Measure::FirstSync(_)
                | Measure::RemoteApplyMilliseconds
                | Measure::SyncingMilliseconds => None,
                Measure::DownloadFilesPerSecond
                | Measure::DownloadBytesPerSecond
                | Measure::UploadFilesPerSecond
                | Measure::UploadBytesPerSecond => Some(0),
            },
            ..Histogram::new(shift)
        }
    }

    pub fn all() -> impl Iterator<Item = Measure> {
        (0..=21)
            .map(|bucket| Measure::FirstSync(FileCount(bucket)))
            .chain([
                Measure::DownloadFilesPerSecond,
                Measure::DownloadBytesPerSecond,
                Measure::UploadFilesPerSecond,
                Measure::UploadBytesPerSecond,
                Measure::RemoteApplyMilliseconds,
                Measure::SyncingMilliseconds,
            ])
    }

    pub(crate) fn key(self) -> (&'static str, &'static str, u8) {
        match self {
            Measure::FirstSync(files) => ("first_sync_ms", files.name(), 10),
            Measure::DownloadFilesPerSecond => ("download_per_second", "files", 0),
            Measure::DownloadBytesPerSecond => ("download_per_second", "bytes", 15),
            Measure::UploadFilesPerSecond => ("upload_per_second", "files", 0),
            Measure::UploadBytesPerSecond => ("upload_per_second", "bytes", 15),
            Measure::RemoteApplyMilliseconds => ("remote_apply_ms", "effect", 10),
            Measure::SyncingMilliseconds => ("syncing_ms", "completed", 10),
        }
    }
}

/// The CLI's top-level commands, by name. A group such as `koi folder` counts once, whichever action it runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Status,
    Resolve,
    Setup,
    Join,
    Server,
    Space,
    Folder,
    Device,
    History,
    Share,
    Service,
    Pause,
    Resume,
    Daemon,
    Telemetry,
    Diagnose,
    Update,
}

impl Command {
    pub const ALL: [Command; 17] = [
        Command::Status,
        Command::Resolve,
        Command::Setup,
        Command::Join,
        Command::Server,
        Command::Space,
        Command::Folder,
        Command::Device,
        Command::History,
        Command::Share,
        Command::Service,
        Command::Pause,
        Command::Resume,
        Command::Daemon,
        Command::Telemetry,
        Command::Diagnose,
        Command::Update,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Command::Status => "status",
            Command::Resolve => "resolve",
            Command::Setup => "setup",
            Command::Join => "join",
            Command::Server => "server",
            Command::Space => "space",
            Command::Folder => "folder",
            Command::Device => "device",
            Command::History => "history",
            Command::Share => "share",
            Command::Service => "service",
            Command::Pause => "pause",
            Command::Resume => "resume",
            Command::Daemon => "daemon",
            Command::Telemetry => "telemetry",
            Command::Diagnose => "diagnose",
            Command::Update => "update",
        }
    }
}

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
pub struct SourceFile(String);

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("not a valid {0}")]
pub struct Invalid(&'static str);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_histogram_puts_each_value_in_its_power_of_two_bucket() {
        let mut histogram = Histogram::new(5);
        for value in [0, 31, 32, 63, 64, 1 << 18, 1 << 19, u64::MAX] {
            histogram.record(value);
        }
        let mut expected = [0u64; 16];
        expected[0] = 2;
        expected[1] = 2;
        expected[2] = 1;
        expected[14] = 1;
        expected[15] = 2;
        assert_eq!(histogram.buckets, expected);
        assert_eq!(histogram.count, 8);
        assert_eq!(histogram.sum, Some(u64::MAX));
    }

    #[test]
    fn histograms_with_another_shift_do_not_merge() {
        let mut ms = Histogram::new(5);
        ms.record(100);
        let mut other = Histogram::new(10);
        other.record(100);
        ms.merge(&other);
        assert_eq!(ms.count, 1);
        let same = ms.clone();
        ms.merge(&same);
        assert_eq!(ms.count, 2);
    }

    #[test]
    fn a_stored_report_keeps_only_known_keys() {
        let stored = serde_json::json!({
            "hour": 100,
            "counters": {
                "command": { "status": 3, "/home/alice/private": 1 },
                "/home/alice/private": { "confidential.txt": 1 },
            },
            "histograms": {
                "/home/alice": { "notes.txt": { "shift": 0, "buckets": vec![0u64; 16], "count": 1, "sum": 1 } },
            },
        });
        let report: HourlyReport = serde_json::from_value(stored).unwrap();
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            serde_json::json!({ "hour": 100, "counters": { "command": { "status": 3 } } })
        );
    }

    #[test]
    fn a_stored_report_keeps_only_known_sync_counters_and_levels() {
        let stored = serde_json::json!({
            "hour": 100,
            "counters": { "sync": { "bytes_uploaded": 4096, "Alice Notes": 1 } },
            "levels": {
                "sync": { "spaces": 2, "nodes": 1500, "/home/alice": 1 },
                "Alice Notes": { "spaces": 1 },
            },
        });
        let report: HourlyReport = serde_json::from_value(stored).unwrap();
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            serde_json::json!({
                "hour": 100,
                "counters": { "sync": { "bytes_uploaded": 4096 } },
                "levels": { "sync": { "nodes": 1500, "spaces": 2 } },
            })
        );
    }

    #[test]
    fn a_stored_report_keeps_only_known_failure_counters() {
        let stored = serde_json::json!({
            "hour": 100,
            "counters": {
                "halted": { "store_behind": 1, "the server refused it: Alice Notes": 1 },
                "refused": { "quota_exceeded": 2, "418": 1 },
                "asked": { "conflict": 3 },
                "/home/alice": { "conflict": 1 },
            },
        });
        let report: HourlyReport = serde_json::from_value(stored).unwrap();
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            serde_json::json!({
                "hour": 100,
                "counters": {
                    "asked": { "conflict": 3 },
                    "halted": { "store_behind": 1 },
                    "refused": { "quota_exceeded": 2 },
                },
            })
        );
    }

    #[test]
    fn levels_merge_as_the_highest_and_counters_as_the_sum() {
        let report = |bytes: u64, nodes: u64| -> HourlyReport {
            serde_json::from_value(serde_json::json!({
                "hour": 100,
                "counters": { "sync": { "bytes_downloaded": bytes } },
                "levels": { "sync": { "nodes": nodes } },
            }))
            .unwrap()
        };
        let mut hour = report(100, 70);
        hour.merge(report(20, 30));
        assert_eq!(hour.counters["sync"]["bytes_downloaded"], 120);
        assert_eq!(hour.levels["sync"]["nodes"], 70);
    }

    #[test]
    fn every_counter_and_level_has_its_own_key() {
        let counters: std::collections::BTreeSet<_> = Counter::all().map(Counter::key).collect();
        assert_eq!(counters.len(), Counter::all().count());
        let levels: std::collections::BTreeSet<_> = Level::all().map(Level::key).collect();
        assert_eq!(levels.len(), Level::ALL.len());
    }

    #[test]
    fn timing_measures_have_closed_unique_keys_and_separate_large_file_counts() {
        let keys: std::collections::BTreeSet<_> = Measure::all().map(Measure::key).collect();
        assert_eq!(keys.len(), Measure::all().count());
        assert_eq!(FileCount::bucket(0).name(), "0");
        assert_eq!(FileCount::bucket(1).name(), "1");
        assert_eq!(FileCount::bucket(100_000).name(), "65536_131071");
        assert_eq!(FileCount::bucket(400_000).name(), "262144_524287");
        assert_eq!(FileCount::bucket(u64::MAX).name(), "1048576_plus");
    }

    #[test]
    fn duration_histograms_never_emit_sums_even_from_stored_reports() {
        let recorder = crate::Recorder::on();
        for measure in Measure::all() {
            recorder.measure_on(1, measure, 90_000);
            recorder.measure_on(1, measure, 600_000);
        }
        let report = recorder.take().hours.remove(&1).unwrap();
        let original = serde_json::to_value(&report).unwrap();
        let mut stored = original.clone();
        for measure in Measure::all() {
            let (metric, label, _) = measure.key();
            let histogram = &original["histograms"][metric][label];
            assert_eq!(histogram["count"], 2);
            if matches!(
                measure,
                Measure::FirstSync(_)
                    | Measure::RemoteApplyMilliseconds
                    | Measure::SyncingMilliseconds
            ) {
                assert!(histogram.get("sum").is_none());
                stored["histograms"][metric][label]["sum"] = serde_json::json!(1794527123456u64);
            } else {
                assert_eq!(histogram["sum"], 690_000);
            }
        }
        let restored: HourlyReport = serde_json::from_value(stored).unwrap();
        assert_eq!(serde_json::to_value(&restored).unwrap(), original);
        let mut merged = HourlyReport {
            hour: 1,
            ..HourlyReport::default()
        };
        merged.merge(report);
        merged.merge(restored);
        let merged = serde_json::to_value(merged).unwrap();
        for measure in Measure::all() {
            let (metric, label, _) = measure.key();
            let histogram = &merged["histograms"][metric][label];
            assert_eq!(histogram["count"], 4);
            for (index, count) in original["histograms"][metric][label]["buckets"]
                .as_array()
                .unwrap()
                .iter()
                .enumerate()
            {
                assert_eq!(histogram["buckets"][index], count.as_u64().unwrap() * 2);
            }
            if matches!(
                measure,
                Measure::FirstSync(_)
                    | Measure::RemoteApplyMilliseconds
                    | Measure::SyncingMilliseconds
            ) {
                assert!(histogram.get("sum").is_none());
            } else {
                assert_eq!(histogram["sum"], 1_380_000);
            }
        }
    }

    #[test]
    fn duration_histograms_distinguish_minutes_and_round_trip_with_shift_ten() {
        let recorder = crate::Recorder::on();
        for measure in Measure::all().filter(|measure| {
            matches!(
                measure,
                Measure::FirstSync(_)
                    | Measure::RemoteApplyMilliseconds
                    | Measure::SyncingMilliseconds
            )
        }) {
            let (metric, label, shift) = measure.key();
            assert_eq!(shift, 10);
            for millis in [1023, 60_000, 600_000, 5_280_000, 16_777_215, 16_777_216] {
                recorder.measure_on(1, measure, millis);
            }
            let mut report = recorder.take().hours.remove(&1).unwrap();
            let histogram = &report.histograms[metric][label];
            assert_eq!(histogram.buckets[0], 1);
            assert_eq!(histogram.buckets[6], 1);
            assert_eq!(histogram.buckets[10], 1);
            assert_eq!(histogram.buckets[13], 1);
            assert_eq!(histogram.buckets[14], 1);
            assert_eq!(histogram.buckets[15], 1);
            assert_eq!(histogram.count, 6);
            let stored = serde_json::to_value(&report).unwrap();
            assert_eq!(
                serde_json::from_value::<HourlyReport>(stored).unwrap(),
                report
            );
            report
                .histograms
                .get_mut(metric)
                .unwrap()
                .get_mut(label)
                .unwrap()
                .shift = 0;
            let rejected =
                serde_json::from_value::<HourlyReport>(serde_json::to_value(report).unwrap())
                    .unwrap();
            assert!(rejected.is_empty());
        }
    }

    #[test]
    fn byte_rate_histograms_distinguish_one_ten_and_one_hundred_mebibytes_per_second() {
        for measure in [
            Measure::UploadBytesPerSecond,
            Measure::DownloadBytesPerSecond,
        ] {
            let recorder = crate::Recorder::on();
            for rate in [1 << 20, 10 << 20, 100 << 20] {
                recorder.measure_on(1, measure, rate);
            }
            let report = recorder.take().hours.remove(&1).unwrap();
            let (metric, label, _) = measure.key();
            let histogram = &report.histograms[metric][label];
            for bucket in [6, 9, 12] {
                assert_eq!(histogram.buckets[bucket], 1);
            }
            assert_eq!(histogram.buckets[15], 0);
        }
    }

    #[test]
    fn stored_unfinished_span_counters_and_active_ages_are_dropped() {
        for shift in [0, 10] {
            let report: HourlyReport = serde_json::from_value(serde_json::json!({
                "hour": 1,
                "counters": { "sync": { "unfinished": 2 } },
                "histograms": { "syncing_ms": { "active_age": {
                    "shift": shift,
                    "buckets": vec![0; 16],
                    "count": 1,
                    "sum": 1794527123456u64,
                } } },
            }))
            .unwrap();
            assert_eq!(
                serde_json::to_value(report).unwrap(),
                serde_json::json!({
                    "hour": 1,
                })
            );
        }
    }

    #[test]
    fn the_failure_keys_are_exactly_these() {
        let mut keys: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for failure in Failure::all() {
            let (group, name) = failure.key();
            keys.entry(group).or_default().push(name);
        }
        let transfer = vec![
            "call",
            "source_changed",
            "corrupt",
            "refused",
            "unavailable",
            "no_target",
            "io",
        ];
        let refusal = vec![
            "unauthenticated",
            "forbidden",
            "invalid_argument",
            "missing_chunks",
            "limit_exceeded",
            "quota_exceeded",
            "read_only",
            "retry_later",
            "unsupported_version",
            "internal",
            "history_mismatch",
            "stale_device_state",
            "head_moved",
            "ahead_of_server",
            "not_found",
            "confirmation_required",
            "unknown",
        ];
        let halted = vec![
            "withdrawn",
            "root",
            "replaced",
            "other_root",
            "placement",
            "claim",
            "no_replace",
            "store_behind",
            "disk_behind",
            "record",
            "store",
            "io",
            "session_start",
            "session_closed",
            "credentials_refused",
            "device_revoked",
            "unsupported_version",
            "not_a_member",
            "space_not_found",
            "connection_refused",
            "recovery",
            "refused",
            "protocol",
            "signed_out",
            "start",
            "unlink",
        ];
        assert_eq!(
            keys,
            BTreeMap::from([
                ("halted", halted),
                ("upload_failed", transfer.clone()),
                ("download_failed", transfer),
                ("refused", refusal.clone()),
                ("problem_node", refusal),
                ("too_large", vec!["commit", "problem_node"]),
                (
                    "asked",
                    vec![
                        "mass_delete",
                        "remote_mass_delete",
                        "ambiguous_disappearance",
                        "recovery",
                        "conflict",
                    ]
                ),
                (
                    "daemon_start_failed",
                    vec!["busy", "profile", "socket", "signals"]
                ),
            ])
        );
    }

    #[test]
    fn every_command_is_listed_once() {
        let names: std::collections::BTreeSet<_> = Command::ALL.map(Command::name).into();
        assert_eq!(names.len(), Command::ALL.len());
    }

    #[test]
    fn magnitudes_are_bit_lengths() {
        assert_eq!(
            [0, 1, 2, 3, 4, 7, 8, 1000].map(magnitude),
            [0, 1, 2, 2, 3, 3, 4, 10]
        );
    }

    #[test]
    fn install_ids_are_32_random_hex_characters() {
        let (a, b) = (InstallId::generate(), InstallId::generate());
        assert_ne!(a, b);
        assert!(InstallId::try_from(a.as_str().to_owned()).is_ok());
        for bad in [
            "",
            "ABCDEF0123456789abcdef0123456789",
            "/home/alice/some/thing/0123456789",
        ] {
            assert!(InstallId::try_from(bad.to_owned()).is_err(), "{bad}");
        }
    }

    #[test]
    fn event_uuids_are_random_version_4_uuids() {
        let (a, b) = (
            EventUuid::generate().unwrap(),
            EventUuid::generate().unwrap(),
        );
        assert_ne!(a, b);
        assert_eq!(&a.as_str()[14..15], "4");
        assert!(EventUuid::try_from(a.as_str().to_owned()).is_ok());
        assert!(EventUuid::try_from("/home/alice/notes.txt/0123456789abcdef".to_owned()).is_err());
    }

    #[test]
    fn versions_refuse_anything_but_a_version_number() {
        for good in ["0.1.0", "0.1.0-dev+abc123", "14"] {
            assert!(Version::parse(good).is_ok(), "{good}");
        }
        for bad in ["", "v0.1", "0.1 beta", "0.1/../x", "1.0\n"] {
            assert!(Version::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_panic_location_outside_the_source_tree_is_external() {
        assert_eq!(
            SourceFile::from_location("core/driver/src/folder.rs").0,
            "core/driver/src/folder.rs"
        );
        for outside in [
            "/Users/alice/.cargo/registry/src/index.crates.io-1/tokio-1.48.0/src/lib.rs",
            "core/../../Users/alice/x.rs",
            "core/Alice Notes.rs",
            "src/main.rs",
        ] {
            assert_eq!(
                SourceFile::from_location(outside).0,
                SourceFile::EXTERNAL,
                "{outside}"
            );
        }
        assert!(SourceFile::try_from("/Users/alice/x.rs".to_owned()).is_err());
    }
}
