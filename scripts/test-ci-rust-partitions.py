#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
from pathlib import Path
import importlib.util
import itertools
import re
import shlex
import subprocess
import unittest


ROOT = Path(__file__).resolve().parent.parent
WORKFLOW = ROOT / ".github/workflows/ci.yml"
SELECTOR = ROOT / ".github/ci-select.toml"
TARGETS = ("aarch64-apple-darwin", "x86_64-unknown-linux-gnu")
SPEC = importlib.util.spec_from_file_location("ci_rust_tests", ROOT / "scripts/ci-rust-tests.py")
CI_RUST_TESTS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CI_RUST_TESTS)
ARCHIVED = ("rust", "rust_driver", "rust_engine")


def job(workflow, name):
    body = workflow.split(f"\n  {name}:\n", 1)[1]
    return re.split(r"\n  \S", body, maxsplit=1)[0]


def members():
    text = (ROOT / "Cargo.toml").read_text()
    paths = re.findall(r'"([^"]+)"', re.search(r"(?s)members = \[(.*?)\]", text)[1])
    return {re.search(r'(?m)^name = "([^"]+)"', (ROOT / path / "Cargo.toml").read_text())[1] for path in paths}


def selected_packages(args, workspace):
    packages, excluded, rest = [], [], iter(args)
    everything = False
    for arg in rest:
        if arg == "--workspace":
            everything = True
        elif arg == "--exclude":
            excluded.append(next(rest))
        elif arg in ("-p", "--package"):
            packages.append(next(rest))
        elif arg == "--features":
            next(rest)
        else:
            raise ValueError(f"Archive arguments must not filter default test targets, but pass {arg}")
    if everything == bool(packages):
        raise ValueError("Archive arguments must name the workspace or packages, not both")
    return set(workspace) - set(excluded) if everything else set(packages)


def sim_command(workflow):
    body = job(workflow, "rust-sim")
    if "needs.changes.outputs.rust_sim == 'true'" not in body:
        raise ValueError("The sim job must run when the change selects rust_sim")
    commands = [shlex.split(line) for line in re.findall(r"cargo test[^\n]+", body) if "koidrive-test-support" not in line]
    commands = [command[:command.index(">")] if ">" in command else command for command in commands]
    builds = [command for command in commands if "--no-run" in command]
    runs = [command for command in commands if "--no-run" not in command]
    if len(builds) != 1 or len(runs) != 2:
        raise ValueError("The sim job must have one checked build, one listing and one execution command")
    listing, execution = runs
    if listing[listing.index("--") + 1:listing.index("--") + 2] != ["--list"] or listing[:listing.index("--")] != execution:
        raise ValueError("Inventory and execution must use identical Cargo arguments")
    if [arg for arg in builds[0] if arg != "--no-run" and not arg.startswith("--message-format=")] != execution:
        raise ValueError("The checked build must use the execution's Cargo arguments")
    if execution.count("-p") != 1 or execution[execution.index("-p") + 1] != "koidrive-sim":
        raise ValueError("The sim job must execute koidrive-sim alone")
    return execution


def check_partitions(workflow, selector=None, archive_args=CI_RUST_TESTS.archive_args):
    selector = SELECTOR.read_text() if selector is None else selector
    missing = [name for name in (*ARCHIVED, "rust_sim", "rust_lint") if f"\n[job.{name}]\n" not in selector]
    if missing:
        raise ValueError(f"ci-select.toml has no job for {missing}")
    if "ci-rust-tests.py archive-args" not in job(workflow, "rust-build-linux"):
        raise ValueError("The archive must be built from ci-rust-tests.py archive-args")
    workspace = members()
    partitions = {}
    for size in range(1, len(ARCHIVED) + 1):
        for subset in itertools.combinations(ARCHIVED, size):
            args = archive_args(set(subset))
            selected_packages(args, workspace)
            partitions["+".join(subset)] = ["cargo", "test", "--locked", *args]
    archived = selected_packages(archive_args(set(ARCHIVED)), workspace)
    sim = sim_command(workflow)
    if "koidrive-sim" in archived:
        raise ValueError("Every package must run in exactly one Rust partition, but koidrive-sim is in the archive and the sim job")
    if archived | {"koidrive-sim"} != workspace:
        raise ValueError(f"Every package must run in exactly one Rust partition, but {sorted(workspace - archived - {'koidrive-sim'})} runs in none")
    for package, job_name in (("koidrive-driver", "rust_driver"), ("koidrive-engine", "rust_engine")):
        if package not in selected_packages(archive_args({job_name}), workspace):
            raise ValueError(f"{job_name} alone must archive {package}")
    partitions["sim"] = [arg for arg in sim if arg not in ("--profile", "checked", "--no-fail-fast")]
    return partitions


def cargo_features(arguments, target, roots_only=False):
    command = ["cargo", "tree", *arguments, "--target", target,
               "--prefix", "none", "--format", "{p}\t{f}", "--color", "never"]
    if roots_only:
        command.extend(["--depth", "0"])
    output = subprocess.check_output(command, cwd=ROOT, text=True)
    features = {}
    for line in output.splitlines():
        if "\t" not in line:
            continue
        package, enabled = line.split("\t", 1)
        name = package.split()[0]
        enabled = enabled.removesuffix(" (*)")
        features.setdefault(name, set()).add(frozenset(filter(None, enabled.split(","))))
    return features


def check_features(workflow, target, archive_args=CI_RUST_TESTS.archive_args):
    expected = cargo_features(["--locked", "--workspace", "--features",
                               "koidrive-cli/test-install-inventory,koidrive-cli/test-setup"], target, roots_only=True)
    for partition, command in check_partitions(workflow, archive_args=archive_args).items():
        actual = cargo_features(command[2:], target)
        for package in expected.keys() & actual.keys():
            if expected[package] != actual[package]:
                raise ValueError(f"{target} {partition}: {package} features differ from workspace: "
                                 f"expected {expected[package]}, got {actual[package]}")


class RustPartitions(unittest.TestCase):
    def setUp(self):
        self.workflow = WORKFLOW.read_text()

    def test_every_package_runs_once(self):
        check_partitions(self.workflow)

    def test_sim_in_the_archive_is_rejected(self):
        def archive(selected):
            return ["--workspace"] if "rust" in selected else CI_RUST_TESTS.archive_args(selected)
        with self.assertRaisesRegex(ValueError, "koidrive-sim is in the archive and the sim job"):
            check_partitions(self.workflow, archive_args=archive)

    def test_a_package_in_no_partition_is_rejected(self):
        def archive(selected):
            args = CI_RUST_TESTS.archive_args(selected)
            return ["--exclude", "koidrive-fs", *args] if "rust" in selected else args
        with self.assertRaisesRegex(ValueError, r"\['koidrive-fs'\] runs in none"):
            check_partitions(self.workflow, archive_args=archive)

    def test_an_archive_that_drops_a_suite_is_rejected(self):
        def archive(selected):
            return CI_RUST_TESTS.archive_args({"rust_driver"} if selected == {"rust_engine"} else selected)
        with self.assertRaisesRegex(ValueError, "rust_engine alone must archive koidrive-engine"):
            check_partitions(self.workflow, archive_args=archive)

    def test_a_target_filter_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "must not filter default test targets"):
            check_partitions(self.workflow, archive_args=lambda selected: [*CI_RUST_TESTS.archive_args(selected), "--lib"])

    def test_a_sim_job_on_another_package_is_rejected(self):
        changed = self.workflow.replace("-p koidrive-sim --features", "-p koidrive-chunk --features")
        with self.assertRaisesRegex(ValueError, "execute koidrive-sim alone"):
            check_partitions(changed)

    def test_different_listing_selection_is_rejected(self):
        changed = self.workflow.replace("koidrive-engine/serde -- --list", "koidrive-engine/serde --lib -- --list")
        with self.assertRaisesRegex(ValueError, "identical Cargo arguments"):
            check_partitions(changed)

    def test_checked_build_with_other_arguments_is_rejected(self):
        changed = self.workflow.replace("koidrive-engine/serde --no-run", "koidrive-engine/serde --release --no-run")
        with self.assertRaisesRegex(ValueError, "checked build must use the execution's Cargo arguments"):
            check_partitions(changed)

    def test_suite_without_a_selector_job_is_rejected(self):
        selector = SELECTOR.read_text().replace("[job.rust_sim]", "[job.rust_simulation]")
        with self.assertRaisesRegex(ValueError, "no job for"):
            check_partitions(self.workflow, selector)


class FeatureParity(unittest.TestCase):
    def setUp(self):
        self.workflow = WORKFLOW.read_text()

    def test_features_match_workspace_on_both_platforms(self):
        for target in TARGETS:
            with self.subTest(target=target):
                check_features(self.workflow, target)

    def test_missing_engine_serde_is_rejected(self):
        def archive(selected):
            args = CI_RUST_TESTS.archive_args(selected)
            return args if "rust" in selected else [arg for arg in args if arg not in ("--features", "koidrive-engine/serde")]
        with self.assertRaisesRegex(ValueError, "rust_engine: koidrive-engine features differ"):
            check_features(self.workflow, TARGETS[0], archive_args=archive)

    def test_missing_sim_engine_serde_is_rejected(self):
        changed = self.workflow.replace(" --features koidrive-engine/serde", "")
        with self.assertRaisesRegex(ValueError, "sim: koidrive-engine features differ"):
            check_features(changed, TARGETS[0])


if __name__ == "__main__":
    unittest.main()
