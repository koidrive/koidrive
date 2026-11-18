// SPDX-License-Identifier: Apache-2.0

#[cfg(not(test))]
pub(crate) use real_time::{instant, system};
#[cfg(test)]
pub(crate) use virtual_time::{advance_to, instant, system};

pub(crate) fn unix_ms() -> u64 {
    (system().duration_since(std::time::UNIX_EPOCH)).map_or(0, |since| since.as_millis() as u64)
}

#[cfg(not(test))]
mod real_time {
    use std::time::{Instant, SystemTime};

    pub(crate) fn instant() -> Instant {
        Instant::now()
    }

    pub(crate) fn system() -> SystemTime {
        SystemTime::now()
    }
}

#[cfg(test)]
mod virtual_time {
    use std::cell::Cell;
    use std::time::{Duration, Instant, SystemTime};

    thread_local! {
        static AHEAD: Cell<Duration> = const { Cell::new(Duration::ZERO) };
    }

    pub(crate) fn instant() -> Instant {
        Instant::now() + AHEAD.get()
    }

    pub(crate) fn system() -> SystemTime {
        SystemTime::now() + AHEAD.get()
    }

    pub(crate) fn advance_to(at: Instant) {
        AHEAD.set(
            AHEAD
                .get()
                .max(at.saturating_duration_since(Instant::now())),
        );
    }
}
