// SPDX-License-Identifier: Apache-2.0

use std::time::Duration;

use koidrive_net::{CallError, ChunkError, ServerError};
use koidrive_proto::v1::error::Code;

use crate::status::WaitKind;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    RetryAfter(Duration),
    Backoff,
    Halt,
}

pub fn after_refusal(error: &ServerError) -> Disposition {
    match Code::try_from(error.0.code) {
        Ok(Code::RetryLater) => error
            .retry_after()
            .map_or(Disposition::Backoff, Disposition::RetryAfter),
        Ok(Code::QuotaExceeded) => error
            .retry_after()
            .map_or(Disposition::Halt, Disposition::RetryAfter),
        Ok(Code::Internal) => Disposition::Backoff,
        Ok(_) | Err(_) => Disposition::Halt,
    }
}

pub fn wait_kind(refusal: Option<&ServerError>) -> WaitKind {
    match refusal.map(|refusal| Code::try_from(refusal.0.code)) {
        Some(Ok(Code::QuotaExceeded)) => WaitKind::Quota,
        _ => WaitKind::Retry,
    }
}

pub fn unreachable(error: &ChunkError) -> bool {
    matches!(error, ChunkError::Unavailable { cause: Some(cause), .. } if cause.unreachable())
}

pub fn after_call_error(error: &CallError) -> Disposition {
    match error {
        CallError::Server(error) => after_refusal(error),
        _ => Disposition::Halt,
    }
}

pub fn describe(error: &ServerError) -> String {
    match Code::try_from(error.0.code) {
        Ok(code) if code != Code::Unspecified => error.to_string(),
        _ => format!("unknown refusal code {}: {}", error.0.code, error.message()),
    }
}

#[cfg(test)]
mod tests {
    use koidrive_proto::v1::Error as ProtoError;

    use super::*;

    fn refusal(code: i32, retry_after_ms: u64) -> ServerError {
        ServerError(ProtoError {
            code,
            message: "m".into(),
            missing_chunks: Vec::new(),
            retry_after_ms,
        })
    }

    #[test]
    fn unknown_codes_halt_and_are_never_resent() {
        for code in [0, 99, 1_000, -1] {
            assert_eq!(
                after_refusal(&refusal(code, 5_000)),
                Disposition::Halt,
                "{code}"
            );
        }
        assert_eq!(describe(&refusal(99, 0)), "unknown refusal code 99: m");
    }

    #[test]
    fn a_quota_refusal_is_a_quota_wait_and_any_other_cause_a_retry() {
        assert_eq!(
            wait_kind(Some(&refusal(Code::QuotaExceeded as i32, 1_500))),
            WaitKind::Quota
        );
        assert_eq!(
            wait_kind(Some(&refusal(Code::RetryLater as i32, 1_500))),
            WaitKind::Retry
        );
        assert_eq!(wait_kind(None), WaitKind::Retry);
    }

    #[test]
    fn quota_is_retried_only_when_the_server_says_when() {
        let quota = Code::QuotaExceeded as i32;
        assert_eq!(after_refusal(&refusal(quota, 0)), Disposition::Halt);
        assert_eq!(
            after_refusal(&refusal(quota, 1_500)),
            Disposition::RetryAfter(Duration::from_millis(1_500))
        );
    }

    #[test]
    fn transient_codes_are_retried_and_known_refusals_halt() {
        let retry = Code::RetryLater as i32;
        assert_eq!(after_refusal(&refusal(retry, 0)), Disposition::Backoff);
        assert_eq!(
            after_refusal(&refusal(retry, 20)),
            Disposition::RetryAfter(Duration::from_millis(20))
        );
        assert_eq!(
            after_refusal(&refusal(Code::Internal as i32, 0)),
            Disposition::Backoff
        );
        for code in [
            Code::Unauthenticated,
            Code::Forbidden,
            Code::InvalidArgument,
            Code::LimitExceeded,
            Code::ReadOnly,
            Code::UnsupportedVersion,
            Code::NotFound,
        ] {
            assert_eq!(
                after_refusal(&refusal(code as i32, 1_000)),
                Disposition::Halt,
                "{code:?}"
            );
        }
        assert_eq!(
            after_call_error(&CallError::Server(refusal(retry, 0))),
            Disposition::Backoff
        );
        assert_eq!(
            after_call_error(&CallError::UnexpectedResult),
            Disposition::Halt
        );
    }
}
