#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
import importlib.util
import os
import subprocess
import sys
import tempfile
import textwrap
import unittest

CHECK = os.path.join(os.path.dirname(os.path.abspath(__file__)), "check-test-lines.py")
CARGO = '[workspace]\nmembers = [\n    "core/a",\n    "core/test-support",\n]\n'


def load():
    spec = importlib.util.spec_from_file_location("check", CHECK)
    check = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(check)
    return check


class TestLines(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.repo = self.dir.name
        subprocess.run(["git", "init", "-q", self.repo], check=True)
        self.check = load()

    def tearDown(self):
        self.dir.cleanup()

    def tree(self, files):
        for path, text in files.items():
            full = os.path.join(self.repo, path)
            if text is None:
                os.remove(full)
                continue
            os.makedirs(os.path.dirname(full), exist_ok=True)
            with open(full, "w") as f:
                f.write(textwrap.dedent(text))
        subprocess.run(["git", "-C", self.repo, "add", "-A"], check=True)
        return subprocess.run(["git", "-C", self.repo, "write-tree"], check=True, capture_output=True, text=True).stdout.strip()

    def counts(self, files):
        cwd = os.getcwd()
        os.chdir(self.repo)
        try:
            return dict(self.check.count(self.check.Tree(self.tree({"Cargo.toml": CARGO, **files}))))
        finally:
            os.chdir(cwd)

    def net(self, before, after):
        base = self.tree({"Cargo.toml": CARGO, **before})
        head = self.tree(after)
        cwd = os.getcwd()
        os.chdir(self.repo)
        try:
            return self.check.count(self.check.Tree(head))["test"] - self.check.count(self.check.Tree(base))["test"]
        finally:
            os.chdir(cwd)

    def run_check(self, before, after, pr=715):
        base = self.tree({"Cargo.toml": CARGO, **before})
        head = self.tree(after)
        summary = os.path.join(self.repo, ".summary")
        args = [] if pr is None else ["--pr", str(pr)]
        result = subprocess.run([sys.executable, CHECK, base, head, *args], cwd=self.repo, capture_output=True, text=True,
                                env={**os.environ, "GITHUB_STEP_SUMMARY": summary})
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        with open(summary) as f:
            self.assertIn("## Rust test lines", f.read())
        return [line.split("::", 2)[2] for line in result.stdout.splitlines() if line.startswith("::warning")]

    def lines(self, n, prefix="t"):
        return "".join(f"fn {prefix}{i}() {{}}\n" for i in range(n))

    def test_files_under_tests_and_test_support_are_test_code(self):
        counts = self.counts({
            "core/a/src/lib.rs": "pub fn a() {}\n",
            "core/a/tests/flow.rs": "#[test]\nfn flow() {}\n",
            "core/a/benches/speed.rs": "fn main() {}\n",
            "core/test-support/src/lib.rs": "pub fn helper() {}\n",
        })
        self.assertEqual((counts["test"], counts["implementation"]), (4, 1))

    def test_a_cfg_test_module_counts_from_its_attribute_to_its_closing_brace(self):
        counts = self.counts({"core/a/src/lib.rs": """\
            pub fn a() -> [u8; 2] {
                let s = "} not the end {";
                let c = '}';
                [1, 2]
            }

            #[cfg(test)]
            mod tests {
                // a } in a comment
                /* a nested /* } */ block */
                const RAW: &str = r#"}"#;
                fn lifetime<'a>(s: &'a str) -> &'a str { s }
            }
            pub fn b() {}
        """})
        self.assertEqual((counts["test"], counts["implementation"]), (7, 7))

    def test_test_only_fields_statements_and_items_count_as_test_code(self):
        counts = self.counts({"core/a/src/lib.rs": """\
            struct S {
                #[cfg(test)]
                hook: Option<Box<dyn Fn(u8, u8)>>,
                kept: u8,
            }
            fn make() -> S {
                #[cfg(test)]
                if let Some(x) = hook() {
                    x();
                }
                let s = S {
                    #[cfg(test)]
                    hook: None,
                    kept: 1,
                };
                s
            }
            #[cfg(test)]
            const SEED: Seed = Seed { a: 1 };
        """})
        self.assertEqual(counts["test"], 10)

    def test_only_cfgs_that_exclude_non_test_builds_count(self):
        counts = self.counts({"core/a/src/lib.rs": """\
            #[cfg(all(test, target_os = "linux"))]
            fn linux_only() {}
            #[cfg(any(test, target_os = "macos"))]
            fn shared() {}
            #[cfg(not(test))]
            fn production() {}
            #[cfg(feature = "test-setup")]
            fn setup() {}
            #[cfg(all(not(test), unix))]
            fn unix_production() {}
        """})
        self.assertEqual((counts["test"], counts["implementation"]), (2, 8))

    def test_files_of_cfg_test_modules_and_their_children_are_test_code(self):
        counts = self.counts({
            "core/a/src/lib.rs": "pub mod store;\n#[cfg(test)]\nmod fake;\n",
            "core/a/src/store.rs": "pub fn put() {}\n#[cfg(test)]\nmod checks;\n",
            "core/a/src/store/checks.rs": "mod cases;\nfn t() {}\n",
            "core/a/src/store/checks/cases.rs": "fn case() {}\n",
            "core/a/src/fake/mod.rs": "pub fn fake() {}\n",
            "core/a/src/plain.rs": "#![cfg(test)]\nfn only_tests() {}\n",
        })
        self.assertEqual((counts["test"], counts["implementation"]), (10, 2))

    def test_a_file_that_implementation_also_includes_stays_implementation(self):
        counts = self.counts({
            "core/a/build.rs": '#[path = "src/shared.rs"]\nmod shared;\nfn main() {}\n',
            "core/a/src/lib.rs": "#[cfg(test)]\nmod shared;\n",
            "core/a/src/shared.rs": "pub fn shared() {}\n",
        })
        self.assertEqual((counts["test"], counts["implementation"]), (2, 4))

    def test_fixtures_regressions_and_snapshots_count_apart_from_test_code(self):
        counts = self.counts({
            "core/a/tests/fixtures/input.json": "{\n}\n",
            "core/a/tests/flow.proptest-regressions": "cc 1\ncc 2\n",
            "core/a/tests/flow.cases/one.txt": "cc 3\n",
            "core/a/proptest-regressions/engine.txt": "cc 4\n",
            "core/a/tests/goldens/setup/out.txt": "a\nb\nc\n",
            "core/a/tests/snapshots/x.snap": "s\n",
            "core/a/tests/data.bin": "\0\0\n",
            "spec/testdata/scenario.json": "[]\n",
            "server/test/a.test.ts": "it()\n",
        })
        self.assertEqual(counts, {"test": 0, "implementation": 0, "fixture": 3, "regression": 4, "snapshot": 4})

    def test_a_change_within_the_allowance_does_not_warn(self):
        self.assertEqual(self.run_check({}, {"core/a/tests/t.rs": self.lines(400)}), [])

    def test_a_change_above_the_allowance_warns_with_the_excess(self):
        warnings = self.run_check({}, {"core/a/tests/t.rs": self.lines(450)})
        self.assertEqual(len(warnings), 1)
        self.assertIn("adds 450 net test lines", warnings[0])
        self.assertIn("Remove or consolidate 50 test lines", warnings[0])

    def test_deleted_tests_offset_added_ones(self):
        before = {"core/a/tests/old.rs": self.lines(300)}
        after = {"core/a/tests/old.rs": None, "core/a/tests/new.rs": self.lines(650)}
        self.assertEqual(self.run_check(before, after), [])

    def test_fixtures_do_not_use_the_allowance(self):
        self.assertEqual(self.run_check({}, {"core/a/tests/fixtures/big.json": "1\n" * 5000}), [])

    def test_moving_a_module_file_changes_nothing(self):
        before = {"core/a/src/lib.rs": "#[cfg(test)]\nmod h;\n", "core/a/src/h/mod.rs": self.lines(413)}
        self.assertEqual(self.net(before, {"core/a/src/h/mod.rs": None, "core/a/src/h.rs": self.lines(413)}), 0)

    def test_a_module_declared_inside_an_inline_test_module_is_test_code(self):
        after = {
            "core/a/src/lib.rs": "#[cfg(all(unix, all(test, any(unix, windows))))]\nmod inline {\n    mod h;\n}\n",
            "core/a/src/inline/h.rs": self.lines(413),
        }
        self.assertEqual(self.net({}, after), 417)

    def test_a_test_only_const_fn_ends_at_its_body(self):
        after = {"core/a/src/lib.rs": "#[cfg(test)]\nconst fn fixture() -> u8 {\n    1\n}\n" + self.lines(413, "p")}
        self.assertEqual(self.net({}, after), 4)

    def test_a_test_only_field_named_like_a_keyword_ends_at_its_comma(self):
        counts = self.counts({"core/a/src/lib.rs": "struct S {\n    #[cfg(test)]\n    user_data: usize,\n    kept: u8,\n}\n"})
        self.assertEqual((counts["test"], counts["implementation"]), (2, 3))

    def test_a_file_production_also_reaches_stays_implementation(self):
        counts = self.counts({
            "core/a/benches/speed.rs": '#[path = "../src/shared.rs"]\nmod shared;\n',
            "core/a/src/lib.rs": "mod production;\n#[cfg(test)]\nmod validation;\n",
            "core/a/src/production.rs": '#[path = "shared.rs"]\nmod shared;\n',
            "core/a/src/validation.rs": '#[path = "shared.rs"]\nmod shared;\n',
            "core/a/src/shared.rs": self.lines(10),
        })
        self.assertEqual((counts["test"], counts["implementation"]), (6, 13))

    def test_grandchildren_of_an_out_of_line_test_module_are_test_code(self):
        after = {
            "core/a/src/lib.rs": "#[cfg(test)]\nmod checks;\n",
            "core/a/src/checks.rs": "mod a;\n",
            "core/a/src/checks/a.rs": "mod b;\n",
            "core/a/src/checks/a/b.rs": self.lines(413),
        }
        self.assertEqual(self.net({}, after), 417)

    def test_rust_files_under_fixtures_are_fixture_data(self):
        counts = self.counts({"core/a/tests/fixtures/template.rs": self.lines(5), "core/a/tests/flow.rs": self.lines(2)})
        self.assertEqual((counts["test"], counts["fixture"]), (2, 5))

    def test_a_path_override_on_an_inline_test_module_moves_its_children(self):
        after = {
            "core/a/src/lib.rs": '#[cfg(test)]\n#[path = "thread_files"]\nmod thread {\n    #[path = "tls.rs"]\n    mod local_data;\n}\n',
            "core/a/src/thread_files/tls.rs": self.lines(413),
        }
        self.assertEqual(self.net({}, after), 419)

    def test_a_module_after_a_test_module_on_the_same_line_is_implementation(self):
        after = {"core/a/src/lib.rs": "#[cfg(test)] mod tests {} mod production;\n", "core/a/src/production.rs": self.lines(413)}
        self.assertEqual(self.net({}, after), 1)

    def test_a_child_declared_after_a_tab_is_still_found(self):
        after = {
            "core/a/src/lib.rs": "#[cfg(test)]\nmod h;\n",
            "core/a/src/h.rs": "mod\tcases;\n",
            "core/a/src/h/cases.rs": self.lines(413),
        }
        self.assertEqual(self.net({}, after), 416)

    def test_the_pull_requests_own_record_covers_the_excess(self):
        record = "# A model of every resend order needs one case per order.\n600\n"
        after = {"core/a/tests/t.rs": self.lines(1000), ".github/test-line-allowances/pr-715.txt": record}
        self.assertEqual(self.run_check({}, after), [])

    def test_another_pull_requests_record_gives_no_credit(self):
        after = {"core/a/tests/t.rs": self.lines(450), ".github/test-line-allowances/pr-716.txt": "# Other.\n600\n"}
        warnings = self.run_check({}, after)
        self.assertEqual(len(warnings), 1, warnings)
        self.assertIn("Remove or consolidate 50 test lines, or add .github/test-line-allowances/pr-715.txt", warnings[0])

    def test_without_a_pull_request_number_any_new_record_counts(self):
        after = {"core/a/tests/t.rs": self.lines(450), ".github/test-line-allowances/pr-716.txt": "# Other.\n600\n"}
        self.assertEqual(self.run_check({}, after, pr=None), [])

    def test_a_record_already_on_the_base_gives_no_credit(self):
        before = {".github/test-line-allowances/pr-715.txt": "# Old reason.\n600\n", ".github/test-line-allowances/pr-1.txt": "200\n"}
        warnings = self.run_check(before, {"core/a/tests/t.rs": self.lines(1000)}, pr=None)
        self.assertEqual(len(warnings), 1, warnings)
        self.assertIn("Remove or consolidate 600 test lines", warnings[0])

    def test_a_moved_record_gives_no_credit(self):
        record = "# Earlier reason.\n13\n"
        before = {".github/test-line-allowances/pr-1.txt": record}
        after = {".github/test-line-allowances/pr-1.txt": None, ".github/test-line-allowances/pr-715.txt": record,
                 "core/a/tests/t.rs": self.lines(413)}
        warnings = self.run_check(before, after)
        self.assertEqual(len(warnings), 2, warnings)
        self.assertIn("pr-715.txt repeats a record already on the base", warnings[0])
        self.assertIn("Remove or consolidate 13 test lines", warnings[1])

    def test_raising_a_record_credits_only_the_raise(self):
        before = {".github/test-line-allowances/pr-715.txt": "# Model cases.\n100\n"}
        after = {"core/a/tests/t.rs": self.lines(600), ".github/test-line-allowances/pr-715.txt": "# Model cases.\n250\n"}
        warnings = self.run_check(before, after)
        self.assertEqual(len(warnings), 1, warnings)
        self.assertIn("plus 150 from records", warnings[0])
        self.assertIn("Remove or consolidate 50 test lines", warnings[0])

    def test_a_record_without_a_reason_a_count_or_a_pull_request_name_warns_and_gives_no_credit(self):
        after = {
            "core/a/tests/t.rs": self.lines(500),
            ".github/test-line-allowances/pr-715.txt": "200\n",
            ".github/test-line-allowances/resend-model.txt": "# Reason.\n200\n",
        }
        warnings = self.run_check({}, after)
        self.assertEqual(len(warnings), 3, warnings)
        self.assertIn("pr-715.txt grants 200 lines but gives no reason", warnings[0])
        self.assertIn("resend-model.txt gives no credit. Name a record after its pull request", warnings[1])
        self.assertIn("Remove or consolidate 100 test lines", warnings[2])

    def test_a_count_that_is_not_ascii_digits_warns(self):
        after = {"core/a/tests/t.rs": self.lines(500), ".github/test-line-allowances/pr-715.txt": "# Reason.\n\u00b2\n"}
        warnings = self.run_check({}, after)
        self.assertEqual(len(warnings), 2, warnings)
        self.assertIn("must hold exactly one non-negative integer", warnings[0])

if __name__ == "__main__":
    unittest.main()
