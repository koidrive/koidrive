#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

from pathlib import Path
import contextlib
import importlib.util
import io
import json
import os
import re
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parent.parent
WORKFLOW = ROOT / ".github/workflows/property-battery.yml"
HEAVY_WORKFLOW = ROOT / ".github/workflows/heavy-tests.yml"
CI_WORKFLOW = ROOT / ".github/workflows/ci.yml"
RUNTIME_CHECKS = ROOT / "scripts/require-runtime-checks.sh"


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
                 "unit tests run once per shard", "saved regressions also replay"):
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
        self.assertIn("Bounded properties keep their own case budgets; unit tests run once per shard", output)
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
            setup = 'dir=tests; binary=bin/probe; CASES=2; seed=17; SHA=abc; package=koidrive-sim; selector="--test engine_sync"; filter_args=("a filter"); repro_filter=" -- a\\ filter"; log="$GITHUB_WORKSPACE/log"\n'
            for command in (execution, repro + '\neval "$repro"'):
                output = subprocess.check_output(["bash", "-euo", "pipefail", "-c", setup + command],
                                                 cwd=root, env=environment, text=True)
                self.assertIn("ordinary=unset stress=2 seed=17", output)
                self.assertIn("arg=a filter", output)
            self.assertIn("arg=--test\narg=engine_sync", output)


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
        self.assertIn('scripts/require-runtime-checks.sh "$RUNNER_TEMP/build.json" "$RUNNER_TEMP/probe.json"', ci)
        self.assertIn('cargo test --locked --profile "$PROFILE" -p koidrive-test-support --lib --no-run', ci)

    def test_battery_builds_and_reproduces_with_the_checked_profile(self):
        workflow = WORKFLOW.read_text()
        _, repro = commands(workflow)
        build = next(line for line in workflow.splitlines() if "--no-run --tests" in line)
        for command in (build, repro):
            self.assertIn("cargo test --locked --profile checked ", command)

    def test_ci_selects_only_dev_or_checked(self):
        for workflow in (WORKFLOW.read_text(), CI_WORKFLOW.read_text()):
            for profile in re.findall(r"--profile (\S+)", workflow):
                self.assertIn(profile, ("checked", '"$PROFILE"'))
        expressions = re.findall(r"(?m)^ +PROFILE: (.+)$", CI_WORKFLOW.read_text())
        self.assertTrue(expressions)
        for expression in expressions:
            self.assertEqual(set(re.findall(r"'([^']*)'", expression)) - {"sim"}, {"checked", "dev"})


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
            probe = '#!/bin/sh\nprintf "ordinary=%s stress=%s seed=%s\\n" "${PROPTEST_CASES-unset}" "${KOIDRIVE_BATTERY_CASES-unset}" "$PROPTEST_RNG_SEED"\nprintf "arg=%s\\n" "$@"\n'
            for name, content in (("cargo", probe), ("git", "#!/bin/sh\nexit 0\n")):
                path = root / name
                path.write_text(content)
                path.chmod(0o755)
            environment = dict(os.environ, PROPTEST_CASES="8192", KOIDRIVE_BATTERY_CASES="999",
                               PATH=str(root) + os.pathsep + os.environ["PATH"])
            for cases, expected in (("", "unset"), ("2", "2"), ("0", "0")):
                setup = f'CASES="{cases}"; SHA=abc; seed=17; cargo_args=(--locked -p koidrive-sim); test_args=("a filter"); log="{root}/log"; repro_args=" --locked -p koidrive-sim -- a\\ filter"\n'
                for command in (budget + "\n" + execution,
                                repro + '\neval "${repro/<seed>/$seed}"'):
                    output = subprocess.check_output(["bash", "-euo", "pipefail", "-c", setup + command],
                                                     env=environment, text=True)
                    self.assertIn(f"ordinary=unset stress={expected} seed=17", output)
                    self.assertIn("arg=a filter", output)
                    self.assertIn("arg=koidrive-sim", output)


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
        self.assertEqual(overflow, {"result": "fail",
                                    "message": "killed by signal 6 after: fatal runtime error: stack overflow, aborting"})
        self.assertEqual(abort, {"result": "fail", "message": "killed by signal 6 after: test devices_converge ..."})
        self.assertEqual(triage.verdict([abort] * 2, [overflow] * 2), "PR-caused")
        self.assertEqual(triage.verdict([overflow] * 2, [overflow] * 2), triage.SAME)

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
            (runs("fail", "lost at a.rs:12"), runs("fail", "lost at b.rs:12"), "PR-caused"),
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
        text = triage.markdown(spec, rows, [], "https://example.com/runs/7")
        self.assertIn(f"1 PR-caused, 1 unstable, 0 unknown, 0 {triage.MOVED}, 0 flaky or fixed, 1 {triage.SAME}", text)
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
