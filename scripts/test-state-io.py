#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import importlib.util
import json
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
    def introduced(self, before, after, build_script=False, **kwargs):
        item = diagnostic(**kwargs)
        return guard.introduced(item, {item["spans"][0]["file_name"]: (before, after)}, build_script)

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
        self.assertFalse(self.introduced("", "fs::write(a, b);", path="cli/build.rs", build_script=True))

    def test_a_runtime_module_named_build_rs_is_checked(self):
        self.assertTrue(self.introduced("", "fs::write(a, b);", path="core/driver/src/build.rs"))
        self.assertTrue(self.introduced("", "fs::write(a, b);", path="cli/build.rs"))

    def test_new_macro_invocations_cannot_grandfather_an_unchanged_definition(self):
        item = diagnostic(path="core/driver/src/macros.rs")
        call = diagnostic(path="cli/src/service/command.rs")["spans"][0]
        call["is_primary"] = False
        item["spans"][0]["expansion"] = {"span": call}
        self.assertTrue(guard.introduced(item, {call["file_name"]: ("", "save!(path);")}))

    def test_nested_macro_expansions_check_the_outer_invocation(self):
        item = diagnostic(path="core/driver/src/macros.rs")
        inner = diagnostic(path="core/driver/src/wrapper.rs")["spans"][0]
        outer = diagnostic(path="cli/src/service/command.rs")["spans"][0]
        inner["is_primary"] = outer["is_primary"] = False
        inner["expansion"] = {"span": outer}
        item["spans"][0]["expansion"] = {"span": inner}
        self.assertTrue(guard.introduced(item, {outer["file_name"]: ("", "save!(path);")}))

    def test_a_new_suppression_fails(self):
        self.assertEqual(list(guard.new_suppressions("", "#[allow(clippy::disallowed_methods)]")), [1])
        self.assertEqual(list(guard.new_suppressions("", "#[expect(\nclippy::await_holding_lock)]")), [1])

    def test_a_lint_group_cannot_hide_a_guardrail(self):
        for group in ["warnings", "clippy::all", "clippy::style", "clippy::suspicious"]:
            self.assertEqual(list(guard.new_suppressions("", f"#[allow({group})]")), [1])

    def test_an_existing_suppression_does_not_allow_a_duplicate(self):
        allow = "#[allow(clippy::disallowed_methods)]"
        self.assertEqual(list(guard.new_suppressions(allow, allow + "\n" + allow)), [2])

    def test_deprecated_lint_aliases_are_suppressions_too(self):
        self.assertEqual(list(guard.new_suppressions("", "#[allow(clippy::disallowed_method)]")), [1])

    def test_comments_cannot_end_a_suppression_early(self):
        self.assertEqual(list(guard.new_suppressions("", "#[allow(/* ) */ clippy::disallowed_methods)]")), [1])
        self.assertEqual(list(guard.new_suppressions("", "#[cfg_attr(any(), allow(/* ) */ clippy::disallowed_method))]")), [1])

    def test_nested_cfg_attr_suppressions_are_checked(self):
        text = '#[cfg_attr(feature = "nested", cfg_attr(unix, expect(clippy::await_holding_lock)))]'
        self.assertEqual(list(guard.new_suppressions("", text)), [1])

    def test_rust_strings_comments_and_expect_methods_are_not_attributes(self):
        for text in [
            'value.expect("clippy::all");',
            'value.expect("warnings");',
            '// allow(clippy::disallowed_methods)\nfn run() {}',
            'let message = r#"#[allow(clippy::all)]"#;',
            '#[doc = "#[allow(clippy::all)]"] fn run() {}',
        ]:
            self.assertEqual(list(guard.new_suppressions("", text)), [], text)

    def test_the_lifecycle_exception_pins_the_exact_function_and_method(self):
        policy = guard.LIFECYCLE_EXCEPTION
        text = (guard.ROOT / policy["file"]).read_text()
        item = diagnostic(path=policy["file"], start=policy["line"])
        item["message"] = "use of a disallowed method `std::fs::rename`"
        self.assertFalse(guard.introduced(item, {policy["file"]: (text, text)}))
        changed = text.replace('with_extension("json.tmp")', 'with_extension("other.tmp")', 1)
        self.assertNotEqual(text, changed)
        self.assertTrue(guard.introduced(item, {policy["file"]: (text, changed)}))
        item["message"] = "use of a disallowed method `std::fs::write`"
        self.assertTrue(guard.introduced(item, {policy["file"]: (text, text)}))

    def test_state_io_gates_the_required_ci_report(self):
        workflow = (guard.ROOT / ".github/workflows/ci.yml").read_text()
        job = workflow.split("\n  state-io:\n", 1)[1].split("\n  ci-report:\n", 1)[0]
        report = workflow.split("\n  ci-report:\n", 1)[1]
        self.assertIn("state-io", report.split("needs: [", 1)[1].split("]", 1)[0].split(", "))
        self.assertIn("nice -n 10 python3 scripts/test-state-io.py -v", job)
        self.assertIn('nice -n 10 python3 scripts/check-state-io.py "$BASE"', job)
        self.assertIn("BASE=$(git rev-parse HEAD^1)", job)
        self.assertIn("merge_group", job)
        self.assertIn("--force-warn", SCRIPT.read_text())
        self.assertIn("koidrive-cli/test-install-inventory,koidrive-cli/test-setup", SCRIPT.read_text())


class CompilerProof(unittest.TestCase):
    def compile(self, name, lints, dependencies=()):
        with tempfile.TemporaryDirectory() as output:
            arguments = []
            if dependencies:
                checked = subprocess.run([
                    "nice", "-n", "10", "cargo", "check", "--locked", "--message-format=json",
                    *[argument for name in dependencies for argument in ("-p", name)],
                ], cwd=guard.ROOT, capture_output=True, text=True)
                self.assertEqual(checked.returncode, 0, checked.stderr)
                artifacts = [json.loads(line) for line in checked.stdout.splitlines()]
                found = set()
                directories = set()
                for artifact in artifacts:
                    if artifact.get("reason") != "compiler-artifact":
                        continue
                    for filename in artifact["filenames"]:
                        if filename.endswith(".rmeta"):
                            directories.add(str(Path(filename).parent))
                            if artifact["target"]["name"] in dependencies:
                                dependency = artifact["target"]["name"]
                                found.add(dependency)
                                arguments.extend(["--extern", f"{dependency}={filename}"])
                self.assertEqual(found, set(dependencies))
                for directory in sorted(directories):
                    arguments.extend(["-L", f"dependency={directory}"])
            return subprocess.run([
                "nice", "-n", "10", "clippy-driver", "--edition=2024", "--crate-type=lib",
                "--emit=metadata", "--out-dir", output, str(guard.CONFIG / name), *arguments, *lints,
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

    def test_previous_service_persistence_and_alternate_persist_methods_fail(self):
        result = self.compile("past-persist.rs", ["-D", "clippy::disallowed_methods"], ("tempfile",))
        self.assertNotEqual(result.returncode, 0, result.stderr)
        for method in ["NamedTempFile::persist", "NamedTempFile::persist_noclobber", "TempPath::persist", "TempPath::persist_noclobber"]:
            self.assertIn(method, result.stderr)
        self.assertIn("koidrive_state_file::AtomicFile", result.stderr)
        self.assertNotIn("error[E", result.stderr)

    def test_macro_supplied_lint_levels_cannot_hide_compiler_diagnostics(self):
        ordinary_flags = ["--error-format=json", "-W", guard.IO_LINT]
        for lint in sorted(guard.LOCK_LINTS):
            ordinary_flags.extend(["-D", lint])
        ordinary = self.compile("macro-levels.rs", ordinary_flags)
        self.assertEqual(ordinary.returncode, 0, ordinary.stderr)
        hidden = [json.loads(line) for line in ordinary.stderr.splitlines()]
        self.assertFalse([item for item in hidden if (item.get("code") or {}).get("code") in guard.FORCED_LINTS])
        flags = ["--error-format=json"]
        for lint in guard.FORCED_LINTS:
            flags.extend(["--force-warn", lint])
        result = self.compile("macro-levels.rs", flags)
        self.assertEqual(result.returncode, 0, result.stderr)
        diagnostics = [json.loads(line) for line in result.stderr.splitlines()]
        violations = [item for item in diagnostics if (item.get("code") or {}).get("code") in guard.FORCED_LINTS]
        self.assertEqual({item["code"]["code"] for item in violations}, set(guard.FORCED_LINTS))
        source = (guard.CONFIG / "macro-levels.rs").read_text()
        for item in violations:
            self.assertEqual(item["level"], "warning")
            path = item["spans"][0]["file_name"]
            self.assertTrue(guard.introduced(item, {path: ("", source)}))


if __name__ == "__main__":
    unittest.main()
