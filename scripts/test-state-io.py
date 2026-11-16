#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("check-state-io.py")
SPEC = importlib.util.spec_from_file_location("state_io", SCRIPT)
guard = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(guard)


def diagnostic(path="core/telemetry/src/store.rs", start=1, end=None, code=guard.IO_LINT):
    return {"code": {"code": code}, "spans": [{
        "is_primary": True, "file_name": path, "line_start": start, "line_end": end or start,
    }]}


class ChangedIo(unittest.TestCase):
    def introduced(self, before, after, **kwargs):
        item = diagnostic(**kwargs)
        return guard.introduced(item, {item["spans"][0]["file_name"]: (before, after)})

    def test_an_existing_call_is_not_a_new_violation(self):
        self.assertFalse(self.introduced("fs::write(path, bytes);", "fs::write(path, bytes);"))

    def test_a_new_call_fails(self):
        self.assertTrue(self.introduced("", "fs::write(path, bytes);"))

    def test_a_changed_argument_fails(self):
        self.assertTrue(self.introduced("fs::write(log, bytes);", "fs::write(state, bytes);"))

    def test_a_copied_call_fails(self):
        self.assertTrue(self.introduced("fs::write(path, bytes);", "fs::write(path, bytes);\nfs::write(path, bytes);", start=2))

    def test_a_multiline_call_with_one_new_line_fails(self):
        self.assertTrue(self.introduced("fs::write(\nlog, bytes);", "fs::write(\nstate, bytes);"))

    def test_multiline_call_matching_ignores_comments_but_not_string_contents(self):
        self.assertFalse(self.introduced('fs::write(\npath, b"(;");', 'fs::write(\n/* ) */ path, b"(;");'))
        self.assertTrue(self.introduced('fs::write(\npath, br#"(;)"#);', 'fs::write(\npath, br#"(;))"#);'))

    def test_shifted_existing_calls_are_not_new(self):
        self.assertFalse(self.introduced("fs::write(path, bytes);", "let ready = true;\nfs::write(path, bytes);", start=2))

    def test_changed_aliases_cannot_grandfather_a_call(self):
        self.assertTrue(self.introduced("use safe as io;\nio::write(path, bytes);", "use std::fs as io;\nio::write(path, bytes);", start=2))

    def test_a_test_writer_cannot_become_production_code_unchecked(self):
        self.assertTrue(self.introduced("#[cfg(test)]\nfn save() {\nfs::write(path, bytes);\n}", "\nfn save() {\nfs::write(path, bytes);\n}", start=3))

    def test_old_lock_violations_fail_too(self):
        self.assertTrue(self.introduced("", "", code="clippy::await_holding_lock"))
        self.assertTrue(self.introduced("", "", code="clippy::await_holding_refcell_ref"))

    def test_the_publication_helper_owns_raw_io(self):
        self.assertFalse(self.introduced("", "fs::rename(a, b);", path=guard.HELPER))

    def test_build_outputs_are_not_runtime_state(self):
        self.assertFalse(self.introduced("", "fs::write(a, b);", path="cli/build.rs"))

    def test_a_new_suppression_fails(self):
        self.assertEqual(list(guard.new_suppressions("", "#[allow(clippy::disallowed_methods)]")), [1])
        self.assertEqual(list(guard.new_suppressions("", "#[expect(\nclippy::await_holding_lock)]")), [1])

    def test_a_lint_group_cannot_hide_a_guardrail(self):
        for group in ["warnings", "clippy::all", "clippy::style", "clippy::suspicious"]:
            self.assertEqual(list(guard.new_suppressions("", f"#[allow({group})]")), [1])

    def test_an_existing_suppression_does_not_allow_a_duplicate(self):
        allow = "#[allow(clippy::disallowed_methods)]"
        self.assertEqual(list(guard.new_suppressions(allow, allow + "\n" + allow)), [2])


class CompilerProof(unittest.TestCase):
    def compile(self, name, lints):
        with tempfile.TemporaryDirectory() as output:
            return subprocess.run([
                "nice", "-n", "10", "clippy-driver", "--edition=2024", "--crate-type=lib",
                "--out-dir", output, str(guard.CONFIG / name), *lints,
            ], env=dict(os.environ, CLIPPY_CONF_DIR=str(guard.CONFIG)), capture_output=True, text=True)

    def test_past_573_write_and_rename_fail_with_the_replacement_named(self):
        result = self.compile("past-573.rs", ["-D", "clippy::disallowed_methods"])
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertIn("std::fs::write", result.stderr)
        self.assertIn("std::fs::rename", result.stderr)
        self.assertIn("koidrive_state_file::write", result.stderr)
        self.assertIn("koidrive_state_file::AtomicFile", result.stderr)

    def test_mutex_and_refcell_borrows_fail_across_await(self):
        result = self.compile("async-locks.rs", ["-D", "clippy::await_holding_lock", "-D", "clippy::await_holding_refcell_ref"])
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertIn("await_holding_lock", result.stderr)
        self.assertIn("await_holding_refcell_ref", result.stderr)


if __name__ == "__main__":
    unittest.main()
