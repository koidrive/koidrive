#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

from pathlib import Path
import contextlib
import importlib.util
import io
import itertools
import json
import os
import re
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parent.parent
WORKFLOW = ROOT / ".github/workflows/property-battery.yml"
HEAVY_WORKFLOW = ROOT / ".github/workflows/heavy-tests.yml"
CI_WORKFLOW = ROOT / ".github/workflows/ci.yml"
RUNTIME_CHECKS = ROOT / "scripts/require-runtime-checks.sh"


def ci_job(workflow, name):
    return re.split(r"\n  \S", workflow.split(f"\n  {name}:\n", 1)[1], maxsplit=1)[0]


def commands(workflow):
    if "PROPTEST_CASES" in re.sub(r"env -u PROPTEST_CASES\b", "", workflow):
        raise ValueError("Battery must only mention PROPTEST_CASES to unset it")
    execution = next(line.strip() for line in workflow.splitlines()
                     if '(cd "$dir" &&' in line and 'tee "$log"' in line)
    repro = next(line.strip() for line in workflow.splitlines() if 'repro="git checkout' in line)
    for command in (execution, repro):
        if "env -u PROPTEST_CASES" not in command or "KOIDRIVE_BATTERY_CASES=" not in command:
            raise ValueError("Execution and repro must clear ordinary overrides and set stress budgets")
    if "cases per property test" in workflow or "each property test ran" in workflow:
        raise ValueError("Summary must not apply the stress budget to every property")
    for text in ("Cases per opted-in stress property", "Bounded properties keep their own",
                 "tests outside property blocks run once per battery", "saved regressions also replay"):
        if text not in workflow:
            raise ValueError("Input and summary must distinguish stress budgets, bounded properties and unit tests")
    return execution, repro


def heavy_commands(workflow):
    if re.search(r"(?:export |\s)PROPTEST_CASES=\"?\$", workflow):
        raise ValueError("Heavy tests must not export a global case override")
    lines = [line.strip() for line in workflow.splitlines()]
    clear = next(line for line in lines if line.startswith("unset PROPTEST_CASES"))
    budget = next(line for line in lines if '|| export KOIDRIVE_BATTERY_CASES=' in line)
    execution = next(line for line in lines if line.startswith("PROPTEST_RNG_SEED=$seed cargo test"))
    repro = next(line for line in lines if line.startswith('repro="git checkout'))
    if clear != "unset PROPTEST_CASES KOIDRIVE_BATTERY_CASES":
        raise ValueError("Heavy tests must clear both inherited budgets")
    if "env -u PROPTEST_CASES -u KOIDRIVE_BATTERY_CASES" not in repro:
        raise ValueError("Heavy-test repro must clear both inherited budgets")
    if "${CASES:+ KOIDRIVE_BATTERY_CASES=$CASES}" not in repro:
        raise ValueError("Heavy-test repro must preserve the optional stress budget")
    return clear + "\n" + budget, execution, repro


class PropertyBattery(unittest.TestCase):
    def setUp(self):
        self.workflow = WORKFLOW.read_text()

    def test_workflow_budget_contract(self):
        commands(self.workflow)
        self.assertIn("python3 scripts/test-property-battery.py -v",
                      (ROOT / ".github/workflows/ci.yml").read_text())

    def test_global_case_override_is_rejected(self):
        for assignment in ("  PROPTEST_CASES: '8192'", "  export PROPTEST_CASES=8192"):
            with self.subTest(assignment=assignment), self.assertRaisesRegex(ValueError, "only mention"):
                commands(self.workflow + "\n" + assignment)

    def test_execution_and_repro_must_clear_inherited_overrides(self):
        for command in commands(self.workflow):
            changed = self.workflow.replace(command, command.replace("env -u PROPTEST_CASES ", ""))
            with self.assertRaisesRegex(ValueError, "clear ordinary overrides"):
                commands(changed)

    def test_execution_and_repro_must_supply_stress_budget(self):
        for command in commands(self.workflow):
            changed = self.workflow.replace(command, command.replace("KOIDRIVE_BATTERY_CASES=", "OTHER_CASES="))
            with self.assertRaisesRegex(ValueError, "set stress budgets"):
                commands(changed)

    def test_misleading_summary_is_rejected(self):
        changed = self.workflow.replace("Stress budget:", "Cases: each property test ran")
        with self.assertRaisesRegex(ValueError, "every property"):
            commands(changed)

    def test_summary_reports_stress_budgets_separately_from_tests(self):
        lines = [line.strip() for line in self.workflow.splitlines()
                 if any(marker in line for marker in ('echo "- Stress budget:',
                                                       'echo "- Bounded properties',
                                                       'echo "- Tests:'))]
        output = subprocess.check_output(
            ["bash", "-eu", "-c", 'CASES=2; passing=3; tests_passed=8; tests_failed=1\n' + "\n".join(lines)],
            text=True)
        self.assertIn("2 generated cases per selected, opted-in stress property on each passing shard, 6 in total", output)
        self.assertIn("saved regressions also replay", output)
        self.assertIn("Bounded properties keep their own case budgets; tests outside property blocks run once per battery",
                      output)
        self.assertIn("Tests: 8 passed and 1 failed, summed over shards", output)

    def test_execution_and_repro_pass_isolated_budgets_and_same_selection(self):
        execution, repro = commands(self.workflow)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "tests").mkdir()
            (root / "battery/bin").mkdir(parents=True)
            (root / "bin").mkdir()
            probe = '#!/bin/sh\nprintf "ordinary=%s stress=%s seed=%s\\n" "${PROPTEST_CASES-unset}" "$KOIDRIVE_BATTERY_CASES" "$PROPTEST_RNG_SEED"\nprintf "arg=%s\\n" "$@"\n'
            for path, content in ((root / "battery/bin/probe", probe),
                                  (root / "bin/cargo", probe),
                                  (root / "bin/git", "#!/bin/sh\nexit 0\n")):
                path.write_text(content)
                path.chmod(0o755)
            environment = dict(os.environ, PROPTEST_CASES="8192", GITHUB_WORKSPACE=str(root),
                               PATH=str(root / "bin") + os.pathsep + os.environ["PATH"])
            setup = 'dir=tests; binary=bin/probe; CASES=2; seed=17; SHA=abc; package=koidrive-sim; selector="--test engine_sync"; tests=(mod::red other); log="$GITHUB_WORKSPACE/log"\n'
            failed_tests = "\n".join(line.strip() for line in self.workflow.splitlines()
                                     if line.strip().startswith("red=") or "repro_tests=" in line)
            outputs = []
            for command in (execution, failed_tests + "\n" + repro + '\neval "$repro"'):
                outputs.append(subprocess.check_output(["bash", "-euo", "pipefail", "-c", setup + command],
                                                       cwd=root, env=environment, text=True))
                (root / "log").write_text("test mod::red ... FAILED\ntest other ... ok\n")
            for output in outputs:
                self.assertIn("ordinary=unset stress=2 seed=17", output)
            self.assertIn("arg=--exact\narg=mod::red\narg=other\n", outputs[0])
            self.assertIn("arg=--test\narg=engine_sync\narg=--\narg=--exact\narg=mod::red\n", outputs[1])
            self.assertNotIn("arg=other", outputs[1])


class Selection(unittest.TestCase):
    def setUp(self):
        workflow = WORKFLOW.read_text()
        self.split = next(line.strip() for line in workflow.splitlines() if "print > every" in line)
        self.spread = next(line.strip() for line in workflow.splitlines() if "% shards == shard" in line)
        self.packages = next(line.strip() for line in workflow.splitlines() if "then spread=1" in line)

    def split_tests(self, names, listed, package="koidrive-sim"):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "names.txt").write_text("".join(name + "\n" for name in names))
            (root / "listed.txt").write_text("".join(test + "\n" for test in listed))
            script = (f'RUNNER_TEMP=.; ONCE_PACKAGES="koidrive-engine koidrive-sim"; package={package}; '
                      'every=every.txt; once=once.txt; : > "$every"; : > "$once"\n'
                      + self.packages + "\n" + self.split)
            subprocess.check_call(["bash", "-euo", "pipefail", "-c", script], cwd=root)
            return (root / "every.txt").read_text().split(), (root / "once.txt").read_text().split()

    def test_property_tests_go_to_every_shard_and_other_tests_once(self):
        listed = ["tests::a_unit", "tests::grows", "grows_more", "deep::mod::shrinks", "shrinks_not"]
        every, once = self.split_tests(["grows", "shrinks"], listed)
        self.assertEqual(every, ["tests::grows", "deep::mod::shrinks"])
        self.assertEqual(once, ["tests::a_unit", "grows_more", "shrinks_not"])
        self.assertEqual(self.split_tests([], listed), ([], listed))

    def test_every_test_of_a_package_outside_the_once_list_goes_to_every_shard(self):
        listed = ["tests::a_unit", "tests::grows"]
        for package in ("koidrive-driver", "koidrive-sim-extra", "koidrive"):
            with self.subTest(package=package):
                self.assertEqual(self.split_tests(["grows"], listed, package), (listed, []))

    def test_a_property_name_that_ends_another_test_is_ambiguous(self):
        line = next(line.strip() for line in WORKFLOW.read_text().splitlines() if line.strip().startswith("ambiguous=$(awk"))
        for listed, expected in ((["a::same", "b::same", "c::other"], "same"),
                                 (["b::same", "c::other", "same_more"], ""),
                                 (["same", "deep::mod::same"], "same")):
            with self.subTest(listed=listed), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                (root / "names.txt").write_text("same\nother\n")
                (root / "listed.txt").write_text("".join(test + "\n" for test in listed))
                output = subprocess.check_output(["bash", "-euo", "pipefail", "-c",
                                                  'RUNNER_TEMP=.\n' + line + '\necho "$ambiguous"'],
                                                 cwd=root, text=True)
                self.assertEqual(output.strip(), expected)

    def test_once_packages_hold_no_driver_or_net_tests(self):
        once = re.search(r"^  ONCE_PACKAGES: (.*)$", WORKFLOW.read_text(), re.M).group(1).split()
        self.assertEqual(once, ["koidrive-chunk", "koidrive-engine", "koidrive-ignore", "koidrive-sim",
                                "koidrive-test-support"])

    def test_each_other_test_runs_on_exactly_one_shard(self):
        for shards, binaries in ((1, [3]), (4, [3, 0, 6]), (48, [700, 115, 2])):
            with self.subTest(shards=shards), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                runs = []
                per_shard = []
                for shard in range(shards):
                    before = len(runs)
                    offset = 0
                    for binary, count in enumerate(binaries):
                        tests = [f"{binary}::{index}" for index in range(count)]
                        (root / "once.txt").write_text("".join(test + "\n" for test in tests))
                        script = (f"SHARD={shard}; SHARDS={shards}; offset={offset}; name=x; out=.; "
                                  "mkdir -p battery/once battery/every; mv once.txt battery/once/x.txt; "
                                  "echo every > battery/every/x.txt\n" + self.spread)
                        subprocess.check_call(["bash", "-euo", "pipefail", "-c", script], cwd=root)
                        ran = (root / "tests-x.txt").read_text().split()
                        self.assertEqual(ran[0], "every")
                        runs.extend(ran[1:])
                        offset += count
                    per_shard.append(len(runs) - before)
                self.assertLessEqual(max(per_shard) - min(per_shard), 1, per_shard)
                expected = [f"{binary}::{index}" for binary, count in enumerate(binaries) for index in range(count)]
                self.assertEqual(sorted(runs), sorted(expected))


class RuntimeChecks(unittest.TestCase):
    def check(self, *units, probe="test result: ok. 1 passed; 0 failed"):
        with tempfile.TemporaryDirectory() as directory:
            executable = Path(directory) / "probe"
            executable.write_text(f"#!/bin/sh\necho '{probe}'\n")
            executable.chmod(0o755)
            support = self.unit("koidrive_test_support")
            support["profile"]["test"] = True
            support["executable"] = str(executable)
            build = Path(directory) / "build.json"
            build.write_text("".join(json.dumps(unit) + "\n" for unit in (*units, support) if probe is not None or unit is not support))
            return subprocess.run([str(RUNTIME_CHECKS), str(build)], capture_output=True, text=True)

    @staticmethod
    def unit(name, kind="lib", debug_assertions=True, overflow_checks=True):
        return {"reason": "compiler-artifact", "package_id": name, "target": {"name": name, "kind": [kind]},
                "profile": {"debug_assertions": debug_assertions, "overflow_checks": overflow_checks}}

    def test_units_with_both_checks_pass(self):
        result = self.check(self.unit("koidrive_engine"), self.unit("devices", "test"), {"reason": "build-finished"})
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("3 units build with debug assertions and overflow checks, and the runtime probe passed", result.stdout)

    def test_a_failing_or_missing_probe_fails(self):
        for probe in ("test result: FAILED. 0 passed; 1 failed", "test result: ok. 0 passed; 0 failed", None):
            with self.subTest(probe=probe):
                result = self.check(self.unit("devices", "test"), probe=probe)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("RUSTFLAGS", result.stdout)

    def test_a_unit_without_a_check_fails(self):
        for weakened in ({"debug_assertions": False}, {"overflow_checks": False}, {"overflow_checks": None}):
            with self.subTest(weakened=weakened):
                result = self.check(self.unit("devices", "test"), self.unit("koidrive_engine", **weakened))
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("koidrive_engine", result.stdout)

    def test_build_scripts_and_proc_macros_run_only_at_build_time(self):
        result = self.check(self.unit("devices", "test"),
                            self.unit("build_script_build", "custom-build", False, False),
                            self.unit("prost_derive", "proc-macro", False, False))
        self.assertEqual(result.returncode, 0, result.stdout)

    def test_a_build_without_units_fails(self):
        result = self.check({"reason": "build-finished"}, probe=None)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("no compiled unit", result.stdout)

    def test_every_test_build_checks_its_units(self):
        battery = WORKFLOW.read_text()
        self.assertIn('scripts/require-runtime-checks.sh "$RUNNER_TEMP/build.json"', battery)
        ci = CI_WORKFLOW.read_text()
        check = 'scripts/require-runtime-checks.sh "$RUNNER_TEMP/build.json" "$RUNNER_TEMP/probe.json"'
        jobs = {
            "rust-build-linux": ('cargo nextest archive --locked "$@" --archive-file "$out/archive.tar.zst" --cargo-message-format json-render-diagnostics > "$RUNNER_TEMP/build.json"',
                                 'cargo test --locked -p koidrive-test-support --lib --no-run --message-format=json-render-diagnostics > "$RUNNER_TEMP/probe.json"'),
            "rust-sim": ('cargo test --locked --profile checked --no-fail-fast -p koidrive-sim --features koidrive-engine/serde --no-run --message-format=json-render-diagnostics > "$RUNNER_TEMP/build.json"',
                         'cargo test --locked --profile checked -p koidrive-test-support --lib --no-run --message-format=json-render-diagnostics > "$RUNNER_TEMP/probe.json"'),
        }
        for name, (build, probe) in jobs.items():
            body = ci_job(ci, name)
            with self.subTest(job=name):
                self.assertLess(body.index(build), body.index(check))
                self.assertLess(body.index(probe), body.index(check))
        self.assertIn("    steps: *rust-build-steps\n", ci_job(ci, "rust-build-macos"))
        self.assertIn("    steps: &rust-build-steps\n", ci_job(ci, "rust-build-linux"))

    def test_battery_builds_and_reproduces_with_the_checked_profile(self):
        workflow = WORKFLOW.read_text()
        _, repro = commands(workflow)
        build = next(line for line in workflow.splitlines() if "--no-run --tests" in line)
        for command in (build, repro):
            self.assertIn("cargo test --locked --profile checked ", command)

    def test_ci_selects_only_dev_or_checked(self):
        for workflow in (WORKFLOW.read_text(), CI_WORKFLOW.read_text()):
            commands = re.findall(r"cargo (?:test|build|run|nextest (?:archive|run|list))\b[^\n]*", workflow)
            for command in commands:
                with self.subTest(command=command):
                    if command.startswith(("cargo test", "cargo nextest")):
                        self.assertNotRegex(command, r"(?:^|\s)(?:-r|--release)(?:\s|$)")
                    option = "--cargo-profile" if command.startswith("cargo nextest") else "--profile"
                    for profile in re.findall(rf"(?:^|\s){option}[= ](\S+)", command):
                        self.assertIn(profile, ("checked", '"$PROFILE"'))
        self.assertIn("cargo nextest archive", CI_WORKFLOW.read_text())


class HeavyTests(unittest.TestCase):
    def setUp(self):
        self.workflow = HEAVY_WORKFLOW.read_text()

    def test_global_override_is_rejected(self):
        changed = self.workflow.replace("export KOIDRIVE_BATTERY_CASES=", "export PROPTEST_CASES=")
        with self.assertRaisesRegex(ValueError, "global case override"):
            heavy_commands(changed)

    def test_inherited_budgets_must_be_cleared_in_execution_and_repro(self):
        for original in ("unset PROPTEST_CASES KOIDRIVE_BATTERY_CASES",
                         "env -u PROPTEST_CASES -u KOIDRIVE_BATTERY_CASES"):
            changed = self.workflow.replace(original, original.replace(" KOIDRIVE_BATTERY_CASES", ""))
            with self.assertRaisesRegex(ValueError, "clear both"):
                heavy_commands(changed)

    def test_execution_and_repro_use_only_the_optional_stress_budget(self):
        budget, execution, repro = heavy_commands(self.workflow)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            probe = '#!/bin/sh\nprintf "ordinary=%s stress=%s seed=%s\\n" "${PROPTEST_CASES-unset}" "${KOIDRIVE_BATTERY_CASES-unset}" "$PROPTEST_RNG_SEED"\nprintf "pair=%s\\n" "${MODEL_PAIR-unset}"\nprintf "arg=%s\\n" "$@"\n'
            for name, content in (("cargo", probe), ("git", "#!/bin/sh\nexit 0\n")):
                path = root / name
                path.write_text(content)
                path.chmod(0o755)
            environment = dict(os.environ, PROPTEST_CASES="8192", KOIDRIVE_BATTERY_CASES="999",
                               PATH=str(root) + os.pathsep + os.environ["PATH"])
            for cases, expected in (("", "unset"), ("2", "2"), ("0", "0")):
                setup = f'CASES="{cases}"; SHA=abc; seed=17; cargo_args=(--locked -p koidrive-sim); test_args=("a filter"); log="{root}/log"; repro_args=" --locked -p koidrive-sim -- a\\ filter"; repro_env=" MODEL_PAIR=a\\*b"\n'
                for command, pair in ((budget + "\n" + execution, "unset"),
                                      (repro + '\neval "${repro/<seed>/$seed}"', "a*b")):
                    output = subprocess.check_output(["bash", "-euo", "pipefail", "-c", setup + command],
                                                     env=environment, text=True)
                    self.assertIn(f"ordinary=unset stress={expected} seed=17", output)
                    self.assertIn(f"pair={pair}", output)
                    self.assertIn("arg=a filter", output)
                    self.assertIn("arg=koidrive-sim", output)

    def step(self, name):
        block = self.workflow.split(f"\n      - name: {name}\n", 1)[1]
        lines = block.split("        run: |\n", 1)[1].splitlines()
        return "\n".join(line[10:] for line in itertools.takewhile(lambda line: line.startswith(" " * 10) or not line, lines))

    def check_env(self, extra_env):
        environment = dict(os.environ, MODE="cargo", PARALLEL="1", CRATE="koidrive-chunk", TEST="", FILTER="", CASES="",
                           REPEAT="1", SEED="", FEATURES="", IGNORED="false", RELEASE="false", EXTRA_ENV=extra_env, REF="", OS="ubuntu")
        return subprocess.run(["bash", "-eo", "pipefail", "-c", self.step("Check the inputs")],
                              env=environment, capture_output=True, text=True)

    def run_tests(self, extra_env, output, code=0):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "bin").mkdir()
            (root / "bin/cargo").write_text(f'#!/bin/sh\nprintf "arg=%s\\n" "$@" > "{root}/argv"\ncat "{root}/output"\nexit {code}\n')
            (root / "bin/git").write_text("#!/bin/sh\nexit 0\n")
            for stub in ("cargo", "git"):
                (root / "bin" / stub).chmod(0o755)
            (root / "output").write_text(output)
            (root / "temp").mkdir()
            environment = dict(os.environ, PATH=f"{root / 'bin'}{os.pathsep}{os.environ['PATH']}", RUNNER_TEMP=str(root / "temp"),
                               RUNNER_OS="Linux", GITHUB_STEP_SUMMARY=str(root / "summary"), SHA="abc", SEED_BASE="17",
                               CRATE="koidrive-chunk", TEST="", FILTER="", CASES="", REPEAT="1", FEATURES="", IGNORED="false",
                               RELEASE="false", EXTRA_ENV=extra_env)
            result = subprocess.run(["bash", "-eo", "pipefail", "-c", self.step("Run the tests")],
                                    env=environment, capture_output=True, text=True)
            return (result.returncode, (root / "argv").read_text().splitlines(),
                    (root / "temp/heavy-tests/failed-tests.txt").read_text().splitlines())

    def test_proptest_verbose_is_the_only_new_name_and_takes_a_level(self):
        self.assertEqual(self.check_env("PROPTEST_VERBOSE=2 RUST_LOG=debug").returncode, 0)
        for extra_env in ("PROPTEST_VERBOSE=x", "PROPTEST_VERBOSE=", "PROPTEST_VERBOSE=12", "PROPTEST_CASES=0"):
            with self.subTest(extra_env=extra_env):
                self.assertNotEqual(self.check_env(extra_env).returncode, 0)

    def test_output_capture_is_turned_off_only_for_a_nonzero_proptest_verbose(self):
        passing = "     Running tests/chunking.rs (target/debug/deps/chunking-0)\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n"
        for extra_env, expected in (("", []), ("RUST_LOG=debug", []), ("PROPTEST_VERBOSE=0", []),
                                    ("PROPTEST_VERBOSE=2", ["arg=--nocapture"])):
            with self.subTest(extra_env=extra_env):
                code, argv, _ = self.run_tests(extra_env, passing)
                self.assertEqual(code, 0)
                self.assertEqual(argv[argv.index("arg=--") + 1:], expected)

    def test_failed_tests_come_from_the_closing_failures_list(self):
        failing = ("     Running tests/chunking.rs (target/debug/deps/chunking-0)\n"
                   "test a::first ... Test case passed\nFAILED\n"
                   "test second ... ok\ntest third ... FAILED\n\n"
                   "failures:\n\n---- third stdout ----\n    indented panic text\n\n"
                   "failures:\n    a::first\n    third\n\n"
                   "test result: FAILED. 1 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out\n")
        for extra_env in ("", "PROPTEST_VERBOSE=2 RUST_TEST_THREADS=1"):
            with self.subTest(extra_env=extra_env):
                code, _, failed = self.run_tests(extra_env, failing, code=101)
                self.assertNotEqual(code, 0)
                self.assertEqual(failed, ["a::first", "third"])



TRIAGE_SPEC = importlib.util.spec_from_file_location("triage", ROOT / "scripts/triage-battery.py")
triage = importlib.util.module_from_spec(TRIAGE_SPEC)
TRIAGE_SPEC.loader.exec_module(triage)

FAILING_LOG = """running 3 tests
test devices_converge ... FAILED
test devices_lose_no_write ... FAILED
test devices_plain ... ok

failures:

---- devices_converge stdout ----
proptest regressions: replay 4 saved cases for tests/devices.proptest-regressions
proptest regression saved in /home/runner/work/koidrive/koidrive/core/sim/tests/devices.cases/cc-{case}.txt: cc {case}

thread 'devices_converge' (2963) panicked at core/sim/tests/devices.rs:326:1:
Test failed: assertion failed: §(left == right)§
  left: §1§,
 right: §2§: write 1 is lost at core/sim/tests/devices/common.rs:120.
minimal failing input: devices = 2
---- devices_lose_no_write stdout ----

thread 'devices_lose_no_write' (2965) panicked at core/sim/tests/devices.rs:278:1:
Test failed: a committed case fails.
minimal failing input: devices = 1

failures:
    devices_converge
    devices_lose_no_write

test result: FAILED. 1 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.00s
""".replace("§", chr(96))


KNOWN = {
    "GUN-1-A-write-lost.toml": """class = "A"
issue = "GUN-1"
tests = ["devices_converge", "devices_lose_no_write"]
message = '''write <n> is lost'''
examples = ["write 1 is lost at core/sim/tests/devices/common.rs:120."]
""",
    "GUN-2-B-no-convergence.toml": """class = "B"
issue = "GUN-2"
tests = ["devices_converge"]
message = '''no convergence within <n> steps'''
examples = ["no convergence within 400 steps"]
""",
    "GUN-3-C-no-convergence.toml": """class = "C"
issue = "GUN-3"
tests = ["devices_lose_no_write"]
message = '''no convergence within <n> steps'''
examples = ["no convergence within 400 steps"]
""",
}


DEVICES_REPLAY = """running 1 test
test {test} ... FAILED

failures:

---- {test} stdout ----

thread '{test}' (306516347) panicked at core/sim/tests/devices.rs:569:5:
{lines}
note: run with §RUST_BACKTRACE=1§ environment variable to display a backtrace


failures:
    {test}

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 538 filtered out; finished in 0.75s
""".replace("§", chr(96))


def write_registry(directory, files):
    directory.mkdir()
    for name, text in reversed(sorted(files.items())):
        (directory / name).write_text(text)
    return directory


def known_failures(files):
    with tempfile.TemporaryDirectory() as directory:
        return triage.known_failures(write_registry(Path(directory) / "known", files))


class KnownFailures(unittest.TestCase):
    def report(self, known, rows):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            spec = {"run": "7", "head": "h" * 40, "base": "b" * 40, "checked_profile": "", "others": [],
                    "cases": [{"case": f"{index:064}", "path": f"t.cases/cc-{index:064}.txt", "binary": "devices",
                               "test": test, "shards": [1], "shared": False}
                              for index, (test, _, _) in enumerate(rows)]}
            (root / "cases.json").write_text(json.dumps(spec))
            for side, position in (("head", 1), ("base", 2)):
                results = [{"runs": [{"result": row[position][0], "message": row[position][1]}] * 2,
                            "target": "sim --test devices"} for row in rows]
                (root / f"replay-{side}.json").write_text(json.dumps({"side": side, "results": results}))
            write_registry(root / "known", known)
            run = subprocess.run([sys.executable, str(ROOT / "scripts/triage-battery.py"), "report",
                                  "--cases", str(root / "cases.json"), "--replays", str(root / "replay-head.json"),
                                  str(root / "replay-base.json"), "--run-url", "https://example.com/runs/7",
                                  "--known-failures", str(root / "known"), "--out", str(root / "out")],
                                 stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
            return (run, json.loads((root / "out/triage.json").read_text())["cases"],
                    (root / "out/triage.md").read_text())

    def test_normalized_masks_ids_numbers_and_positions(self):
        self.assertEqual(
            triage.normalized("assertion failed: §(left == right)§ \n  left: §(\"01a1c2c0-39a0-7d6b-8097-314d939736f8\", "
                              "\"ß (pending)\")§,\n right: §[]§: write 12 of device-0 2026-11-09 is lost at "
                              "core/sim/tests/engine_sync/common.rs:1599:13 and core/x.rs:7.".replace("§", chr(96))),
            "assertion failed: §(left == right)§ left: §(\"<id>\", \"ß (pending)\")§, right: §[]§: write <n> of "
            "device-<n> <n>-<n>-<n> is lost at core/sim/tests/engine_sync/common.rs:<line> and core/x.rs:<line>."
            .replace("§", chr(96)))

    def test_a_message_matches_whole_and_only_for_the_entry_tests(self):
        entries = known_failures(KNOWN)
        def classes(test, message):
            return [entry["class"] for entry in triage.matches(entries, test, message)]
        self.assertEqual(classes("devices_converge", "write 3 is lost"), ["A"])
        self.assertEqual(classes("devices_converge", "write 3 is lost: [Op { id: 9 }]"), [])
        self.assertEqual(classes("devices_converge", "after phase 1: write 3 is lost"), [])
        self.assertEqual(classes("devices_plain", "write 3 is lost"), [])
        self.assertEqual(classes("devices_converge", "no convergence within 400 steps"), ["B"])
        self.assertEqual(classes("devices_lose_no_write", "no convergence within 400 steps"), ["C"])

    def test_the_report_labels_only_rows_that_fail_on_base_with_the_same_message(self):
        run, rows, text = self.report(KNOWN, [
            ("devices_converge", ("fail", "write 1 is lost"), ("fail", "write 1 is lost")),
            ("devices_converge", ("fail", "write 2 is lost"), ("pass", None)),
            ("devices_converge", ("fail", "a name no user gave"), ("fail", "a name no user gave")),
            ("devices_lose_no_write", ("fail", "no convergence within 9 steps"),
             ("fail", "no convergence within 9 steps")),
        ])
        self.assertEqual(run.returncode, 0, run.stdout)
        self.assertEqual([(row["verdict"], row["class"], row["issue"]) for row in rows],
                         [("PR-caused", None, None), (triage.SAME, "A", "GUN-1"),
                          (triage.SAME, triage.UNCLASSIFIED, None), (triage.SAME, "C", "GUN-3")])
        self.assertIn("| PR-caused |  |  |", text)
        self.assertIn(f"| {triage.SAME} | A | GUN-1 |", text)
        self.assertIn(f"| {triage.SAME} | {triage.UNCLASSIFIED} |  |", text)
        self.assertIn(f"2 of the 3 such rows match one; 1 {triage.UNCLASSIFIED} match none.", text)

    def test_a_row_that_matches_two_entries_fails_the_report(self):
        overlapping = {name: text.replace('tests = ["devices_lose_no_write"]', 'tests = ["devices_converge"]')
                       for name, text in KNOWN.items()}
        run, rows, text = self.report(overlapping, [
            ("devices_converge", ("fail", "no convergence within 9 steps"), ("fail", "no convergence within 9 steps")),
        ])
        self.assertNotEqual(run.returncode, 0)
        self.assertIn("devices_converge matches B, C", run.stdout)
        self.assertEqual(rows[0]["class"], "B, C")
        self.assertIn(f"| {triage.SAME} | B, C | GUN-2, GUN-3 |", text)

    def test_an_entry_that_lists_cases_labels_only_those_cases_and_comes_first(self):
        scoped = KNOWN | {"GUN-4-D-write-lost-in-one-case.toml": f"""class = "D"
issue = "GUN-4"
tests = ["devices_converge"]
message = '''write <n> is lost'''
examples = ["write 1 is lost"]
cases = ["{1:064}"]
"""}
        run, rows, text = self.report(scoped, [
            ("devices_converge", ("fail", "write 1 is lost"), ("fail", "write 1 is lost")),
            ("devices_converge", ("fail", "write 2 is lost"), ("fail", "write 2 is lost")),
            ("devices_lose_no_write", ("fail", "write 3 is lost"), ("fail", "write 3 is lost")),
        ])
        self.assertEqual(run.returncode, 0, run.stdout)
        self.assertEqual({row["case"]: (row["class"], row["issue"]) for row in rows},
                         {f"{0:064}": ("A", "GUN-1"), f"{1:064}": ("D", "GUN-4"), f"{2:064}": ("A", "GUN-1")})
        self.assertIn(f"| {triage.SAME} | D | GUN-4 |", text)

    def test_every_registry_example_matches_its_own_entry_and_no_other(self):
        entries = triage.known_failures(triage.KNOWN_FAILURES)
        self.assertTrue(entries)
        for entry in entries:
            with self.subTest(file=entry["file"]):
                self.assertLessEqual(set(entry) - {"class", "issue", "tests", "message", "examples", "pattern", "file"},
                                     {"cases"})
                self.assertRegex(entry["class"], r"^[A-Z]$")
                self.assertRegex(entry["issue"], r"^GUN-[1-9][0-9]*$")
                self.assertTrue(entry["tests"])
                self.assertEqual(len(entry["tests"]), len(set(entry["tests"])))
                self.assertTrue(entry["examples"])
                for example in entry["examples"]:
                    for test in entry["tests"]:
                        for case in entry.get("cases", [""]):
                            found = triage.matches(entries, test, example, case)
                            self.assertEqual([other["file"] for other in found], [entry["file"]], (test, example, case))

    def test_registry_entries_that_share_a_message_label_only_their_own_tests_and_cases(self):
        entries = triage.known_failures(triage.KNOWN_FAILURES)
        left = "after phase 2: write 6 left the disk of device 2: the server never held it, and its user never overwrote or deleted it."
        quiet = ("after phase 2: device 0 is quiet, but it still sends nodes again: "
                 "{\"01a1c2c0-39a0-7cdc-8389-c4663807ce68\"}, at {}, lost creates {}.")
        restores_and_rewinds = "devices_converge_across_restores_and_rewinds"
        server_restores = "devices_converge_across_server_restores"
        rewinds_and_rollbacks = "devices_converge_across_rewinds_and_rollbacks"
        a7b0 = "a7b0eabd2b85c630555e19125c650ce2d64fba476e043a43b9729f368183f7bf"
        c96d = "0c96dc79dd54604004d89c1bd0230b2e6865bd96dab99890aed5feedfb9088db"
        cd97 = "cd970b7805c3" + "0" * 52
        ae45 = "45aeea4283bad112a4dfb008e7c67a75c58dea15306a57cb165f8b6cbd33238c"
        e0a5 = "5e0a61643769" + "0" * 52
        d1f7 = "1d7f5cc9bb86920e85aebe47883949e98f8768e54f859425dbef30cbbb41919a"
        d93b = "d93bc61897d139c24ba2b9cf1da1cf8a696676689896335347e9f41fabe4c95e"
        other = "0" * 64
        folder = "devices_converge_around_a_folder_another_device_replaces"
        lose_rollbacks = "devices_lose_no_write_across_rewinds_and_rollbacks"
        lose_coarse = "devices_on_coarse_volumes_lose_no_write_across_restores_and_rewinds"
        x2293 = "2293e681b3bc09faeca001d034140fd95df720190105bcb9a754644310248df5"
        w74e3 = "74e37aa2bf801d15aab9bf0f584dddf33f51a77e14d48566c299c6ec6b7b18e4"
        lose_restores = "devices_lose_no_write_across_restores_and_rewinds"
        g3a0d = "3a0d2e9f93add5bd8adc686860704064f6142131917fe4203d878de80082695b"
        never_reached = next(entry for entry in entries
                             if entry["file"] == "GUN-864-I-delete-never-reached-server-across-server-restores.toml")["examples"][0]
        undone = next(entry for entry in entries if entry["file"] == "GUN-864-I-delete-came-undone.toml")["examples"][1]
        def example(case):
            return next(entry for entry in entries if case.startswith(tuple(entry.get("cases", ["-"]))))["examples"][0]
        update = "}, Op { intent: None, fields: None, op: Some(UpdateContent"
        three_ops = example(d1f7).replace(
            update, "}, Op { intent: None, fields: None, op: Some(Delete(Delete { node_id: \"x\" })) " + update, 1)
        written = example(d93b)
        not_displaced = written[:written.index(", replaces:")] + ", replaces: None }" + written[written.index(" removed write"):]
        for test, message, case, labels in [
            (restores_and_rewinds, three_ops, d1f7, []),
            (rewinds_and_rollbacks, not_displaced, d93b, []),
            (restores_and_rewinds, example(a7b0) + " Then write 7 is lost too.", a7b0, []),
            (restores_and_rewinds, left, a7b0, [("J", "GUN-1012", ["a7b0eabd2b85"])]),
            (restores_and_rewinds, left, c96d, [("J", "GUN-1012", ["0c96dc79dd54"])]),
            (restores_and_rewinds, left, cd97, []),
            (restores_and_rewinds, left, other, []),
            (server_restores, left, cd97, [("J", "GUN-1012", ["cd970b7805c3"])]),
            (server_restores, left, a7b0, []),
            (rewinds_and_rollbacks, quiet, other, []),
            (restores_and_rewinds, quiet, ae45, [("D", "GUN-984", None)]),
            (server_restores, quiet, ae45, [("D", "GUN-984", None)]),
            (server_restores, quiet, e0a5, [("J", "GUN-1012", ["5e0a61643769"])]),
            (folder, never_reached, other, []),
            (rewinds_and_rollbacks, never_reached, other, []),
            (server_restores, never_reached, other, [("I", "GUN-864", None)]),
            (restores_and_rewinds, never_reached, other, [("I", "GUN-864", None)]),
            ("devices_on_coarse_volumes_converge_across_restores_and_rewinds", never_reached, other, [("I", "GUN-864", None)]),
            (lose_rollbacks, example(x2293), x2293, [("X", "GUN-1199", ["2293e681b3bc"])]),
            (lose_rollbacks, example(x2293), other, []),
            (lose_coarse, example(w74e3), w74e3, [("W", "GUN-1200", ["74e37aa2bf80"])]),
            (lose_coarse, example(w74e3), other, []),
            (lose_rollbacks, example(w74e3), w74e3, [("A", "GUN-1004", None)]),
            ("devices_on_coarse_volumes_converge_once_the_faults_stop", undone + "\n" + example(w74e3), other, []),
            (lose_restores, example(g3a0d), g3a0d, [("G", "GUN-1203", ["3a0d2e9f93ad"])]),
            (lose_restores, example(g3a0d), other, []),
        ]:
            with self.subTest(test=test, message=message[:40], case=case[:12]):
                found = triage.matches(entries, test, message, case)
                self.assertEqual([(entry["class"], entry["issue"], entry.get("cases")) for entry in found], labels)

    def test_a_devices_replay_labels_its_case_like_the_proptest_failure_it_replaced(self):
        entries = triage.known_failures(triage.KNOWN_FAILURES)
        restores_and_rewinds = "devices::devices_converge_across_restores_and_rewinds"
        lose_restores = "devices::devices_lose_no_write_across_restores_and_rewinds"
        coarse = "devices::devices_on_coarse_volumes_converge_across_restores_and_rewinds"
        c7fa6 = "cc-7fa68fe1dd2cdc808c836467367c2bc474ff54db1bf9d6ba4f1658e491ffd5fe"
        d15d9 = "cc-15d9a15887573be1669890fa3fbe7c6788bbc2e8008c8c52f361254742b326fc"
        debc6 = "cc-ebc688b05bff4a95fd7afb5c83abebd59e77b8c3c230151a5fe5746dac44bafc"
        never_ran = " never ran case cc-1cc97e001edf41a424a2764d26cbbb32894c769662a63d0a71c8b844aea4096b"
        coarse_never_ran = [coarse + " never ran case cc-79e1ecbfd8d69fa3b00e4960fa7a3a1949d66654b791f86672e512bda85e8526",
                            coarse + " never ran case cc-a0e99500cef5dbe9fa8d6b87b6520da3f8bf411ee15b54c22ddcc61a77688464"]
        uncounted = ("after phase 2: recovery started with no rewind, no rolled-back store and no restore of commits it "
                     "lost: the server counts 1 commits of this device, which the store cannot account for")
        taken = ("after phase 2: a restore took write 5 from the server while a disk held it, and the server does not "
                 "hold it again, though no user overwrote or deleted it")
        copied = ("after phase 2: write 4 is in more than one file on the server, and no user copied it: "
                  "[[\"A\"], [\"A (conflict from device-1 2026-11-09)\"]]")
        for test, case, failure, after, label in [
            (restores_and_rewinds, c7fa6, uncounted, [restores_and_rewinds + never_ran], ("C", "GUN-984")),
            (lose_restores, c7fa6, uncounted, [lose_restores + never_ran], ("C", "GUN-984")),
            (lose_restores, d15d9, taken, [lose_restores + never_ran], ("D", "GUN-984")),
            (coarse, debc6, copied, [
                f"not recorded: {coarse}\t{debc6}\tDeleteUndone\tthe delete of node 01a1c2c0-39a0-7c5c-8466-a7d0954b76a3 "
                "on device 1 came undone, and no change its user did not see brought the node back", *coarse_never_ran],
             ("D", "GUN-984")),
        ]:
            name = test.removeprefix("devices::")
            replayed = triage.outcome(101, DEVICES_REPLAY.format(
                test=name, lines="\n".join([f"{test} case {case} fails: {failure}", *after])))
            proptest = triage.outcome(101, DEVICES_REPLAY.format(
                test=name, lines=f"Test failed: {failure}.\nminimal failing input: devices = 3, phases = []"))
            with self.subTest(test=name, case=case[:15]):
                self.assertEqual(replayed, proptest)
                self.assertEqual(replayed, {"result": "fail", "message": failure + "."})
                self.assertEqual([(entry["class"], entry["issue"])
                                  for entry in triage.matches(entries, name, replayed["message"], case[3:])], [label])

    def test_a_devices_replay_keeps_its_unrecorded_reds_and_drops_the_cases_triage_removed(self):
        entries = triage.known_failures(triage.KNOWN_FAILURES)
        test = "devices_converge_across_restores_and_rewinds"
        red = (f"not recorded: devices::{test}\tcc-085dc3892c1039055c5cb4b0523f824a775a089818ccdd9b62a5960caad5158f\t"
               "DeleteUndone\tthe delete of node 01a1c2c0-39a0-7510-83bc-38d6c6b89fe4 on device 1 came undone, and no "
               "change its user did not see brought the node back")
        never_ran = (f"devices::{test} never ran case "
                     "cc-1cc97e001edf41a424a2764d26cbbb32894c769662a63d0a71c8b844aea4096b")
        replayed = triage.outcome(101, DEVICES_REPLAY.format(test=test, lines=f"{red}\n{never_ran}"))
        self.assertEqual(replayed, {"result": "fail", "message": red})
        self.assertEqual([(entry["class"], entry["issue"]) for entry in triage.matches(entries, test, replayed["message"])],
                         [("I", "GUN-864")])
        passed = triage.outcome(101, DEVICES_REPLAY.format(test=test, lines=never_ran))
        self.assertEqual(passed, {"result": "pass", "message": None})
        self.assertEqual(triage.verdict([passed] * 2, [passed] * 2), "flaky or fixed")
        self.assertEqual(triage.outcome(101, DEVICES_REPLAY.format(test=test, lines="")),
                         {"result": "fail", "message": "exit code 101"})

    def test_no_registry_entry_labels_its_example_with_text_after_it(self):
        entries = triage.known_failures(triage.KNOWN_FAILURES)
        for entry in entries:
            for example in entry["examples"]:
                for test in entry["tests"]:
                    for case in entry.get("cases", [""]):
                        with self.subTest(file=entry["file"], test=test, case=case):
                            self.assertEqual(triage.matches(entries, test, example + " Then write 7 is lost too.", case), [])

    def test_every_registry_entry_labels_its_example_after_the_assertion_moves_to_another_file(self):
        entries = triage.known_failures(triage.KNOWN_FAILURES)
        for entry in entries:
            for example in entry["examples"]:
                moved = re.sub(r"[\w./-]*\.rs:", "core/sim/tests/moved/elsewhere.rs:", example)
                for test in entry["tests"]:
                    for case in entry.get("cases", [""]):
                        with self.subTest(file=entry["file"], test=test, case=case):
                            self.assertIn(entry, triage.matches(entries, test, moved, case))

    def test_every_registry_test_name_is_a_test_in_core(self):
        files = subprocess.run(["git", "ls-files", "-z", "core/*.rs"], cwd=ROOT, check=True,
                               stdout=subprocess.PIPE, text=True).stdout.split("\0")
        source = "".join((ROOT / path).read_text(errors="replace") for path in files if path)
        for entry in triage.known_failures(triage.KNOWN_FAILURES):
            for test in entry["tests"]:
                with self.subTest(file=entry["file"], test=test):
                    *modules, name = test.split("::")
                    self.assertRegex(source, rf"\bfn {re.escape(name)}\(")
                    for module in modules:
                        self.assertRegex(source, rf"\bmod {re.escape(module)}\b")

    def test_a_known_failure_that_cant_be_read_is_named_in_the_error(self):
        a, b = KNOWN["GUN-1-A-write-lost.toml"], KNOWN["GUN-2-B-no-convergence.toml"]
        broken = {
            "not toml": ("GUN-1-A-write-lost.toml", "[[failure]\n"),
            "an empty file": ("GUN-1-A-write-lost.toml", ""),
            "a [[failure]] table": ("GUN-1-A-write-lost.toml", "[[failure]]\n" + a),
            "a missing field": ("GUN-2-B-no-convergence.toml", b.replace('examples = ["no convergence within 400 steps"]\n', "")),
            "tests as a string": ("GUN-2-B-no-convergence.toml", b.replace('tests = ["devices_converge"]', 'tests = "devices_converge"')),
            "a regex that doesn't compile": ("GUN-1-A-write-lost.toml", a.replace("'''write <n> is lost'''", "'''write (<n> is lost'''")),
            "a message that isn't a string": ("GUN-1-A-write-lost.toml", a.replace("'''write <n> is lost'''", "1")),
            "a class that isn't a string": ("GUN-1-A-write-lost.toml", a.replace('class = "A"', "class = 1")),
            "cases as a string": ("GUN-1-A-write-lost.toml", a + 'cases = "0123456789ab"\n'),
            "a case that is too short": ("GUN-1-A-write-lost.toml", a + 'cases = ["0123"]\n'),
            "a case that isn't hex": ("GUN-1-A-write-lost.toml", a + 'cases = ["0123456789az"]\n'),
            "no cases": ("GUN-1-A-write-lost.toml", a + "cases = []\n"),
            "an unknown field": ("GUN-1-A-write-lost.toml", a + 'shards = ["1"]\n'),
            "a name without its issue": ("GUN-2-A-write-lost.toml", a),
            "a name without its class": ("GUN-1-write-lost.toml", a),
            "a name with another class": ("GUN-1-B-write-lost.toml", a),
            "a name without a slug": ("GUN-1-A.toml", a),
            "a slug that isn't lowercase and hyphenated": ("GUN-1-A-Write_Lost.toml", a),
            "a name that isn't .toml": ("GUN-1-A-write-lost.yaml", a),
        }
        for name, (file, text) in broken.items():
            with self.subTest(name), self.assertRaisesRegex(ValueError, re.escape(file)):
                known_failures(KNOWN | {file: text})
        with self.assertRaisesRegex(ValueError, "no known failures"):
            known_failures({})

    def test_known_failures_load_in_file_name_order(self):
        self.assertEqual([entry["file"] for entry in known_failures(KNOWN)], sorted(KNOWN))

    def test_the_registry_is_one_file_per_known_failure(self):
        single_file = ROOT / ".github/battery-known-failures.toml"
        self.assertFalse(single_file.exists(),
                         ".github/battery-known-failures.toml is back, most likely from a rebase. Known failures live "
                         "one per file: move each [[failure]] table in it to its own "
                         ".github/battery-known-failures/<issue>-<class>-<slug>.toml without the [[failure]] line, then "
                         "delete .github/battery-known-failures.toml.")

    def test_a_registry_that_cant_be_read_still_writes_the_report(self):
        run, rows, text = self.report({"GUN-1-A-write-lost.toml": "[[failure]\n"}, [
            ("devices_converge", ("fail", "write 1 is lost"), ("fail", "write 1 is lost")),
        ])
        self.assertNotEqual(run.returncode, 0)
        self.assertIn("::error::No row has a class: ", run.stdout)
        self.assertIn("known can't be read: GUN-1-A-write-lost.toml is not TOML", run.stdout)
        self.assertEqual((rows[0]["verdict"], rows[0]["class"], rows[0]["issue"]), (triage.SAME, None, None))
        self.assertIn("No row has a class: ", text)
        self.assertIn(f"| {triage.SAME} |  |  |", text)



class Triage(unittest.TestCase):
    def shard(self, root, number, cases, extra=()):
        shard = root / f"failure-shard-{number}"
        (shard / "logs").mkdir(parents=True)
        directory = shard / "regressions/core/sim/tests/devices.cases"
        directory.mkdir(parents=True)
        for case in (*cases, *extra):
            (directory / f"cc-{case}.txt").write_text(f"cc {case} # shrinks to devices = 2\n")
        (shard / "logs/devices-09cbc274e49e4fd7.log").write_text(
            "".join(FAILING_LOG.format(case=case) for case in cases))

    def test_collect_maps_each_case_to_its_test_and_merges_shards(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.shard(root, 4, ["a" * 64])
            self.shard(root, 10, ["a" * 64, "b" * 64], extra=["c" * 64])
            (root / "failure-shard-11.zip").write_text("")
            cases, others = triage.collect(root)
        self.assertEqual([(case["case"][0], case["test"], case["shards"], case["shared"]) for case in cases],
                         [("a", "devices_converge", [4, 10], False),
                          ("a", "devices_lose_no_write", [4, 10], True),
                          ("b", "devices_converge", [10], False),
                          ("b", "devices_lose_no_write", [10], True),
                          ("c", None, [10], False)])
        self.assertEqual(cases[0]["path"], f"core/sim/tests/devices.cases/cc-{'a' * 64}.txt")
        self.assertEqual(cases[0]["text"], f"cc {'a' * 64} # shrinks to devices = 2\n")
        self.assertEqual(cases[0]["battery_message"],
                         "assertion failed: §(left == right)§\n  left: §1§,\n"
                         " right: §2§: write 1 is lost at core/sim/tests/devices/common.rs:120.".replace("§", chr(96)))
        self.assertEqual(cases[1]["battery_message"], "a committed case fails.")
        self.assertEqual(others, [{"binary": "devices", "test": "devices_lose_no_write", "shards": [4, 10],
                                   "cases": [cases[0]["path"], cases[2]["path"]]}])

    def test_collect_reads_a_single_artifact_extracted_without_its_directory(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.shard(root, 3, ["a" * 64, "b" * 64])
            (root / "failure-shard-3/result-3.json").write_text(json.dumps({"shard": 3}))
            nested = triage.collect(root)
            flat = triage.collect(root / "failure-shard-3")
        self.assertEqual(flat, nested)
        self.assertEqual([(case["case"][0], case["test"], case["shards"]) for case in flat[0]],
                         [("a", "devices_converge", [3]), ("a", "devices_lose_no_write", [3]),
                          ("b", "devices_converge", [3]), ("b", "devices_lose_no_write", [3])])

    def test_collect_refuses_a_download_it_cannot_read_as_shards(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with self.assertRaisesRegex(ValueError, "holds no failure-shard artifact"):
                triage.collect(root)
            self.shard(root, 5, ["a" * 64])
            with self.assertRaisesRegex(ValueError, "0 result-\\*.json files name its shard"):
                triage.collect(root / "failure-shard-5")

    def test_a_target_whose_log_names_no_failing_test_is_unattributed_in_both_layouts(self):
        crashed = {"target": "koidrive-engine --test recovery", "log": "recovery-1111111111111111",
                   "reason": "exit code 134", "repro": "cargo test -p koidrive-engine"}
        named = {"target": "koidrive-sim --test recovery", "log": "recovery-2222222222222222",
                 "reason": "exit code 101", "repro": "cargo test -p koidrive-sim"}
        unlabelled_bin = {"target": "koidrive-cli --bin koi", "reason": "exit code 101", "repro": "cargo test"}
        mapped = {"target": "koidrive-sim --test devices", "log": "devices-09cbc274e49e4fd7",
                  "reason": "exit code 101", "repro": "cargo test"}
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.shard(root, 2, ["a" * 64])
            shard = root / "failure-shard-2"
            (shard / "logs/recovery-1111111111111111.log").write_text(
                "running 1 test\n\nthread 'main' has overflowed its stack\nfatal runtime error: stack overflow\n")
            (shard / "logs/recovery-2222222222222222.log").write_text(
                "failures:\n\n---- restore_keeps_writes stdout ----\npanicked\n\nfailures:\n")
            (shard / "logs/koi-3333333333333333.log").write_text(
                "failures:\n\n---- prints_help stdout ----\npanicked\n\nfailures:\n")
            (shard / "result-2.json").write_text(
                json.dumps({"shard": 2, "failing": [mapped, crashed, named, unlabelled_bin]}))
            for failures in (root, shard):
                with self.subTest(failures=failures.name):
                    self.assertEqual(triage.unattributed(failures),
                                     [{"shard": 2, **{key: crashed[key] for key in ("target", "reason", "repro")}}])
            self.shard(root, 6, [])
            self.assertEqual(triage.unattributed(root)[1:],
                             [{"shard": 6, "target": None, "reason": "the artifact has no result-6.json", "repro": ""}])

    def test_the_report_lists_unattributed_targets_and_fails(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            spec = {"run": "7", "head": "h" * 40, "base": "b" * 40, "checked_profile": "", "cases": [], "others": [],
                    "unattributed": [{"shard": 2, "target": "koidrive-sim --test engine_sync",
                                      "reason": "exit code 134", "repro": "cargo test -p koidrive-sim"}]}
            (root / "cases.json").write_text(json.dumps(spec))
            run = subprocess.run([sys.executable, str(ROOT / "scripts/triage-battery.py"), "report",
                                  "--cases", str(root / "cases.json"), "--run-url", "https://example.com/runs/7",
                                  "--out", str(root / "out")], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
            text = (root / "out/triage.md").read_text()
        self.assertNotEqual(run.returncode, 0)
        self.assertIn("::error::shard 2: koidrive-sim --test engine_sync failed (exit code 134)", run.stdout)
        self.assertIn("- shard 2: koidrive-sim --test engine_sync, exit code 134: `cargo test -p koidrive-sim`", text)

    def test_a_test_that_saved_nothing_gets_rows_for_its_shards_cases_that_fail_it(self):
        path = "core/sim/tests/devices.cases/cc-" + "a" * 64 + ".txt"
        spec = {"run": "7", "head": "h" * 40, "base": "b" * 40,
                "cases": [{"case": "a" * 64, "path": path, "binary": "devices", "test": test, "shards": [2],
                           "shared": shared} for test, shared in (("converge", False), ("lose_no_write", True))],
                "others": [{"binary": "devices", "test": "lose_no_write", "shards": [2], "cases": [path]}]}
        def side(result, message):
            return {"runs": [{"result": result, "message": message}] * 2, "target": "sim --test devices"}
        flaky = {"runs": [{"result": "fail", "message": "lost"}, {"result": "pass", "message": None}],
                 "target": "sim --test devices"}
        for head, kept, listed in ((side("fail", "lost"), ["converge", "lose_no_write"], []),
                                   (flaky, ["converge", "lose_no_write"], []),
                                   (side("pass", None), ["converge"], ["lose_no_write"])):
            with self.subTest(head=[run["result"] for run in head["runs"]]):
                replays = {"head": {"results": [side("fail", "lost"), head]},
                           "base": {"results": [side("fail", "lost"), side("fail", "lost")]}}
                rows = triage.combine(spec, replays)
                others = triage.caseless(spec, rows)
                self.assertEqual(sorted(row["test"] for row in rows), kept)
                if head is flaky:
                    self.assertEqual(next(row["verdict"] for row in rows if row["shared"]), "unstable")
                self.assertEqual([other["test"] for other in others], listed)
                triage.classify(rows, [])
                text = triage.markdown(spec, rows, others, "https://example.com/runs/7")
                self.assertEqual("(saved by another test)" in text, bool(listed) is False)
                self.assertEqual("- devices §lose_no_write§ on shards 2".replace("§", chr(96)) in text, bool(listed))

    def test_failure_message_falls_back_to_the_panic(self):
        self.assertEqual(triage.failure_message("thread 'x' panicked at src/lib.rs:3:5:\nboom\nnote: run with"), "boom")
        self.assertIsNone(triage.failure_message("test x ... ok\n"))

    def test_outcome_reads_the_harness_result(self):
        self.assertEqual(triage.outcome(0, "test result: ok. 1 passed; 0 failed; 0 ignored")["result"], "pass")
        self.assertEqual(triage.outcome(0, "test result: ok. 0 passed; 0 failed; 1 ignored; 0 measured")["result"],
                         "ignored")
        self.assertEqual(triage.outcome(101, FAILING_LOG.format(case="a"))["message"].splitlines()[0],
                         "assertion failed: §(left == right)§".replace("§", chr(96)))
        self.assertEqual(triage.outcome(-6, ""), {"result": "fail", "message": "killed by signal 6"})

    def test_a_stack_overflow_and_an_abort_are_different_failures(self):
        running = "running 1 test\ntest devices_converge ... "
        overflow = triage.outcome(-6, running + "\nthread 'devices_converge' (2963) has overflowed its stack\n"
                                       "fatal runtime error: stack overflow, aborting\n")
        abort = triage.outcome(-6, running)
        null_write = triage.outcome(-11, running)
        panic = {"result": "fail", "message": "lost"}
        self.assertEqual(overflow, {"result": "fail",
                                    "message": "killed by signal 6 after: fatal runtime error: stack overflow, aborting"})
        self.assertEqual(abort, {"result": "fail", "message": "killed by signal 6"})
        self.assertEqual(triage.outcome(-6, running + "free(): invalid pointer\n"),
                         {"result": "fail", "message": "killed by signal 6 after: free(): invalid pointer"})
        self.assertEqual(triage.verdict([overflow] * 2, [overflow] * 2), triage.SAME)
        for head, base in ((abort, abort), (null_write, null_write), (abort, null_write), (abort, overflow),
                           (overflow, abort), (panic, abort), (abort, panic)):
            with self.subTest(head=head["message"], base=base["message"]):
                self.assertEqual(triage.verdict([head] * 2, [base] * 2), triage.SILENT)
        self.assertEqual(triage.verdict([abort] * 2, [{"result": "pass", "message": None}] * 2), "PR-caused")

    def test_an_outcome_without_a_harness_result_is_an_error(self):
        for returncode, output in ((101, "error: no such test"), (127, ""), (0, ""), (0, "running 0 tests\n"),
                                   (0, "test result: ok. 0 passed; 0 failed; 0 ignored; 1 filtered out")):
            with self.subTest(returncode=returncode, output=output):
                result = triage.outcome(returncode, output)
                self.assertEqual(result["result"], "error")
                self.assertEqual(triage.verdict([{"result": "fail", "message": "lost"}] * 2, [result] * 2), "unknown")
                self.assertEqual(triage.verdict([result] * 2, [{"result": "fail", "message": "lost"}] * 2), "unknown")

    def test_only_a_successful_listing_can_report_a_missing_test(self):
        listings = (("listing exits 1", "echo 'devices_converge: test'; exit 1", "error"),
                    ("listing killed", "kill -9 $$", "error"),
                    ("listing without the test", "echo 'other: test'", "missing"),
                    ("empty listing", "true", "missing"),
                    ("listing with the test", "echo 'devices_converge: test'", "pass"))
        for name, listing, expected in listings:
            with self.subTest(name), tempfile.TemporaryDirectory() as directory:
                binary = Path(directory) / "devices"
                binary.write_text("#!/bin/sh\nif [ \"$3\" = --list ]; then " + listing + "; exit; fi\n"
                                  "echo 'test result: ok. 1 passed; 0 failed;'\n")
                binary.chmod(0o755)
                with contextlib.redirect_stdout(io.StringIO()):
                    result = triage.replay_one(str(binary), Path(directory), "devices_converge", 10)
                self.assertEqual(result["result"], expected)
                if expected == "error":
                    self.assertEqual(triage.verdict([{"result": "fail", "message": "lost"}] * 2, [result] * 2),
                                     "unknown")

    def test_verdicts(self):
        def runs(result, message=None, times=2):
            return [{"result": result, "message": message}] * times
        rules = [
            (runs("fail", "lost at a.rs:12:3"), runs("pass"), "PR-caused"),
            (runs("fail", "lost at a.rs:12:3"), runs("missing", times=1), "PR-caused"),
            (runs("fail", "lost at a.rs:12:3"), runs("ignored"), "PR-caused"),
            (runs("fail", "lost at a.rs:12:3"), runs("fail", "lost twice at a.rs:12:3"), "PR-caused"),
            (runs("fail", "lost at a.rs:12:3"), runs("fail", "lost at a.rs:12:3"), triage.SAME),
            (runs("fail", "lost at a.rs:12"), runs("fail", "lost at b.rs:12"), triage.MOVED),
            (runs("fail", "node 01a1-1 lost"), runs("fail", "node 01a1-2 lost"), "PR-caused"),
            (runs("pass"), runs("fail", "lost"), "flaky or fixed"),
            (runs("pass"), runs("pass"), "flaky or fixed"),
            (runs("fail", "lost"), runs("timeout", "no result"), "unknown"),
            (runs("fail", "lost"), runs("error", "build failed", times=1), "unknown"),
            (runs("timeout", "no result"), runs("fail", "lost"), "unknown"),
            (runs("unmapped", "no log", times=1), runs("unmapped", "no log", times=1), "unknown"),
            (runs("pass")[:1] + runs("fail", "lost")[:1], runs("fail", "lost"), "unstable"),
            (runs("fail", "lost at a.rs:1")[:1] + runs("fail", "lost at a.rs:2")[:1], runs("fail", "lost"), "unstable"),
        ]
        for head, base, expected in rules:
            with self.subTest(head=head, base=base):
                self.assertEqual(triage.verdict(head, base), expected)

    def test_a_moved_assertion_is_never_counted_as_the_same_failure(self):
        head = [{"result": "fail", "message": "write is lost at invariant.rs:220:1"}] * 2
        base = [{"result": "fail", "message": "write is lost at invariant.rs:120:1"}] * 2
        self.assertEqual(triage.verdict(head, base), triage.MOVED)
        self.assertNotEqual(triage.MOVED, triage.SAME)

    def test_the_same_message_from_another_file_is_moved_not_pr_caused(self):
        head = [{"result": "fail", "message": "write 1 is lost at core/sim/tests/engine_sync/checks.rs:40."}] * 2
        base = [{"result": "fail", "message": "write 1 is lost at core/sim/tests/engine_sync/common.rs:1869."}] * 2
        self.assertEqual(triage.verdict(head, base), triage.MOVED)

    def test_a_different_message_from_another_file_is_still_pr_caused(self):
        head = [{"result": "fail", "message": "write 2 is lost at core/sim/tests/engine_sync/checks.rs:40."}] * 2
        base = [{"result": "fail", "message": "write 1 is lost at core/sim/tests/engine_sync/common.rs:1869."}] * 2
        self.assertEqual(triage.verdict(head, base), "PR-caused")

    def test_a_flaky_base_is_unstable_instead_of_either_verdict(self):
        head = [{"result": "fail", "message": "write is lost at invariant.rs:220:1"}] * 2
        for base in ([{"result": "fail", "message": "write is lost at invariant.rs:220:1"}, {"result": "pass", "message": None}],
                     [{"result": "pass", "message": None}, {"result": "fail", "message": "write is lost at invariant.rs:220:1"}]):
            with self.subTest(base=base):
                self.assertEqual(triage.verdict(head, base), "unstable")

    def test_report_orders_by_verdict_and_shows_each_run(self):
        spec = {"run": "7", "head": "h" * 40, "base": "b" * 40, "others": [],
                "cases": [{"case": c * 64, "path": f"t.cases/cc-{c * 64}.txt", "binary": "devices", "test": "t",
                           "shards": [1], "shared": False} for c in "abc"]}
        def side(*runs):
            return {"runs": [{"result": result, "message": message} for result, message in runs],
                    "target": "sim --test devices"}
        replays = {"head": {"results": [side(("fail", "x at a.rs:1"), ("fail", "x at a.rs:1")),
                                        side(("fail", "y | z"), ("fail", "y | z")),
                                        side(("fail", "w"), ("fail", "w"))]},
                   "base": {"results": [side(("fail", "x at a.rs:1"), ("fail", "x at a.rs:1")),
                                        side(("pass", None), ("pass", None)),
                                        side(("fail", "w"), ("pass", None))]}}
        rows = triage.combine(spec, replays)
        self.assertEqual([(row["case"][0], row["verdict"]) for row in rows],
                         [("b", "PR-caused"), ("c", "unstable"), ("a", triage.SAME)])
        triage.classify(rows, [])
        text = triage.markdown(spec, rows, [], "https://example.com/runs/7")
        self.assertIn(f"1 PR-caused, 1 unstable, 0 unknown, 0 {triage.SILENT}, 0 {triage.MOVED}, 0 flaky or fixed, "
                      f"1 {triage.SAME}", text)
        self.assertIn("with nothing normalised", text)
        self.assertIn("| fail / pass | w | run 1: w / run 2: (none) | unstable |", text)
        self.assertIn("y \\| z", text)
        rows = triage.combine(spec, {"head": replays["head"]})
        self.assertEqual({row["base"][0]["result"] for row in rows}, {"error"})

    def test_isolate_leaves_only_the_case_under_test(self):
        with tempfile.TemporaryDirectory() as directory:
            checkout = Path(directory)
            tests = checkout / "core/sim/tests"
            (tests / "devices.cases").mkdir(parents=True)
            (tests / "devices.cases/cc-old.txt").write_text("cc old\n")
            (tests / "devices.proptest-regressions").write_text("# Seeds\ncc legacy # shrinks\n")
            (tests / "devices.rs").write_text("// SPDX-License-Identifier: Apache-2.0\n")
            (tests / "engine_sync.cases").mkdir()
            (tests / "engine_sync.cases/cc-other.txt").write_text("cc other\n")
            triage.isolate(checkout, {"path": "core/sim/tests/devices.cases/cc-new.txt", "text": "cc new\n"})
            self.assertEqual(sorted(path.relative_to(tests).as_posix() for path in tests.rglob("*") if path.is_file()),
                             ["devices.cases/cc-new.txt", "devices.rs", "engine_sync.cases/cc-other.txt"])

    def test_resolve_picks_the_innermost_package_and_its_target(self):
        checkout = Path("/w")
        metadata = {"packages": [
            {"name": "koidrive-sim", "manifest_path": "/w/core/sim/Cargo.toml",
             "targets": [{"name": "devices", "kind": ["test"]}, {"name": "koidrive_sim", "kind": ["lib"]}]},
            {"name": "koidrive-driver", "manifest_path": "/w/core/driver/Cargo.toml",
             "targets": [{"name": "koidrive_driver", "kind": ["lib"]}]},
            {"name": "koidrive", "manifest_path": "/w/Cargo.toml", "targets": [{"name": "devices", "kind": ["test"]}]},
        ]}
        def case(path, binary):
            return {"path": path, "binary": binary}
        self.assertEqual(triage.resolve(metadata, checkout, case("core/sim/tests/devices.cases/cc-a.txt", "devices")),
                         ("koidrive-sim", ["--test", "devices"], "core/sim"))
        self.assertEqual(triage.resolve(metadata, checkout,
                                        case("core/driver/proptest-regressions/engine.cases/cc-a.txt", "koidrive_driver")),
                         ("koidrive-driver", ["--lib"], "core/driver"))
        self.assertIsNone(triage.resolve(metadata, checkout, case("core/sim/tests/devices.cases/cc-a.txt", "gone")))

    def test_replay_clears_inherited_budgets(self):
        names = ("PROPTEST_CASES", "PROPTEST_RNG_SEED", "KOIDRIVE_BATTERY_CASES", "KOIDRIVE_PROPTEST_DETERMINISTIC")
        saved = {name: os.environ.get(name) for name in names}
        os.environ.update({name: "5" for name in names})
        try:
            environment = triage.replay_environment(Path("/w/core/sim"))
        finally:
            for name, value in saved.items():
                if value is None:
                    os.environ.pop(name)
                else:
                    os.environ[name] = value
        self.assertEqual({name: environment.get(name) for name in names},
                         {"PROPTEST_CASES": "0", "PROPTEST_RNG_SEED": None, "KOIDRIVE_BATTERY_CASES": None,
                          "KOIDRIVE_PROPTEST_DETERMINISTIC": None})

    def test_a_replay_build_must_pass_the_runtime_checks_and_their_probe(self):
        def unit(name, kind, overflow_checks=True, executable=None):
            return {"reason": "compiler-artifact", "package_id": name, "target": {"name": name, "kind": [kind]},
                    "profile": {"debug_assertions": True, "overflow_checks": overflow_checks, "test": True},
                    "executable": executable}
        cases = (("checked", True, True, 0, 0, None),
                 ("weakened unit", False, True, 0, 0, "require-runtime-checks.sh"),
                 ("failing probe", True, True, 1, 0, "require-runtime-checks.sh"),
                 ("no probe binary", True, False, 0, 0, "require-runtime-checks.sh"),
                 ("checkout's probe passes but the triage's fails", True, True, 0, 1, "triage_runtime_probe::"),
                 ("triage probe prints ok but exits nonzero", True, True, 0, 2, "triage_runtime_probe::"))
        for name, overflow_checks, probe_built, probe_exit, triage_exit, problem_names in cases:
            with self.subTest(name), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                probe = root / "probe"
                probe.write_text("#!/bin/sh\ncase \"$2\" in triage_runtime_probe::*) failed=" + str(triage_exit)
                                 + " ;; *) failed=" + str(probe_exit) + " ;; esac\n"
                                 "if [ $failed = 0 ] || [ $failed = 2 ]; then echo 'test result: ok. 1 passed;'; "
                                 "else echo 'test result: FAILED. 0 passed; 1 failed;'; fi\nexit $failed\n")
                probe.chmod(0o755)
                (root / "target.json").write_text(json.dumps(unit("koidrive_engine", "lib", overflow_checks)) + "\n"
                                                  + json.dumps(unit("devices", "test", executable="/bin/devices")) + "\n")
                (root / "probe.json").write_text(json.dumps(unit("koidrive_test_support", "lib",
                                                                 executable=str(probe) if probe_built else None)) + "\n")
                cargo = root / "cargo"
                cargo.write_text("#!/bin/sh\ncase \"$*\" in *koidrive-test-support*) cat " + str(root / "probe.json")
                                 + " ;; *) cat " + str(root / "target.json") + " ;; esac\n")
                cargo.chmod(0o755)
                saved = os.environ["PATH"]
                os.environ["PATH"] = str(root) + os.pathsep + saved
                try:
                    with contextlib.redirect_stdout(io.StringIO()):
                        executable, problem = triage.build(root, "koidrive-sim", ["--test", "devices"])
                finally:
                    os.environ["PATH"] = saved
                if problem_names is None:
                    self.assertEqual((executable, problem), ("/bin/devices", None))
                    continue
                self.assertIsNone(executable)
                self.assertIn(problem_names, problem)
                self.assertEqual(triage.verdict([{"result": "fail", "message": "lost"}] * 2,
                                                [{"result": "error", "message": problem}]), "unknown")

    def test_every_checkout_gets_the_triage_probe_and_the_named_probe_when_missing(self):
        source = (ROOT / "core/test-support/src/lib.rs").read_text()
        probe = re.search(r"(?ms)^    #\[test\]\n    fn the_build_keeps_debug_assertions_and_overflow_checks\(\).*?^    \}\n", source)[0]
        own = "    #[test]\n    fn the_build_keeps_debug_assertions_and_overflow_checks() {}\n"
        triage_module = "\n\n#[cfg(test)]\nmod triage_runtime_probe {\n" + probe + "}\n"
        for name, before, after, missing in (
                ("existing tests module", "pub fn a() {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn b() {}\n}\n",
                 "pub fn a() {}\n\n#[cfg(test)]\nmod tests {\n" + probe + "\n    #[test]\n    fn b() {}\n}" + triage_module, True),
                ("no tests module", "pub fn a() {}\n",
                 "pub fn a() {}\n\n#[cfg(test)]\nmod tests {\n" + probe + "}" + triage_module, True),
                ("a weakened probe of its own", "#[cfg(test)]\nmod tests {\n" + own + "}\n",
                 "#[cfg(test)]\nmod tests {\n" + own + "}" + triage_module, False)):
            with self.subTest(name), tempfile.TemporaryDirectory() as directory:
                checkout = Path(directory)
                lib = checkout / "core/test-support/src/lib.rs"
                lib.parent.mkdir(parents=True)
                lib.write_text(before)
                self.assertEqual(triage.add_probes(checkout, source), missing)
                self.assertEqual(lib.read_text(), after)

    def test_checked_profile_is_copied_whole(self):
        manifest = (ROOT / "Cargo.toml").read_text()
        profile = triage.checked_profile(manifest)
        self.assertTrue(profile.startswith("[profile.checked]\n"))
        self.assertIn(profile, manifest)
        for setting in ("debug-assertions = true", "overflow-checks = true"):
            self.assertIn(setting, profile)
        self.assertIsNone(triage.checked_profile(manifest.replace("[profile.checked]", "[profile.other]")))

    def test_triage_never_feeds_the_battery_verdict(self):
        jobs = re.split(r"(?m)^  (?=[a-z-]+:\n)", WORKFLOW.read_text().split("\njobs:\n", 1)[1])
        jobs = {job.split(":", 1)[0]: job for job in jobs if job}
        self.assertTrue({"triage", "triage-replay", "triage-report"} <= set(jobs))
        needs = re.search(r"(?m)^    needs: \[([^]]+)\]$", jobs["summary"])
        self.assertEqual({name.strip() for name in needs[1].split(",")}, {"build", "shard", "mutations"})
        for name in ("build", "shard", "mutations", "summary"):
            self.assertNotIn("triage-", jobs[name].replace("!inputs.triage_run", ""))


if __name__ == "__main__":
    unittest.main()
