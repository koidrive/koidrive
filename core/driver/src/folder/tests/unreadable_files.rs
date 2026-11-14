// SPDX-License-Identifier: Apache-2.0

const VERSIONS: [&str; 2] = ["first", "second version"];

fn version(at: usize, path: &str) -> String {
    format!("{} of {path}", VERSIONS[at])
}

proptest::proptest! {
    #![proptest_config(koidrive_test_support::regression_config(
        concat!(env!("CARGO_MANIFEST_DIR"), "/proptest-regressions/folder_unreadable.txt"),
        8,
    ))]
    #[test]
    fn files_that_cannot_be_read_hold_back_no_other_change(
        unreadable in proptest::strategy::Strategy::prop_filter(
            proptest::collection::vec(proptest::bool::ANY, 2..7),
            "some files can be read and some cannot",
            |files: &Vec<bool>| files.contains(&true) && files.contains(&false),
        ),
    ) {
        every_file_but_the_unreadable_ones_is_uploaded(&unreadable);
    }
}

fn every_file_but_the_unreadable_ones_is_uploaded(unreadable: &[bool]) {
    use std::os::unix::fs::PermissionsExt;

    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::create_dir(site.root.join("folder")).unwrap();
    let paths: Vec<String> = (0..unreadable.len())
        .map(|at| match at % 2 {
            0 => format!("{at}.txt"),
            _ => format!("folder/{at}.txt"),
        })
        .collect();
    for path in &paths {
        fs::write(site.root.join(path), version(0, path)).unwrap();
    }
    a.scan_until(|a| !a.work.is_empty());
    a.rest(&mut remote);

    for path in &paths {
        fs::write(site.root.join(path), version(1, path)).unwrap();
    }
    a.scan_until(|a| (a.work.iter()).any(|work| matches!(work, Work::Upload(_))));
    let mode = |mode| fs::Permissions::from_mode(mode);
    for (path, _) in paths.iter().zip(unreadable).filter(|(_, unreadable)| **unreadable) {
        fs::set_permissions(site.root.join(path), mode(0o000)).unwrap();
    }
    let on_server = |remote: &Remote| -> Vec<u64> {
        let tree = remote.tree();
        (paths.iter())
            .map(|path| server_size(remote, &tree[path.as_str()]))
            .collect()
    };
    let expected: Vec<u64> = (paths.iter().zip(unreadable))
        .map(|(path, unreadable)| version(usize::from(!unreadable), path).len() as u64)
        .collect();
    let mut timers = Vec::new();
    eventually("every file that can be read reaches the server", || {
        for _ in 0..50 {
            let Some(work) = a.work.pop_front() else { break };
            match work {
                Work::RetryAfter(_) => timers.push(work),
                work => a.perform(work, &mut remote).unwrap(),
            }
        }
        a.scan(Duration::from_millis(100));
        on_server(&remote) == expected
    });

    for path in &paths {
        fs::set_permissions(site.root.join(path), mode(0o644)).unwrap();
    }
    a.work.extend(timers);
    a.rest(&mut remote);
    let everything: Vec<u64> = (paths.iter())
        .map(|path| version(1, path).len() as u64)
        .collect();
    assert_eq!(on_server(&remote), everything);
    assert_eq!(a.folder.view().problem, None);
}

fn leave_out_until_the_upload_fails(a: &mut Device, remote: &mut Remote, path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    a.scan_until(|a| (a.work.iter()).any(|work| matches!(work, Work::Upload(_))));
    fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
    eventually("the file is left out of commits", || {
        while let Some(work) = a.work.pop_front() {
            if !matches!(work, Work::RetryAfter(_)) {
                a.perform(work, remote).unwrap();
            }
        }
        a.scan(Duration::from_millis(100));
        !a.folder.unreadable.is_empty()
            && (a.folder.unreadable.keys()).all(|node| a.folder.engine.commits.problems().contains(node))
    });
}

#[test]
fn a_file_left_out_of_commits_hides_no_other_problem() {
    use std::os::unix::fs::PermissionsExt;

    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let secret = site.root.join("secret.txt");
    fs::write(&secret, b"first").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.rest(&mut remote);

    fs::write(site.root.join("notes.txt"), b"written on a").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let Some(upload @ Work::Upload(_)) = a.work.pop_front() else {
        panic!("the new file is uploaded");
    };
    a.perform(upload, &mut remote).unwrap();
    let Some(Work::Send(_)) = a.work.pop_front() else {
        panic!("the commit is sent");
    };
    let last_seq = a.folder.engine.cursor.as_ref().unwrap().last_seq;
    let result = CommitResult {
        seq: last_seq + 1,
        results: vec![koidrive_proto::v1::OpResult {
            code: op_result::Code::Rejected as i32,
            reject_reason: "INVALID_NAME".into(),
            ..Default::default()
        }],
        changes: Vec::new(),
        ..Default::default()
    };
    let step = a.folder.sent(Ok(result));
    a.take(step).unwrap();

    fs::write(&secret, b"second version").unwrap();
    leave_out_until_the_upload_fails(&mut a, &mut remote, &secret);
    let problem = a.folder.view().problem.unwrap_or_default();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(problem.contains("notes.txt"), "{problem}");
    assert!(problem.contains("secret.txt"), "{problem}");
    assert!(problem.contains("run koi folder restart"), "{problem}");
}

#[test]
fn a_left_out_file_alone_asks_for_no_restart() {
    use std::os::unix::fs::PermissionsExt;

    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let secret = site.root.join("secret.txt");
    fs::write(&secret, b"first").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.rest(&mut remote);

    fs::write(&secret, b"second version").unwrap();
    leave_out_until_the_upload_fails(&mut a, &mut remote, &secret);
    let problem = a.folder.view().problem.unwrap_or_default();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(problem.contains("secret.txt"), "{problem}");
    assert!(problem.contains("trying again at"), "{problem}");
    assert!(!problem.contains("restart"), "{problem}");
}

#[test]
fn a_timer_for_another_retry_keeps_a_left_out_file_out_until_its_own_wait_ends() {
    use std::os::unix::fs::PermissionsExt;

    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let secret = site.root.join("secret.txt");
    fs::write(&secret, b"first").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.rest(&mut remote);

    fs::write(&secret, b"second version").unwrap();
    leave_out_until_the_upload_fails(&mut a, &mut remote, &secret);
    let step = a.folder.retry();
    a.take(step).unwrap();
    let held = a.folder.engine.commits.problems().len();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(held, 1, "a timer that fired early let the file back in");
    assert!(
        (a.work.iter()).any(|work| matches!(work, Work::RetryAfter(wait) if *wait > Duration::from_secs(10))),
        "the file's own retry is still scheduled"
    );
}

#[test]
fn a_left_out_file_saved_again_is_sent_without_waiting_for_its_retry() {
    use std::os::unix::fs::PermissionsExt;

    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let secret = site.root.join("secret.txt");
    fs::write(&secret, b"first").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.rest(&mut remote);

    fs::write(&secret, b"second version").unwrap();
    leave_out_until_the_upload_fails(&mut a, &mut remote, &secret);
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o644)).unwrap();
    fs::write(&secret, b"third, saved after the fix").unwrap();
    eventually("the saved file reaches the server", || {
        while let Some(work) = a.work.pop_front() {
            if !matches!(work, Work::RetryAfter(_)) {
                a.perform(work, &mut remote).unwrap();
            }
        }
        a.scan(Duration::from_millis(100));
        let tree = remote.tree();
        server_size(&remote, &tree["secret.txt"]) == 26
    });
    assert!(a.folder.engine.commits.problems().is_empty());
}



fn fail_upload_of(a: &mut Device, remote: &mut Remote, size: usize) {
    eventually("the changed file is uploaded", || {
        while let Some(work) = a.work.pop_front() {
            match work {
                Work::Upload(upload) => {
                    let Some(chunk) = (upload.chunks.iter()).find(|chunk| chunk.size as usize == size) else {
                        a.perform(Work::Upload(upload), remote).unwrap();
                        continue;
                    };
                    let error = ChunkError::SourceChanged {
                        hash: chunk.hash.clone(),
                    };
                    let step = a.folder.uploaded(Err(error));
                    a.take(step).unwrap();
                    return true;
                }
                Work::RetryAfter(_) => {}
                work => a.perform(work, remote).unwrap(),
            }
        }
        a.scan(Duration::from_millis(100));
        false
    });
}

#[test]
fn a_left_out_file_that_changes_again_is_left_out_at_its_next_failure() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let secret = site.root.join("secret.txt");
    fs::write(&secret, b"first").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.rest(&mut remote);

    for size in 10..10 + LEFT_OUT_AFTER as usize {
        fs::write(&secret, "c".repeat(size)).unwrap();
        fail_upload_of(&mut a, &mut remote, size);
    }
    let left_out = a.folder.view().problem.unwrap_or_default();
    assert!(left_out.contains("secret.txt"), "{left_out}");

    fs::write(&secret, "c".repeat(30)).unwrap();
    fail_upload_of(&mut a, &mut remote, 30);
    let problem = a.folder.view().problem.unwrap_or_default();
    assert!(problem.contains("secret.txt"), "the file went back to abandoning commits: {problem:?}");
}

proptest::proptest! {
    #![proptest_config(koidrive_test_support::regression_config(
        concat!(env!("CARGO_MANIFEST_DIR"), "/proptest-regressions/folder_unwritable.txt"),
        8,
    ))]
    #[test]
    fn files_that_cannot_be_written_hold_back_no_other_download(
        unwritable in proptest::strategy::Strategy::prop_filter(
            proptest::collection::vec(proptest::bool::ANY, 2..7),
            "some files can be written and some cannot",
            |files: &Vec<bool>| files.contains(&true) && files.contains(&false),
        ),
    ) {
        every_file_but_the_unwritable_ones_is_downloaded(&unwritable);
    }
}

fn every_file_but_the_unwritable_ones_is_downloaded(unwritable: &[bool]) {
    use std::os::unix::fs::PermissionsExt;

    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    let (site_b, mut b) = linked(&mut remote, "b");
    let paths: Vec<String> = (0..unwritable.len())
        .map(|at| format!("{at}/file.txt"))
        .collect();
    for (at, path) in paths.iter().enumerate() {
        fs::create_dir(site_a.root.join(at.to_string())).unwrap();
        fs::write(site_a.root.join(path), version(0, path)).unwrap();
    }
    a.scan_until(|a| !a.work.is_empty());
    a.rest(&mut remote);
    b.pull(&remote);
    b.rest(&mut remote);

    let mode = |mode| fs::Permissions::from_mode(mode);
    for (at, _) in (unwritable.iter().enumerate()).filter(|(_, unwritable)| **unwritable) {
        fs::set_permissions(site_b.root.join(at.to_string()), mode(0o555)).unwrap();
    }
    for path in &paths {
        fs::write(site_a.root.join(path), version(1, path)).unwrap();
    }
    a.scan_until(|a| !a.work.is_empty());
    a.rest(&mut remote);
    b.pull(&remote);
    let on_b = |site: &Site| -> Vec<String> {
        (paths.iter())
            .map(|path| fs::read_to_string(site.root.join(path)).unwrap())
            .collect()
    };
    let expected: Vec<String> = (paths.iter().zip(unwritable))
        .map(|(path, unwritable)| version(usize::from(!unwritable), path))
        .collect();
    let mut timers = Vec::new();
    eventually("every file that can be written reaches the folder", || {
        for _ in 0..50 {
            let Some(work) = b.work.pop_front() else { break };
            match work {
                Work::RetryAfter(_) => timers.push(work),
                work => b.perform(work, &mut remote).unwrap(),
            }
        }
        b.scan(Duration::from_millis(100));
        on_b(&site_b) == expected
    });

    for at in 0..paths.len() {
        fs::set_permissions(site_b.root.join(at.to_string()), mode(0o755)).unwrap();
    }
    drop(b);
    let mut b = restarted(&site_b, &mut remote);
    b.rest(&mut remote);
    let everything: Vec<String> = (paths.iter()).map(|path| version(1, path)).collect();
    assert_eq!(on_b(&site_b), everything);
    assert_eq!(b.folder.view().problem, None);
}



#[test]
fn a_file_let_back_in_without_an_upload_keeps_the_servers_refusal_of_its_next_change() {
    use std::os::unix::fs::PermissionsExt;

    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let secret = site.root.join("secret.txt");
    fs::write(&secret, b"first").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.rest(&mut remote);

    fs::write(&secret, b"second version").unwrap();
    leave_out_until_the_upload_fails(&mut a, &mut remote, &secret);
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o644)).unwrap();
    fs::write(&secret, b"first").unwrap();
    eventually("the file is let back in", || {
        a.scan(Duration::from_millis(100));
        while let Some(work) = a.work.pop_front() {
            if !matches!(work, Work::RetryAfter(_)) {
                a.perform(work, &mut remote).unwrap();
            }
        }
        a.folder.engine.commits.problems().is_empty()
    });

    fs::rename(&secret, site.root.join("renamed.txt")).unwrap();
    let mut sent = None;
    eventually("the rename is sent", || {
        a.scan(Duration::from_millis(100));
        while let Some(work) = a.work.pop_front() {
            match work {
                Work::Send(commit) => sent = Some(commit),
                Work::RetryAfter(_) => {}
                work => a.perform(work, &mut remote).unwrap(),
            }
        }
        sent.is_some()
    });
    let last_seq = a.folder.engine.cursor.as_ref().unwrap().last_seq;
    let result = CommitResult {
        seq: last_seq + 1,
        results: vec![
            koidrive_proto::v1::OpResult {
                code: op_result::Code::Rejected as i32,
                reject_reason: "INVALID_NAME".into(),
                ..Default::default()
            };
            sent.as_ref().unwrap().ops.len()
        ],
        changes: Vec::new(),
        ..Default::default()
    };
    let step = a.folder.sent(Ok(result));
    a.take(step).unwrap();
    let problem = a.folder.view().problem.unwrap_or_default();
    assert!(problem.contains("INVALID_NAME"), "{problem}");
    assert!(problem.contains("run koi folder restart"), "{problem}");
    a.scan(Duration::from_millis(100));
    let problem = a.folder.view().problem.unwrap_or_default();
    assert!(problem.contains("INVALID_NAME"), "the next batch cleared the refusal: {problem}");
}

#[test]
fn a_recovery_forgets_which_files_were_left_out() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let secret = site.root.join("secret.txt");
    fs::write(&secret, b"first").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.rest(&mut remote);
    let backup = remote.space.clone();

    fs::write(&secret, b"second version").unwrap();
    leave_out_until_the_upload_fails(&mut a, &mut remote, &secret);
    a.work.clear();
    remote.space.rewind(&backup, NOW);
    a.event(Event::ResyncRequired(ResyncRequired {
        reason: Reason::HistoryChanged as i32,
        ..ResyncRequired::default()
    }))
    .unwrap();
    while !a.folder.engine.commits.problems().is_empty() {
        let work = a.work.pop_front().expect("the recovery ends");
        a.perform(work, &mut remote).unwrap();
    }
    let left_out = (a.folder.unreadable.values()).any(|unreadable| unreadable.left_out.is_some());
    fs::set_permissions(&secret, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
    assert!(!left_out, "a file stays left out after the recovery let every node back in");
}

fn synced_on_both(files: &[&str]) -> (Remote, Site, Device, Site, Device) {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    let (site_b, mut b) = linked(&mut remote, "b");
    for file in files {
        let path = site_a.root.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "first").unwrap();
    }
    a.scan_until(|a| !a.work.is_empty());
    a.rest(&mut remote);
    b.pull(&remote);
    b.rest(&mut remote);
    (remote, site_a, a, site_b, b)
}

fn run_holding_timers(b: &mut Device, remote: &mut Remote, timers: &mut Vec<Work>) -> usize {
    let mut stages = 0;
    for _ in 0..20 {
        while let Some(work) = b.work.pop_front() {
            match work {
                Work::RetryAfter(_) => timers.push(work),
                work => {
                    stages += usize::from(matches!(work, Work::Stage(_) | Work::StageAfter(..)));
                    b.perform(work, remote).unwrap();
                }
            }
        }
        b.scan(Duration::from_millis(50));
    }
    stages
}

#[test]
fn a_file_moved_and_rewritten_into_a_folder_this_device_does_not_allow_stays_where_it_was() {
    use std::os::unix::fs::PermissionsExt;

    let (mut remote, site_a, mut a, site_b, mut b) = synced_on_both(&["locked/f.txt", "open/g.txt"]);
    let locked = site_b.root.join("locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
    fs::rename(site_a.root.join("open/g.txt"), site_a.root.join("locked/g.txt")).unwrap();
    fs::write(site_a.root.join("locked/g.txt"), "rewritten").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.rest(&mut remote);
    b.pull(&remote);
    let mut timers = Vec::new();
    run_holding_timers(&mut b, &mut remote, &mut timers);
    let problem = b.folder.view().problem.unwrap_or_default();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(fs::read_to_string(site_b.root.join("open/g.txt")).unwrap(), "first");
    assert!(!locked.join("g.txt").exists());
    assert!(problem.contains("g.txt"), "{problem}");

    drop(b);
    let mut b = restarted(&site_b, &mut remote);
    b.rest(&mut remote);
    assert_eq!(fs::read_to_string(locked.join("g.txt")).unwrap(), "rewritten");
    assert!(!site_b.root.join("open/g.txt").exists());
}

#[test]
fn a_download_this_device_does_not_allow_is_fetched_again_only_after_its_own_wait() {
    use std::os::unix::fs::PermissionsExt;

    let (mut remote, site_a, mut a, site_b, mut b) = synced_on_both(&["locked/big.bin", "open/g.txt"]);
    let locked = site_b.root.join("locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
    fs::write(site_a.root.join("locked/big.bin"), vec![7u8; 1 << 20]).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.rest(&mut remote);
    b.pull(&remote);
    let mut timers = Vec::new();
    let mut stages = run_holding_timers(&mut b, &mut remote, &mut timers);
    for _ in 0..6 {
        for timer in std::mem::take(&mut timers) {
            b.perform(timer, &mut remote).unwrap();
        }
        stages += run_holding_timers(&mut b, &mut remote, &mut timers);
    }
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(stages, 1);
}

#[test]
fn a_node_denied_before_that_now_fails_for_another_reason_waits_only_the_ordinary_retry() {
    use std::os::unix::fs::PermissionsExt;

    let (mut remote, site_a, mut a, site_b, mut b) = synced_on_both(&["locked/file.txt", "open/g.txt"]);
    let locked = site_b.root.join("locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
    fs::write(site_a.root.join("locked/file.txt"), "second version").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.rest(&mut remote);
    b.pull(&remote);
    let mut timers = Vec::new();
    run_holding_timers(&mut b, &mut remote, &mut timers);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

    fs::write(site_a.root.join("locked/file.txt"), "third remote version").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.rest(&mut remote);
    b.pull(&remote);
    let Some(Work::Stage(pending)) = b.work.pop_front() else {
        panic!("the changed remote version must be planned without the old permission wait");
    };
    fs::write(locked.join("file.txt"), "local edit during staging").unwrap();
    let staged = ready(pending.stage(&remote.chunks));
    let step = b.folder.staged(pending, staged);
    b.take(step).unwrap();
    assert_eq!(b.folder.engine.effects.blocked().len(), 1);
    let why = &b.folder.effect_failures.values().next().unwrap().1;
    assert_eq!(why, "the entry changed on this device");

    let step = b.folder.retry();
    b.take(step).unwrap();
    assert!(
        b.folder.engine.effects.blocked().is_empty(),
        "the ordinary failure is held by the earlier permission wait"
    );
}

fn denied_download(file: &str) -> (Remote, Site, Device, Site, Device, Vec<Work>, usize) {
    use std::os::unix::fs::PermissionsExt;

    let (mut remote, site_a, mut a, site_b, mut b) = synced_on_both(&[file, "open/g.txt"]);
    let folder = site_b.root.join(Path::new(file).parent().unwrap());
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o555)).unwrap();
    fs::write(site_a.root.join(file), "second version").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.rest(&mut remote);
    b.pull(&remote);
    let mut timers = Vec::new();
    let stages = run_holding_timers(&mut b, &mut remote, &mut timers);
    (remote, site_a, a, site_b, b, timers, stages)
}

fn writable_again(site: &Site, folder: &str) {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(site.root.join(folder), fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn a_denied_download_is_not_fetched_again_while_its_folder_stays_read_only() {
    let (mut remote, _site_a, _a, site_b, mut b, mut timers, mut stages) = denied_download("locked/file.txt");
    for _ in 0..6 {
        for due in b.folder.denied_due.values_mut() {
            *due = Instant::now();
        }
        timers.clear();
        let step = b.folder.retry();
        b.take(step).unwrap();
        stages += run_holding_timers(&mut b, &mut remote, &mut timers);
    }
    writable_again(&site_b, "locked");
    assert_eq!(stages, 1);
}

#[test]
fn a_folder_made_writable_again_gets_its_download_without_waiting() {
    let (mut remote, _site_a, _a, site_b, mut b, mut timers, _) = denied_download("locked/file.txt");
    writable_again(&site_b, "locked");
    eventually("the download lands once its folder can be written", || {
        run_holding_timers(&mut b, &mut remote, &mut timers);
        fs::read_to_string(site_b.root.join("locked/file.txt")).unwrap() == "second version"
    });
}

#[test]
fn a_denied_change_says_koidrive_tries_again_by_itself() {
    let (_remote, _site_a, _a, site_b, b, _, _) = denied_download("locked/file.txt");
    let problem = b.folder.view().problem.unwrap_or_default();
    writable_again(&site_b, "locked");
    assert!(problem.contains("locked/file.txt"), "{problem}");
    assert!(problem.contains("Koidrive tries again by itself"), "{problem}");
    assert!(!problem.contains("restart"), "{problem}");
}

#[test]
fn a_change_koidrive_cannot_make_in_its_own_folder_names_that_folder() {
    use std::os::unix::fs::PermissionsExt;

    let (mut remote, site_a, mut a, site_b, mut b) = synced_on_both(&["docs/file.txt"]);
    let quarantine = site_b.root.join(".koidrive/quarantine");
    fs::create_dir_all(&quarantine).unwrap();
    fs::set_permissions(&quarantine, fs::Permissions::from_mode(0o555)).unwrap();
    fs::write(site_a.root.join("docs/file.txt"), "second version").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.rest(&mut remote);
    b.pull(&remote);
    let mut timers = Vec::new();
    run_holding_timers(&mut b, &mut remote, &mut timers);
    let problem = b.folder.view().problem.unwrap_or_default();
    fs::set_permissions(&quarantine, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(problem.contains(r#"".koidrive/quarantine""#), "{problem}");
    assert!(problem.contains("Koidrive tries again by itself"), "{problem}");
}
