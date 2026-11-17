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
fn a_rolled_back_store_enters_recovery_when_the_server_holds_another_commit_at_a_lower_slot() {
    converge(
        2,
        &[
            vec![
                (0, Step::Work(4896410705339688140)),
                (2, Step::Work(16723344530533104394)),
                (0, Step::Connect),
                (2, Step::Work(8021961896345153970)),
                (
                    0,
                    Step::User(Action {
                        kind: 3,
                        target: 12672309778009316114,
                        parent: 15710750899980576251,
                        name: 8,
                        text: 0,
                    }),
                ),
                (0, Step::Effect(Outcome::Crash(Cut::Placed))),
                (1, Step::Work(14067493017841771415)),
                (1, Step::Effect(Outcome::Run)),
                (1, Step::Answer(1)),
                (1, Step::Backup),
                (
                    0,
                    Step::User(Action {
                        kind: 0,
                        target: 11840876125649253605,
                        parent: 11727201081695032578,
                        name: 9,
                        text: 1,
                    }),
                ),
                (2, Step::RollBack),
                (2, Step::Deliver(7506214105150539357)),
                (0, Step::Effect(Outcome::Run)),
                (2, Step::Effect(Outcome::Run)),
                (2, Step::Crash),
                (2, Step::Work(7596533688908269851)),
                (0, Step::Commit(Reply::Served)),
                (
                    1,
                    Step::User(Action {
                        kind: 0,
                        target: 10990663142708786751,
                        parent: 11911756597118007744,
                        name: 7,
                        text: 2,
                    }),
                ),
                (0, Step::Commit(Reply::Served)),
                (1, Step::Effect(Outcome::Run)),
                (2, Step::Answer(1)),
                (1, Step::Commit(Reply::NoChunks)),
                (0, Step::Commit(Reply::Served)),
                (
                    1,
                    Step::User(Action {
                        kind: 2,
                        target: 7623571027611869407,
                        parent: 1015568459825341129,
                        name: 6,
                        text: 1,
                    }),
                ),
            ],
            vec![
                (0, Step::Work(11558010647325126483)),
                (1, Step::Commit(Reply::NoChunks)),
                (1, Step::Work(3069760823068194535)),
                (1, Step::Work(16910980275605317758)),
                (0, Step::Work(5310992995587820303)),
                (2, Step::Effect(Outcome::Fail)),
                (
                    0,
                    Step::User(Action {
                        kind: 1,
                        target: 4347118488695239924,
                        parent: 15363861802225637572,
                        name: 4,
                        text: 2,
                    }),
                ),
                (1, Step::Effect(Outcome::Crash(Cut::Done))),
                (1, Step::Commit(Reply::Served)),
                (2, Step::Deliver(7003363735828417436)),
                (
                    0,
                    Step::User(Action {
                        kind: 2,
                        target: 8074253639330051193,
                        parent: 6366525019672751454,
                        name: 2,
                        text: 1,
                    }),
                ),
                (2, Step::Deliver(8971130056825930032)),
                (1, Step::Rewind),
                (0, Step::Deliver(3219418458128426578)),
                (0, Step::Deliver(13834753699756444731)),
                (1, Step::Connect),
                (
                    1,
                    Step::User(Action {
                        kind: 2,
                        target: 9684134736789371525,
                        parent: 9116313826053761333,
                        name: 2,
                        text: 1,
                    }),
                ),
                (2, Step::Backup),
                (1, Step::Backup),
                (1, Step::RestoreEarlier),
                (2, Step::Drop),
                (0, Step::Effect(Outcome::Run)),
                (0, Step::Work(12545684805627025843)),
                (
                    2,
                    Step::User(Action {
                        kind: 1,
                        target: 2385180695903006205,
                        parent: 8002716011930771496,
                        name: 7,
                        text: 3,
                    }),
                ),
                (2, Step::Commit(Reply::Served)),
                (0, Step::Commit(Reply::Served)),
                (
                    0,
                    Step::User(Action {
                        kind: 0,
                        target: 1870029211797109911,
                        parent: 10775035017233410327,
                        name: 2,
                        text: 2,
                    }),
                ),
                (1, Step::Deliver(5554162087094715283)),
                (
                    2,
                    Step::User(Action {
                        kind: 2,
                        target: 15222228763714933820,
                        parent: 992343231151758404,
                        name: 4,
                        text: 1,
                    }),
                ),
                (0, Step::Connect),
                (0, Step::Work(6041645900581227996)),
                (0, Step::Work(794959316963855413)),
                (2, Step::RestoreEarlier),
            ],
            vec![
                (1, Step::Scan),
                (2, Step::Connect),
                (2, Step::Work(2462378688506646910)),
                (
                    1,
                    Step::User(Action {
                        kind: 3,
                        target: 1186758896852520370,
                        parent: 16429227052596154916,
                        name: 1,
                        text: 2,
                    }),
                ),
                (0, Step::Effect(Outcome::Run)),
                (1, Step::Work(7488472420260230664)),
                (2, Step::Drop),
                (0, Step::Restore),
                (0, Step::Work(5100621079349363707)),
                (2, Step::Answer(0)),
                (1, Step::Deliver(14292014730997073799)),
                (2, Step::CopyStore),
                (1, Step::Duplicate(14872754043242171117)),
                (2, Step::Commit(Reply::Refused(error::Code::LimitExceeded))),
                (2, Step::Work(9582228424229349453)),
                (2, Step::Work(16115158366662587086)),
                (
                    2,
                    Step::User(Action {
                        kind: 0,
                        target: 8102596371803064856,
                        parent: 13269208307977719536,
                        name: 0,
                        text: 2,
                    }),
                ),
                (
                    1,
                    Step::User(Action {
                        kind: 2,
                        target: 17653091613941332331,
                        parent: 1402512529475154382,
                        name: 7,
                        text: 3,
                    }),
                ),
                (
                    2,
                    Step::User(Action {
                        kind: 1,
                        target: 6284144589003716724,
                        parent: 2189881988461225779,
                        name: 3,
                        text: 1,
                    }),
                ),
                (2, Step::Restore),
                (2, Step::Connect),
                (0, Step::Deliver(11041050558712433527)),
                (0, Step::Work(6490795292745064656)),
                (1, Step::Commit(Reply::Served)),
                (2, Step::Work(12525559075351750035)),
                (1, Step::Effect(Outcome::Fail)),
                (0, Step::Work(10862751031706064105)),
                (2, Step::Work(2396962820806254357)),
                (0, Step::Scan),
                (2, Step::Connect),
                (2, Step::RollBack),
            ],
        ],
    )
    .unwrap();
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
