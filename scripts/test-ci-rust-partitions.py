#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
from pathlib import Path
import argparse
import json
import re
import shlex
import subprocess
import unittest


WORKFLOW = Path(__file__).resolve().parent.parent / ".github/workflows/ci.yml"
SELECTOR = WORKFLOW.parent.parent / "ci-select.toml"
TARGETS = ("aarch64-apple-darwin", "x86_64-unknown-linux-gnu")


def job(workflow, name):
    body = workflow.split(f"\n  {name}:\n", 1)[1]
    return re.split(r"\n  \S", body, maxsplit=1)[0]


def test_arguments(body, marker):
    commands = [shlex.split(line) for line in re.findall(r"cargo test[^\n]+", body) if marker in line]
    if len(commands) != 2:
        raise ValueError("Each partition must have one listing and one execution command")
    listing, execution = commands
    if "--" not in listing or listing[listing.index("--") + 1:listing.index("--") + 2] != ["--list"]:
        raise ValueError("The inventory must list the executed tests")
    if listing[:listing.index("--")] != execution:
        raise ValueError("Inventory and execution must use identical Cargo arguments")
    return execution


def suite_features(expression, suite):
    body = re.fullmatch(r"\$\{\{\s*(.*?)\s*\}\}", expression)
    if body is None:
        raise ValueError("Expected a Rust suite feature expression")
    for clause in body[1].split("||"):
        conditional = re.fullmatch(r"matrix\.suite == '([^']+)' && '([^']*)'", clause.strip())
        if conditional:
            if conditional[1] == suite:
                return conditional[2]
        else:
            fallback = re.fullmatch(r"'([^']*)'", clause.strip())
            if fallback:
                return fallback[1]
            raise ValueError("Unsupported Rust suite feature expression")
    raise ValueError("Rust suite features need a fallback")


def check_partitions(workflow, selector=None):
    selector = SELECTOR.read_text() if selector is None else selector
    remainder = test_arguments(job(workflow, "rust"), "--workspace")
    parser = argparse.ArgumentParser(add_help=False, exit_on_error=False)
    parser.add_argument("--workspace", action="store_true")
    parser.add_argument("--exclude", action="append", default=[])
    parser.add_argument("-p", "--package", action="append", default=[])
    parser.add_argument("--features", action="append", default=[])
    parser.add_argument("--locked", action="store_true")
    remaining, unknown = parser.parse_known_args(remainder[2:])
    if unknown:
        raise ValueError("The remainder must not filter default test targets")
    if not remaining.workspace or remaining.package:
        raise ValueError("The remainder must select the workspace")
    excluded = remaining.exclude
    shards = job(workflow, "rust-tests")
    if re.search(r"(?m)^        suite: \$\{\{ fromJSON\(needs\.changes\.outputs\.rust_suites\) \}\}$", shards) is None:
        raise ValueError("The Rust suite matrix must come from the changes job's rust_suites")
    every = re.search(r"""(?m)^ +echo 'rust_suites=(\[[^']*\])'$""", job(workflow, "changes"))
    if every is None:
        raise ValueError("Expected the changes job to list every Rust suite for a push")
    suites = json.loads(every[1])
    missing = [suite for suite in suites if f"\n[job.rust_{suite}]\n" not in selector]
    if missing:
        raise ValueError(f"ci-select.toml has no job for the Rust suites {missing}")
    packages = ["koidrive-" + suite for suite in suites]
    if len(excluded) != len(set(excluded)) or len(packages) != len(set(packages)):
        raise ValueError("Rust partitions must not contain duplicate packages")
    if set(excluded) != set(packages):
        raise ValueError("Every excluded package must run in exactly one Rust shard")
    command = test_arguments(shards, "koidrive-$SUITE")
    selected, unknown = parser.parse_known_args(command[2:])
    if unknown:
        raise ValueError("Rust shards must not filter default test targets")
    if selected.package != ["koidrive-$SUITE"] or selected.workspace or selected.exclude:
        raise ValueError("Each Rust shard must execute its matrix package")
    features = re.findall(r"(?m)^          FEATURES: (.+)$", shards)
    if len(features) != 2 or features[0] != features[1]:
        raise ValueError("Inventory and execution must use identical feature expressions")
    partitions = {"remainder": remainder}
    for package in packages:
        suite = package.removeprefix("koidrive-")
        values = {"koidrive-$SUITE": package, "$FEATURES": suite_features(features[0], suite)}
        partitions[suite] = [values.get(arg, arg) for arg in command]
    return partitions


def cargo_features(arguments, target, roots_only=False):
    command = ["cargo", "tree", *arguments, "--target", target,
               "--prefix", "none", "--format", "{p}\t{f}", "--color", "never"]
    if roots_only:
        command.extend(["--depth", "0"])
    output = subprocess.check_output(command, cwd=WORKFLOW.parent.parent, text=True)
    features = {}
    for line in output.splitlines():
        if "\t" not in line:
            continue
        package, enabled = line.split("\t", 1)
        name = package.split()[0]
        enabled = enabled.removesuffix(" (*)")
        features.setdefault(name, set()).add(frozenset(filter(None, enabled.split(","))))
    return features


def check_features(workflow, target):
    expected = cargo_features(["--locked", "--workspace", "--features",
                               "koidrive-cli/test-install-inventory,koidrive-cli/test-setup"], target, roots_only=True)
    for partition, command in check_partitions(workflow).items():
        actual = cargo_features(command[2:], target)
        for package in expected.keys() & actual.keys():
            if expected[package] != actual[package]:
                raise ValueError(f"{target} {partition}: {package} features differ from workspace: "
                                 f"expected {expected[package]}, got {actual[package]}")


class RustPartitions(unittest.TestCase):
    def setUp(self):
        self.workflow = WORKFLOW.read_text()

    def test_every_excluded_package_runs_once(self):
        check_partitions(self.workflow)

    def test_missing_exclusion_is_rejected(self):
        changed = self.workflow.replace(" --exclude koidrive-sim", "")
        with self.assertRaisesRegex(ValueError, "Every excluded package"):
            check_partitions(changed)

    def test_missing_shard_is_rejected(self):
        changed = self.workflow.replace("""'rust_suites=["driver","engine","sim"]'""", """'rust_suites=["driver","engine"]'""")
        with self.assertRaisesRegex(ValueError, "Every excluded package"):
            check_partitions(changed)

    def test_duplicate_shard_is_rejected(self):
        changed = self.workflow.replace("""'rust_suites=["driver","engine","sim"]'""", """'rust_suites=["driver","engine","sim","sim"]'""")
        with self.assertRaisesRegex(ValueError, "duplicate packages"):
            check_partitions(changed)

    def test_literal_suite_matrix_is_rejected(self):
        changed = self.workflow.replace("suite: ${{ fromJSON(needs.changes.outputs.rust_suites) }}", "suite: [driver, engine, sim]")
        with self.assertRaisesRegex(ValueError, "must come from the changes job"):
            check_partitions(changed)

    def test_suite_without_a_selector_job_is_rejected(self):
        selector = SELECTOR.read_text().replace("[job.rust_sim]", "[job.rust_simulation]")
        with self.assertRaisesRegex(ValueError, "no job for the Rust suites"):
            check_partitions(self.workflow, selector)

    def test_different_listing_selection_is_rejected(self):
        changed = self.workflow.replace(" --exclude koidrive-sim", "", 1)
        with self.assertRaisesRegex(ValueError, "identical Cargo arguments"):
            check_partitions(changed)

    def test_additional_shard_package_is_rejected(self):
        changed = self.workflow.replace('-p "koidrive-$SUITE"', '-p "koidrive-$SUITE" -p koidrive-cli')
        with self.assertRaisesRegex(ValueError, "execute its matrix package"):
            check_partitions(changed)

    def test_shard_target_filter_is_rejected(self):
        changed = self.workflow.replace('-p "koidrive-$SUITE"', '-p "koidrive-$SUITE" --lib')
        with self.assertRaisesRegex(ValueError, "not filter default test targets"):
            check_partitions(changed)

    def test_remainder_target_filter_is_rejected(self):
        changed = self.workflow.replace("cargo test --workspace", "cargo test --workspace --lib")
        with self.assertRaisesRegex(ValueError, "not filter default test targets"):
            check_partitions(changed)

    def test_different_inventory_features_are_rejected(self):
        changed = self.workflow.replace("FEATURES: ${{ matrix.suite", "FEATURES: changed ${{ matrix.suite", 1)
        with self.assertRaisesRegex(ValueError, "identical feature expressions"):
            check_partitions(changed)

class FeatureParity(unittest.TestCase):
    def setUp(self):
        self.workflow = WORKFLOW.read_text()

    def test_features_match_workspace_on_both_platforms(self):
        for target in TARGETS:
            with self.subTest(target=target):
                check_features(self.workflow, target)

    def test_missing_engine_serde_is_rejected(self):
        changed = self.workflow.replace("matrix.suite == 'engine' && 'serde' || ", "")
        with self.assertRaisesRegex(ValueError, "engine: koidrive-engine features differ"):
            check_features(changed, TARGETS[0])

    def test_missing_sim_engine_serde_is_rejected(self):
        changed = self.workflow.replace("matrix.suite == 'sim' && 'koidrive-engine/serde' || ", "")
        with self.assertRaisesRegex(ValueError, "sim: koidrive-engine features differ"):
            check_features(changed, TARGETS[0])


if __name__ == "__main__":
    unittest.main()
