#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import tempfile
import textwrap
import unittest


RUNNER = Path(__file__).with_name("mutations.sh")
CASE = "normal.cases/cc-" + "a" * 64 + ".txt"
SAVED = "cc " + "a" * 64 + " # checked-in case\n"
REASON = "Test failed: complete failure reason " + "detail " * 30
LEGACY_CC = "cc " + "b" * 64 + " # generated legacy case"
CASE_CC = "cc " + "c" * 64 + " # generated case file"
TRACKED_CC = "cc " + "d" * 64 + " # appended tracked case"


class EngineMutations(unittest.TestCase):
    def test_renamed_revert_disables_both_rename_paths_and_preserves_follows(self):
        root = RUNNER.parent.parent
        result = subprocess.run([
            "bash", "-c",
            'mutation() { if [ "$1" = renamed-revert ]; then printf "%s\\0%s\\0" "$2" "$3"; fi; }; source scripts/engine-mutations.sh',
        ], cwd=root, capture_output=True, text=True, check=True)
        file, expression, end = result.stdout.split("\0")
        self.assertEqual(end, "")
        source = (root / file).read_text()
        immediate = "(renamed && !left && !kept)"
        deferred = "else if renamed && !left"
        self.assertEqual(source.count(immediate), 1)
        self.assertEqual(source.count(deferred), 1)
        expected = source.replace(immediate, "(false && renamed && !left && !kept)", 1)
        expected = expected.replace(deferred, "else if false && renamed && !left", 1)
        transformed = subprocess.run(["perl", "-0pe", expression], input=source, capture_output=True, text=True, check=True).stdout
        self.assertEqual(transformed, expected)
        self.assertIn("let mut unreverted: BTreeSet<NodeId> = BTreeSet::new();", transformed)
        self.assertEqual(transformed.count("unreverted.insert("), 1)
        self.assertIn("if (false && renamed && !left && !kept) || (done && follows) {\n                trees.revert(node.clone());", transformed)
        self.assertIn("else if false && renamed && !left {\n                unreverted.insert(node.clone());", transformed)
        self.assertIn("for id in unreverted.iter().filter(|id| !taken.contains(*id)) {\n            trees.revert(id.clone());", transformed)

    def test_renamed_revert_targets_the_existing_renamed_result_unit_test(self):
        root = RUNNER.parent.parent
        helper = (root / "scripts/test-engine-mutations.sh").read_text()
        tests = shlex.split(re.search(r"^tests=\((.*?)\)", helper, re.MULTILINE | re.DOTALL).group(1))
        mutants = shlex.split(re.search(r"^mutants=\((.*?)\)", helper, re.MULTILINE | re.DOTALL).group(1))
        self.assertEqual(len(tests), len(mutants))
        test = tests[mutants.index("renamed-revert")]
        self.assertEqual(test, "a_renamed_result_reverts_the_local_name_to_the_server_one")
        existing = (root / "core/engine/tests/commit/commit_moves_and_conflicts.rs").read_text()
        self.assertIn(f"fn {test}()", existing)


class MutationRunner(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="mutation-runner.")
        self.root = Path(self.temp.name)
        (self.root / "scripts").mkdir()
        (self.root / "bin").mkdir()
        (self.root / "normal.cases").mkdir()
        shutil.copyfile(RUNNER, self.root / "scripts/mutations.sh")
        (self.root / "scripts/test-engine-mutations.sh").write_text("printf 'fixture guard\\n'\n")
        (self.root / "engine.rs").write_text("original\n")
        (self.root / CASE).write_text(SAVED)
        (self.root / "normal.proptest-regressions").write_text(SAVED)
        (self.root / "scripts/engine-mutations.sh").write_text(textwrap.dedent("""\
            unit=(cargo unit)
            property=(cargo property)
            devices=(cargo devices)
            mutation unit-mut engine.rs 's/original/unit/' unit
            mutation prop-mut engine.rs 's/original/property/' prop
            mutation devices-mut engine.rs 's/original/devices/' devices
            mutation both-mut engine.rs 's/original/both/' both
        """))
        self.git = shutil.which("git")
        subprocess.run([self.git, "init", "-q", str(self.root)], check=True)
        subprocess.run([self.git, "add", "scripts", "engine.rs", CASE, "normal.proptest-regressions"], cwd=self.root, check=True)
        self.executable("git", """\
            import os
            from pathlib import Path
            import subprocess
            import sys
            if sys.argv[1] == "status":
                sys.exit(0)
            if sys.argv[1] == "checkout":
                print("fixture restore", flush=True)
                changed = Path("engine.rs").read_text() != "original\\n"
                subprocess.run([os.environ["REAL_GIT"], *sys.argv[1:]], check=True)
                if changed:
                    os.utime("engine.rs", (1, 1))
            else:
                os.execv(os.environ["REAL_GIT"], [os.environ["REAL_GIT"], *sys.argv[1:]])
        """)
        self.executable("cargo", """\
            import json
            import os
            from pathlib import Path
            import sys
            suite = sys.argv[1]
            compile_only = "--no-run" in sys.argv
            state = Path("engine.rs").read_text().strip()
            case = "normal.cases/cc-" + "a" * 64 + ".txt"
            saved = "cc " + "a" * 64 + " # checked-in case\\n"
            fixtures = Path(case).read_text() == saved and Path("normal.proptest-regressions").read_text() == saved
            fresh = Path("engine.rs").stat().st_mtime > 1
            clean = not Path("generated.proptest-regressions").exists() and not Path("normal.cases/cc-" + "c" * 64 + ".txt").exists()
            with open("calls.jsonl", "a") as log:
                log.write(json.dumps(dict(seed=os.environ.get("PROPTEST_RNG_SEED"), cases=os.environ.get("PROPTEST_CASES"), suite=suite, compile=compile_only, state=state, fixtures=fixtures, fresh=fresh, clean=clean)) + "\\n")
            if not fixtures or not fresh or not clean:
                print("fixture inputs changed between runs")
                sys.exit(90)
            if not compile_only and state == suite == os.environ.get("HANG"):
                import subprocess, time
                Path("hang.pid").write_text(str(subprocess.Popen(["sleep", "300"]).pid))
                time.sleep(300)
            if compile_only:
                if state == "unit" and suite == os.environ.get("FAIL_COMPILE"):
                    print("error[E0001]: complete compile reason " + "detail " * 30)
                    for i in range(160):
                        print(f"compile trailing diagnostic {i}")
                    sys.exit(1)
                sys.exit(0)
            baseline = state == "original" and suite == os.environ.get("FAIL_BASELINE")
            mutant = state == suite or (state == "both" and suite == "property")
            if baseline or (mutant and not os.environ.get("SURVIVE")):
                print("Test failed: complete failure reason " + "detail " * 30)
                print(f"failure context {state}/{suite}")
                Path("generated.proptest-regressions").write_text("cc " + "b" * 64 + " # generated legacy case\\n")
                Path("normal.cases/cc-" + "c" * 64 + ".txt").write_text("cc " + "c" * 64 + " # generated case file\\n")
                with open("normal.proptest-regressions", "a") as saved:
                    saved.write("cc " + "d" * 64 + " # appended tracked case\\n")
                for i in range(160):
                    print(f"trailing diagnostic {i}")
                sys.exit(1)
        """)

    def tearDown(self):
        self.temp.cleanup()

    def executable(self, name, body):
        path = self.root / "bin" / name
        path.write_text("#!/usr/bin/env python3\n" + textwrap.dedent(body))
        path.chmod(0o755)

    def run_runner(self, *args, expect_guard=True, **overrides):
        env = os.environ.copy()
        for key in ["MUTATION_SEED", "PROPTEST_RNG_SEED", "FAIL_BASELINE", "FAIL_COMPILE", "SURVIVE", "HANG"]:
            env.pop(key, None)
        env.update(PATH=str(self.root / "bin") + os.pathsep + env["PATH"], REAL_GIT=self.git, MUTATION_CASES="8")
        env.update(overrides)
        (self.root / "calls.jsonl").unlink(missing_ok=True)
        result = subprocess.run(["bash", "scripts/mutations.sh", *args], cwd=self.root, env=env, capture_output=True, text=True)
        log = self.root / "calls.jsonl"
        calls = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
        self.assertEqual(result.stdout.count("fixture guard\n"), int(expect_guard))
        return result, calls

    def assert_inputs(self, calls, seed):
        self.assertTrue(calls)
        self.assertEqual({call["seed"] for call in calls}, {seed})
        self.assertEqual({call["cases"] for call in calls}, {"8"})
        self.assertTrue(all(call["fixtures"] and call["fresh"] and call["clean"] for call in calls), calls)
        self.assertEqual((self.root / CASE).read_text(), SAVED)
        self.assertEqual((self.root / "normal.proptest-regressions").read_text(), SAVED)
        self.assertFalse((self.root / "generated.proptest-regressions").exists())
        self.assertFalse((self.root / "normal.cases" / ("cc-" + "c" * 64 + ".txt")).exists())

    def test_default_seed_is_identical_for_every_suite_and_repeatable(self):
        first, calls = self.run_runner(PROPTEST_RNG_SEED="999")
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        self.assert_inputs(calls, "0")
        self.assertEqual(len(calls), 20)
        self.assertEqual([(c["state"], c["suite"]) for c in calls if not c["compile"]], [
            ("original", "unit"), ("original", "property"), ("original", "devices"),
            ("unit", "unit"), ("property", "property"), ("devices", "devices"),
            ("both", "unit"), ("both", "property"),
        ])
        second, repeated = self.run_runner(PROPTEST_RNG_SEED="111")
        self.assertEqual(second.returncode, 0, second.stdout + second.stderr)
        self.assertEqual(calls, repeated)

    def test_caller_seed_is_used_for_baseline_compilation_and_all_mutants(self):
        result, calls = self.run_runner(MUTATION_SEED="371", PROPTEST_RNG_SEED="999")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_inputs(calls, "371")
        self.assertEqual(len(calls), 20)

    def test_baseline_failures_preserve_full_reason_and_cc_before_cleanup(self):
        for suite in ["unit", "property", "devices"]:
            with self.subTest(suite=suite):
                result, calls = self.run_runner(FAIL_BASELINE=suite)
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assert_inputs(calls, "0")
                self.assertNotIn("caught by", result.stdout)
                for text in [REASON, LEGACY_CC, CASE_CC, TRACKED_CC, "trailing diagnostic 159"]:
                    self.assertIn(text, result.stdout)
                    self.assertLess(result.stdout.index(text), result.stdout.index("fixture restore"))

    def test_invalid_seed_is_rejected_before_the_guard_or_cargo(self):
        for seed in ["random", "-1", "18446744073709551616", "\u0661"]:
            with self.subTest(seed=seed):
                result, calls = self.run_runner(expect_guard=False, MUTATION_SEED=seed)
                self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
                self.assertEqual(calls, [])
                self.assertIn("MUTATION_SEED must be an unsigned 64-bit integer", result.stderr)

    def test_largest_supported_seed_is_preserved(self):
        seed = "18446744073709551615"
        result, calls = self.run_runner(MUTATION_SEED=seed)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_inputs(calls, seed)

    def test_mutant_failures_preserve_full_reason_and_cc_before_cleanup(self):
        result, calls = self.run_runner()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_inputs(calls, "0")
        segments = result.stdout.split("fixture restore")
        for state, suite in [("unit", "unit"), ("property", "property"), ("devices", "devices"), ("both", "property")]:
            segment = next(s for s in segments if f"failure context {state}/{suite}\n" in s)
            for text in [REASON, LEGACY_CC, CASE_CC, TRACKED_CC, "trailing diagnostic 159"]:
                self.assertIn(text, segment)
        self.assertEqual(result.stdout.count("caught by"), 4)

    def test_compile_failure_preserves_full_diagnostics(self):
        result, calls = self.run_runner(FAIL_COMPILE="property")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assert_inputs(calls, "0")
        self.assertIn("unit-mut: BROKEN", result.stdout)
        segment = result.stdout.split("fixture restore")[0]
        self.assertIn("error[E0001]: complete compile reason " + "detail " * 30, segment)
        self.assertIn("compile trailing diagnostic 159", segment)

    def test_survivors_run_each_suite_once_without_retries(self):
        result, calls = self.run_runner(SURVIVE="1")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assert_inputs(calls, "0")
        self.assertEqual(len(calls), 20)
        self.assertEqual(result.stdout.count("SURVIVED"), 4)

    def test_a_hanging_suite_is_caught_by_its_time_limit_and_killed(self):
        result, calls = self.run_runner(HANG="unit")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_inputs(calls, "0")
        self.assertRegex(result.stdout, r"unit-mut: caught by unit tests \(timed out after [0-9]+s, 3 times the baseline run\)")
        self.assertEqual(result.stdout.count("caught by"), 4)
        with self.assertRaises(ProcessLookupError):
            os.kill(int((self.root / "hang.pid").read_text()), 0)

    def test_sharding_does_not_change_the_seed(self):
        result, calls = self.run_runner("--shard", "1/2", MUTATION_SEED="371")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_inputs(calls, "371")
        self.assertEqual({call["state"] for call in calls}, {"original", "property", "both"})
        self.assertEqual(len(calls), 12)

    def test_preexisting_untracked_case_is_not_deleted_or_run(self):
        path = self.root / "normal.cases" / ("cc-" + "c" * 64 + ".txt")
        path.write_text(CASE_CC)
        result, calls = self.run_runner(expect_guard=False)
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertEqual(calls, [])
        self.assertEqual(path.read_text(), CASE_CC)


if __name__ == "__main__":
    unittest.main()
