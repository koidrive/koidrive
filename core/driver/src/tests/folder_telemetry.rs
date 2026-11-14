// SPDX-License-Identifier: Apache-2.0

use super::*;
use koidrive_telemetry::Recorder;

fn histograms(recorder: &Recorder) -> serde_json::Value {
    let mut all = koidrive_telemetry::HourlyReport::default();
    for report in recorder.take().hours.into_values() {
        all.merge(report);
    }
    let histograms = serde_json::to_value(all).unwrap()["histograms"].clone();
    for metric in ["first_sync_ms", "remote_apply_ms", "syncing_ms"] {
        if let Some(labels) = histograms[metric].as_object() {
            for histogram in labels.values() {
                assert!(histogram.get("sum").is_none());
            }
        }
    }
    histograms
}

#[test]
fn telemetry_folder_hooks_measure_first_sync_transfers_and_successful_apply() {
    let mut remote = Remote::new_at(Time::now().unix_ms - 10_000);
    let (site_a, mut a) = linked(&mut remote, "private-device-a");
    let uploaded = Recorder::on();
    a.folder.timing.enable(uploaded.clone());
    fs::write(site_a.root.join("private-file"), b"verified bytes").unwrap();
    fs::write(site_a.root.join("private-empty"), b"").unwrap();
    fs::write(site_a.root.join("private-deduplicated"), b"verified bytes").unwrap();
    fs::write(site_a.root.join("trashed"), b"gone").unwrap();
    fs::create_dir(site_a.root.join("private-directory")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.rest(&mut remote);
    fs::remove_file(site_a.root.join("trashed")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.rest(&mut remote);
    let sent = histograms(&uploaded);
    assert!(
        sent["upload_per_second"]["files"]["count"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(sent["upload_per_second"]["bytes"]["sum"].as_u64().unwrap() > 0);

    let mut site_b = Site::new(&remote, "private-device-b");
    site_b.link.linked_at_ms = Time::now().unix_ms - 1000;
    Store::open(&site_b.db)
        .unwrap()
        .transaction(|tx| tx.set_link(&site_b.link))
        .unwrap();
    let received = Recorder::on();
    Timing::initialize(&site_b.db, site_b.link.linked_at_ms, &received);
    let mut b = site_b.start();
    b.folder.timing.enable(received.clone());
    b.connect(&remote);
    drop(b);
    assert!(histograms(&received).get("first_sync_ms").is_none());
    let mut b = site_b.start();
    b.folder.timing.enable(received.clone());
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.rest(&mut remote);
    assert_eq!(b.state(), FolderState::UpToDate);
    assert_eq!(
        fs::read(site_b.root.join("private-file")).unwrap(),
        b"verified bytes"
    );
    assert_eq!(fs::read(site_b.root.join("private-empty")).unwrap(), b"");
    let first = histograms(&received);
    assert_eq!(first["first_sync_ms"]["2_3"]["count"], 1);
    assert_eq!(first["download_per_second"]["files"]["count"], 1);
    assert!(
        first["download_per_second"]["bytes"]["sum"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(first.get("remote_apply_ms").is_none());
    drop(b);
    let mut b = site_b.start();
    b.folder.timing.enable(received.clone());
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.rest(&mut remote);
    assert!(histograms(&received).get("first_sync_ms").is_none());

    a.create(
        &mut remote,
        &site_a.root.join("private-file"),
        b"second logical version",
    );
    a.settle();
    b.pull(&remote);
    assert_eq!(b.state(), FolderState::CatchingUp);
    let receipt = histograms(&received);
    assert!(receipt.get("remote_apply_ms").is_none());
    assert_eq!(
        fs::read(site_b.root.join("private-file")).unwrap(),
        b"verified bytes"
    );
    b.rest(&mut remote);
    let applied = histograms(&received);
    assert_eq!(applied["remote_apply_ms"]["effect"]["count"], 1);
    assert_eq!(
        fs::read(site_b.root.join("private-file")).unwrap(),
        b"second logical version"
    );
    let value = applied.to_string();
    for private in [
        site_b.root.to_str().unwrap(),
        "private-file",
        "private-device-b",
        &site_b.link.space_id,
        "1700000000000",
    ] {
        assert!(!value.contains(private), "{value}");
    }
}

#[test]
#[ignore]
fn telemetry_real_folder_release_measurement() {
    for run in 0..5 {
        for on in if run % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        } {
            let mut remote = Remote::new_at(Time::now().unix_ms - 10_000);
            let (site_a, mut a) = linked(&mut remote, "a");
            fs::create_dir(site_a.root.join("files")).unwrap();
            for file in 0..16 {
                fs::write(
                    site_a.root.join(format!("files/{file}")),
                    vec![file as u8; 1024],
                )
                .unwrap();
            }
            a.scan_until(|a| !a.work.is_empty());
            a.rest(&mut remote);
            let mut site_b = Site::new(&remote, "b");
            site_b.link.linked_at_ms = Time::now().unix_ms;
            Store::open(&site_b.db)
                .unwrap()
                .transaction(|tx| tx.set_link(&site_b.link))
                .unwrap();
            let recorder = if on { Recorder::on() } else { Recorder::off() };
            Timing::initialize(&site_b.db, site_b.link.linked_at_ms, &recorder);
            let mut b = site_b.start();
            b.folder.timing.enable(recorder.clone());
            let started = Instant::now();
            b.connect(&remote);
            b.scan_until(|b| b.folder.scanned);
            b.rest(&mut remote);
            let elapsed = started.elapsed();
            assert_eq!(b.state(), FolderState::UpToDate);
            for file in 0..16 {
                assert_eq!(
                    fs::read(site_b.root.join(format!("files/{file}"))).unwrap(),
                    vec![file as u8; 1024]
                );
            }
            let recorded = histograms(&recorder);
            if on {
                assert_eq!(recorded["first_sync_ms"]["16_31"]["count"], 1);
                assert_eq!(recorded["download_per_second"]["files"]["count"], 1);
                assert!(
                    recorded["download_per_second"]["bytes"]["sum"]
                        .as_u64()
                        .unwrap()
                        > 0
                );
            } else {
                assert!(recorded.is_null());
            }
            eprintln!(
                "timing real folder run={run} on={on} files=16 bytes=16384 elapsed_ns={}",
                elapsed.as_nanos()
            );
        }
    }
}

#[test]
fn telemetry_folder_retries_preserve_apply_origin_without_success_on_failure() {
    let mut remote = Remote::new_at(Time::now().unix_ms - 10_000);
    let (site_a, mut a) = linked(&mut remote, "a");
    let (site_b, mut b) = linked(&mut remote, "b");
    let recorder = Recorder::on();
    b.folder.timing.enable(recorder.clone());
    a.create(
        &mut remote,
        &site_a.root.join("incoming"),
        b"remote content",
    );
    b.pull(&remote);
    let stage = b.work.pop_front().expect("the remote file is staged");
    assert!(matches!(stage, Work::Stage(_)));
    fs::write(site_b.root.join("incoming"), b"unobserved obstacle").unwrap();
    b.perform(stage, &mut remote).unwrap();
    assert!(b.folder.retry_at.is_some());
    assert_eq!(b.state(), FolderState::UpToDate);
    let failed = histograms(&recorder);
    assert!(failed.get("remote_apply_ms").is_none());
    assert!(failed.get("download_per_second").is_none());
    assert!(failed.get("syncing_ms").is_none());
    fs::remove_file(site_b.root.join("incoming")).unwrap();
    b.rest(&mut remote);
    assert_eq!(
        fs::read(site_b.root.join("incoming")).unwrap(),
        b"remote content"
    );
    let completed = histograms(&recorder);
    assert_eq!(completed["remote_apply_ms"]["effect"]["count"], 1);
    assert_eq!(completed["download_per_second"]["files"]["count"], 1);
    assert_eq!(completed["syncing_ms"]["completed"]["count"], 1);
}

#[test]
fn first_sync_waits_for_a_scheduled_disk_effect_retry() {
    let mut remote = Remote::new_at(Time::now().unix_ms - 10_000);
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(
        &mut remote,
        &site_a.root.join("incoming"),
        b"remote content",
    );
    let mut site_b = Site::new(&remote, "b");
    site_b.link.linked_at_ms = Time::now().unix_ms - 1000;
    Store::open(&site_b.db)
        .unwrap()
        .transaction(|tx| tx.set_link(&site_b.link))
        .unwrap();
    let recorder = Recorder::on();
    Timing::initialize(&site_b.db, site_b.link.linked_at_ms, &recorder);
    let mut b = site_b.start();
    b.folder.timing.enable(recorder.clone());
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    let mut failed = false;
    while let Some(work) = b.work.pop_front() {
        let stage = matches!(work, Work::Stage(_));
        if stage {
            fs::write(site_b.root.join("incoming"), b"unobserved obstacle").unwrap();
        }
        b.perform(work, &mut remote).unwrap();
        if stage {
            failed = true;
            break;
        }
    }
    assert!(failed);
    assert!(b.folder.retry_at.is_some());
    assert_eq!(b.state(), FolderState::UpToDate);
    assert!(b.folder.timing.first_pending());
    assert!(site_b.db.with_file_name("first-sync").is_file());
    let failed = histograms(&recorder);
    assert!(failed.get("first_sync_ms").is_none());
    assert!(failed.get("syncing_ms").is_none());
    fs::remove_file(site_b.root.join("incoming")).unwrap();
    b.rest(&mut remote);
    assert_eq!(
        fs::read(site_b.root.join("incoming")).unwrap(),
        b"remote content"
    );
    assert!(!b.folder.timing.has_first());
    assert!(!site_b.db.with_file_name("first-sync").exists());
    assert_eq!(histograms(&recorder)["first_sync_ms"]["1"]["count"], 1);
}

#[test]
fn resent_commits_without_a_burst_do_not_inflate_later_uploads() {
    let mut remote = Remote::new_at(Time::now().unix_ms - 10_000);
    let (site, mut a) = linked(&mut remote, "a");
    for file in 0..20 {
        fs::write(
            site.root.join(format!("first-{file}")),
            vec![file as u8; 1000],
        )
        .unwrap();
    }
    a.scan_until(|a| !a.work.is_empty());
    let upload = a.work.pop_front().unwrap();
    a.perform(upload, &mut remote).unwrap();
    assert!(matches!(a.work.pop_front(), Some(Work::Send(_))));
    drop(a);
    let recorder = Recorder::on();
    let mut a = site.start();
    a.folder.timing.enable(recorder.clone());
    a.connect(&remote);
    assert!(matches!(a.work.front(), Some(Work::Send(_))));
    a.scan_until(|a| a.folder.scanned);
    a.rest(&mut remote);
    assert_eq!(a.state(), FolderState::UpToDate);
    assert_eq!(remote.names().len(), 20);
    assert!(histograms(&recorder).get("upload_per_second").is_none());
    for file in 0..20 {
        fs::write(
            site.root.join(format!("later-{file}")),
            vec![100 + file as u8; 1000],
        )
        .unwrap();
    }
    a.scan_until(|a| !a.work.is_empty());
    a.rest(&mut remote);
    assert_eq!(a.state(), FolderState::UpToDate);
    assert_eq!(remote.names().len(), 40);
    let sent = histograms(&recorder);
    let files = sent["upload_per_second"]["files"]["sum"].as_u64().unwrap() as f64;
    let bytes = sent["upload_per_second"]["bytes"]["sum"].as_u64().unwrap() as f64;
    assert!(files > 0.0);
    let counted = 20_000.0 * files / bytes;
    assert!(
        (counted - 20.0).abs() < 2.0,
        "counted {counted} files for a 20-file burst"
    );
}

#[test]
fn every_effect_of_a_batch_records_its_remote_apply() {
    let mut remote = Remote::new_at(Time::now().unix_ms - 10_000);
    let (site_a, mut a) = linked(&mut remote, "a");
    let (site_b, mut b) = linked(&mut remote, "b");
    let recorder = Recorder::on();
    b.folder.timing.enable(recorder.clone());
    for name in ["first", "second", "third"] {
        a.create(&mut remote, &site_a.root.join(name), name.as_bytes());
    }
    b.pull_at_once(&remote);
    assert!(matches!(b.work.front(), Some(Work::Stage(_))));
    assert_eq!(b.folder.engine.effects.in_flight().len(), 3);
    b.rest(&mut remote);
    for name in ["first", "second", "third"] {
        assert_eq!(fs::read(site_b.root.join(name)).unwrap(), name.as_bytes());
    }
    let applied = histograms(&recorder);
    assert_eq!(applied["remote_apply_ms"]["effect"]["count"], 3);
    assert_eq!(applied["download_per_second"]["files"]["count"], 1);
}
