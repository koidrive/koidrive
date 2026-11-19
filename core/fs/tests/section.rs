// SPDX-License-Identifier: Apache-2.0

mod common;

use std::cell::Cell;
use std::fs;
use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use common::{
    Bytes, REMOTE, SYNCED, assert_settled, at, bind, contents, dir, entries, marker, now, trashed,
};
use koidrive_fs::{Action, Displaced, Effect, Outcome, Step};

const DONE_FROM: [Step; 5] = [
    Step::Placed,
    Step::Placed,
    Step::Placed,
    Step::Quarantined,
    Step::Quarantined,
];

fn prepare(root: &Path) -> Vec<Effect> {
    fs::create_dir(root.join("docs")).unwrap();
    fs::write(root.join("docs/report.txt"), SYNCED).unwrap();
    fs::write(root.join("docs/old.txt"), SYNCED).unwrap();
    let space = bind(root);
    for id in ["a", "b", "r"] {
        let mut file = space.stage(id).unwrap();
        file.write_all(REMOTE).unwrap();
        drop(file);
        space.finish_staged(id, false).unwrap();
    }
    let found = entries(root, &["docs", "docs/report.txt", "docs/old.txt"]);
    let docs = |name: &str| at(vec![dir("docs", &found[0])], name);
    let effect = |id: &str, action| Effect {
        id: id.into(),
        node: "node".into(),
        action,
    };
    vec![
        effect(
            "a",
            Action::WriteFile {
                at: docs("a.txt"),
                replaces: None,
                staged: None,
            },
        ),
        effect(
            "b",
            Action::WriteFile {
                at: docs("b.txt"),
                replaces: None,
                staged: None,
            },
        ),
        effect("c", Action::CreateDir { at: docs("sub") }),
        effect(
            "r",
            Action::WriteFile {
                at: docs("report.txt"),
                replaces: Some(Displaced {
                    at: docs("report.txt"),
                    entry: found[1],
                }),
                staged: None,
            },
        ),
        effect(
            "x",
            Action::RemoveFile {
                displaced: Displaced {
                    at: docs("old.txt"),
                    entry: found[2],
                },
            },
        ),
    ]
}

fn run(
    root: &Path,
    effects: &[Effect],
    fail_at: Option<usize>,
) -> (Vec<(usize, Step)>, Option<Vec<Outcome>>) {
    let space = bind(root);
    let current = Cell::new(0);
    let mut reached = Vec::new();
    let mut hook = |step| {
        if Some(reached.len()) == fail_at {
            panic!("failure injected at {step:?}");
        }
        reached.push((current.get(), step));
    };
    let finished = catch_unwind(AssertUnwindSafe(|| {
        let mut section = space.section(&marker(), &mut hook).unwrap();
        for (index, effect) in effects.iter().enumerate() {
            current.set(index);
            section.apply(effect, &mut Bytes, now()).unwrap();
        }
        section.finish().unwrap()
    }));
    (reached, finished.ok())
}

fn recover(root: &Path, effects: &[Effect]) -> Vec<Outcome> {
    let space = bind(root);
    let mut hook = |_| {};
    let mut section = space.section(&marker(), &mut hook).unwrap();
    for effect in effects {
        section.recover(effect, &mut Bytes, now()).unwrap();
    }
    let outcomes = section.finish().unwrap();
    space.clear_staging(None).unwrap();
    outcomes
}

fn done(outcomes: &[Outcome]) -> Vec<bool> {
    outcomes
        .iter()
        .map(|outcome| matches!(outcome, Outcome::Done { .. }))
        .collect()
}

fn check(root: &Path, outcomes: &[Outcome]) {
    let done = done(outcomes);
    let written = |done: bool| done.then_some(REMOTE);
    assert_eq!(
        contents(root.join("docs/a.txt")).as_deref(),
        written(done[0])
    );
    assert_eq!(
        contents(root.join("docs/b.txt")).as_deref(),
        written(done[1])
    );
    assert_eq!(root.join("docs/sub").is_dir(), done[2]);
    let report = if done[3] { REMOTE } else { SYNCED };
    assert_eq!(
        contents(root.join("docs/report.txt")).as_deref(),
        Some(report)
    );
    assert_eq!(root.join("docs/old.txt").exists(), !done[4]);
    let displaced = usize::from(done[3]) + usize::from(done[4]);
    assert_eq!(trashed(root), vec![SYNCED; displaced]);
    assert_settled(root);
}

#[test]
fn a_section_applies_several_effects_and_marks_the_root_once() {
    let dir = tempfile::tempdir().unwrap();
    let effects = prepare(dir.path());
    let (steps, outcomes) = run(dir.path(), &effects, None);
    let outcomes = outcomes.unwrap();
    assert_eq!(done(&outcomes), [true; 5], "{outcomes:?}");
    assert_eq!(steps[0], (0, Step::Marked));
    assert_eq!(
        steps
            .iter()
            .filter(|(_, step)| *step == Step::Marked)
            .count(),
        1
    );
    check(dir.path(), &outcomes);
}

#[test]
fn every_effect_of_a_section_recovers_after_a_failure_at_every_step() {
    let dir = tempfile::tempdir().unwrap();
    let effects = prepare(dir.path());
    let (steps, _) = run(dir.path(), &effects, None);
    let done_at: Vec<usize> = (0..)
        .zip(DONE_FROM)
        .map(|done| steps.iter().position(|reached| *reached == done).unwrap())
        .collect();
    for fail_at in 0..=steps.len() {
        let context = format!("failed at {fail_at} of {steps:?}");
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let effects = prepare(root);
        let (reached, finished) = run(root, &effects, (fail_at < steps.len()).then_some(fail_at));
        assert_eq!(reached, steps[..fail_at], "{context}");
        assert_eq!(finished.is_some(), fail_at == steps.len(), "{context}");

        let outcomes = recover(root, &effects);
        let expected: Vec<bool> = done_at.iter().map(|&at| fail_at >= at).collect();
        assert_eq!(done(&outcomes), expected, "{context}: {outcomes:?}");
        check(root, &outcomes);
        assert_eq!(recover(root, &effects), outcomes, "{context}: again");
        check(root, &outcomes);
    }
}
