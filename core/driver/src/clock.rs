// SPDX-License-Identifier: Apache-2.0

#[cfg(not(test))]
pub(crate) use real_time::{instant, system};
#[cfg(test)]
pub(crate) use virtual_time::{advance, instant, system};

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

    pub(crate) fn advance(by: Duration) {
        AHEAD.set(AHEAD.get() + by);
    }
}
