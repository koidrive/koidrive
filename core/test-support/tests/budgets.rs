// SPDX-License-Identifier: Apache-2.0

use std::process::{Command, Output};

fn probe(test: &str, variables: &[(&str, &str)], saved: bool) -> Output {
    let root = tempfile::tempdir().unwrap();
    if saved {
        std::fs::write(
            root.path().join("regressions.txt"),
            format!("cc {}\n", "1".repeat(64)),
        )
        .unwrap();
    }
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .current_dir(root.path())
        .args(["--ignored", "--exact", test, "--nocapture"]);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("PROPTEST_")
            || key.to_string_lossy().starts_with("KOIDRIVE_")
        {
            command.env_remove(key);
        }
    }
    command
        .env("PROPTEST_RNG_SEED", "7")
        .envs(variables.iter().copied());
    command.output().unwrap()
}

fn values(test: &str, variables: &[(&str, &str)], saved: bool) -> Vec<String> {
    let output = probe(test, variables, saved);
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| line.strip_prefix("budget-case: ").map(str::to_owned))
        .collect()
}

#[test]
fn battery_budgets_require_explicit_opt_in_and_can_be_lowered() {
    for budget in [None, Some("9"), Some("2"), Some("0")] {
        let variables: Vec<_> = budget
            .map(|value| ("KOIDRIVE_BATTERY_CASES", value))
            .into_iter()
            .collect();
        assert_eq!(values("fixtures::bounded", &variables, false).len(), 4);
        assert_eq!(
            values("fixtures::stress", &variables, false).len(),
            budget.map_or(4, |value| value.parse().unwrap())
        );
        assert_eq!(values("fixtures::replay_only", &variables, false).len(), 0);
        assert_eq!(values("fixtures::replay_only", &variables, true).len(), 1);
    }
}

#[test]
fn malformed_battery_budgets_fail_only_opted_in_properties() {
    for budget in ["", "many", "-1", "4294967296"] {
        let variables = [("KOIDRIVE_BATTERY_CASES", budget)];
        assert_eq!(values("fixtures::bounded", &variables, false).len(), 4);
        let output = probe("fixtures::stress", &variables, false);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("KOIDRIVE_BATTERY_CASES"));
    }
}

#[test]
fn battery_configuration_preserves_saved_cases_and_seed_selection() {
    let expected = values("fixtures::bounded", &[], true);
    for deterministic in ["false", "true"] {
        let variables = [
            ("KOIDRIVE_BATTERY_CASES", "4"),
            ("KOIDRIVE_PROPTEST_DETERMINISTIC", deterministic),
        ];
        let bounded = values("fixtures::bounded", &variables, true);
        assert_eq!(bounded.len(), 5);
        assert_eq!(bounded, expected);
        assert_eq!(bounded, values("fixtures::stress", &variables, true));
        assert_eq!(bounded, values("fixtures::bounded", &variables, true));
        let output = probe("fixtures::configuration", &variables, false);
        assert!(output.status.success(), "{output:?}");
    }
}

#[test]
fn ordinary_proptest_zero_replays_saved_cases_for_both_budgets() {
    let variables = [("KOIDRIVE_BATTERY_CASES", "9"), ("PROPTEST_CASES", "0")];
    for test in ["fixtures::bounded", "fixtures::stress"] {
        assert_eq!(values(test, &variables, false).len(), 0);
        assert_eq!(values(test, &variables, true).len(), 1);
    }
}

mod fixtures {
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(koidrive_test_support::regression_config("regressions.txt", 4))]
        #[test]
        #[ignore]
        fn bounded(value in any::<u64>()) {
            println!("budget-case: {value}");
        }
    }

    proptest! {
        #![proptest_config(koidrive_test_support::battery_config("regressions.txt", 4))]
        #[test]
        #[ignore]
        fn stress(value in any::<u64>()) {
            println!("budget-case: {value}");
        }
    }

    #[test]
    #[ignore]
    fn replay_only() {
        let mut runner = proptest::test_runner::TestRunner::new(
            koidrive_test_support::regression_config("regressions.txt", 0),
        );
        runner
            .run(&any::<u64>(), |value| {
                println!("budget-case: {value}");
                Ok(())
            })
            .unwrap();
    }

    #[test]
    #[ignore]
    fn configuration() {
        let bounded = koidrive_test_support::regression_config("regressions.txt", 4);
        let stress = koidrive_test_support::battery_config("regressions.txt", 4);
        assert_eq!(bounded.rng_seed, stress.rng_seed);
        assert_eq!(
            bounded.failure_persistence.as_ref(),
            stress.failure_persistence.as_ref()
        );
        assert_eq!(
            bounded.rng_seed,
            proptest::test_runner::RngSeed::Fixed(
                if std::env::var("KOIDRIVE_PROPTEST_DETERMINISTIC").unwrap() == "true" {
                    0
                } else {
                    7
                }
            )
        );
    }
}
