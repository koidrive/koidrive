#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
import importlib.util
import os
import re
import subprocess
import sys
import tempfile
import textwrap
import unittest

CHECK = os.path.join(os.path.dirname(os.path.abspath(__file__)), "check-new-comments.py")


def setUpModule():
    spec = importlib.util.spec_from_file_location("check", CHECK)
    check = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(check)
    os.environ["FN_DOCS"] = check.fn_docs()


class NewComments(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.repo = self.dir.name
        subprocess.run(["git", "init", "-q", self.repo], check=True)

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
                f.write(text)
        subprocess.run(["git", "-C", self.repo, "add", "-A"], check=True)
        return subprocess.run(["git", "-C", self.repo, "write-tree"], check=True, capture_output=True, text=True).stdout.strip()

    def run_check(self, before, after):
        base = self.tree(before)
        head = self.tree(after)
        result = subprocess.run([sys.executable, CHECK, base, head], cwd=self.repo, capture_output=True, text=True)
        flagged = [line.split("::", 2)[1] for line in result.stdout.splitlines() if line.startswith("::error")]
        self.assertEqual(result.returncode, 1 if flagged else 0, result.stdout + result.stderr)
        return flagged

    def assertFlags(self, before, after, *expected):
        self.assertEqual(self.run_check(before, after), [f"error file={path},line={line}" for path, line in expected])

    def test_new_line_comment_fails(self):
        self.assertFlags({"core/a/src/lib.rs": "fn a() {}\n"}, {"core/a/src/lib.rs": "// explains a\nfn a() {}\n"}, ("core/a/src/lib.rs", 1))

    def test_trailing_comment_fails(self):
        self.assertFlags({"server/src/a.ts": "const a = 1;\n"}, {"server/src/a.ts": "const a = 2; // bumped\n"}, ("server/src/a.ts", 1))

    def test_every_line_of_a_new_block_comment_fails(self):
        self.assertFlags({"apps/macos/Koidrive/A.swift": "let a = 1\n"}, {"apps/macos/Koidrive/A.swift": "/* one\n two */\nlet a = 1\n"}, ("apps/macos/Koidrive/A.swift", 1), ("apps/macos/Koidrive/A.swift", 2))

    def test_deleting_comments_passes(self):
        self.assertFlags({"core/a/src/lib.rs": "/// Doc.\n// note\nfn a() {} // why\n"}, {"core/a/src/lib.rs": "fn a() {}\n"})

    def test_trimming_a_comment_passes(self):
        self.assertFlags(
            {"server/src/a.ts": "// Workers cap subrequests at 1000 per invocation, see spec v0 section 4.\nconst MAX = 1000;\n"},
            {"server/src/a.ts": "// Workers cap subrequests at 1000 per invocation.\nconst MAX = 1000;\n"},
        )

    def test_rewording_a_comment_fails(self):
        self.assertFlags(
            {"server/src/a.ts": "// Workers cap subrequests.\nconst MAX = 1000;\n"},
            {"server/src/a.ts": "// Workers limit subrequests.\nconst MAX = 1000;\n"},
            ("server/src/a.ts", 1),
        )

    def test_moving_a_doc_between_files_passes(self):
        self.assertFlags(
            {"server/src/a.ts": "/** Opens the space. */\nexport function open() {}\n", "server/src/b.ts": "export function close() {}\n"},
            {"server/src/a.ts": "export function open() {}\n", "server/src/b.ts": "/** Opens the space. */\nexport function close() {}\n"},
        )

    def test_doc_comment_on_a_library_passes(self):
        self.assertFlags({"core/a/src/lib.rs": "pub fn a() {}\n"}, {"core/a/src/lib.rs": "//! The a crate.\n\n/// Returns nothing.\npub fn a() {}\n"})

    def test_doc_comment_on_internal_code_fails(self):
        self.assertFlags(
            {"core/a/tests/t.rs": "fn t() {}\n", "server/src/a.ts": "export function a() {}\n", "apps/macos/KoidriveFFI/Tests/KoidriveFFITests/T.swift": "func t() {}\n"},
            {"core/a/tests/t.rs": "/// Helper.\nfn t() {}\n", "server/src/a.ts": "/** Helper. */\nexport function a() {}\n", "apps/macos/KoidriveFFI/Tests/KoidriveFFITests/T.swift": "/// Helper.\nfunc t() {}\n"},
            ("apps/macos/KoidriveFFI/Tests/KoidriveFFITests/T.swift", 1),
            ("core/a/tests/t.rs", 1),
            ("server/src/a.ts", 1),
        )

    def test_markers_fail_in_any_comment(self):
        self.assertFlags(
            {"core/a/src/lib.rs": "pub fn a() {}\n"},
            {"core/a/src/lib.rs": "/// Returns nothing. TODO: return something.\npub fn a() {}\n/// Fine for now.\npub fn b() {}\n"},
            ("core/a/src/lib.rs", 1),
            ("core/a/src/lib.rs", 3),
        )

    def test_a_kept_marker_fails_even_when_trimmed(self):
        self.assertFlags({"core/a/src/lib.rs": "// TODO: drop this hack once v2 ships\nfn a() {}\n"}, {"core/a/src/lib.rs": "// TODO: drop this hack\nfn a() {}\n"}, ("core/a/src/lib.rs", 1))

    def test_allowed_comments_pass(self):
        self.assertFlags(
            {"cli/src/a.rs": "fn a() {}\n", "server/src/a.ts": "export const a = 1;\n", "apps/macos/Koidrive/A.swift": "let a = 1\n"},
            {
                "cli/src/a.rs": "// SPDX-License-Identifier: Apache-2.0\nfn a() {\n    // SAFETY: fd is open for the whole call.\n    unsafe {}\n}\n",
                "server/src/a.ts": "// Durable Objects drop the alarm on eviction: https://developers.cloudflare.com/durable-objects/api/alarms/\n// eslint-disable-next-line no-console\nexport const a = 1;\n",
                "apps/macos/Koidrive/A.swift": "// swiftlint:disable:next identifier_name\nlet a = 1\n",
            },
        )

    def test_comment_markers_inside_strings_pass(self):
        self.assertFlags(
            {"core/a/src/lib.rs": "fn a() {}\n", "server/src/a.ts": "export const a = 1;\n", "apps/macos/Koidrive/A.swift": "let a = 1\n"},
            {
                "core/a/src/lib.rs": 'fn a() {\n    let u = "http://x // y";\n    let r = r#"a // "b" /* c"#;\n    let c = \'/\';\n    let s = "line\n// still a string";\n}\n',
                "server/src/a.ts": "export const a = '//';\nconst re = /\\/\\//g;\nconst t = \x60x ${'// y'} ${\x60//\x60} //\x60;\nconst d = 4 / 2 / 1;\n",
                "apps/macos/Koidrive/A.swift": 'let a = "x \\("//") y"\nlet b = #"a // "b""#\nlet c = """\n// text\n"""\n',
            },
        )

    def test_comments_after_strings_still_fail(self):
        self.assertFlags(
            {"server/src/a.ts": "export const a = 1;\n", "core/a/src/lib.rs": "fn a() {}\n"},
            {"server/src/a.ts": "const t = \x60${'x'}\x60; // after a template\nconst re = /a/; // after a regex\n", "core/a/src/lib.rs": 'fn a<\'a>(x: &\'a str) -> char { \'"\' } // after a lifetime and a char\n'},
            ("core/a/src/lib.rs", 1),
            ("server/src/a.ts", 1),
            ("server/src/a.ts", 2),
        )

    def test_nested_rust_block_comment_is_one_comment(self):
        self.assertFlags({"core/a/src/lib.rs": "fn a() {}\n"}, {"core/a/src/lib.rs": "/* outer /* inner */ still outer */ fn a() {}\n"}, ("core/a/src/lib.rs", 1))

    def test_generated_code_and_other_languages_are_skipped(self):
        self.assertFlags(
            {"server/src/gen/a.ts": "export const a = 1;\n", "scripts/a.sh": "true\n"},
            {"server/src/gen/a.ts": "// Code generated by protoc-gen-es.\nexport const a = 1;\n", "scripts/a.sh": "# a note\ntrue\n"},
        )

    def test_new_and_deleted_files(self):
        self.assertFlags({"server/src/old.ts": "// old note\nexport const a = 1;\n"}, {"server/src/old.ts": None, "server/src/new.ts": "// new note\nexport const b = 2;\n"}, ("server/src/new.ts", 1))


    def comments_only(self, before, after):
        base = self.tree(before)
        head = self.tree(after)
        result = subprocess.run([sys.executable, CHECK, "--comments-only", base, head], cwd=self.repo, capture_output=True, text=True)
        return result.returncode, result.stdout

    def test_comments_only_passes_when_only_comments_change(self):
        code, out = self.comments_only(
            {"core/a/src/lib.rs": '/// Doc.\nfn a() {\n    let s = "// kept"; // gone\n}\n', "server/src/a.ts": '/** Doc. */\nexport const a = "x  y";\n'},
            {"core/a/src/lib.rs": 'fn a() {\n    let s = "// kept";\n}\n', "server/src/a.ts": 'export const a = "x  y";\n'},
        )
        self.assertEqual(code, 0, out)
        self.assertIn("All 2 changed files", out)

    def test_comments_only_fails_on_a_code_change(self):
        code, out = self.comments_only({"server/src/a.ts": '// note\nexport const a = "x  y";\n'}, {"server/src/a.ts": 'export const a = "x y";\n'})
        self.assertEqual((code, out.splitlines()[0]), (1, "server/src/a.ts: code differs once comments are stripped"))

    def test_comments_only_fails_on_added_deleted_or_other_files(self):
        code, out = self.comments_only({"server/src/a.ts": "export const a = 1;\n", "notes.md": "a\n"}, {"server/src/a.ts": None, "server/src/b.ts": "export const a = 1;\n", "notes.md": "b\n"})
        self.assertEqual(code, 1, out)
        self.assertEqual(out.splitlines()[:3], ["notes.md: M on a file that is not Rust, Swift, TypeScript or JavaScript", "server/src/a.ts: status D", "server/src/b.ts: status A"])
    def test_template_interpolations_are_code(self):
        self.assertFlags(
            {"server/src/a.ts": "export const a = 1;\n"},
            {"server/src/a.ts": "const t = \x60${\x60a//b\x60}\x60;\nconst u = \x60${ /* why */ 1}\x60;\n"},
            ("server/src/a.ts", 2),
        )

    def test_pragmas_count_only_at_the_start_of_a_comment(self):
        self.assertFlags(
            {"apps/macos/KoidriveFFI/Package.swift": "let a = 1\n", "cli/src/a.rs": "fn a() {}\n"},
            {"apps/macos/KoidriveFFI/Package.swift": "// swift-tools-version: 6.1\nlet a = 1\n", "cli/src/a.rs": "// Not a SAFETY: issue, retry because it races.\nfn a() {}\n"},
            ("cli/src/a.rs", 1),
        )

    def test_a_link_anywhere_in_a_comment_run_allows_the_run(self):
        self.assertFlags(
            {"server/src/a.ts": "export const a = 1;\n"},
            {"server/src/a.ts": "// Durable Objects drop the alarm on eviction, see\n// https://developers.cloudflare.com/durable-objects/api/alarms/\nexport const a = 1;\n"},
        )

    def test_docs_on_build_scripts_binaries_examples_and_benches_fail(self):
        self.assertFlags(
            {"core/a/build.rs": "fn main() {}\n", "core/a/src/bin/b.rs": "fn main() {}\n", "core/a/benches/c.rs": "fn c() {}\n"},
            {"core/a/build.rs": "/// Builds.\nfn main() {}\n", "core/a/src/bin/b.rs": "//! A tool.\nfn main() {}\n", "core/a/benches/c.rs": "/// Measures.\nfn c() {}\n"},
            ("core/a/benches/c.rs", 1),
            ("core/a/build.rs", 1),
            ("core/a/src/bin/b.rs", 1),
        )

    def test_clap_help_text_is_not_a_comment(self):
        self.assertFlags(
            {"cli/tests/t.rs": "#[derive(Parser)]\nstruct Cli {}\n"},
            {"cli/tests/t.rs": "#[derive(Parser)]\n/// Syncs folders.\nstruct Cli {}\n"},
        )

    def test_comments_only_counts_clap_help_and_doctests_as_code(self):
        clap = "#[derive(Subcommand)]\nenum Command {\n    /// Show sync history\n    History,\n}\n"
        doctest = "/// Adds.\n///\n/// ```\n/// assert_eq!(a::add(1, 1), 2);\n/// ```\npub fn add(a: u8, b: u8) -> u8 { a + b }\n"
        code, out = self.comments_only({"cli/src/main.rs": clap, "core/a/src/lib.rs": doctest}, {"cli/src/main.rs": clap.replace("    /// Show sync history\n", ""), "core/a/src/lib.rs": doctest.split("\n", 5)[5]})
        self.assertEqual(code, 1, out)
        self.assertEqual([line.split(":")[0] for line in out.splitlines()[:2]], ["cli/src/main.rs", "core/a/src/lib.rs"])

    def test_comments_only_protects_only_docs_on_clap_items(self):
        src = (
            '/// Syncs folders.\n#[derive(Parser)]\n#[command(name = "koi {")]\nstruct Cli {\n    /// Print JSON.\n    #[arg(long)]\n    json: bool,\n}\n\n'
            "#[derive(Subcommand)]\nenum Command {\n    /// Show sync history.\n    History {\n        /// The folder to show.\n        folder: String,\n    },\n}\n\n"
            "/// One line per space.\nfn table() {}\n\n/// A found server.\n#[derive(Debug)]\nstruct Found;\n"
        )
        for doc in ["/// Syncs folders.\n", "    /// Print JSON.\n", "    /// Show sync history.\n", "        /// The folder to show.\n"]:
            with self.subTest(doc=doc):
                code, out = self.comments_only({"cli/src/a.rs": src}, {"cli/src/a.rs": src.replace(doc, "", 1)})
                self.assertEqual(code, 1, out)
        code, out = self.comments_only({"cli/src/a.rs": src}, {"cli/src/a.rs": src.replace("/// One line per space.\n", "").replace("/// A found server.\n", "")})
        self.assertEqual(code, 0, out)

    def test_a_new_doc_beside_clap_items_in_internal_code_fails(self):
        self.assertFlags(
            {"cli/tests/t.rs": "#[derive(Parser)]\nstruct Cli {}\nfn run() {}\n"},
            {"cli/tests/t.rs": "#[derive(Parser)]\nstruct Cli {}\n/// Runs.\nfn run() {}\n"},
            ("cli/tests/t.rs", 3),
        )

    def test_comments_only_passes_plain_rust_doc_removal(self):
        code, out = self.comments_only({"core/a/src/lib.rs": "/// Adds.\npub fn add(a: u8, b: u8) -> u8 { a + b }\n"}, {"core/a/src/lib.rs": "pub fn add(a: u8, b: u8) -> u8 { a + b }\n"})
        self.assertEqual(code, 0, out)

    def test_comments_only_keeps_line_breaks_that_change_javascript(self):
        code, out = self.comments_only({"server/src/a.ts": "export function f() {\n  return /*\n  */ 1;\n}\n"}, {"server/src/a.ts": "export function f() {\n  return 1;\n}\n"})
        self.assertEqual((code, out.splitlines()[0]), (1, "server/src/a.ts: code differs once comments are stripped"))

    def test_a_rust_inner_attribute_on_the_first_line_is_code(self):
        self.assertFlags({"core/a/src/lib.rs": "#![no_std]\n"}, {"core/a/src/lib.rs": "#![no_std] // why\n"}, ("core/a/src/lib.rs", 1))
        code, out = self.comments_only({"core/a/src/lib.rs": "#![no_std]\n"}, {"core/a/src/lib.rs": "#![forbid(unsafe_code)]\n"})
        self.assertEqual((code, out.splitlines()[0]), (1, "core/a/src/lib.rs: code differs once comments are stripped"))

    def test_markers_fail_in_clap_help_text(self):
        self.assertFlags({"cli/src/a.rs": "#[derive(Parser)]\nstruct Cli {}\n"}, {"cli/src/a.rs": "#[derive(Parser)]\n/// TODO: describe\nstruct Cli {}\n"}, ("cli/src/a.rs", 2))

    def test_comments_only_counts_a_changed_doctest_body_as_code(self):
        before = "/// Adds.\n///\n/// ```\n/// assert_eq!(a::add(1, 1), 2);\n/// ```\npub fn add(a: u8, b: u8) -> u8 { a + b }\n"
        code, out = self.comments_only({"core/a/src/lib.rs": before}, {"core/a/src/lib.rs": before.replace("2);", "3);")})
        self.assertEqual((code, out.splitlines()[0].split(":")[0]), (1, "core/a/src/lib.rs"))


    def test_comments_only_strips_comments_inside_template_interpolations(self):
        before = "const t = \x60prefix ${ /* why */ 1 + DB_NEST } suffix\x60;\n"
        before = before.replace("DB_NEST", "\x60inner ${ /* nested */ 2}\x60")
        after = before.replace("/* why */", "").replace("/* nested */", "")
        code, out = self.comments_only({"server/src/a.ts": before}, {"server/src/a.ts": after})
        self.assertEqual(code, 0, out)
        code, out = self.comments_only({"server/src/a.ts": before}, {"server/src/a.ts": after.replace("2}", "3}")})
        self.assertEqual(code, 1, out)


    def test_comments_only_preserves_operator_boundaries(self):
        for before, after in [
            ("const x = a + ++b;\n", "const x = a++ + b;\n"),
            ("const x = a - --b;\n", "const x = a-- - b;\n"),
            ("const x = a + /* separated */ +b;\n", "const x = a++b;\n"),
            ("const x = 1 .toString();\n", "const x = 1.toString();\n"),
            ("const x = \x60" + "$" + "{a + ++b}\x60;\n", "const x = \x60" + "$" + "{a++ + b}\x60;\n"),
        ]:
            with self.subTest(before=before):
                code, out = self.comments_only({"server/src/a.ts": before}, {"server/src/a.ts": after})
                self.assertEqual(code, 1, out)
        code, out = self.comments_only(
            {"core/a/src/lib.rs": "fn f() { a & &b; }\n"},
            {"core/a/src/lib.rs": "fn f() { a && b; }\n"},
        )
        self.assertEqual(code, 1, out)

    def test_a_helper_between_a_pub_fn_and_its_doc_comment_fails(self):
        self.assertFlags(
            {"core/a/src/lib.rs": "/// Acknowledges.\npub fn ack() {}\n"},
            {"core/a/src/lib.rs": "/// Acknowledges.\nfn helper() {}\n\npub fn ack() {\n    helper();\n}\n"},
            ("core/a/src/lib.rs", 2),
            ("core/a/src/lib.rs", 4),
        )

    def test_a_documented_helper_between_a_pub_fn_and_its_doc_comment_fails(self):
        self.assertFlags(
            {"core/a/src/lib.rs": "/// Acknowledges.\npub fn ack() {}\n"},
            {"core/a/src/lib.rs": "/// Acknowledges.\n/// Helps.\nfn helper() {}\n\npub fn ack() {\n    helper();\n}\n"},
            ("core/a/src/lib.rs", 3),
            ("core/a/src/lib.rs", 5),
        )

    def test_a_helper_between_a_doc_comment_and_its_multiline_attribute_fails(self):
        self.assertFlags(
            {"core/a/src/lib.rs": "/// Acknowledges.\n#[allow(\n    dead_code,\n)]\npub fn ack() {}\n"},
            {"core/a/src/lib.rs": "/// Acknowledges.\n#[allow(\n    dead_code,\n)]\nfn helper() {}\n\npub fn ack() {}\n"},
            ("core/a/src/lib.rs", 5),
            ("core/a/src/lib.rs", 7),
        )

    def test_a_helper_after_a_blank_line_below_a_doc_comment_fails(self):
        self.assertFlags(
            {"core/a/src/lib.rs": "/// Acknowledges.\n\npub fn ack() {}\n"},
            {"core/a/src/lib.rs": "/// Acknowledges.\n\nfn helper() {}\n\npub fn ack() {}\n"},
            ("core/a/src/lib.rs", 3),
            ("core/a/src/lib.rs", 5),
        )

    def test_deleting_one_of_two_equal_doc_comments_passes(self):
        self.assertFlags(
            {"core/a/src/lib.rs": "/// Runs.\npub fn a() {}\n/// Runs.\npub fn b() {}\n"},
            {"core/a/src/lib.rs": "pub fn a() { let _x = 1; }\n/// Runs.\npub fn b() {}\n"},
        )

    def test_renaming_a_private_fn_whose_doc_a_pub_fn_shares_passes(self):
        self.assertFlags(
            {"core/a/src/lib.rs": "/// Runs.\npub fn a() {}\n/// Runs.\nfn b() {}\n"},
            {"core/a/src/lib.rs": "/// Runs.\npub fn a() {}\n/// Runs.\nfn c() {}\n"},
        )

    def test_deleting_a_pub_fn_and_leaving_its_doc_on_a_helper_fails(self):
        self.assertFlags(
            {"core/a/src/lib.rs": "/// Acknowledges.\npub fn ack() {}\nfn helper() {}\n"},
            {"core/a/src/lib.rs": "/// Acknowledges.\nfn helper() {}\n"},
            ("core/a/src/lib.rs", 2),
        )

    def test_a_helper_under_a_pub_fn_doc_in_a_file_with_clap_help_fails(self):
        prefix = "#[derive(Parser)]\nstruct Args;\n"
        self.assertFlags(
            {"core/a/src/lib.rs": prefix + "/// Acknowledges.\npub fn ack() {}\n"},
            {"core/a/src/lib.rs": prefix + "/// Acknowledges.\nfn helper() {}\n\npub fn ack() {}\n"},
            ("core/a/src/lib.rs", 4),
            ("core/a/src/lib.rs", 6),
        )

    def test_brackets_in_attribute_strings_and_comments_keep_the_doc_attached(self):
        for attribute in ['#[allow(dead_code, reason = "[")]', '#[allow(dead_code, reason = "]")]', "#[allow(dead_code)] // [", "#[allow(/* ] */ dead_code)]"]:
            with self.subTest(attribute=attribute):
                self.assertFlags(
                    {"core/a/src/lib.rs": f"/// Runs.\n{attribute}\npub fn a() {{}}\n"},
                    {"core/a/src/lib.rs": f"/// Runs.\n{attribute}\nfn helper() {{}}\npub fn a() {{}}\n"},
                    ("core/a/src/lib.rs", 3),
                    ("core/a/src/lib.rs", 4),
                )

    def test_moving_different_block_docs_between_pub_fns_passes(self):
        self.assertFlags(
            {"core/a/src/lib.rs": "/**\n * Acknowledges.\n */\npub fn a() {}\npub fn b() {}\n"},
            {"core/a/src/lib.rs": "pub fn a() {}\n/**\n * Lists.\n */\npub fn b() {}\n"},
        )

    def test_a_block_doc_moved_onto_a_helper_fails(self):
        self.assertFlags(
            {"core/a/src/lib.rs": "/**\n * Acknowledges.\n */\npub fn ack() {}\n"},
            {"core/a/src/lib.rs": "/**\n * Acknowledges.\n */\nfn helper() {}\npub fn ack() {}\n"},
            ("core/a/src/lib.rs", 4),
            ("core/a/src/lib.rs", 5),
        )

    def test_a_code_edit_below_a_shebang_passes(self):
        self.assertFlags(
            {"core/a/src/main.rs": "#!/usr/bin/env rust-script\npub fn a() {}\n"},
            {"core/a/src/main.rs": "#!/usr/bin/env rust-script\npub fn a() { let _x = 1; }\n"},
        )

    def test_reordering_modules_with_same_named_documented_fns_passes(self):
        a = "mod a {\n    /// Runs a.\n    pub fn run() {}\n}\n"
        b = "mod b {\n    /// Runs b.\n    pub fn run() {}\n}\n"
        self.assertFlags({"core/a/src/lib.rs": a + b}, {"core/a/src/lib.rs": b + a})

    def test_a_pub_trait_method_doc_moved_onto_a_helper_fails(self):
        self.assertFlags(
            {"core/a/src/lib.rs": "pub trait Api {\n    /// Calls.\n    fn call(&self);\n}\nfn helper() {}\n"},
            {"core/a/src/lib.rs": "pub trait Api {\n    fn call(&self);\n}\n/// Calls.\nfn helper() {}\n"},
            ("core/a/src/lib.rs", 2),
            ("core/a/src/lib.rs", 5),
        )

    def test_reordering_cfg_alternatives_of_a_documented_fn_passes(self):
        mac = '#[cfg(target_os = "macos")]\n/// Runs on macOS.\npub fn run() {}\n'
        other = '/// Runs elsewhere.\n#[cfg(not(target_os = "macos"))]\npub fn run() {}\n'
        self.assertFlags({"core/a/src/lib.rs": mac + other}, {"core/a/src/lib.rs": other + mac})

    def test_reordering_cfg_alternatives_of_an_impl_passes(self):
        mac = '#[cfg(target_os = "macos")]\nimpl A {\n    /// Runs on macOS.\n    pub fn run() {}\n}\n'
        other = '#[cfg(not(target_os = "macos"))]\nimpl A {\n    /// Runs elsewhere.\n    pub fn run() {}\n}\n'
        self.assertFlags({"core/a/src/lib.rs": mac + other}, {"core/a/src/lib.rs": other + mac})

    def test_reordering_cfg_attr_alternatives_of_a_documented_fn_passes(self):
        mac = '#[cfg_attr(all(), cfg(target_os = "macos"))]\n/// Runs on macOS.\npub fn run() {}\n'
        other = '#[cfg_attr(all(), cfg(not(target_os = "macos")))]\n/// Runs elsewhere.\npub fn run() {}\n'
        self.assertFlags({"core/a/src/lib.rs": mac + other}, {"core/a/src/lib.rs": other + mac})

    def test_renaming_one_cfg_alternative_with_its_doc_passes(self):
        mac = '#[cfg(target_os = "macos")]\n/// Runs on macOS.\npub fn {}() {{}}\n'
        other = '#[cfg(not(target_os = "macos"))]\n/// Runs elsewhere.\npub fn run() {}\n'
        self.assertFlags({"core/a/src/lib.rs": mac.format("run") + other}, {"core/a/src/lib.rs": mac.format("moved") + other})

    def test_a_doc_moved_from_a_pub_fn_to_a_private_namesake_fails(self):
        self.assertFlags(
            {"core/a/src/lib.rs": '#[cfg(target_os = "macos")]\n/// Runs.\npub fn run() {}\n#[cfg(not(target_os = "macos"))]\nfn run() {}\n'},
            {"core/a/src/lib.rs": '#[cfg(target_os = "macos")]\npub fn run() {}\n#[cfg(not(target_os = "macos"))]\n/// Runs.\nfn run() {}\n'},
            ("core/a/src/lib.rs", 2),
            ("core/a/src/lib.rs", 5),
        )

    def test_a_moved_doc_that_prefixes_an_extended_namesake_doc_fails(self):
        self.assertFlags(
            {"core/a/src/lib.rs": '#[cfg(unix)]\n/// Runs.\npub fn run() {}\n#[cfg(not(unix))]\n/// Runs.\n/// Fast.\npub fn run() {}\nfn helper() {}\n'},
            {"core/a/src/lib.rs": '#[cfg(unix)]\npub fn run() {}\n#[cfg(not(unix))]\n/// Runs.\n/// Fast.\n/// More.\npub fn run() {}\n/// Runs.\nfn helper() {}\n'},
            ("core/a/src/lib.rs", 2),
            ("core/a/src/lib.rs", 9),
        )

    def test_deleting_a_pub_fn_doc_comment_passes(self):
        self.assertFlags(
            {"core/a/src/lib.rs": "/// Acknowledges.\npub fn ack() {}\n"},
            {"core/a/src/lib.rs": "pub fn ack() {}\n"},
        )

    def test_a_new_doc_comment_on_a_private_fn_outside_internal_code_passes(self):
        self.assertFlags(
            {"core/a/src/lib.rs": "pub(crate) fn a() {}\n"},
            {"core/a/src/lib.rs": "/// Explains a.\npub(crate) fn a() {}\n"},
        )

    def test_renaming_a_documented_private_fn_passes(self):
        self.assertFlags(
            {"core/a/src/lib.rs": "/// Explains a.\n#[inline]\nfn a() {}\n"},
            {"core/a/src/lib.rs": "/// Explains a.\n#[inline]\nfn b() {}\n"},
        )

    def test_a_helper_below_a_documented_pub_fn_passes(self):
        self.assertFlags(
            {"core/a/src/lib.rs": "/// Acknowledges.\npub fn ack() {}\n"},
            {"core/a/src/lib.rs": "/// Acknowledges.\npub fn ack() {\n    helper();\n}\n\nfn helper() {}\n"},
        )

    def test_clap_help_on_a_private_fn_passes(self):
        self.assertFlags(
            {"cli/src/a.rs": "#[derive(Parser)]\nstruct A;\nfn run() {}\n"},
            {"cli/src/a.rs": "#[derive(Parser)]\nstruct A;\n/// Runs.\nfn run() {}\n"},
        )


class WorkflowBase(unittest.TestCase):
    def run_workflow(self, event):
        workflow = os.path.join(os.path.dirname(CHECK), "..", ".github", "workflows", "ci.yml")
        with open(workflow) as f:
            job = re.split(r"\n  [A-Za-z0-9_-]+:\n", f.read().split("\n  comments:\n", 1)[1], maxsplit=1)[0]
        script = textwrap.dedent(job.rsplit("        run: |\n", 1)[1])
        with tempfile.TemporaryDirectory() as tmp:
            for name, body in {
                "git": 'printf "%s\\n" "$*" >> "$TRACE"\nif [ "$1" = rev-parse ]; then printf "%s\\n" actual-parent; fi\n',
                "python3": 'printf "%s\\n" "$*" > "$CHECK_ARGS"\n',
            }.items():
                path = os.path.join(tmp, name)
                with open(path, "w") as f:
                    f.write("#!/bin/sh\n" + body)
                os.chmod(path, 0o755)
            env = dict(os.environ, PATH=tmp + os.pathsep + os.environ["PATH"], EVENT=event, BASE="stale-event-base",
                       TRACE=os.path.join(tmp, "trace"), CHECK_ARGS=os.path.join(tmp, "check-args"))
            subprocess.run(["bash", "-e", "-c", script], cwd=tmp, env=env, check=True, capture_output=True, text=True)
            with open(env["TRACE"]) as f:
                trace = f.read().splitlines()
            with open(env["CHECK_ARGS"]) as f:
                args = f.read().strip()
            return trace, args, job

    def test_pull_request_uses_actual_merge_parent_not_stale_event_base(self):
        trace, args, job = self.run_workflow("pull_request")
        self.assertEqual(args, "scripts/check-new-comments.py actual-parent HEAD")
        self.assertEqual(trace, ["rev-parse HEAD^1"])
        self.assertIn("fetch-depth: 2", job)

    def test_merge_group_keeps_event_base(self):
        trace, args, _ = self.run_workflow("merge_group")
        self.assertEqual(args, "scripts/check-new-comments.py stale-event-base HEAD")
        self.assertEqual(trace, ["fetch --no-tags --depth=1 origin stale-event-base"])


if __name__ == "__main__":
    unittest.main()
