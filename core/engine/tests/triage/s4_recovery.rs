// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn gun563_an_answer_whose_retrash_bundle_exceeds_one_commit_is_sent() {
    let children: Vec<String> = (0..600).map(|index| format!("f{index}")).collect();
    let names: Vec<String> = (0..600).map(|index| format!("child{index}")).collect();
    let mut synced = vec![("d", dir(ROOT, "old"))];
    let mut remote = vec![Remote("d", dir(ROOT, "old"), true, 1)];
    for (id, name) in children.iter().zip(&names) {
        synced.push((id.as_str(), file("d", name, 1)));
        remote.push(Remote(id.as_str(), file("d", name, 1), true, 1));
    }
    let mut trees = trees(&[("d", dir(ROOT, "new"))], &synced, &remote);
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    let limit = Error {
        code: error::Code::LimitExceeded as i32,
        ..Default::default()
    };
    let mut sizes = Vec::new();
    for _ in 0..3 {
        for question in commits.questions().to_vec() {
            commits.decide(&question.id, Answer::Publish);
        }
        let sent = ops(&mut commits, &mut trees, &mut ids);
        sizes.push(sent.len());
        if sent.is_empty() {
            continue;
        }
        if sent.len() <= koidrive_engine::commit::MAX_OPS {
            break;
        }
        assert_eq!(commits.failed(&mut trees, &limit), Ok(Failure::Split));
    }
    assert!(
        sizes.iter().all(|size| *size <= koidrive_engine::commit::MAX_OPS),
        "commit sizes per round: {sizes:?}"
    );
}

#[test]
fn gun987_publishing_a_folder_moved_out_of_a_trashed_parent_with_a_move_into_a_new_folder_inside_it_sends_every_change()
{
    let mut trees = trees(
        &[
            ("a", dir(ROOT, "a")),
            ("n", dir("a", "n")),
            ("b", dir("n", "b")),
        ],
        &[
            ("p", dir(ROOT, "p")),
            ("a", dir("p", "a")),
            ("c", dir("a", "c")),
            ("b", dir("c", "b")),
        ],
        &[
            Remote("p", dir(ROOT, "p"), true, 2),
            Remote("a", dir("p", "a"), true, 1),
            Remote("c", dir("a", "c"), true, 1),
            Remote("b", dir("c", "b"), true, 1),
        ],
    );
    let (mut commits, mut ids) = recovered(&mut trees, 0);
    let mut rounds = Vec::new();
    for _ in 0..5 {
        for question in commits.questions().to_vec() {
            commits.decide(&question.id, Answer::Publish);
        }
        let sent = ops(&mut commits, &mut trees, &mut ids);
        let done = !sent.is_empty();
        rounds.push(sent);
        if done {
            break;
        }
    }
    assert!(
        rounds.iter().any(|sent| !sent.is_empty()),
        "every round sent nothing: {rounds:?}, questions {:?}",
        commits.questions()
    );
}

#[test]
fn gun931_a_rekeyed_folder_leaves_its_unchanged_child_unasked() {
    let (trees, commits, _) = recovered_by_marks(
        &[("d1", dir(ROOT, "p")), ("c", file("d1", "b", 1))],
        &[],
        &[
            live("d1", dir(ROOT, "p")),
            live("d2", dir(ROOT, "q")),
            live("c", file("d1", "b", 1)),
        ],
        &[("d2", dir(ROOT, "p")), ("d2", dir(ROOT, "q"))],
        &[("d1", "d2"), ("c", "c")],
    );
    let local: Vec<(String, Shape)> = (trees.local().iter())
        .map(|(id, node)| (id.clone(), node.shape.clone()))
        .collect();
    let questions: Vec<(RecoveryGroup, Vec<String>)> = (commits.questions().iter())
        .map(|question| (group(question), question.holds().cloned().collect()))
        .collect();
    eprintln!("GUN931 local {local:?}");
    eprintln!("GUN931 questions {questions:?}");
    assert!(
        !questions.iter().any(|(group, holds)| *group == RecoveryGroup::ChangedOnThisDevice
            && holds.iter().any(|id| id == "c")),
        "the unchanged child is asked about as changed: {questions:?}"
    );
}

