#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import importlib.util
import json
import pathlib
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("guard", ROOT / "scripts/check-store-upgrades.py")
guard = importlib.util.module_from_spec(spec)
spec.loader.exec_module(guard)


class GuardTests(unittest.TestCase):
    def check(self, changes, harness="", cases=None, previous="", migrations=()):
        cases = cases or {}
        return guard.check(changes, harness, list(cases), lambda name: cases[name], previous, lambda name: name in migrations)

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
        with self.assertRaisesRegex(ValueError, "upgraded store"):
            self.check(changes, harness, {"volume_mounts": "upgrade_case!(volume_mounts, |store| { assert!(true); });"})

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
