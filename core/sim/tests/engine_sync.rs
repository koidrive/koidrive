// SPDX-License-Identifier: Apache-2.0

const FIELD_INTENTS_BY_DEFAULT: bool = false;

include!("engine_sync/common.rs");
include!("engine_sync/first_link.rs");
include!("engine_sync/held_renames_and_purged_folders.rs");
include!("engine_sync/moves_and_deletes.rs");
include!("engine_sync/parked_nodes.rs");
include!("engine_sync/restores.rs");
include!("engine_sync/rules_and_held_entries.rs");
include!("engine_sync/waiting_updates.rs");

type During = Vec<(bool, Vec<Action>)>;

type LocalChanges = (
    Vec<Action>,
    Vec<Action>,
    Vec<Action>,
    usize,
    bool,
    During,
    Cut,
);

fn local_changes() -> impl Strategy<Value = LocalChanges> {
    (
        prop::collection::vec(action(2), 0..12),
        prop::collection::vec(action(7), 0..8),
        prop::collection::vec(action(7), 0..8),
        prop_oneof![Just(MAX_OPS), 1usize..4],
        any::<bool>(),
        prop::collection::vec(
            (any::<bool>(), prop::collection::vec(action(7), 1..4)),
            0..6,
        ),
        prop::sample::select(vec![
            Cut::None,
            Cut::Built,
            Cut::Applied,
            Cut::Answered,
            Cut::Effect,
        ]),
    )
}

fn local_changes_converge(
    (seed, local, remote, fits, fresh_dir_names, during, cut): LocalChanges,
) -> Result<(), TestCaseError> {
    run_cut(&seed, &local, &remote, fits, fresh_dir_names, &during, cut)?;
    Ok(())
}

type RulesChange = (
    Vec<Action>,
    Vec<Action>,
    Vec<Action>,
    usize,
    During,
    Vec<Ruling>,
);

fn rules_change() -> impl Strategy<Value = RulesChange> {
    (
        prop::collection::vec(action(2), 0..12),
        prop::collection::vec(action(4), 0..8),
        prop::collection::vec(action(4), 0..8),
        prop_oneof![Just(MAX_OPS), 1usize..4],
        prop::collection::vec(
            (any::<bool>(), prop::collection::vec(action(4), 1..4)),
            0..6,
        ),
        prop::collection::vec(ruling(), 0..7),
    )
}

fn rules_change_deletes_nothing(
    (seed, local, remote, fits, during, rulings): RulesChange,
) -> Result<(), TestCaseError> {
    let ran = run_ignoring(
        &seed,
        &local,
        &remote,
        fits,
        true,
        &during,
        Cut::None,
        &rulings,
        Answer::Delete,
        None,
    )?;
    prop_assert_eq!((ran.deletes, ran.removals, ran.trashed), (0, 0, 0));
    Ok(())
}

type MovedOntoLeftOut = (
    Vec<Action>,
    Vec<Action>,
    Vec<Action>,
    usize,
    During,
    Vec<Ruling>,
);

fn moved_onto_left_out() -> impl Strategy<Value = MovedOntoLeftOut> {
    (
        prop::collection::vec(action(2), 0..6),
        prop::collection::vec(action(4), 0..6),
        prop::collection::vec(action(4), 0..6),
        any::<usize>(),
        prop::collection::vec(
            (any::<bool>(), prop::collection::vec(action(4), 1..4)),
            0..6,
        ),
        prop::collection::vec(ruling(), 0..4),
    )
}

fn moved_onto_left_out_settles(
    (seed, local, remote, moved, during, rulings): MovedOntoLeftOut,
) -> Result<(), TestCaseError> {
    run_ignoring(
        &[vec![act(1, 0, 0, 0, 0)], seed].concat(),
        &[vec![act(0, 0, 0, 6, 0)], local].concat(),
        &[vec![act(3, moved, 0, 6, 0)], remote].concat(),
        MAX_OPS,
        false,
        &during,
        Cut::None,
        &[vec![Ruling::Exclude(vec!["ss/"])], rulings].concat(),
        Answer::Delete,
        None,
    )?;
    Ok(())
}

type RemovingMost = (usize, usize, Vec<Action>, usize, Answer, Cut);

fn removing_most() -> impl Strategy<Value = RemovingMost> {
    (
        11usize..20,
        9usize..11,
        prop::collection::vec(action(3), 0..4),
        prop_oneof![Just(MAX_OPS), 1usize..4],
        prop_oneof![Just(Answer::Delete), Just(Answer::Restore)],
        prop::sample::select(CUTS.to_vec()),
    )
}

fn removing_most_is_held(
    (inside, loose, local, fits, answer, cut): RemovingMost,
) -> Result<(), TestCaseError> {
    let mut seed = vec![act(0, 0, 0, 0, 0)];
    seed.extend((0..inside).map(|i| act(1, 0, 1, i % NAMES.len(), 0)));
    seed.extend((0..loose).map(|i| act(1, 0, 0, i % NAMES.len(), 0)));
    let (space, _) = seeded(&seed);
    let root = space.space_id().to_owned();
    let mut live: Vec<_> = (space.nodes().into_iter())
        .filter(|node| node.status == 1 && node.node_id != root)
        .collect();
    live.sort_by(|a, b| a.node_id.cmp(&b.node_id));
    prop_assert_eq!(
        live.len(),
        inside + loose + 1,
        "every seeded file has its own name"
    );
    let folder = live.iter().position(|node| node.kind == 1).unwrap();
    let mut actions = vec![act(4, folder, 0, 0, 0)];
    actions.extend(local);

    let ran = run_ignoring(
        &seed,
        &actions,
        &[],
        fits,
        true,
        &[],
        cut,
        &[],
        answer,
        None,
    )?;

    prop_assert!(ran.asked >= 1, "the removal was asked about");
    match answer {
        Answer::Restore => prop_assert_eq!((ran.deletes, ran.removals, ran.trashed), (0, 0, 0)),
        Answer::Delete => prop_assert_eq!(
            ran.trashed,
            inside + 1,
            "a Delete answer trashes the folder and every file in it"
        ),
        other => unreachable!("the guard does not offer {other}"),
    }
    Ok(())
}

type FirstLinkHeld = (Vec<Action>, Vec<Action>, usize, During, Answer, Cut);

fn first_link_held() -> impl Strategy<Value = FirstLinkHeld> {
    (
        prop::collection::vec(action(2), 1..12),
        prop::collection::vec(action(2), 1..8),
        prop_oneof![Just(MAX_OPS), 1usize..4],
        prop::collection::vec(
            (any::<bool>(), prop::collection::vec(action(5), 1..4)),
            0..6,
        ),
        prop_oneof![Just(Answer::Publish), Just(Answer::Discard)],
        prop::sample::select(CUTS.to_vec()),
    )
}

fn first_link_held_sends_nothing(
    (seed, local, fits, during, answer, cut): FirstLinkHeld,
) -> Result<(), TestCaseError> {
    run_ignoring(
        &seed,
        &local,
        &[],
        fits,
        false,
        &during,
        cut,
        &[],
        answer,
        Some(PriorSync::Shown),
    )?;
    Ok(())
}

type FreshFirstLink = (Vec<Action>, Vec<Action>, Vec<Action>, usize, Cut);

fn fresh_first_link() -> impl Strategy<Value = FreshFirstLink> {
    (
        prop::collection::vec(action(2), 1..12),
        prop::collection::vec(action(2), 1..8),
        prop::collection::vec(action(2), 0..8),
        prop_oneof![Just(MAX_OPS), 1usize..4],
        prop::sample::select(CUTS.to_vec()),
    )
}

fn fresh_first_link_asks_nothing(
    (seed, local, remote, fits, cut): FreshFirstLink,
) -> Result<(), TestCaseError> {
    let ran = run_ignoring(
        &seed,
        &local,
        &remote,
        fits,
        false,
        &[],
        cut,
        &[],
        Answer::Publish,
        Some(PriorSync::Fresh),
    )?;
    prop_assert_eq!((ran.asked, ran.conflicts), (0, 0));
    Ok(())
}

type FirstLinkLeftOut = (
    Vec<Action>,
    Vec<Action>,
    usize,
    During,
    Vec<Ruling>,
    Answer,
    Cut,
);

fn first_link_left_out() -> impl Strategy<Value = FirstLinkLeftOut> {
    (
        prop::collection::vec(action(2), 1..12),
        prop::collection::vec(action(2), 1..8),
        prop_oneof![Just(MAX_OPS), 1usize..4],
        prop::collection::vec(
            (any::<bool>(), prop::collection::vec(action(5), 1..4)),
            0..6,
        ),
        prop::collection::vec(ruling(), 0..7),
        prop_oneof![Just(Answer::Publish), Just(Answer::Discard)],
        prop::sample::select(CUTS.to_vec()),
    )
}

fn first_link_left_out_sends_nothing(
    (seed, local, fits, during, rulings, answer, cut): FirstLinkLeftOut,
) -> Result<(), TestCaseError> {
    run_ignoring(
        &seed,
        &local,
        &[],
        fits,
        false,
        &during,
        cut,
        &rulings,
        answer,
        Some(PriorSync::Shown),
    )?;
    Ok(())
}

type DiscardTakesAlong = (
    Vec<Action>,
    Vec<Action>,
    usize,
    During,
    Vec<Ruling>,
    usize,
    Cut,
);

fn discard_takes_along() -> impl Strategy<Value = DiscardTakesAlong> {
    (
        prop::collection::vec(action(2), 2..12),
        prop::collection::vec(action(2), 1..8),
        prop_oneof![Just(MAX_OPS), 1usize..4],
        prop::collection::vec(
            (
                any::<bool>(),
                prop::collection::vec(
                    action(10).prop_map(|action| Action {
                        kind: if matches!(action.kind, 5 | 6) {
                            7
                        } else {
                            action.kind
                        },
                        ..action
                    }),
                    1..4,
                ),
            ),
            0..10,
        ),
        prop::collection::vec(ruling(), 1..8),
        0usize..12,
        prop::sample::select(CUTS.to_vec()),
    )
}

fn discard_takes_along_only_untouched(
    (seed, local, fits, during, rulings, answer_after, cut): DiscardTakesAlong,
) -> Result<(), TestCaseError> {
    run_ignoring_answering_after(
        &seed,
        &local,
        &[],
        fits,
        false,
        &during,
        cut,
        &rulings,
        Answer::Discard,
        Some(PriorSync::Shown),
        answer_after,
    )?;
    Ok(())
}

type DiscardKeeps = (
    Vec<Action>,
    Vec<Action>,
    During,
    Action,
    During,
    Action,
    Action,
    Vec<&'static str>,
    usize,
    Cut,
);

fn discard_keeps() -> impl Strategy<Value = DiscardKeeps> {
    (
        prop::collection::vec(action(2), 2..8),
        prop::collection::vec(action(2), 1..6),
        prop::collection::vec(
            (any::<bool>(), prop::collection::vec(action(5), 1..3)),
            0..3,
        ),
        action(1).prop_map(|action| Action { kind: 8, ..action }),
        prop::collection::vec(
            (any::<bool>(), prop::collection::vec(action(5), 1..3)),
            0..3,
        ),
        action(1).prop_map(|action| Action { kind: 7, ..action }),
        action(1).prop_map(|action| Action { kind: 9, ..action }),
        prop::collection::vec(prop::sample::select(PATTERNS), 1..3),
        0usize..12,
        prop::sample::select(CUTS.to_vec()),
    )
}

fn discard_keeps_a_changed_folder(
    (seed, local, before, moved, gap, purged, changed, excluded, answer_after, cut): DiscardKeeps,
) -> Result<(), TestCaseError> {
    let (during, rulings) =
        the_server_drops_part_of_a_discarded_folder(before, moved, gap, purged, changed, excluded);
    run_ignoring_answering_after(
        &seed,
        &local,
        &[],
        MAX_OPS,
        false,
        &during,
        cut,
        &rulings,
        Answer::Discard,
        Some(PriorSync::Shown),
        answer_after,
    )?;
    Ok(())
}

proptest! {
    #![proptest_config(koidrive_test_support::battery_config(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/engine_sync.proptest-regressions"),
        1024,
    ))]

    #[test]
    fn a_device_with_local_changes_converges_with_the_server_and_keeps_every_write(
        inputs in local_changes(),
    ) {
        local_changes_converge(inputs)?;
    }

    #[test]
    fn a_device_whose_rules_change_deletes_nothing_on_either_side(inputs in rules_change()) {
        rules_change_deletes_nothing(inputs)?;
    }

    #[test]
    fn a_node_another_device_moves_onto_a_folder_the_rules_leave_out_here_settles(
        inputs in moved_onto_left_out(),
    ) {
        moved_onto_left_out_settles(inputs)?;
    }
}

proptest! {
    #![proptest_config(koidrive_test_support::battery_config(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/engine_sync.proptest-regressions"),
        128,
    ))]

    #[test]
    fn removing_most_of_a_space_is_held_until_the_person_answers(
        inputs in removing_most(),
    ) {
        removing_most_is_held(inputs)?;
    }
}

proptest! {
    #![proptest_config(koidrive_test_support::battery_config(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/engine_sync.proptest-regressions"),
        256,
    ))]

    #[test]
    fn a_first_link_sends_nothing_it_holds_until_the_person_publishes_it(
        inputs in first_link_held(),
    ) {
        first_link_held_sends_nothing(inputs)?;
    }

    #[test]
    fn a_fresh_first_link_asks_nothing_and_keeps_every_file_on_both_sides(
        inputs in fresh_first_link(),
    ) {
        fresh_first_link_asks_nothing(inputs)?;
    }

    #[test]
    fn a_first_link_sends_nothing_the_rules_leave_out_for_a_while_until_the_person_publishes_it(
        inputs in first_link_left_out(),
    ) {
        first_link_left_out_sends_nothing(inputs)?;
    }

    #[test]
    fn a_first_link_discard_takes_along_only_the_server_entries_the_user_leaves_alone(
        inputs in discard_takes_along(),
    ) {
        discard_takes_along_only_untouched(inputs)?;
    }
}

proptest! {
    #![proptest_config(koidrive_test_support::battery_config(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/engine_sync.proptest-regressions"),
        0,
    ))]

    #[test]
    fn a_first_link_discard_keeps_a_folder_the_user_changes_after_the_server_drops_part_of_it(
        inputs in discard_keeps(),
    ) {
        discard_keeps_a_changed_folder(inputs)?;
    }
}

const LEGACY_CASES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/engine_sync.proptest-regressions"
);

const SAVED_CASES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/engine_sync.cases");

#[derive(Clone, Debug, PartialEq)]
struct SavedCase(proptest::test_runner::PersistedSeed);

impl proptest::test_runner::FailurePersistence for SavedCase {
    fn load_persisted_failures2(
        &self,
        _: Option<&'static str>,
    ) -> Vec<proptest::test_runner::PersistedSeed> {
        vec![self.0.clone()]
    }

    fn save_persisted_failure2(
        &mut self,
        _: Option<&'static str>,
        _: proptest::test_runner::PersistedSeed,
        _: &dyn std::fmt::Debug,
    ) {
    }

    fn box_clone(&self) -> Box<dyn proptest::test_runner::FailurePersistence> {
        Box::new(self.clone())
    }

    fn eq(&self, other: &dyn proptest::test_runner::FailurePersistence) -> bool {
        other.as_any().downcast_ref::<Self>() == Some(self)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[derive(Debug)]
struct NewCases(Box<dyn proptest::test_runner::FailurePersistence>);

impl proptest::test_runner::FailurePersistence for NewCases {
    fn load_persisted_failures2(
        &self,
        _: Option<&'static str>,
    ) -> Vec<proptest::test_runner::PersistedSeed> {
        Vec::new()
    }

    fn save_persisted_failure2(
        &mut self,
        source: Option<&'static str>,
        seed: proptest::test_runner::PersistedSeed,
        value: &dyn std::fmt::Debug,
    ) {
        self.0.save_persisted_failure2(source, seed, value);
    }

    fn box_clone(&self) -> Box<dyn proptest::test_runner::FailurePersistence> {
        Box::new(NewCases(self.0.box_clone()))
    }

    fn eq(&self, other: &dyn proptest::test_runner::FailurePersistence) -> bool {
        other
            .as_any()
            .downcast_ref::<Self>()
            .is_some_and(|other| proptest::test_runner::FailurePersistence::eq(&*self.0, &*other.0))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

fn holds_with_field_intents<S: Strategy>(
    test: &'static str,
    cases: u32,
    strategy: S,
    property: fn(S::Value) -> Result<(), TestCaseError>,
) -> Vec<String> {
    let outcome =
        |value: S::Value| match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            property(value)
        })) {
            Ok(Ok(())) => None,
            Ok(Err(TestCaseError::Fail(reason) | TestCaseError::Reject(reason))) => {
                Some(reason.message().to_owned())
            }
            Err(panic) => Some(panic_message(&*panic)),
        };
    let intents = FIELD_INTENTS.replace(true);
    let mut saved: Vec<_> = (std::fs::read_dir(SAVED_CASES).expect("read the saved cases"))
        .map(|entry| entry.expect("read a saved case").path())
        .collect();
    saved.sort();
    let mut wrong = Vec::new();
    let mut failed_as_recorded = BTreeSet::new();
    let mut present = BTreeSet::new();
    for path in saved {
        let line = std::fs::read_to_string(&path).expect("read a saved case");
        let seed = (line.split('#').next()).and_then(|seed| seed.trim().parse().ok());
        let Some(seed) = seed else {
            panic!("{} holds no seed", path.display());
        };
        let case = (path.file_stem().and_then(|stem| stem.to_str())).expect("a case is named");
        present.insert(case.to_owned());
        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            test_name: Some(test),
            failure_persistence: Some(Box::new(SavedCase(seed))),
            fork: false,
            timeout: 0,
            ..ProptestConfig::with_cases(0)
        });
        let failure = std::cell::RefCell::new(None);
        (runner.run(&strategy, |value| {
            *failure.borrow_mut() = outcome(value);
            Ok(())
        }))
        .expect("a saved case runs");
        match failure.into_inner() {
            None => {}
            Some(failure) => match recorded_red(test, Some(case), &failure) {
                Some(red) => {
                    failed_as_recorded.insert((red.site, case.to_owned()));
                }
                None => wrong.push(format!("{test} {case} with field intents on: {failure}")),
            },
        }
    }
    for red in (INTENTS_ON_REDS.iter()).filter(|red| red.test == test) {
        let (Where::Case(case) | Where::AnyCase { witness: case }) = red.fails_in else {
            continue;
        };
        if present.contains(case) && !failed_as_recorded.contains(&(red.site, case.to_owned())) {
            wrong.push(format!(
                "{test} {case} no longer fails with field intents on as its {} entry in INTENTS_ON_REDS records: {}",
                red.issue, red.site
            ));
        }
    }
    if std::env::var_os("KOIDRIVE_BATTERY_CASES").is_some() {
        let mut config = koidrive_test_support::battery_config(LEGACY_CASES, cases);
        let saving = config.failure_persistence.take();
        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            test_name: Some(test),
            failure_persistence: saving.map(|saving| Box::new(NewCases(saving)) as _),
            ..config
        });
        let result = runner.run(&strategy, |value| match outcome(value) {
            Some(failure) if recorded_red(test, None, &failure).is_none() => {
                Err(TestCaseError::fail(failure))
            }
            _ => Ok(()),
        });
        match result {
            Ok(()) => {}
            Err(proptest::test_runner::TestError::Fail(reason, _)) => {
                wrong.push(format!(
                    "{test} with field intents on: {}",
                    reason.message()
                ));
            }
            Err(proptest::test_runner::TestError::Abort(reason)) => {
                wrong.push(format!(
                    "{test} with field intents on stopped: {}",
                    reason.message()
                ));
            }
        }
    }
    FIELD_INTENTS.set(intents);
    wrong
}

#[test]
fn every_intents_on_pin_names_a_saved_case() {
    let missing: Vec<&str> = (INTENTS_ON_REDS.iter())
        .filter_map(|red| match red.fails_in {
            Where::Test => None,
            Where::Case(case) | Where::AnyCase { witness: case } => Some(case),
        })
        .filter(|case| {
            !std::path::Path::new(SAVED_CASES)
                .join(format!("{case}.txt"))
                .exists()
        })
        .collect();
    assert!(
        missing.is_empty(),
        "INTENTS_ON_REDS names cases with no saved file: {missing:?}"
    );
}

#[test]
fn every_property_holds_with_field_intents_on() {
    let wrong = [
        holds_with_field_intents(
            "a_device_with_local_changes_converges_with_the_server_and_keeps_every_write",
            1024,
            local_changes(),
            local_changes_converge,
        ),
        holds_with_field_intents(
            "a_device_whose_rules_change_deletes_nothing_on_either_side",
            1024,
            rules_change(),
            rules_change_deletes_nothing,
        ),
        holds_with_field_intents(
            "a_node_another_device_moves_onto_a_folder_the_rules_leave_out_here_settles",
            1024,
            moved_onto_left_out(),
            moved_onto_left_out_settles,
        ),
        holds_with_field_intents(
            "removing_most_of_a_space_is_held_until_the_person_answers",
            128,
            removing_most(),
            removing_most_is_held,
        ),
        holds_with_field_intents(
            "a_first_link_sends_nothing_it_holds_until_the_person_publishes_it",
            256,
            first_link_held(),
            first_link_held_sends_nothing,
        ),
        holds_with_field_intents(
            "a_fresh_first_link_asks_nothing_and_keeps_every_file_on_both_sides",
            256,
            fresh_first_link(),
            fresh_first_link_asks_nothing,
        ),
        holds_with_field_intents(
            "a_first_link_sends_nothing_the_rules_leave_out_for_a_while_until_the_person_publishes_it",
            256,
            first_link_left_out(),
            first_link_left_out_sends_nothing,
        ),
        holds_with_field_intents(
            "a_first_link_discard_takes_along_only_the_server_entries_the_user_leaves_alone",
            256,
            discard_takes_along(),
            discard_takes_along_only_untouched,
        ),
        holds_with_field_intents(
            "a_first_link_discard_keeps_a_folder_the_user_changes_after_the_server_drops_part_of_it",
            0,
            discard_keeps(),
            discard_keeps_a_changed_folder,
        ),
    ]
    .concat();
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
