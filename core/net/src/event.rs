// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::time::Duration;

use koidrive_proto::v1::{CaughtUp, CommitRecord, CommitResult, Notice, ResyncRequired, Welcome};

use crate::error::StopReason;
use crate::transport::{Cause, Transport};

/// What a session reports, in the order the driver must handle it.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// A connection is up: the server's `Welcome`. It follows every reconnect (RFC 0002 sections 2 and 6).
    Connected(Welcome),
    /// Commits from the log, contiguous and in seq order, continuing from the subscribed seq.
    Changes(Vec<CommitRecord>),
    /// This device's commit was accepted. It is placed at its seq among `Changes`, so applying events in order
    /// applies the result in seq order (RFC 0002 section 5). It also comes when the commit's record already
    /// arrived by replay, because the result still has to be acknowledged.
    Committed(Committed),
    /// The backlog after a `Resume` is delivered; live pushes follow.
    CaughtUp(CaughtUp),
    /// The server cannot continue from the subscribed point. The stream stops until the driver subscribes again.
    ResyncRequired(ResyncRequired),
    Notice(Notice),
    /// The connection dropped. The session reconnects after `retry_in` and resends what RFC 0002 allows.
    Disconnected {
        reason: DisconnectReason,
        retry_in: Duration,
    },
    /// The session stopped for good; no further events follow.
    Stopped(StopReason),
}

/// An accepted commit of this device.
#[derive(Clone, Debug, PartialEq)]
pub struct Committed {
    pub commit_id: String,
    pub result: CommitResult,
}

/// Why a connection dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DisconnectReason {
    /// The connection could not be made or broke.
    Transport(Transport),
    /// The server refused the WebSocket upgrade with this HTTP status.
    Refused { status: u16 },
    /// The server closed the connection, after a `Goodbye` when it sent one.
    Closed { code: Option<u16>, reason: String },
    /// The server broke the protocol, for example with a message larger than 1 MiB.
    ProtocolViolation(String),
    /// The connection, its upgrade or its `Welcome` took too long, or nothing arrived for two ping intervals.
    Timeout,
}

impl DisconnectReason {
    /// What failed, for logs: the step and the kind of failure, such as `connect: dns`, `upgrade: http 503` or
    /// `closed: 1011`.
    pub fn kind(&self) -> String {
        match self {
            DisconnectReason::Transport(transport) => {
                format!("{}: {}", transport.step, transport.cause.name())
            }
            DisconnectReason::Refused { status } => format!("upgrade: http {status}"),
            DisconnectReason::Closed {
                code: Some(code), ..
            } => format!("closed: {code}"),
            DisconnectReason::Closed { code: None, .. } => "closed".into(),
            DisconnectReason::ProtocolViolation(_) => "protocol".into(),
            DisconnectReason::Timeout => "timeout".into(),
        }
    }

    /// The underlying error with its sources, the server's close reason, or what broke the protocol. Empty when
    /// there is none.
    pub fn detail(&self) -> &str {
        match self {
            DisconnectReason::Transport(transport) => &transport.detail,
            DisconnectReason::Closed { reason, .. } => reason,
            DisconnectReason::ProtocolViolation(what) => what,
            DisconnectReason::Refused { .. } | DisconnectReason::Timeout => "",
        }
    }

    /// Whether the server could not be reached, rather than that it answered and closed or refused.
    pub fn unreachable(&self) -> bool {
        match self {
            DisconnectReason::Transport(transport) => transport.cause.unreachable(),
            DisconnectReason::Timeout => true,
            _ => false,
        }
    }
}

/// The reason in plain words.
impl fmt::Display for DisconnectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DisconnectReason::Transport(Transport {
                cause: Cause::Other,
                detail,
                ..
            }) => f.write_str(detail),
            DisconnectReason::Transport(transport) => transport.cause.fmt(f),
            DisconnectReason::Refused { status } => write!(f, "the server answered HTTP {status}"),
            DisconnectReason::Closed {
                code: Some(code), ..
            } => {
                write!(f, "the server closed the connection (code {code})")
            }
            DisconnectReason::Closed { code: None, .. } => f.write_str("the connection was closed"),
            DisconnectReason::ProtocolViolation(what) => {
                write!(f, "the server broke the protocol: {what}")
            }
            DisconnectReason::Timeout => f.write_str("the server did not answer in time"),
        }
    }
}
