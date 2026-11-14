// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::schema::{Counter, DailyReport, Event, Level, Measure};

/// A cheap handle that code passes around to record. Clones share one switch and one set of sums. While it is off it
/// records nothing and costs one branch per call. Recording takes a short lock on the sums and does no I/O.
#[derive(Clone, Debug, Default)]
pub struct Recorder(Arc<Shared>);

#[derive(Debug, Default)]
struct Shared {
    on: AtomicBool,
    pending: Mutex<Pending>,
}

/// What a recorder holds and [`Recorder::take`] hands over.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pending {
    pub days: BTreeMap<u32, DailyReport>,
    pub events: Vec<Event>,
}

impl Recorder {
    pub fn off() -> Self {
        Recorder::default()
    }

    pub fn on() -> Self {
        let recorder = Recorder::default();
        recorder.switch(true);
        recorder
    }

    pub fn is_on(&self) -> bool {
        self.0.on.load(Ordering::Relaxed)
    }

    /// Starts or stops recording, for every clone. Stopping drops what is held.
    pub fn switch(&self, on: bool) {
        let Ok(mut pending) = self.0.pending.lock() else {
            return;
        };
        self.0.on.store(on, Ordering::Relaxed);
        if !on {
            *pending = Pending::default();
        }
    }

    pub fn count(&self, counter: Counter, by: u64) {
        self.count_on(crate::today(), counter, by);
    }

    pub fn measure(&self, measure: Measure, value: u64) {
        self.measure_on(crate::today(), measure, value);
    }

    /// Keeps `value` when it is the highest reading of `level` today.
    pub fn level(&self, level: Level, value: u64) {
        self.level_on(crate::today(), level, value);
    }

    /// Keeps an event, unless [`Event::PER_KIND_PER_DAY`] of its kind are already held for its day.
    pub fn event(&self, event: Event) {
        self.with(|pending| {
            let same = (pending.events.iter())
                .filter(|held| held.kind() == event.kind() && held.day() == event.day())
                .count();
            if same < Event::PER_KIND_PER_DAY {
                pending.events.push(event);
            }
        });
    }

    /// Everything recorded so far, leaving the recorder empty.
    pub fn take(&self) -> Pending {
        let mut taken = Pending::default();
        self.with(|pending| taken = std::mem::take(pending));
        taken
    }

    pub fn restore(&self, taken: Pending) {
        self.with(|pending| {
            for (day, report) in taken.days {
                pending
                    .days
                    .entry(day)
                    .or_insert_with(|| DailyReport {
                        day,
                        ..DailyReport::default()
                    })
                    .merge(report);
            }
            let mut events = taken.events;
            for event in std::mem::take(&mut pending.events) {
                let same = (events.iter())
                    .filter(|held| held.kind() == event.kind() && held.day() == event.day())
                    .count();
                if same < Event::PER_KIND_PER_DAY {
                    events.push(event);
                }
            }
            pending.events = events;
        });
    }

    pub fn count_on(&self, day: u32, counter: Counter, by: u64) {
        let (metric, label) = counter.key();
        self.with(|pending| {
            let report = pending.days.entry(day).or_insert_with(|| DailyReport {
                day,
                ..DailyReport::default()
            });
            let sum = report
                .counters
                .entry(metric.to_owned())
                .or_default()
                .entry(label.to_owned())
                .or_default();
            *sum = sum.saturating_add(by);
        });
    }

    pub(crate) fn level_on(&self, day: u32, level: Level, value: u64) {
        let (metric, label) = level.key();
        self.with(|pending| {
            let report = pending.days.entry(day).or_insert_with(|| DailyReport {
                day,
                ..DailyReport::default()
            });
            let highest = report
                .levels
                .entry(metric.to_owned())
                .or_default()
                .entry(label.to_owned())
                .or_default();
            *highest = (*highest).max(value);
        });
    }

    pub fn measure_on(&self, day: u32, measure: Measure, value: u64) {
        let (metric, label, _) = measure.key();
        self.with(|pending| {
            let report = pending.days.entry(day).or_insert_with(|| DailyReport {
                day,
                ..DailyReport::default()
            });
            let labels = report.histograms.entry(metric.to_owned()).or_default();
            labels
                .entry(label.to_owned())
                .or_insert_with(|| measure.histogram())
                .record(value);
        });
    }

    fn with(&self, update: impl FnOnce(&mut Pending)) {
        if self.is_on()
            && let Ok(mut pending) = self.0.pending.lock()
            && self.is_on()
        {
            update(&mut pending);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Command, SourceFile};

    fn panic_on(day: u32) -> Event {
        Event::Panic {
            day,
            file: SourceFile::from_location("cli/src/main.rs"),
            line: 1,
        }
    }

    #[test]
    fn an_off_recorder_holds_nothing() {
        let recorder = Recorder::off();
        recorder.count(Counter::Command(Command::Status), 1);
        recorder.event(panic_on(1));
        assert_eq!(recorder.take(), Pending::default());
    }

    #[test]
    fn a_switch_reaches_every_clone_and_stopping_drops_the_sums() {
        let recorder = Recorder::off();
        let clone = recorder.clone();
        clone.count_on(1, Counter::Command(Command::Status), 1);
        recorder.switch(true);
        assert!(clone.is_on());
        clone.count_on(1, Counter::Command(Command::Folder), 1);
        assert_eq!(
            recorder.take().days[&1].counters["command"],
            BTreeMap::from([("folder".to_owned(), 1)])
        );
        clone.event(panic_on(1));
        recorder.switch(false);
        clone.count_on(1, Counter::Command(Command::Status), 1);
        recorder.switch(true);
        assert_eq!(recorder.take(), Pending::default());
    }

    #[test]
    fn counts_sum_per_day_and_label() {
        let recorder = Recorder::on();
        recorder.count_on(10, Counter::Command(Command::Status), 1);
        recorder.count_on(10, Counter::Command(Command::Status), 2);
        recorder.count_on(11, Counter::Command(Command::Folder), 1);
        let taken = recorder.take();
        assert_eq!(taken.days[&10].counters["command"]["status"], 3);
        assert_eq!(taken.days[&11].counters["command"]["folder"], 1);
        assert_eq!(recorder.take(), Pending::default());
    }

    #[test]
    fn a_looping_event_is_kept_at_most_20_times_a_day() {
        let recorder = Recorder::on();
        for _ in 0..50 {
            recorder.event(panic_on(5));
        }
        recorder.event(panic_on(6));
        let taken = recorder.take();
        assert_eq!(
            taken.events.iter().filter(|event| event.day() == 5).count(),
            Event::PER_KIND_PER_DAY
        );
        assert_eq!(taken.events.len(), Event::PER_KIND_PER_DAY + 1);
    }

    #[test]
    fn restored_sums_add_to_what_came_after() {
        let recorder = Recorder::on();
        recorder.count_on(3, Counter::Command(Command::Status), 1);
        let taken = recorder.take();
        recorder.count_on(3, Counter::Command(Command::Status), 4);
        recorder.restore(taken);
        assert_eq!(recorder.take().days[&3].counters["command"]["status"], 5);
    }

    #[test]
    fn restoring_keeps_the_event_cap() {
        let recorder = Recorder::on();
        for _ in 0..Event::PER_KIND_PER_DAY {
            recorder.event(panic_on(5));
        }
        for _ in 0..3 {
            let taken = recorder.take();
            for _ in 0..Event::PER_KIND_PER_DAY {
                recorder.event(panic_on(5));
            }
            recorder.restore(taken);
        }
        assert_eq!(recorder.take().events.len(), Event::PER_KIND_PER_DAY);
    }
}
