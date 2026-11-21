// SPDX-License-Identifier: Apache-2.0

fn put_back_across_an_s_only_pull(commit: bool) -> (Remote, Site, &'static str) {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let (other, mut b) = linked(&mut remote, "b");
    fs::write(other.root.join("f.txt"), b"A").unwrap();
    sync(&mut remote, (&site, &mut a), (&other, &mut b));
    let copy = site.db.with_extension("copy");
    copy_store_while_open(&site.db, &copy);

    fs::write(other.root.join("f.txt"), b"B").unwrap();
    b.rescan();
    b.rest(&mut remote);
    let marker = || koidrive_fs::read_marker(&site.root).unwrap();
    let (sent, before) = (a.sent.len(), marker());
    a.connect(&remote);
    fs::write(site.root.join("f.txt"), b"B").unwrap();
    a.rescan();
    a.settle();
    a.pull(&remote);
    a.rest(&mut remote);
    assert_eq!((a.sent.len(), marker()), (sent, before), "{commit}");

    let then = match commit {
        true => {
            a.create(&mut remote, &site.root.join("h.txt"), b"h");
            "h.txt"
        }
        false => {
            b.create(&mut remote, &other.root.join("g.txt"), b"g");
            a.pull(&remote);
            a.rest(&mut remote);
            "g.txt"
        }
    };
    assert_eq!(names(&site.root), ["f.txt", then], "{commit}");
    copy_store_while_open(&copy, &site.db);

    fs::remove_file(other.root.join("f.txt")).unwrap();
    b.rescan();
    b.rest(&mut remote);
    let records = remote.space.resume(&a.since()).unwrap();
    let stop = a.folder.event(Event::Changes(records)).err();
    assert!(
        matches!(&stop, Some(stop @ Stop::Store(koidrive_store::Error::Replaced)) if stop.remedy() == crate::status::Remedy::Root),
        "{commit}: {stop:?}"
    );
    drop(a);
    (remote, site, then)
}

fn recovered(site: &Site, remote: &mut Remote, commit: bool) -> Device {
    let mut a = site.start();
    let halted = a.folder.engine.commits.halted();
    assert!(
        matches!(halted, Some(CommitHalt::Recovery { .. })),
        "{commit}"
    );
    a.connect(remote);
    a.settle();
    a.rest(remote);
    a
}

fn trashed_here(site: &Site) -> Vec<(String, String)> {
    files(&(site.root.join(koidrive_fs::PRIVATE_DIR)).join(koidrive_fs::TRASH_DIR))
}

#[test]
fn a_store_put_back_while_the_folder_runs_across_an_s_only_pull_brings_nothing_back() {
    for commit in [false, true] {
        let (mut remote, site, then) = put_back_across_an_s_only_pull(commit);
        let a = recovered(&site, &mut remote, commit);
        assert!(a.sent.is_empty(), "{commit}: {:?}", a.sent);
        assert_eq!(a.folder.view().questions, 0, "{commit}");
        assert_eq!(remote.names(), [then], "{commit}");
        assert_eq!(names(&site.root), [then], "{commit}");
        let trashed = trashed_here(&site);
        assert!(
            trashed.iter().any(|(_, text)| text == "B"),
            "{commit}: {trashed:?}"
        );
    }
}

#[test]
fn a_store_put_back_across_an_s_only_pull_asks_about_bytes_the_server_never_had() {
    for commit in [false, true] {
        let (mut remote, site, then) = put_back_across_an_s_only_pull(commit);
        fs::write(site.root.join("f.txt"), b"C").unwrap();
        let a = recovered(&site, &mut remote, commit);
        assert!(a.sent.is_empty(), "{commit}: {:?}", a.sent);
        let [question] = &shown(&site)[..] else {
            panic!("{commit}: one question");
        };
        let held: Vec<_> = (question.items.iter())
            .map(|item| item.path.unquoted())
            .collect();
        assert_eq!(held, ["f.txt"], "{commit}");
        drop(a);

        answer(&site, Answer::Discard);
        let mut a = restarted(&site, &mut remote);
        a.rest(&mut remote);
        assert_eq!(a.folder.view().questions, 0, "{commit}");
        assert_eq!(remote.names(), [then], "{commit}");
        assert_eq!(names(&site.root), [then], "{commit}");
        let trashed = trashed_here(&site);
        assert!(
            trashed.iter().any(|(_, text)| text == "C"),
            "{commit}: {trashed:?}"
        );
    }
}

fn s_only_pull_after_a_copy() -> (Remote, Site, Device, Site, Device, PathBuf) {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let (other, mut b) = linked(&mut remote, "b");
    fs::write(other.root.join("f.txt"), b"A").unwrap();
    sync(&mut remote, (&site, &mut a), (&other, &mut b));
    let copy = site.db.with_extension("copy");
    copy_store_while_open(&site.db, &copy);
    fs::write(other.root.join("f.txt"), b"B").unwrap();
    b.rescan();
    b.rest(&mut remote);
    a.connect(&remote);
    fs::write(site.root.join("f.txt"), b"B").unwrap();
    a.rescan();
    a.settle();
    a.pull(&remote);
    a.rest(&mut remote);
    (remote, site, a, other, b, copy)
}

fn sent_ops(device: &Device) -> Vec<&op::Op> {
    (device.sent.iter())
        .flat_map(|commit| commit.ops.iter())
        .filter_map(|op| op.op.as_ref())
        .collect()
}

#[test]
fn a_store_put_back_across_an_s_only_pull_alone_reloads_before_the_next_pull_is_recorded() {
    let (mut remote, site, mut a, other, mut b, copy) = s_only_pull_after_a_copy();
    copy_store_while_open(&copy, &site.db);
    let copied = site.stored().cursor;
    b.create(&mut remote, &other.root.join("g.txt"), b"g");
    a.compares_store = false;
    let pulled = a.resume(&remote, a.since());
    assert!(
        matches!(&pulled, Err(stop @ Stop::Store(koidrive_store::Error::Replaced)) if stop.remedy() == crate::status::Remedy::Root),
        "{pulled:?}"
    );
    assert_eq!(site.stored().cursor, copied);
    assert_eq!(names(&site.root), ["f.txt"]);
    drop(a);

    let mut a = restarted(&site, &mut remote);
    a.rest(&mut remote);
    assert_eq!(sent_ops(&a), Vec::<&op::Op>::new());
    assert_eq!(names(&site.root), ["f.txt", "g.txt"]);
    assert_eq!(fs::read(site.root.join("f.txt")).unwrap(), b"B");
}

#[test]
fn a_store_put_back_before_a_commit_build_reloads_and_sends_only_what_the_user_made() {
    let (mut remote, site, mut a, _other, _b, copy) = s_only_pull_after_a_copy();
    fs::write(site.root.join("h.txt"), b"h").unwrap();
    a.rescan();
    a.scan_until(|device| !device.work.is_empty());
    copy_store_while_open(&copy, &site.db);
    a.compares_store = false;
    let built = a.try_run(&mut remote);
    assert!(
        matches!(&built, Err(stop @ Stop::Store(koidrive_store::Error::Replaced)) if stop.remedy() == crate::status::Remedy::Root),
        "{built:?}"
    );
    assert!(a.sent.is_empty(), "{:?}", a.sent);
    drop(a);

    let mut a = restarted(&site, &mut remote);
    a.rest(&mut remote);
    assert!(a.sent.is_empty(), "{:?}", a.sent);
    let [question] = &shown(&site)[..] else {
        panic!("one question");
    };
    let held: Vec<_> = (question.items.iter())
        .map(|item| item.path.unquoted())
        .collect();
    assert_eq!(held, ["h.txt"]);
    drop(a);

    answer(&site, Answer::Publish);
    let mut a = restarted(&site, &mut remote);
    a.rest(&mut remote);
    let sent = sent_ops(&a);
    assert!(
        matches!(&sent[..], [op::Op::CreateFile(create)] if create.name == "h.txt"),
        "{sent:?}"
    );
    assert_eq!(remote.names(), ["f.txt", "h.txt"]);
}

#[test]
fn a_store_put_back_while_the_folder_is_stopped_sends_nothing_after_a_restart() {
    let (mut remote, site, a, other, mut b, copy) = s_only_pull_after_a_copy();
    drop(a);
    copy_store_while_open(&copy, &site.db);
    b.create(&mut remote, &other.root.join("g.txt"), b"g");
    let mut a = restarted(&site, &mut remote);
    a.rest(&mut remote);
    assert_eq!(sent_ops(&a), Vec::<&op::Op>::new());
    assert_eq!(names(&site.root), ["f.txt", "g.txt"]);
}

#[test]
fn a_store_from_before_the_write_stamp_syncs_without_a_reload() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let (other, mut b) = linked(&mut remote, "b");
    fs::write(other.root.join("f.txt"), b"A").unwrap();
    sync(&mut remote, (&site, &mut a), (&other, &mut b));
    drop(a);
    rusqlite::Connection::open(&site.db)
        .unwrap()
        .execute_batch("DROP TABLE write_stamp; DELETE FROM migrations WHERE name = 'write_stamp';")
        .unwrap();

    let mut a = restarted(&site, &mut remote);
    b.create(&mut remote, &other.root.join("g.txt"), b"g");
    a.pull(&remote);
    a.rest(&mut remote);
    a.create(&mut remote, &site.root.join("h.txt"), b"h");
    a.rest(&mut remote);
    assert_eq!(a.folder.view().problem, None);
    assert_eq!(remote.names(), ["f.txt", "g.txt", "h.txt"]);
    let stamp: u64 = rusqlite::Connection::open(&site.db)
        .unwrap()
        .query_row("SELECT stamp FROM write_stamp WHERE id = 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(stamp > 0, "{stamp}");
}

#[test]
fn an_answer_given_while_the_folder_runs_is_taken_with_no_reload_and_a_scan_that_finds_nothing_writes_nothing()
 {
    let mut remote = Remote::new();
    let (site, mut a) = with_a_held_folder(&mut remote);
    answer(&site, Answer::Restore);
    let step = a.folder.requested();
    let taken = a.take(step);
    assert!(taken.is_ok(), "{taken:?}");
    a.rest(&mut remote);
    assert_eq!(names(&site.root.join("d")).len(), 15);
    assert_eq!(a.folder.view().questions, 0);
    assert_eq!(a.folder.view().problem, None);
    a.settle();
    a.rest(&mut remote);

    let watcher = rusqlite::Connection::open(&site.db).unwrap();
    let written = || -> (i64, u64) {
        let version = (watcher.query_row("PRAGMA data_version", [], |row| row.get(0))).unwrap();
        let stamp =
            (watcher.query_row("SELECT stamp FROM write_stamp", [], |row| row.get(0))).unwrap();
        (version, stamp)
    };
    let before = written();
    a.settle();
    a.rest(&mut remote);
    assert!(
        a.sent
            .iter()
            .all(|commit| !kinds(commit).contains(&"delete"))
    );
    assert_eq!(written(), before, "the scan wrote to the store");
}

#[test]
fn a_second_store_put_back_at_the_stamp_the_reloaded_folder_reached_reloads_it_again() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let (other, mut b) = linked(&mut remote, "b");
    fs::write(other.root.join("f.txt"), b"A").unwrap();
    sync(&mut remote, (&site, &mut a), (&other, &mut b));
    let x = site.db.with_extension("x");
    copy_store_while_open(&site.db, &x);
    let ys: Vec<PathBuf> = (1..=8)
        .map(|writes| {
            let folder = &mut a.folder;
            (folder.store)
                .transaction(|tx| folder.engine.checkpoint(tx))
                .unwrap();
            let y = site.db.with_extension(format!("y{writes}"));
            copy_store_while_open(&site.db, &y);
            y
        })
        .collect();
    copy_store_while_open(&x, &site.db);
    drop(a);

    let mut a = site.start();
    let stamp = |db: &Path| -> u64 {
        (rusqlite::Connection::open(db).unwrap())
            .query_row("SELECT stamp FROM write_stamp", [], |row| row.get(0))
            .unwrap()
    };
    b.create(&mut remote, &other.root.join("g.txt"), b"g");
    a.compares_store = false;
    for y in &ys {
        copy_store_while_open(y, &site.db);
        let copied = site.stored().cursor;
        let welcome = remote.space.welcome(&a.caller);
        let pulled =
            (a.event(Event::Connected(welcome))).and_then(|()| a.resume(&remote, a.since()));
        assert!(
            matches!(&pulled, Err(stop @ Stop::Store(koidrive_store::Error::Replaced)) if stop.remedy() == crate::status::Remedy::Root),
            "{y:?}: {pulled:?}"
        );
        assert_eq!(site.stored().cursor, copied, "{y:?}");
        assert_eq!(stamp(&site.db), stamp(y), "{y:?}");
    }
    assert_eq!(names(&site.root), ["f.txt"]);
}
