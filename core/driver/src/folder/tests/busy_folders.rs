// SPDX-License-Identifier: Apache-2.0

fn busy_logs(remote: &mut Remote, sync: &SyncRules) -> (Site, Device) {
    let (site, mut b) = linked(remote, "b");
    fs::create_dir(site.root.join("logs")).unwrap();
    fs::write(site.root.join("logs/app.log"), b"started\n").unwrap();
    fs::write(site.root.join("report.docx"), b"report").unwrap();
    b.settle();
    b.rest(remote);
    drop(b);
    let b = started(&site, remote, sync);
    (site, b)
}

fn until(b: &mut Device, remote: &mut Remote, done: impl Fn(&Device, &Remote) -> bool) -> bool {
    (0..100).any(|_| {
        b.scan(Duration::from_millis(20));
        b.run(remote);
        done(b, remote)
    })
}

#[test]
fn a_rules_file_downloaded_into_a_busy_folder_holds_no_download_elsewhere() {
    let mut remote = Remote::new();
    let (site, mut b) = busy_logs(&mut remote, &SyncRules::default());
    let (site_a, mut a) = linked(&mut remote, "a");
    a.rest(&mut remote);
    let (site_c, mut c) = linked(&mut remote, "c");
    c.rest(&mut remote);
    b.observer.keep_pending(PathBuf::from("logs"));

    a.create(
        &mut remote,
        &site_a.root.join("logs/.koiignore"),
        b"*.tmp\n",
    );
    b.pull(&remote);
    b.run(&mut remote);
    assert_eq!(
        fs::read(site.root.join("logs/.koiignore")).unwrap(),
        b"*.tmp\n"
    );
    fs::write(site_c.root.join("logs/cache.tmp"), b"cache").unwrap();
    fs::write(site_c.root.join("notes.txt"), b"notes").unwrap();
    c.rest(&mut remote);
    b.pull(&remote);
    let arrived = until(&mut b, &mut remote, |_, _| {
        site.root.join("notes.txt").exists()
    });
    assert!(arrived, "notes.txt is downloaded while logs stays busy");
    assert!(
        !site.root.join("logs/cache.tmp").exists(),
        "what the rules file in logs leaves out stays out while no scan has read it"
    );
}

#[test]
fn a_file_that_changed_while_it_was_uploaded_in_a_busy_folder_holds_no_upload_elsewhere() {
    let mut remote = Remote::new();
    let (site, mut b) = busy_logs(&mut remote, &SyncRules::default());
    fs::write(site.root.join("logs/app.log"), b"started\nbuilding\n").unwrap();
    b.scan_until(|b| !b.work.is_empty());
    let Some(Work::Upload(upload)) = b.work.pop_front() else {
        panic!("the content is uploaded first");
    };
    fs::write(
        site.root.join("logs/app.log"),
        b"started\nbuilding\nlinking\n",
    )
    .unwrap();
    b.observer.keep_pending(PathBuf::from("logs"));
    let step = (b.folder).uploaded(Err(ChunkError::SourceChanged {
        hash: upload.chunks[0].hash.clone(),
    }));
    b.take(step).unwrap();

    fs::write(site.root.join("report.docx"), b"report, edited").unwrap();
    let edited = |_: &Device, remote: &Remote| {
        (remote.tree().get("report.docx")).is_some_and(|id| server_size(remote, id) == 14)
    };
    assert!(
        until(&mut b, &mut remote, edited),
        "the edit outside logs is uploaded while logs stays busy"
    );
}

#[test]
fn a_synced_file_deleted_inside_a_left_out_folder_comes_back_while_another_folder_stays_busy() {
    let mut remote = Remote::new();
    let (site, mut b) = busy_logs(&mut remote, &dist_left_out());
    fs::create_dir(site.root.join("dist")).unwrap();
    fs::rename(
        site.root.join("report.docx"),
        site.root.join("dist/report.docx"),
    )
    .unwrap();
    b.settle();
    b.rest(&mut remote);
    b.observer.keep_pending(PathBuf::from("logs"));

    fs::remove_file(site.root.join("dist/report.docx")).unwrap();
    let back = until(&mut b, &mut remote, |_, _| {
        site.root.join("report.docx").exists()
    });
    let sent: Vec<&str> = b.sent.iter().flat_map(kinds).collect();
    assert!(!sent.contains(&"delete"), "{sent:?}");
    assert!(back, "report.docx comes back while logs stays busy");
}

#[test]
fn a_dropped_batch_waits_only_for_the_folder_that_stays_busy() {
    let mut remote = Remote::new();
    let (site, mut b) = busy_logs(&mut remote, &dist_left_out());
    fs::create_dir(site.root.join("dist")).unwrap();
    fs::rename(
        site.root.join("report.docx"),
        site.root.join("dist/report.docx"),
    )
    .unwrap();
    b.settle();
    b.rest(&mut remote);
    b.observer.keep_pending(PathBuf::from("logs"));
    let sections = b.folder.sections();
    let batch = (0..100)
        .find_map(|_| (sections.observe(&mut b.observer, Duration::from_millis(20))).unwrap())
        .expect("the batch that finds logs busy");
    b.folder.disk.forget_sections();
    let entries = b.observer.snapshot().entries.len() as u64;
    let step = b.folder.observed(batch, entries);
    b.take(step).unwrap();

    fs::remove_file(site.root.join("dist/report.docx")).unwrap();
    let back = until(&mut b, &mut remote, |_, _| {
        site.root.join("report.docx").exists()
    });
    assert!(back, "report.docx comes back while logs stays busy");
}

#[test]
fn a_synced_file_deleted_inside_a_left_out_folder_comes_back_past_a_locked_file_and_a_socket_in_a_busy_folder()
 {
    let mut remote = Remote::new();
    let (site, mut b) = busy_logs(&mut remote, &dist_left_out());
    fs::create_dir(site.root.join("dist")).unwrap();
    fs::rename(
        site.root.join("report.docx"),
        site.root.join("dist/report.docx"),
    )
    .unwrap();
    b.settle();
    b.rest(&mut remote);
    use std::os::unix::fs::PermissionsExt;
    let locked = site.root.join("logs/locked.log");
    fs::write(&locked, b"locked").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let _socket = std::os::unix::net::UnixListener::bind(site.root.join("logs/dev.sock")).unwrap();
    b.observer.keep_pending(PathBuf::from("logs"));

    fs::remove_file(site.root.join("dist/report.docx")).unwrap();
    let back = until(&mut b, &mut remote, |_, _| {
        site.root.join("report.docx").exists()
    });
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(back, "report.docx comes back while logs stays busy");
}
