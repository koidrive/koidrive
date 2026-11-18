// SPDX-License-Identifier: Apache-2.0

use camino::Utf8Path;
use determinator::Determinator;
use determinator::rules::{
    DeterminatorMarkChanged, DeterminatorPostRule, DeterminatorRules, PathMatch, PathRule,
    RuleIndex,
};
use globset::{Glob, GlobSet, GlobSetBuilder};
use guppy::MetadataCommand;
use guppy::graph::cargo::CargoOptions;
use guppy::graph::feature::StandardFeatures;
use guppy::graph::{DependencyDirection, PackageGraph};
use guppy::platform::PlatformSpec;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::process::ExitCode;
use std::{env, fs};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    path: Vec<PathEntry>,
    #[serde(default)]
    read: Vec<Read>,
    job: BTreeMap<String, Job>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Read {
    globs: Vec<String>,
    by: Vec<String>,
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
    EveryJob,
    Unknown,
    Entry(usize),
    Package,
    Ignored,
}

fn rules(config: &Config) -> DeterminatorRules {
    let mut rules = DeterminatorRules::parse("use-default-rules = false")
        .expect("rules without defaults parse");
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
        PathMatch::RuleMatchedAll => PathOutcome::EveryJob,
        PathMatch::NoMatches => PathOutcome::Unknown,
        PathMatch::RuleMatched(RuleIndex::CustomPath(index)) => PathOutcome::Entry(index),
        PathMatch::RuleMatched(_) => PathOutcome::Ignored,
        PathMatch::AncestorMatched => PathOutcome::Package,
    }
}

fn tests(job: &Job, package: &str) -> bool {
    match &job.packages {
        Packages::Any(_) => !job.except.iter().any(|name| name == package),
        Packages::Named(packages) => packages.iter().any(|name| name == package),
        Packages::None => false,
    }
}

fn glob_set(globs: &[String]) -> Result<GlobSet, String> {
    let mut set = GlobSetBuilder::new();
    for glob in globs {
        set.add(Glob::new(glob).map_err(|error| format!("ci-select config: {error}"))?);
    }
    set.build()
        .map_err(|error| format!("ci-select config: {error}"))
}

struct ReadMatch {
    path: String,
    package: String,
    rule: usize,
}

fn reads(config: &Config, changed: &[&str]) -> Result<Vec<ReadMatch>, String> {
    let mut found = Vec::new();
    for (rule, read) in config.read.iter().enumerate() {
        let globs = glob_set(&read.globs)?;
        for path in changed.iter().filter(|path| globs.is_match(path)) {
            found.extend(read.by.iter().map(|package| ReadMatch {
                path: path.to_string(),
                package: package.clone(),
                rule,
            }));
        }
    }
    Ok(found)
}

struct Owner {
    path: String,
    packages: Vec<String>,
    rule: Option<usize>,
    dependents: BTreeSet<String>,
}

fn dependents<'g>(
    graph: &'g PackageGraph,
    packages: impl IntoIterator<Item = guppy::graph::PackageMetadata<'g>>,
) -> Option<BTreeSet<String>> {
    let ids: Vec<_> = packages.into_iter().map(|package| package.id()).collect();
    Some(
        graph
            .query_reverse(ids)
            .ok()?
            .resolve()
            .packages(DependencyDirection::Forward)
            .map(|dependent| dependent.name().to_string())
            .collect(),
    )
}

fn owners(
    config: &Config,
    base: &PackageGraph,
    head: &PackageGraph,
    paths: &[(String, PathOutcome)],
) -> Vec<Owner> {
    paths
        .iter()
        .filter_map(|(path, outcome)| match outcome {
            PathOutcome::Package => [head, base].into_iter().find_map(|graph| {
                let workspace = graph.workspace();
                let package = Utf8Path::new(path)
                    .ancestors()
                    .skip(1)
                    .find_map(|dir| workspace.member_by_path(dir).ok())?;
                Some(Owner {
                    path: path.clone(),
                    packages: vec![package.name().to_string()],
                    rule: None,
                    dependents: dependents(graph, [package])?,
                })
            }),
            PathOutcome::Entry(index) if !config.path[*index].packages.is_empty() => {
                let packages = &config.path[*index].packages;
                let workspace = head.workspace();
                Some(Owner {
                    path: path.clone(),
                    packages: packages.clone(),
                    rule: Some(*index),
                    dependents: dependents(
                        head,
                        packages
                            .iter()
                            .filter_map(|name| workspace.member_by_name(name.as_str()).ok()),
                    )?,
                })
            }
            _ => None,
        })
        .collect()
}

fn select(
    config: &Config,
    affected: &BTreeSet<String>,
    paths: &[(String, PathOutcome)],
    reads: &[ReadMatch],
    owners: &[Owner],
) -> Result<BTreeMap<String, Option<String>>, String> {
    let mut every_job = None;
    for (path, outcome) in paths {
        match outcome {
            PathOutcome::Unknown => {
                every_job = Some(format!(
                    "no rule in ci-select.toml and no package covers `{path}`, so every job runs"
                ));
                break;
            }
            PathOutcome::EveryJob => {
                let mut globs = None;
                for entry in config
                    .path
                    .iter()
                    .filter(|entry| matches!(entry.jobs, Jobs::All(_)))
                {
                    if glob_set(&entry.globs)?.is_match(path) {
                        globs = Some(&entry.globs);
                        break;
                    }
                }
                let globs = globs.ok_or_else(|| {
                    format!(
                        "{path} runs every job, but no path rule with jobs = \"all\" matches it"
                    )
                })?;
                every_job = Some(format!(
                    "`{path}` matches the path rule {globs:?}, which runs every job"
                ));
                break;
            }
            _ => {}
        }
    }
    Ok(config
        .job
        .iter()
        .map(|(name, job)| {
            let by_path = || {
                paths.iter().find_map(|(path, outcome)| match outcome {
                    PathOutcome::Entry(index) => match &config.path[*index].jobs {
                        Jobs::Named(jobs) if jobs.contains(name) => Some(format!(
                            "`{path}` matches the path rule {:?}",
                            config.path[*index].globs
                        )),
                        _ => None,
                    },
                    _ => None,
                })
            };
            let by_read = || {
                reads
                    .iter()
                    .find(|read| tests(job, &read.package))
                    .map(|read| {
                        format!(
                            "the tests of {} read `{}` (read rule {:?})",
                            read.package, read.path, config.read[read.rule].globs
                        )
                    })
            };
            let by_owner = || {
                owners.iter().find_map(|owner| {
                    let tested = owner
                        .packages
                        .iter()
                        .chain(&owner.dependents)
                        .find(|package| affected.contains(*package) && tests(job, package))?;
                    let source = match owner.rule {
                        Some(index) => format!(
                            "`{}` matches the path rule {:?}, which marks {:?} changed",
                            owner.path, config.path[index].globs, owner.packages
                        ),
                        None => format!("`{}` is in {}", owner.path, owner.packages[0]),
                    };
                    Some(if owner.packages.contains(tested) {
                        format!("{source}, which this job tests")
                    } else {
                        format!(
                            "{source}, and {tested}, which this job tests, depends on {}",
                            if owner.packages.len() == 1 {
                                "it"
                            } else {
                                "them"
                            }
                        )
                    })
                })
            };
            let by_package = || {
                affected
                    .iter()
                    .find(|package| tests(job, package))
                    .map(|package| {
                        format!("the dependencies of {package}, which this job tests, changed")
                    })
            };
            (
                name.clone(),
                every_job
                    .clone()
                    .or_else(by_path)
                    .or_else(by_owner)
                    .or_else(by_read)
                    .or_else(by_package),
            )
        })
        .collect())
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
    for read in &config.read {
        if let Some(package) = read
            .by
            .iter()
            .find(|package| !workspace.contains_name(package.as_str()))
        {
            return Err(format!(
                "read rule {:?} names {package}, which is not a workspace package",
                read.globs
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
    let in_a_checkout = |path: &str| {
        path.split('/').all(|part| !matches!(part, "" | "." | ".."))
            && [base_dir, head_dir].iter().any(|dir| {
                Utf8Path::new(dir)
                    .join(path)
                    .symlink_metadata()
                    .is_ok_and(|metadata| !metadata.is_dir())
            })
    };
    if let Some(path) = changed.iter().find(|path| !in_a_checkout(path)) {
        return Err(format!(
            "{changed_path} lists {path:?}, which is no file in either checkout. Pass the NUL-separated output of git diff -z --name-only --no-renames BASE HEAD."
        ));
    }
    compute(&config, &graph(base_dir)?, &graph(head_dir)?, &changed)
}

fn compute(
    config: &Config,
    base: &PackageGraph,
    head: &PackageGraph,
    changed: &[&str],
) -> Result<BTreeMap<String, Option<String>>, String> {
    check(config, head)?;
    let readers = reads(config, changed)?;
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
    let owners = owners(config, base, head, &paths);
    select(config, &affected, &paths, &readers, &owners)
}

const SKIPPED: &str = "the change reaches none of its packages or paths";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match run(&args) {
        Ok(selection) => {
            let mut summary =
                String::from("### Jobs ci-select chose\n\n| Job | Runs | Why |\n|---|---|---|\n");
            for (job, reason) in &selection {
                println!("{job}={}", reason.is_some());
                let why = reason.as_deref().unwrap_or(SKIPPED);
                eprintln!(
                    "{job}: {}, because {why}",
                    if reason.is_some() { "runs" } else { "skipped" }
                );
                summary.push_str(&format!(
                    "| {job} | {} | {} |\n",
                    if reason.is_some() { "yes" } else { "no" },
                    why.replace('|', "\\|")
                ));
            }
            if let Ok(path) = env::var("GITHUB_STEP_SUMMARY")
                && let Err(error) = fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(&path)
                    .and_then(|mut file| file.write_all(summary.as_bytes()))
            {
                eprintln!("::warning::Writing the job summary to {path} failed: {error}");
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
            running(&["docs/deploy.md", "README.md", "LICENSE"]),
            jobs(&[])
        );
    }

    #[test]
    fn battery_triage_and_known_failure_changes_run_no_selected_job() {
        assert_eq!(
            running(&[
                ".github/battery-known-failures/GUN-1-A-write-lost.toml",
                "scripts/triage-battery.py",
                "scripts/test-property-battery.py",
            ]),
            jobs(&[])
        );
    }

    #[test]
    fn a_readme_or_license_inside_a_package_selects_that_package() {
        for path in [
            "core/engine/README.md",
            "core/engine/tests/README_selector_probe.rs",
            "core/engine/LICENSE",
        ] {
            let running = running(&[path]);
            for job in ["rust_engine", "rust_driver", "rust_sim"] {
                assert!(running.contains(job), "{path} must run {job}: {running:?}");
            }
        }
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
            jobs(&["driver_integration", "rust_driver", "rust_lint", "rust_sim"])
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
    fn a_cli_change_runs_every_job_that_builds_the_cli() {
        assert_eq!(
            running(&["cli/src/main.rs"]),
            jobs(&[
                "rust",
                "rust_lint",
                "rust_driver",
                "cli_integration",
                "ffi_integration",
                "macos_app"
            ])
        );
    }

    #[test]
    fn a_change_that_introduced_a_past_regression_runs_the_job_whose_test_caught_it() {
        for (fixed_by, path, job) in [
            (
                "#682",
                ".github/workflows/property-battery.yml",
                "rust_engine",
            ),
            ("#664", "server/src/account/claim.ts", "net_integration"),
            ("#664", "server/src/account/claim.ts", "server"),
            ("#400", "spec/rfc/0018-resends-and-deletes.md", "rust_sim"),
            ("#367", "docs/json-schema/v1/koi-status.json", "rust_driver"),
            ("#367", "docs/json-schema/v1/koi-status.json", "rust"),
            ("#347", "scripts/test-split/mapping.json", "rust_engine"),
            ("#674", "server/scripts/deploy-test/e2e.ts", "server"),
        ] {
            let running = running(&[path]);
            assert!(
                running.contains(job),
                "{path} introduced the bug {fixed_by} fixed, which {job} caught, but it runs only {running:?}"
            );
        }
    }

    #[test]
    fn a_deleted_or_moved_file_selects_what_it_was_part_of() {
        let deleted = running(&["core/engine/src/deleted_module.rs"]);
        for job in ["rust_engine", "rust_driver", "rust_sim"] {
            assert!(deleted.contains(job), "{job} must run: {deleted:?}");
        }
        assert_eq!(
            running(&[
                "core/removed-package/src/lib.rs",
                "core/engine/src/moved.rs"
            ]),
            every_job()
        );
    }

    fn select_from_list(
        name: &str,
        list: &str,
    ) -> Result<BTreeMap<String, Option<String>>, String> {
        let file = env::temp_dir().join(format!("ci-select-{}-{name}", std::process::id()));
        fs::write(&file, list).unwrap();
        let selection = run(&[
            format!("{ROOT}/.github/ci-select.toml"),
            ROOT.to_string(),
            ROOT.to_string(),
            file.to_str().unwrap().to_string(),
        ]);
        fs::remove_file(&file).unwrap();
        selection
    }

    #[test]
    fn a_change_list_that_is_not_nul_separated_paths_fails_instead_of_skipping_jobs() {
        let modified_by_595 = [
            "core/driver/src/disk/tests.rs",
            "core/driver/src/engine.rs",
            "core/driver/src/observe.rs",
            "core/fs/src/lib.rs",
            "core/fs/src/mark.rs",
            "core/sim/src/world/device.rs",
            "core/sim/tests/devices.rs",
        ];
        let selection = select_from_list("nul", &(modified_by_595.join("\0") + "\0")).unwrap();
        assert!(selection["rust_sim"].is_some(), "{selection:?}");
        for (name, list) in [
            ("newline", modified_by_595.join("\n") + "\n"),
            ("absolute", format!("{ROOT}/core/sim/tests/devices.rs\0")),
            (
                "parent",
                "core/driver/../sim/tests/devices.rs\0".to_string(),
            ),
            ("missing", "core/sim/tests/no-such-file.rs\0".to_string()),
            ("dot", "core/sim/./tests/devices.rs\0".to_string()),
            ("double slash", "core/sim//tests/devices.rs\0".to_string()),
            ("directory", "core/sim/tests\0".to_string()),
        ] {
            let error = select_from_list(name, &list).unwrap_err();
            assert!(error.contains("git diff -z"), "{name}: {error}");
        }
    }

    #[test]
    fn a_change_under_each_package_runs_every_job_that_builds_it() {
        let workspace = GRAPH.workspace();
        let builds = |tested: &guppy::graph::PackageMetadata,
                      changed: &guppy::graph::PackageMetadata| {
            GRAPH
                .query_forward([tested.id()])
                .unwrap()
                .resolve_with_fn(|_, link| link.from().id() == tested.id() || !link.dev_only())
                .contains(changed.id())
                .unwrap()
        };
        for changed in workspace.iter() {
            let dir = changed.source().workspace_path().unwrap();
            let expected: BTreeSet<String> = CONFIG
                .job
                .iter()
                .filter(|(_, job)| {
                    workspace
                        .iter()
                        .any(|tested| tests(job, tested.name()) && builds(&tested, &changed))
                })
                .map(|(name, _)| name.clone())
                .collect();
            assert!(!expected.is_empty(), "no job builds {}", changed.name());
            for file in [
                "src/lib.rs",
                "tests/new.rs",
                "README.md",
                "LICENSE",
                "Cargo.toml",
                "nested dir/new file.txt",
            ] {
                let path = dir.join(file);
                let running = running(&[path.as_str()]);
                let missing: Vec<_> = expected.difference(&running).collect();
                assert!(
                    missing.is_empty(),
                    "{path} must run {missing:?}, ran {running:?}"
                );
            }
        }
    }

    #[test]
    fn a_read_rule_alone_runs_the_jobs_that_test_the_reading_package() {
        let config: Config = toml::from_str(
            r#"
            [[path]]
            globs = ["notes/**"]

            [[read]]
            globs = ["notes/read-by-fs.txt"]
            by = ["koidrive-fs"]

            [job.tests_fs]
            packages = ["koidrive-fs"]

            [job.tests_cli]
            packages = ["koidrive-cli"]
            "#,
        )
        .unwrap();
        let select = |path: &str| compute(&config, &GRAPH, &GRAPH, &[path]).unwrap();
        assert_eq!(
            select("notes/read-by-fs.txt"),
            BTreeMap::from([
                ("tests_cli".to_string(), None),
                (
                    "tests_fs".to_string(),
                    Some(
                        "the tests of koidrive-fs read `notes/read-by-fs.txt` (read rule [\"notes/read-by-fs.txt\"])"
                            .to_string()
                    )
                ),
            ])
        );
        assert_eq!(
            select("notes/read-by-nobody.txt"),
            BTreeMap::from([
                ("tests_cli".to_string(), None),
                ("tests_fs".to_string(), None)
            ])
        );
    }

    #[test]
    fn a_change_to_a_crate_the_quoted_paths_test_reads_runs_the_jobs_that_test_koidrive_fs() {
        let quoted = fs::read_to_string(format!("{ROOT}/core/fs/tests/quoted.rs")).unwrap();
        let crates: Vec<_> = quoted
            .split_once("const CRATES_THE_DAEMON_AND_THE_APP_RUN")
            .and_then(|(_, rest)| rest.split_once("= ["))
            .and_then(|(_, rest)| rest.split_once("];"))
            .unwrap()
            .0
            .split('"')
            .skip(1)
            .step_by(2)
            .collect();
        assert!(crates.len() >= 10, "{crates:?}");
        let testing_fs: BTreeSet<String> = CONFIG
            .job
            .iter()
            .filter(|(_, job)| tests(job, "koidrive-fs"))
            .map(|(name, _)| name.clone())
            .collect();
        for name in crates {
            let path = format!("core/{name}/src/new_module/new.rs");
            assert!(
                reads(&CONFIG, &[path.as_str()])
                    .unwrap()
                    .iter()
                    .any(|read| read.path == path && read.package == "koidrive-fs"),
                "core/fs/tests/quoted.rs reads {path}, but no read rule in ci-select.toml says so"
            );
            let running = running(&[path.as_str()]);
            let missing: Vec<_> = testing_fs.difference(&running).collect();
            assert!(
                missing.is_empty(),
                "{path} must run {missing:?}, ran {running:?}"
            );
        }
    }

    fn built(text: &str) -> BTreeSet<String> {
        let words: Vec<_> = text.split_whitespace().collect();
        words
            .windows(2)
            .filter(|pair| matches!(pair[0], "-p" | "--package"))
            .map(|pair| pair[1].trim_matches('"').to_string())
            .filter(|name| !name.contains('$'))
            .collect()
    }

    #[test]
    fn every_package_a_job_builds_selects_that_job() {
        let workflow = fs::read_to_string(format!("{ROOT}/.github/workflows/ci.yml")).unwrap();
        let steps = |job: &str| -> Vec<String> {
            let body: String = workflow
                .split(&format!("\n  {job}:\n"))
                .nth(1)
                .unwrap()
                .lines()
                .take_while(|line| !line.starts_with("  ") || line.starts_with("   "))
                .map(|line| format!("{line}\n"))
                .collect();
            body.split("\n      - ")
                .skip(1)
                .map(str::to_string)
                .collect()
        };
        let suite = |name: &str, job: &str| {
            steps(job)
                .iter()
                .filter(|step| {
                    let condition = step
                        .lines()
                        .find(|line| line.trim_start().starts_with("if:"))
                        .unwrap_or("");
                    !condition.contains("needs.changes.outputs.rust")
                        || condition
                            .contains(&format!("needs.changes.outputs.rust_{name} == 'true'"))
                })
                .map(|step| built(step))
                .fold(BTreeSet::from([format!("koidrive-{name}")]), |all, more| {
                    all.union(&more).cloned().collect()
                })
        };
        let script = |path: &str| built(&fs::read_to_string(format!("{ROOT}/{path}")).unwrap());
        let workspace = GRAPH.workspace();
        let id = |name: &str| workspace.member_by_name(name).unwrap().id().clone();
        for (job, packages) in [
            ("rust_engine", suite("engine", "rust-special")),
            ("rust_driver", suite("driver", "rust-special")),
            ("rust_sim", suite("sim", "rust-sim")),
            (
                "net_integration",
                script("core/net/scripts/integration-test.sh"),
            ),
            (
                "driver_integration",
                script("core/driver/scripts/integration-test.sh"),
            ),
            ("cli_integration", script("cli/scripts/integration-test.sh")),
            (
                "ffi_integration",
                script("core/ffi/scripts/integration-test.sh"),
            ),
            ("macos_app", script("apps/macos/scripts/build-ffi.sh")),
        ] {
            assert!(!packages.is_empty(), "{job} builds no package");
            let Packages::Named(listed) = &CONFIG.job[job].packages else {
                continue;
            };
            for package in &packages {
                let selects = listed.iter().any(|name| {
                    GRAPH
                        .query_forward([&id(name)])
                        .unwrap()
                        .resolve()
                        .contains(&id(package))
                        .unwrap()
                });
                assert!(
                    selects,
                    "{job} builds {package}, but a change to {package} does not select {job}"
                );
            }
        }
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
