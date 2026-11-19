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
