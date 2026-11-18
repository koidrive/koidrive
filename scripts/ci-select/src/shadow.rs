// SPDX-License-Identifier: Apache-2.0

use crate::{Analysis, Config, Jobs, PathOutcome};
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
    pub files: Vec<MapFile>,
    pub tests: BTreeMap<String, Vec<usize>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MapFile {
    pub path: String,
    pub blob: String,
    pub functions: Vec<[u32; 2]>,
}

pub struct FileChange {
    pub base_blob: Option<String>,
    pub deleted: bool,
    pub hunks: Vec<[u32; 2]>,
}

fn git(args: &[&str]) -> Result<String, String> {
    let output = std::process::Command::new("git")
        .args(args)
        .output()
        .map_err(|error| format!("git {args:?}: {error}"))?;
    if !matches!(output.status.code(), Some(0 | 1)) {
        return Err(format!(
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    String::from_utf8(output.stdout).map_err(|error| format!("git {args:?}: {error}"))
}

fn hunks(diff: &str) -> Vec<[u32; 2]> {
    diff.lines()
        .filter_map(|line| line.strip_prefix("@@ -")?.split(' ').next())
        .filter_map(|old| {
            let (start, count) = old.split_once(',').unwrap_or((old, "1"));
            let (start, count): (u32, u32) = (start.parse().ok()?, count.parse().ok()?);
            Some(match count {
                0 => [start, start + 1],
                _ => [start, start + count - 1],
            })
        })
        .collect()
}

pub fn changes(
    base: &Utf8Path,
    head: &Utf8Path,
    paths: &[(String, PathOutcome)],
) -> Result<BTreeMap<String, FileChange>, String> {
    let mut found = BTreeMap::new();
    for (path, outcome) in paths {
        if *outcome != PathOutcome::Package || !path.ends_with(".rs") {
            continue;
        }
        let (old, new) = (base.join(path), head.join(path));
        let base_blob = match old.exists() {
            true => Some(
                git(&["hash-object", "--no-filters", old.as_str()])?
                    .trim()
                    .to_string(),
            ),
            false => None,
        };
        let hunks = match old.exists() && new.exists() {
            true => hunks(&git(&[
                "diff",
                "--no-index",
                "--no-color",
                "-U0",
                "--",
                old.as_str(),
                new.as_str(),
            ])?),
            false => Vec::new(),
        };
        found.insert(
            path.clone(),
            FileChange {
                base_blob,
                deleted: !new.exists(),
                hunks,
            },
        );
    }
    Ok(found)
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
    analysis: &Analysis,
    changes: &BTreeMap<String, FileChange>,
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
    let age = now.saturating_sub(map.created);
    if age > tests.max_map_age_days * 86_400 {
        return everything(format!(
            "the coverage map of {} is {:.1} days old",
            map.sha,
            age as f64 / 86_400.0
        ));
    }
    let readers: BTreeSet<&str> = analysis
        .reads
        .iter()
        .map(|read| read.package.as_str())
        .collect();
    let known: BTreeMap<&str, usize> = map
        .files
        .iter()
        .enumerate()
        .map(|(index, file)| (file.path.as_str(), index))
        .collect();
    let mut changed = BTreeSet::new();
    for (path, outcome) in &analysis.paths {
        if path == "Cargo.lock" {
            return everything(
                "`Cargo.lock` changes, and the coverage map cannot see dependency code".to_string(),
            );
        }
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
            PathOutcome::Package => {
                let Some(index) = known.get(path.as_str()).filter(|_| path.ends_with(".rs")) else {
                    return everything(format!(
                        "the coverage map of {} has no record of `{path}`",
                        map.sha
                    ));
                };
                let file = &map.files[*index];
                let Some(change) = changes.get(path) else {
                    return everything(format!("the change to `{path}` was not read"));
                };
                if change.base_blob.as_deref() != Some(file.blob.as_str()) {
                    return everything(format!(
                        "`{path}` changed on main after the coverage map of {}",
                        map.sha
                    ));
                }
                if change.deleted {
                    return everything(format!("`{path}` is deleted"));
                }
                let inside = |hunk: &[u32; 2]| {
                    file.functions
                        .iter()
                        .any(|function| function[0] <= hunk[0] && hunk[1] <= function[1])
                };
                if let Some(hunk) = change.hunks.iter().find(|hunk| !inside(hunk)) {
                    return everything(format!(
                        "`{path}` changes lines {}-{} outside the function bodies the coverage map records, where code runs without coverage (constants, macros, derives, types)",
                        hunk[0], hunk[1]
                    ));
                }
                changed.insert(*index);
            }
            PathOutcome::Ignored => {}
        }
    }
    TestSelection {
        map: Some(map.sha.clone()),
        everything: None,
        changed: changed
            .iter()
            .map(|index| map.files[*index].path.clone())
            .collect(),
        skipped: map
            .tests
            .iter()
            .filter(|(test, files)| {
                let package = test.split([' ', ':']).next().unwrap_or(test);
                !files.is_empty()
                    && !files.iter().any(|file| changed.contains(file))
                    && !random.holds(test)
                    && !readers.contains(package)
            })
            .map(|(test, _)| test.clone())
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_reads_the_base_blob_and_the_base_lines_of_each_hunk() {
        let root = Utf8PathBuf::from_path_buf(
            std::env::temp_dir().join(format!("ci-select-changes-{}", std::process::id())),
        )
        .unwrap();
        let (base, head) = (root.join("base"), root.join("head"));
        for dir in [&base, &head] {
            fs::create_dir_all(dir.join("src")).unwrap();
        }
        fs::write(base.join("src/a.rs"), "a\n").unwrap();
        fs::write(head.join("src/a.rs"), "b\n").unwrap();
        fs::write(base.join("src/b.rs"), "1\n2\n3\n4\n5\n").unwrap();
        fs::write(head.join("src/b.rs"), "1\n2\nnew\n3\n4\n").unwrap();
        fs::write(base.join("src/gone.rs"), "x\n").unwrap();
        let paths: Vec<_> = ["src/a.rs", "src/b.rs", "src/gone.rs"]
            .iter()
            .map(|path| (path.to_string(), PathOutcome::Package))
            .collect();
        let found = changes(&base, &head, &paths).unwrap();
        fs::remove_dir_all(&root).unwrap();
        assert_eq!(
            found["src/a.rs"].base_blob.as_deref(),
            Some("78981922613b2afb6025042ff6bd878ac1994e85")
        );
        assert_eq!(found["src/a.rs"].hunks, [[1, 1]]);
        assert_eq!(found["src/b.rs"].hunks, [[2, 3], [5, 5]]);
        assert!(found["src/gone.rs"].deleted && !found["src/a.rs"].deleted);
    }
}
