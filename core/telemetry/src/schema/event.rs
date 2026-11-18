// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

use super::{
    Age, BinaryId, Client, CrashKind, CrashSignal, ErrorCode, Failure, Invalid, MachException,
    SourceFile, Symbol, Version,
};

/// The release and process that recorded an event, which it is reported under whoever sends it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    pub koi_version: Version,
    pub client: Client,
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
        /// Koidrive's own functions on the panicking thread's stack, innermost first.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        frames: Vec<Symbol>,
    },
    /// A `koi` command ended with an error.
    CommandFailed {
        at: u64,
        command: Command,
        code: ErrorCode,
    },
    /// The daemon saw a failure, `count` times since its last tick.
    Problem {
        at: u64,
        problem: Failure,
        count: u64,
    },
    /// A halted folder syncs again, after being halted for `problem` for `lasted`.
    ProblemCleared {
        at: u64,
        problem: Failure,
        lasted: Age,
    },
    /// A linked folder's disk was mounted under another device number, and the daemon showed it is the same folder.
    RootAdopted {
        at: u64,
        proof: RootProof,
        /// For [`RootProof::Children`], how many entries directly in the folder matched, as a [`magnitude`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        children_magnitude: Option<u8>,
    },
    /// The Mac app crashed. KSCrash recorded it, and only these fields are kept from its report.
    AppCrash {
        at: u64,
        kind: CrashKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exception: Option<MachException>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signal: Option<CrashSignal>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binary: Option<BinaryId>,
        /// Offsets into the app's executable of the crashed thread's frames in it, innermost first.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        offsets: Vec<u64>,
    },
}

/// How the daemon showed that a folder found under another device number is the linked folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootProof {
    /// The volume's UUID and the folder's own identity match.
    Volume,
    /// The link predates recording the volume's UUID, and the entries directly in the folder kept their identities.
    Children,
}

impl RootProof {
    pub fn name(self) -> &'static str {
        match self {
            RootProof::Volume => "volume",
            RootProof::Children => "children",
        }
    }
}

impl Event {
    pub fn at(&self) -> u64 {
        match self {
            Event::SetupStep { at, .. }
            | Event::Panic { at, .. }
            | Event::CommandFailed { at, .. }
            | Event::Problem { at, .. }
            | Event::ProblemCleared { at, .. }
            | Event::RootAdopted { at, .. }
            | Event::AppCrash { at, .. } => *at,
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

    /// The event's kind, which the per-day cap counts: its name, and for a failure or a problem what failed.
    pub fn kind(&self) -> (&'static str, &'static str, &'static str) {
        match self {
            Event::SetupStep { .. } => ("setup_step", "", ""),
            Event::Panic { .. } => ("panic", "", ""),
            Event::AppCrash { .. } => ("app_crash", "", ""),
            Event::RootAdopted { proof, .. } => ("root_adopted", proof.name(), ""),
            Event::CommandFailed { code, .. } => ("command_failed", code.name(), ""),
            Event::Problem { problem, .. } => {
                let (group, name) = problem.key();
                ("problem", group, name)
            }
            Event::ProblemCleared { problem, .. } => {
                let (group, name) = problem.key();
                ("problem_cleared", group, name)
            }
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

named!(
    Command,
    "command",
    Command::ALL.into_iter(),
    |command: Command| command.name().to_owned()
);
