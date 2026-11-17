#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import importlib.util
from collections import Counter
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch
from types import SimpleNamespace


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

    def test_an_unrelated_import_does_not_make_an_existing_call_new(self):
        before = "use tokio::sync::Mutex;\nfs::write(path, b\"paused\\n\");"
        after = "use tokio::sync::Mutex;\nuse tokio::io::AsyncWriteExt;\nfs::write(path, b\"paused\\n\");"
        self.assertFalse(self.introduced(before, after, start=3))
        call = "fs::write(path, bytes);"
        self.assertFalse(self.introduced(call, "use std::path::PathBuf;\n" + call, start=2))
        self.assertFalse(self.introduced(call, "use std::{collections::HashMap, path::{self as paths, Path}};\n" + call, start=2))

    def test_an_import_that_shadows_fs_or_tempfile_still_fails(self):
        call = "fs::write(path, bytes);"
        self.assertTrue(self.introduced("use std::fs;\n" + call, "use crate::io as fs;\n" + call, start=2))
        call = "tempfile::NamedTempFile::new_in(dir);"
        self.assertTrue(self.introduced(call, "use crate::scratch as tempfile;\n" + call, start=2))
        self.assertTrue(self.introduced("fs::write(path, bytes);", "use crate::io::*;\nfs::write(path, bytes);", start=2))

    def test_an_import_that_rebinds_a_name_in_the_call_still_fails(self):
        call = "io::write(path, bytes);"
        self.assertTrue(self.introduced("use crate::safe as io;\n" + call, "use crate::other as io;\n" + call, start=2))
        call = "std::fs::write(TARGET, bytes);"
        before = "use crate::paths::LOG as TARGET;\n" + call
        self.assertTrue(self.introduced(before, "use crate::paths::STATE as TARGET;\n" + call, start=2))
        chain = "use selected::TARGET;\n" + call
        before = "use crate::log_paths as selected;\n" + chain
        self.assertTrue(self.introduced(before, "use crate::state_paths as selected;\n" + chain, start=3))
        self.assertTrue(self.introduced("use std::{io, path};\n" + call, "use std::{fs as TARGET, path};\n" + call, start=2))
        for prefix in ("self::", "super::"):
            chain = f"use {prefix}selected::TARGET;\n" + call
            before = "use crate::log_paths as selected;\n" + chain
            self.assertTrue(self.introduced(before, "use crate::state_paths as selected;\n" + chain, start=3))
        raw = "std::fs::write(r#TARGET, bytes);"
        before = "use crate::paths::LOG as r#TARGET;\n" + raw
        self.assertTrue(self.introduced(before, "use crate::paths::STATE as r#TARGET;\n" + raw, start=2))

    def test_an_import_without_a_terminator_is_compared_not_fatal(self):
        call = "fs::write(path, bytes);\n"
        tests = "#[cfg(test)]\nmod tests { #[test] fn t() {} }"
        self.assertFalse(self.introduced(call + tests, call + "pub fn g() {}\n" + tests))
        self.assertTrue(self.introduced(call + "mod tests {}", call + "mod tests { use crate::io as fs; }"))

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

    def test_unchanged_macro_invocations_ignore_unrelated_edits_for_every_delimiter(self):
        for invocation in ["save!(path);", "save! { path }", "save![path];"]:
            item = diagnostic(path="core/driver/src/macros.rs")
            call = diagnostic(path="cli/src/service/command.rs")["spans"][0]
            call["is_primary"] = False
            item["spans"][0]["expansion"] = {"span": call}
            before = invocation + "\nlet other = 1;"
            after = invocation + "\nlet other = 2;"
            with self.subTest(invocation=invocation):
                self.assertFalse(guard.introduced(item, {call["file_name"]: (before, after)}))

    def test_macro_arguments_and_nested_delimiters_are_compared(self):
        for opening, closing in [("(", ")"), ("[", "]"), ("{", "}")]:
            before = f'save!{opening} [path, "}}]"] {closing}'
            after = f'save!{opening} [other, "}}]"] {closing}'
            self.assertEqual(guard.call_tokens(before, 1, 1), guard.call_tokens(before + "\nlet other = 1;", 1, 1))
            self.assertIsNotNone(guard.call_tokens(before, 1, 1))
            self.assertNotEqual(guard.call_tokens(before, 1, 1), guard.call_tokens(after, 1, 1))

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


class CompilerInventory(unittest.TestCase):
    def message(self, path="core/telemetry/src/store.rs", start=1, code=guard.IO_LINT):
        item = diagnostic(path=path, start=start, code=code)
        item["message"] = "use of a disallowed method `std::fs::write`"
        item["spans"][0].update(column_start=1, column_end=10)
        return {"message": item, "target": {"name": "telemetry", "kind": ["lib"], "src_path": "core/telemetry/src/lib.rs"}}

    def baseline(self, *messages):
        return Counter(guard.diagnostic_key(message) for message in messages)

    def test_unchanged_diagnostics_require_compiler_baseline_evidence(self):
        message = self.message()
        self.assertTrue(guard.compiler_introduced(message, {}, self.baseline()))
        self.assertFalse(guard.compiler_introduced(message, {}, self.baseline(message)))

    def test_another_call_in_the_same_file_is_not_evidence(self):
        self.assertTrue(guard.compiler_introduced(self.message(start=2), {}, self.baseline(self.message())))

    def test_a_changed_child_with_an_unrelated_edit_still_requires_evidence(self):
        message = self.message()
        files = {message["message"]["spans"][0]["file_name"]: ("fs::write(path, bytes);\nfn other() {}", "fs::write(path, bytes);\nfn other() { return; }")}
        self.assertTrue(guard.compiler_introduced(message, files, self.baseline()))
        self.assertFalse(guard.compiler_introduced(message, files, self.baseline(message)))

    def test_shifted_spans_match_their_original_compiler_locations(self):
        before = self.message()
        after = self.message(start=2)
        files = {before["message"]["spans"][0]["file_name"]: ("fs::write(path, bytes);", "fn other() {}\nfs::write(path, bytes);")}
        self.assertFalse(guard.compiler_introduced(after, files, self.baseline(before)))

    def test_source_changes_still_fail_even_with_a_matching_inventory_entry(self):
        message = self.message()
        for before, after in [
            ("fs::write(log, bytes);", "fs::write(state, bytes);"),
            ("use safe as fs;\nfs::write(path, bytes);", "use std::fs;\nfs::write(path, bytes);"),
            ("#[cfg(test)]\nfs::write(path, bytes);", "\nfs::write(path, bytes);"),
        ]:
            with self.subTest(before=before):
                files = {message["message"]["spans"][0]["file_name"]: (before, after)}
                self.assertTrue(guard.compiler_introduced(message, files, self.baseline(message)))

    def test_identity_includes_the_resolved_method_lint_target_and_columns(self):
        for change in ["method", "lint", "target", "column", "end"]:
            before, after = self.message(), self.message()
            if change == "method":
                after["message"]["message"] = "use of a disallowed method `std::fs::rename`"
            elif change == "lint":
                after["message"]["code"]["code"] = "clippy::await_holding_lock"
            elif change == "target":
                after["target"]["name"] = "other"
            else:
                after["message"]["spans"][0]["column_start" if change == "column" else "column_end"] += 1
            with self.subTest(change=change):
                self.assertNotEqual(guard.diagnostic_key(before), guard.diagnostic_key(after))

    def test_absolute_archive_paths_normalize_to_the_same_identity(self):
        message = self.message()
        absolute = self.message(path="/private/archive/core/telemetry/src/store.rs")
        absolute["target"]["src_path"] = "/private/archive/core/telemetry/src/lib.rs"
        self.assertEqual(guard.diagnostic_key(message), guard.diagnostic_key(absolute, root=Path("/private/archive")))

    def test_macro_identity_includes_every_invocation_in_the_chain(self):
        before = self.message(path="macros.rs")
        inner = diagnostic(path="wrapper.rs")["spans"][0]
        outer = diagnostic(path="caller.rs")["spans"][0]
        inner["expansion"] = {"span": outer, "macro_decl_name": "wrapper!"}
        before["message"]["spans"][0]["expansion"] = {"span": inner, "macro_decl_name": "write!"}
        after = json.loads(json.dumps(before))
        after["message"]["spans"][0]["expansion"]["span"]["expansion"]["span"].update(line_start=2, line_end=2)
        self.assertTrue(guard.compiler_introduced(after, {}, self.baseline(before)))
        self.assertFalse(guard.compiler_introduced(before, {}, self.baseline(before)))

    def test_shifted_macro_invocations_match_but_new_invocations_do_not(self):
        before = self.message(path="macros.rs")
        before["message"]["spans"][0]["expansion"] = {"span": diagnostic(path="caller.rs")["spans"][0]}
        after = json.loads(json.dumps(before))
        after["message"]["spans"][0]["expansion"]["span"].update(line_start=2, line_end=2)
        files = {"caller.rs": ("save!(path);", "fn other() {}\nsave!(path);")}
        self.assertFalse(guard.compiler_introduced(after, files, self.baseline(before)))
        files["caller.rs"] = ("save!(path);", "save!(path);\nsave!(path);")
        self.assertTrue(guard.compiler_introduced(after, files, self.baseline(before)))

    def test_inventory_entries_cannot_be_reused_for_duplicate_diagnostics(self):
        message = self.message()
        baseline = self.baseline(message)
        self.assertFalse(guard.compiler_introduced(message, {}, baseline))
        self.assertTrue(guard.compiler_introduced(message, {}, baseline))

    def test_helper_and_cargo_build_outputs_do_not_need_baseline_evidence(self):
        self.assertFalse(guard.compiler_introduced(self.message(path=guard.HELPER), {}, self.baseline()))
        build = self.message(path="cli/build.rs")
        build["target"]["kind"] = ["custom-build"]
        self.assertFalse(guard.compiler_introduced(build, {}, self.baseline()))
        build["target"]["kind"] = ["lib"]
        self.assertTrue(guard.compiler_introduced(build, {}, self.baseline()))

    def test_both_lock_lints_remain_errors_even_if_the_baseline_has_them(self):
        for code in guard.LOCK_LINTS:
            message = self.message(code=code)
            self.assertTrue(guard.compiler_introduced(message, {}, self.baseline(message)))
            message["target"]["kind"] = ["custom-build"]
            self.assertTrue(guard.compiler_introduced(message, {}, self.baseline(message)))

    def test_failed_or_incomplete_compilation_cannot_supply_a_baseline(self):
        for returncode, output in [
            (1, '{"reason":"build-finished","success":false}'),
            (1, '{"reason":"build-finished","success":true}'),
            (0, '{"reason":"build-finished","success":false}'),
            (0, ""),
        ]:
            with self.subTest(returncode=returncode, output=output):
                result = SimpleNamespace(returncode=returncode, stdout=output)
                with patch.object(guard.subprocess, "run", return_value=result):
                    with self.assertRaisesRegex(ValueError, "baseline"):
                        guard.compile_inventory(guard.ROOT, {}, "baseline")

    def test_compiler_inventory_normalizes_archive_paths_before_source_checks(self):
        message = self.message(path="/private/archive/core/telemetry/src/store.rs")
        message["reason"] = "compiler-message"
        message["message"]["level"] = "warning"
        message["target"]["src_path"] = "/private/archive/core/telemetry/src/lib.rs"
        output = json.dumps(message) + '\n{"reason":"build-finished","success":true}'
        with patch.object(guard.subprocess, "run", return_value=SimpleNamespace(returncode=0, stdout=output)):
            inventory = guard.compile_inventory(Path("/private/archive"), {}, "baseline")
        self.assertEqual(guard.diagnostic_key(inventory[0]), guard.diagnostic_key(self.message()))

    def test_baseline_failure_stops_before_the_head_build(self):
        with tempfile.TemporaryDirectory() as scratch:
            target = Path(scratch) / "target"
            with patch.dict(os.environ, {"CARGO_TARGET_DIR": str(target)}), \
                    patch.object(guard, "git", side_effect=[b"commit", b"", b"archive"]), \
                    patch.object(guard.subprocess, "check_output", return_value="current-toolchain"), \
                    patch.object(guard.subprocess, "run"), \
                    patch.object(guard, "compile_inventory", side_effect=ValueError("baseline failed")) as compile:
                with self.assertRaisesRegex(ValueError, "baseline failed"):
                    guard.check("HEAD")
                compile.assert_called_once()
                root, env, label = compile.call_args.args
                self.assertEqual(label, "baseline")
                self.assertNotEqual(root, guard.ROOT)
                self.assertEqual(Path(env["CARGO_TARGET_DIR"]), root.parent / "target")
                self.assertNotEqual(Path(env["CARGO_TARGET_DIR"]), target)
                self.assertEqual(env["CLIPPY_CONF_DIR"], str(guard.CONFIG))
                self.assertEqual(env["RUSTUP_TOOLCHAIN"], "current-toolchain")


class CompilationBoundary(unittest.TestCase):
    def compile(self, root):
        output = root / "target"
        output.mkdir()
        flags = [flag for lint in guard.FORCED_LINTS for flag in ("--force-warn", lint)]
        result = subprocess.run([
            "nice", "-n", "10", "clippy-driver", "--edition=2024", "--crate-type=lib",
            "--emit=metadata", "--error-format=json", "--out-dir", str(output), str(root / "lib.rs"), *flags,
        ], env=dict(os.environ, CLIPPY_CONF_DIR=str(guard.CONFIG)), capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        messages = []
        for line in result.stderr.splitlines():
            item = json.loads(line)
            if (item.get("code") or {}).get("code") not in guard.FORCED_LINTS:
                continue
            for chain in guard.span_chains(item):
                for span in chain:
                    span["file_name"] = guard.relative_path(span["file_name"], root)
            messages.append({"message": item, "target": {"name": "activation", "kind": ["lib"], "src_path": "lib.rs"}})
        return messages

    def test_parent_activation_requires_baseline_diagnostics_from_the_unchanged_child(self):
        with tempfile.TemporaryDirectory() as scratch:
            base, head = Path(scratch) / "base", Path(scratch) / "head"
            parent = (guard.CONFIG / "activation.rs").read_text()
            child = (guard.CONFIG / "activation-child.rs").read_text()
            for root, text in [(base, parent), (head, parent.replace("#[cfg(test)]\n", ""))]:
                root.mkdir()
                (root / "lib.rs").write_text(text)
                (root / "activation-child.rs").write_text(child)
            baseline = self.compile(base)
            current = self.compile(head)
            self.assertEqual(baseline, [])
            self.assertEqual({item["message"]["code"]["code"] for item in current}, set(guard.FORCED_LINTS))
            files = {"lib.rs": (parent, (head / "lib.rs").read_text())}
            for item in current:
                with self.subTest(code=item["message"]["code"]["code"]):
                    self.assertTrue(guard.compiler_introduced(item, files, Counter()))
            edited = Path(scratch) / "edited"
            edited.mkdir()
            (edited / "lib.rs").write_text((head / "lib.rs").read_text())
            edited_child = child + "\npub fn unrelated() {}\n"
            (edited / "activation-child.rs").write_text(edited_child)
            files["activation-child.rs"] = (child, edited_child)
            current = self.compile(edited)
            self.assertEqual(len(current), 4)
            for item in current:
                with self.subTest(edited_child=True, code=item["message"]["code"]["code"]):
                    self.assertTrue(guard.compiler_introduced(item, files, Counter()))

    def test_compiler_locations_survive_unrelated_child_edits_and_macro_expansion(self):
        with tempfile.TemporaryDirectory() as scratch:
            base, head = Path(scratch) / "base", Path(scratch) / "head"
            parent = (guard.CONFIG / "activation.rs").read_text().replace("#[cfg(test)]\n", "")
            child = (guard.CONFIG / "activation-child.rs").read_text()
            edited = "\n\n" + child + "\npub fn unrelated() {}\n"
            for root, text in [(base, child), (head, edited)]:
                root.mkdir()
                (root / "lib.rs").write_text(parent)
                (root / "activation-child.rs").write_text(text)
            baseline = Counter(guard.diagnostic_key(message) for message in self.compile(base))
            current = self.compile(head)
            files = {"activation-child.rs": (child, edited)}
            io = [item for item in current if item["message"]["code"]["code"] == guard.IO_LINT]
            self.assertEqual(len(io), 2)
            self.assertTrue(any(len(chain) >= 3 for item in io for chain in guard.span_chains(item["message"])))
            for item in io:
                self.assertFalse(guard.compiler_introduced(item, files, baseline))


if __name__ == "__main__":
    unittest.main()
