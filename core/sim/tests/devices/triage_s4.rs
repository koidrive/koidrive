// SPDX-License-Identifier: Apache-2.0

use super::*;

fn u(kind: u8, target: usize, parent: usize, name: usize, text: u8) -> Step {
    Step::User(Action {
        kind,
        target,
        parent,
        name,
        text,
    })
}

#[test]
fn gun531_an_idle_device_does_not_send_a_position_it_never_set_after_a_restore() {
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
fn gun539_undoing_a_restore_is_not_taken_for_a_rolled_back_store() {
    let phases = vec![
        vec![(0, Step::Backup), (0, u(0, 0, 0, 0, 0))],
        vec![(0, Step::Backup)],
        vec![(0, Step::RestoreEarlier), (0, Step::Connect), (0, Step::Restore)],
    ];
    converge(2, &phases).unwrap();
}

#[test]
fn gun539_restore_restore_earlier_and_restore_again_after_overwrites() {
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

fn in_flight_change() -> impl Strategy<Value = Vec<(usize, Step)>> {
    let change = (
        prop_oneof![Just(2u8), Just(3u8)],
        any::<usize>(),
        0..NAMES.len(),
        0..TEXTS,
    );
    let others = || prop::collection::vec((0..3usize, step(Losses::None)), 0..8);
    (
        (
            0..3usize,
            any::<usize>(),
            change.clone(),
            change,
            any::<bool>(),
        ),
        (prop::collection::vec(any::<usize>(), 0..3), any::<bool>()),
        (others(), others()),
    )
        .prop_map(
            |((d, target, first, second, backup), (delivers, send_again), (before, after))| {
                let act = |(kind, parent, name, text)| {
                    Step::User(Action {
                        kind,
                        target,
                        parent,
                        name,
                        text,
                    })
                };
                let send = [
                    (d, Step::Scan),
                    (d, Step::Commit(Reply::Served)),
                    (d, Step::Commit(Reply::Served)),
                ];
                let mut steps = Vec::new();
                if backup {
                    steps.push((d, Step::Backup));
                }
                steps.push((d, act(first)));
                steps.extend(send);
                steps.extend(delivers.into_iter().map(|at| (d, Step::Deliver(at))));
                steps.push((d, act(second)));
                if send_again {
                    steps.extend(send);
                }
                steps.extend(before);
                steps.push((d, Step::Restore));
                steps.extend(after);
                steps
            },
        )
}

fn in_flight_phases() -> impl Strategy<Value = Vec<Vec<(usize, Step)>>> {
    let made = (0..3usize, any::<usize>(), 0..NAMES.len(), 0..TEXTS).prop_map(
        |(d, parent, name, text)| {
            let made = Action {
                kind: 1,
                target: 0,
                parent,
                name,
                text,
            };
            (d, Step::User(made))
        },
    );
    (
        prop::collection::vec(made, 1..5),
        prop::collection::vec((0..3usize, step(Losses::None)), 0..24),
        prop::collection::vec(in_flight_change(), 1..=3),
    )
        .prop_map(|(made, steps, changes)| {
            let mut setup = made;
            setup.extend(steps);
            setup.push((0, Step::Backup));
            std::iter::once(setup).chain(changes).collect()
        })
}

proptest! {
    #![proptest_config(koidrive_test_support::battery_config(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/devices.proptest-regressions"),
        0,
    ))]

    #[test]
    #[ignore = "triage port of 1d838a3a80"]
    fn devices_keep_changes_made_while_their_commit_is_in_flight_across_restores(devices in 2..=3usize, phases in in_flight_phases()) {
        converge(devices, &phases).map_err(|failure| TestCaseError::fail(failure.to_string()))?;
    }
}

