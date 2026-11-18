// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

use super::{Command, Histogram, Invalid, magnitude};

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
    /// The server said what failed inside it, or Cloudflare's edge answered for it.
    Server(ServerCause),
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
            .chain(ServerCause::ALL.iter().copied().map(Failure::Server))
            .chain(Refusal::ALL.iter().copied().map(Failure::ProblemNode))
            .chain(TooLarge::ALL.iter().copied().map(Failure::TooLarge))
            .chain(Asked::ALL.iter().copied().map(Failure::Asked))
            .chain(DaemonStart::ALL.iter().copied().map(Failure::DaemonStart))
    }

    pub fn key(self) -> (&'static str, &'static str) {
        match self {
            Failure::Halted(why) => ("halted", why.name()),
            Failure::Upload(why) => ("upload_failed", why.name()),
            Failure::Download(why) => ("download_failed", why.name()),
            Failure::Refused(code) => ("refused", code.name()),
            Failure::Server(cause) => ("server_cause", cause.name()),
            Failure::ProblemNode(code) => ("problem_node", code.name()),
            Failure::TooLarge(what) => ("too_large", what.name()),
            Failure::Asked(kind) => ("asked", kind.name()),
            Failure::DaemonStart(why) => ("daemon_start_failed", why.name()),
        }
    }
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
    /// What failed inside the server (RFC 0028), or the error page Cloudflare's edge sent when the Worker gave no answer.
    pub enum ServerCause {
        Exception => "exception",
        R2RateLimited => "r2_rate_limited",
        R2Unavailable => "r2_unavailable",
        R2Error => "r2_error",
        DoOverloaded => "do_overloaded",
        DoUnavailable => "do_unavailable",
        CpuExceeded => "cpu_exceeded",
        MemoryExceeded => "memory_exceeded",
        SubrequestLimit => "subrequest_limit",
        StorageFull => "storage_full",
        DailyLimit => "daily_limit",
        /// A cause this build does not know.
        Unknown => "unknown",
        /// Edge error 1101: the Worker threw an exception it did not answer.
        EdgeException => "edge_exception",
        /// Edge error 1102: the Worker exceeded its CPU time or memory limit.
        EdgeResourceLimit => "edge_resource_limit",
        /// Edge error 1027: the account reached the Workers Free daily request limit.
        EdgeDailyRequests => "edge_daily_requests",
        /// Another edge error code.
        EdgeOther => "edge_other",
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
    /// Folders halted.
    Halted,
    /// Folders halted for this reason.
    HaltedBy(Halted),
    /// Minutes since the folder halted longest ago halted.
    OldestHaltMinutes,
    /// Folders whose last moment up to date is this long ago. An up-to-date folder is under an hour.
    LastSynced(Age),
    /// Nodes that differ between the folders and the server, as their tasks last saw them.
    PendingNodes,
    /// 1 while sync is paused.
    Paused,
}

impl Level {
    pub fn all() -> impl Iterator<Item = Level> {
        [
            Level::Spaces,
            Level::Nodes,
            Level::Halted,
            Level::OldestHaltMinutes,
            Level::PendingNodes,
            Level::Paused,
        ]
        .into_iter()
        .chain(Halted::ALL.iter().copied().map(Level::HaltedBy))
        .chain(Age::ALL.iter().copied().map(Level::LastSynced))
    }

    pub(crate) fn key(self) -> (&'static str, &'static str) {
        match self {
            Level::Spaces => ("sync", "spaces"),
            Level::Nodes => ("sync", "nodes"),
            Level::Halted => ("sync", "halted"),
            Level::HaltedBy(why) => ("halted", why.name()),
            Level::OldestHaltMinutes => ("sync", "oldest_halt_minutes"),
            Level::LastSynced(age) => ("last_synced", age.name()),
            Level::PendingNodes => ("sync", "pending_nodes"),
            Level::Paused => ("sync", "paused"),
        }
    }
}

labels! {
    /// A range of elapsed time, from its lower bound up to its upper one.
    pub enum Age {
        UnderOneHour => "under_1h",
        OneToSixHours => "1h_6h",
        SixHoursToOneDay => "6h_1d",
        OneToThreeDays => "1d_3d",
        ThreeToSevenDays => "3d_7d",
        OverSevenDays => "over_7d",
        /// Nothing records when it began.
        Unknown => "unknown",
    }
}

impl Age {
    pub fn of(seconds: u64) -> Age {
        const HOUR: u64 = 3_600;
        match seconds / HOUR {
            0 => Age::UnderOneHour,
            1..6 => Age::OneToSixHours,
            6..24 => Age::SixHoursToOneDay,
            24..72 => Age::OneToThreeDays,
            72..168 => Age::ThreeToSevenDays,
            _ => Age::OverSevenDays,
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

    pub(super) fn name(self) -> &'static str {
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

named!(
    Halted,
    "halt reason",
    Halted::ALL.iter().copied(),
    |why: Halted| why.name().to_owned()
);
named!(Age, "age", Age::ALL.iter().copied(), |age: Age| age
    .name()
    .to_owned());
named!(Failure, "failure", Failure::all(), |failure: Failure| {
    let (group, kind) = failure.key();
    format!("{group}.{kind}")
});
