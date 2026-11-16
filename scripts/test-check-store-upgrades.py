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

    def check(self, changes, harness="", cases=None, previous="", migrations=()):
        cases = cases or {}
        harness = guard.canonical_harness((ROOT / guard.HARNESS).read_text()) + harness
        return guard.check(changes, harness, list(cases), lambda name: cases[name], previous, lambda name: name in migrations)

    def test_harness_cannot_skip_the_fixture(self):
        harness = (ROOT / guard.HARNESS).read_text()
        start = harness.index("            std::fs::copy(")
        end = harness.index("            if std::path::Path::new", start)
        mutated = harness[:start] + "            let mut $store = Store::open(&path).unwrap();\n" + harness[end:]
        mutated = mutated.replace("            assert_eq!(preserved_rows(&rusqlite::Connection::open(&path).unwrap()), preserved);\n", "")
        self.assertNotIn("std::fs::copy(", mutated)
        self.assertIn("let mut $store = Store::open(&path).unwrap();", mutated)
        with self.assertRaisesRegex(ValueError, "pinned release-state harness"):
            guard.validate_harness(mutated)

    def test_harness_only_accepts_new_top_level_includes(self):
        harness = (ROOT / guard.HARNESS).read_text()
        added = 'include!("upgrade_cases/new_migration.rs");\n'
        self.assertIn("new_migration", guard.validate_harness(harness + added))
        for mutation in ["/*\n" + harness + "*/\n", harness + "fn extra() {}\n",
                         harness + "/*\n" + added + "*/\n", harness.replace("#[test]", "#[ignore]\n        #[test]"),
                         harness + 'include!("other.rs");\n']:
            with self.subTest(mutation=mutation[-80:]), self.assertRaisesRegex(ValueError, "pinned release-state harness"):
                guard.validate_harness(mutation)

    def test_unrelated_test_edit_does_not_cover_a_migration(self):
        changes = [("A", "core/store/migrations/volume_mounts.sql"), ("M", "core/store/tests/store.rs")]
        with self.assertRaisesRegex(ValueError, "volume_mounts"):
            self.check(changes)

    def test_each_migration_needs_its_own_new_registered_case(self):
        changes = [("A", "core/store/migrations/volume_mounts.sql"), ("A", "core/store/tests/upgrade_cases/volume_mounts.rs")]
        harness = 'include!("upgrade_cases/volume_mounts.rs");\n'
        cases = {"volume_mounts": "upgrade_case!(volume_mounts, |store| { assert!(store.is_ok()); });"}
        self.assertEqual(self.check(changes, harness, cases), ["volume_mounts"])
        with self.assertRaisesRegex(ValueError, "include"):
            self.check(changes, "", cases)
        with self.assertRaisesRegex(ValueError, "exactly"):
            self.check(changes, harness, {"volume_mounts": "#[ignore] " + cases["volume_mounts"]})
        with self.assertRaisesRegex(ValueError, "duplicate"):
            self.check(changes, harness * 2, cases)

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

    def test_legacy_and_unrecognized_migration_additions_fail(self):
        for path in ["core/store/migrations/legacy/99_add.sql", "core/store/migrations/a-b.sql", "core/store/migrations/nested/new.sql"]:
            with self.subTest(path=path), self.assertRaisesRegex(ValueError, "named SQL"):
                self.check([("A", path)])

    def test_merged_migrations_cannot_change_or_disappear(self):
        for status in ["M", "D"]:
            with self.subTest(status=status), self.assertRaisesRegex(ValueError, "merged migrations"):
                self.check([(status, "core/store/migrations/unseen_changes.sql")])

    def test_existing_cases_are_validated_and_cannot_lose_coverage(self):
        harness = 'include!("upgrade_cases/unseen_changes.rs");\n'
        valid = {"unseen_changes": "upgrade_case!(unseen_changes, |store| { assert!(store.is_ok()); });"}
        self.assertEqual(self.check([], harness, valid), ["unseen_changes"])
        with self.assertRaisesRegex(ValueError, "exactly"):
            self.check([("M", "core/store/tests/upgrade_cases/unseen_changes.rs")], harness,
                       {"unseen_changes": "#[ignore] " + valid["unseen_changes"]})
        with self.assertRaisesRegex(ValueError, "include each"):
            self.check([], harness)
        with self.assertRaisesRegex(ValueError, "include each"):
            self.check([], "", valid)
        with self.assertRaisesRegex(ValueError, "cannot remove upgrade coverage"):
            self.check([], "", {}, previous=harness, migrations=["unseen_changes"])

    def test_all_registered_files_are_returned(self):
        harness = 'include!("upgrade_cases/z.rs");\ninclude!("upgrade_cases/a.rs");\n'
        cases = {name: f"upgrade_case!({name}, |store| {{ assert!(store.is_ok()); }});" for name in ["z", "a"]}
        self.assertEqual(self.check([], harness, cases), ["a", "z"])

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
