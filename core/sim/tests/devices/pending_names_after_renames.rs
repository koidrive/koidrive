// SPDX-License-Identifier: Apache-2.0

use super::*;

fn a_folder_renamed_back_to_its_pending_name(
    drop: bool,
    via: &str,
    k: usize,
    j: usize,
) -> Vec<Vec<(usize, Step)>> {
    let mut p1 = vec![
        (0, folder("A")),
        (0, rename(0, 1, "a")),
        (0, SCAN),
        (0, COMMIT),
        (0, COMMIT),
        (0, DELIVER),
    ];
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
#[ignore = "128 of the 160 schedules leave the folder at \"A\" on both devices instead of at its pending name"]
fn a_folder_renamed_through_another_name_back_to_its_pending_name_keeps_it() {
    let mut bad = Vec::new();
    for drop in [false, true] {
        for via in ["b", "A"] {
            for k in 0..10 {
                for j in 0..4 {
                    let phases = a_folder_renamed_back_to_its_pending_name(drop, via, k, j);
                    let folders = converge(2, &phases).map(|world| {
                        (world.devices.iter())
                            .map(|device| {
                                let mut names: Vec<String> = (device.disk.entries().values())
                                    .filter(|entry| entry.content.is_none())
                                    .map(|entry| entry.name.clone())
                                    .collect();
                                names.sort();
                                names
                            })
                            .collect::<Vec<_>>()
                    });
                    match folders {
                        Ok(folders)
                            if (folders.iter())
                                .all(|names| names.is_empty() || names == &["A (pending)"]) => {}
                        other => bad.push((drop, via, k, j, other.map_err(|f| f.to_string()))),
                    }
                }
            }
        }
    }
    assert!(
        bad.is_empty(),
        "{} of 160 fail, first {:?}",
        bad.len(),
        bad.first()
    );
}

fn u(kind: u8, target: usize, parent: usize, name: usize) -> Step {
    Step::User(Action {
        kind,
        target,
        parent,
        name,
        text: 0,
    })
}

#[test]
fn only_the_namer_finishes_a_pending_name_after_its_commit_was_refused_for_retry() {
    let w = |n: usize| Step::Work(n);
    let phases = vec![
        vec![(0, u(1, 0, 0, 0))],
        vec![
            (0, u(0, 0, 0, 1)),
            (0, u(3, 0, 1, 0)),
            (0, w(0)),
            (0, w(0)),
            (0, w(0)),
            (0, w(0)),
            (0, w(0)),
            (1, w(0)),
            (1, w(0)),
            (1, w(0)),
            (1, w(0)),
            (1, w(0)),
            (0, Step::Commit(Reply::Refused(error::Code::RetryLater))),
            (1, u(4, 1, 0, 0)),
            (1, Step::Scan),
            (1, Step::Commit(Reply::Served)),
            (1, Step::Commit(Reply::Served)),
            (1, Step::Deliver(0)),
            (0, u(1, 0, 1, 2)),
            (0, Step::Scan),
            (0, Step::Commit(Reply::Served)),
            (0, Step::Commit(Reply::Served)),
            (0, Step::Deliver(0)),
            (1, u(4, 1, 0, 0)),
            (2, w(12276220042150375480)),
            (2, w(2911995295995607241)),
            (0, u(0, 0, 0, 0)),
            (2, u(3, 0, 0, 2)),
            (2, w(5559466202305603081)),
            (2, w(8664856006031173953)),
            (2, u(0, 0, 3503715600015695281, 2)),
            (2, w(5683380402076589777)),
            (0, u(0, 0, 0, 0)),
            (2, w(892960135619390085)),
        ],
    ];
    converge(3, &phases).unwrap();
}

#[test]
#[ignore = "device 1 finishes the folder's pending name \"A\" that its user never gave, and device 2 then moves the folder to \"A\", a place its user never gave it"]
fn only_the_namer_finishes_a_pending_name_after_a_commit_over_the_limit_and_a_crash() {
    let w = |n: usize| Step::Work(n);
    let phases = vec![
        vec![(0, u(1, 0, 0, 0))],
        vec![
            (0, u(0, 0, 0, 1)),
            (0, u(3, 0, 1, 0)),
            (0, w(0)),
            (0, w(0)),
            (0, w(0)),
            (0, w(0)),
            (0, w(0)),
            (1, w(0)),
            (1, w(0)),
            (1, w(0)),
            (1, w(0)),
            (1, w(0)),
            (1, w(0)),
            (1, w(0)),
            (1, w(0)),
            (1, u(3, 1, 0, 1)),
            (1, Step::Commit(Reply::Refused(error::Code::LimitExceeded))),
            (2, u(3, 0, 0, 2)),
            (2, w(0)),
            (1, u(0, 0, 0, 0)),
            (0, Step::Connect),
            (2, Step::Effect(Outcome::Crash(Cut::Placed))),
            (0, u(0, 0, 0, 0)),
            (1, w(12931887994201877600)),
            (2, w(0)),
            (1, w(0)),
            (1, w(0)),
            (1, Step::Commit(Reply::Served)),
            (2, Step::Connect),
            (2, Step::Effect(Outcome::Run)),
            (2, w(1791283433846243090)),
        ],
    ];
    converge(3, &phases).unwrap();
}
