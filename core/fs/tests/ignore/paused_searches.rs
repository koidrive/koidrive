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
