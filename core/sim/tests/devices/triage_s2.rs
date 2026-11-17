// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn gun858_bystander_resend_takes_a_write_out() {
    let u = drawn_with;
    let phases: Vec<Vec<(usize, Step)>> = vec![
        vec![(2, u(1, 9030220727657072367, 11378016110811731764, 0, 1)), (1, Step::Backup)],
        vec![(2, Step::Backup), (1, u(2, 1195153182784767404, 16487922661405148831, 2, 0))],
        vec![(0, Step::RestoreEarlier), (2, Step::Restore)],
    ];
    converge(3, &phases).unwrap();
}

#[test]
fn gun717_recovery_copy_keeps_a_write_the_server_kept() {
    use koidrive_sim::world::{Reply, Step::*};
    let u = drawn_with;
    let mut first = vec![(0, Backup), (0, u(0, 0, 0, 2, 0)), (0, u(3, 0, 1, 0, 0)), (0, Scan), (0, Commit(Reply::Served)), (0, Commit(Reply::Served)), (0, Deliver(0))];
    first.extend([(1, Work(0)); 14]);
    first.extend([(0, Restore), (2, u(4, 0, 0, 0, 0)), (1, Connect), (0, u(3, 700175955643442024, 0, 3, 0))]);
    let phases = vec![
        vec![(0, u(1, 0, 0, 0, 0))],
        first,
        vec![(0, Restore), (1, u(2, 1996617580910179362, 0, 0, 0)), (1, Connect), (0, u(2, 8035601735646909626, 0, 0, 0)), (1, Work(129976861001331385)), (1, Work(0)), (1, Work(15422585896085069985))],
    ];
    converge(3, &phases).unwrap();
}

#[test]
fn gun716_a_rename_and_edit_the_holder_had_not_taken_leaves_one_file_after_a_second_loss() {
    let mut world = converge(2, &[vec![(0, BACKUP), (0, file("a"))]]).unwrap();
    let mut steps = vec![(0, RESTORE)];
    reconnect(&mut steps, 1, false);
    steps.extend([(0, rename(0, 0, "b")), (0, overwrite(0))]);
    reconnect(&mut steps, 0, true);
    steps.extend([(1, DELIVER); 6]);
    steps.extend([(1, Step::Effect(Outcome::Fail)), (0, RESTORE)]);
    reconnect(&mut steps, 1, true);
    reconnect(&mut steps, 0, true);
    steps.extend([(1, DELIVER); 4]);
    world.phase(9, &steps).unwrap();
    for device in 0..2 {
        let names: Vec<String> = (files_on(&world, device).into_iter()).map(|(name, _)| name).collect();
        assert_eq!(names, ["b"], "device {device}");
    }
}

#[test]
fn gun716_a_second_rename_and_edit_before_the_holder_resends_leaves_one_file() {
    let mut world = converge(2, &[vec![(0, BACKUP), (0, file("a"))]]).unwrap();
    let mut steps = vec![(0, RESTORE)];
    reconnect(&mut steps, 1, false);
    steps.push((0, rename(0, 0, "b")));
    reconnect(&mut steps, 0, true);
    steps.extend([(1, DELIVER); 6]);
    steps.extend([(1, Step::Effect(Outcome::Fail)), (0, RESTORE)]);
    steps.push((1, Step::Connect));
    steps.extend([(1, DELIVER); 8]);
    steps.extend([(0, rename(0, 0, "c.txt")), (0, overwrite(0))]);
    reconnect(&mut steps, 0, true);
    steps.extend([(1, DELIVER); 6]);
    steps.extend(commit(1));
    steps.extend([(1, DELIVER); 4]);
    steps.extend([(0, DELIVER); 4]);
    world.phase(9, &steps).unwrap();
    for device in 0..2 {
        let names: Vec<String> = (files_on(&world, device).into_iter()).map(|(name, _)| name).collect();
        assert_eq!(names, ["c.txt"], "device {device}");
    }
}

#[test]
fn gun716_mirrored_the_holder_edits_while_the_rename_is_untaken() {
    let mut world = converge(2, &[vec![(0, BACKUP), (0, file("a"))]]).unwrap();
    let mut steps = vec![(0, RESTORE)];
    reconnect(&mut steps, 1, false);
    steps.push((0, rename(0, 0, "b")));
    reconnect(&mut steps, 0, true);
    steps.extend([(1, DELIVER); 6]);
    steps.extend([(1, Step::Effect(Outcome::Fail)), (1, overwrite(0)), (0, RESTORE)]);
    reconnect(&mut steps, 1, true);
    reconnect(&mut steps, 0, true);
    steps.extend([(1, DELIVER); 4]);
    world.phase(9, &steps).unwrap();
    for device in 0..2 {
        let names: Vec<String> = (files_on(&world, device).into_iter()).map(|(name, _)| name).collect();
        assert_eq!(names, ["b"], "device {device}");
    }
}

#[test]
fn gun809_a_rewind_case_from_493_shard_23() {
    use koidrive_sim::world::Step::*;
    let u = drawn_with;
    let phases = vec![ vec![ (0, u(2, 0, 0, 0, 0)), (2, CopyStore), (0, Work(0)), (0, Work(0)), (0, RollBack), (1, u(0, 0, 0, 1, 0)), (0, RollBack), (0, u(2, 0, 0, 0, 0)), ], vec![ (0, Work(0)), (0, Work(0)), (0, Backup), (0, u(2, 0, 0, 0, 0)), (0, RollBack), (0, u(2, 0, 0, 0, 0)), (0, Work(0)), (0, Work(0)), (0, Work(0)), (0, Work(0)), (0, u(0, 0, 4825147716793901084, 4, 0)), (2, u(0, 0, 1217712248255906526, 8, 0)), (0, Work(0)), (0, u(0, 0, 302259315049645841, 0, 0)), (0, u(0, 0, 0, 0, 0)), (2, u(1, 0, 13300269512425867482, 2, 0)), (2, u(0, 0, 4394716142236277855, 1, 0)), (2, u(1, 0, 14735933815287640514, 0, 0)), (2, u(0, 0, 0, 3, 0)), (2, RollBack), (2, u(2, 0, 0, 0, 0)), ], vec![ (2, RollBack), (0, Rewind), (0, u(0, 0, 13681509702714634966, 2, 0)), (0, u(3, 524682883208758344, 3253836883127625536, 3, 0)), (1, u(0, 0, 107340232139505673, 0, 0)), ], ];
    converge(3, &phases).unwrap();
}

#[test]
fn gun669_observer_older_bytes_do_not_become_a_copy_beside_a_resent_edit() {
    let mut world = converge(2, &[vec![(0, folder("a"))], vec![(0, BACKUP), (0, user(0, 0, 1, "b")), (0, user(1, 0, 2, "c.txt"))]]).unwrap();
    world.run(2, &[(0, overwrite(2)), (0, SCAN), (0, COMMIT), (0, COMMIT), (0, DELIVER), (1, DELIVER), (1, DELIVER), (1, DELIVER)]).unwrap();
    world.phase(3, &[(0, SCAN), (0, RESTORE)]).unwrap();
    for device in 0..2 {
        let names: Vec<String> = (files_on(&world, device).into_iter()).map(|(name, _)| name).collect();
        assert!(!names.iter().any(|name| name.contains("conflict")), "device {device}: {names:?}");
    }
}

#[test]
fn gun599_a_lost_move_and_edit_of_one_file_end_as_one_file() {
    let mut world = converge(2, &[vec![(0, file("a"))], vec![(0, BACKUP)]]).unwrap();
    let mut steps = vec![(0, rename(0, 0, "b")), (0, overwrite(0))];
    steps.extend(commit(0));
    steps.extend([(1, DELIVER); 4]);
    steps.push((0, RESTORE));
    reconnect(&mut steps, 0, true);
    reconnect(&mut steps, 1, true);
    world.phase(9, &steps).unwrap();
    for device in 0..2 {
        let names: Vec<String> = (files_on(&world, device).into_iter()).map(|(name, _)| name).collect();
        assert_eq!(names, ["b"], "device {device}");
    }
}

#[test]
fn gun599_a_lost_move_and_edit_the_other_device_never_saw_end_as_one_file() {
    let mut world = converge(2, &[vec![(0, file("a"))], vec![(0, BACKUP)]]).unwrap();
    let mut steps = vec![(0, rename(0, 0, "b")), (0, overwrite(0))];
    steps.extend(commit(0));
    steps.push((0, RESTORE));
    reconnect(&mut steps, 0, true);
    reconnect(&mut steps, 1, true);
    world.phase(9, &steps).unwrap();
    for device in 0..2 {
        let names: Vec<String> = (files_on(&world, device).into_iter()).map(|(name, _)| name).collect();
        assert_eq!(names, ["b"], "device {device}");
    }
}


#[test]
fn gun664_case_17e5f7864722() {
    use koidrive_sim::world::{Reply::*, Step::*};
    converge(3, &[vec![(1, User(Action { kind: 1, target: 0, parent: 0, name: 0, text: 0 })), (1, Work(0)), (1, Commit(Served)), (1, Work(0)), (2, Deliver(0)), (1, Work(0)), (1, User(Action { kind: 3, target: 0, parent: 0, name: 1, text: 0 })), (1, Work(0)), (0, Backup), (1, Work(0)), (0, User(Action { kind: 0, target: 0, parent: 0, name: 0, text: 0 })), (2, User(Action { kind: 0, target: 0, parent: 0, name: 1, text: 0 })), (0, User(Action { kind: 0, target: 0, parent: 3307894247439793688, name: 0, text: 0 })), (2, Work(2654930495610263060)), (2, Work(0)), (1, User(Action { kind: 2, target: 0, parent: 0, name: 0, text: 0 })), (1, Work(0)), (0, User(Action { kind: 0, target: 0, parent: 961904704637406560, name: 0, text: 0 })), (2, Work(8041453028609285195)), (2, Work(7459037410794658530)), (0, Restore)], vec![(0, Restore)]]).unwrap();
}

#[test]
fn gun664_case_6bf3487902a9() {
    use koidrive_sim::world::{Reply::*, Step::*};
    converge(3, &[vec![(0, Backup), (0, User(Action { kind: 0, target: 0, parent: 0, name: 2, text: 0 })), (0, Work(0)), (0, User(Action { kind: 1, target: 0, parent: 0, name: 3, text: 0 })), (0, User(Action { kind: 2, target: 0, parent: 0, name: 0, text: 0 }))], vec![(1, User(Action { kind: 0, target: 0, parent: 1285814101918428590, name: 0, text: 0 })), (0, Restore), (0, Connect), (2, User(Action { kind: 3, target: 8848640273642333149, parent: 6382992032102656458, name: 0, text: 0 })), (0, Work(0)), (0, Work(4000746636949428065)), (2, Connect), (2, Work(1648833308860000556)), (0, Commit(Served)), (0, User(Action { kind: 2, target: 2483995871143172111, parent: 9171623605514407635, name: 1, text: 0 })), (2, Backup)], vec![(1, Work(2993819066984428697)), (1, Scan), (0, Deliver(17276249781050633269)), (0, Deliver(17043816143112026969)), (2, Effect(Outcome::Run)), (2, Work(16160981802101898727)), (2, Work(10282781507765826793)), (0, User(Action { kind: 1, target: 18392913543402149518, parent: 11861810052478043431, name: 5, text: 3 })), (2, Scan), (2, User(Action { kind: 2, target: 9563148829197439984, parent: 3721480683115858519, name: 4, text: 1 })), (2, Effect(Outcome::Crash(Cut::Placed))), (1, Connect), (0, Effect(Outcome::Run)), (0, User(Action { kind: 4, target: 5534979547166299467, parent: 11483298059832606243, name: 6, text: 0 })), (1, Duplicate(476988474317801881)), (0, Effect(Outcome::Crash(Cut::Placed))), (1, Scan), (0, Work(8603510855888096381)), (1, Work(12025954559901055639)), (0, Work(8419907464343443582)), (2, Crash), (1, Work(9463836557793324486)), (1, Restore), (2, Work(14992856410635154335)), (2, Deliver(5585018825101531003)), (0, Work(2410541318081646983)), (0, Work(10890549453080099804)), (1, Work(18061957845702363319)), (2, Connect), (2, Restore), (2, Work(75288745139796733)), (1, Work(16240715555806020138)), (1, User(Action { kind: 0, target: 15137973004049328398, parent: 16644388509644097824, name: 1, text: 2 })), (0, User(Action { kind: 2, target: 5923069940376990471, parent: 17055329173100555307, name: 2, text: 3 })), (1, Commit(Served)), (1, Work(5197059063883068885)), (2, User(Action { kind: 0, target: 16332341456421641696, parent: 4935467471380997141, name: 1, text: 2 })), (0, Effect(Outcome::Run)), (2, User(Action { kind: 2, target: 4966629693323885985, parent: 14589464943526988929, name: 1, text: 1 })), (1, Crash)]]).unwrap();
}

#[test]
fn gun664_case_c39aa0042d7c() {
    use koidrive_sim::world::{Reply::*, Step::*};
    converge(2, &[vec![(0, User(Action { kind: 1, target: 0, parent: 0, name: 0, text: 0 })), (0, Work(0)), (2, User(Action { kind: 1, target: 0, parent: 0, name: 1, text: 0 })), (2, Work(226769641356761788)), (0, Work(154764964883893176)), (0, Backup)], vec![(0, Restore), (0, User(Action { kind: 0, target: 0, parent: 0, name: 0, text: 0 })), (2, User(Action { kind: 2, target: 9248250813484896738, parent: 0, name: 0, text: 0 })), (0, Work(0)), (2, Connect), (2, Work(0)), (2, Commit(Served)), (0, Work(334267421000585101)), (2, Crash), (2, User(Action { kind: 4, target: 17001073352406634806, parent: 5552969616310016050, name: 6, text: 0 }))], vec![(0, Work(6066336003454004995)), (0, Work(2157439711314118186)), (1, Restore), (2, Commit(Served)), (2, Backup), (0, Deliver(18432019605995179659)), (0, User(Action { kind: 1, target: 7204822777281988933, parent: 10483849714204012889, name: 0, text: 3 })), (2, Work(12436860910610899253)), (0, Commit(Served)), (2, Effect(Outcome::Run)), (0, Commit(Served)), (0, Deliver(6773614427085039713)), (0, Deliver(6896852882504849322)), (0, Commit(Served)), (1, Effect(Outcome::Crash(Cut::Quarantined))), (2, Commit(Served)), (2, Effect(Outcome::Run)), (1, Work(10674601498695086820)), (0, Scan), (1, User(Action { kind: 4, target: 2914501090887244250, parent: 11582849181724836164, name: 4, text: 2 })), (0, Work(12531001567461460175)), (0, Work(13659522318414733105)), (1, Work(3384626037290750696)), (1, User(Action { kind: 2, target: 18019003530138293784, parent: 983002806470690905, name: 5, text: 0 })), (0, Work(12578335883886218491)), (1, Work(10443429435274116226)), (2, Deliver(17144292681594891497)), (1, User(Action { kind: 1, target: 6471515815065853230, parent: 19625063437672755, name: 5, text: 3 })), (1, User(Action { kind: 4, target: 3666823056793280571, parent: 14225806117082265611, name: 1, text: 0 })), (1, User(Action { kind: 3, target: 11931321251074486125, parent: 9235597965952925308, name: 0, text: 2 })), (2, User(Action { kind: 2, target: 9821567200189906265, parent: 14228924832452645471, name: 4, text: 2 })), (1, Backup), (2, Work(17613774861465103757))]]).unwrap();
}

#[test]
fn gun822_shrunk_03426d3f103f() {
    use koidrive_sim::world::{Reply::*, Step::*};
    converge(2, &[vec![(0, User(Action { kind: 1, target: 0, parent: 0, name: 0, text: 0 })), (0, CopyStore), (0, Backup)], vec![(0, Restore), (0, RollBack)]]).unwrap();
}

#[test]
fn gun822_shrunk_0a41f1e9ae61() {
    use koidrive_sim::world::{Reply::*, Step::*};
    converge(3, &[vec![(2, CopyStore)], vec![(2, User(Action { kind: 1, target: 0, parent: 0, name: 0, text: 0 })), (2, Work(0)), (2, Commit(Served)), (0, Backup), (2, Work(0)), (1, Work(0)), (0, Restore), (1, Work(0)), (2, RollBack)]]).unwrap();
}

#[test]
fn gun822_shrunk_3de68be52410() {
    use koidrive_sim::world::{Reply::*, Step::*};
    converge(2, &[vec![(0, CopyStore), (0, User(Action { kind: 1, target: 0, parent: 0, name: 0, text: 0 })), (0, Backup)], vec![(0, Restore), (2, RollBack)]]).unwrap();
}

#[test]
fn gun822_shrunk_3ee5348adf47() {
    use koidrive_sim::world::{Reply::*, Step::*};
    converge(3, &[vec![(0, User(Action { kind: 0, target: 0, parent: 0, name: 0, text: 0 }))], vec![(2, User(Action { kind: 0, target: 0, parent: 0, name: 5, text: 0 })), (2, User(Action { kind: 0, target: 0, parent: 0, name: 7, text: 0 })), (2, User(Action { kind: 0, target: 0, parent: 50448361321329, name: 0, text: 0 })), (2, CopyStore), (0, Backup), (2, User(Action { kind: 1, target: 0, parent: 0, name: 2, text: 0 })), (0, Backup)], vec![(2, RollBack), (0, Restore)]]).unwrap();
}

#[test]
fn gun822_shrunk_7651a55b7ff5() {
    use koidrive_sim::world::{Reply::*, Step::*};
    converge(2, &[vec![(0, Backup), (0, User(Action { kind: 0, target: 0, parent: 0, name: 0, text: 0 })), (0, User(Action { kind: 0, target: 0, parent: 0, name: 2, text: 0 })), (2, User(Action { kind: 0, target: 0, parent: 2345804654425692805, name: 0, text: 0 }))], vec![(2, CopyStore), (0, Backup), (2, User(Action { kind: 1, target: 0, parent: 0, name: 3, text: 0 })), (0, RestoreEarlier)], vec![(2, RollBack)]]).unwrap();
}

#[test]
fn gun822_shrunk_80a87d182a17() {
    use koidrive_sim::world::{Reply::*, Step::*};
    converge(3, &[vec![(0, CopyStore), (0, Backup)], vec![(0, User(Action { kind: 1, target: 0, parent: 0, name: 0, text: 0 })), (0, Work(0)), (0, Work(0)), (0, Work(0)), (0, RollBack), (1, Work(0)), (0, Restore), (1, Work(0))]]).unwrap();
}

#[test]
fn gun822_shrunk_937f82c28d9a() {
    use koidrive_sim::world::{Reply::*, Step::*};
    converge(2, &[vec![(0, CopyStore), (2, User(Action { kind: 1, target: 0, parent: 0, name: 0, text: 0 })), (0, Backup)], vec![(0, Restore), (2, RollBack)]]).unwrap();
}

#[test]
fn gun822_shrunk_95968ffa1ff1() {
    use koidrive_sim::world::{Reply::*, Step::*};
    converge(3, &[vec![(0, Backup), (0, CopyStore), (0, User(Action { kind: 1, target: 0, parent: 0, name: 0, text: 0 }))], vec![(0, Restore), (0, RollBack)]]).unwrap();
}

#[test]
fn gun822_shrunk_999066977e98() {
    use koidrive_sim::world::{Reply::*, Step::*};
    converge(2, &[vec![(2, CopyStore), (0, Backup), (0, User(Action { kind: 1, target: 0, parent: 0, name: 0, text: 0 })), (2, User(Action { kind: 0, target: 0, parent: 0, name: 2, text: 0 }))], vec![(0, Restore), (2, RollBack)]]).unwrap();
}

#[test]
fn gun822_shrunk_e4b4ab528fa9() {
    use koidrive_sim::world::{Reply::*, Step::*};
    converge(2, &[vec![(0, CopyStore)], vec![(0, User(Action { kind: 0, target: 0, parent: 0, name: 2, text: 0 })), (0, Backup), (2, User(Action { kind: 1, target: 0, parent: 0, name: 0, text: 0 }))], vec![(0, Restore), (2, RollBack)]]).unwrap();
}

#[test]
fn gun822_shrunk_f0860cfbf189() {
    use koidrive_sim::world::{Reply::*, Step::*};
    converge(2, &[vec![(1, CopyStore), (1, User(Action { kind: 1, target: 0, parent: 0, name: 0, text: 0 })), (0, Backup)], vec![(0, Restore), (0, User(Action { kind: 0, target: 0, parent: 0, name: 0, text: 0 })), (1, RollBack)]]).unwrap();
}

#[test]
fn gun646_shrunk_76e95b2b653c() {
    use koidrive_sim::world::{Reply::*, Step::*};
    keep(3, &[vec![(1, User(Action { kind: 1, target: 0, parent: 0, name: 0, text: 0 })), (0, User(Action { kind: 1, target: 0, parent: 0, name: 2, text: 0 }))], vec![(1, User(Action { kind: 2, target: 1206730729911617966, parent: 0, name: 0, text: 0 })), (1, Work(0)), (1, Work(0)), (0, Backup), (1, Commit(Served)), (1, Work(0)), (2, User(Action { kind: 4, target: 12624898511826296137, parent: 0, name: 0, text: 0 })), (0, Work(0)), (0, Effect(Outcome::Crash(Cut::Quarantined))), (0, Restore)]]).unwrap();
}


#[test]
fn gun748_an_edit_inside_the_rollback_window_ends_as_one_file() {
    let mut world = converge(2, &[vec![(1, file("a"))]]).unwrap();
    world.phase(1, &[(0, Step::CopyStore), (0, overwrite(0)), (0, Step::RollBack)]).unwrap();
    for device in 0..2 {
        let names: Vec<String> = (files_on(&world, device).into_iter()).map(|(name, _)| name).collect();
        assert_eq!(names, ["a"], "device {device}");
    }
    assert_eq!(content_on(&world, 0, "a"), content_on(&world, 1, "a"));
}

