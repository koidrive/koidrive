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
