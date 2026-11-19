#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import importlib.util
import json
import pathlib
import re
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("guard", ROOT / "scripts/check-store-upgrades.py")
guard = importlib.util.module_from_spec(spec)
spec.loader.exec_module(guard)


class GuardTests(unittest.TestCase):
    def test_upgrade_job_gates_the_existing_required_ci_report(self):
        workflow = (ROOT / ".github/workflows/ci.yml").read_text()
        self.assertIn("\n  store-upgrade:\n", workflow)
        report = workflow.split("\n  ci-report:\n", 1)[1]
        needs = re.search(r"^    needs: \[([^]]+)\]$", report, re.M)
        self.assertIsNotNone(needs, "ci-report must declare its required jobs")
        self.assertIn("store-upgrade", {name.strip() for name in needs.group(1).split(",")})

    def check(self, changes, cases=None, migrations=(), harness=None):
        cases = cases or {}
        harness = (ROOT / guard.HARNESS).read_text() if harness is None else harness
        return guard.check(changes, harness, list(cases), lambda name: cases[name], lambda name: name in migrations)

    def test_harness_cannot_skip_the_fixture(self):
        harness = (ROOT / guard.HARNESS).read_text()
        start = harness.index("            std::fs::copy(", harness.index("macro_rules! upgrade_case"))
        end = harness.index("            if std::path::Path::new", start)
        mutated = harness[:start] + "            let mut $store = Store::open(&path).unwrap();\n" + harness[end:]
        mutated = mutated.replace("            assert_eq!(preserved_rows(&rusqlite::Connection::open(&path).unwrap()), preserved);\n", "")
        self.assertNotIn("std::fs::copy(", mutated[mutated.index("macro_rules! upgrade_case"):])
        self.assertIn("let mut $store = Store::open(&path).unwrap();", mutated)
        with self.assertRaisesRegex(ValueError, "pinned release-state harness"):
            guard.validate_harness(mutated)

    def test_guard_runs_the_additive_columns_regression(self):
        self.assertIn("additive_columns_do_not_change_preserved_release_rows", guard.HARNESS_TESTS)

    def test_harness_is_pinned_whole(self):
        harness = (ROOT / guard.HARNESS).read_text()
        guard.validate_harness(harness)
        for mutation in ["/*\n" + harness + "*/\n", harness + "fn extra() {}\n",
                         harness + 'include!("upgrade_cases/new_migration.rs");\n',
                         harness.replace("#[test]", "#[ignore]\n        #[test]"),
                         harness.replace('"/upgrade_cases.rs"', '"/other.rs"')]:
            with self.subTest(mutation=mutation[-80:]), self.assertRaisesRegex(ValueError, "pinned release-state harness"):
                guard.validate_harness(mutation)

    def test_unrelated_test_edit_does_not_cover_a_migration(self):
        changes = [("A", "core/store/migrations/volume_mounts.sql"), ("M", "core/store/tests/store.rs")]
        with self.assertRaisesRegex(ValueError, "volume_mounts"):
            self.check(changes)

    def test_each_migration_needs_its_own_new_registered_case(self):
        changes = [("A", "core/store/migrations/volume_mounts.sql"), ("A", "core/store/tests/upgrade_cases/volume_mounts.rs")]
        cases = {"volume_mounts": "upgrade_case!(volume_mounts, |store| { assert!(store.is_ok()); });"}
        self.assertEqual(self.check(changes, cases), ["volume_mounts"])
        with self.assertRaisesRegex(ValueError, "exactly"):
            self.check(changes, {"volume_mounts": "#[ignore] " + cases["volume_mounts"]})
        with self.assertRaisesRegex(ValueError, "volume_mounts"):
            self.check(changes[:1], {"other": cases["volume_mounts"].replace("volume_mounts", "other")})

    def test_case_rejects_top_level_macro_calls_and_redefinitions(self):
        valid = "upgrade_case!(volume_mounts, |store| { assert!(store.is_ok()); });"
        for extra in [valid, "macro_rules! upgrade_case { ($($t:tt)*) => {}; }" + valid, "fn extra() {}"]:
            with self.subTest(extra=extra), self.assertRaisesRegex(ValueError, "exactly"):
                guard.validate_case("volume_mounts", valid + extra)
        for invalid in ["#[ignore] " + valid, valid.replace("|store|", "|other|"),
                        valid.replace("is_ok()", "is_ok(]"), valid[:-3]]:
            with self.subTest(invalid=invalid), self.assertRaisesRegex(ValueError, "exactly"):
                guard.validate_case("volume_mounts", invalid)

    def test_case_balancing_skips_comments_and_string_delimiters(self):
        guard.validate_case("example", '''
            // SPDX-License-Identifier: Apache-2.0
            upgrade_case!(example, |store| {
                /* } ); macro_rules! upgrade_case { /* nested */ } */
                let braces = ["}", r#"}); upgrade_case!(fake, |store| {"#, '}'];
                assert_eq!(store.value(), (braces, { 1 }));
            });
        ''')

    def test_schema_source_requires_new_case(self):
        for path in ["core/store/src/lib.rs", "core/store/src/migrations.rs", "core/store/src/new_schema.rs", "core/store/build/plan.rs", "core/store/build/nested/helpers.rs"]:
            with self.subTest(path=path), self.assertRaisesRegex(ValueError, "schema_"):
                self.check([("M", path), ("M", "core/store/tests/upgrade_cases/schema_old.rs")])
        self.check([("M", "core/store/src/lib.rs"), ("A", "core/store/tests/upgrade_cases/schema_new.rs")],
                   {"schema_new": "upgrade_case!(schema_new, |store| { assert!(store.is_ok()); });"})

    def test_legacy_and_unrecognized_migration_additions_fail(self):
        for path in ["core/store/migrations/legacy/99_add.sql", "core/store/migrations/a-b.sql", "core/store/migrations/nested/new.sql"]:
            with self.subTest(path=path), self.assertRaisesRegex(ValueError, "named SQL"):
                self.check([("A", path)])

    def test_merged_migrations_cannot_change_or_disappear(self):
        for status in ["M", "D"]:
            with self.subTest(status=status), self.assertRaisesRegex(ValueError, "merged migrations"):
                self.check([(status, "core/store/migrations/unseen_changes.sql")])

    def test_existing_cases_are_validated_and_cannot_lose_coverage(self):
        valid = {"unseen_changes": "upgrade_case!(unseen_changes, |store| { assert!(store.is_ok()); });"}
        self.assertEqual(self.check([], valid), ["unseen_changes"])
        with self.assertRaisesRegex(ValueError, "exactly"):
            self.check([("M", "core/store/tests/upgrade_cases/unseen_changes.rs")],
                       {"unseen_changes": "#[ignore] " + valid["unseen_changes"]})
        removal = [("D", "core/store/tests/upgrade_cases/unseen_changes.rs")]
        with self.assertRaisesRegex(ValueError, "cannot remove upgrade coverage"):
            self.check(removal, migrations=["unseen_changes"])
        self.assertEqual(self.check(removal), [])

    def test_all_case_files_are_returned(self):
        cases = {name: f"upgrade_case!({name}, |store| {{ assert!(store.is_ok()); }});" for name in ["z", "a"]}
        self.assertEqual(self.check([], cases), ["a", "z"])

    def test_real_569_diff_is_rejected(self):
        fixture = json.loads((ROOT / "scripts/test-fixtures/store-upgrade-569.json").read_text())
        self.assertEqual(fixture["base"], "cf69c8ad2d12afd2e7f3f9f2baed8aa4c73ec026")
        self.assertEqual(fixture["head"], "7dc030c1c0b055ab941064926fe4683087d8caee")
        changes = [tuple(change) for change in fixture["changes"]]
        self.assertIn(("M", "core/store/tests/store.rs"), changes)
        with self.assertRaisesRegex(ValueError, "volume_mounts"):
            self.check(changes)


if __name__ == "__main__":
    unittest.main(verbosity=2)
