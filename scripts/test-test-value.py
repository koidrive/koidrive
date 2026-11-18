#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
import importlib.util
import json
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


def planned(sample, count):
    return {"sample": sample, "mutants": [
        {"id": number, "name": f"m{number}", "file": f"core/x/src/{sample}.rs", "line": 1, "function": None,
         "genre": "FnValue"} for number in range(count)]}


def records(killers, status=None):
    return [{"mutant": number, "status": status or ("killed" if tests else "survived"), "killers": tests, "seconds": 1}
            for number, tests in enumerate(killers)]


REDUNDANT = "redundant within core/x/src/guard.rs"
NOTHING_ELSE = "slow, catches nothing else within core/x/src/guard.rs"


class Analysis(unittest.TestCase):
    def analyse(self, killers, durations, properties=(), flaky=(), few=2):
        base = {"durations": durations, "flaky": list(flaky)}
        rows, mutants, _, _, _ = test_value.analyse([(planned("guard", len(killers)), base, records(killers))],
                                                 set(properties), slow_seconds=10, few=few)
        return {row["test"]: row for row in rows}, mutants

    def test_a_test_whose_kills_another_test_covers_is_a_deletion_candidate_naming_its_cover(self):
        rows, _ = self.analyse([["b x", "b y", "b z"], ["b y", "b z"], ["b z"]], {"b x": 0.1, "b y": 0.5, "b z": 0.2})
        self.assertEqual((rows["b y"]["verdict"], rows["b y"]["covered_by"]), (REDUNDANT, "b z"))
        self.assertEqual((rows["b x"]["verdict"], rows["b x"]["covered_by"]), (REDUNDANT, "b z"))
        self.assertEqual((rows["b z"]["kills"], rows["b z"]["unique_kills"], rows["b z"]["verdict"]), (3, 1, "keep"))

    def test_two_tests_that_only_cover_each_other_leave_one_to_keep(self):
        rows, _ = self.analyse([["b x", "b y"]], {"b x": 1, "b y": 2})
        self.assertEqual((rows["b y"]["verdict"], rows["b y"]["covered_by"]), (REDUNDANT, "b x"))
        self.assertEqual(rows["b x"]["verdict"], "keep")

    def test_applying_every_candidate_leaves_each_killed_mutant_a_killer(self):
        killers = [["b a", "b b"], ["b b", "b c"], ["b c", "b a"], ["b a", "b b", "b c"], ["b d", "b a"]]
        rows, _ = self.analyse(killers, {"b a": 1, "b b": 2, "b c": 3, "b d": 4})
        kept = {test for test, row in rows.items() if row["verdict"] == "keep"}
        self.assertTrue(all(set(tests) & kept for tests in killers))

    def test_covers_name_only_tests_that_stay(self):
        rows, _ = self.analyse([["b x", "b w", "b a"], ["b x", "b w", "b b"]], {"b x": 3, "b w": 2, "b a": 1, "b b": 1})
        self.assertEqual((rows["b x"]["verdict"], rows["b w"]["verdict"]), (REDUNDANT, REDUNDANT))
        self.assertEqual((rows["b x"]["covered_by"], rows["b w"]["covered_by"]), ("b a; b b", "b a; b b"))

    def test_a_test_whose_subject_lies_outside_the_mutated_code_is_judged_only_within_it(self):
        rows, _ = self.analyse([["koidrive-engine::guard g", "koidrive-engine::commit c"], ["koidrive-engine::guard g"]],
                               {"koidrive-engine::guard g": 0.1, "koidrive-engine::commit c": 0.2,
                                "koidrive-engine::first_upload work": 90})
        self.assertEqual(rows["koidrive-engine::commit c"]["verdict"], REDUNDANT)
        self.assertEqual(rows["koidrive-engine::first_upload work"]["verdict"], NOTHING_ELSE)
        self.assertEqual(rows["koidrive-engine::guard g"]["verdict"], "keep")
        self.assertEqual({row["mutated"] for row in rows.values()}, {"core/x/src/guard.rs"})

    def test_a_cover_made_of_several_tests_lists_each(self):
        rows, _ = self.analyse([["b x", "b y"], ["b x", "b z"], ["b y"], ["b z"]], {"b x": 1, "b y": 0.5, "b z": 0.5})
        self.assertEqual(rows["b x"]["covered_by"], "b y; b z")

    def test_slow_tests_move_only_while_the_merge_path_loses_at_most_few_mutants(self):
        rows, _ = self.analyse([["b slow"], ["b slow", "b slower"], ["b slower"], ["b slower"], ["b fast"]],
                               {"b slow": 60, "b slower": 90, "b fast": 0.1, "b idle": 60}, few=1)
        self.assertEqual((rows["b slower"]["verdict"], rows["b slower"]["merge_path_loses"]), ("keep", 2))
        self.assertEqual((rows["b slow"]["verdict"], rows["b slow"]["merge_path_loses"]),
                         ("slow, alone catches 1 within core/x/src/guard.rs", 1))
        self.assertEqual(rows["b idle"]["verdict"], NOTHING_ELSE)
        self.assertEqual(rows["b idle"]["ran_on"], "mutants the faster tests missed")

    def test_two_slow_tests_that_only_cover_each_other_do_not_both_move(self):
        rows, _ = self.analyse([["b s1", "b s2"], ["b s1", "b s2"], ["b s1", "b s2"]], {"b s1": 60, "b s2": 70}, few=0)
        self.assertEqual(sorted(row["verdict"] for row in rows.values()), ["keep", NOTHING_ELSE])

    def test_property_tests_and_flaky_tests_are_never_candidates(self):
        rows, _ = self.analyse([["b prop", "b x"]], {"b prop": 1, "b x": 0.5, "b flaky": 9},
                               properties={"prop"}, flaky={"b flaky"})
        self.assertEqual(rows["b prop"]["verdict"], "keep: property test")
        self.assertEqual(rows["b flaky"]["verdict"], "flaky in baseline: not measured")
        self.assertEqual((rows["b x"]["verdict"], rows["b x"]["covered_by"]), (REDUNDANT, "b prop"))

    def test_survivors_and_tests_without_kills_are_reported(self):
        rows, mutants = self.analyse([[], ["b x"]], {"b x": 1, "b idle": 1})
        self.assertEqual(rows["b idle"]["verdict"], "no kills within core/x/src/guard.rs")
        self.assertEqual([row["status"] for row in mutants], ["survived", "killed"])

    def test_mutants_without_a_result_make_every_verdict_provisional_and_attribute_nothing(self):
        base = {"durations": {"b x": 1, "b y": 1}, "flaky": []}
        done = records([["b x", "b y"]]) + [{"mutant": 1, "status": "error", "killers": ["b y"], "seconds": 1}]
        rows, _, measured, provisional, _ = test_value.analyse([(planned("guard", 3), base, done)], set(), 10, 2)
        rows = {row["test"]: row for row in rows}
        self.assertTrue(provisional)
        self.assertEqual(measured, "core/x/src/guard.rs")
        self.assertEqual(rows["b y"]["kills"], 1)
        self.assertTrue(all(row["verdict"].endswith("(provisional)") for row in rows.values()))

    def test_runs_of_several_samples_count_kills_together(self):
        base = {"durations": {"b x": 1, "b y": 1}, "flaky": []}
        runs = [(planned("guard", 1), base, records([["b x", "b y"]])),
                (planned("commit", 1), base, records([["b x"]]))]
        rows, mutants, measured, _, _ = test_value.analyse(runs, set(), 10, 2)
        rows = {row["test"]: row for row in rows}
        self.assertEqual((rows["b x"]["kills"], rows["b x"]["unique_kills"]), (2, 1))
        self.assertEqual(rows["b y"]["verdict"], "redundant within core/x/src/commit.rs, core/x/src/guard.rs")
        self.assertEqual([(row["sample"], row["id"]) for row in mutants], [("commit", 0), ("guard", 0)])
        self.assertEqual(measured, "core/x/src/commit.rs, core/x/src/guard.rs")


class Report(unittest.TestCase):
    def test_writes_headers_when_no_mutant_survived(self):
        with tempfile.TemporaryDirectory() as directory:
            run = Path(directory) / "run"
            run.mkdir()
            (run / "mutants.json").write_text(json.dumps(planned("guard", 1)))
            (run / "baseline.json").write_text(json.dumps({"durations": {"b x": 1}, "flaky": []}))
            (run / "results-0.jsonl").write_text(json.dumps(records([["b x"]])[0]) + "\n")
            test_value.main(["--root", directory, "report", "--sample", "guard", "--runs", str(run),
                             "--out", str(Path(directory) / "out")])
            survivors = (Path(directory) / "out/guard-survivors.csv").read_text()
            self.assertEqual(survivors, ",".join(test_value.MUTANT_FIELDS) + "\n")


class Tier(unittest.TestCase):
    def tier(self, code, results, incomplete=False):
        original = test_value.nextest
        test_value.nextest = lambda *args: (code, results, incomplete, 1.0)
        try:
            return test_value.tier(None, None, "all()", None, 1, None)
        finally:
            test_value.nextest = original

    def test_each_nextest_outcome_maps_to_one_status(self):
        failing = {"b x": ("FAIL", 1.0), "b y": ("PASS", 1.0), "b z": ("LEAK", 1.0)}
        self.assertEqual(self.tier(100, failing)["status"], "killed")
        self.assertEqual(self.tier(100, failing)["killers"], ["b x"])
        self.assertEqual(self.tier(0, {"b y": ("PASS", 1.0)})["status"], "survived")
        self.assertEqual(self.tier(101, {})["status"], "unviable")
        self.assertEqual(self.tier(100, {})["status"], "error")
        self.assertEqual(self.tier(104, {})["status"], "error")
        self.assertEqual(self.tier(0, {}, incomplete=True)["status"], "incomplete")
        self.assertEqual(self.tier(None, failing, incomplete=True)["status"], "incomplete")


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
