// SPDX-License-Identifier: Apache-2.0

mod common;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};

use common::{
    EDITED, ID, REMOTE, SCENARIOS, SYNCED, apply_one, assert_settled, at, bind, contents, entries,
    marker, now, prepare, private_names, recover_one, rewrite_keeping_mtime, same, stage, trashed,
};
use koidrive_fs::{
    Action, Base, Displaced, Effect, Location, Outcome, PARKED_DIR, PRIVATE_DIR, QUARANTINE_DIR,
    Reason, STAGING_DIR, Snapshot, Step, TRASH_RETENTION, Unresolved, read_marker,
};

fn replace(root: &Path) -> Effect {
    prepare(&SCENARIOS[0], root)
}

fn apply(root: &Path, effect: &Effect, mut hook: impl FnMut(Step)) -> Outcome {
    apply_one(&bind(root), effect, &mut hook).unwrap()
}

fn failed(reason: Reason) -> Outcome {
    Outcome::Failed {
        reason,
        recovered: None,
    }
}

fn made(root: &Path, path: &str) -> Outcome {
    Outcome::Done {
        identity: Some(entries(root, &[path])[0].id),
    }
}

const MOVED_OR_REMOVED: Outcome = Outcome::Done { identity: None };

fn withdraw(root: &Path, moved: &Path) {
    fs::rename(root, moved).unwrap();
    fs::create_dir(root).unwrap();
}

fn withdrawn(err: &std::io::Error) -> bool {
    matches!(
        err.get_ref()
            .and_then(|err| err.downcast_ref::<Unresolved>()),
        Some(Unresolved::RootWithdrawn(_))
    )
}

fn listing(path: &Path) -> Vec<PathBuf> {
    let mut names: Vec<PathBuf> = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    names.sort();
    names
}

#[test]
fn replacement_places_the_new_file_and_trashes_the_old_one() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = replace(root);
    let mut steps = Vec::new();
    let outcome = apply(root, &effect, |step| steps.push(step));
    assert_eq!(outcome, made(root, "docs/report.txt"));
    assert_eq!(
        steps,
        [
            Step::Marked,
            Step::Checked,
            Step::Quarantined,
            Step::Compared,
            Step::Placed,
            Step::Trashed
        ]
    );
    assert_eq!(fs::read(root.join("docs/report.txt")).unwrap(), REMOTE);
    assert_eq!(trashed(root), [SYNCED]);
    assert_settled(root);
}

#[test]
fn marker_holds_the_counter_before_the_disk_changes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = replace(root);
    let mut seen = None;
    apply(root, &effect, |step| {
        if step == Step::Marked {
            seen = Some((
                read_marker(root).unwrap(),
                fs::read(root.join("docs/report.txt")).unwrap(),
            ));
        }
    });
    assert_eq!(seen, Some((Some(marker()), SYNCED.to_vec())));
}

#[test]
fn write_before_the_check_fails_without_displacing_anything() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = replace(root);
    fs::write(root.join("docs/report.txt"), "user edit").unwrap();
    assert_eq!(apply(root, &effect, |_| {}), failed(Reason::Changed));
    assert_eq!(
        fs::read(root.join("docs/report.txt")).unwrap(),
        b"user edit"
    );
    assert!(trashed(root).is_empty());
    assert_settled(root);
}

#[test]
fn write_between_the_check_and_the_quarantine_is_put_back() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = replace(root);
    let outcome = apply(root, &effect, |step| {
        if step == Step::Checked {
            fs::write(root.join("docs/report.txt"), "user edit").unwrap();
        }
    });
    assert_eq!(outcome, failed(Reason::Changed));
    assert_eq!(
        fs::read(root.join("docs/report.txt")).unwrap(),
        b"user edit"
    );
    assert!(trashed(root).is_empty());
    assert_settled(root);
}

#[test]
fn write_through_an_old_descriptor_before_the_comparison_is_put_back() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = replace(root);
    let held = OpenOptions::new()
        .write(true)
        .open(root.join("docs/report.txt"))
        .unwrap();
    let outcome = apply(root, &effect, |step| {
        if step == Step::Quarantined {
            held.write_all_at(b"SYNCED", 0).unwrap();
        }
    });
    assert_eq!(outcome, failed(Reason::Changed));
    assert_eq!(fs::read(root.join("docs/report.txt")).unwrap(), b"SYNCED");
    assert_settled(root);
}

#[test]
fn edit_only_the_content_shows_is_put_back() {
    for scenario in [&SCENARIOS[0], &SCENARIOS[4]] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let effect = prepare(scenario, root);
        let report = root.join("docs/report.txt");
        rewrite_keeping_mtime(&report, EDITED);
        let displaced = match &effect.action {
            Action::WriteFile {
                replaces: Some(displaced),
                ..
            }
            | Action::RemoveFile { displaced } => displaced.entry,
            _ => unreachable!(),
        };
        assert_eq!(
            entries(root, &["docs/report.txt"]),
            [displaced],
            "{}",
            scenario.name
        );
        assert_eq!(
            apply(root, &effect, |_| {}),
            failed(Reason::Changed),
            "{}",
            scenario.name
        );
        assert_eq!(fs::read(&report).unwrap(), EDITED, "{}", scenario.name);
        assert!(trashed(root).is_empty(), "{}", scenario.name);
        assert_settled(root);
    }
}

#[test]
fn edit_whose_name_was_taken_again_becomes_a_recovered_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = replace(root);
    let outcome = apply(root, &effect, |step| match step {
        Step::Checked => fs::write(root.join("docs/report.txt"), "user edit").unwrap(),
        Step::Compared => fs::write(root.join("docs/report.txt"), "new file").unwrap(),
        _ => {}
    });
    let recovered = PathBuf::from("docs/report (recovered 2023-11-14).txt");
    assert_eq!(
        outcome,
        Outcome::Failed {
            reason: Reason::Changed,
            recovered: Some(recovered.clone()),
        }
    );
    assert_eq!(fs::read(root.join(recovered)).unwrap(), b"user edit");
    assert_eq!(fs::read(root.join("docs/report.txt")).unwrap(), b"new file");
    assert_settled(root);
}

#[test]
fn target_taken_during_placement_keeps_the_entry_there() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = replace(root);
    let outcome = apply(root, &effect, |step| {
        if step == Step::Compared {
            fs::write(root.join("docs/report.txt"), "someone else").unwrap();
        }
    });
    assert_eq!(outcome, failed(Reason::Overtaken));
    assert_eq!(
        fs::read(root.join("docs/report.txt")).unwrap(),
        b"someone else"
    );
    assert_eq!(trashed(root), [SYNCED]);
    assert_settled(root);
}

#[test]
fn write_through_a_descriptor_held_across_the_effect_lands_in_the_trash() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = replace(root);
    let mut held = OpenOptions::new()
        .append(true)
        .open(root.join("docs/report.txt"))
        .unwrap();
    let outcome = apply(root, &effect, |_| {});
    assert_eq!(outcome, made(root, "docs/report.txt"));
    held.write_all(b" and more").unwrap();
    assert_eq!(fs::read(root.join("docs/report.txt")).unwrap(), REMOTE);
    assert_eq!(trashed(root), [b"synced and more".to_vec()]);
}

#[test]
fn removed_file_goes_to_the_trash_unless_it_changed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = prepare(&SCENARIOS[4], root);
    assert_eq!(apply(root, &effect, |_| {}), MOVED_OR_REMOVED);
    assert_eq!(contents(root.join("docs/report.txt")), None);
    assert_eq!(trashed(root), [SYNCED]);
    assert_settled(root);

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = prepare(&SCENARIOS[4], root);
    let outcome = apply(root, &effect, |step| {
        if step == Step::Checked {
            fs::write(root.join("docs/report.txt"), "user edit").unwrap();
        }
    });
    assert_eq!(outcome, failed(Reason::Changed));
    assert_eq!(
        fs::read(root.join("docs/report.txt")).unwrap(),
        b"user edit"
    );
    assert!(trashed(root).is_empty());
    assert_settled(root);
}

#[test]
fn folder_that_gained_an_entry_is_put_back() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = prepare(&SCENARIOS[5], root);
    let outcome = apply(root, &effect, |step| {
        if step == Step::Checked {
            fs::write(root.join("docs/empty/late.txt"), "late").unwrap();
        }
    });
    assert_eq!(outcome, failed(Reason::Holds(vec!["late.txt".into()])));
    assert_eq!(fs::read(root.join("docs/empty/late.txt")).unwrap(), b"late");
    assert!(private_names(root, "trash").is_empty());
    assert_settled(root);
}

#[test]
fn folder_that_holds_only_platform_files_enters_the_trash_empty() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = prepare(&SCENARIOS[6], root);
    let outcome = apply(root, &effect, |_| {});
    assert_eq!(outcome, MOVED_OR_REMOVED);
    (SCENARIOS[6].check)(root, &outcome);
    assert_settled(root);

    let reclaimed = bind(root)
        .reclaim_trash(now() + TRASH_RETENTION, u64::MAX, &mut same)
        .unwrap();
    assert_eq!(reclaimed.removed, [ID], "an empty folder is deleted");
    assert_eq!(reclaimed.recovered, Vec::<std::ffi::OsString>::new());
}

#[test]
fn folder_that_holds_anything_but_platform_files_is_put_back_as_it_was() {
    let others: [fn(&Path); 3] = [
        |folder| fs::write(folder.join("build.log"), "log").unwrap(),
        |folder| fs::create_dir(folder.join("Thumbs.db")).unwrap(),
        |folder| std::os::unix::fs::symlink("build.log", folder.join("desktop.ini")).unwrap(),
    ];
    let names = ["build.log", "Thumbs.db", "desktop.ini"];
    for (other, name) in others.into_iter().zip(names) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let effect = prepare(&SCENARIOS[5], root);
        let folder = root.join("docs/empty");
        fs::write(folder.join(".DS_Store"), "view").unwrap();
        other(&folder);
        let before = listing(&folder);
        assert_eq!(before.len(), 2);

        let held = Reason::Holds(vec![name.into()]);
        assert_eq!(apply(root, &effect, |_| {}), failed(held));
        assert_eq!(listing(&folder), before);
        assert_eq!(fs::read(folder.join(".DS_Store")).unwrap(), b"view");
        assert!(private_names(root, "trash").is_empty());
        assert_settled(root);
    }
}

#[test]
fn creations_never_take_a_name_that_is_in_use() {
    for scenario in [&SCENARIOS[1], &SCENARIOS[2]] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let effect = prepare(scenario, root);
        let name = match &effect.action {
            Action::CreateDir { .. } => "docs/sub",
            _ => "docs/new.txt",
        };
        fs::write(root.join(name), "already here").unwrap();
        assert_eq!(
            apply(root, &effect, |_| {}),
            failed(Reason::Taken),
            "{}",
            scenario.name
        );
        assert_eq!(fs::read(root.join(name)).unwrap(), b"already here");
        assert_settled(root);
    }
}

#[test]
fn move_needs_the_source_identity_and_a_free_target() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = prepare(&SCENARIOS[3], root);
    fs::write(root.join("other/b.txt"), "taken").unwrap();
    assert_eq!(apply(root, &effect, |_| {}), failed(Reason::Taken));
    assert_eq!(fs::read(root.join("docs/a.txt")).unwrap(), SYNCED);
    assert_eq!(fs::read(root.join("other/b.txt")).unwrap(), b"taken");

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = prepare(&SCENARIOS[3], root);
    fs::write(root.join("docs/a2.txt"), SYNCED).unwrap();
    fs::rename(root.join("docs/a2.txt"), root.join("docs/a.txt")).unwrap();
    assert_eq!(apply(root, &effect, |_| {}), failed(Reason::Changed));
    assert_eq!(contents(root.join("other/b.txt")), None);
}

#[test]
fn move_into_and_out_of_the_parked_folder() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("a")).unwrap();
    fs::write(root.join("a/f.txt"), SYNCED).unwrap();
    let found = entries(root, &["a", "a/f.txt"]);
    let parked = Location::Parked("node-1".into());
    let park = Effect {
        id: ID.into(),
        action: Action::Move {
            from: at(vec![common::dir("a", &found[0])], "f.txt"),
            id: found[1].id,
            to: parked.clone(),
        },
    };
    assert_eq!(apply(root, &park, |_| {}), MOVED_OR_REMOVED);
    let parked_path = root.join(PRIVATE_DIR).join(PARKED_DIR).join("node-1");
    assert_eq!(fs::read(&parked_path).unwrap(), SYNCED);

    let unpark = Effect {
        id: "e2".into(),
        action: Action::Move {
            from: parked,
            id: found[1].id,
            to: at(Vec::new(), "F.txt"),
        },
    };
    assert_eq!(apply(root, &unpark, |_| {}), MOVED_OR_REMOVED);
    assert_eq!(fs::read(root.join("F.txt")).unwrap(), SYNCED);
}

#[test]
fn parent_swapped_for_a_symlink_after_the_check_fails_the_effect() {
    let outside = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = replace(root);
    let outcome = apply(root, &effect, |step| {
        if step == Step::Checked {
            fs::rename(root.join("docs"), root.join("docs-old")).unwrap();
            std::os::unix::fs::symlink(outside.path(), root.join("docs")).unwrap();
        }
    });
    assert_eq!(outcome, failed(Reason::Moved));
    assert_eq!(fs::read(root.join("docs-old/report.txt")).unwrap(), SYNCED);
    assert!(listing(outside.path()).is_empty());
    assert_settled(root);
}

#[test]
fn parent_swapped_for_another_folder_after_the_check_fails_the_effect() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = replace(root);
    let outcome = apply(root, &effect, |step| {
        if step == Step::Checked {
            fs::rename(root.join("docs"), root.join("docs-old")).unwrap();
            fs::create_dir(root.join("docs")).unwrap();
            fs::write(root.join("docs/report.txt"), SYNCED).unwrap();
        }
    });
    assert_eq!(outcome, failed(Reason::Moved));
    assert_eq!(fs::read(root.join("docs-old/report.txt")).unwrap(), SYNCED);
    assert_eq!(fs::read(root.join("docs/report.txt")).unwrap(), SYNCED);
    assert_settled(root);
}

#[test]
fn parent_moved_out_of_the_root_after_the_check_is_left_alone() {
    for scenario in &SCENARIOS {
        let outside = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let effect = prepare(scenario, root);
        let before = Snapshot::scan(root.join("docs")).unwrap();
        let outcome = apply(root, &effect, |step| {
            if step == Step::Checked {
                fs::rename(root.join("docs"), outside.path().join("docs")).unwrap();
            }
        });
        assert_eq!(outcome, failed(Reason::Moved), "{}", scenario.name);
        assert_eq!(
            listing(outside.path()),
            [outside.path().join("docs")],
            "{}",
            scenario.name
        );
        let after = Snapshot::scan(outside.path().join("docs")).unwrap();
        assert_eq!(after.entries, before.entries, "{}", scenario.name);
        assert_settled(root);
    }
}

#[test]
fn parked_effects_need_the_parked_folder_identity() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let parked = root.join(PRIVATE_DIR).join(PARKED_DIR);
    fs::create_dir_all(parked.join("node-1")).unwrap();
    fs::write(parked.join("node-1/f.txt"), SYNCED).unwrap();
    fs::create_dir(root.join("x")).unwrap();
    let wrong = entries(root, &["x"])[0].id;
    let file = Snapshot::scan(parked.join("node-1")).unwrap().entries[Path::new("f.txt")];
    let effect = Effect {
        id: ID.into(),
        action: Action::RemoveFile {
            displaced: Displaced {
                at: Location::At {
                    base: Base::Parked {
                        name: "node-1".into(),
                        id: wrong,
                    },
                    dirs: Vec::new(),
                    name: "f.txt".into(),
                },
                entry: file,
            },
        },
    };
    assert_eq!(apply(root, &effect, |_| {}), failed(Reason::Moved));
    assert_eq!(fs::read(parked.join("node-1/f.txt")).unwrap(), SYNCED);
}

#[test]
fn recovery_puts_back_a_file_written_through_an_old_descriptor_after_a_crash() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = replace(root);
    let held = OpenOptions::new()
        .write(true)
        .open(root.join("docs/report.txt"))
        .unwrap();
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        apply(root, &effect, |step| {
            if step == Step::Quarantined {
                panic!("crash");
            }
        })
    }));
    assert!(crashed.is_err());
    held.write_all_at(b"SYNCED", 0).unwrap();

    let space = bind(root);
    let outcome = recover_one(&space, &effect, &mut |_| {}).unwrap();
    assert_eq!(outcome, failed(Reason::Changed));
    assert_eq!(fs::read(root.join("docs/report.txt")).unwrap(), b"SYNCED");
    assert_settled(root);
}

#[test]
fn replacement_without_a_staged_file_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = replace(root);
    fs::remove_file(root.join(PRIVATE_DIR).join(STAGING_DIR).join(ID)).unwrap();
    let err = apply_one(&bind(root), &effect, &mut |_| {}).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound, "{err}");
    assert_eq!(fs::read(root.join("docs/report.txt")).unwrap(), SYNCED);
    assert!(private_names(root, QUARANTINE_DIR).is_empty());
    assert!(trashed(root).is_empty());
}

#[test]
fn staged_files_without_a_record_are_cleared() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let space = bind(root);
    stage(&space, REMOTE);
    let mut other = space.stage("e0").unwrap();
    other.write_all(b"older").unwrap();
    assert_eq!(space.clear_staging(Some(ID)).unwrap(), ["e0"]);
    assert_eq!(private_names(root, STAGING_DIR), [ID]);
    assert_eq!(space.clear_staging(None).unwrap(), [ID]);
    assert!(private_names(root, STAGING_DIR).is_empty());
}

#[test]
fn staged_file_gets_the_executable_bit() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let space = bind(root);
    stage(&space, REMOTE);
    let staged = root.join(PRIVATE_DIR).join(STAGING_DIR).join(ID);
    assert_eq!(
        fs::metadata(&staged).unwrap().permissions().mode() & 0o111,
        0
    );
    space.finish_staged(ID, true).unwrap();
    let mode = fs::metadata(&staged).unwrap().permissions().mode();
    assert_eq!(mode & 0o111, (mode & 0o444) >> 2);
    assert_ne!(mode & 0o111, 0);
}

#[test]
fn effect_ids_must_be_single_names() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut effect = replace(root);
    let space = bind(root);
    for id in ["", ".", "..", "../e1", "a/b"] {
        effect.id = id.into();
        let err = apply_one(&space, &effect, &mut |_| {}).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{id:?}");
        assert_eq!(
            space.stage(id).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
    }
    assert_eq!(fs::read(root.join("docs/report.txt")).unwrap(), SYNCED);
}
#[test]
fn done_carries_the_identity_of_the_entry_the_effect_made() {
    for scenario in &SCENARIOS {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let effect = prepare(scenario, root);
        let staged = fs::metadata(root.join(PRIVATE_DIR).join(STAGING_DIR).join(ID)).ok();
        let outcome = apply(root, &effect, |_| {});
        assert!(
            matches!(outcome, Outcome::Done { .. }),
            "{}: {outcome:?}",
            scenario.name
        );
        (scenario.check)(root, &outcome);
        if let Some(staged) = staged {
            let Outcome::Done {
                identity: Some(identity),
            } = outcome
            else {
                panic!("{}: {outcome:?}", scenario.name);
            };
            assert_eq!(
                (identity.dev, identity.ino),
                (staged.dev(), u128::from(staged.ino())),
                "{}",
                scenario.name
            );
        }
    }
}

#[test]
fn root_moved_during_an_effect_gets_nothing_at_its_path_and_recovers_where_it_went() {
    for scenario in &SCENARIOS {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("space");
        let effect = prepare(scenario, &root);
        let mut steps = Vec::new();
        apply_one(&bind(&root), &effect, &mut |step| steps.push(step)).unwrap();
        let done_from = scenario
            .done_from
            .map(|from| steps.iter().position(|step| *step == from).unwrap());

        for (index, &step) in steps.iter().enumerate() {
            let context = format!("{} moved at {step:?}", scenario.name);
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("space");
            let moved = dir.path().join("moved");
            let effect = prepare(scenario, &root);
            let result = apply_one(&bind(&root), &effect, &mut |reached| {
                if reached == step {
                    withdraw(&root, &moved);
                }
            });
            match result {
                Err(err) => assert!(withdrawn(&err), "{context}: {err}"),
                Ok(outcome) => assert_eq!(index, steps.len() - 1, "{context}: {outcome:?}"),
            }
            assert!(listing(&root).is_empty(), "{context}");

            let outcome = recover_one(&bind(&moved), &effect, &mut |_| {}).unwrap();
            assert_eq!(
                matches!(outcome, Outcome::Done { .. }),
                done_from.is_some_and(|from| index >= from),
                "{context}: {outcome:?}"
            );
            (scenario.check)(&moved, &outcome);
            assert_settled(&moved);
            assert!(listing(&root).is_empty(), "{context}");
        }
    }
}

#[test]
fn space_changes_nothing_once_its_root_is_moved_away() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("space");
    let moved = dir.path().join("moved");
    let effect = replace(&root);
    let space = bind(&root);
    assert!(space.held().unwrap());
    withdraw(&root, &moved);
    assert!(!space.held().unwrap());

    let results = [
        apply_one(&space, &effect, &mut |_| {}).map(drop),
        recover_one(&space, &effect, &mut |_| {}).map(drop),
        space.stage("e2").map(drop),
        space.finish_staged(ID, true),
        space.clear_staging(None).map(drop),
        space.probe_no_replace().map(drop),
        space.reclaim_trash(now(), 0, &mut same).map(drop),
    ];
    for result in results {
        let err = result.unwrap_err();
        assert!(withdrawn(&err), "{err}");
    }
    assert!(listing(&root).is_empty());
    assert_eq!(read_marker(&moved).unwrap(), None);
    assert_eq!(private_names(&moved, STAGING_DIR), [ID]);
    assert_eq!(fs::read(moved.join("docs/report.txt")).unwrap(), SYNCED);

    let outcome = apply_one(&bind(&moved), &effect, &mut |_| {}).unwrap();
    assert_eq!(outcome, made(&moved, "docs/report.txt"));
    assert_settled(&moved);
    assert!(listing(&root).is_empty());
}
#[test]
fn recovery_of_a_removal_whose_file_the_user_moved_first_fails() {
    for scenario in ["remove_file", "remove_dir"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let effect = prepare(common::scenario(scenario), root);
        let (from, to) = match scenario {
            "remove_file" => ("docs/report.txt", "docs/moved.txt"),
            _ => ("docs/empty", "docs/moved"),
        };
        fs::rename(root.join(from), root.join(to)).unwrap();
        let outcome = recover_one(&bind(root), &effect, &mut |_| {}).unwrap();
        assert_eq!(outcome, failed(Reason::Missing), "{scenario}");
        assert!(root.join(to).exists(), "{scenario}");
        assert!(trashed(root).is_empty(), "{scenario}");
    }
}

#[test]
fn recovery_of_a_replacement_whose_target_the_user_changed_is_overtaken() {
    let remove: fn(&Path) = |file| fs::remove_file(file).unwrap();
    let write: fn(&Path) = |file| fs::write(file, EDITED).unwrap();
    let cases = [
        (Step::Quarantined, write),
        (Step::Placed, remove),
        (Step::Placed, write),
        (Step::Trashed, remove),
        (Step::Trashed, write),
    ];
    for (kill, change) in cases {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let effect = replace(root);
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            apply(root, &effect, |step| {
                if step == kill {
                    panic!("crash");
                }
            })
        }));
        assert!(crashed.is_err());
        change(&root.join("docs/report.txt"));
        let outcome = recover_one(&bind(root), &effect, &mut |_| {}).unwrap();
        assert_eq!(outcome, failed(Reason::Overtaken), "{kill:?}");
        assert_eq!(trashed(root), [SYNCED], "{kill:?}");
        assert_settled(root);
    }
}

#[test]
fn recovery_of_a_replacement_whose_name_was_taken_before_a_crash_after_the_trash_is_overtaken() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let effect = replace(root);
    let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        apply(root, &effect, |step| match step {
            Step::Compared => fs::write(root.join("docs/report.txt"), EDITED).unwrap(),
            Step::Trashed => panic!("crash"),
            _ => {}
        })
    }));
    assert!(crashed.is_err());
    let outcome = recover_one(&bind(root), &effect, &mut |_| {}).unwrap();
    assert_eq!(outcome, failed(Reason::Overtaken));
    assert_eq!(fs::read(root.join("docs/report.txt")).unwrap(), EDITED);
    assert_eq!(trashed(root), [SYNCED]);
    assert_settled(root);
}

#[test]
fn an_effect_the_system_does_not_allow_fails_and_changes_nothing() {
    use std::os::unix::fs::PermissionsExt;

    if rustix::process::geteuid().is_root() {
        return;
    }
    for (scenario, mode) in SCENARIOS
        .iter()
        .flat_map(|scenario| [(scenario, 0o555), (scenario, 0o400), (scenario, 0o000)])
    {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let effect = prepare(scenario, root);
        let docs = root.join("docs");
        let before = listing(&docs);
        fs::set_permissions(&docs, fs::Permissions::from_mode(mode)).unwrap();
        let outcome = apply(root, &effect, |_| {});
        fs::set_permissions(&docs, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            outcome,
            failed(Reason::Denied),
            "{} {mode:o}",
            scenario.name
        );
        assert_eq!(listing(&docs), before, "{} {mode:o}", scenario.name);
        assert_eq!(private_names(root, QUARANTINE_DIR), Vec::<String>::new());
    }
}

#[test]
fn a_permission_lost_after_the_first_change_fails_the_section_and_recovery_finishes_once_it_returns()
 {
    use std::os::unix::fs::PermissionsExt;

    if rustix::process::geteuid().is_root() {
        return;
    }
    let mode = |mode| fs::Permissions::from_mode(mode);
    for crashed in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let effect = replace(root);
        let space = bind(root);
        let docs = root.join("docs");
        let result = if crashed {
            let crash = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                apply_one(&space, &effect, &mut |step| {
                    assert_ne!(step, Step::Quarantined, "crash");
                })
            }));
            assert!(crash.is_err());
            fs::set_permissions(&docs, mode(0o000)).unwrap();
            recover_one(&space, &effect, &mut |_| {})
        } else {
            apply_one(&space, &effect, &mut |step| {
                if step == Step::Quarantined {
                    fs::set_permissions(&docs, mode(0o000)).unwrap();
                }
            })
        };
        fs::set_permissions(&docs, mode(0o755)).unwrap();
        let error = result.unwrap_err();
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::PermissionDenied,
            "crashed: {crashed}"
        );

        assert!(matches!(
            recover_one(&space, &effect, &mut |_| {}).unwrap(),
            Outcome::Done { .. }
        ));
        assert_eq!(fs::read(docs.join("report.txt")).unwrap(), REMOTE);
        assert_eq!(trashed(root), [SYNCED.to_vec()]);
    }
}
