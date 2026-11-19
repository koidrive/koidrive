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
            args = type("Args", (), {"os": "fixture", "shards": 3, "list": f"{temp}/list.json", "out": f"{temp}/shards.toml",
                                    "event": "merge_group"})
            with contextlib.redirect_stdout(io.StringIO()):
                tool.command_plan(args)
            text = (Path(temp) / "shards.toml").read_text()
        self.assertTrue(text.startswith(tool.CONFIG.read_text()))
        self.assertEqual(re.findall(r"(?m)^\[profile\.(shard-\d+)\]$", text), ["shard-1", "shard-2", "shard-3"])
        self.assertEqual(text.count('inherits = "ci"'), 3)
        self.assertNotIn("slow_ignored", text)

    def test_a_pull_request_plan_leaves_out_the_merge_only_tests_and_a_merge_group_plan_keeps_them(self):
        original, tool.merge_only = tool.merge_only, lambda: {("big", "t0"), ("sim", "absent_binary")}
        self.addCleanup(setattr, tool, "merge_only", original)
        for event, present in (("pull_request", False), ("merge_group", True), ("push", True)):
            with self.subTest(event=event), tempfile.TemporaryDirectory() as temp:
                (Path(temp) / "list.json").write_text(json.dumps(listing(self.tests)))
                args = type("Args", (), {"os": "fixture", "shards": 3, "list": f"{temp}/list.json",
                                         "out": f"{temp}/shards.toml", "event": event})
                with contextlib.redirect_stdout(io.StringIO()):
                    tool.command_plan(args)
                filters = re.findall(r"default-filter = '''(.*)'''", (Path(temp) / "shards.toml").read_text())
                self.assertEqual(len(filters), 3)
                for expression in filters:
                    left_out = expression.endswith(" & !((binary_id(=big) & test(=t0)))")
                    self.assertEqual(left_out, not present, expression[-80:])
                    self.assertTrue(left_out or "!(" not in expression)

    def test_a_merge_only_test_the_archive_lost_fails_the_plan(self):
        with self.assertRaises(SystemExit), contextlib.redirect_stdout(io.StringIO()) as out:
            tool.deferred({"big": ["t1"]}, "merge_group", {("big", "renamed")})
        self.assertIn("big renamed", out.getvalue())

    def test_the_merge_only_list_names_tests_that_exist(self):
        for binary, name in tool.merge_only():
            package, _, target = binary.partition("::")
            source = {"koidrive-engine": "core/engine/tests", "koidrive-driver": "core/driver/src",
                      "koidrive-sim": "core/sim/tests"}[package]
            function = name.rsplit("::", 1)[-1]
            with self.subTest(test=f"{binary} {name}"):
                self.assertTrue(any(f"fn {function}(" in path.read_text() for path in (ROOT / source).rglob("*.rs")))

    def grouped(self):
        found = {}
        for binary, name in self.tests:
            found.setdefault(binary, []).append(name)
        return found


class Report(unittest.TestCase):
    def run_report(self, cases, tests, event="merge_group"):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        root = Path(temp.name)
        (root / "list.json").write_text(json.dumps(listing(tests)))
        for shard, rows in cases.items():
            (root / "junit" / shard).mkdir(parents=True)
            body = "".join(f'<testcase classname="{b}" name="{n}" time="{t}">{extra}</testcase>' for b, n, t, extra in rows)
            (root / "junit" / shard / "junit.xml").write_text(f"<testsuites><testsuite>{body}</testsuite></testsuites>")
        args = type("Args", (), {"os": "fixture", "list": [str(root / "list.json")], "junit": str(root / "junit"), "out": str(root / "out"),
                                 "event": event})
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

    def test_a_pull_request_expects_no_merge_only_test_and_fails_if_one_ran(self):
        original, tool.merge_only = tool.merge_only, lambda: {("a", "slow")}
        self.addCleanup(setattr, tool, "merge_only", original)
        status, out, _ = self.run_report({"s1": [("a", "x", "1", "")]}, [("a", "x"), ("a", "slow")], event="pull_request")
        self.assertEqual(status, 0)
        self.assertIn("1 slow tests listed in", out)
        status, out, _ = self.run_report({"s1": [("a", "x", "1", ""), ("a", "slow", "9", "")]}, [("a", "x"), ("a", "slow")],
                                         event="pull_request")
        self.assertEqual(status, 1)
        self.assertIn("a slow waits for the merge queue but ran on this pull request", out)
        status, out, _ = self.run_report({"s1": [("a", "x", "1", "")]}, [("a", "x"), ("a", "slow")], event="merge_group")
        self.assertEqual(status, 1)
        self.assertIn("a slow is in the archive's test list but no shard ran it", out)

    def test_skip_args_leave_out_a_packages_merge_only_tests_on_pull_requests_only(self):
        original, tool.merge_only = tool.merge_only, lambda: {("koidrive-sim::devices", "seed_1"), ("koidrive-engine", "e")}
        self.addCleanup(setattr, tool, "merge_only", original)
        with tempfile.TemporaryDirectory() as temp:
            inventory = Path(temp) / "inventory.txt"
            inventory.write_text("seed_1: test\nother: test\n")
            for event, expected in (("pull_request", "--skip\nseed_1\n"), ("merge_group", "")):
                args = type("Args", (), {"package": "koidrive-sim", "inventory": str(inventory), "event": event})
                with contextlib.redirect_stdout(io.StringIO()) as out:
                    self.assertEqual(tool.command_skip_args(args), 0)
                self.assertEqual(out.getvalue(), expected)
            inventory.write_text("other: test\n")
            args = type("Args", (), {"package": "koidrive-sim", "inventory": str(inventory), "event": "merge_group"})
            with contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(tool.command_skip_args(args), 1)

    def test_a_test_two_shards_ran_fails(self):
        status, out, _ = self.run_report({"s1": [("a", "x", "1", "")], "s2": [("a", "x", "1", "")]}, [("a", "x")])
        self.assertEqual(status, 1)
        self.assertIn("a x ran in more than one shard", out)


class Shadow(unittest.TestCase):
    def run_shadow(self, selection, rows, complete=True):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        root = Path(temp.name)
        if selection is not None:
            (root / "selection.json").write_text(json.dumps(selection))
        (root / "junit" / "s1").mkdir(parents=True)
        body = "".join(f'<testcase classname="{b}" name="{n}">{extra}</testcase>' for b, n, extra in rows)
        (root / "junit" / "s1" / "junit.xml").write_text(f"<testsuites><testsuite>{body}</testsuite></testsuites>")
        args = type("Args", (), {"selection": str(root / "selection.json"), "junit": [str(root / "junit")], "out": str(root / "out/shadow.json"), "complete": complete})
        with contextlib.redirect_stdout(io.StringIO()) as out:
            status = tool.command_shadow(args)
        return status, out.getvalue(), json.loads((root / "out/shadow.json").read_text())

    def test_a_failed_test_the_selection_skips_is_a_miss(self):
        status, out, record = self.run_shadow({"map": "abc", "everything": None, "skipped": ["a x", "a y", "a gone"]},
                                              [("a", "x", '<failure message="no"/>'), ("a", "y", ""), ("a", "z", '<failure message="no"/>')])
        self.assertEqual(status, 0)
        self.assertEqual(record["misses"], ["a x"])
        self.assertEqual((record["ran"], record["skipped"], record["failed"]), (3, 2, ["a x", "a z"]))
        self.assertIn("would skip 2 of the 3 tests that ran. 1 of the 2 tests", out)
        self.assertIn("| a x |", out)

    def test_a_kept_selection_reports_why_and_never_misses(self):
        _, out, record = self.run_shadow({"map": None, "everything": "there is no coverage map", "skipped": []}, [("a", "x", '<failure message="no"/>')])
        self.assertEqual(record["misses"], [])
        self.assertIn("It kept every test, because there is no coverage map.", out)

    def test_missing_shard_results_mark_the_count_incomplete(self):
        _, out, record = self.run_shadow({"map": "abc", "everything": None, "skipped": ["a x"]}, [("a", "x", "")], complete=False)
        self.assertFalse(record["complete"])
        self.assertIn("The shard results are incomplete", out)

    def test_a_missing_selection_keeps_every_test(self):
        status, out, _ = self.run_shadow(None, [("a", "x", "")])
        self.assertEqual(status, 0)
        self.assertIn("the changes job recorded no selection", out)


class CoverageMap(unittest.TestCase):
    def test_only_files_inside_the_checkout_with_covered_lines_count(self):
        report = {"data": [{"files": [
            {"filename": "/w/core/a.rs", "summary": {"lines": {"covered": 3}}},
            {"filename": "/w/core/b.rs", "summary": {"lines": {"covered": 0}}},
            {"filename": "/home/.cargo/registry/x.rs", "summary": {"lines": {"covered": 9}}},
        ]}]}
        self.assertEqual(tool.covered_files(report, "/w/"), {"core/a.rs"})

    def test_a_function_spans_its_own_file_regions_only(self):
        report = {"data": [{"functions": [
            {"filenames": ["/w/core/a.rs", "/w/core/m.rs"], "regions": [[10, 1, 12, 2, 1, 0, 0, 0], [11, 1, 30, 1, 0, 0, 0, 0], [1, 1, 99, 1, 0, 1, 0, 0]]},
            {"filenames": ["/w/core/a.rs"], "regions": [[40, 1, 41, 1, 0, 0, 0, 0]]},
            {"filenames": ["/home/.cargo/registry/x.rs"], "regions": [[1, 1, 2, 1, 1, 0, 0, 0]]},
        ]}]}
        self.assertEqual(tool.function_spans(report, "/w"), {"core/a.rs": {(10, 30), (40, 41)}})

    def test_a_const_fn_has_no_span_because_its_body_can_run_at_compile_time(self):
        source = ["pub const fn limit() -> u32 {", "    7", "}", "pub fn twice(x: u32) -> u32 {", "    x * 2", "}",
                  "pub const unsafe fn raw() -> u32 {", "    1", "}", 'const extern "C" fn c() {}', "fn constant_name() {}"]
        self.assertEqual(tool.without_const_fns({"a.rs": {(1, 3), (4, 6), (7, 9), (10, 10), (11, 11)}}, lambda file: source),
                         {"a.rs": {(4, 6), (11, 11)}})

    def test_a_blob_is_the_git_object_id(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "a.rs"
            path.write_text("a\n")
            self.assertEqual(tool.blob(path), "78981922613b2afb6025042ff6bd878ac1994e85")

    def test_the_map_indexes_each_file_once(self):
        self.assertEqual(tool.coverage_map({"a y": {"c.rs", "b.rs"}, "a x": {"b.rs"}}, {"b.rs": {(5, 9), (1, 3)}}, {"b.rs": "1", "c.rs": "2"}, "sha", 7), {
            "sha": "sha", "created": 7,
            "files": [{"path": "b.rs", "blob": "1", "functions": [[1, 3], [5, 9]]}, {"path": "c.rs", "blob": "2", "functions": []}],
            "tests": {"a x": [0], "a y": [0, 1]}})


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
