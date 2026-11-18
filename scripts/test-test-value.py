#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
import importlib.util
from pathlib import Path
import subprocess
import tempfile
import textwrap
import unittest

ROOT = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location("test_value", ROOT / "scripts/test-value.py")
test_value = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(test_value)


class Status(unittest.TestCase):
    def test_reads_each_test_outcome_from_nextest_status_lines(self):
        output = textwrap.dedent("""\
                PASS [   0.013s] koidrive-engine::guard deletions_add_up_within_the_window
                FAIL [   1.350s] (  2/783) koidrive-driver tests::health::a_halt
             TIMEOUT [  60.004s] (  3/783) koidrive-driver tests::watch::a_slow_watch
                SLOW [> 30.000s] koidrive-driver tests::watch::a_slow_watch
             SIGSEGV [   0.200s] koidrive-sim::devices crash
            ────────────
                 Summary [   1.372s] 40 tests run: 40 passed, 0 skipped
        """)
        self.assertEqual(test_value.parse_status(output), {
            "koidrive-engine::guard deletions_add_up_within_the_window": ("PASS", 0.013),
            "koidrive-driver tests::health::a_halt": ("FAIL", 1.35),
            "koidrive-driver tests::watch::a_slow_watch": ("TIMEOUT", 60.004),
            "koidrive-sim::devices crash": ("SIGSEGV", 0.2),
        })


class Shards(unittest.TestCase):
    def test_shards_split_every_mutant_exactly_once(self):
        mutants = [{"id": number} for number in range(101)]
        seen = [mutant["id"] for index in range(7) for mutant in test_value.shard(mutants, index, 7)]
        self.assertEqual(sorted(seen), list(range(101)))
        self.assertEqual(test_value.shard(mutants, 3, 7), test_value.shard(mutants, 3, 7))


class Selection(unittest.TestCase):
    def test_fast_tier_excludes_slow_tests_and_slow_tier_runs_only_them(self):
        sample = test_value.SAMPLES["guard"]
        slow = {"koidrive-engine::guard_model the_notice_follows_the_model"}
        self.assertEqual(test_value.selection(sample, slow, include=False),
                         "(all()) & !((binary_id(=koidrive-engine::guard_model) & test(=the_notice_follows_the_model)))")
        self.assertEqual(test_value.selection(sample, slow, include=True),
                         "(all()) & ((binary_id(=koidrive-engine::guard_model) & test(=the_notice_follows_the_model)))")
        self.assertEqual(test_value.selection(sample, set(), include=False), "all()")
        self.assertEqual(test_value.selection(sample, set(), include=True), "none()")


class Analysis(unittest.TestCase):
    def analyse(self, killers, durations, properties=(), flaky=()):
        planned = {"sample": "guard", "mutants": [
            {"id": number, "name": f"m{number}", "file": "f.rs", "line": 1, "function": None, "genre": "FnValue"}
            for number in range(len(killers))]}
        base = {"durations": durations, "flaky": list(flaky)}
        records = [{"mutant": number, "status": "killed" if tests else "survived", "killers": tests, "seconds": 1}
                   for number, tests in enumerate(killers)]
        rows, mutants = test_value.analyse([(planned, base, records)], set(properties), slow_seconds=10, few=2)
        return {row["test"]: row for row in rows}, mutants

    def test_a_test_whose_kills_another_test_covers_is_a_deletion_candidate_naming_the_cheapest_cover(self):
        rows, _ = self.analyse(
            [["b x", "b y", "b z"], ["b y", "b z"], ["b z"]],
            {"b x": 0.1, "b y": 0.5, "b z": 0.2})
        self.assertEqual((rows["b x"]["unique_kills"], rows["b x"]["verdict"], rows["b x"]["covered_by"]),
                         (0, "delete or merge", "b z"))
        self.assertEqual((rows["b z"]["kills"], rows["b z"]["unique_kills"], rows["b z"]["verdict"]), (3, 1, "keep"))

    def test_a_cover_made_of_several_tests_lists_each(self):
        rows, _ = self.analyse([["b x", "b y"], ["b x", "b z"], ["b y"], ["b z"]], {"b x": 1, "b y": 1, "b z": 1})
        self.assertEqual(rows["b x"]["covered_by"], "b y; b z")

    def test_a_slow_test_with_few_unique_kills_moves_off_the_merge_path(self):
        rows, _ = self.analyse([["b slow"], ["b slow", "b fast"]], {"b slow": 60, "b fast": 0.1})
        self.assertEqual(rows["b slow"]["verdict"], "move off the merge path")

    def test_property_tests_and_flaky_tests_are_never_deletion_candidates(self):
        rows, _ = self.analyse([["b prop", "b x", "b flaky"]], {"b prop": 1, "b x": 1, "b flaky": 1},
                               properties={"prop"}, flaky={"b flaky"})
        self.assertEqual(rows["b prop"]["verdict"], "keep: property test")
        self.assertEqual(rows["b flaky"]["verdict"], "flaky in baseline: not measured")

    def test_survivors_and_tests_without_kills_are_reported(self):
        rows, mutants = self.analyse([[], ["b x"]], {"b x": 1, "b idle": 1})
        self.assertEqual(rows["b idle"]["verdict"], "no kills in the mutated code")
        self.assertEqual([row["status"] for row in mutants], ["survived", "killed"])


    def test_runs_of_several_samples_count_kills_together(self):
        def run(sample, killers):
            planned = {"sample": sample, "mutants": [
                {"id": n, "name": f"m{n}", "file": "f.rs", "line": 1, "function": None, "genre": "FnValue"}
                for n in range(len(killers))]}
            records = [{"mutant": n, "status": "killed", "killers": tests, "seconds": 1} for n, tests in enumerate(killers)]
            return planned, {"durations": {"b x": 1, "b y": 1}, "flaky": []}, records
        rows, mutants = test_value.analyse([run("guard", [["b x", "b y"]]), run("commit", [["b x"]])], set(), 10, 2)
        rows = {row["test"]: row for row in rows}
        self.assertEqual((rows["b x"]["kills"], rows["b x"]["unique_kills"]), (2, 1))
        self.assertEqual(rows["b y"]["verdict"], "delete or merge")
        self.assertEqual([(row["sample"], row["id"]) for row in mutants], [("commit", 0), ("guard", 0)])


class Sources(unittest.TestCase):
    def test_finds_functions_inside_proptest_blocks_only(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "core/x/tests/t.rs"
            source.parent.mkdir(parents=True)
            source.write_text(textwrap.dedent("""\
                proptest! {
                    #![proptest_config(battery_config("x", 8))]
                    #[test]
                    fn holds(x in 0..3u8) { if x > 1 { assert!(true) } }
                }
                #[test]
                fn plain() {}
            """))
            self.assertEqual(test_value.property_tests(Path(directory)), {"holds"})

    def test_every_hand_mutant_applies_to_the_checkout(self):
        mutants, stale = test_value.hand_mutants(ROOT)
        self.assertEqual(len(mutants) + len(stale), int(subprocess.run(
            ["bash", "scripts/mutations.sh", "--count"], cwd=ROOT, capture_output=True, text=True, check=True).stdout))
        for mutant in mutants:
            with self.subTest(mutant=mutant["name"]):
                subprocess.run(["git", "apply", "--check", "-"], cwd=ROOT, input=mutant["diff"], text=True, check=True)


if __name__ == "__main__":
    unittest.main()
