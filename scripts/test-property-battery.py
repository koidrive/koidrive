#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

from pathlib import Path
import os
import re
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parent.parent
WORKFLOW = ROOT / ".github/workflows/property-battery.yml"
HEAVY_WORKFLOW = ROOT / ".github/workflows/heavy-tests.yml"
CI_WORKFLOW = ROOT / ".github/workflows/ci.yml"
MANIFEST = ROOT / "Cargo.toml"
CARGO_CONFIGS = (ROOT / ".cargo/config.toml", ROOT / ".cargo/config")
RUNTIME_CHECK = re.compile(r"debug-assertions|overflow-checks|\bpanic\b")
RUSTC_OVERRIDE = re.compile(r"CARGO_PROFILE_|debug-assertions=(?:off|no|n|false)|overflow-checks=(?:off|no|n|false)|-C\s*panic=")


def sections(manifest):
    header = ""
    for line in manifest.splitlines():
        stripped = line.split("#", 1)[0].strip()
        if re.fullmatch(r"\[\[?[^\]]+\]\]?", stripped):
            header = stripped.strip("[]").strip()
        elif stripped:
            yield header, stripped


def check_checked_profile(manifest):
    top = {}
    for header, line in sections(manifest):
        key, _, value = (part.strip() for part in line.partition("="))
        if header == "profile.checked":
            top[key] = value
        if header.startswith(("profile.release", "profile.bench")) or not RUNTIME_CHECK.search(line):
            continue
        if header == "profile.checked" and key in ("debug-assertions", "overflow-checks") and value == "true":
            continue
        raise ValueError(f"[{header}] sets {line!r}: only [profile.checked] may set debug-assertions or overflow-checks, only to true, and nothing that the checked profile inherits may change them or panic")
    required = {"inherits": '"dev"', "debug-assertions": "true", "overflow-checks": "true"}
    if any(top.get(key) != value for key, value in required.items()):
        raise ValueError(f"[profile.checked] must inherit dev and set debug-assertions and overflow-checks to true, got {top}")


def check_no_profile_overrides(configs, workflows):
    for text in configs:
        if re.search(r"(?m)^\s*\[\s*profile|^\s*profile\s*[.=]", text) or RUSTC_OVERRIDE.search(text):
            raise ValueError("Cargo config must not override profiles or runtime checks")
    for text in workflows:
        if RUSTC_OVERRIDE.search(text):
            raise ValueError("Workflows must not override profiles or runtime checks")


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


class CheckedProfile(unittest.TestCase):
    def test_checked_profile_keeps_every_runtime_check(self):
        check_checked_profile(MANIFEST.read_text())

    def test_weakened_profile_is_rejected(self):
        manifest = MANIFEST.read_text()
        for original, weakened in (("debug-assertions = true", "debug-assertions = false"),
                                   ("overflow-checks = true", "overflow-checks = false"),
                                   ("overflow-checks = true", ""),
                                   ('inherits = "dev"', 'inherits = "release"'),
                                   ("overflow-checks = true", 'overflow-checks = true\npanic = "abort"'),
                                   ("overflow-checks = true", "overflow-checks = true\npackage.koidrive-engine.debug-assertions = false"),
                                   ("overflow-checks = true", 'overflow-checks = true\npackage = { "*" = { overflow-checks = false } }')):
            with self.subTest(weakened=weakened), self.assertRaises(ValueError):
                check_checked_profile(manifest.replace(original, weakened, 1))
        for table in ("profile.checked.package.koidrive-engine", "profile.checked.package.\"*\"",
                      "profile.dev.package.koidrive-sim", "profile.dev", "profile.checked.build-override",
                      "profile"):
            for setting in ("debug-assertions = false", "overflow-checks = false", 'panic = "abort"'):
                key = setting if table != "profile" else "checked.package.koidrive-engine." + setting
                with self.subTest(table=table, setting=setting), self.assertRaises(ValueError):
                    check_checked_profile(manifest + f"\n[{table}]\n{key}\n")

    def test_release_profiles_may_turn_checks_off(self):
        check_checked_profile(MANIFEST.read_text() + '\n[profile.release]\ndebug-assertions = false\npanic = "abort"\n')

    def test_no_profile_override_outside_cargo_toml(self):
        check_no_profile_overrides([path.read_text() for path in CARGO_CONFIGS if path.exists()],
                                   [path.read_text() for path in (ROOT / ".github/workflows").glob("*.yml")])
        for config in ("[profile.checked]\noverflow-checks = false\n", "profile.dev.debug-assertions = false\n",
                       '[build]\nrustflags = ["-C", "overflow-checks=off"]\n'):
            with self.subTest(config=config), self.assertRaises(ValueError):
                check_no_profile_overrides([config], [])
        for workflow in ("env:\n  CARGO_PROFILE_CHECKED_OVERFLOW_CHECKS: 'false'\n",
                         "env:\n  RUSTFLAGS: -C debug-assertions=off\n", "env:\n  RUSTFLAGS: -Cpanic=abort\n"):
            with self.subTest(workflow=workflow), self.assertRaises(ValueError):
                check_no_profile_overrides([], [workflow])

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


if __name__ == "__main__":
    unittest.main()
