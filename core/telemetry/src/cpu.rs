// SPDX-License-Identifier: Apache-2.0

use crate::{Recorder, schema::Counter};

#[derive(Default)]
pub struct Cpu {
    previous: Option<(u64, u32, u64)>,
}

impl Cpu {
    pub fn sample(&mut self, recorder: &Recorder) {
        let Some(session) = recorder.session() else {
            return;
        };
        self.record(recorder, session, crate::this_hour(), process_nanoseconds());
    }

    fn record(&mut self, recorder: &Recorder, session: u64, hour: u32, nanos: Option<u64>) {
        let previous = self.previous.take();
        let Some(nanos) = nanos else { return };
        self.previous = Some((session, hour, nanos));
        if let Some((before_session, before_hour, before)) = previous
            && before_session == session
            && (before_hour..=before_hour.saturating_add(1)).contains(&hour)
            && nanos >= before
        {
            recorder.count_on(
                hour,
                Counter::CpuMilliseconds,
                nanos / 1_000_000 - before / 1_000_000,
            );
        }
    }
}

fn process_nanoseconds() -> Option<u64> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let time = rustix::time::clock_gettime_dynamic(rustix::time::DynamicClockId::Known(
            rustix::time::ClockId::ProcessCPUTime,
        ))
        .ok()?;
        u64::try_from(time.tv_sec)
            .ok()?
            .checked_mul(1_000_000_000)?
            .checked_add(u64::try_from(time.tv_nsec).ok()?)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_time_goes_to_the_hour_a_sample_ends_in_without_errors_or_rewound_clocks() {
        let recorder = Recorder::on();
        let mut cpu = Cpu::default();
        for (hour, nanos) in [
            (1, Some(0)),
            (1, Some(5_000_000)),
            (2, Some(100_000_000)),
            (2, Some(107_000_000)),
            (2, None),
            (2, Some(900_000_000)),
            (2, Some(903_000_000)),
            (2, Some(1)),
        ] {
            cpu.record(&recorder, 1, hour, nanos);
        }
        let value = serde_json::to_value(recorder.take().hours).unwrap();
        assert_eq!(value["1"]["counters"]["process"]["cpu_ms"], 5);
        assert_eq!(value["2"]["counters"]["process"]["cpu_ms"], 105);
    }

    #[test]
    fn cpu_time_spent_while_reports_were_off_is_not_counted() {
        let recorder = Recorder::on();
        let mut cpu = Cpu::default();
        for (session, nanos) in [(1, 0), (1, 4_000_000), (2, 90_000_000), (2, 93_000_000)] {
            cpu.record(&recorder, session, 1, Some(nanos));
        }
        let value = serde_json::to_value(recorder.take().hours).unwrap();
        assert_eq!(value["1"]["counters"]["process"]["cpu_ms"], 7);
    }

    #[test]
    fn cpu_off_has_no_clock_or_report_state() {
        let mut cpu = Cpu::default();
        cpu.sample(&Recorder::off());
        assert_eq!(cpu.previous, None);
    }

    #[test]
    fn submillisecond_samples_accumulate_and_gaps_of_hours_are_not_attributed() {
        let recorder = Recorder::on();
        let mut cpu = Cpu::default();
        for (hour, nanos) in [
            (1, 1),
            (1, 500_000),
            (1, 1_000_001),
            (4, 900_000_000),
            (4, 902_000_000),
        ] {
            cpu.record(&recorder, 1, hour, Some(nanos));
        }
        let value = serde_json::to_value(recorder.take().hours).unwrap();
        assert_eq!(value["1"]["counters"]["process"]["cpu_ms"], 1);
        assert_eq!(value["4"]["counters"]["process"]["cpu_ms"], 2);
        assert!(value.get("2").is_none());
        assert!(value.get("3").is_none());
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn process_clock_is_available() {
        assert!(process_nanoseconds().is_some());
    }
}
