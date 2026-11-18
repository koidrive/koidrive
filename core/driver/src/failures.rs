// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::sync::Mutex;

use koidrive_net::{CallError, ChunkError, ServerError, StopReason};
use koidrive_proto::v1::error::{Cause, Code};
pub use koidrive_telemetry::schema::Failure;
pub(crate) use koidrive_telemetry::schema::{
    Asked, Halted, Refusal, ServerCause, TooLarge, Transfer,
};

use crate::disk::Stop;
use crate::question::QuestionKind;

static FAILURES: Mutex<BTreeMap<Failure, u64>> = Mutex::new(BTreeMap::new());

#[cfg(test)]
thread_local! {
    static HERE: std::cell::RefCell<BTreeMap<Failure, u64>> =
        const { std::cell::RefCell::new(BTreeMap::new()) };
}

/// How many times each kind failed in this process.
pub fn failures() -> BTreeMap<Failure, u64> {
    FAILURES
        .lock()
        .map(|failures| failures.clone())
        .unwrap_or_default()
}

#[cfg(test)]
pub(crate) fn count(failure: Failure) -> u64 {
    failures().get(&failure).copied().unwrap_or(0)
}

#[cfg(test)]
pub(crate) fn take_counted_here() -> BTreeMap<Failure, u64> {
    HERE.take()
}

pub(crate) fn failed(failure: Failure) {
    if let Ok(mut failures) = FAILURES.lock() {
        *failures.entry(failure).or_default() += 1;
    }
    #[cfg(test)]
    HERE.with_borrow_mut(|here| *here.entry(failure).or_default() += 1);
}

pub(crate) fn refusal(error: &ServerError) -> Refusal {
    match error.code() {
        Code::Unauthenticated => Refusal::Unauthenticated,
        Code::Forbidden => Refusal::Forbidden,
        Code::InvalidArgument => Refusal::InvalidArgument,
        Code::MissingChunks => Refusal::MissingChunks,
        Code::LimitExceeded => Refusal::LimitExceeded,
        Code::QuotaExceeded => Refusal::QuotaExceeded,
        Code::ReadOnly => Refusal::ReadOnly,
        Code::RetryLater => Refusal::RetryLater,
        Code::UnsupportedVersion => Refusal::UnsupportedVersion,
        Code::Internal => Refusal::Internal,
        Code::HistoryMismatch => Refusal::HistoryMismatch,
        Code::StaleDeviceState => Refusal::StaleDeviceState,
        Code::HeadMoved => Refusal::HeadMoved,
        Code::AheadOfServer => Refusal::AheadOfServer,
        Code::NotFound => Refusal::NotFound,
        Code::ConfirmationRequired => Refusal::ConfirmationRequired,
        Code::Unspecified => Refusal::Unknown,
    }
}

fn server_cause(error: &ServerError) -> Option<ServerCause> {
    Some(match Cause::try_from(error.0.cause) {
        Ok(Cause::Unspecified) => return None,
        Ok(Cause::Exception) => ServerCause::Exception,
        Ok(Cause::R2RateLimited) => ServerCause::R2RateLimited,
        Ok(Cause::R2Unavailable) => ServerCause::R2Unavailable,
        Ok(Cause::R2Error) => ServerCause::R2Error,
        Ok(Cause::DoOverloaded) => ServerCause::DoOverloaded,
        Ok(Cause::DoUnavailable) => ServerCause::DoUnavailable,
        Ok(Cause::CpuExceeded) => ServerCause::CpuExceeded,
        Ok(Cause::MemoryExceeded) => ServerCause::MemoryExceeded,
        Ok(Cause::SubrequestLimit) => ServerCause::SubrequestLimit,
        Ok(Cause::StorageFull) => ServerCause::StorageFull,
        Ok(Cause::DailyLimit) => ServerCause::DailyLimit,
        Err(_) => ServerCause::Unknown,
    })
}

fn edge_cause(code: u16) -> ServerCause {
    match code {
        1101 => ServerCause::EdgeException,
        1102 => ServerCause::EdgeResourceLimit,
        1027 => ServerCause::EdgeDailyRequests,
        _ => ServerCause::EdgeOther,
    }
}

fn server_refused(error: &ServerError) {
    failed(Failure::Refused(refusal(error)));
    if let Some(cause) = server_cause(error) {
        failed(Failure::Server(cause));
    }
}

pub(crate) fn call_failed(error: &CallError) {
    if let CallError::Server(refusal) = error {
        server_refused(refusal);
    }
}

pub(crate) fn transfer_failed(error: &ChunkError, as_failure: fn(Transfer) -> Failure) {
    let why = match error {
        ChunkError::Call(CallError::Stopped(_)) => return,
        ChunkError::Call(call) => {
            call_failed(call);
            Transfer::Call
        }
        ChunkError::SourceChanged { .. } => Transfer::SourceChanged,
        ChunkError::Corrupt { .. } => Transfer::Corrupt,
        ChunkError::Refused {
            error,
            cloudflare_error_code,
            ..
        } => {
            if let Some(refusal) = error {
                server_refused(refusal);
            }
            if let Some(code) = cloudflare_error_code {
                failed(Failure::Server(edge_cause(*code)));
            }
            Transfer::Refused
        }
        ChunkError::Unavailable { .. } => Transfer::Unavailable,
        ChunkError::NoTarget { .. } => Transfer::NoTarget,
        ChunkError::Io { .. } => Transfer::Io,
    };
    failed(as_failure(why));
}

pub(crate) fn halted(stop: &Stop) -> Halted {
    match stop {
        Stop::Withdrawn(_) => Halted::Withdrawn,
        Stop::Root(_) => Halted::Root,
        Stop::NoReplace => Halted::NoReplace,
        Stop::DiskBehind { .. } => Halted::DiskBehind,
        Stop::OtherRoot => Halted::OtherRoot,
        Stop::Record { .. } => Halted::Record,
        Stop::Store(_) => Halted::Store,
        Stop::Io(_) => Halted::Io,
        Stop::Session(reason) => session_ended(reason),
        Stop::Refused(_) | Stop::Policy { .. } => Halted::Refused,
        Stop::Protocol(_) => Halted::Protocol,
    }
}

pub(crate) fn session_ended(reason: &StopReason) -> Halted {
    match reason {
        StopReason::Closed => Halted::SessionClosed,
        StopReason::CredentialsRefused => Halted::CredentialsRefused,
        StopReason::DeviceRevoked => Halted::DeviceRevoked,
        StopReason::UnsupportedVersion(_) => Halted::UnsupportedVersion,
        StopReason::Forbidden => Halted::NotAMember,
        StopReason::NotFound => Halted::SpaceNotFound,
        StopReason::BadUrl(_) | StopReason::Tls(_) => Halted::SessionStart,
        StopReason::Refused(_) => Halted::ConnectionRefused,
    }
}

pub(crate) fn asked(kind: &QuestionKind) -> Asked {
    match kind {
        QuestionKind::MassDelete { .. } => Asked::MassDelete,
        QuestionKind::RemoteMassDelete { .. } => Asked::RemoteMassDelete,
        QuestionKind::AmbiguousDisappearance => Asked::AmbiguousDisappearance,
        QuestionKind::Recovery { .. } => Asked::Recovery,
        QuestionKind::Conflict { .. } => Asked::Conflict,
    }
}

#[cfg(test)]
mod tests {
    use koidrive_proto::v1::Error as ProtoError;

    use super::*;

    fn refused(error: Option<ServerError>, cloudflare_error_code: Option<u16>) -> ChunkError {
        ChunkError::Refused {
            hash: Vec::new(),
            status: 503,
            error,
            cloudflare_error_code,
        }
    }

    fn server(code: Code, cause: i32) -> ServerError {
        ServerError(ProtoError {
            code: code as i32,
            cause,
            ..Default::default()
        })
    }

    #[test]
    fn an_edge_error_page_is_named_by_its_code() {
        let names = [1101, 1102, 1027, 1015, 520].map(|code| edge_cause(code).name());
        assert_eq!(
            names,
            [
                "edge_exception",
                "edge_resource_limit",
                "edge_daily_requests",
                "edge_other",
                "edge_other",
            ]
        );
    }

    #[test]
    fn a_refusal_counts_the_server_cause_or_the_edge_error_beside_its_code() {
        take_counted_here();
        transfer_failed(
            &refused(
                Some(server(Code::RetryLater, Cause::DoOverloaded as i32)),
                None,
            ),
            Failure::Upload,
        );
        transfer_failed(&refused(None, Some(1102)), Failure::Download);
        transfer_failed(&refused(None, Some(1015)), Failure::Download);
        call_failed(&CallError::Server(server(Code::Internal, 999)));
        call_failed(&CallError::Server(server(Code::Internal, 0)));
        assert_eq!(
            take_counted_here(),
            BTreeMap::from([
                (Failure::Upload(Transfer::Refused), 1),
                (Failure::Download(Transfer::Refused), 2),
                (Failure::Refused(Refusal::RetryLater), 1),
                (Failure::Refused(Refusal::Internal), 2),
                (Failure::Server(ServerCause::DoOverloaded), 1),
                (Failure::Server(ServerCause::Unknown), 1),
                (Failure::Server(ServerCause::EdgeResourceLimit), 1),
                (Failure::Server(ServerCause::EdgeOther), 1),
            ])
        );
    }
}
