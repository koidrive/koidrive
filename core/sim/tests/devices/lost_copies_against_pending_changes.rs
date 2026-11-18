// SPDX-License-Identifier: Apache-2.0

use super::*;

fn u(kind: u8, target: usize, name: usize, text: u8) -> Step {
    Step::User(Action {
        kind,
        target,
        parent: 0,
        name,
        text,
    })
}

fn server_files(world: &World) -> Vec<(String, Option<i64>)> {
    let mut files: Vec<_> = (world.space().nodes().into_iter())
        .filter(|node| node.status == koidrive_proto::v1::NodeStatus::Live as i32)
        .filter(|node| !node.name.is_empty())
        .map(|node| {
            let write = node.current.and_then(|version| version.content);
            (node.name, write.as_ref().map(tag))
        })
        .collect();
    files.sort();
    files
}

fn a_write_kept_in_a_lost_copy() -> Vec<Vec<(usize, Step)>> {
    vec![
        vec![(0, u(1, 0, 0, 0))],
        vec![
            (2, u(2, 0, 0, 0)),
            (2, Step::Work(0)),
            (0, Step::Backup),
            (2, Step::Work(0)),
            (2, Step::Work(0)),
            (0, Step::Backup),
            (0, u(2, 0, 0, 0)),
            (0, Step::RestoreEarlier),
        ],
    ]
}

fn an_offline_edit_across_a_rewind_answered(answers: &[Step]) -> World {
    let mut phases = a_write_kept_in_a_lost_copy();
    let mut steps = vec![
        (1, u(2, 4100883647648707004, 7, 0)),
        (0, Step::Rewind),
        (1, Step::Connect),
    ];
    steps.extend([(1, Step::Work(0)); 6]);
    steps.extend(answers.iter().map(|answer| (1, *answer)));
    phases.push(steps);
    converge(3, &phases).unwrap()
}

#[test]
#[ignore = "a delete of the server file that holds a lost copy's write keeps that write in one file (one file per write)"]
fn a_delete_of_the_file_holding_a_lost_copys_write_keeps_the_write_in_one_file() {
    converge(
        3,
        &[
            vec![(0, u(1, 0, 0, 0))],
            vec![
                (0, u(2, 0, 0, 0)),
                (0, Step::Backup),
                (2, u(2, 0, 0, 0)),
                (2, Step::Work(0)),
                (2, Step::Work(0)),
                (2, Step::Work(0)),
                (0, Step::Backup),
                (0, Step::RestoreEarlier),
            ],
            vec![(0, Step::Backup), (0, Step::RestoreEarlier)],
            vec![(0, u(4, 7189654988924490997, 9, 0)), (0, Step::Restore)],
        ],
    )
    .unwrap();
}

#[test]
#[ignore = "a Publish answer given after the lost copy's answer keeps the copy's write in the server file and lands the offline edit beside it (RFC 0029 section 5.5)"]
fn an_offline_edit_published_after_the_lost_copys_answer_lands_beside_the_copys_write() {
    let world = an_offline_edit_across_a_rewind_answered(&[Step::Answer(0), Step::Answer(0)]);
    the_offline_edit_lands_beside_the_copys_write(&world);
}

#[test]
#[ignore = "a Publish answer given before the lost copy's answer keeps the copy's write in the server file and lands the offline edit beside it (RFC 0029 section 5.5)"]
fn an_offline_edit_published_before_the_lost_copys_answer_lands_beside_the_copys_write() {
    let world = an_offline_edit_across_a_rewind_answered(&[Step::AnswerLast(0), Step::Answer(0)]);
    the_offline_edit_lands_beside_the_copys_write(&world);
}

fn the_offline_edit_lands_beside_the_copys_write(world: &World) {
    let files = server_files(world);
    assert_eq!(files.len(), 2, "{files:?}");
    assert!(files.contains(&("a".to_owned(), Some(2))), "{files:?}");
    assert!(
        (files.iter())
            .any(|(name, write)| *write == Some(4) && name.starts_with("a (conflict from ")),
        "{files:?}"
    );
}

#[test]
fn a_rolled_back_store_makes_no_recovery_copy_of_a_write_the_server_kept() {
    converge(
        2,
        &[
            vec![
                (0, Step::Backup),
                (1, u(0, 0, 2, 0)),
                (
                    1,
                    Step::User(Action {
                        kind: 1,
                        target: 0,
                        parent: 962789128453331756,
                        name: 0,
                        text: 0,
                    }),
                ),
                (0, u(0, 0, 0, 0)),
            ],
            vec![
                (2, u(2, 1173499300033365664, 0, 0)),
                (0, Step::Work(0)),
                (2, Step::Commit(Reply::Served)),
                (0, Step::Restore),
                (0, Step::CopyStore),
            ],
            vec![
                (2, Step::RollBack),
                (0, Step::Backup),
                (0, Step::Backup),
                (0, Step::Restore),
                (1, u(2, 3034077457195674380, 0, 0)),
            ],
        ],
    )
    .unwrap();
}
