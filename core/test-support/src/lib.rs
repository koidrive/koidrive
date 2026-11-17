// SPDX-License-Identifier: Apache-2.0

use std::any::Any;
use std::collections::BTreeSet;
use std::env::{self, VarError};
use std::fmt::Debug;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use proptest::test_runner::{Config, FailurePersistence, PersistedSeed, RngSeed};

mod disks;
pub use disks::{Disks, is_volume_metadata};
mod net;
pub use net::reset_tcp_address;

const DETERMINISTIC: &str = "KOIDRIVE_PROPTEST_DETERMINISTIC";
const FIXED_SEED: u64 = 0;

/// Replays saved regressions beside `legacy` and saves each new failure in its own file.
/// With `KOIDRIVE_PROPTEST_DETERMINISTIC=true`, the generated cases come from seed 0.
pub fn regression_config(legacy: &'static str, cases: u32) -> Config {
    let default = Config::with_cases(cases);
    Config {
        failure_persistence: Some(Box::new(RegressionCases(PathBuf::from(legacy)))),
        rng_seed: rng_seed(env::var(DETERMINISTIC), default.rng_seed),
        ..default
    }
}

/// Uses `KOIDRIVE_BATTERY_CASES` for stress properties, retaining regression persistence and seeds.
/// Without that variable, uses the property's own case budget.
pub fn battery_config(legacy: &'static str, cases: u32) -> Config {
    let cases = match env::var("KOIDRIVE_BATTERY_CASES") {
        Err(VarError::NotPresent) => cases,
        Ok(value) => value
            .parse::<u32>()
            .expect("KOIDRIVE_BATTERY_CASES must be an unsigned 32-bit integer"),
        Err(error) => panic!("KOIDRIVE_BATTERY_CASES must be an unsigned 32-bit integer: {error}"),
    };
    regression_config(legacy, cases)
}

fn rng_seed(deterministic: Result<String, VarError>, default: RngSeed) -> RngSeed {
    match deterministic.as_deref() {
        Err(VarError::NotPresent) | Ok("false") => default,
        Ok("true") => RngSeed::Fixed(FIXED_SEED),
        other => panic!("{DETERMINISTIC} must be true or false, got {other:?}"),
    }
}

#[derive(Clone, Debug, PartialEq)]
struct RegressionCases(PathBuf);

impl RegressionCases {
    fn load(&self) -> Vec<PersistedSeed> {
        let mut seeds = BTreeSet::new();
        seeds.extend(read_seeds(&self.0));
        let directory = self.0.with_extension("cases");
        let mut paths: Vec<_> = match fs::read_dir(&directory) {
            Ok(entries) => entries
                .map(|entry| entry.expect("read regression directory entry").path())
                .filter(|path| path.extension().is_some_and(|extension| extension == "txt"))
                .collect(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("read {}: {error}", directory.display()),
        };
        paths.sort();
        for path in paths {
            let saved = read_seeds(&path);
            assert_eq!(saved.len(), 1, "{} must hold one seed", path.display());
            assert_eq!(
                path.file_name().unwrap(),
                case_name(&saved[0]).as_str(),
                "{} must be named after its seed",
                path.display()
            );
            seeds.extend(saved);
        }
        eprintln!(
            "proptest regressions: replay {} saved cases for {}",
            seeds.len(),
            self.0.display()
        );
        for seed in &seeds {
            eprintln!("proptest regression: {seed}");
        }
        seeds.into_iter().collect()
    }

    fn save(&self, seed: &PersistedSeed, value: &dyn Debug) -> io::Result<()> {
        let directory = self.0.with_extension("cases");
        fs::create_dir_all(&directory)?;
        let path = directory.join(case_name(seed));
        let mut file = tempfile::NamedTempFile::new_in(&directory)?;
        let value = format!("{value:?}").replace(['\r', '\n'], " ");
        writeln!(file, "{seed} # shrinks to {value}")?;
        match file.persist_noclobber(&path) {
            Ok(_) => (),
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                if read_seeds(&path) != [seed.clone()] {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{} holds a different seed", path.display()),
                    ));
                }
            }
            Err(error) => return Err(error.error),
        }
        eprintln!("proptest regression saved in {}: {seed}", path.display());
        Ok(())
    }
}

fn case_name(seed: &PersistedSeed) -> String {
    format!("{}.txt", seed.to_string().replace(' ', "-"))
}

fn read_seeds(path: &Path) -> Vec<PersistedSeed> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => panic!("read {}: {error}", path.display()),
    };
    text.lines()
        .enumerate()
        .filter_map(|(line, text)| {
            let text = text.split('#').next().unwrap().trim();
            if text.is_empty() {
                return None;
            }
            Some(text.parse().unwrap_or_else(|()| {
                panic!("invalid regression seed in {}:{}", path.display(), line + 1)
            }))
        })
        .collect()
}

impl FailurePersistence for RegressionCases {
    fn load_persisted_failures2(&self, _: Option<&'static str>) -> Vec<PersistedSeed> {
        self.load()
    }

    fn save_persisted_failure2(
        &mut self,
        _: Option<&'static str>,
        seed: PersistedSeed,
        value: &dyn Debug,
    ) {
        if let Err(error) = self.save(&seed, value) {
            eprintln!("cannot save proptest regression {seed}: {error}");
        }
    }

    fn box_clone(&self) -> Box<dyn FailurePersistence> {
        Box::new(self.clone())
    }

    fn eq(&self, other: &dyn FailurePersistence) -> bool {
        other.as_any().downcast_ref::<Self>() == Some(self)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::strategy::{Strategy, ValueTree};
    use proptest::test_runner::{TestCaseError, TestRunner};
    use std::cell::Cell;

    fn seed(byte: char) -> PersistedSeed {
        format!("cc {}", byte.to_string().repeat(64))
            .parse()
            .unwrap()
    }

    #[test]
    fn saved_cases_keep_a_single_line_shrink_description() {
        struct Value;
        impl Debug for Value {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("first\nsecond\rthird")
            }
        }

        let root = tempfile::tempdir().unwrap();
        let mut cases = RegressionCases(root.path().join("property.txt"));
        cases.save_persisted_failure2(None, seed('1'), &Value);
        let saved = cases.0.with_extension("cases").join(case_name(&seed('1')));
        assert_eq!(
            fs::read_to_string(saved).unwrap(),
            format!("{} # shrinks to first second third\n", seed('1'))
        );
        assert_eq!(cases.load(), vec![seed('1')]);
    }

    #[test]
    fn independent_failures_add_independent_immutable_files() {
        let root = tempfile::tempdir().unwrap();
        let cases = RegressionCases(root.path().join("property.txt"));
        cases.save(&seed('1'), &()).unwrap();
        let first = cases.0.with_extension("cases").join(case_name(&seed('1')));
        fs::write(&first, format!("{} # shrinks to original\n", seed('1'))).unwrap();
        let original = fs::read(&first).unwrap();
        cases.save(&seed('2'), &()).unwrap();
        cases.save(&seed('1'), &"replacement").unwrap();
        assert_eq!(fs::read(first).unwrap(), original);
        assert_eq!(
            fs::read_dir(cases.0.with_extension("cases"))
                .unwrap()
                .count(),
            2
        );
        assert_eq!(cases.load(), vec![seed('1'), seed('2')]);
    }

    #[test]
    fn legacy_appends_and_case_files_replay_once_each() {
        let root = tempfile::tempdir().unwrap();
        let cases = RegressionCases(root.path().join("property.txt"));
        fs::write(
            &cases.0,
            format!("# legacy\n{} # shrinks to old\n{}\n", seed('2'), seed('1')),
        )
        .unwrap();
        cases.save(&seed('2'), &()).unwrap();
        cases.save(&seed('3'), &()).unwrap();
        assert_eq!(cases.load(), vec![seed('1'), seed('2'), seed('3')]);
    }

    #[test]
    fn concurrent_saves_leave_complete_cases_and_ignore_unfinished_files() {
        let root = tempfile::tempdir().unwrap();
        let cases = RegressionCases(root.path().join("property.txt"));
        assert!(cases.load().is_empty());
        std::thread::scope(|scope| {
            for byte in ['1', '2', '1', '3'] {
                let cases = &cases;
                scope.spawn(move || cases.save(&seed(byte), &()).unwrap());
            }
        });
        fs::write(
            cases.0.with_extension("cases").join(".unfinished"),
            "cc broken",
        )
        .unwrap();
        assert_eq!(cases.load(), vec![seed('1'), seed('2'), seed('3')]);
    }

    #[test]
    #[should_panic(expected = "must be named after its seed")]
    fn a_filename_cannot_hide_a_different_seed() {
        let root = tempfile::tempdir().unwrap();
        let cases = RegressionCases(root.path().join("property.txt"));
        cases.save(&seed('1'), &()).unwrap();
        fs::write(
            cases.0.with_extension("cases").join(case_name(&seed('1'))),
            format!("{}\n", seed('2')),
        )
        .unwrap();
        cases.load();
    }

    #[test]
    #[should_panic(expected = "must hold one seed")]
    fn a_case_file_cannot_hide_multiple_seeds() {
        let root = tempfile::tempdir().unwrap();
        let cases = RegressionCases(root.path().join("property.txt"));
        cases.save(&seed('1'), &()).unwrap();
        fs::write(
            cases.0.with_extension("cases").join(case_name(&seed('1'))),
            format!("{}\n{}\n", seed('1'), seed('2')),
        )
        .unwrap();
        cases.load();
    }

    #[test]
    fn shrinking_saves_a_case_that_replays_with_zero_random_cases() {
        let root = tempfile::tempdir().unwrap();
        let cases = RegressionCases(root.path().join("property.txt"));
        let config = Config {
            cases: 1,
            failure_persistence: Some(Box::new(cases.clone())),
            ..Config::default()
        };
        let failure =
            TestRunner::new(config).run(&(1u8..10), |_| Err(TestCaseError::fail("mutant")));
        assert!(failure.is_err());
        assert_eq!(cases.load().len(), 1);
        let config = Config {
            cases: 0,
            failure_persistence: Some(Box::new(cases)),
            ..Config::default()
        };
        let runs = Cell::new(0);
        TestRunner::new(config.clone())
            .run(&(1u8..10), |_| {
                runs.set(runs.get() + 1);
                Ok(())
            })
            .unwrap();
        assert_eq!(runs.get(), 1);
        assert!(
            TestRunner::new(config)
                .run(&(1u8..10), |_| Err(TestCaseError::fail("mutant")))
                .is_err()
        );
    }

    fn generated(config: Config) -> Vec<u64> {
        let mut runner = TestRunner::new(config);
        (0..16)
            .map(|_| {
                proptest::num::u64::ANY
                    .new_tree(&mut runner)
                    .unwrap()
                    .current()
            })
            .collect()
    }

    #[test]
    fn deterministic_mode_generates_the_same_cases_on_every_run() {
        let config = || Config {
            rng_seed: rng_seed(Ok("true".into()), RngSeed::Random),
            ..Config::default()
        };
        assert_eq!(generated(config()), generated(config()));
    }

    #[test]
    fn exploring_mode_keeps_the_default_seed() {
        assert_eq!(
            rng_seed(Err(VarError::NotPresent), RngSeed::Random),
            RngSeed::Random
        );
        assert_eq!(
            rng_seed(Ok("false".into()), RngSeed::Fixed(7)),
            RngSeed::Fixed(7)
        );
    }

    #[test]
    #[should_panic(expected = "must be true or false")]
    fn an_unknown_mode_fails_instead_of_exploring() {
        rng_seed(Ok("1".into()), RngSeed::Random);
    }

    #[test]
    #[should_panic(expected = "invalid regression seed")]
    fn malformed_regressions_fail_instead_of_disappearing() {
        let root = tempfile::tempdir().unwrap();
        let cases = RegressionCases(root.path().join("property.txt"));
        cases.save(&seed('1'), &()).unwrap();
        fs::write(
            cases.0.with_extension("cases").join(case_name(&seed('1'))),
            "cc broken\n",
        )
        .unwrap();
        cases.load();
    }

    #[test]
    fn the_build_keeps_debug_assertions_and_overflow_checks() {
        let debug_assertion =
            std::panic::catch_unwind(|| debug_assert!(std::hint::black_box(false)));
        let overflow =
            std::panic::catch_unwind(|| std::hint::black_box(u8::MAX) + std::hint::black_box(1));
        assert!(debug_assertion.is_err(), "debug_assert! did not fire");
        assert!(overflow.is_err(), "an integer overflow did not panic");
    }
}
