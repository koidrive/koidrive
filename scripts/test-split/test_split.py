#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location("split_tests", Path(__file__).parents[1] / "split-tests.py")
sys.dont_write_bytecode = True
split = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(split)


class SplitTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.executable = split.parser()

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.addCleanup(patch.stopall)
        patch.object(split, "ROOT", self.root).start()
        patch.object(split, "MAPPING", self.root / "mapping.json").start()
        self.source = (
            split.LICENSE
            + b'const WORD: &str = "\xc3\x9f";\n\n'
            + b'fn helper() -> usize { 1 }\n\n'
            + b'#[test]\n#[ignore]\nfn one() { assert_eq!(helper(), 1); }\n'
        )
        self.mapping = {"tests.rs": {"const:WORD": "common", "fn:helper": "common", "fn:one": "scenario"}}
        self.path = self.root / "tests.rs"
        self.path.write_bytes(self.source)

    def generate(self):
        split.write(self.executable, split.render(self.executable, self.mapping, {"tests.rs": self.source}))

    def test_unicode_offsets_are_byte_offsets(self):
        found = split.items(self.executable, self.source)
        word = found[0]
        self.assertEqual(self.source[word["start"]:word["end"]], b'const WORD: &str = "\xc3\x9f";')

    def test_split_is_byte_identical_and_repeatable(self):
        self.generate()
        first = {path: path.read_bytes() for path in self.root.rglob("*.rs")}
        mtimes = {path: path.stat().st_mtime_ns for path in first}
        with patch.object(split, "git_source", return_value=self.source):
            split.verify(self.executable, self.mapping, "base")
            split.split(self.executable, self.mapping, "base")
        split.split(self.executable, self.mapping, None)
        self.assertEqual(first, {path: path.read_bytes() for path in self.root.rglob("*.rs")})
        self.assertEqual(mtimes, {path: path.stat().st_mtime_ns for path in first})

    def test_a_source_without_a_header_starts_with_its_includes(self):
        self.source = self.source.removeprefix(split.LICENSE)
        self.generate()
        self.assertTrue(self.path.read_bytes().startswith(b'include!("tests/common.rs");'))
        with patch.object(split, "git_source", return_value=self.source):
            split.verify(self.executable, self.mapping, "base")

    def test_unmapped_items_do_not_write(self):
        del self.mapping["tests.rs"]["fn:one"]
        with self.assertRaisesRegex(ValueError, "assign these items"):
            split.render(self.executable, self.mapping, {"tests.rs": self.source})
        self.assertEqual(self.path.read_bytes(), self.source)

    def test_changed_body_fails_identity(self):
        self.generate()
        path = self.root / "tests/scenario.rs"
        path.write_bytes(path.read_bytes().replace(b"helper(), 1", b"helper(), 2"))
        with patch.object(split, "git_source", return_value=self.source):
            with self.assertRaisesRegex(ValueError, "changed=.*fn:one"):
                split.verify(self.executable, self.mapping, "base")

    def test_missing_include_fails_identity(self):
        self.generate()
        self.path.write_bytes(self.path.read_bytes().replace(b'include!("tests/scenario.rs");', b""))
        with patch.object(split, "git_source", return_value=self.source):
            with self.assertRaisesRegex(ValueError, "include list"):
                split.verify(self.executable, self.mapping, "base")

    def test_commented_include_fails_identity(self):
        self.generate()
        self.path.write_bytes(self.path.read_bytes().replace(
            b'include!("tests/scenario.rs");', b'// include!("tests/scenario.rs");'))
        with patch.object(split, "git_source", return_value=self.source):
            with self.assertRaisesRegex(ValueError, "include list"):
                split.verify(self.executable, self.mapping, "base")

    def test_conditional_include_is_rejected(self):
        self.generate()
        self.path.write_bytes(self.path.read_bytes().replace(
            b'include!("tests/scenario.rs");', b'#[cfg(any())]\ninclude!("tests/scenario.rs");'))
        with patch.object(split, "git_source", return_value=self.source):
            with self.assertRaisesRegex(ValueError, "must be unconditional"):
                split.verify(self.executable, self.mapping, "base")

    def test_include_text_in_a_string_is_not_expanded(self):
        source = b'const TEXT: &str = r#"include!("tests/missing.rs");"#;\n'
        self.path.write_bytes(source)
        self.assertEqual(split.load_source(self.executable, "tests.rs"), source)

    def test_module_cfg_cannot_disable_the_suite(self):
        self.generate()
        self.path.write_bytes(b'#![cfg(any())]\n' + self.path.read_bytes())
        with patch.object(split, "git_source", return_value=self.source):
            with self.assertRaisesRegex(ValueError, "module header"):
                split.verify(self.executable, self.mapping, "base")

    def test_scenario_include_is_rejected(self):
        self.generate()
        path = self.root / "tests/scenario.rs"
        path.write_bytes(path.read_bytes() + b'\ninclude!("common.rs");\n')
        with patch.object(split, "git_source", return_value=self.source):
            with self.assertRaisesRegex(ValueError, "scenario fragments cannot include"):
                split.verify(self.executable, self.mapping, "base")

    def test_scenario_inner_attributes_are_rejected(self):
        self.generate()
        path = self.root / "tests/scenario.rs"
        path.write_bytes(b'#![cfg(any())]\n' + path.read_bytes())
        with patch.object(split, "git_source", return_value=self.source):
            with self.assertRaisesRegex(ValueError, "scenario fragments cannot have inner attributes"):
                split.verify(self.executable, self.mapping, "base")

    def test_unreferenced_nested_rust_file_is_rejected(self):
        self.generate()
        path = self.root / "tests/nested/extra.rs"
        path.parent.mkdir()
        path.write_bytes(b'#[test]\nfn hidden_added() {}\n')
        with patch.object(split, "git_source", return_value=self.source):
            with self.assertRaisesRegex(ValueError, "scenario file set"):
                split.verify(self.executable, self.mapping, "base")

    def test_test_lists_keep_duplicates(self):
        old, new = self.root / "old", self.root / "new"
        old.write_text("one: test\none: test\n")
        new.write_text("one: test\n")
        with self.assertRaisesRegex(ValueError, "inventory differs"):
            split.compare_lists(old, new)
        new.write_text(old.read_text())
        split.compare_lists(old, new)

    def test_port_moves_changed_and_added_tests(self):
        self.generate()
        branch = self.source.replace(b"helper(), 1", b"helper(), 2") + b"\n#[test]\nfn two() {}\n"
        self.mapping["tests.rs"]["fn:two"] = "scenario"
        with patch.object(split, "git_source", side_effect=lambda ref, path: self.source if ref == "base" else branch):
            split.port(self.executable, self.mapping, "base", "branch")
        with patch.object(split, "git_source", return_value=branch):
            split.verify(self.executable, self.mapping, "branch")

    def test_port_preserves_modules_in_root(self):
        for module in [
            b'#[path = "other_tests.rs"]\nmod telemetry;',
            b'#[cfg(test)]\nmod telemetry { #[test] fn nested() {} }',
        ]:
            with self.subTest(module=module):
                self.generate()
                branch = self.source + b"\n" + module + b"\n"
                self.mapping["tests.rs"]["mod:telemetry"] = "root"
                with patch.object(split, "git_source", side_effect=lambda ref, path: self.source if ref == "base" else branch):
                    split.port(self.executable, self.mapping, "base", "branch")
                self.assertIn(module, self.path.read_bytes())
                with patch.object(split, "git_source", return_value=branch):
                    split.verify(self.executable, self.mapping, "branch")

    def test_modules_cannot_move_to_scenarios(self):
        source = self.source + b'\n#[path = "other_tests.rs"]\nmod telemetry;\n'
        self.mapping["tests.rs"]["mod:telemetry"] = "common"
        with self.assertRaisesRegex(ValueError, "keep modules in root"):
            split.render(self.executable, self.mapping, {"tests.rs": source})
        self.assertEqual(self.path.read_bytes(), self.source)

    def test_port_refuses_overlapping_edits_without_writes(self):
        self.generate()
        scenario = self.root / "tests/scenario.rs"
        scenario.write_bytes(scenario.read_bytes().replace(b"helper(), 1", b"helper(), 3"))
        before = scenario.read_bytes()
        branch = self.source.replace(b"helper(), 1", b"helper(), 2")
        with patch.object(split, "git_source", side_effect=lambda ref, path: self.source if ref == "base" else branch):
            with self.assertRaisesRegex(ValueError, "overlapping edits"):
                split.port(self.executable, self.mapping, "base", "branch")
        self.assertEqual(scenario.read_bytes(), before)

    def test_port_removes_an_empty_scenario(self):
        self.generate()
        branch = self.source[:self.source.index(b"#[test]")]
        with patch.object(split, "git_source", side_effect=lambda ref, path: self.source if ref == "base" else branch):
            split.port(self.executable, self.mapping, "base", "branch")
        self.assertFalse((self.root / "tests/scenario.rs").exists())
        with patch.object(split, "git_source", return_value=branch):
            split.verify(self.executable, self.mapping, "branch")

    def property_source(self, base, path=b"proptest", config=b""):
        return base + path + b"! {\n" + config + b"    #[test]\n    fn holds(x in 0..4u8) { assert!(x < 4); }\n}\n"

    def block(self, *functions):
        config = b'    #![proptest_config(koidrive_test_support::battery_config(concat!(file!(), ".regressions.txt"), 1))]\n'
        return b"proptest::proptest! {\n" + config + b"".join(
            b"    #[test]\n    fn " + name + b"(x in 0..4u8) { assert!(x < 4); }\n" for name in functions) + b"}\n"

    def against(self, base):
        def source(ref, path):
            if path not in base:
                raise subprocess.CalledProcessError(128, ["git", "show", f"{ref}:{path}"])
            return base[path]
        return patch.object(split, "git_source", side_effect=source)

    def layout(self):
        return {path.relative_to(self.root).as_posix(): path.read_bytes() for path in self.root.rglob("*.rs")}

    def map_against(self, base):
        with self.against(base):
            split.map_items(self.executable, self.mapping, "base")

    def test_proptest_blocks_split_port_and_verify_in_root(self):
        original = self.source
        for path in [b"proptest", b"proptest::proptest"]:
            with self.subTest(path=path):
                self.source = self.property_source(original, path)
                self.mapping["tests.rs"]["proptest:holds"] = "root"
                self.generate()
                self.assertIn(path + b"! {", self.path.read_bytes())
                with patch.object(split, "git_source", return_value=self.source):
                    split.verify(self.executable, self.mapping, "base")
                branch = self.source.replace(b"x < 4", b"x <= 3")
                with patch.object(split, "git_source", side_effect=lambda ref, path: self.source if ref == "base" else branch):
                    split.port(self.executable, self.mapping, "base", "branch")
                with patch.object(split, "git_source", return_value=branch):
                    split.verify(self.executable, self.mapping, "branch")

    def test_verify_names_items_the_mapping_lacks(self):
        self.generate()
        del self.mapping["tests.rs"]["fn:one"]
        with patch.object(split, "git_source", return_value=self.source):
            with self.assertRaisesRegex(ValueError, r"assign these items.*fn:one"):
                split.verify(self.executable, self.mapping, "base")

    def test_verify_names_items_a_scenario_adds_without_a_mapping(self):
        self.generate()
        scenario = self.root / "tests/scenario.rs"
        scenario.write_bytes(scenario.read_bytes() + b"\n#[test]\nfn added() {}\n")
        with patch.object(split, "git_source", return_value=self.source):
            with self.assertRaisesRegex(ValueError, r"assign these items.*fn:added"):
                split.verify(self.executable, self.mapping, "base")

    def test_map_assigns_an_added_test_to_the_file_that_holds_it(self):
        self.generate()
        scenario = self.root / "tests/scenario.rs"
        scenario.write_bytes(scenario.read_bytes() + b"\n#[test]\nfn added() {}\n")
        with self.assertRaisesRegex(ValueError, r"fn:added.*split-tests\.py map"):
            split.verify(self.executable, self.mapping, None)
        self.mapping["tests.rs"]["fn:removed"] = "scenario"
        self.map_against({})
        written = json.loads(split.MAPPING.read_text())["tests.rs"]
        self.assertEqual(written["fn:added"], "scenario")
        self.assertNotIn("fn:removed", written)
        split.verify(self.executable, self.mapping, None)

    def test_check_format_covers_only_the_lines_a_change_wrote(self):
        self.generate()
        common = self.root / "tests/common.rs"
        self.assertTrue(split.unformatted_lines("tests/common.rs", common.read_bytes()))
        common.write_bytes(common.read_bytes() + b"\nfn added() {}\n")
        split.check_format(self.executable, {"tests/common.rs": {len(common.read_bytes().splitlines())}}, None)
        scenario = self.root / "tests/scenario.rs"
        scenario.write_bytes(scenario.read_bytes() + b"\nfn  spaced(){}\n")
        spaced = len(scenario.read_bytes().splitlines())
        split.check_format(self.executable, {"tests.rs": {1}, "tests/scenario.rs": {1}}, None)
        with self.assertRaisesRegex(ValueError, rf"tests/scenario\.rs lines \[{spaced}\].*rustfmt --edition 2024 --emit stdout"):
            split.check_format(self.executable, {"tests/scenario.rs": {spaced}}, None)

    def test_changed_lines_reads_file_headers_only_outside_hunks(self):
        diff = ("diff --git a/x.rs b/x.rs\n--- a/x.rs\n+++ b/x.rs\n@@ -0,0 +1,2 @@\n+a\n+++ expected\n"
                "@@ -5 +7 @@\n+fn  spaced(){}\n")
        self.assertEqual(split.changed_lines(diff), {"x.rs": {1, 2, 7}})

    def test_check_format_names_a_fragment_rustfmt_cannot_parse(self):
        self.generate()
        scenario = self.root / "tests/scenario.rs"
        scenario.write_bytes(scenario.read_bytes() + b"\nfn broken( {\n")
        with self.assertRaisesRegex(ValueError, r"rustfmt cannot format tests/scenario\.rs"):
            split.check_format(self.executable, {"tests/scenario.rs": {1}}, None)

    def test_verify_checks_only_sources_still_one_file_at_the_revision(self):
        self.generate()
        scenario = self.root / "tests/scenario.rs"
        scenario.write_bytes(scenario.read_bytes() + b"\n#[test]\nfn added() {}\n")
        with patch.object(split, "git_source", return_value=self.path.read_bytes()):
            self.assertEqual(split.pending(self.executable, self.mapping, "base"), {})
        with patch.object(split, "git_source", return_value=self.source):
            pending = split.pending(self.executable, self.mapping, "base")
            with self.assertRaisesRegex(ValueError, r"assign these items.*fn:added"):
                split.verify(self.executable, pending, "base")
        with self.against({}):
            self.assertEqual(split.pending(self.executable, self.mapping, "base"), self.mapping)

    def test_verify_refuses_a_split_this_change_makes_without_a_mapping(self):
        self.generate()
        with self.against({"tests.rs": self.source, "other.rs": b'include!("other/scenario.rs");\n'}):
            with self.assertRaisesRegex(ValueError, r"this change splits \['tests\.rs'\] without a mapping"):
                split.require_mapped_splits(self.executable, {}, "base", ["tests.rs"])
            split.require_mapped_splits(self.executable, self.mapping, "base", ["tests.rs"])
            (self.root / "other.rs").write_bytes(b'include!("other/scenario.rs");\n')
            split.require_mapped_splits(self.executable, {}, "base", ["other.rs"])

    def test_map_drops_an_entry_whose_source_is_already_split_at_the_base(self):
        self.generate()
        (self.root / "other.rs").write_bytes(split.LICENSE + b"#[test]\nfn kept() {}\n")
        self.mapping["other.rs"] = {}
        self.map_against(self.layout())
        self.assertEqual(json.loads(split.MAPPING.read_text()), {"other.rs": {"fn:kept": "root"}})

    def test_check_format_covers_fragments_of_every_split_source_and_nothing_else(self):
        self.generate()
        scenario = self.root / "tests/scenario.rs"
        scenario.write_bytes(scenario.read_bytes() + b"\nfn  spaced(){}\n")
        spaced = len(scenario.read_bytes().splitlines())
        module = self.root / "plain/child.rs"
        module.parent.mkdir()
        module.write_bytes(b"fn  spaced(){}\n")
        (self.root / "plain.rs").write_bytes(b'mod child;\ninclude!(concat!(env!("OUT_DIR"), "/generated.rs"));\n')
        changed = {"tests/scenario.rs": {spaced}, "plain/child.rs": {1}}
        self.assertEqual(split.fragments(self.executable, changed, None), ["tests/scenario.rs"])
        with self.assertRaisesRegex(ValueError, r"lines \[%d\]" % spaced):
            split.check_format(self.executable, changed, None)

    def test_map_names_the_fetch_when_origin_main_is_missing(self):
        split.MAPPING.write_text(json.dumps(self.mapping))
        before = split.MAPPING.read_bytes()
        with patch.object(sys, "argv", ["split-tests.py", "map"]), patch.object(split, "parser", return_value=self.executable):
            with self.assertRaisesRegex(ValueError, "git fetch origin main"):
                split.main()
        self.assertEqual(split.MAPPING.read_bytes(), before)

    def test_a_module_moved_into_a_scenario_is_refused(self):
        self.generate()
        module = b"mod telemetry {\n    #[test]\n    fn nested() {}\n}\n"
        scenario = self.root / "tests/scenario.rs"
        scenario.write_bytes(scenario.read_bytes() + b"\n" + module)
        with self.assertRaisesRegex(ValueError, r"move \['mod:telemetry'\] back to tests\.rs"):
            self.map_against({})
        self.assertFalse(split.MAPPING.exists())
        self.mapping["tests.rs"]["mod:telemetry"] = "scenario"
        with patch.object(split, "git_source", return_value=self.source + b"\n" + module):
            with self.assertRaisesRegex(ValueError, "mod:telemetry must stay in tests.rs"):
                split.verify(self.executable, self.mapping, "branch")

    def test_check_format_reads_a_fragment_from_the_revision_even_when_the_tree_lacks_it(self):
        self.generate()
        sources = {"tests.rs": b'include!("tests/deleted.rs");\n', "tests/deleted.rs": b"fn  spaced(){}\n"}
        with patch.object(split, "read_source", side_effect=lambda revision, path: sources[path]):
            with self.assertRaisesRegex(ValueError, r"tests/deleted\.rs lines \[1\]"):
                split.check_format(self.executable, {"tests/deleted.rs": {1}}, "HEAD")

    def test_check_format_flags_a_removed_final_newline(self):
        self.generate()
        scenario = self.root / "tests/scenario.rs"
        scenario.write_bytes(b"fn added() {}\n\nfn last() {}")
        self.assertEqual(split.unformatted_lines("tests/scenario.rs", scenario.read_bytes()), {3})
        with self.assertRaisesRegex(ValueError, r"tests/scenario\.rs lines \[3\]"):
            split.check_format(self.executable, {"tests/scenario.rs": {3}}, None)

    def test_map_follows_a_test_moved_between_scenarios(self):
        self.generate()
        common = self.root / "tests/common.rs"
        scenario = self.root / "tests/scenario.rs"
        helper = b"fn helper() -> usize { 1 }\n"
        self.assertIn(helper, common.read_bytes())
        common.write_bytes(common.read_bytes().replace(helper, b""))
        scenario.write_bytes(scenario.read_bytes() + b"\n" + helper)
        with self.assertRaisesRegex(ValueError, r"wrong scenario for fn:helper.*split-tests\.py map"):
            split.verify(self.executable, self.mapping, None)
        self.map_against({})
        self.assertEqual(json.loads(split.MAPPING.read_text())["tests.rs"]["fn:helper"], "scenario")
        split.verify(self.executable, self.mapping, None)

    def test_a_proptest_block_never_leaves_its_file(self):
        original = self.source
        self.generate()
        for config in [
            b'    #![proptest_config(koidrive_test_support::battery_config(concat!(file!(), ".regressions.txt"), 1))]\n',
            b'    #![proptest_config(koidrive_test_support::regression_config(concat!(env!("CARGO_MANIFEST_DIR"), "/cases.txt"), 8))]\n',
            b'    #![proptest_config(koidrive_test_support::battery_config(concat!(env!("CARGO_MANIFEST_DIR"), "/cases.txt"), 1))]\n',
        ]:
            with self.subTest(config=config):
                source = self.property_source(original, b"proptest::proptest", config)
                self.mapping["tests.rs"]["proptest:holds"] = "scenario"
                with patch.object(split, "git_source", return_value=source):
                    with self.assertRaisesRegex(ValueError, "proptest:holds must stay in tests.rs"):
                        split.split(self.executable, self.mapping, "base")
                with patch.object(split, "git_source", side_effect=lambda ref, path: original if ref == "base" else source):
                    with self.assertRaisesRegex(ValueError, r"\['proptest:holds'\] differ between base, branch and current"):
                        split.port(self.executable, self.mapping, "base", "branch")
                scenario = self.root / "tests/scenario.rs"
                before = scenario.read_bytes()
                scenario.write_bytes(before + b"\n" + source[len(original):])
                with patch.object(split, "git_source", return_value=source):
                    with self.assertRaisesRegex(ValueError, "proptest:holds must stay in tests.rs"):
                        split.verify(self.executable, self.mapping, "base")
                scenario.write_bytes(before)

    def test_a_proptest_block_in_a_scenario_keeps_its_file(self):
        self.mapping["tests.rs"]["proptest:holds"] = "scenario"
        self.generate()
        scenario = self.root / "tests/scenario.rs"
        scenario.write_bytes(self.property_source(scenario.read_bytes() + b"\n", b"proptest::proptest"))
        layout = {path: path.read_bytes() for path in self.root.rglob("*.rs")}
        reference = lambda ref, path: layout[self.root / path]
        with patch.object(split, "git_source", side_effect=reference):
            split.verify(self.executable, self.mapping, "base")
            split.split(self.executable, self.mapping, "base")
            self.assertEqual(scenario.read_bytes(), layout[scenario])
            branch = split.load_source(self.executable, "tests.rs", "base").replace(b"x < 4", b"x <= 3")
        with patch.object(split, "git_source", side_effect=lambda ref, path: branch if ref == "branch" else reference(ref, path)):
            split.port(self.executable, self.mapping, "base", "branch")
        self.assertIn(b"x <= 3", scenario.read_bytes())
        self.mapping["tests.rs"]["proptest:holds"] = "root"
        with patch.object(split, "git_source", side_effect=reference):
            with self.assertRaisesRegex(ValueError, "proptest:holds must stay in tests/scenario.rs"):
                split.split(self.executable, self.mapping, "base")
            with self.assertRaisesRegex(ValueError, "proptest:holds must stay in tests/scenario.rs"):
                split.verify(self.executable, self.mapping, "base")

    def test_a_scenario_file_named_root_is_refused(self):
        self.mapping["tests.rs"]["fn:one"] = "root"
        self.generate()
        original = self.path.read_bytes()
        (self.root / "tests/root.rs").write_bytes(self.property_source(split.LICENSE, b"proptest::proptest"))
        self.path.write_bytes(original.replace(b'include!("tests/common.rs");', b'include!("tests/common.rs");\ninclude!("tests/root.rs");'))
        layout = {path: path.read_bytes() for path in self.root.rglob("*.rs")}
        self.mapping["tests.rs"]["proptest:holds"] = "root"
        with patch.object(split, "git_source", side_effect=lambda ref, path: layout[self.root / path]):
            with self.assertRaisesRegex(ValueError, "unexpected include path: tests/root.rs"):
                split.split(self.executable, self.mapping, "base")
            (self.root / "tests/root.rs").unlink()
            self.path.write_bytes(original + b"\n" + self.property_source(b"", b"proptest::proptest"))
            with self.assertRaisesRegex(ValueError, "unexpected include path: tests/root.rs"):
                split.verify(self.executable, self.mapping, "base")


    def test_port_refuses_a_function_added_to_a_proptest_block(self):
        self.mapping["tests.rs"]["proptest:holds"] = "scenario"
        self.generate()
        scenario = self.root / "tests/scenario.rs"
        scenario.write_bytes(self.property_source(scenario.read_bytes() + b"\n", b"proptest::proptest"))
        layout = {path: path.read_bytes() for path in self.root.rglob("*.rs")}
        base = split.load_source(self.executable, "tests.rs")
        branch = base.replace(b"x < 4); }\n}", b"x < 4); }\n    #[test]\n    fn also_holds(x in 0..4u8) { assert!(x < 4); }\n}")
        self.mapping["tests.rs"]["proptest:holds,also_holds"] = "root"
        with patch.object(split, "git_source", side_effect=lambda ref, path: base if ref == "base" else branch):
            with self.assertRaisesRegex(ValueError, r"proptest blocks \['proptest:holds', 'proptest:holds,also_holds'\] differ.*by hand"):
                split.port(self.executable, self.mapping, "base", "branch")
        self.assertEqual(layout, {path: path.read_bytes() for path in self.root.rglob("*.rs")})

    def test_port_with_unchanged_proptest_blocks_goes_through(self):
        self.mapping["tests.rs"]["proptest:holds"] = "scenario"
        self.generate()
        scenario = self.root / "tests/scenario.rs"
        scenario.write_bytes(self.property_source(scenario.read_bytes() + b"\n", b"proptest::proptest"))
        base = split.load_source(self.executable, "tests.rs")
        branch = base.replace(b"helper(), 1", b"helper(), 2")
        with patch.object(split, "git_source", side_effect=lambda ref, path: base if ref == "base" else branch):
            split.port(self.executable, self.mapping, "base", "branch")
        self.assertIn(b"helper(), 2", scenario.read_bytes())
        self.assertIn(b"fn holds(x in 0..4u8)", scenario.read_bytes())

    REPLAY_BASE = (
        b"use std::fmt;\n\npub struct Driver;\n\nimpl Driver {\n    fn one(&self) -> u8 {\n        1\n    }\n\n"
        b"    fn two(&self) -> u8 {\n        2\n    }\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n"
        b"    #[test]\n    fn first() {\n        assert_eq!(Driver.one(), 1);\n    }\n}\n"
    )

    def split_driver(self):
        files = {
            "driver.rs": b"use std::fmt;\n\nmod parts;\n\npub struct Driver;\n\nimpl Driver {\n    fn one(&self) -> u8 {\n"
                         b"        1\n    }\n}\n\n#[cfg(test)]\nmod tests;\n",
            "driver/parts.rs": b"use super::*;\n\nimpl Driver {\n    pub(super) fn two(&self) -> u8 {\n        2\n    }\n}\n",
            "driver/tests.rs": b"use super::*;\n\ninclude!(\"tests/cases.rs\");\n",
            "driver/tests/cases.rs": split.LICENSE + b"#[test]\nfn first() {\n    assert_eq!(Driver.one(), 1);\n}\n",
        }
        for name, text in files.items():
            (self.root / name).parent.mkdir(parents=True, exist_ok=True)
            (self.root / name).write_bytes(text)
        return {name: (self.root / name).read_bytes() for name in files}

    def replay(self, branch):
        sources = {"base": self.REPLAY_BASE, "branch": branch}
        with patch.object(split, "git_source", side_effect=lambda ref, path: sources[ref]):
            for file, source in split.replay_onto_split(self.executable, "driver.rs", "base", "branch").items():
                file.write_bytes(source)

    def test_replay_merges_a_changed_method_where_the_split_moved_it(self):
        self.split_driver()
        self.replay(self.REPLAY_BASE.replace(b"        2\n", b"        3\n"))
        self.assertEqual((self.root / "driver/parts.rs").read_bytes(),
                         b"use super::*;\n\nimpl Driver {\n    pub(super) fn two(&self) -> u8 {\n        3\n    }\n}\n")

    def test_replay_puts_an_added_test_after_its_neighbour_at_the_fragment_indent(self):
        self.split_driver()
        added = b"    }\n\n    #[test]\n    fn second() {\n        assert_eq!(Driver.two(), 2);\n    }\n}\n"
        self.replay(self.REPLAY_BASE.removesuffix(b"    }\n}\n") + added)
        self.assertEqual((self.root / "driver/tests/cases.rs").read_bytes(), split.LICENSE
                         + b"#[test]\nfn first() {\n    assert_eq!(Driver.one(), 1);\n}\n\n"
                         + b"#[test]\nfn second() {\n    assert_eq!(Driver.two(), 2);\n}\n")

    def test_replay_removes_a_method_from_the_file_the_split_moved_it_to(self):
        self.split_driver()
        self.replay(self.REPLAY_BASE.replace(b"    }\n\n    fn two(&self) -> u8 {\n        2\n    }\n}", b"    }\n}"))
        self.assertEqual((self.root / "driver/parts.rs").read_bytes(), b"use super::*;\n\nimpl Driver {}\n")

    def test_replay_refuses_an_edit_outside_any_item_without_writes(self):
        before = self.split_driver()
        with self.assertRaisesRegex(ValueError, "outside any item"):
            self.replay(self.REPLAY_BASE.replace(b"mod tests {", b"#[allow(unused)]\nmod tests {"))
        self.assertEqual(before, {name: (self.root / name).read_bytes() for name in before})

    def test_replay_refuses_an_import_change_the_split_rewrote_without_writes(self):
        self.split_driver()
        root = self.root / "driver.rs"
        root.write_bytes(root.read_bytes().replace(b"use std::fmt;", b"use std::fmt::{self};"))
        before = {path: path.read_bytes() for path in self.root.rglob("*.rs")}
        with self.assertRaisesRegex(ValueError, "removes .use:std :: fmt., which the split rewrote"):
            self.replay(self.REPLAY_BASE.replace(b"use std::fmt;\n", b"use std::fmt::Write;\n"))
        self.assertEqual(before, {path: path.read_bytes() for path in self.root.rglob("*.rs")})

    def test_replay_keeps_the_lines_of_a_multiline_literal_when_it_dedents_a_test(self):
        self.split_driver()
        added = b'    }\n\n    #[test]\n    fn second() {\n        let text = r#"keep\n    build/\n"#;\n        assert!(!text.is_empty());\n    }\n}\n'
        self.replay(self.REPLAY_BASE.removesuffix(b"    }\n}\n") + added)
        self.assertIn(b'#[test]\nfn second() {\n    let text = r#"keep\n    build/\n"#;\n    assert!(!text.is_empty());\n}\n',
                      (self.root / "driver/tests/cases.rs").read_bytes())

    def test_replay_keeps_the_literals_of_an_added_module_when_it_dedents_it(self):
        self.split_driver()
        added = b'    }\n\n    mod helpers {\n        pub fn fixture() -> &\'static str {\n            r#"keep\n    build/\n"#\n        }\n    }\n}\n'
        self.replay(self.REPLAY_BASE.removesuffix(b"    }\n}\n") + added)
        self.assertIn(b'mod helpers {\n    pub fn fixture() -> &\'static str {\n        r#"keep\n    build/\n"#\n    }\n}\n',
                      (self.root / "driver/tests/cases.rs").read_bytes())

    def test_replay_refuses_a_key_that_repeats(self):
        before = self.split_driver()
        repeated = b"const _: () = assert!(true);\n\nconst _: () = assert!(!false);\n\npub struct Driver;"
        with self.assertRaisesRegex(ValueError, "const:_ appears 2 times"):
            self.replay(self.REPLAY_BASE.replace(b"pub struct Driver;", repeated))
        self.assertEqual(before, {name: (self.root / name).read_bytes() for name in before})

    def test_replay_refuses_to_remove_an_item_the_split_tree_holds_under_another_key(self):
        self.split_driver()
        parts = self.root / "driver/parts.rs"
        parts.write_bytes(parts.read_bytes().replace(b"    pub(super) fn two", b"    #[cfg(unix)]\n    pub(super) fn two"))
        with self.assertRaisesRegex(ValueError, "removes impl::Driver/fn:two, which the split tree lacks"):
            self.replay(self.REPLAY_BASE.replace(b"    }\n\n    fn two(&self) -> u8 {\n        2\n    }\n}", b"    }\n}"))

    def test_replay_refuses_overlapping_edits_without_writes(self):
        before = self.split_driver()
        parts = self.root / "driver/parts.rs"
        parts.write_bytes(parts.read_bytes().replace(b"        2\n", b"        4\n"))
        before["driver/parts.rs"] = parts.read_bytes()
        with self.assertRaisesRegex(ValueError, "overlapping edits in impl::Driver/fn:two"):
            self.replay(self.REPLAY_BASE.replace(b"        2\n", b"        3\n"))
        self.assertEqual(before, {name: (self.root / name).read_bytes() for name in before})


if __name__ == "__main__":
    unittest.main()
