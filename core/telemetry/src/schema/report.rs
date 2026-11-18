// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{Counter, Level, Measure};

/// What one install recorded in one UTC hour. Its keys are always those of a [`Counter`], a [`Level`] or a [`Measure`]: only a
/// [`Recorder`](crate::Recorder) fills it, and reading one from JSON keeps only the keys of the closed set.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(from = "StoredReport")]
pub struct HourlyReport {
    /// Hours since 1970-01-01T00:00Z.
    pub hour: u32,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) counters: BTreeMap<String, BTreeMap<String, u64>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) levels: BTreeMap<String, BTreeMap<String, u64>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) histograms: BTreeMap<String, BTreeMap<String, Histogram>>,
}

#[derive(Deserialize)]
struct StoredReport {
    hour: u32,
    #[serde(default)]
    counters: BTreeMap<String, BTreeMap<String, u64>>,
    #[serde(default)]
    levels: BTreeMap<String, BTreeMap<String, u64>>,
    #[serde(default)]
    histograms: BTreeMap<String, BTreeMap<String, Histogram>>,
}

impl From<StoredReport> for HourlyReport {
    fn from(stored: StoredReport) -> Self {
        let mut report = HourlyReport {
            hour: stored.hour,
            ..HourlyReport::default()
        };
        for (metric, label) in Counter::all().map(Counter::key) {
            if let Some(&count) = stored
                .counters
                .get(metric)
                .and_then(|labels| labels.get(label))
            {
                (report.counters.entry(metric.to_owned()).or_default())
                    .insert(label.to_owned(), count);
            }
        }
        for (metric, label) in Level::all().map(Level::key) {
            if let Some(&value) = stored
                .levels
                .get(metric)
                .and_then(|labels| labels.get(label))
            {
                (report.levels.entry(metric.to_owned()).or_default())
                    .insert(label.to_owned(), value);
            }
        }
        for measure in Measure::all() {
            let (metric, label, shift) = measure.key();
            if let Some(histogram) = (stored.histograms.get(metric))
                .and_then(|labels| labels.get(label))
                .filter(|histogram| histogram.shift == shift)
            {
                let mut kept = measure.histogram();
                kept.merge(histogram);
                (report.histograms.entry(metric.to_owned()).or_default())
                    .insert(label.to_owned(), kept);
            }
        }
        report
    }
}

impl HourlyReport {
    pub fn merge(&mut self, other: HourlyReport) {
        for (metric, labels) in other.counters {
            let into = self.counters.entry(metric).or_default();
            for (label, count) in labels {
                let sum = into.entry(label).or_default();
                *sum = sum.saturating_add(count);
            }
        }
        for (metric, labels) in other.levels {
            let into = self.levels.entry(metric).or_default();
            for (label, value) in labels {
                let highest = into.entry(label).or_default();
                *highest = (*highest).max(value);
            }
        }
        for (metric, labels) in other.histograms {
            let into = self.histograms.entry(metric).or_default();
            for (label, histogram) in labels {
                into.entry(label)
                    .or_insert_with(|| Histogram::new(histogram.shift))
                    .merge(&histogram);
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.counters.is_empty() && self.levels.is_empty() && self.histograms.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Histogram {
    pub shift: u8,
    pub buckets: [u64; 16],
    pub count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sum: Option<u64>,
}

impl Histogram {
    pub fn new(shift: u8) -> Self {
        Histogram {
            shift,
            buckets: [0; 16],
            count: 0,
            sum: Some(0),
        }
    }

    pub fn record(&mut self, value: u64) {
        let scaled = value >> self.shift.min(63);
        let bucket = (64 - scaled.leading_zeros()).min(15) as usize;
        self.buckets[bucket] += 1;
        self.count += 1;
        self.sum = self.sum.map(|sum| sum.saturating_add(value));
    }

    pub fn merge(&mut self, other: &Histogram) {
        if other.shift != self.shift {
            return;
        }
        for (into, from) in self.buckets.iter_mut().zip(other.buckets) {
            *into = into.saturating_add(from);
        }
        self.count = self.count.saturating_add(other.count);
        self.sum = self
            .sum
            .zip(other.sum)
            .map(|(sum, other)| sum.saturating_add(other));
    }
}

/// A count as its bit length: 0 for 0, 1 for 1, 2 for 2 and 3, 3 for 4 to 7, and so on. Reports send this instead of
/// an exact number that could tell one setup from another.
pub fn magnitude(value: u64) -> u8 {
    (64 - value.leading_zeros()) as u8
}
