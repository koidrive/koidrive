// SPDX-License-Identifier: Apache-2.0

use camino::Utf8Path;
use determinator::Determinator;
use determinator::rules::{
    DeterminatorMarkChanged, DeterminatorPostRule, DeterminatorRules, PathMatch, PathRule,
    RuleIndex,
};
use guppy::MetadataCommand;
use guppy::graph::cargo::CargoOptions;
use guppy::graph::feature::StandardFeatures;
use guppy::graph::{DependencyDirection, PackageGraph};
use guppy::platform::PlatformSpec;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::process::ExitCode;
use std::{env, fs};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    path: Vec<PathEntry>,
    job: BTreeMap<String, Job>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PathEntry {
    globs: Vec<String>,
    #[serde(default)]
    packages: Vec<String>,
    #[serde(default)]
    jobs: Jobs,
}

#[derive(Deserialize, Default)]
#[serde(untagged)]
enum Jobs {
    All(All),
    #[default]
    None,
    Named(Vec<String>),
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum All {
    All,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Job {
    #[serde(default)]
    packages: Packages,
    #[serde(default)]
    except: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(untagged)]
enum Packages {
    Any(Any),
    #[default]
    None,
    Named(Vec<String>),
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Any {
    Any,
}

#[derive(Debug, PartialEq)]
enum PathOutcome {
    Everything,
    Entry(usize),
    Package,
    Ignored,
}

fn rules(config: &Config) -> DeterminatorRules {
    let mut rules = DeterminatorRules::parse("").expect("empty rules parse");
    rules.path_rules = config
        .path
        .iter()
        .map(|entry| PathRule {
            globs: entry.globs.clone(),
            mark_changed: match entry.jobs {
                Jobs::All(_) => DeterminatorMarkChanged::All,
                _ => DeterminatorMarkChanged::Packages(entry.packages.clone()),
            },
            post_rule: DeterminatorPostRule::Skip,
        })
        .collect();
    rules
}

fn outcome(matched: PathMatch) -> PathOutcome {
    match matched {
        PathMatch::RuleMatchedAll | PathMatch::NoMatches => PathOutcome::Everything,
        PathMatch::RuleMatched(RuleIndex::CustomPath(index)) => PathOutcome::Entry(index),
        PathMatch::RuleMatched(_) => PathOutcome::Ignored,
        PathMatch::AncestorMatched => PathOutcome::Package,
    }
}

fn select(
    config: &Config,
    affected: &BTreeSet<String>,
    paths: &[(String, PathOutcome)],
) -> BTreeMap<String, Option<String>> {
    config
        .job
        .iter()
        .map(|(name, job)| {
            let everything = paths
                .iter()
                .find(|(_, outcome)| *outcome == PathOutcome::Everything)
                .map(|(path, _)| format!("{path} changes what every job depends on"));
            let by_path = || {
                paths.iter().find_map(|(path, outcome)| match outcome {
                    PathOutcome::Entry(index) => match &config.path[*index].jobs {
                        Jobs::Named(jobs) if jobs.contains(name) => {
                            Some(format!("{path} is an input of this job"))
                        }
                        _ => None,
                    },
                    _ => None,
                })
            };
            let by_package = || {
                affected
                    .iter()
                    .find(|package| match &job.packages {
                        Packages::Any(_) => !job.except.contains(package),
                        Packages::Named(packages) => packages.contains(package),
                        Packages::None => false,
                    })
                    .map(|package| format!("the change affects {package}"))
            };
            (
                name.clone(),
                everything.or_else(by_path).or_else(by_package),
            )
        })
        .collect()
}

fn check(config: &Config, graph: &PackageGraph) -> Result<(), String> {
    let workspace = graph.workspace();
    for entry in &config.path {
        if let Jobs::Named(jobs) = &entry.jobs
            && let Some(job) = jobs.iter().find(|job| !config.job.contains_key(*job))
        {
            return Err(format!(
                "path rule {:?} names the unknown job {job}",
                entry.globs
            ));
        }
    }
    for (name, job) in &config.job {
        let named = match &job.packages {
            Packages::Named(packages) => packages.iter().chain(&job.except).collect::<Vec<_>>(),
            _ => job.except.iter().collect(),
        };
        if let Some(package) = named
            .into_iter()
            .find(|package| !workspace.contains_name(package.as_str()))
        {
            return Err(format!(
                "job {name} names {package}, which is not a workspace package"
            ));
        }
    }
    Ok(())
}

fn graph(dir: &str) -> Result<PackageGraph, String> {
    let mut command = MetadataCommand::new();
    command
        .current_dir(dir)
        .other_options(["--locked".to_string()]);
    command
        .build_graph()
        .map_err(|error| format!("cargo metadata in {dir} failed: {error}"))
}

fn load(path: &str) -> Result<Config, String> {
    let text = fs::read_to_string(path).map_err(|error| format!("{path}: {error}"))?;
    toml::from_str(&text).map_err(|error| format!("{path}: {error}"))
}

fn run(args: &[String]) -> Result<BTreeMap<String, Option<String>>, String> {
    let [config_path, base_dir, head_dir, changed_path] = args else {
        return Err("usage: ci-select CONFIG BASE_CHECKOUT HEAD_CHECKOUT CHANGED_PATHS_NUL".into());
    };
    let config = load(config_path)?;
    let changed = fs::read(changed_path).map_err(|error| format!("{changed_path}: {error}"))?;
    let changed = String::from_utf8(changed).map_err(|error| format!("{changed_path}: {error}"))?;
    let changed: Vec<&str> = changed
        .split('\0')
        .filter(|path| !path.is_empty())
        .collect();
    compute(&config, &graph(base_dir)?, &graph(head_dir)?, &changed)
}

fn compute(
    config: &Config,
    base: &PackageGraph,
    head: &PackageGraph,
    changed: &[&str],
) -> Result<BTreeMap<String, Option<String>>, String> {
    check(config, head)?;
    let rules = rules(config);
    let changed: Vec<&Utf8Path> = changed.iter().copied().map(Utf8Path::new).collect();
    let mut options = CargoOptions::new();
    options
        .set_include_dev(true)
        .set_platform(PlatformSpec::Any);
    let mut determinator = Determinator::new(base, head);
    determinator
        .set_rules(&rules)
        .map_err(|error| format!("ci-select config: {error}"))?
        .set_cargo_options(&options)
        .set_features_only(
            head.workspace().iter().map(|package| package.name()),
            StandardFeatures::All,
        )
        .map_err(|error| error.to_string())?
        .add_changed_paths(changed.iter().copied());
    let affected = determinator
        .compute()
        .affected_set
        .packages(DependencyDirection::Forward)
        .map(|package| package.name().to_string())
        .collect();
    let paths: Vec<_> = changed
        .iter()
        .map(|path| {
            (
                path.to_string(),
                outcome(determinator.match_path(path, |_| {})),
            )
        })
        .collect();
    Ok(select(config, &affected, &paths))
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match run(&args) {
        Ok(selection) => {
            for (job, reason) in &selection {
                println!("{job}={}", reason.is_some());
                match reason {
                    Some(reason) => eprintln!("{job}: runs, because {reason}"),
                    None => eprintln!(
                        "{job}: skipped, because the change reaches none of its packages or paths"
                    ),
                }
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("::error::{error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::LazyLock;

    const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");

    static CONFIG: LazyLock<Config> =
        LazyLock::new(|| load(&format!("{ROOT}/.github/ci-select.toml")).unwrap());
    static GRAPH: LazyLock<PackageGraph> = LazyLock::new(|| graph(ROOT).unwrap());

    fn running(changed: &[&str]) -> BTreeSet<String> {
        compute(&CONFIG, &GRAPH, &GRAPH, changed)
            .unwrap()
            .into_iter()
            .filter_map(|(job, reason)| reason.map(|_| job))
            .collect()
    }

    fn jobs(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    fn every_job() -> BTreeSet<String> {
        CONFIG.job.keys().cloned().collect()
    }

    #[test]
    fn docs_run_no_selected_job() {
        assert_eq!(
            running(&["docs/deploy.md", "README.md", "core/engine/README.md"]),
            jobs(&[])
        );
    }

    #[test]
    fn a_path_without_a_rule_outside_every_package_runs_every_job() {
        assert_eq!(
            running(&["docs/deploy.md", "notes/new-tool.sh"]),
            every_job()
        );
    }

    #[test]
    fn workflow_toolchain_and_selector_changes_run_every_job() {
        for path in [
            ".github/workflows/ci.yml",
            ".github/ci-select.toml",
            "scripts/ci-select/src/main.rs",
            "rust-toolchain.toml",
            "Cargo.toml",
        ] {
            assert_eq!(running(&[path]), every_job(), "{path}");
        }
    }

    #[test]
    fn an_engine_change_runs_every_rust_job_that_builds_the_engine() {
        let running = running(&["core/engine/src/lib.rs"]);
        for job in [
            "rust",
            "rust_engine",
            "rust_driver",
            "rust_sim",
            "cli_integration",
            "ffi_integration",
            "macos_app",
        ] {
            assert!(running.contains(job), "{job} must run: {running:?}");
        }
        for job in ["server", "server_deploy"] {
            assert!(!running.contains(job), "{job} must not run: {running:?}");
        }
    }

    #[test]
    fn a_sim_test_change_runs_the_suites_that_build_the_sim() {
        assert_eq!(
            running(&["core/sim/tests/devices.rs"]),
            jobs(&[
                "driver_integration",
                "rust_driver",
                "rust_engine",
                "rust_sim"
            ])
        );
    }

    #[test]
    fn a_server_change_runs_the_server_and_every_integration_job() {
        assert_eq!(
            running(&["server/src/index.ts"]),
            jobs(&[
                "server",
                "server_deploy",
                "net_integration",
                "driver_integration",
                "cli_integration",
                "ffi_integration"
            ])
        );
    }

    #[test]
    fn a_spec_change_runs_the_server_and_the_rust_jobs_that_read_it() {
        let running = running(&["spec/testdata/scenarios/space-conformance-v1.json"]);
        for job in ["server", "rust_sim", "rust_engine", "rust_driver"] {
            assert!(running.contains(job), "{job} must run: {running:?}");
        }
    }

    #[test]
    fn an_app_change_runs_only_the_app() {
        assert_eq!(
            running(&["apps/macos/Koidrive/App.swift"]),
            jobs(&["macos_app"])
        );
    }

    #[test]
    fn every_job_is_an_output_ci_yml_reads() {
        let workflow = fs::read_to_string(format!("{ROOT}/.github/workflows/ci.yml")).unwrap();
        for job in CONFIG.job.keys() {
            assert!(
                workflow.contains(&format!("needs.changes.outputs.{job} ")),
                "ci.yml never reads needs.changes.outputs.{job}"
            );
        }
    }

    #[test]
    fn no_path_rule_covers_a_file_inside_a_package() {
        let output = Command::new("git")
            .args(["ls-files", "-z"])
            .current_dir(ROOT)
            .output()
            .unwrap();
        assert!(output.status.success());
        let files = String::from_utf8(output.stdout).unwrap();
        let determinator = Determinator::new(&GRAPH, &GRAPH);
        let rules = rules(&CONFIG);
        let mut determinator = determinator;
        determinator.set_rules(&rules).unwrap();
        let workspace = GRAPH.workspace();
        let inside = |path: &Utf8Path| {
            path.ancestors()
                .skip(1)
                .any(|dir| workspace.member_by_path(dir).is_ok())
        };
        for path in files
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(Utf8Path::new)
        {
            if inside(path) {
                assert!(
                    !matches!(
                        outcome(determinator.match_path(path, |_| {})),
                        PathOutcome::Entry(_)
                    ),
                    "{path} is inside a package, but a rule in ci-select.toml takes it away from that package"
                );
            }
        }
    }

    #[test]
    fn every_tracked_file_has_a_package_or_a_rule() {
        let output = Command::new("git")
            .args(["ls-files", "-z"])
            .current_dir(ROOT)
            .output()
            .unwrap();
        let files = String::from_utf8(output.stdout).unwrap();
        let rules = rules(&CONFIG);
        let mut determinator = Determinator::new(&GRAPH, &GRAPH);
        determinator.set_rules(&rules).unwrap();
        let unruled: Vec<_> = files
            .split('\0')
            .filter(|path| !path.is_empty())
            .filter(|path| {
                determinator.match_path(Utf8Path::new(path), |_| {}) == PathMatch::NoMatches
            })
            .collect();
        assert!(
            unruled.is_empty(),
            "these files run every job because no rule covers them: {unruled:?}"
        );
    }
}
