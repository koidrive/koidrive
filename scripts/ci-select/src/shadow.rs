// SPDX-License-Identifier: Apache-2.0

use crate::{Config, Jobs, PathOutcome, ReadMatch};
use camino::{Utf8Path, Utf8PathBuf};
use guppy::graph::PackageGraph;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tests {
    pub jobs: Vec<String>,
    pub max_map_age_days: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoverageMap {
    pub sha: String,
    pub created: u64,
    pub files: Vec<String>,
    pub tests: BTreeMap<String, Vec<usize>>,
}

#[derive(Serialize, Debug, PartialEq)]
pub struct TestSelection {
    pub map: Option<String>,
    pub everything: Option<String>,
    pub changed: Vec<String>,
    pub skipped: Vec<String>,
}

#[derive(Default)]
pub struct Random {
    binaries: BTreeSet<String>,
    functions: BTreeSet<(String, String)>,
}

impl Random {
    fn holds(&self, test: &str) -> bool {
        let (binary, name) = test.split_once(' ').unwrap_or((test, ""));
        let package = binary.split("::").next().unwrap_or(binary);
        let function = name.rsplit("::").next().unwrap_or(name);
        self.binaries.contains(binary)
            || self
                .functions
                .contains(&(package.to_string(), function.to_string()))
    }
}

fn rust_files(dir: &Utf8Path, found: &mut Vec<Utf8PathBuf>) -> Result<(), String> {
    let entries = dir
        .read_dir_utf8()
        .map_err(|error| format!("{dir}: {error}"))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("{dir}: {error}"))?;
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|error| format!("{path}: {error}"))?;
        if kind.is_dir() && !matches!(entry.file_name(), "target" | "node_modules") {
            rust_files(path, found)?;
        } else if kind.is_file() && path.extension() == Some("rs") {
            found.push(path.to_path_buf());
        }
    }
    Ok(())
}

pub fn random(root: &Utf8Path, graph: &PackageGraph) -> Result<Random, String> {
    let mut random = Random::default();
    for package in graph.workspace().iter() {
        let Some(dir) = package.source().workspace_path() else {
            continue;
        };
        let dir = root.join(dir);
        let mut files = Vec::new();
        rust_files(&dir, &mut files)?;
        for file in files {
            let text = fs::read_to_string(&file).map_err(|error| format!("{file}: {error}"))?;
            if !text.contains("proptest!") && !text.contains("TestRunner") {
                continue;
            }
            let relative = file.strip_prefix(&dir).unwrap_or(&file);
            let mut parts = relative.components().map(|part| part.as_str());
            if parts.next() == Some("tests")
                && let Some(target) = parts.next()
            {
                let target = target.strip_suffix(".rs").unwrap_or(target);
                random
                    .binaries
                    .insert(format!("{}::{target}", package.name()));
                continue;
            }
            for rest in text.split("fn ").skip(1) {
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if !name.is_empty() {
                    random.functions.insert((package.name().to_string(), name));
                }
            }
        }
    }
    Ok(random)
}

pub fn select(
    config: &Config,
    tests: &Tests,
    paths: &[(String, PathOutcome)],
    reads: &[ReadMatch],
    random: &Random,
    map: Option<&CoverageMap>,
    now: u64,
) -> TestSelection {
    let everything = |reason: String| TestSelection {
        map: map.map(|map| map.sha.clone()),
        everything: Some(reason),
        changed: Vec::new(),
        skipped: Vec::new(),
    };
    let Some(map) = map else {
        return everything("there is no coverage map".to_string());
    };
    let age = now.saturating_sub(map.created) / 86_400;
    if age > tests.max_map_age_days {
        return everything(format!("the coverage map of {} is {age} days old", map.sha));
    }
    let readers: BTreeSet<&str> = reads.iter().map(|read| read.package.as_str()).collect();
    let known: BTreeMap<&str, usize> = map
        .files
        .iter()
        .enumerate()
        .map(|(index, file)| (file.as_str(), index))
        .collect();
    let mut changed = BTreeSet::new();
    for (path, outcome) in paths {
        match outcome {
            PathOutcome::Unknown | PathOutcome::EveryJob => {
                return everything(format!("`{path}` runs every job"));
            }
            PathOutcome::Entry(index) => {
                let entry = &config.path[*index];
                let tested = match &entry.jobs {
                    Jobs::Named(jobs) => jobs.iter().any(|job| tests.jobs.contains(job)),
                    _ => false,
                };
                if tested || !entry.packages.is_empty() {
                    return everything(format!("`{path}` matches the path rule {:?}", entry.globs));
                }
            }
            PathOutcome::Package => match known.get(path.as_str()) {
                Some(index) if path.ends_with(".rs") => {
                    changed.insert(*index);
                }
                _ => {
                    return everything(format!(
                        "the coverage map of {} has no record of `{path}`",
                        map.sha
                    ));
                }
            },
            PathOutcome::Ignored => {}
        }
    }
    TestSelection {
        map: Some(map.sha.clone()),
        everything: None,
        changed: changed
            .iter()
            .map(|index| map.files[*index].clone())
            .collect(),
        skipped: map
            .tests
            .iter()
            .filter(|(test, files)| {
                let package = test.split([' ', ':']).next().unwrap_or(test);
                !files.iter().any(|file| changed.contains(file))
                    && !random.holds(test)
                    && !readers.contains(package)
            })
            .map(|(test, _)| test.clone())
            .collect(),
    }
}
