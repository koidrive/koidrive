// SPDX-License-Identifier: Apache-2.0

use std::future::Future;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use crate::backoff::Backoff;
use crate::error::{ApiError, Resend};

const SENDS: u32 = 4;
const BACKOFF: Backoff = Backoff {
    initial: Duration::from_millis(500),
    max: Duration::from_secs(4),
};
const LONGEST_WAIT: Duration = Duration::from_secs(30);

/// A new `request_id` for one intended create (RFC 0024): 16 random bytes, base64url without padding.
pub fn request_id() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("the operating system provides random bytes");
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Sends a create until its answer arrives, at most four times. `send` must send the same request, `request_id`
/// included, each time, so the server answers a resend with what the first send created (RFC 0024). An answer that
/// was lost, a 5xx status and a refusal whose code allows a resend lead to a resend; any other refusal is returned at
/// once. A resend waits at most 30 seconds, so a refusal that asks for a longer wait, such as a daily limit, is returned
/// at once with its `retry_after`.
pub async fn resend_lost<T, F, Fut>(mut send: F) -> Result<T, ApiError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ApiError>>,
{
    let mut sent = 0;
    loop {
        let error = match send().await {
            Err(error) if sent + 1 < SENDS => error,
            outcome => return outcome,
        };
        let wait = match &error {
            ApiError::Transport(_) | ApiError::TooLarge { .. } => BACKOFF.delay(sent),
            ApiError::Server { error, .. } if error.resend() == Resend::Allowed => {
                error.retry_after().unwrap_or_else(|| BACKOFF.delay(sent))
            }
            ApiError::Server { status, .. }
            | ApiError::Status { status }
            | ApiError::Unreadable { status }
                if *status >= 500 =>
            {
                BACKOFF.delay(sent)
            }
            _ => return Err(error),
        };
        if wait > LONGEST_WAIT {
            return Err(error);
        }
        tokio::time::sleep(wait).await;
        sent += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ServerError;
    use koidrive_proto::v1::{Error as ProtoError, error::Code};
    use std::cell::Cell;

    fn refusal(status: u16, code: Code) -> ApiError {
        ApiError::Server {
            status,
            error: ServerError(ProtoError {
                code: code as i32,
                ..ProtoError::default()
            }),
        }
    }

    async fn sends(answers: Vec<Result<u8, ApiError>>) -> (Result<u8, ApiError>, usize) {
        let answers = std::cell::RefCell::new(answers.into_iter());
        let count = Cell::new(0);
        let outcome = resend_lost(|| {
            count.set(count.get() + 1);
            let next = answers.borrow_mut().next().expect("an answer per send");
            async move { next }
        })
        .await;
        (outcome, count.get())
    }

    #[tokio::test(start_paused = true)]
    async fn a_lost_answer_is_sent_again_until_one_arrives() {
        let (outcome, count) = sends(vec![
            Err(ApiError::TooLarge { limit: 1 }),
            Err(ApiError::Status { status: 502 }),
            Err(refusal(429, Code::RetryLater)),
            Ok(7),
        ])
        .await;
        assert_eq!((outcome.unwrap(), count), (7, 4));
    }

    #[tokio::test(start_paused = true)]
    async fn a_refusal_is_returned_without_a_resend() {
        let (outcome, count) = sends(vec![Err(refusal(409, Code::InvalidArgument))]).await;
        assert_eq!(outcome.unwrap_err().status(), Some(409));
        assert_eq!(count, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn the_fourth_lost_answer_is_returned() {
        let lost = || Err(ApiError::Unreadable { status: 503 });
        let (outcome, count) = sends(vec![lost(), lost(), lost(), lost()]).await;
        assert_eq!(outcome.unwrap_err().status(), Some(503));
        assert_eq!(count, 4);
    }

    #[tokio::test(start_paused = true)]
    async fn a_refusal_that_asks_for_a_long_wait_is_returned_at_once() {
        let daily_limit = || {
            Err(ApiError::Server {
                status: 429,
                error: ServerError(ProtoError {
                    code: Code::QuotaExceeded as i32,
                    retry_after_ms: 6 * 3_600_000,
                    ..ProtoError::default()
                }),
            })
        };
        let started = tokio::time::Instant::now();
        let (outcome, count) = sends(vec![daily_limit()]).await;
        let error = outcome.unwrap_err();
        assert_eq!(
            error.server().and_then(ServerError::retry_after),
            Some(Duration::from_secs(6 * 3600))
        );
        assert_eq!(count, 1);
        assert_eq!(started.elapsed(), Duration::ZERO);

        let (outcome, count) = sends(vec![
            Err(ApiError::Server {
                status: 429,
                error: ServerError(ProtoError {
                    code: Code::RetryLater as i32,
                    retry_after_ms: 30_000,
                    ..ProtoError::default()
                }),
            }),
            Ok(7),
        ])
        .await;
        assert_eq!((outcome.unwrap(), count), (7, 2));
    }

    #[test]
    fn request_ids_are_22_characters_and_differ() {
        let (one, two) = (request_id(), request_id());
        assert_eq!(one.len(), 22);
        assert_ne!(one, two);
    }
}
