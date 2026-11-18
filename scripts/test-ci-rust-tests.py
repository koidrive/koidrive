#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
from pathlib import Path
import contextlib
import importlib.util
import io
import json
import re
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location("ci_rust_tests", ROOT / "scripts/ci-rust-tests.py")
tool = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(tool)


def matches(expression, binary, name):
    for term in re.split(r" \| (?=\(|binary_id)", expression):
        whole = re.fullmatch(r"binary_id\(=(\S+)\)", term)
        one = re.fullmatch(r"\(binary_id\(=(\S+)\) & test\(=(\S+)\)\)", term)
        group = re.fullmatch(r"\(binary_id\(=(\S+)\) & \((.*)\)\)", term)
        if whole and whole[1] == binary:
            return True
        if one and (one[1], one[2]) == (binary, name):
            return True
        if group and group[1] == binary and name in re.findall(r"test\(=([^)]+)\)", group[2]):
            return True
        if not (whole or one or group) and term != "none()":
            raise AssertionError(f"unexpected filterset term {term}")
    return False


def listing(tests, ignored=()):
    suites = {}
    for binary, name in tests:
        suites.setdefault(binary, {"binary-id": binary, "testcases": {}})["testcases"][name] = {"filter-match": {"status": "matches"}}
    for binary, name in ignored:
        suites.setdefault(binary, {"binary-id": binary, "testcases": {}})["testcases"][name] = {
            "filter-match": {"status": "mismatch", "reason": "ignored"}}
    return {"rust-suites": suites}


class ArchiveArgs(unittest.TestCase):
    def test_the_remainder_archives_the_workspace_without_unselected_suites(self):
        self.assertEqual(tool.archive_args({"rust", "rust_engine"}),
                         ["--workspace", "--exclude", "koidrive-sim", "--exclude", "koidrive-driver", "--features", tool.FEATURES])

    def test_suites_alone_archive_their_packages(self):
        self.assertEqual(tool.archive_args({"rust_driver", "rust_engine"}),
                         ["-p", "koidrive-driver", "-p", "koidrive-engine", "--features", "koidrive-engine/serde"])

    def test_no_selection_is_an_error(self):
        with self.assertRaises(ValueError):
            tool.archive_args({"rust_sim"})


class Plan(unittest.TestCase):
    def setUp(self):
        self.tests = [("big", f"t{i}") for i in range(200)] + [("mid", f"m{i}") for i in range(40)] \
            + [(f"small{b}", f"s{i}") for b in range(30) for i in range(5)] + [("new", "fresh")]
        self.times = {"big": {"total": 900.0, "count": 200, "tests": {"t0": 230.0, "t1": 60.0, "t2": 40.0, "gone": 9.0}},
                      "mid": {"total": 120.0, "count": 40, "tests": {}}}
        for b in range(30):
            self.times[f"small{b}"] = {"total": 6.0, "count": 5, "tests": {}}

    def test_every_listed_test_lands_in_exactly_one_shard(self):
        for shards in (1, 3, 5, 40):
            with self.subTest(shards=shards):
                loads = tool.plan(self.grouped(), self.times, shards)
                expressions = [" | ".join(e) if e else "none()" for _, e, _ in loads]
                for binary, name in self.tests:
                    hits = [index for index, expression in enumerate(expressions) if matches(expression, binary, name)]
                    self.assertEqual(len(hits), 1, (binary, name, hits))
                self.assertEqual(sum(count for _, _, count in loads), len(self.tests))

    def test_shards_balance_measured_time(self):
        loads = [seconds for seconds, _, _ in tool.plan(self.grouped(), self.times, 4)]
        self.assertLessEqual(max(loads), max(230.0, sum(loads) / 4) * 1.05)

    def test_the_plan_writes_one_profile_per_shard(self):
        with tempfile.TemporaryDirectory() as temp:
            (Path(temp) / "list.json").write_text(json.dumps(listing(self.tests, ignored=[("big", "slow_ignored")])))
            args = type("Args", (), {"os": "fixture", "shards": 3, "list": f"{temp}/list.json", "out": f"{temp}/shards.toml"})
            with contextlib.redirect_stdout(io.StringIO()):
                tool.command_plan(args)
            text = (Path(temp) / "shards.toml").read_text()
        self.assertTrue(text.startswith(tool.CONFIG.read_text()))
        self.assertEqual(re.findall(r"(?m)^\[profile\.(shard-\d+)\]$", text), ["shard-1", "shard-2", "shard-3"])
        self.assertEqual(text.count('inherits = "ci"'), 3)
        self.assertNotIn("slow_ignored", text)

    def grouped(self):
        found = {}
        for binary, name in self.tests:
            found.setdefault(binary, []).append(name)
        return found


class Report(unittest.TestCase):
    def run_report(self, cases, tests):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        root = Path(temp.name)
        (root / "list.json").write_text(json.dumps(listing(tests)))
        for shard, rows in cases.items():
            (root / "junit" / shard).mkdir(parents=True)
            body = "".join(f'<testcase classname="{b}" name="{n}" time="{t}">{extra}</testcase>' for b, n, t, extra in rows)
            (root / "junit" / shard / "junit.xml").write_text(f"<testsuites><testsuite>{body}</testsuite></testsuites>")
        args = type("Args", (), {"os": "fixture", "list": [str(root / "list.json")], "junit": str(root / "junit"), "out": str(root / "out")})
        with contextlib.redirect_stdout(io.StringIO()) as out:
            status = tool.command_report(args)
        return status, out.getvalue(), root / "out"

    def test_a_complete_run_reports_timings_and_flaky_tests(self):
        status, out, files = self.run_report({
            "rust-junit-fixture-1": [("a", "x", "3.5", ""), ("a", "y", "0.1", '<flakyFailure message="boom"/>')],
            "rust-junit-fixture-2": [("b", "z", "1.0", '<failure message="no"/>'), ("b", "w", "0.2", '<flakyError message="crash"/>')],
        }, [("a", "x"), ("a", "y"), ("b", "z"), ("b", "w")])
        self.assertEqual(status, 0)
        self.assertIn("| a | y | rust-junit-fixture-1 |", out)
        self.assertIn("| b | w | rust-junit-fixture-2 |", out)
        self.assertIn("1 failed, 2 passed only on retry", out)
        times = json.loads((files / "rust-test-times.json").read_text())["fixture"]
        self.assertEqual(times["a"], {"total": 3.6, "count": 2, "tests": {"x": 3.5}})

    def test_a_binary_total_is_rounded_once(self):
        _, _, files = self.run_report({"s1": [("a", name, "0.04", "") for name in "xyz"]}, [("a", "x"), ("a", "y"), ("a", "z")])
        self.assertEqual(json.loads((files / "rust-test-times.json").read_text())["fixture"]["a"]["total"], 0.1)

    def test_a_test_no_shard_ran_fails(self):
        status, out, _ = self.run_report({"s1": [("a", "x", "1", "")]}, [("a", "x"), ("a", "lost")])
        self.assertEqual(status, 1)
        self.assertIn("a lost is in the archive's test list but no shard ran it", out)

    def test_a_test_two_shards_ran_fails(self):
        status, out, _ = self.run_report({"s1": [("a", "x", "1", "")], "s2": [("a", "x", "1", "")]}, [("a", "x")])
        self.assertEqual(status, 1)
        self.assertIn("a x ran in more than one shard", out)



class FilterTerms(unittest.TestCase):
    TERMS = ["koidrive-fs::volumes", "koidrive-driver=disk_ejected"]

    def run_tool(self, *argv):
        with contextlib.redirect_stdout(io.StringIO()) as out:
            status = tool.main(list(argv))
        return status, out.getvalue()

    def matched(self, rows):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        junit = Path(temp.name) / "junit.xml"
        body = "".join(f'<testcase classname="{b}" name="{n}" time="0.1"/>' for b, n in rows)
        junit.write_text(f"<testsuites><testsuite>{body}</testsuite></testsuites>")
        return self.run_tool("matched", "--junit", str(junit), *self.TERMS)

    def test_terms_become_one_filter_expression(self):
        status, out = self.run_tool("filter", *self.TERMS)
        self.assertEqual(status, 0)
        self.assertEqual(out.strip(), "binary_id(=koidrive-fs::volumes) | (binary_id(=koidrive-driver) & test(disk_ejected))")

    def test_every_term_that_matched_a_test_passes(self):
        status, out = self.matched([("koidrive-fs::volumes", "a_volume"), ("koidrive-driver", "folder::tests::a_disk_ejected_while_open")])
        self.assertEqual((status, out), (0, ""))

    def test_a_term_that_matched_nothing_fails_and_names_the_term(self):
        status, out = self.matched([("koidrive-fs::volumes", "a_volume"), ("koidrive-engine", "a_disk_ejected_while_open")])
        self.assertEqual(status, 1)
        self.assertIn("The filter term koidrive-driver=disk_ejected matched no test", out)
        self.assertNotIn("koidrive-fs::volumes matched", out)


if __name__ == "__main__":
    unittest.main()
