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
