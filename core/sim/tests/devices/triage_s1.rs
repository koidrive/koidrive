// SPDX-License-Identifier: Apache-2.0

use super::*;

fn holders(world: &World) -> Vec<Vec<String>> {
    (0..world.devices.len()).map(|d| paths_on(world, d)).collect()
}

fn phases(devices: usize, phases: &[Vec<(usize, Step)>]) -> World {
    let mut world = World::new(devices);
    world.trace = std::env::var_os("KOIDRIVE_SIM_TRACE").is_some();
    for (index, schedule) in phases.iter().enumerate() {
        if let Err(failure) = world.phase(index, schedule) {
            panic!("TRIAGE-FAIL phase {index}: {failure}");
        }
    }
    world
}

#[test]
fn s1_gun580_a_delete_beats_a_resent_rename_it_never_saw() {
    let mut world = converge(3, &[vec![(0, file("a"))], vec![(0, BACKUP), (0, rename(0, 0, "b"))]]).unwrap();
    let mut steps = vec![(0, RESTORE)];
    for device in [1, 2] {
        steps.push((device, Step::Connect));
        steps.extend([(device, DELIVER); 8]);
    }
    steps.extend([(2, delete(0)), (2, SCAN), (2, COMMIT)]);
    steps.extend([(1, COMMIT), (1, COMMIT)]);
    steps.push((2, COMMIT));
    world.phase(2, &steps).unwrap();
    assert!(no_device_holds_a_file(&world), "TRIAGE-REVIVED {:?}", holders(&world));
}

#[test]
fn s1_gun589_a_delete_planned_against_a_resend_the_server_then_loses() {
    let mut world = converge(2, &[vec![(0, file("a"))], vec![(0, BACKUP), (0, overwrite(0))]]).unwrap();
    let mut steps = vec![(0, RESTORE), (1, Step::Connect)];
    steps.extend([(1, DELIVER); 8]);
    steps.push((0, Step::Connect));
    steps.extend([(0, DELIVER); 8]);
    steps.push((0, BACKUP));
    steps.extend([(0, delete(0)), (0, SCAN)]);
    steps.extend([(1, COMMIT), (1, COMMIT)]);
    steps.extend([(0, DELIVER); 8]);
    steps.extend([(1, DELIVER); 8]);
    steps.push((0, RESTORE));
    steps.push((0, Step::Connect));
    steps.extend([(0, DELIVER); 8]);
    steps.extend(commit(0));
    steps.push((1, Step::Connect));
    for _ in 0..2 {
        for d in [0, 1] {
            steps.extend([(d, DELIVER); 8]);
            steps.extend(commit(d));
        }
    }
    world.phase(2, &steps).unwrap();
    assert!(no_device_holds_a_file(&world), "TRIAGE-REVIVED {:?}", holders(&world));
}

#[test]
fn s1_gun581_a_delete_keeps_against_a_resend_learned_from_a_listing() {
    let mut world = converge(2, &[vec![(0, file("a"))], vec![(0, BACKUP), (0, overwrite(0))]]).unwrap();
    let mut steps = vec![(0, RESTORE), (1, Step::Connect)];
    steps.extend([(1, DELIVER); 8]);
    steps.push((0, Step::Connect));
    steps.extend([(0, DELIVER); 8]);
    steps.extend([(0, Step::Drop), (0, delete(0)), (0, SCAN)]);
    steps.extend([(1, COMMIT), (1, COMMIT)]);
    steps.extend([(1, DELIVER); 8]);
    world.run(2, &steps).unwrap();
    world.truncate_log();
    let mut steps = vec![(0, Step::Connect)];
    steps.extend([(0, DELIVER); 12]);
    steps.extend(commit(0));
    steps.extend([(1, DELIVER); 8]);
    world.phase(3, &steps).unwrap();
    assert!(world.seen.contains_key("listing replaces R"), "TRIAGE-NOLISTING");
    assert!(no_device_holds_a_file(&world), "TRIAGE-REVIVED {:?}", holders(&world));
}

#[test]
fn s1_gun692_second_loss_of_an_observers_resend() {
    let world = keep(2, &[
        vec![(0, folder("a"))],
        vec![(0, BACKUP), (0, user(0, 0, 1, "b")), (0, user(1, 0, 2, "c.txt"))],
        {
            let mut steps = vec![(0, RESTORE), (0, BACKUP), (1, Step::Connect)];
            steps.extend([(1, Step::Work(0)); 12]);
            steps
        },
        {
            let mut steps = vec![(0, RESTORE), (0, delete(0)), (0, SCAN), (0, Step::Connect)];
            steps.extend([(0, Step::Work(0)); 12]);
            steps.push((1, Step::Connect));
            steps.extend([(1, DELIVER); 8]);
            steps
        },
    ]);
    match world {
        Ok(world) => assert!(no_device_holds_a_file(&world), "TRIAGE-REVIVED {:?}", holders(&world)),
        Err(failure) => panic!("TRIAGE-FAIL {failure}"),
    }
}

#[test]
fn s1_gun550_a_createdir_brings_back_a_deleted_folder_without_a_restore() {
    let u = |kind, target, parent, name, text| Step::User(Action { kind, target, parent, name, text });
    phases(2, &[
        vec![(1, u(0, 0, 0, 5, 0)), (0, Step::Work(0)), (0, u(1, 0, 0, 0, 0))],
        vec![(1, u(1, 0, 312516678923509493, 0, 0)), (0, u(0, 0, 0, 0, 0)), (0, u(0, 0, 0, 0, 0)), (0, u(0, 0, 0, 0, 0)), (2, u(2, 0, 0, 0, 0)), (0, Step::Work(0)), (0, u(2, 0, 0, 0, 0)), (0, u(2, 0, 0, 0, 0)), (0, u(2, 0, 0, 0, 0))],
        vec![(1, u(0, 0, 10871575225237674698, 6, 0)), (2, u(4, 3068338248260445892, 0, 0, 0)), (1, Step::Work(0)), (1, Step::Work(0)), (1, Step::Commit(Reply::Served)), (1, u(0, 0, 8899275754376522441, 0, 0))],
    ]);
}

#[test]
fn s1_gun678_case_db6913261ff5() {
    let u = |kind, target, parent, name, text| Step::User(Action { kind, target, parent, name, text });
    use Step::*;
    let c = Commit(Reply::Served);
    phases(2, &[
        vec![(2, u(1, 0, 0, 2, 0)), (0, u(1, 0, 0, 3, 0)), (1, u(1, 0, 0, 0, 0)), (0, u(1, 0, 0, 4, 0)), (0, u(0, 0, 0, 0, 0)), (0, Backup)],
        vec![(0, Backup), (0, u(2, 0, 0, 0, 0)), (0, Scan), (0, c.clone()), (0, c.clone()), (0, u(2, 0, 0, 0, 0)), (0, u(0, 0, 2844692439453172141, 0, 0)), (0, Restore)],
        vec![(0, u(2, 0, 0, 0, 0)), (0, Scan), (0, c.clone()), (0, c.clone()), (0, Deliver(0)), (0, u(2, 0, 0, 0, 0)), (1, Work(0)), (0, u(4, 5916588535269142056, 0, 0, 0)), (2, Work(0)), (2, Work(0)), (0, Work(0)), (1, Work(8005512575619858474)), (0, Restore)],
    ]);
}

#[test]
fn s1_gun678_k544_a_lost_folder_is_not_created_again_inside_a_folder_deleted_while_it_waited() {
    let u = |kind, target, parent, name, text| Step::User(Action { kind, target, parent, name, text });
    phases(3, &[
        vec![(0, u(1, 0, 0, 4, 0)), (1, u(1, 0, 0, 0, 0)), (1, u(0, 0, 0, 3, 0))],
        vec![(0, Step::Backup), (2, u(0, 0, 0, 6, 0)), (2, u(0, 0, 1, 0, 0)), (2, u(3, 1809843479218423697, 3, 2, 0))],
        vec![(0, u(4, 23960249537131406, 7340898669396870823, 6, 2)), (0, Step::Restore), (1, Step::Connect), (1, Step::Work(11631325315051589583)), (1, COMMIT), (2, u(4, 6786950160668269581, 8740926674541383984, 4, 2))],
    ]);
}

#[test]
fn s1_gun678_k544_a_kept_child_of_a_lost_folder_stays_deleted_after_a_crash_mid_effect() {
    let u = |kind, target, parent, name, text| Step::User(Action { kind, target, parent, name, text });
    phases(2, &[
        vec![(1, u(1, 0, 0, 1, 0)), (0, u(1, 0, 0, 0, 0)), (1, u(1, 0, 0, 3, 0)), (1, u(1, 0, 0, 6, 0))],
        vec![(0, Step::Backup), (2, u(0, 0, 1, 4, 0)), (2, u(3, 10565563719318406587, 1, 3, 0)), (2, u(3, 8511544876117996999, 2, 1, 0))],
        vec![(0, u(4, 17295685281375005034, 6989454378278442494, 6, 2)), (0, Step::Restore), (1, Step::Connect), (1, Step::Work(6048236135531082776)), (1, CRASH)],
    ]);
}

#[test]
fn s1_gun678_k544_a_kept_child_of_a_lost_folder_stays_deleted_across_a_second_restore() {
    let u = |kind, target, parent, name, text| Step::User(Action { kind, target, parent, name, text });
    phases(3, &[
        vec![(1, u(0, 0, 0, 3, 0)), (1, u(1, 0, 0, 4, 0))],
        vec![(0, Step::Backup), (1, u(0, 0, 0, 2, 0)), (1, u(3, 11736929676757859842, 2, 5, 0))],
        vec![(1, u(4, 1080863262105148645, 1490760445408345681, 2, 2)), (0, Step::Restore), (2, Step::Connect), (2, Step::Work(17783959944508942500)), (0, Step::Restore), (2, Step::Work(16535215316277980988))],
    ]);
}

#[test]
fn s1_gun869_an_unsent_delete_across_a_restore_and_back() {
    let mut world = converge(2, &[vec![(0, BACKUP)], vec![(1, file("a"))]]).unwrap();
    let mut steps = vec![(0, BACKUP), (0, delete(0)), (0, SCAN), (0, Step::RestoreEarlier), (0, Step::Connect)];
    steps.extend([(0, DELIVER); 8]);
    steps.extend([(0, Step::Work(0)); 4]);
    steps.push((0, RESTORE));
    world.phase(2, &steps).unwrap();
    assert!(no_device_holds_a_file(&world), "TRIAGE-REVIVED {:?}", holders(&world));
}

#[test]
fn s1_gun868_an_unscanned_delete_forgotten_after_a_restore() {
    let mut world = converge(2, &[vec![(0, BACKUP)], vec![(1, file("a"))]]).unwrap();
    let mut steps = vec![(0, delete(0)), (0, RESTORE), (0, Step::Connect)];
    steps.extend([(0, DELIVER); 8]);
    steps.push((1, Step::Connect));
    steps.extend([(1, DELIVER); 8]);
    steps.extend(commit(1));
    steps.extend([(0, DELIVER); 8]);
    world.phase(2, &steps).unwrap();
    assert!(no_device_holds_a_file(&world), "TRIAGE-REVIVED {:?}", holders(&world));
}

#[test]
fn s1_gun862_a_delete_undone_by_a_resent_create_at_the_moved_position() {
    let mut world = converge(2, &[vec![(0, BACKUP)], vec![(0, file("a"))]]).unwrap();
    let mut steps = vec![(0, rename(0, 0, "A")), (0, SCAN), (0, COMMIT), (0, COMMIT)];
    steps.extend([(1, DELIVER); 8]);
    steps.extend([(1, Step::Work(0)); 6]);
    steps.extend([(0, delete(0)), (0, SCAN), (0, RESTORE)]);
    steps.push((1, Step::Connect));
    steps.extend([(1, DELIVER); 8]);
    steps.extend(commit(1));
    steps.push((0, Step::Connect));
    steps.extend([(0, DELIVER); 8]);
    world.phase(2, &steps).unwrap();
    assert!(no_device_holds_a_file(&world), "TRIAGE-REVIVED {:?}", holders(&world));
}

#[test]
fn s1_gun541_a_reverted_edit_of_a_file_another_device_deleted() {
    let mut world = converge(2, &[vec![(0, file("a"))]]).unwrap();
    let mut steps = vec![(0, act(2, 0, 0, "a", 1)), (0, SCAN)];
    steps.extend([(1, delete(0))]);
    steps.extend(commit(1));
    steps.extend([(0, DELIVER); 8]);
    steps.extend([(0, act(2, 0, 0, "a", 0)), (0, SCAN)]);
    steps.extend(commit(0));
    world.phase(1, &steps).unwrap();
    assert!(no_device_holds_a_file(&world), "TRIAGE-REVIVED {:?}", holders(&world));
}

fn s1_gun586(scan_first: bool) {
    let mut world = converge(2, &[vec![(0, file("a"))]]).unwrap();
    let mut steps = vec![(1, delete(0))];
    if scan_first {
        steps.push((1, SCAN));
    }
    steps.push((0, act(2, 0, 0, "a", 1)));
    steps.extend(commit(0));
    steps.push((0, act(2, 0, 0, "a", 0)));
    steps.extend(commit(0));
    steps.extend([(1, DELIVER); 8]);
    steps.extend(commit(1));
    world.phase(1, &steps).unwrap();
    assert!(every_device_holds_a_file(&world), "TRIAGE-EDIT-TRASHED {:?}", holders(&world));
}

#[test]
fn s1_gun586_a_delete_keeps_an_aba_edit_scan_first() {
    s1_gun586(true);
}

#[test]
fn s1_gun586_a_delete_keeps_an_aba_edit_records_first() {
    s1_gun586(false);
}

#[test]
fn s1_min767_shrunk() {
    use Cut::*;
    use Outcome::*;
    use Reply::*;
    use Step::*;
    phases(2, &vec![
    vec![
        (
            0,
            User(
                Action {
                    kind: 2,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
            ),
        ),
        (
            2,
            User(
                Action {
                    kind: 1,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
            ),
        ),
        (
            0,
            User(
                Action {
                    kind: 1,
                    target: 0,
                    parent: 0,
                    name: 2,
                    text: 0,
                },
            ),
        ),
        (
            0,
            User(
                Action {
                    kind: 3,
                    target: 2470589270640886874,
                    parent: 0,
                    name: 9,
                    text: 0,
                },
            ),
        ),
        (
            1,
            User(
                Action {
                    kind: 1,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
            ),
        ),
        (
            0,
            User(
                Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 2,
                    text: 0,
                },
            ),
        ),
        (
            1,
            User(
                Action {
                    kind: 3,
                    target: 0,
                    parent: 0,
                    name: 7,
                    text: 0,
                },
            ),
        ),
    ],
    vec![
        (
            0,
            User(
                Action {
                    kind: 1,
                    target: 0,
                    parent: 0,
                    name: 4,
                    text: 0,
                },
            ),
        ),
        (
            0,
            User(
                Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 2,
                    text: 0,
                },
            ),
        ),
        (
            0,
            User(
                Action {
                    kind: 3,
                    target: 861725947464577428,
                    parent: 0,
                    name: 3,
                    text: 0,
                },
            ),
        ),
        (
            1,
            User(
                Action {
                    kind: 3,
                    target: 1809117414448709468,
                    parent: 0,
                    name: 5,
                    text: 0,
                },
            ),
        ),
        (
            0,
            Work(
                0,
            ),
        ),
        (
            0,
            Work(
                0,
            ),
        ),
        (
            0,
            Work(
                2488612757522,
            ),
        ),
        (
            1,
            Work(
                10710087073685529507,
            ),
        ),
        (
            0,
            Work(
                12415476291504558141,
            ),
        ),
        (
            0,
            User(
                Action {
                    kind: 1,
                    target: 8977249278648222204,
                    parent: 14985898319401055985,
                    name: 1,
                    text: 2,
                },
            ),
        ),
        (
            0,
            User(
                Action {
                    kind: 2,
                    target: 10617578100218958860,
                    parent: 109209292269612126,
                    name: 1,
                    text: 3,
                },
            ),
        ),
        (
            2,
            Deliver(
                8471077902145496926,
            ),
        ),
        (
            0,
            Commit(
                NoChunks,
            ),
        ),
        (
            2,
            Backup,
        ),
    ],
    vec![
        (
            1,
            Work(
                11789011912308573104,
            ),
        ),
        (
            1,
            Effect(
                Run,
            ),
        ),
        (
            0,
            Restore,
        ),
        (
            1,
            Effect(
                Outcome::Crash(Placed,
                ),
            ),
        ),
        (
            1,
            User(
                Action {
                    kind: 3,
                    target: 11093896227811117673,
                    parent: 13838485866651207847,
                    name: 7,
                    text: 2,
                },
            ),
        ),
        (
            0,
            Connect,
        ),
        (
            1,
            User(
                Action {
                    kind: 2,
                    target: 11203304110497521370,
                    parent: 14259164623482894290,
                    name: 3,
                    text: 2,
                },
            ),
        ),
        (
            0,
            Work(
                5545155993445983427,
            ),
        ),
        (
            1,
            Work(
                5494100873526995747,
            ),
        ),
        (
            2,
            Backup,
        ),
        (
            0,
            Connect,
        ),
        (
            2,
            Work(
                7378848995949163628,
            ),
        ),
        (
            2,
            Deliver(
                12748348932081378684,
            ),
        ),
        (
            1,
            Commit(
                Served,
            ),
        ),
        (
            2,
            Work(
                17866253982854176093,
            ),
        ),
        (
            2,
            Connect,
        ),
        (
            2,
            Work(
                7349727060586564346,
            ),
        ),
        (
            2,
            Connect,
        ),
        (
            0,
            Work(
                16283421099575165932,
            ),
        ),
        (
            0,
            User(
                Action {
                    kind: 2,
                    target: 14337819998312701825,
                    parent: 8095654488242736789,
                    name: 8,
                    text: 3,
                },
            ),
        ),
        (
            0,
            Effect(
                Run,
            ),
        ),
        (
            0,
            Connect,
        ),
        (
            2,
            Effect(
                Run,
            ),
        ),
        (
            0,
            Backup,
        ),
        (
            0,
            Scan,
        ),
        (
            2,
            User(
                Action {
                    kind: 4,
                    target: 15883777944504750755,
                    parent: 14444554943983268693,
                    name: 1,
                    text: 2,
                },
            ),
        ),
        (
            1,
            Restore,
        ),
        (
            2,
            Work(
                6267074935625447998,
            ),
        ),
        (
            0,
            Work(
                9842791980092395285,
            ),
        ),
        (
            0,
            User(
                Action {
                    kind: 2,
                    target: 15253874012712831777,
                    parent: 1558576738387900523,
                    name: 9,
                    text: 2,
                },
            ),
        ),
        (
            0,
            Work(
                9644443201055046124,
            ),
        ),
        (
            2,
            Commit(
                Served,
            ),
        ),
        (
            0,
            Effect(
                Run,
            ),
        ),
        (
            1,
            Backup,
        ),
        (
            0,
            Scan,
        ),
    ],
]);
}

#[test]
fn s1_min917_shrunk() {
    use Cut::*;
    use Outcome::*;
    use Reply::*;
    use Step::*;
    phases(3, &vec![
    vec![
        (
            1,
            User(
                Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
            ),
        ),
        (
            1,
            User(
                Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 2,
                    text: 0,
                },
            ),
        ),
        (
            1,
            User(
                Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 3,
                    text: 0,
                },
            ),
        ),
        (
            0,
            User(
                Action {
                    kind: 1,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
            ),
        ),
        (
            0,
            User(
                Action {
                    kind: 1,
                    target: 0,
                    parent: 0,
                    name: 1,
                    text: 0,
                },
            ),
        ),
    ],
    vec![
        (
            0,
            Backup,
        ),
        (
            1,
            User(
                Action {
                    kind: 2,
                    target: 12825638307164173158,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
            ),
        ),
    ],
    vec![
        (
            0,
            Restore,
        ),
        (
            1,
            User(
                Action {
                    kind: 1,
                    target: 0,
                    parent: 29226930420878976,
                    name: 4,
                    text: 0,
                },
            ),
        ),
        (
            2,
            Connect,
        ),
        (
            2,
            Work(
                0,
            ),
        ),
        (
            2,
            Commit(
                Served,
            ),
        ),
        (
            2,
            Work(
                9005262372980157177,
            ),
        ),
        (
            2,
            Work(
                1165962725008932860,
            ),
        ),
        (
            1,
            User(
                Action {
                    kind: 3,
                    target: 16404949188246174543,
                    parent: 16571246819194558495,
                    name: 0,
                    text: 0,
                },
            ),
        ),
        (
            1,
            User(
                Action {
                    kind: 4,
                    target: 9520376179847244920,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
            ),
        ),
        (
            0,
            Restore,
        ),
    ],
]);
}

#[test]
fn s1_min755_shrunk() {
    use Cut::*;
    use Outcome::*;
    use Reply::*;
    use Step::*;
    phases(3, &vec![
    vec![
        (
            0,
            User(
                Action {
                    kind: 1,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
            ),
        ),
    ],
    vec![
        (
            0,
            User(
                Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 1,
                    text: 0,
                },
            ),
        ),
        (
            0,
            User(
                Action {
                    kind: 3,
                    target: 0,
                    parent: 1,
                    name: 0,
                    text: 0,
                },
            ),
        ),
        (
            0,
            Work(
                0,
            ),
        ),
        (
            0,
            Work(
                0,
            ),
        ),
        (
            0,
            Work(
                0,
            ),
        ),
        (
            0,
            Work(
                0,
            ),
        ),
        (
            0,
            Work(
                0,
            ),
        ),
        (
            1,
            Work(
                0,
            ),
        ),
        (
            1,
            Work(
                0,
            ),
        ),
        (
            1,
            Work(
                0,
            ),
        ),
        (
            1,
            Work(
                0,
            ),
        ),
        (
            1,
            Work(
                0,
            ),
        ),
        (
            0,
            Commit(
                Refused(
                    error::Code::RetryLater,
                ),
            ),
        ),
        (
            1,
            User(
                Action {
                    kind: 4,
                    target: 1,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
            ),
        ),
        (
            1,
            Scan,
        ),
        (
            1,
            Commit(
                Served,
            ),
        ),
        (
            1,
            Commit(
                Served,
            ),
        ),
        (
            1,
            Deliver(
                0,
            ),
        ),
        (
            0,
            User(
                Action {
                    kind: 1,
                    target: 0,
                    parent: 1,
                    name: 2,
                    text: 0,
                },
            ),
        ),
        (
            0,
            Scan,
        ),
        (
            0,
            Commit(
                Served,
            ),
        ),
        (
            0,
            Commit(
                Served,
            ),
        ),
        (
            0,
            Deliver(
                0,
            ),
        ),
        (
            1,
            User(
                Action {
                    kind: 4,
                    target: 1,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
            ),
        ),
        (
            2,
            User(
                Action {
                    kind: 4,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
            ),
        ),
        (
            2,
            Deliver(
                3568239211836307098,
            ),
        ),
        (
            0,
            User(
                Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
            ),
        ),
        (
            2,
            Work(
                14002780067840989677,
            ),
        ),
        (
            2,
            Work(
                1147192969390414053,
            ),
        ),
        (
            0,
            User(
                Action {
                    kind: 0,
                    target: 0,
                    parent: 0,
                    name: 0,
                    text: 0,
                },
            ),
        ),
        (
            2,
            User(
                Action {
                    kind: 0,
                    target: 0,
                    parent: 1192255131271792001,
                    name: 2,
                    text: 0,
                },
            ),
        ),
        (
            2,
            Work(
                6036150968848666325,
            ),
        ),
        (
            2,
            Work(
                1548992151789521797,
            ),
        ),
        (
            2,
            Work(
                346916552495723481,
            ),
        ),
    ],
]);
}
