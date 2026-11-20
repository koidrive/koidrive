// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_folder_replaced_on_the_path_of_a_paused_search_keeps_the_move() {
    let deep: String = (0..30).map(|i| format!("c{i}/")).collect();
    for budget in (1..=8).chain(30..=50) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, KOIIGNORE, "ig/\n");
        write(root, "f.txt", "f");
        write(root, &format!("ig/p/{deep}leaf"), "x");
        for i in 0..5 {
            write(root, &format!("ig/p/n{i}"), "x");
        }
        let mut rules = rules().with_search_budget(budget);
        let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
        let id = snapshot.entries[Path::new("f.txt")].id;
        fs::rename(root.join("f.txt"), root.join("ig/p/f.txt")).unwrap();
        let mut paths = whole().to_vec();
        for rescan in 1.. {
            assert!(rescan < 1000, "budget {budget}: the search never ended");
            let result = snapshot.rescan_with_rules(root, paths, &mut rules).unwrap();
            if !result.changes.is_empty() {
                assert_eq!(
                    result.changes,
                    [Change::Ignored {
                        from: "f.txt".into(),
                        to: "ig/p/f.txt".into(),
                        kind: File,
                        id,
                    }],
                    "budget {budget}"
                );
                break;
            }
            if rescan == 1 {
                fs::rename(root.join("ig/p"), root.join("ig/old")).unwrap();
                fs::create_dir(root.join("ig/p")).unwrap();
                fs::rename(root.join("ig/old/f.txt"), root.join("ig/p/f.txt")).unwrap();
            }
            paths = result.unsettled;
        }
    }
}

#[test]
fn a_deep_folder_renamed_while_a_search_is_paused_is_followed() {
    let deep: PathBuf = ["c"; 30].iter().collect();
    for budget in [3, 50] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, KOIIGNORE, "ig/\n");
        write(root, "f.txt", "f");
        for k in 0..30 {
            let level: PathBuf = ["c"; 30].iter().take(k).collect();
            write(root, &format!("ig/chain/{}/s{k}", level.display()), "x");
        }
        fs::create_dir_all(root.join("ig/chain").join(&deep)).unwrap();
        let mut rules = rules().with_search_budget(budget);
        let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
        let id = snapshot.entries[Path::new("f.txt")].id;
        fs::rename(
            root.join("f.txt"),
            root.join("ig/chain").join(&deep).join("f.txt"),
        )
        .unwrap();
        let mut paths = whole().to_vec();
        for rescan in 1.. {
            assert!(rescan < 10_000, "budget {budget}: the search never ended");
            let result = snapshot.rescan_with_rules(root, paths, &mut rules).unwrap();
            if !result.changes.is_empty() {
                assert_eq!(
                    result.changes,
                    [Change::Ignored {
                        from: "f.txt".into(),
                        to: Path::new("ig/moved").join(&deep).join("f.txt"),
                        kind: File,
                        id,
                    }],
                    "budget {budget}"
                );
                break;
            }
            if rescan == 1 {
                fs::rename(root.join("ig/chain"), root.join("ig/moved")).unwrap();
            }
            paths = result.unsettled;
        }
    }
}

#[test]
fn a_scanned_folder_replaced_above_a_paused_search_keeps_the_move() {
    for budget in 1..=8 {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, KOIIGNORE, "ig/\n");
        write(root, "f.txt", "f");
        write(root, "proj/keep.txt", "k");
        for i in 0..5 {
            write(root, &format!("proj/ig/p/n{i}"), "x");
        }
        let mut rules = rules().with_search_budget(budget);
        let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
        fs::rename(root.join("f.txt"), root.join("proj/ig/p/f.txt")).unwrap();
        for rescan in 1.. {
            assert!(rescan < 1000, "budget {budget}: the search never ended");
            let changes = snapshot
                .rescan_with_rules(root, whole(), &mut rules)
                .unwrap()
                .changes;
            if let Some(change) = changes.iter().find(|change| match change {
                Change::Removed { path, .. } | Change::Ignored { from: path, .. } => {
                    path == Path::new("f.txt")
                }
                _ => false,
            }) {
                assert!(
                    matches!(change, Change::Ignored { .. }),
                    "budget {budget}: {change:?}"
                );
                break;
            }
            if rescan == 1 {
                fs::rename(root.join("proj"), root.join("old")).unwrap();
                fs::create_dir_all(root.join("proj/ig/p")).unwrap();
                fs::rename(root.join("old/ig/p/f.txt"), root.join("proj/ig/p/f.txt")).unwrap();
            }
        }
    }
}

#[test]
fn an_unreadable_folder_past_the_open_folder_cap_does_not_stop_the_search() {
    use std::os::unix::fs::PermissionsExt;
    for depth in [3, 15, 16, 20, 30] {
        for budget in [50, 1000] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            write(root, KOIIGNORE, "ig/\n");
            let locked: PathBuf = std::iter::once("ig")
                .chain(std::iter::repeat_n("c", depth))
                .chain(["locked"])
                .collect();
            write(root, &locked.join("x").to_string_lossy(), "x");
            write(root, "f.txt", "f");
            let mut rules = rules().with_search_budget(budget);
            let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
            let id = snapshot.entries[Path::new("f.txt")].id;
            fs::set_permissions(root.join(&locked), fs::Permissions::from_mode(0o000)).unwrap();
            let unopened = fs::read_dir(root.join(&locked)).is_err();
            fs::remove_file(root.join("f.txt")).unwrap();
            let mut paths = whole().to_vec();
            let mut changes = Vec::new();
            for _ in 0..1000 {
                let rescan = snapshot.rescan_with_rules(root, paths, &mut rules).unwrap();
                if !rescan.changes.is_empty() {
                    changes = rescan.changes;
                    break;
                }
                paths = rescan.unsettled;
            }
            fs::set_permissions(root.join(&locked), fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(rules.coverage().everywhere(), !unopened);
            let expected = match unopened && !cfg!(target_os = "macos") {
                true => Change::Missing {
                    path: "f.txt".into(),
                    kind: File,
                    id,
                },
                false => removed("f.txt", File),
            };
            assert_eq!(changes, [expected], "depth {depth} budget {budget}");
        }
    }
}

#[test]
fn a_search_settles_while_a_folder_on_its_way_is_replaced_before_every_rescan() {
    let chain: String = (0..20).map(|i| format!("c{i}/")).collect();
    let mut files = vec![format!("proj/ig/a/{chain}leaf")];
    files.extend((0..50).map(|i| format!("proj/ig/z{i}")));
    let deep = format!("proj/ig/a/{}", chain.trim_end_matches("c19/"));
    for churned in [
        "proj",
        "proj/ig",
        "proj/ig/a",
        "proj/ig/a/c0/c1/c2",
        deep.as_str(),
    ] {
        for (to, budget) in [None, Some("x/ig/f.txt")]
            .into_iter()
            .flat_map(|to| [1, 10, 30].map(|budget| (to, budget)))
        {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            write(root, KOIIGNORE, "ig/\n");
            write(root, "f.txt", "f");
            write(root, "x/ig/y.txt", "y");
            for file in &files {
                write(root, file, "x");
            }
            let mut rules = rules().with_search_budget(budget);
            let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
            let id = snapshot.entries[Path::new("f.txt")].id;
            match to {
                Some(to) => fs::rename(root.join("f.txt"), root.join(to)).unwrap(),
                None => fs::remove_file(root.join("f.txt")).unwrap(),
            }
            let folders = 23;
            let entries = files.len() + folders + 1 + usize::from(to.is_some());
            let bound = 10 * (folders + entries + 2);
            let mut paths = whole().to_vec();
            let mut rescans = 0;
            let changes = loop {
                rescans += 1;
                assert!(
                    rescans <= bound,
                    "{churned} budget {budget}: no end in {bound} rescans"
                );
                let rescan = snapshot.rescan_with_rules(root, paths, &mut rules).unwrap();
                if !rescan.changes.is_empty() {
                    break rescan.changes;
                }
                fs::remove_dir_all(root.join(churned)).unwrap();
                for file in files
                    .iter()
                    .filter(|file| Path::new(file).starts_with(churned))
                {
                    write(root, file, "x");
                }
                paths = rescan.unsettled;
            };
            let expected = match to {
                Some(to) => Change::Ignored {
                    from: "f.txt".into(),
                    to: to.into(),
                    kind: File,
                    id,
                },
                None if rules.coverage().everywhere() || cfg!(target_os = "macos") => {
                    removed("f.txt", File)
                }
                None => Change::Missing {
                    path: "f.txt".into(),
                    kind: File,
                    id,
                },
            };
            assert_eq!(changes, [expected], "{churned} budget {budget}");
        }
    }
}

#[test]
fn a_folder_replaced_above_the_open_folders_of_a_paused_search_keeps_the_move() {
    let at: String = (0..22).map(|i| format!("c{i}/")).collect();
    let below: String = (0..10).map(|i| format!("c{i}/")).collect();
    let to = format!("ig/p/{at}f.txt");
    for budget in 1..=60 {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, KOIIGNORE, "ig/\n");
        write(root, "f.txt", "f");
        write(root, &format!("ig/p/{at}{below}leaf"), "x");
        for i in 0..5 {
            write(root, &format!("ig/p/{at}n{i}"), "x");
        }
        let mut rules = rules().with_search_budget(budget);
        let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
        let id = snapshot.entries[Path::new("f.txt")].id;
        fs::rename(root.join("f.txt"), root.join(&to)).unwrap();
        let mut paths = whole().to_vec();
        for rescan in 1.. {
            assert!(rescan < 1000, "budget {budget}: the search never ended");
            let result = snapshot.rescan_with_rules(root, paths, &mut rules).unwrap();
            if !result.changes.is_empty() {
                let moved = Change::Ignored {
                    from: "f.txt".into(),
                    to: to.clone().into(),
                    kind: File,
                    id,
                };
                assert_eq!(result.changes, [moved], "budget {budget}");
                break;
            }
            if rescan == 1 {
                fs::rename(root.join("ig/p"), root.join("ig/old")).unwrap();
                fs::create_dir_all(root.join(format!("ig/p/{at}"))).unwrap();
                fs::rename(root.join(format!("ig/old/{at}f.txt")), root.join(&to)).unwrap();
            }
            paths = result.unsettled;
        }
    }
}

#[test]
fn a_search_keeps_a_move_while_folders_on_its_way_move_or_keep_being_replaced() {
    fn mv(root: &Path, from: &str, to: &str) {
        fs::rename(root.join(from), root.join(to)).unwrap();
    }
    type Fill = fn(usize) -> String;
    type Churn = dyn Fn(&Path, usize);
    let cases: [(Fill, &str, &str, &Churn); 3] = [
        (
            |i| format!("ig/p/box/a{i:02}"),
            "ig/p/box/f.txt",
            "ig/a/box/f.txt",
            &|root, rescan| {
                if rescan == 1 {
                    mv(root, "ig/p/box", "ig/a/box");
                }
            },
        ),
        (
            |i| format!("ig/inbox/a{i:02}"),
            "ig/inbox/f.txt",
            "ig/inbox-2/f.txt",
            &|root, rescan| match rescan {
                1 => mv(root, "ig/inbox", "ig/inbox-1"),
                2 => mv(root, "ig/inbox-1", "ig/inbox-2"),
                _ => {}
            },
        ),
        (
            |i| format!("ig/m/k{i:02}/x"),
            "ig/m/k39/f.txt",
            "ig/old001/k39/f.txt",
            &|root, rescan| {
                mv(root, "ig/m", &format!("ig/old{rescan:03}"));
                for i in 0..40 {
                    write(root, &format!("ig/m/k{i:02}/x"), "x");
                }
            },
        ),
    ];
    for (fill, start, to, churn) in cases {
        for budget in [10, 20] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            write(root, KOIIGNORE, "ig/\n");
            write(root, "f.txt", "f");
            write(root, "ig/a/keep", "k");
            for i in 0..40 {
                write(root, &fill(i), "x");
            }
            let mut rules = rules().with_search_budget(budget);
            let mut snapshot = Snapshot::scan_with_rules(root, &mut rules).unwrap();
            let id = snapshot.entries[Path::new("f.txt")].id;
            fs::rename(root.join("f.txt"), root.join(start)).unwrap();
            let mut paths = whole().to_vec();
            let changes = (1..2000)
                .find_map(|rescan| {
                    let result = snapshot
                        .rescan_with_rules(root, paths.clone(), &mut rules)
                        .unwrap();
                    churn(root, rescan);
                    paths = result.unsettled;
                    Some(result.changes).filter(|changes| !changes.is_empty())
                })
                .unwrap_or_else(|| panic!("{start} budget {budget}: the search never ended"));
            let moved = Change::Ignored {
                from: "f.txt".into(),
                to: to.into(),
                kind: File,
                id,
            };
            assert_eq!(changes, [moved], "{start} budget {budget}");
        }
    }
}
