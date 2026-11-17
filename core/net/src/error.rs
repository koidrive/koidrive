// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::time::Duration;

use koidrive_proto::v1::{Error as ProtoError, error::Code};
use prost::Message;

/// A refusal from the server: the `Error` of an RPC response or of an HTTP reply.
#[derive(Clone, Debug, PartialEq)]
pub struct ServerError(pub ProtoError);

impl ServerError {
    /// An empty body carries no refusal, so it is not read as `UNSPECIFIED`.
    pub(crate) fn decode(body: &[u8]) -> Option<ServerError> {
        if body.is_empty() {
            return None;
        }
        ProtoError::decode(body).ok().map(ServerError)
    }

    pub fn code(&self) -> Code {
        Code::try_from(self.0.code).unwrap_or(Code::Unspecified)
    }

    pub fn message(&self) -> &str {
        &self.0.message
    }

    /// The wait the server asked for, if any.
    pub fn retry_after(&self) -> Option<Duration> {
        (self.0.retry_after_ms > 0).then(|| Duration::from_millis(self.0.retry_after_ms))
    }

    /// Whether the refused request may be sent again (H1). An unknown code is never a reason to resend, so that a
    /// refusal a newer server adds stops older clients instead of making them loop.
    pub fn resend(&self) -> Resend {
        match self.code() {
            Code::RetryLater => Resend::Allowed,
            Code::QuotaExceeded if self.retry_after().is_some() => Resend::Allowed,
            Code::QuotaExceeded | Code::Unspecified => Resend::Never,
            _ => Resend::ByCode,
        }
    }

    pub fn missing_chunks(&self) -> &[Vec<u8>] {
        &self.0.missing_chunks
    }
}

/// What a client may do with a request the server refused (H1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resend {
    /// `RETRY_LATER`, or `QUOTA_EXCEEDED` with `retry_after_ms`: send it again after `retry_after`, or after a
    /// backoff when the server gave no wait.
    Allowed,
    /// A code this client does not know, `UNSPECIFIED` included, or `QUOTA_EXCEEDED` without `retry_after_ms`: the
    /// operation stops and reports the error, and is not sent again.
    Never,
    /// Another known code, which the caller handles by its meaning.
    ByCode,
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code().as_str_name(), self.0.message)
    }
}

impl std::error::Error for ServerError {}

/// A failed call to one of the HTTP endpoints of spec v0 section 15.1.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("the server answered HTTP {status}: {error}")]
    Server { status: u16, error: ServerError },
    #[error("the server answered HTTP {status} without an error message")]
    Status { status: u16 },
    #[error("could not reach the server")]
    Transport(#[source] reqwest::Error),
    /// The answer was larger than the endpoint allows, so reading it stopped there. Like `Transport`, the call may be
    /// tried again.
    #[error("the server sent an answer of more than {limit} bytes")]
    TooLarge { limit: usize },
    /// The server refused with an error that could not be read: cut short, too large, or not an `Error`. As with `Status`,
    /// only the status is known, and a caller that tries again bounds how often (H1).
    #[error("the server answered HTTP {status} with an error that could not be read")]
    Unreadable { status: u16 },
    #[error("the server sent a body that is not a valid {0}")]
    Decode(&'static str),
    #[error("the server URL cannot be used: {0}")]
    Url(String),
    /// An ID that cannot be one segment of a request path: empty, `.`, `..`, or holding an ASCII control character.
    /// Nothing was sent.
    #[error("{0:?} cannot be one segment of a request path")]
    PathSegment(String),
    /// The stored credential could not be read again after the server refused the one this process held.
    #[error("could not read the stored credential")]
    Credential(#[source] std::io::Error),
}

impl ApiError {
    /// The server refused the credentials or token with `UNAUTHENTICATED`: the token expired, the device is revoked or
    /// the refresh credential is wrong. A 401 with another code is a refusal like any other (H1).
    pub fn is_unauthenticated(&self) -> bool {
        matches!(self, ApiError::Server { status: 401, error } if error.code() == Code::Unauthenticated)
    }

    /// The HTTP status of a refusal, whether or not its error could be read.
    pub fn status(&self) -> Option<u16> {
        match self {
            ApiError::Server { status, .. }
            | ApiError::Status { status }
            | ApiError::Unreadable { status } => Some(*status),
            _ => None,
        }
    }

    /// The server's error, when it sent one.
    pub fn server(&self) -> Option<&ServerError> {
        match self {
            ApiError::Server { error, .. } => Some(error),
            _ => None,
        }
    }
}

/// Why a session stopped for good. Every pending and later call fails with it.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StopReason {
    #[error("the session was closed")]
    Closed,
    #[error("the server refused this device's credentials; it may have been revoked")]
    CredentialsRefused,
    #[error("the server closed the connection because this device was revoked")]
    DeviceRevoked,
    #[error("the server does not support this client's protocol versions: {0}")]
    UnsupportedVersion(String),
    #[error("this device is not a member of the space")]
    Forbidden,
    #[error("the server does not know this space")]
    NotFound,
    #[error("the server URL cannot be used for a WebSocket: {0}")]
    BadUrl(String),
    #[error("TLS cannot be set up: {0}")]
    Tls(String),
    /// The server refused to let this device connect, with an error the client may not retry (H1): a code it does not
    /// know, or `QUOTA_EXCEEDED` without a wait.
    #[error("the server refused the connection: {0}")]
    Refused(String),
}

/// Why the session did not resend the commit that was in flight when its connection dropped (RFC 0002 section 2).
/// Its outcome cannot be known from this connection, so the caller enters recovery (RFC 0002 section 6).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NotResent {
    #[error("the space is now in history {welcome}, but the commit was built in {commit}")]
    HistoryChanged { commit: String, welcome: String },
    #[error(
        "the server's device_seq for this device is {welcome}, which the commit's slot {slot} cannot account for"
    )]
    DeviceSeqMismatch { slot: u64, welcome: u64 },
}

/// A failed RPC.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum CallError {
    #[error("the server refused the call: {0}")]
    Server(ServerError),
    #[error("the commit was not resent: {0}")]
    NotResent(NotResent),
    #[error("the request is {bytes} bytes, more than the 8 MiB a ClientMessage may hold")]
    TooLarge { bytes: usize },
    #[error("the session stopped: {0}")]
    Stopped(StopReason),
    #[error("the server answered with a result of another kind")]
    UnexpectedResult,
}

impl CallError {
    /// The server's error code, when the server refused the call.
    pub fn code(&self) -> Option<Code> {
        match self {
            CallError::Server(error) => Some(error.code()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_retry_later_and_quota_with_a_wait_may_be_resent() {
        let error = |code: i32, retry_after_ms: u64| {
            ServerError(ProtoError {
                code,
                retry_after_ms,
                ..Default::default()
            })
        };
        assert_eq!(error(Code::RetryLater as i32, 0).resend(), Resend::Allowed);
        assert_eq!(error(Code::RetryLater as i32, 50).resend(), Resend::Allowed);
        assert_eq!(
            error(Code::QuotaExceeded as i32, 50).resend(),
            Resend::Allowed
        );
        assert_eq!(error(Code::QuotaExceeded as i32, 0).resend(), Resend::Never);
        assert_eq!(error(Code::Unspecified as i32, 50).resend(), Resend::Never);
        assert_eq!(
            error(999, 50).resend(),
            Resend::Never,
            "a code this client does not know"
        );
        assert_eq!(error(Code::Internal as i32, 50).resend(), Resend::ByCode);
        assert_eq!(error(Code::Forbidden as i32, 0).resend(), Resend::ByCode);
    }
}
