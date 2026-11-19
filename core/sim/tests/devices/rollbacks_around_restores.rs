// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn a_store_copied_after_a_create_and_rolled_back_after_a_restore_converges() {
    use Step::*;
    converge(
        2,
        &[
            vec![(0, u(1, 0, 0, 0, 0)), (0, CopyStore), (0, Backup)],
            vec![(0, Restore), (0, RollBack)],
        ],
    )
    .unwrap();
}

#[test]
fn a_store_copied_before_a_create_and_rolled_back_after_a_restore_converges() {
    use Step::*;
    converge(
        2,
        &[
            vec![(0, CopyStore), (0, u(1, 0, 0, 0, 0)), (0, Backup)],
            vec![(0, Restore), (2, RollBack)],
        ],
    )
    .unwrap();
}

#[test]
fn a_store_copied_after_a_backup_and_rolled_back_after_a_restore_converges() {
    use Step::*;
    converge(
        3,
        &[
            vec![(0, Backup), (0, CopyStore), (0, u(1, 0, 0, 0, 0))],
            vec![(0, Restore), (0, RollBack)],
        ],
    )
    .unwrap();
}

#[test]
fn a_store_copied_before_a_backup_and_rolled_back_after_a_restore_converges() {
    use Step::*;
    converge(
        2,
        &[
            vec![
                (2, CopyStore),
                (0, Backup),
                (0, u(1, 0, 0, 0, 0)),
                (2, u(0, 0, 0, 2, 0)),
            ],
            vec![(0, Restore), (2, RollBack)],
        ],
    )
    .unwrap();
}

#[test]
fn a_store_copied_a_phase_before_a_create_and_rolled_back_after_a_restore_converges() {
    use Step::*;
    converge(
        2,
        &[
            vec![(0, CopyStore)],
            vec![(0, u(0, 0, 0, 2, 0)), (0, Backup), (2, u(1, 0, 0, 0, 0))],
            vec![(0, Restore), (2, RollBack)],
        ],
    )
    .unwrap();
}

#[test]
fn a_store_rolled_back_after_another_device_restores_and_makes_a_folder_converges() {
    use Step::*;
    converge(
        2,
        &[
            vec![(1, CopyStore), (1, u(1, 0, 0, 0, 0)), (0, Backup)],
            vec![(0, Restore), (0, u(0, 0, 0, 0, 0)), (1, RollBack)],
        ],
    )
    .unwrap();
}

#[test]
fn a_store_rolled_back_past_a_served_create_after_a_restore_converges() {
    use koidrive_sim::world::{Reply::*, Step::*};
    converge(
        3,
        &[
            vec![(2, CopyStore)],
            vec![
                (2, u(1, 0, 0, 0, 0)),
                (2, Work(0)),
                (2, Commit(Served)),
                (0, Backup),
                (2, Work(0)),
                (1, Work(0)),
                (0, Restore),
                (1, Work(0)),
                (2, RollBack),
            ],
        ],
    )
    .unwrap();
}

#[test]
fn a_store_rolled_back_past_a_create_before_a_restore_converges() {
    use Step::*;
    converge(
        3,
        &[
            vec![(0, CopyStore), (0, Backup)],
            vec![
                (0, u(1, 0, 0, 0, 0)),
                (0, Work(0)),
                (0, Work(0)),
                (0, Work(0)),
                (0, RollBack),
                (1, Work(0)),
                (0, Restore),
                (1, Work(0)),
            ],
        ],
    )
    .unwrap();
}

#[test]
fn a_store_rolled_back_before_a_restore_that_lost_a_create_converges() {
    use Step::*;
    converge(
        3,
        &[
            vec![(0, u(0, 0, 0, 0, 0))],
            vec![
                (2, u(0, 0, 0, 5, 0)),
                (2, u(0, 0, 0, 7, 0)),
                (2, u(0, 0, 50448361321329, 0, 0)),
                (2, CopyStore),
                (0, Backup),
                (2, u(1, 0, 0, 2, 0)),
                (0, Backup),
            ],
            vec![(2, RollBack), (0, Restore)],
        ],
    )
    .unwrap();
}

#[test]
fn a_store_rolled_back_after_a_restore_to_an_earlier_backup_converges() {
    use Step::*;
    converge(
        2,
        &[
            vec![
                (0, Backup),
                (0, u(0, 0, 0, 0, 0)),
                (0, u(0, 0, 0, 2, 0)),
                (2, u(0, 0, 2345804654425692805, 0, 0)),
            ],
            vec![
                (2, CopyStore),
                (0, Backup),
                (2, u(1, 0, 0, 3, 0)),
                (0, RestoreEarlier),
            ],
            vec![(2, RollBack)],
        ],
    )
    .unwrap();
}

#[test]
fn an_edit_inside_the_rollback_window_ends_as_one_file() {
    let mut world = converge(2, &[vec![(1, file("a"))]]).unwrap();
    world
        .phase(
            1,
            &[(0, Step::CopyStore), (0, overwrite(0)), (0, Step::RollBack)],
        )
        .unwrap();
    for device in 0..2 {
        let names: Vec<String> = (files_on(&world, device).into_iter())
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["a"], "device {device}");
    }
    assert_eq!(content_on(&world, 0, "a"), content_on(&world, 1, "a"));
}

#[test]
fn rewinds_and_rollbacks_around_a_backup_converge() {
    use koidrive_sim::world::Step::*;
    let u = drawn_with;
    let phases = vec![
        vec![
            (0, u(2, 0, 0, 0, 0)),
            (2, CopyStore),
            (0, Work(0)),
            (0, Work(0)),
            (0, RollBack),
            (1, u(0, 0, 0, 1, 0)),
            (0, RollBack),
            (0, u(2, 0, 0, 0, 0)),
        ],
        vec![
            (0, Work(0)),
            (0, Work(0)),
            (0, Backup),
            (0, u(2, 0, 0, 0, 0)),
            (0, RollBack),
            (0, u(2, 0, 0, 0, 0)),
            (0, Work(0)),
            (0, Work(0)),
            (0, Work(0)),
            (0, Work(0)),
            (0, u(0, 0, 4825147716793901084, 4, 0)),
            (2, u(0, 0, 1217712248255906526, 8, 0)),
            (0, Work(0)),
            (0, u(0, 0, 302259315049645841, 0, 0)),
            (0, u(0, 0, 0, 0, 0)),
            (2, u(1, 0, 13300269512425867482, 2, 0)),
            (2, u(0, 0, 4394716142236277855, 1, 0)),
            (2, u(1, 0, 14735933815287640514, 0, 0)),
            (2, u(0, 0, 0, 3, 0)),
            (2, RollBack),
            (2, u(2, 0, 0, 0, 0)),
        ],
        vec![
            (2, RollBack),
            (0, Rewind),
            (0, u(0, 0, 13681509702714634966, 2, 0)),
            (0, u(3, 524682883208758344, 3253836883127625536, 3, 0)),
            (1, u(0, 0, 107340232139505673, 0, 0)),
        ],
    ];
    converge(3, &phases).unwrap();
}

#[test]
fn a_crash_while_a_download_is_quarantined_keeps_writes_across_a_restore() {
    use koidrive_sim::world::{Reply::*, Step::*};
    keep(
        3,
        &[
            vec![(1, u(1, 0, 0, 0, 0)), (0, u(1, 0, 0, 2, 0))],
            vec![
                (1, u(2, 1206730729911617966, 0, 0, 0)),
                (1, Work(0)),
                (1, Work(0)),
                (0, Backup),
                (1, Commit(Served)),
                (1, Work(0)),
                (2, u(4, 12624898511826296137, 0, 0, 0)),
                (0, Work(0)),
                (0, Effect(Outcome::Crash(Cut::Quarantined))),
                (0, Restore),
            ],
        ],
    )
    .unwrap();
}

#[test]
fn restores_after_moves_and_edits_on_three_devices_converge() {
    let _red = expect_red(
        Check::OneWriteOneNode,
        "write 2 is in 2 live nodes on the server: [[\"A\"], [\"A (conflict from device-0 2026-11-09 2)\"]]",
    );
    use koidrive_sim::world::{Reply::*, Step::*};
    converge(
        3,
        &[
            vec![
                (1, u(1, 0, 0, 0, 0)),
                (1, Work(0)),
                (1, Commit(Served)),
                (1, Work(0)),
                (2, Deliver(0)),
                (1, Work(0)),
                (1, u(3, 0, 0, 1, 0)),
                (1, Work(0)),
                (0, Backup),
                (1, Work(0)),
                (0, u(0, 0, 0, 0, 0)),
                (2, u(0, 0, 0, 1, 0)),
                (0, u(0, 0, 3307894247439793688, 0, 0)),
                (2, Work(2654930495610263060)),
                (2, Work(0)),
                (1, u(2, 0, 0, 0, 0)),
                (1, Work(0)),
                (0, u(0, 0, 961904704637406560, 0, 0)),
                (2, Work(8041453028609285195)),
                (2, Work(7459037410794658530)),
                (0, Restore),
            ],
            vec![(0, Restore)],
        ],
    )
    .unwrap();
}

#[test]
fn restores_crashes_and_duplicated_messages_on_three_devices_converge() {
    use koidrive_sim::world::{Reply::*, Step::*};
    converge(
        3,
        &[
            vec![
                (0, Backup),
                (0, u(0, 0, 0, 2, 0)),
                (0, Work(0)),
                (0, u(1, 0, 0, 3, 0)),
                (0, u(2, 0, 0, 0, 0)),
            ],
            vec![
                (1, u(0, 0, 1285814101918428590, 0, 0)),
                (0, Restore),
                (0, Connect),
                (2, u(3, 8848640273642333149, 6382992032102656458, 0, 0)),
                (0, Work(0)),
                (0, Work(4000746636949428065)),
                (2, Connect),
                (2, Work(1648833308860000556)),
                (0, Commit(Served)),
                (0, u(2, 2483995871143172111, 9171623605514407635, 1, 0)),
                (2, Backup),
            ],
            vec![
                (1, Work(2993819066984428697)),
                (1, Scan),
                (0, Deliver(17276249781050633269)),
                (0, Deliver(17043816143112026969)),
                (2, Effect(Outcome::Run)),
                (2, Work(16160981802101898727)),
                (2, Work(10282781507765826793)),
                (0, u(1, 18392913543402149518, 11861810052478043431, 5, 3)),
                (2, Scan),
                (2, u(2, 9563148829197439984, 3721480683115858519, 4, 1)),
                (2, Effect(Outcome::Crash(Cut::Placed))),
                (1, Connect),
                (0, Effect(Outcome::Run)),
                (0, u(4, 5534979547166299467, 11483298059832606243, 6, 0)),
                (1, Duplicate(476988474317801881)),
                (0, Effect(Outcome::Crash(Cut::Placed))),
                (1, Scan),
                (0, Work(8603510855888096381)),
                (1, Work(12025954559901055639)),
                (0, Work(8419907464343443582)),
                (2, Crash),
                (1, Work(9463836557793324486)),
                (1, Restore),
                (2, Work(14992856410635154335)),
                (2, Deliver(5585018825101531003)),
                (0, Work(2410541318081646983)),
                (0, Work(10890549453080099804)),
                (1, Work(18061957845702363319)),
                (2, Connect),
                (2, Restore),
                (2, Work(75288745139796733)),
                (1, Work(16240715555806020138)),
                (1, u(0, 15137973004049328398, 16644388509644097824, 1, 2)),
                (0, u(2, 5923069940376990471, 17055329173100555307, 2, 3)),
                (1, Commit(Served)),
                (1, Work(5197059063883068885)),
                (2, u(0, 16332341456421641696, 4935467471380997141, 1, 2)),
                (0, Effect(Outcome::Run)),
                (2, u(2, 4966629693323885985, 14589464943526988929, 1, 1)),
                (1, Crash),
            ],
        ],
    )
    .unwrap();
}

#[test]
#[ignore = "a delete comes undone across restores, crashes and a store rollback (GUN-664)"]
fn a_delete_stays_across_restores_crashes_and_a_rollback() {
    use koidrive_sim::world::{Reply::*, Step::*};
    converge(
        2,
        &[
            vec![
                (0, u(1, 0, 0, 0, 0)),
                (0, Work(0)),
                (2, u(1, 0, 0, 1, 0)),
                (2, Work(226769641356761788)),
                (0, Work(154764964883893176)),
                (0, Backup),
            ],
            vec![
                (0, Restore),
                (0, u(0, 0, 0, 0, 0)),
                (2, u(2, 9248250813484896738, 0, 0, 0)),
                (0, Work(0)),
                (2, Connect),
                (2, Work(0)),
                (2, Commit(Served)),
                (0, Work(334267421000585101)),
                (2, Crash),
                (2, u(4, 17001073352406634806, 5552969616310016050, 6, 0)),
            ],
            vec![
                (0, Work(6066336003454004995)),
                (0, Work(2157439711314118186)),
                (1, Restore),
                (2, Commit(Served)),
                (2, Backup),
                (0, Deliver(18432019605995179659)),
                (0, u(1, 7204822777281988933, 10483849714204012889, 0, 3)),
                (2, Work(12436860910610899253)),
                (0, Commit(Served)),
                (2, Effect(Outcome::Run)),
                (0, Commit(Served)),
                (0, Deliver(6773614427085039713)),
                (0, Deliver(6896852882504849322)),
                (0, Commit(Served)),
                (1, Effect(Outcome::Crash(Cut::Quarantined))),
                (2, Commit(Served)),
                (2, Effect(Outcome::Run)),
                (1, Work(10674601498695086820)),
                (0, Scan),
                (1, u(4, 2914501090887244250, 11582849181724836164, 4, 2)),
                (0, Work(12531001567461460175)),
                (0, Work(13659522318414733105)),
                (1, Work(3384626037290750696)),
                (1, u(2, 18019003530138293784, 983002806470690905, 5, 0)),
                (0, Work(12578335883886218491)),
                (1, Work(10443429435274116226)),
                (2, Deliver(17144292681594891497)),
                (1, u(1, 6471515815065853230, 19625063437672755, 5, 3)),
                (1, u(4, 3666823056793280571, 14225806117082265611, 1, 0)),
                (1, u(3, 11931321251074486125, 9235597965952925308, 0, 2)),
                (2, u(2, 9821567200189906265, 14228924832452645471, 4, 2)),
                (1, Backup),
                (2, Work(17613774861465103757)),
            ],
        ],
    )
    .unwrap();
}

#[test]
fn an_idle_device_does_not_send_a_place_it_never_set_after_a_restore() {
    let phases = vec![
        vec![
            (0, Step::Work(0)),
            (2, u(0, 0, 0, 0, 0)),
            (0, u(3, 0, 17891006627277596936, 6, 0)),
            (2, u(1, 0, 3427849071611093531, 2, 0)),
            (0, u(1, 0, 0, 0, 0)),
            (0, Step::Backup),
        ],
        vec![
            (2, u(2, 1651021180859060308, 0, 0, 0)),
            (0, Step::Work(0)),
            (0, Step::Commit(Reply::Served)),
            (0, u(1, 0, 3929345920089210451, 0, 0)),
            (0, u(2, 0, 0, 0, 0)),
            (2, u(0, 0, 0, 1, 0)),
            (0, Step::Restore),
            (0, u(0, 0, 7622918290071365507, 0, 0)),
            (0, Step::Work(0)),
            (0, u(4, 2023099079856833368, 0, 0, 0)),
            (2, u(0, 0, 6188481681091611974, 5, 0)),
        ],
    ];
    converge(2, &phases).unwrap();
}

#[test]
fn undoing_a_restore_is_not_taken_for_a_rolled_back_store() {
    let phases = vec![
        vec![(0, Step::Backup), (0, u(0, 0, 0, 0, 0))],
        vec![(0, Step::Backup)],
        vec![
            (0, Step::RestoreEarlier),
            (0, Step::Connect),
            (0, Step::Restore),
        ],
    ];
    converge(2, &phases).unwrap();
}

#[test]
fn a_restore_a_restore_to_an_earlier_backup_and_a_restore_again_after_overwrites_converge() {
    let settle = |restore: Step| {
        vec![
            (0, restore),
            (0, Step::Connect),
            (1, Step::Connect),
            (0, Step::Commit(Reply::Served)),
            (1, Step::Commit(Reply::Served)),
        ]
    };
    let phases = vec![
        vec![(1, u(1, 0, 0, 0, 0))],
        vec![(0, Step::Backup), (1, u(2, 0, 0, 0, 0))],
        vec![(0, Step::Backup), (0, u(2, 0, 0, 0, 0))],
        settle(Step::Restore),
        settle(Step::RestoreEarlier),
        settle(Step::Restore),
    ];
    converge(2, &phases).unwrap();
}
