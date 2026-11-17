// SPDX-License-Identifier: Apache-2.0

const MIB: usize = 1 << 20;

fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let mut bytes = Vec::with_capacity(len + 8);
    while bytes.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        bytes.extend_from_slice(&state.to_le_bytes());
    }
    bytes.truncate(len);
    bytes
}

fn progress(device: &Device) -> Option<crate::Progress> {
    device.folder.progress.lock().unwrap().progress()
}

fn counts(device: &Device) -> Option<(u64, u64, u64, u64)> {
    progress(device).map(|shown| {
        (
            shown.files_to_upload,
            shown.files_to_download,
            shown.bytes_done,
            shown.bytes_total,
        )
    })
}

fn next_upload(device: &mut Device) -> Upload {
    match device.work.pop_front() {
        Some(Work::Upload(upload)) => upload,
        other => panic!("an upload, not {:?}", other.map(|_| "other work")),
    }
}

fn store_first_chunk(remote: &mut Remote, caller: &Caller, upload: &Upload) -> u64 {
    let chunk = upload.chunks[0].clone();
    let request = PrepareUpload {
        chunks: vec![chunk.clone()],
    };
    remote.space.prepare_upload(caller, remote.now_ms, &request).unwrap();
    let bytes = upload.read(&chunk).unwrap();
    remote.chunks.chunks.insert(chunk.hash.clone(), bytes.to_vec());
    remote.space.put_chunk(&chunk.hash, chunk.size, remote.now_ms);
    u64::from(chunk.size)
}

#[test]
fn an_upload_counts_its_files_until_the_server_holds_them() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    assert_eq!(progress(&a), None);
    let big = noise(1, 20 * MIB);
    fs::write(site.root.join("big.bin"), &big).unwrap();
    fs::write(site.root.join("small.txt"), b"small").unwrap();
    let total = big.len() as u64 + 5;
    a.scan_until(|a| !a.work.is_empty());
    assert_eq!(counts(&a), Some((2, 0, 0, total)));

    let upload = next_upload(&mut a);
    assert!(upload.chunks.len() > 2, "{} chunks", upload.chunks.len());
    a.perform(Work::Upload(upload), &mut remote).unwrap();
    let shown = progress(&a).unwrap();
    assert_eq!(
        (shown.files_to_upload, shown.bytes_done, shown.bytes_total),
        (2, total, total)
    );
    assert!(
        matches!(shown.current.as_deref(), Some("big.bin" | "small.txt")),
        "{shown:?}"
    );
    assert_eq!(a.run(&mut remote), ["send"]);
    a.settle();
    assert_eq!(a.state(), FolderState::UpToDate);
    assert_eq!(progress(&a), None);
}

#[test]
fn a_download_counts_its_files_until_they_are_on_disk() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    let (_site_b, mut b) = linked(&mut remote, "b");
    let big = noise(2, 9 * MIB);
    fs::write(site_a.root.join("big.bin"), &big).unwrap();
    fs::write(site_a.root.join("small.txt"), b"small").unwrap();
    let total = big.len() as u64 + 5;
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    a.settle();
    assert_eq!(remote.names(), ["big.bin", "small.txt"]);

    b.pull(&remote);
    assert_eq!(counts(&b), Some((0, 2, 0, total)));
    let Some(Work::Stage(pending)) = b.work.pop_front() else {
        panic!("the files are fetched");
    };
    let staged = ready(pending.stage(&remote.chunks));
    assert_eq!(counts(&b), Some((0, 2, total, total)));
    assert!(
        matches!(progress(&b).unwrap().current.as_deref(), Some("big.bin" | "small.txt")),
        "{:?}",
        progress(&b)
    );
    let step = b.folder.staged(pending, staged);
    b.take(step).unwrap();
    b.rest(&mut remote);
    assert_eq!(fs::read(_site_b.root.join("big.bin")).unwrap(), big);
    assert_eq!(b.state(), FolderState::UpToDate);
    assert_eq!(progress(&b), None);
}

#[test]
fn a_download_that_starts_before_a_recount_counts_its_files_and_the_bytes_it_fetches() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    let (_site_b, mut b) = linked(&mut remote, "b");
    let big = noise(4, 9 * MIB);
    fs::write(site_a.root.join("big.bin"), &big).unwrap();
    fs::write(site_a.root.join("small.txt"), b"small").unwrap();
    let total = big.len() as u64 + 5;
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    a.settle();

    b.pull(&remote);
    let Some(Work::Stage(pending)) = b.work.pop_front() else {
        panic!("the files are fetched");
    };
    b.folder.progress.lock().unwrap().clear();
    assert_eq!(progress(&b), None);
    ready(pending.stage(&remote.chunks)).unwrap();
    assert_eq!(counts(&b), Some((0, 2, total, total)));
}

#[test]
fn chunks_the_server_holds_already_count_as_done_without_being_sent() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let big = noise(3, 12 * MIB);
    a.create(&mut remote, &site.root.join("big.bin"), &big);
    a.settle();
    let stored = remote.chunks.chunks.len();

    fs::write(site.root.join("copy.bin"), &big).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let size = big.len() as u64;
    assert_eq!(counts(&a), Some((1, 0, 0, size)));
    let upload = next_upload(&mut a);
    a.perform(Work::Upload(upload), &mut remote).unwrap();
    assert_eq!(remote.chunks.chunks.len(), stored, "no chunk is sent again");
    assert_eq!(counts(&a), Some((1, 0, size, size)));
    assert_eq!(progress(&a).unwrap().current.as_deref(), Some("copy.bin"));
    assert_eq!(a.run(&mut remote), ["send"]);
    a.settle();
    assert_eq!(progress(&a), None);
}

#[test]
fn an_upload_resumed_after_a_restart_counts_the_chunks_the_server_took_before() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let big = noise(4, 20 * MIB);
    let size = big.len() as u64;
    fs::write(site.root.join("big.bin"), &big).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let upload = next_upload(&mut a);
    let first = store_first_chunk(&mut remote, &a.caller, &upload);
    drop(upload);
    drop(a);

    let mut a = site.start();
    assert_eq!(progress(&a), None);
    a.connect(&remote);
    a.scan_until(|a| a.folder.scanned);
    assert_eq!(counts(&a), Some((1, 0, 0, size)));
    let stored = remote.chunks.chunks.len();
    let mut uploads = 0;
    while let Some(work) = a.work.pop_front() {
        let upload = matches!(work, Work::Upload(_));
        a.perform(work, &mut remote).unwrap();
        if upload {
            uploads += 1;
            assert_eq!(counts(&a), Some((1, 0, size, size)));
        }
    }
    assert_eq!(uploads, 1);
    a.settle();
    let chunks = (a.sent.iter())
        .flat_map(|commit| &commit.ops)
        .filter_map(|op| match &op.op {
            Some(op::Op::CreateFile(create)) => create.content.as_ref(),
            _ => None,
        })
        .map(|content| content.chunks.len())
        .max()
        .unwrap();
    assert_eq!(remote.chunks.chunks.len(), stored + chunks - 1);
    assert!(first < size);
    assert_eq!(remote.names(), ["big.bin"]);
    assert_eq!(progress(&a), None);
}

#[test]
fn a_file_edited_while_it_is_uploaded_counts_its_new_content_after_the_bytes_done() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let path = site.root.join("big.bin");
    let first = noise(5, 20 * MIB);
    fs::write(&path, &first).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let upload = next_upload(&mut a);
    let stored = store_first_chunk(&mut remote, &a.caller, &upload);
    upload.credit.chunk(&upload.chunks[0].hash);
    assert_eq!(counts(&a), Some((1, 0, stored, first.len() as u64)));

    let second = noise(6, 8 * MIB);
    fs::write(&path, &second).unwrap();
    let changed = (upload.chunks.iter())
        .find(|chunk| !koidrive_net::matches(chunk, &upload.read(chunk).unwrap()))
        .unwrap()
        .hash
        .clone();
    let step = a.folder.uploaded(Err(ChunkError::SourceChanged { hash: changed }));
    a.take(step).unwrap();
    let size = second.len() as u64;
    a.scan_until(|a| counts(a).is_some_and(|(.., total)| total == stored + size));
    assert_eq!(counts(&a), Some((1, 0, stored, stored + size)));
    assert_eq!(a.run(&mut remote), ["wait", "upload", "send"]);
    a.settle();
    assert_eq!(progress(&a), None);
    assert_eq!(fs::read(&path).unwrap(), second);
}

#[test]
fn an_upload_cut_off_part_way_and_sent_again_counts_no_byte_twice() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let big = noise(7, 20 * MIB);
    let size = big.len() as u64;
    fs::write(site.root.join("big.bin"), &big).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let upload = next_upload(&mut a);
    let stored = store_first_chunk(&mut remote, &a.caller, &upload);
    upload.credit.chunk(&upload.chunks[0].hash);
    let cut = ChunkError::Unavailable {
        hash: upload.chunks[1].hash.clone(),
        message: "the connection was reset".into(),
        cause: None,
    };
    let step = a.folder.uploaded(Err(cut));
    a.take(step).unwrap();
    assert_eq!(counts(&a), Some((1, 0, stored, size)));

    assert_eq!(a.work.pop_front().map(|work| matches!(work, Work::ResendAfter(_))), Some(true));
    let step = a.folder.resend();
    a.take(step).unwrap();
    let upload = next_upload(&mut a);
    a.perform(Work::Upload(upload), &mut remote).unwrap();
    assert_eq!(counts(&a), Some((1, 0, size, size)));
    assert_eq!(a.run(&mut remote), ["send"]);
    a.settle();
    assert_eq!(progress(&a), None);
}

#[test]
fn a_file_found_while_others_sync_adds_its_bytes_at_once() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let big = noise(8, 20 * MIB);
    let size = big.len() as u64;
    fs::write(site.root.join("big.bin"), &big).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let upload = next_upload(&mut a);
    let stored = store_first_chunk(&mut remote, &a.caller, &upload);
    upload.credit.chunk(&upload.chunks[0].hash);
    assert_eq!(counts(&a), Some((1, 0, stored, size)));

    fs::write(site.root.join("late.txt"), b"found later").unwrap();
    a.scan_until(|a| counts(a).is_some_and(|(files, ..)| files == 2));
    assert_eq!(counts(&a), Some((2, 0, stored, size + 11)));
    a.perform(Work::Upload(upload), &mut remote).unwrap();
    assert_eq!(counts(&a), Some((2, 0, size, size + 11)));
    let Some(Work::Send(commit)) = a.work.pop_front() else {
        panic!("the commit is sent after its upload");
    };
    a.perform(Work::Send(commit), &mut remote).unwrap();
    assert_eq!(counts(&a), Some((1, 0, size, size + 11)));
    a.rest(&mut remote);
    assert_eq!(remote.names(), ["big.bin", "late.txt"]);
    assert_eq!(progress(&a), None);
}

#[test]
fn an_offline_folder_keeps_its_progress_until_it_connects_again() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let big = noise(9, 20 * MIB);
    let size = big.len() as u64;
    fs::write(site.root.join("big.bin"), &big).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let upload = next_upload(&mut a);
    let stored = store_first_chunk(&mut remote, &a.caller, &upload);
    upload.credit.chunk(&upload.chunks[0].hash);
    assert_eq!(counts(&a), Some((1, 0, stored, size)));
    for state in [FolderState::Offline, FolderState::Connecting, FolderState::CatchingUp] {
        a.folder.track_progress(state);
        assert_eq!(counts(&a), Some((1, 0, stored, size)), "{state:?}");
    }
}

#[test]
fn a_recount_right_after_another_waits_for_a_wake_and_an_end_of_sync_clears_at_once() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::write(site.root.join("one.txt"), b"one").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    assert_eq!(counts(&a), Some((1, 0, 0, 3)));

    a.folder.recount_progress(FolderState::CatchingUp);
    assert!(a.folder.recount_at.is_some());
    assert_eq!(a.folder.next_change(), None);
    a.folder.recount_at = Some(Instant::now() + Duration::from_secs(60));
    assert!(!a.folder.recount_progress(FolderState::CatchingUp));
    let wake = a.folder.next_change().expect("the folder wakes for the recount it put off");
    assert!(wake > Duration::from_secs(59) && wake <= Duration::from_secs(60), "{wake:?}");
    a.folder.recount_at = Some(Instant::now());
    a.folder.recount_progress(FolderState::CatchingUp);
    assert_eq!(a.folder.next_change(), None);

    assert!(a.folder.recount_progress(FolderState::UpToDate));
    assert_eq!(progress(&a), None);
}

fn activity(device: &Device) -> Option<Activity> {
    let observing = device.reports.lock().unwrap().last().copied();
    Activity::live(device.folder.activity(device.state()), observing)
}

#[test]
fn a_new_folder_says_it_scans_and_counts_the_files_its_first_scan_finds() {
    let mut remote = Remote::new();
    let site = Site::new(&remote, "a");
    fs::create_dir(site.root.join("docs")).unwrap();
    for name in ["one.txt", "docs/two.txt", "docs/three.txt"] {
        fs::write(site.root.join(name), name).unwrap();
    }
    let mut a = site.start();
    a.connect(&remote);
    assert_eq!(a.state(), FolderState::Bootstrapping);
    assert_eq!(activity(&a), Some(Activity::Scanning { files: 0 }));

    let sections = a.folder.sections();
    let batch = sections.observe(&mut a.observer, WAIT).unwrap().unwrap();
    let walked = (a.reports.lock().unwrap().iter())
        .filter_map(|report| match report {
            Observing::Walking { files } => Some(*files),
            Observing::Reading { .. } => None,
        })
        .max();
    assert_eq!(walked, Some(3));
    assert_eq!(activity(&a), Some(Activity::Checking { files: 0, bytes: 0 }));
    let step = a.folder.observed(batch, 4);
    a.take(step).unwrap();
    let step = a.folder.scanned();
    a.take(step).unwrap();
    assert_eq!(activity(&a), Some(Activity::Listing));

    a.rest(&mut remote);
    assert_eq!(a.state(), FolderState::UpToDate);
    assert_eq!(activity(&a), None);
}

#[test]
fn files_left_to_read_show_as_checking_beside_the_uploads() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let many = crate::observe::READ_FILES + 20;
    for file in 0..many {
        fs::write(site.root.join(format!("{file}.txt")), file.to_string()).unwrap();
    }
    a.scan_until(|a| a.folder.unread.0 > 0);
    assert_eq!(a.state(), FolderState::CatchingUp);
    let Some(Activity::Checking { files, bytes }) = activity(&a) else {
        panic!("{:?}", activity(&a));
    };
    assert!(files > 0 && files < many as u64, "{files}");
    assert!(bytes >= files, "{files} {bytes}");
    assert!(counts(&a).is_some_and(|(up, ..)| up > 0));

    a.scan_until(|a| a.folder.unread.0 == 0);
    assert_eq!(counts(&a).map(|(up, ..)| up), Some(many as u64));
    assert_eq!(activity(&a), None);
}

#[test]
fn changes_without_content_show_as_changes_to_apply() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::create_dir(site.root.join("Projects")).unwrap();
    fs::create_dir(site.root.join("Drafts")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    assert_eq!(a.state(), FolderState::CatchingUp);
    assert_eq!(progress(&a), None);
    assert_eq!(activity(&a), Some(Activity::Applying { changes: 2 }));
    a.rest(&mut remote);
    assert_eq!(activity(&a), None);
}

#[test]
fn a_folder_that_does_not_sync_reports_no_activity() {
    let remote = Remote::new();
    let site = Site::new(&remote, "a");
    let a = site.start();
    assert_eq!(a.state(), FolderState::Connecting);
    assert_eq!(activity(&a), None);
    for state in [FolderState::Offline, FolderState::UpToDate, FolderState::Halted, FolderState::Stopped] {
        assert_eq!(a.folder.activity(state), None, "{state:?}");
    }
}

fn moved(device: &Device) -> u64 {
    device.folder.progress.lock().unwrap().moved()
}

fn upload_through(upload: Upload, held: Vec<Vec<u8>>) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let (relay, _) = crate::scripted::relay(1).await;
        let (session, _events, api) = crate::scripted::session_holding(relay, held).await;
        let transfer = Transfer::new(&api);
        upload_counted(&session, &transfer, upload, |_| {}).await.unwrap();
    });
}

#[test]
fn a_deduplicated_upload_moves_no_bytes() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::write(site.root.join("held.bin"), noise(3, MIB)).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let upload = next_upload(&mut a);
    let held = upload.chunks.iter().map(|chunk| chunk.hash.clone()).collect();
    upload_through(upload, held);
    assert_eq!(moved(&a), 0);
    assert_eq!(counts(&a).map(|(.., done, total)| done == total), Some(true));
}

#[test]
fn an_upload_moves_the_bytes_of_the_chunks_it_sends_and_only_those() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    for seed in 0..4 {
        fs::write(site.root.join(format!("{seed}.bin")), noise(seed + 10, 64 << 10)).unwrap();
    }
    a.scan_until(|a| !a.work.is_empty());
    let upload = next_upload(&mut a);
    let distinct: BTreeMap<_, _> = (upload.chunks.iter())
        .map(|chunk| (chunk.hash.clone(), u64::from(chunk.size)))
        .collect();
    assert!(distinct.len() > 2, "{} chunks", distinct.len());
    let held: Vec<Vec<u8>> = distinct.keys().step_by(2).cloned().collect();
    let sent: u64 = (distinct.iter())
        .filter(|(hash, _)| !held.contains(hash))
        .map(|(_, size)| size)
        .sum();
    upload_through(upload, held);
    assert_eq!(moved(&a), sent);
}

#[test]
fn downloads_move_their_bytes_and_the_count_only_grows() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    let (_site_b, mut b) = linked(&mut remote, "b");
    let mut seen = moved(&b);
    for (seed, size) in [(6, MIB), (7, 2 * MIB), (8, 3 * MIB)] {
        let name = format!("{seed}.bin");
        a.create(&mut remote, &site_a.root.join(&name), &noise(seed, size));
        a.settle();
        b.pull(&remote);
        b.rest(&mut remote);
        let now = moved(&b);
        assert_eq!(now - seen, size as u64, "{name}");
        seen = now;
    }
    assert_eq!(moved(&a), 0, "the test server stores uploads without the network");
}

#[test]
fn a_large_first_file_shows_its_bytes_fall_once_the_walk_is_done() {
    let remote = Remote::new();
    let site = Site::new(&remote, "a");
    let size = 48 * MIB;
    fs::write(site.root.join("a-video.mov"), noise(7, size)).unwrap();
    for i in 0..20 {
        fs::write(site.root.join(format!("note{i}.txt")), i.to_string()).unwrap();
    }
    let mut a = site.start();
    a.connect(&remote);
    let sections = a.folder.sections();
    let Device {
        folder,
        observer,
        reports,
        ..
    } = &mut a;
    let mut shown = Vec::new();
    let batch = std::thread::scope(|s| {
        let observe = s.spawn(|| sections.observe(observer, WAIT));
        while !observe.is_finished() {
            let observing = reports.lock().unwrap().last().copied();
            let now = Activity::live(folder.activity(FolderState::Bootstrapping), observing);
            if shown.last() != Some(&now) {
                shown.push(now);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        observe.join().unwrap().unwrap().unwrap()
    });
    let reading: Vec<u64> = (shown.iter())
        .filter_map(|shown| match shown {
            Some(Activity::Checking { bytes, .. }) => Some(*bytes),
            _ => None,
        })
        .collect();
    assert!(reading.len() > 2, "{shown:?}");
    assert!(reading.windows(2).all(|pair| pair[1] < pair[0]), "{shown:?}");
    assert!(reading[0] > size as u64 / 2, "{shown:?}");
    let first_read = shown.iter().position(|shown| matches!(shown, Some(Activity::Checking { .. })));
    assert!(
        shown[first_read.unwrap()..]
            .iter()
            .all(|shown| !matches!(shown, Some(Activity::Scanning { .. }))),
        "{shown:?}"
    );
    assert_eq!(
        reports.lock().unwrap().last(),
        Some(&Observing::Reading { files: 0, bytes: 0 })
    );
    let step = a.folder.observed(batch, 22);
    a.take(step).unwrap();
}

#[test]
fn a_folder_whose_removals_wait_for_the_person_says_so() {
    let mut remote = Remote::new();
    let (_site, mut a) = with_a_held_folder(&mut remote);
    a.folder.track_progress(a.state());
    assert_eq!(a.state(), FolderState::CatchingUp);
    assert_eq!(counts(&a), None);
    assert_eq!(activity(&a), Some(Activity::AwaitingAnswers { questions: 1 }));
}

#[test]
fn the_shown_numbers_change_at_most_four_times_a_second_except_when_the_walk_ends() {
    let observing = Arc::default();
    let (progressed, mut marks) = watch::channel(());
    let walked = Arc::new(AtomicBool::new(false));
    let report = crate::folder::reporter(Arc::clone(&observing), Arc::new(progressed), walked.clone());
    let mut marked = || marks.has_changed().unwrap() && { marks.mark_unchanged(); true };
    report(Observing::Walking { files: 1 });
    assert!(marked(), "the first count shows at once");
    for files in 2..1_000 {
        report(Observing::Walking { files });
    }
    assert!(!marked());
    assert_eq!(*observing.lock().unwrap(), Some(Observing::Walking { files: 999 }));
    report(Observing::Walking { files: 10 });
    assert_eq!(*observing.lock().unwrap(), Some(Observing::Walking { files: 999 }), "a walk's count never falls");
    report(Observing::Reading { files: 3, bytes: 300 });
    assert!(marked(), "the end of the walk shows at once");
    report(Observing::Reading { files: 2, bytes: 200 });
    assert!(!marked());
    std::thread::sleep(crate::driver::PROGRESS_INTERVAL);
    report(Observing::Reading { files: 1, bytes: 100 });
    assert!(marked(), "a later number shows once the interval has passed");

    walked.store(true, Ordering::Relaxed);
    std::thread::sleep(crate::driver::PROGRESS_INTERVAL);
    for files in 1..100 {
        report(Observing::Walking { files });
    }
    assert!(!marked(), "the rescans of a scanned folder stay quiet");
}
