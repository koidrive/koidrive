// SPDX-License-Identifier: Apache-2.0

use super::*;

fn adv_action() -> impl Strategy<Value = Action> {
    (
        prop_oneof![3 => Just(0u8), 2 => Just(1u8), 1 => Just(2u8), 4 => Just(3u8), 2 => Just(4u8)],
        any::<usize>(),
        any::<usize>(),
        0..NAMES.len(),
        0..TEXTS,
    )
        .prop_map(|(kind, target, parent, name, text)| Action {
            kind,
            target,
            parent,
            name,
            text,
        })
}

fn adv_step(mixed: bool) -> BoxedStrategy<Step> {
    let cut = prop_oneof![Just(Cut::Quarantined), Just(Cut::Placed), Just(Cut::Done)];
    let outcome = prop_oneof![
        5 => Just(Outcome::Run),
        1 => Just(Outcome::Fail),
        4 => cut.prop_map(Outcome::Crash),
    ];
    let reply = prop_oneof![
        8 => Just(Reply::Served),
        1 => Just(Reply::Refused(error::Code::RetryLater)),
        1 => Just(Reply::NoChunks),
    ];
    let steps = prop_oneof![
        10 => adv_action().prop_map(Step::User),
        12 => any::<usize>().prop_map(Step::Work),
        2 => Just(Step::Scan),
        3 => outcome.prop_map(Step::Effect),
        4 => reply.prop_map(Step::Commit),
        2 => Just(Step::Connect),
        3 => any::<usize>().prop_map(Step::Deliver),
        1 => Just(Step::Drop),
        1 => Just(Step::Crash),
    ]
    .boxed();
    if mixed {
        prop_oneof![
            30 => steps,
            3 => Just(Step::Backup),
            2 => Just(Step::Restore),
            1 => Just(Step::RestoreEarlier),
            1 => Just(Step::Rewind),
            1 => Just(Step::CopyStore),
            1 => Just(Step::RollBack),
            2 => (0..4usize).prop_map(Step::Answer),
        ]
        .boxed()
    } else {
        prop_oneof![
            10 => steps,
            1 => Just(Step::Backup),
            1 => Just(Step::Restore),
        ]
        .boxed()
    }
}

fn adv_phases(mixed: bool) -> impl Strategy<Value = Vec<Vec<(usize, Step)>>> {
    prop::collection::vec(
        prop::collection::vec((0..3usize, adv_step(mixed)), 0..56),
        1..=3,
    )
}

proptest! {
    #![proptest_config(koidrive_test_support::battery_config(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/devices.proptest-regressions"),
        64,
    ))]

    #[test]
    fn adv_folders_converge_across_restores(devices in 2..=3usize, phases in adv_phases(false)) {
        converge(devices, &phases).map_err(|failure| TestCaseError::fail(failure.to_string()))?;
    }

    #[test]
    fn adv_folders_lose_no_write_across_restores(devices in 2..=3usize, phases in adv_phases(false)) {
        keep(devices, &phases).map_err(|failure| TestCaseError::fail(failure.to_string()))?;
    }

    #[test]
    fn adv_folders_converge_across_restores_and_rewinds(devices in 2..=3usize, phases in adv_phases(true)) {
        converge(devices, &phases).map_err(|failure| TestCaseError::fail(failure.to_string()))?;
    }

    #[test]
    fn adv_folders_lose_no_write_across_restores_and_rewinds(devices in 2..=3usize, phases in adv_phases(true)) {
        keep(devices, &phases).map_err(|failure| TestCaseError::fail(failure.to_string()))?;
    }
}

fn adv_lost_folder_prefix() -> Vec<(usize, Step)> {
    let mut prefix = vec![(0, Step::Backup), (1, folder("A")), (1, rename(0, 1, "a"))];
    prefix.extend(commit(1));
    prefix.extend([(0, Step::Work(0)); 8]);
    prefix
}

fn adv_after_lost_folder(mixed: bool) -> impl Strategy<Value = Vec<Vec<(usize, Step)>>> {
    (
        adv_phases(mixed),
        any::<prop::sample::Index>(),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(|(mut phases, at, edit, restore_now)| {
            let mut prefix = adv_lost_folder_prefix();
            if edit {
                prefix.push((0, overwrite(0)));
            }
            if restore_now {
                prefix.push((0, Step::Restore));
            }
            let first = &mut phases[0];
            let at = at.index(first.len() + 1);
            first.splice(at..at, prefix);
            phases.insert(0, vec![(0, file("a"))]);
            phases
        })
}

proptest! {
    #![proptest_config(koidrive_test_support::battery_config(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/devices.proptest-regressions"),
        64,
    ))]

    #[test]
    fn adv_folders_after_a_lost_folder_converge(devices in 2..=3usize, phases in adv_after_lost_folder(false)) {
        converge(devices, &phases).map_err(|failure| TestCaseError::fail(failure.to_string()))?;
    }

    #[test]
    fn adv_folders_after_a_lost_folder_converge_across_rewinds(devices in 2..=3usize, phases in adv_after_lost_folder(true)) {
        converge(devices, &phases).map_err(|failure| TestCaseError::fail(failure.to_string()))?;
    }
}

#[test]
fn adv_lost_folder_prefix_alone_converges() {
    let mut lose = adv_lost_folder_prefix();
    lose.push((0, Step::Restore));
    let world = converge(2, &[vec![(0, file("a"))], lose]).unwrap();
    for device in 0..2 {
        let names: Vec<String> = world.devices[device]
            .disk
            .entries()
            .values()
            .map(|e| e.name.clone())
            .collect();
        println!("device {device}: {names:?}");
    }
}

fn adv_differential<S: Strategy<Value = Vec<Vec<(usize, Step)>>>>(label: &str, strategy: S) {
    let cases: u64 = std::env::var("KOIDRIVE_ADV_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let base: u64 = std::env::var("KOIDRIVE_ADV_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(646_000);
    let strategy = (2..=3usize, strategy);
    let mut failed = Vec::new();
    for i in 0..cases {
        let mut runner = TestRunner::new(ProptestConfig {
            rng_seed: RngSeed::Fixed(base + i),
            failure_persistence: None,
            ..ProptestConfig::default()
        });
        let (devices, phases) = strategy.new_tree(&mut runner).unwrap().current();
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| converge(devices, &phases)));
        let message = match result {
            Ok(Ok(_)) => continue,
            Ok(Err(failure)) => failure.to_string(),
            Err(panic) => format!(
                "PANIC {}",
                panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_default()
            ),
        };
        let short: String = message
            .chars()
            .take(140)
            .collect::<String>()
            .replace('\n', " ");
        failed.push(format!("ADV\t{label}\t{}\t{short}", base + i));
    }
    panic!(
        "ADV-SUMMARY {label}: {} of {cases} failed\n{}\nADV-END",
        failed.len(),
        failed.join("\n")
    );
}

#[test]
fn adv_diff_restores() {
    adv_differential("restores", phases(Losses::Restores));
}

#[test]
fn adv_diff_restores_and_rewinds() {
    adv_differential("mixed", phases(Losses::RestoresAndRewinds));
}

#[test]
fn adv_diff_folders_restores() {
    adv_differential("folders", adv_phases(false));
}

#[test]
fn adv_diff_folders_mixed() {
    adv_differential("folders-mixed", adv_phases(true));
}

#[test]
fn adv_diff_lost_folder() {
    adv_differential("lost-folder", adv_after_lost_folder(false));
}

#[test]
fn adv_diff_lost_folder_mixed() {
    adv_differential("lost-folder-mixed", adv_after_lost_folder(true));
}
