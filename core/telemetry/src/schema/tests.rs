// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;

use super::*;

#[test]
fn a_histogram_puts_each_value_in_its_power_of_two_bucket() {
    let mut histogram = Histogram::new(5);
    for value in [0, 31, 32, 63, 64, 1 << 18, 1 << 19, u64::MAX] {
        histogram.record(value);
    }
    let mut expected = [0u64; 16];
    expected[0] = 2;
    expected[1] = 2;
    expected[2] = 1;
    expected[14] = 1;
    expected[15] = 2;
    assert_eq!(histogram.buckets, expected);
    assert_eq!(histogram.count, 8);
    assert_eq!(histogram.sum, Some(u64::MAX));
}

#[test]
fn histograms_with_another_shift_do_not_merge() {
    let mut ms = Histogram::new(5);
    ms.record(100);
    let mut other = Histogram::new(10);
    other.record(100);
    ms.merge(&other);
    assert_eq!(ms.count, 1);
    let same = ms.clone();
    ms.merge(&same);
    assert_eq!(ms.count, 2);
}

#[test]
fn a_stored_report_keeps_only_known_keys() {
    let stored = serde_json::json!({
        "hour": 100,
        "counters": {
            "command": { "status": 3, "/home/alice/private": 1 },
            "/home/alice/private": { "confidential.txt": 1 },
        },
        "histograms": {
            "/home/alice": { "notes.txt": { "shift": 0, "buckets": vec![0u64; 16], "count": 1, "sum": 1 } },
        },
    });
    let report: HourlyReport = serde_json::from_value(stored).unwrap();
    assert_eq!(
        serde_json::to_value(&report).unwrap(),
        serde_json::json!({ "hour": 100, "counters": { "command": { "status": 3 } } })
    );
}

#[test]
fn a_stored_report_keeps_only_known_sync_counters_and_levels() {
    let stored = serde_json::json!({
        "hour": 100,
        "counters": { "sync": { "bytes_uploaded": 4096, "Alice Notes": 1 } },
        "levels": {
            "sync": { "spaces": 2, "nodes": 1500, "/home/alice": 1 },
            "Alice Notes": { "spaces": 1 },
        },
    });
    let report: HourlyReport = serde_json::from_value(stored).unwrap();
    assert_eq!(
        serde_json::to_value(&report).unwrap(),
        serde_json::json!({
            "hour": 100,
            "counters": { "sync": { "bytes_uploaded": 4096 } },
            "levels": { "sync": { "nodes": 1500, "spaces": 2 } },
        })
    );
}

#[test]
fn a_stored_report_keeps_only_known_failure_counters() {
    let stored = serde_json::json!({
        "hour": 100,
        "counters": {
            "halted": { "store_behind": 1, "the server refused it: Alice Notes": 1 },
            "refused": { "quota_exceeded": 2, "418": 1 },
            "asked": { "conflict": 3 },
            "/home/alice": { "conflict": 1 },
        },
    });
    let report: HourlyReport = serde_json::from_value(stored).unwrap();
    assert_eq!(
        serde_json::to_value(&report).unwrap(),
        serde_json::json!({
            "hour": 100,
            "counters": {
                "asked": { "conflict": 3 },
                "halted": { "store_behind": 1 },
                "refused": { "quota_exceeded": 2 },
            },
        })
    );
}

#[test]
fn ages_split_at_one_six_24_72_and_168_hours() {
    let hour = 3_600;
    let ages = [
        0,
        hour - 1,
        hour,
        6 * hour - 1,
        6 * hour,
        24 * hour,
        72 * hour - 1,
        72 * hour,
        168 * hour,
        u64::MAX,
    ]
    .map(|seconds| Age::of(seconds).name());
    assert_eq!(
        ages,
        [
            "under_1h", "under_1h", "1h_6h", "1h_6h", "6h_1d", "1d_3d", "1d_3d", "3d_7d",
            "over_7d", "over_7d"
        ]
    );
}

#[test]
fn a_cleared_problem_carries_only_its_kind_and_how_long_it_lasted() {
    let event = Event::ProblemCleared {
        at: 1_700_000_000,
        problem: Failure::Halted(Halted::Root),
        lasted: Age::OneToThreeDays,
    };
    let json = serde_json::to_value(&event).unwrap();
    assert_eq!(
        json,
        serde_json::json!({
            "type": "problem_cleared",
            "at": 1_700_000_000,
            "problem": "halted.root",
            "lasted": "1d_3d",
        })
    );
    assert_eq!(serde_json::from_value::<Event>(json).unwrap(), event);
    assert_eq!(event.kind(), ("problem_cleared", "halted", "root"));
}

#[test]
fn a_stored_report_keeps_only_known_health_levels() {
    let stored = serde_json::json!({
        "hour": 100,
        "levels": {
            "sync": { "halted": 1, "oldest_halt_minutes": 1680, "pending_nodes": 4, "paused": 0, "/home/alice": 1 },
            "halted": { "root": 1, "Alice Notes": 1 },
            "last_synced": { "1d_3d": 1, "2 days": 1 },
        },
    });
    let report: HourlyReport = serde_json::from_value(stored).unwrap();
    assert_eq!(
        serde_json::to_value(&report).unwrap(),
        serde_json::json!({
            "hour": 100,
            "levels": {
                "halted": { "root": 1 },
                "last_synced": { "1d_3d": 1 },
                "sync": { "halted": 1, "oldest_halt_minutes": 1680, "paused": 0, "pending_nodes": 4 },
            },
        })
    );
}

#[test]
fn levels_merge_as_the_highest_and_counters_as_the_sum() {
    let report = |bytes: u64, nodes: u64| -> HourlyReport {
        serde_json::from_value(serde_json::json!({
            "hour": 100,
            "counters": { "sync": { "bytes_downloaded": bytes } },
            "levels": { "sync": { "nodes": nodes } },
        }))
        .unwrap()
    };
    let mut hour = report(100, 70);
    hour.merge(report(20, 30));
    assert_eq!(hour.counters["sync"]["bytes_downloaded"], 120);
    assert_eq!(hour.levels["sync"]["nodes"], 70);
}

#[test]
fn every_counter_and_level_has_its_own_key() {
    let counters: std::collections::BTreeSet<_> = Counter::all().map(Counter::key).collect();
    assert_eq!(counters.len(), Counter::all().count());
    let levels: std::collections::BTreeSet<_> = Level::all().map(Level::key).collect();
    assert_eq!(levels.len(), Level::all().count());
}

#[test]
fn timing_measures_have_closed_unique_keys_and_separate_large_file_counts() {
    let keys: std::collections::BTreeSet<_> = Measure::all().map(Measure::key).collect();
    assert_eq!(keys.len(), Measure::all().count());
    assert_eq!(FileCount::bucket(0).name(), "0");
    assert_eq!(FileCount::bucket(1).name(), "1");
    assert_eq!(FileCount::bucket(100_000).name(), "65536_131071");
    assert_eq!(FileCount::bucket(400_000).name(), "262144_524287");
    assert_eq!(FileCount::bucket(u64::MAX).name(), "1048576_plus");
}

#[test]
fn duration_histograms_never_emit_sums_even_from_stored_reports() {
    let recorder = crate::Recorder::on();
    for measure in Measure::all() {
        recorder.measure_on(1, measure, 90_000);
        recorder.measure_on(1, measure, 600_000);
    }
    let report = recorder.take().hours.remove(&1).unwrap();
    let original = serde_json::to_value(&report).unwrap();
    let mut stored = original.clone();
    for measure in Measure::all() {
        let (metric, label, _) = measure.key();
        let histogram = &original["histograms"][metric][label];
        assert_eq!(histogram["count"], 2);
        if matches!(
            measure,
            Measure::FirstSync(_) | Measure::RemoteApplyMilliseconds | Measure::SyncingMilliseconds
        ) {
            assert!(histogram.get("sum").is_none());
            stored["histograms"][metric][label]["sum"] = serde_json::json!(1794527123456u64);
        } else {
            assert_eq!(histogram["sum"], 690_000);
        }
    }
    let restored: HourlyReport = serde_json::from_value(stored).unwrap();
    assert_eq!(serde_json::to_value(&restored).unwrap(), original);
    let mut merged = HourlyReport {
        hour: 1,
        ..HourlyReport::default()
    };
    merged.merge(report);
    merged.merge(restored);
    let merged = serde_json::to_value(merged).unwrap();
    for measure in Measure::all() {
        let (metric, label, _) = measure.key();
        let histogram = &merged["histograms"][metric][label];
        assert_eq!(histogram["count"], 4);
        for (index, count) in original["histograms"][metric][label]["buckets"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
        {
            assert_eq!(histogram["buckets"][index], count.as_u64().unwrap() * 2);
        }
        if matches!(
            measure,
            Measure::FirstSync(_) | Measure::RemoteApplyMilliseconds | Measure::SyncingMilliseconds
        ) {
            assert!(histogram.get("sum").is_none());
        } else {
            assert_eq!(histogram["sum"], 1_380_000);
        }
    }
}

#[test]
fn duration_histograms_distinguish_minutes_and_round_trip_with_shift_ten() {
    let recorder = crate::Recorder::on();
    for measure in Measure::all().filter(|measure| {
        matches!(
            measure,
            Measure::FirstSync(_) | Measure::RemoteApplyMilliseconds | Measure::SyncingMilliseconds
        )
    }) {
        let (metric, label, shift) = measure.key();
        assert_eq!(shift, 10);
        for millis in [1023, 60_000, 600_000, 5_280_000, 16_777_215, 16_777_216] {
            recorder.measure_on(1, measure, millis);
        }
        let mut report = recorder.take().hours.remove(&1).unwrap();
        let histogram = &report.histograms[metric][label];
        assert_eq!(histogram.buckets[0], 1);
        assert_eq!(histogram.buckets[6], 1);
        assert_eq!(histogram.buckets[10], 1);
        assert_eq!(histogram.buckets[13], 1);
        assert_eq!(histogram.buckets[14], 1);
        assert_eq!(histogram.buckets[15], 1);
        assert_eq!(histogram.count, 6);
        let stored = serde_json::to_value(&report).unwrap();
        assert_eq!(
            serde_json::from_value::<HourlyReport>(stored).unwrap(),
            report
        );
        report
            .histograms
            .get_mut(metric)
            .unwrap()
            .get_mut(label)
            .unwrap()
            .shift = 0;
        let rejected =
            serde_json::from_value::<HourlyReport>(serde_json::to_value(report).unwrap()).unwrap();
        assert!(rejected.is_empty());
    }
}

#[test]
fn byte_rate_histograms_distinguish_one_ten_and_one_hundred_mebibytes_per_second() {
    for measure in [
        Measure::UploadBytesPerSecond,
        Measure::DownloadBytesPerSecond,
    ] {
        let recorder = crate::Recorder::on();
        for rate in [1 << 20, 10 << 20, 100 << 20] {
            recorder.measure_on(1, measure, rate);
        }
        let report = recorder.take().hours.remove(&1).unwrap();
        let (metric, label, _) = measure.key();
        let histogram = &report.histograms[metric][label];
        for bucket in [6, 9, 12] {
            assert_eq!(histogram.buckets[bucket], 1);
        }
        assert_eq!(histogram.buckets[15], 0);
    }
}

#[test]
fn stored_unfinished_span_counters_and_active_ages_are_dropped() {
    for shift in [0, 10] {
        let report: HourlyReport = serde_json::from_value(serde_json::json!({
            "hour": 1,
            "counters": { "sync": { "unfinished": 2 } },
            "histograms": { "syncing_ms": { "active_age": {
                "shift": shift,
                "buckets": vec![0; 16],
                "count": 1,
                "sum": 1794527123456u64,
            } } },
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_value(report).unwrap(),
            serde_json::json!({
                "hour": 1,
            })
        );
    }
}

#[test]
fn the_failure_keys_are_exactly_these() {
    let mut keys: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for failure in Failure::all() {
        let (group, name) = failure.key();
        keys.entry(group).or_default().push(name);
    }
    let transfer = vec![
        "call",
        "source_changed",
        "corrupt",
        "refused",
        "unavailable",
        "no_target",
        "io",
    ];
    let refusal = vec![
        "unauthenticated",
        "forbidden",
        "invalid_argument",
        "missing_chunks",
        "limit_exceeded",
        "quota_exceeded",
        "read_only",
        "retry_later",
        "unsupported_version",
        "internal",
        "history_mismatch",
        "stale_device_state",
        "head_moved",
        "ahead_of_server",
        "not_found",
        "confirmation_required",
        "unknown",
    ];
    let halted = vec![
        "withdrawn",
        "root",
        "replaced",
        "other_root",
        "placement",
        "claim",
        "no_replace",
        "store_behind",
        "disk_behind",
        "record",
        "store",
        "io",
        "session_start",
        "session_closed",
        "credentials_refused",
        "device_revoked",
        "unsupported_version",
        "not_a_member",
        "space_not_found",
        "connection_refused",
        "recovery",
        "refused",
        "protocol",
        "signed_out",
        "start",
        "unlink",
    ];
    assert_eq!(
        keys,
        BTreeMap::from([
            ("halted", halted),
            ("upload_failed", transfer.clone()),
            ("download_failed", transfer),
            ("refused", refusal.clone()),
            (
                "server_cause",
                vec![
                    "exception",
                    "r2_rate_limited",
                    "r2_unavailable",
                    "r2_error",
                    "do_overloaded",
                    "do_unavailable",
                    "cpu_exceeded",
                    "memory_exceeded",
                    "subrequest_limit",
                    "storage_full",
                    "daily_limit",
                    "unknown",
                    "edge_exception",
                    "edge_resource_limit",
                    "edge_daily_requests",
                    "edge_other",
                ],
            ),
            ("problem_node", refusal),
            ("too_large", vec!["commit", "problem_node"]),
            (
                "asked",
                vec![
                    "mass_delete",
                    "remote_mass_delete",
                    "ambiguous_disappearance",
                    "recovery",
                    "conflict",
                ]
            ),
            (
                "daemon_start_failed",
                vec!["busy", "profile", "socket", "signals"]
            ),
        ])
    );
}

#[test]
fn every_command_is_listed_once() {
    let names: std::collections::BTreeSet<_> = Command::ALL.map(Command::name).into();
    assert_eq!(names.len(), Command::ALL.len());
}

#[test]
fn an_adopted_root_carries_only_its_proof_and_the_magnitude_of_its_matches() {
    let event = Event::RootAdopted {
        at: 1_700_000_000,
        proof: RootProof::Children,
        children_magnitude: Some(magnitude(3)),
    };
    let json = serde_json::to_value(&event).unwrap();
    assert_eq!(
        json,
        serde_json::json!({
            "type": "root_adopted",
            "at": 1_700_000_000,
            "proof": "children",
            "children_magnitude": 2,
        })
    );
    assert_eq!(serde_json::from_value::<Event>(json).unwrap(), event);
    assert_eq!(event.kind(), ("root_adopted", "children", ""));
    let volume = Event::RootAdopted {
        at: 1,
        proof: RootProof::Volume,
        children_magnitude: None,
    };
    assert_eq!(
        serde_json::to_value(&volume).unwrap(),
        serde_json::json!({"type": "root_adopted", "at": 1, "proof": "volume"})
    );
}

#[test]
fn magnitudes_are_bit_lengths() {
    assert_eq!(
        [0, 1, 2, 3, 4, 7, 8, 1000].map(magnitude),
        [0, 1, 2, 2, 3, 3, 4, 10]
    );
}

#[test]
fn install_ids_are_32_random_hex_characters() {
    let (a, b) = (InstallId::generate(), InstallId::generate());
    assert_ne!(a, b);
    assert!(InstallId::try_from(a.as_str().to_owned()).is_ok());
    for bad in [
        "",
        "ABCDEF0123456789abcdef0123456789",
        "/home/alice/some/thing/0123456789",
    ] {
        assert!(InstallId::try_from(bad.to_owned()).is_err(), "{bad}");
    }
}

#[test]
fn event_uuids_are_random_version_4_uuids() {
    let (a, b) = (
        EventUuid::generate().unwrap(),
        EventUuid::generate().unwrap(),
    );
    assert_ne!(a, b);
    assert_eq!(&a.as_str()[14..15], "4");
    assert!(EventUuid::try_from(a.as_str().to_owned()).is_ok());
    assert!(EventUuid::try_from("/home/alice/notes.txt/0123456789abcdef".to_owned()).is_err());
}

#[test]
fn versions_refuse_anything_but_a_version_number() {
    for good in ["0.1.0", "0.1.0-dev+abc123", "14"] {
        assert!(Version::parse(good).is_ok(), "{good}");
    }
    for bad in ["", "v0.1", "0.1 beta", "0.1/../x", "1.0\n"] {
        assert!(Version::parse(bad).is_err(), "{bad}");
    }
}

#[test]
fn a_panic_location_outside_the_source_tree_is_external() {
    assert_eq!(
        SourceFile::from_location("core/driver/src/folder.rs").0,
        "core/driver/src/folder.rs"
    );
    for outside in [
        "/Users/alice/.cargo/registry/src/index.crates.io-1/tokio-1.48.0/src/lib.rs",
        "core/../../Users/alice/x.rs",
        "core/Alice Notes.rs",
        "src/main.rs",
    ] {
        assert_eq!(
            SourceFile::from_location(outside).0,
            SourceFile::EXTERNAL,
            "{outside}"
        );
    }
    assert!(SourceFile::try_from("/Users/alice/x.rs".to_owned()).is_err());
}
