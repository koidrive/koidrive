// SPDX-License-Identifier: Apache-2.0

use std::fmt::Write;
use std::path::{Path, PathBuf};

#[path = "build/plan.rs"]
mod plan;

fn files(dir: &Path, extension: &str, subdirs: &[&str]) -> Vec<(String, PathBuf)> {
    println!("cargo::rerun-if-changed={}", dir.display());
    let mut files: Vec<(String, PathBuf)> = std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("reading {}: {error}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|path| !path.file_name().unwrap().to_string_lossy().starts_with('.'))
        .filter(|path| {
            let name = path.file_name().unwrap().to_string_lossy();
            if path.is_dir() && !subdirs.contains(&name.as_ref()) {
                panic!(
                    "{} is a directory; put {extension} files directly in {}",
                    path.display(),
                    dir.display()
                );
            }
            path.is_file()
        })
        .map(|path| {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_suffix(extension))
                .unwrap_or_else(|| panic!("{} is not a {extension} file", path.display()))
                .to_owned();
            (name, path)
        })
        .collect();
    files.sort();
    files
}

fn main() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = manifest.join("migrations");
    let legacy = files(&root.join("legacy"), ".sql", &[]);
    let named = files(&root, ".sql", &["legacy"]);
    let sql: Vec<String> = named
        .iter()
        .map(|(_, path)| std::fs::read_to_string(path).unwrap())
        .collect();
    let planned = plan::plan(
        &legacy
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        &named
            .iter()
            .zip(&sql)
            .map(|((name, _), sql)| (name.as_str(), sql.as_str()))
            .collect::<Vec<_>>(),
    )
    .unwrap_or_else(|error| panic!("core/store/migrations: {error}"));
    let mut out = String::from("pub const MIGRATIONS: &[Migration] = &[\n");
    for planned in &planned {
        let path = legacy
            .iter()
            .chain(&named)
            .find(|(name, _)| *name == planned.name)
            .map(|(_, path)| path)
            .unwrap();
        writeln!(
            out,
            "    Migration {{ name: {:?}, requires: &{:?}, sql: include_str!({:?}) }},",
            planned.name,
            planned.requires,
            path.to_str().unwrap()
        )
        .unwrap();
    }
    writeln!(out, "];\npub const LEGACY_STEPS: usize = {};", legacy.len()).unwrap();
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    std::fs::write(out_dir.join("migrations.rs"), out).unwrap();

    let mut cases = String::new();
    for (_, path) in files(&manifest.join("tests/upgrade_cases"), ".rs", &[]) {
        writeln!(cases, "include!({:?});", path.to_str().unwrap()).unwrap();
    }
    std::fs::write(out_dir.join("upgrade_cases.rs"), cases).unwrap();

    let mut tests = String::new();
    for (name, path) in files(&manifest.join("tests/migrations"), ".rs", &[]) {
        if !named.iter().any(|(migration, _)| *migration == name) {
            panic!(
                "core/store/tests/migrations/{name}.rs holds the store tests of a migration, but core/store/migrations/{name}.sql does not exist"
            );
        }
        writeln!(tests, "#[path = {:?}]\nmod {name};", path.to_str().unwrap()).unwrap();
    }
    std::fs::write(out_dir.join("migration_tests.rs"), tests).unwrap();
}
