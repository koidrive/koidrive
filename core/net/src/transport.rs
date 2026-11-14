// SPDX-License-Identifier: Apache-2.0

use std::error::Error;
use std::fmt;
use std::io;

use tokio_tungstenite::tungstenite::{self, error::ProtocolError};

/// A connection to the server that could not be made or broke: the step it failed in, what kind of failure it was,
/// and the error with its sources.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transport {
    pub step: Step,
    pub cause: Cause,
    /// The error and each of its sources, outermost first, joined with `: `.
    pub detail: String,
}

/// Where a connection failed: one of the steps that open a session's WebSocket, or the open connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// The HTTP request for an access token.
    Token,
    /// Looking up the server's address and opening a TCP connection to it.
    Connect,
    Tls,
    /// The WebSocket upgrade request.
    Upgrade,
    /// Reading from or writing to the open WebSocket.
    Stream,
}

/// What kind of failure stopped a connection or a transfer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    /// The server's name did not resolve to an address.
    Dns,
    /// Nothing listens at the server's address, or something there turned the connection away.
    Refused,
    /// No route to the server: this device is offline, or the network or the host is unreachable.
    Unreachable,
    /// The network did not answer in time.
    TimedOut,
    /// The connection was reset or closed without a WebSocket close.
    Reset,
    /// The server's certificate is not trusted for its name.
    Certificate,
    /// The TLS handshake failed for another reason.
    Tls,
    /// The server answered the request with this HTTP status.
    Status(u16),
    Other,
}

impl Transport {
    pub fn of(step: Step, error: &(dyn Error + 'static)) -> Transport {
        let (cause, detail) = Cause::of(error);
        Transport {
            step,
            cause,
            detail,
        }
    }
}

/// `error` and each of its sources, outermost first, joined with `: `.
pub fn chain(error: &(dyn Error + 'static)) -> String {
    Cause::of(error).1
}

fn cause_of(error: &(dyn Error + 'static)) -> Option<Cause> {
    if let Some(error) = error.downcast_ref::<io::Error>() {
        let tls = error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<rustls::Error>());
        return match tls {
            Some(tls) => Some(tls_cause(tls)),
            None => io_cause(error.kind()),
        };
    }
    if let Some(tls) = error.downcast_ref::<rustls::Error>() {
        return Some(tls_cause(tls));
    }
    if let Some(error) = error.downcast_ref::<reqwest::Error>() {
        return error.is_timeout().then_some(Cause::TimedOut);
    }
    if let Some(error) = error.downcast_ref::<tungstenite::Error>() {
        return match error {
            tungstenite::Error::ConnectionClosed
            | tungstenite::Error::AlreadyClosed
            | tungstenite::Error::Protocol(
                ProtocolError::ResetWithoutClosingHandshake | ProtocolError::HandshakeIncomplete,
            ) => Some(Cause::Reset),
            _ => None,
        };
    }
    (error.to_string() == "dns error").then_some(Cause::Dns)
}

fn io_cause(kind: io::ErrorKind) -> Option<Cause> {
    use io::ErrorKind::*;
    match kind {
        ConnectionRefused => Some(Cause::Refused),
        ConnectionReset | ConnectionAborted | BrokenPipe | UnexpectedEof | NotConnected => {
            Some(Cause::Reset)
        }
        TimedOut => Some(Cause::TimedOut),
        NetworkUnreachable | HostUnreachable | NetworkDown | AddrNotAvailable => {
            Some(Cause::Unreachable)
        }
        _ => None,
    }
}

fn tls_cause(error: &rustls::Error) -> Cause {
    match error {
        rustls::Error::InvalidCertificate(_) => Cause::Certificate,
        _ => Cause::Tls,
    }
}

impl Cause {
    /// The kind of the first error in `error`'s chain whose kind is known, and the chain, outermost first, joined
    /// with `: `.
    pub fn of(error: &(dyn Error + 'static)) -> (Cause, String) {
        let mut detail = String::new();
        let mut cause = None;
        let mut next = Some(error);
        while let Some(error) = next {
            let text = error.to_string();
            if !detail.ends_with(&text) {
                if !detail.is_empty() {
                    detail.push_str(": ");
                }
                detail.push_str(&text);
            }
            cause = cause.or_else(|| cause_of(error));
            next = error.source();
        }
        (cause.unwrap_or(Cause::Other), detail)
    }

    /// Whether the failure means the server or its storage cannot be reached, rather than that it answered.
    pub fn unreachable(self) -> bool {
        !matches!(self, Cause::Status(_) | Cause::Other)
    }

    /// A short name for logs, such as `dns` or `http 503`.
    pub fn label(self) -> String {
        match self {
            Cause::Dns => "dns".into(),
            Cause::Refused => "refused".into(),
            Cause::Unreachable => "unreachable".into(),
            Cause::TimedOut => "timed_out".into(),
            Cause::Reset => "reset".into(),
            Cause::Certificate => "certificate".into(),
            Cause::Tls => "tls".into(),
            Cause::Status(status) => format!("http {status}"),
            Cause::Other => "other".into(),
        }
    }
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Step::Token => "token",
            Step::Connect => "connect",
            Step::Tls => "tls",
            Step::Upgrade => "upgrade",
            Step::Stream => "stream",
        })
    }
}

/// The cause in plain words.
impl fmt::Display for Cause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Cause::Dns => f.write_str("the server's address could not be found"),
            Cause::Refused => f.write_str("the server refused the connection"),
            Cause::Unreachable => f.write_str("the network or the server is unreachable"),
            Cause::TimedOut => f.write_str("the connection timed out"),
            Cause::Reset => f.write_str("the connection was cut"),
            Cause::Certificate => f.write_str("the server's certificate is not trusted"),
            Cause::Tls => f.write_str("a secure connection could not be set up"),
            Cause::Status(status) => write!(f, "the server answered HTTP {status}"),
            Cause::Other => f.write_str("the connection failed"),
        }
    }
}
