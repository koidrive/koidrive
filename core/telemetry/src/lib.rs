// SPDX-License-Identifier: Apache-2.0

//! Opt-in usage reports (docs/guide/telemetry.md). Code that records something calls a [`Recorder`] with one of the
//! closed types in [`schema`]: enums, counts and durations. No recording call takes text, so a path, a name or an
//! error message has no field to travel in. The recorder sums what it is given per UTC hour in memory and keeps events with the second they happened. [`Store`]
//! keeps it on disk under the profile's state directory until [`send`] posts it to PostHog's batch API, through
//! the project's own host.

mod app_crash;
mod consent;
mod cpu;
mod panic;
mod recorder;
pub mod schema;
mod send;
mod store;
mod system;

pub use consent::{Mode, Source, Status, VARIABLE, resolve, should_ask};
pub use cpu::Cpu;
pub use panic::install_panic_hook;
pub use recorder::{Pending, Recorder};
pub use schema::{Event, EventUuid, HourlyReport, InstallId, Origin};
pub use send::{
    ENDPOINT, ENDPOINT_VARIABLE, PROJECT_TOKEN, Sent, TOKEN_VARIABLE, UploadError, Uploader, batch,
    old_batch, send,
};
pub use store::{Batch, OldBatch, Queued, Sending, Settings, Store};
pub use system::Environment;

/// Seconds since 1970-01-01T00:00Z, the time an event carries.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// Hours since 1970-01-01T00:00Z, the period an hourly report sums.
pub fn this_hour() -> u32 {
    u32::try_from(now() / 3600).unwrap_or(u32::MAX)
}
