// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn triage_s3_gun673_a_stale_resend_from_a_device_that_never_saw_the_authors_create_keeps_the_authors_name() {
    let mut world = converge(2, &[vec![(0, BACKUP), (0, file("a"))]]).unwrap();
    let mut steps = vec![(0, RESTORE)];
    reconnect(&mut steps, 1, false);
    steps.push((1, Step::Drop));
    steps.push((0, rename(0, 0, "b")));
    reconnect(&mut steps, 0, true);
    steps.push((0, RESTORE));
    reconnect(&mut steps, 1, true);
    reconnect(&mut steps, 0, true);
    steps.extend([(1, DELIVER); 4]);
    world.phase(1, &steps).unwrap();
    for device in 0..2 {
        let names: Vec<String> = (files_on(&world, device).into_iter()).map(|(name, _)| name).collect();
        assert_eq!(names, ["b"], "device {device}");
    }
}

fn triage_s3_gun639_back(drop: bool, via: &str, k: usize, j: usize) -> Vec<Vec<(usize, Step)>> {
    let mut p1 = vec![(0, folder("A")), (0, rename(0, 1, "a")), (0, SCAN), (0, COMMIT), (0, COMMIT), (0, DELIVER)];
    if drop {
        p1.push((0, Step::Drop));
    }
    p1.extend(std::iter::repeat_n((1, Step::Work(0)), k));
    p1.extend([(1, rename(1, 0, via)), (1, SCAN)]);
    p1.extend(std::iter::repeat_n((1, Step::Work(0)), j));
    p1.extend([(1, rename(1, 0, "A (pending)")), (1, SCAN)]);
    p1.extend(std::iter::repeat_n((1, Step::Work(0)), 8));
    p1.push((0, Step::Connect));
    p1.extend(std::iter::repeat_n((0, Step::Work(0)), 16));
    p1.extend(std::iter::repeat_n((1, Step::Work(0)), 8));
    vec![vec![(0, file("a"))], p1]
}

#[test]
fn triage_s3_gun639_back_matrix() {
    let mut bad = Vec::new();
    let mut runs = 0;
    for drop in [false, true] {
        for via in ["b", "A"] {
            for k in 0..10usize {
                for j in 0..4usize {
                    runs += 1;
                    match converge(2, &triage_s3_gun639_back(drop, via, k, j)) {
                        Err(failure) => {
                            eprintln!("FAIL drop={drop} via={via} k={k} j={j}: {}", failure.to_string().chars().take(260).collect::<String>());
                            bad.push((drop, via, k, j));
                        }
                        Ok(world) => {
                            let names: Vec<Vec<String>> = world
                                .devices
                                .iter()
                                .map(|d| {
                                    let mut v: Vec<String> = d.disk.entries().values().filter(|e| e.content.is_none()).map(|e| e.name.clone()).collect();
                                    v.sort();
                                    v
                                })
                                .collect();
                            let ok = names.iter().all(|v| v.is_empty() || v == &vec!["A (pending)".to_string()]);
                            eprintln!("{} drop={drop} via={via} k={k} j={j}: dirs {names:?}", if ok { "OK" } else { "LOST" });
                            if !ok {
                                bad.push((drop, via, k, j));
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(bad.is_empty(), "{} of {runs} bad", bad.len());
}
fn triage_s3_u(kind: u8, target: usize, parent: usize, name: usize) -> Step {
    Step::User(Action { kind, target, parent, name, text: 0 })
}

#[test]
fn triage_s3_gun808_1604d1eb_shrunk() {
    let u = triage_s3_u;
    let w = |n: usize| Step::Work(n);
    let phases = vec![
        vec![(0, u(1, 0, 0, 0))],
        vec![
            (0, u(0, 0, 0, 1)), (0, u(3, 0, 1, 0)),
            (0, w(0)), (0, w(0)), (0, w(0)), (0, w(0)), (0, w(0)),
            (1, w(0)), (1, w(0)), (1, w(0)), (1, w(0)), (1, w(0)),
            (0, Step::Commit(Reply::Refused(error::Code::RetryLater))),
            (1, u(4, 1, 0, 0)), (1, Step::Scan), (1, Step::Commit(Reply::Served)), (1, Step::Commit(Reply::Served)), (1, Step::Deliver(0)),
            (0, u(1, 0, 1, 2)), (0, Step::Scan), (0, Step::Commit(Reply::Served)), (0, Step::Commit(Reply::Served)), (0, Step::Deliver(0)),
            (1, u(4, 1, 0, 0)),
            (2, w(12276220042150375480)), (2, w(2911995295995607241)),
            (0, u(0, 0, 0, 0)),
            (2, u(3, 0, 0, 2)),
            (2, w(5559466202305603081)), (2, w(8664856006031173953)),
            (2, u(0, 0, 3503715600015695281, 2)),
            (2, w(5683380402076589777)),
            (0, u(0, 0, 0, 0)),
            (2, w(892960135619390085)),
        ],
    ];
    converge(3, &phases).unwrap();
}
#[test]
fn triage_s3_gun777_513d3d16_shrunk() {
    use koidrive_sim::world::{Cut as S3Cut, Outcome as S3Outcome};
    let u = triage_s3_u;
    let w = |n: usize| Step::Work(n);
    let phases = vec![
        vec![(0, u(1, 0, 0, 0))],
        vec![
            (0, u(0, 0, 0, 1)), (0, u(3, 0, 1, 0)),
            (0, w(0)), (0, w(0)), (0, w(0)), (0, w(0)), (0, w(0)),
            (1, w(0)), (1, w(0)), (1, w(0)), (1, w(0)), (1, w(0)), (1, w(0)), (1, w(0)), (1, w(0)),
            (1, u(3, 1, 0, 1)),
            (1, Step::Commit(Reply::Refused(error::Code::LimitExceeded))),
            (2, u(3, 0, 0, 2)),
            (2, w(0)),
            (1, u(0, 0, 0, 0)),
            (0, Step::Connect),
            (2, Step::Effect(S3Outcome::Crash(S3Cut::Placed))),
            (0, u(0, 0, 0, 0)),
            (1, w(12931887994201877600)),
            (2, w(0)),
            (1, w(0)), (1, w(0)),
            (1, Step::Commit(Reply::Served)),
            (2, Step::Connect),
            (2, Step::Effect(S3Outcome::Run)),
            (2, w(1791283433846243090)),
        ],
    ];
    converge(3, &phases).unwrap();
}
