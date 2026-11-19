// SPDX-License-Identifier: Apache-2.0

use super::*;

fn device_0_reissues_slot_1(lost: &[(usize, Step)]) -> Result<World, Failure> {
    let mut world = converge(2, &[vec![(0, BACKUP), (0, file("a"))]])?;
    let mut b = vec![(0, BACKUP), (0, Step::RestoreEarlier), (1, Step::Connect)];
    b.extend([(1, Step::Work(0)); 16]);
    b.push((1, file("b")));
    b.extend(commit(1));
    b.extend([(1, Step::Work(0)); 8]);
    world.phase(1, &b)?;
    let mut edit = vec![(1, overwrite(0))];
    edit.extend(commit(1));
    world.phase(2, &edit)?;
    let mut edit = vec![(0, overwrite(1))];
    edit.extend(commit(0));
    world.phase(3, &edit)?;
    world.phase(4, &[(1, Step::Leave)])?;
    world.phase(5, lost)?;
    Ok(world)
}

#[test]
fn another_devices_edit_is_sent_again_when_a_restore_brings_back_another_commit_at_this_devices_slot()
 {
    device_0_reissues_slot_1(&[(0, RESTORE)]).unwrap();
}

#[test]
fn another_devices_edit_is_sent_again_when_a_second_restore_brings_back_another_commit_at_a_kept_slot()
 {
    device_0_reissues_slot_1(&[(0, Step::RestoreEarlier), (0, Step::Connect), (0, RESTORE)])
        .unwrap();
}

#[test]
fn a_store_copied_before_the_listing_takes_the_kept_slot_below_one_the_server_gave_out_again_as_held()
 {
    let mut world = converge(
        2,
        &[vec![(0, file("a"))], vec![(0, BACKUP), (0, file("b"))]],
    )
    .unwrap();
    world
        .phase(2, &[(0, RESTORE), (0, Step::Connect), (0, Step::CopyStore)])
        .unwrap();
    let copy = &world.devices[0].store_copy.as_ref().unwrap().commits;
    assert_eq!(copy.accepted(), 1);
    assert!(copy.made_here(2));
    let shared = copy.state().slots[&1].seq;
    let live = &world.devices[0].engine.commits;
    assert_eq!(live.accepted(), 2);
    world
        .run(
            3,
            &[(0, Step::RollBack), (0, Step::Connect), (0, Step::Connect)],
        )
        .unwrap();
    let commits = &world.devices[0].engine.commits;
    assert_eq!(commits.accepted(), 2);
    assert_eq!(commits.held_through(), shared);
    world.phase(4, &[]).unwrap();
}

#[test]
fn a_store_copied_before_the_listing_starts_no_recovery_when_a_restore_brings_back_the_resend_at_its_kept_slot()
 {
    let mut world = converge(
        2,
        &[vec![(0, file("a"))], vec![(0, BACKUP), (0, file("b"))]],
    )
    .unwrap();
    world
        .phase(2, &[(0, RESTORE), (0, Step::Connect), (0, Step::CopyStore)])
        .unwrap();
    world
        .phase(3, &[(0, BACKUP), (0, Step::RollBack), (0, Step::Connect)])
        .unwrap();
    assert_eq!(world.devices[0].engine.commits.accepted(), 2);
    let mut lost = vec![(0, file("c.txt"))];
    lost.extend(commit(0));
    world.run(4, &lost).unwrap();
    assert_eq!(world.devices[0].engine.commits.accepted(), 3);
    world.phase(5, &[(0, RESTORE), (0, Step::Connect)]).unwrap();
}

#[test]
fn a_store_rolled_back_past_a_kept_slot_starts_recovery_when_a_later_copy_committed_another_commit_there()
 {
    let mut world = converge(
        2,
        &[vec![(0, file("a"))], vec![(0, BACKUP), (0, file("b"))]],
    )
    .unwrap();
    world
        .phase(2, &[(0, RESTORE), (0, Step::Connect), (0, Step::CopyStore)])
        .unwrap();
    world
        .phase(3, &[(0, BACKUP), (0, Step::RollBack), (0, Step::Connect)])
        .unwrap();
    world
        .phase(
            4,
            &[
                (0, Step::CopyStore),
                (0, Step::RestoreEarlier),
                (0, Step::Connect),
                (0, file("c.txt")),
            ],
        )
        .unwrap();
    world
        .run(
            5,
            &[(0, Step::RollBack), (0, Step::Connect), (0, Step::Connect)],
        )
        .unwrap();
    assert!(matches!(
        world.devices[0].engine.commits.halted(),
        Some(koidrive_engine::commit::Halt::Recovery {
            cause: koidrive_engine::question::RecoveryCause {
                trigger: koidrive_engine::question::RecoveryTrigger::StoreRolledBack,
                ..
            },
            ..
        })
    ));
    world.phase(6, &[]).unwrap();
}

#[test]
fn a_store_copied_before_the_listing_takes_another_devices_delete_of_a_file_it_edited_before_the_loss()
 {
    let mut world = converge(2, &[vec![(0, file("a"))], vec![(0, overwrite(0))]]).unwrap();
    world.phase(2, &[(0, BACKUP), (0, file("b"))]).unwrap();
    let mut lost = vec![(0, RESTORE), (0, Step::Connect), (0, Step::CopyStore)];
    lost.extend([(0, Step::Work(0)); 40]);
    lost.push((1, Step::Connect));
    lost.extend([(1, Step::Work(0)); 16]);
    lost.push((1, delete(0)));
    lost.extend(commit(1));
    lost.extend([(1, Step::Work(0)); 8]);
    world.run(3, &lost).unwrap();
    assert_eq!(world.devices[0].engine.commits.accepted(), 3);
    world
        .run(
            4,
            &[(0, Step::RollBack), (0, Step::Connect), (0, Step::Connect)],
        )
        .unwrap();
    assert_eq!(world.devices[0].engine.commits.accepted(), 3);
    world.phase(5, &[]).unwrap();
}

#[test]
fn the_restore_cases_hold_when_the_server_sends_no_commit_token() {
    TOKENLESS_WELCOME.set(true);
    a_file_another_device_edits_and_deletes_across_a_restore_stays_deleted();
    a_kept_child_of_a_lost_folder_stays_deleted_after_a_crash_mid_effect();
    a_lost_folder_is_not_created_again_inside_a_folder_deleted_while_it_waited();
    a_later_edit_made_over_a_kept_write_does_not_need_a_recovery_copy();
    a_lost_move_and_edit_sent_again_after_a_restore_keep_the_write_in_one_file();
}

#[test]
#[ignore = "GUN-1012: one slot token cannot tell a commit that replaced this store's lost commit from one a rolled-back copy never saw, so the copy sends its edit again over a later delete (RFC 0029)"]
fn a_rolled_back_copy_sends_no_edit_again_that_the_server_holds_through_its_resend() {
    let u = |kind| {
        Step::User(Action {
            kind,
            target: 0,
            parent: 0,
            name: 0,
            text: 0,
        })
    };
    converge(
        2,
        &[
            vec![(0, u(1)), (0, Step::Backup)],
            vec![(1, u(2))],
            vec![
                (0, Step::Restore),
                (1, Step::Connect),
                (1, Step::CopyStore),
                (1, Step::Work(0)),
                (1, Step::Commit(Reply::Served)),
                (1, Step::Work(4555966414081959077)),
                (1, Step::RollBack),
                (0, u(0)),
                (0, u(4)),
            ],
        ],
    )
    .unwrap();
}

#[test]
fn a_reapplied_commit_does_not_hold_another_devices_edit_from_the_history_a_restore_left() {
    let _red = expect_red(
        Check::OneWriteOneNode,
        "write 3 is in 2 live nodes on the server: [[\"a\"], [\"a (conflict from device-0 2026-11-09)\"]]",
    );
    let mut world = converge(2, &[vec![(0, file("a")), (1, file("b"))]]).unwrap();
    let mut s = vec![
        (0, BACKUP),
        (0, overwrite(0)),
        (0, SCAN),
        (0, COMMIT),
        (0, COMMIT),
        (0, Step::Drop),
        (0, BACKUP),
        (0, Step::RestoreEarlier),
        (1, Step::Connect),
    ];
    s.extend([(1, Step::Work(0)); 16]);
    s.push((1, overwrite(1)));
    s.extend(commit(1));
    s.extend([(1, Step::Work(0)); 8]);
    s.push((0, Step::Connect));
    s.extend([(0, Step::Work(0)); 24]);
    world.run(1, &s).unwrap();
    world
        .run(2, &[(1, Step::Leave), (0, RESTORE), (0, Step::Connect)])
        .unwrap();
    world.phase(3, &[]).unwrap();
}

#[test]
fn a_rolled_back_store_enters_recovery_when_a_commit_after_the_restore_took_its_slot() {
    converge(
        2,
        &[
            vec![
                (2, Step::Work(12267718578518239834)),
                (0, Step::Connect),
                (1, Step::Connect),
                (2, Step::Work(11453646214662844128)),
                (
                    2,
                    Step::User(Action {
                        kind: 0,
                        target: 8318797114919849338,
                        parent: 12249469183885329173,
                        name: 8,
                        text: 2,
                    }),
                ),
                (2, Step::Work(3514444316500576433)),
                (2, Step::Work(15298596865811664161)),
                (1, Step::Scan),
                (2, Step::CopyStore),
                (2, Step::Backup),
                (
                    2,
                    Step::User(Action {
                        kind: 1,
                        target: 11406035755885105539,
                        parent: 15052356312157459171,
                        name: 5,
                        text: 0,
                    }),
                ),
                (2, Step::Effect(Outcome::Crash(Cut::Quarantined))),
                (0, Step::Deliver(11627084299583293482)),
            ],
            vec![
                (2, Step::Connect),
                (1, Step::Connect),
                (2, Step::Work(10688083400774508753)),
                (1, Step::Restore),
                (2, Step::Commit(Reply::Served)),
                (0, Step::Commit(Reply::Served)),
                (2, Step::CopyStore),
                (1, Step::Backup),
                (
                    0,
                    Step::User(Action {
                        kind: 1,
                        target: 15056844090704155700,
                        parent: 5023520681873993632,
                        name: 0,
                        text: 1,
                    }),
                ),
                (0, Step::Work(7282969155948029946)),
                (0, Step::Commit(Reply::Served)),
                (0, Step::Connect),
                (
                    2,
                    Step::User(Action {
                        kind: 1,
                        target: 3992013969463396657,
                        parent: 6476948719336011986,
                        name: 1,
                        text: 3,
                    }),
                ),
                (0, Step::Scan),
                (0, Step::Commit(Reply::Served)),
                (0, Step::Connect),
                (1, Step::Commit(Reply::Refused(error::Code::RetryLater))),
                (
                    0,
                    Step::User(Action {
                        kind: 2,
                        target: 3749631163116680640,
                        parent: 17654841143953328153,
                        name: 9,
                        text: 3,
                    }),
                ),
                (2, Step::Deliver(7633056873519615465)),
                (2, Step::Commit(Reply::Served)),
                (2, Step::Work(9320922278982703197)),
                (1, Step::Deliver(15993184437109831250)),
                (0, Step::Connect),
                (
                    2,
                    Step::User(Action {
                        kind: 3,
                        target: 16968419168642968411,
                        parent: 18412138271449934375,
                        name: 9,
                        text: 1,
                    }),
                ),
                (
                    2,
                    Step::User(Action {
                        kind: 3,
                        target: 13417808454409653466,
                        parent: 12736507178025465839,
                        name: 1,
                        text: 1,
                    }),
                ),
                (0, Step::Crash),
                (0, Step::Effect(Outcome::Crash(Cut::Quarantined))),
                (0, Step::Scan),
                (
                    1,
                    Step::User(Action {
                        kind: 3,
                        target: 12599019507213326821,
                        parent: 472572499037889516,
                        name: 4,
                        text: 0,
                    }),
                ),
                (2, Step::Deliver(17166006867113384059)),
                (2, Step::RollBack),
                (2, Step::Connect),
                (1, Step::Scan),
                (1, Step::Effect(Outcome::Run)),
                (0, Step::Commit(Reply::Served)),
                (
                    2,
                    Step::User(Action {
                        kind: 3,
                        target: 15837818162164378096,
                        parent: 14091535599852482426,
                        name: 7,
                        text: 0,
                    }),
                ),
                (0, Step::Answer(1)),
                (1, Step::Work(8262024362581988907)),
            ],
            vec![],
        ],
    )
    .unwrap();
}

#[test]
fn a_commit_resent_across_a_restore_is_not_another_commit_when_a_later_restore_brings_back_its_first_application()
 {
    let act = |kind: u8, target: usize, parent: usize, name: usize| {
        Step::User(Action {
            kind,
            target,
            parent,
            name,
            text: 0,
        })
    };
    converge(
        2,
        &[
            vec![
                (2, act(0, 0, 0, 3)),
                (2, Step::Work(0)),
                (0, Step::Backup),
                (2, Step::Work(0)),
                (2, act(3, 0, 5495008244535768158, 0)),
                (0, act(0, 0, 0, 0)),
                (2, Step::Work(12012490443824171944)),
                (0, Step::Backup),
                (0, Step::RestoreEarlier),
            ],
            vec![(0, Step::Restore)],
        ],
    )
    .unwrap();
}

#[test]
#[ignore = "recovery by recorded intent (RFC 0029 §5.4) is not switched on"]
fn a_commit_applied_again_after_a_restore_and_brought_back_by_another_leaves_no_conflict_copy() {
    let mut world = converge(2, &[vec![(0, file("a"))]]).unwrap();
    world
        .run(
            1,
            &[
                (0, BACKUP),
                (0, file("b")),
                (0, SCAN),
                (0, COMMIT),
                (0, COMMIT),
                (0, Step::CopyStore),
                (0, BACKUP),
            ],
        )
        .unwrap();
    world.phase(2, &[(0, overwrite(1))]).unwrap();
    world.phase(3, &[(0, overwrite(0))]).unwrap();
    world
        .phase(4, &[(0, Step::RestoreEarlier), (0, Step::RollBack)])
        .unwrap();
    world.phase(5, &[(0, RESTORE)]).unwrap();
    let tags = |files: Vec<(String, Option<Content>)>| -> Vec<(String, Option<i64>)> {
        (files.into_iter())
            .map(|(name, content)| (name, content.as_ref().map(tag)))
            .collect()
    };
    let root = world.space().space_id().to_owned();
    let mut server: Vec<(String, Option<Content>)> = (world.space().nodes().into_iter())
        .filter(|node| node.node_id != root && node.status == NodeStatus::Live as i32)
        .map(|node| (node.name, node.current.and_then(|version| version.content)))
        .collect();
    server.sort_by(|a, b| a.0.cmp(&b.0));
    let expected = vec![("a".to_owned(), Some(4)), ("b".to_owned(), Some(3))];
    assert_eq!(tags(server), expected, "server");
    for device in 0..2 {
        assert_eq!(tags(files_on(&world, device)), expected, "device {device}");
    }
}
