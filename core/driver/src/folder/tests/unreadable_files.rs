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

