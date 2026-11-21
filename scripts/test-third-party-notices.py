#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import contextlib
import hashlib
import importlib.util
import io
from pathlib import Path
import tempfile
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location("notices", Path(__file__).with_name("third-party-notices.py"))
notices = importlib.util.module_from_spec(spec)
spec.loader.exec_module(notices)

MIT = "Copyright (c) 2020 Someone\n\nPermission is hereby granted, free of charge, to any person obtaining a copy\nTHE SOFTWARE IS PROVIDED \"AS IS\", WITHOUT WARRANTY OF ANY KIND"
APACHE = "                                 Apache License\n                           Version 2.0, January 2004\n\n   END OF TERMS AND CONDITIONS"
ISC = "Copyright 2015 Brian Smith.\n\nPermission to use, copy, modify, and/or distribute this software for any purpose\nTHE SOFTWARE IS PROVIDED \"AS IS\""


class Crate:
    def __init__(self, test, files):
        tmp = tempfile.TemporaryDirectory()
        test.addCleanup(tmp.cleanup)
        self.dir = Path(tmp.name)
        for name, text in files.items():
            path = self.dir / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text, encoding="utf-8")

    def component(self, expression, name="demo", version="1.0.0"):
        return notices.Component(notices.CRATES, name, version, expression, self.dir)


class Expressions(unittest.TestCase):
    def test_or_and_with_and_slash_expand_to_every_option(self):
        options = notices.parse("(MIT OR Apache-2.0) AND Unicode-3.0 / Apache-2.0 WITH LLVM-exception")
        self.assertEqual(options, [frozenset({"MIT", "Unicode-3.0"}), frozenset({"Apache-2.0", "Unicode-3.0"}), frozenset({"Apache-2.0 WITH LLVM-exception"})])

    def test_unbalanced_expression_is_unresolved(self):
        with self.assertRaises(notices.Unresolved):
            notices.parse("(MIT OR Apache-2.0")

    def test_unknown_licence_is_unresolved(self):
        crate = Crate(self, {"LICENSE": MIT})
        with self.assertRaisesRegex(notices.Unresolved, "GPL-3.0-only is a licence this script does not know"):
            notices.resolve(crate.component("GPL-3.0-only"), [crate.dir])


class Choice(unittest.TestCase):
    def test_a_licence_that_needs_no_notice_wins_and_ships_no_text(self):
        crate = Crate(self, {"LICENSE-MIT": MIT, "UNLICENSE": "This is free and unencumbered software"})
        component = crate.component("Unlicense OR MIT")
        notices.resolve(component, [crate.dir])
        self.assertEqual((component.licences, component.texts), (["Unlicense"], []))

    def test_mit_is_preferred_and_its_own_file_is_used(self):
        crate = Crate(self, {"LICENSE-MIT": MIT, "LICENSE-APACHE": APACHE})
        component = crate.component("Apache-2.0 OR MIT")
        notices.resolve(component, [crate.dir])
        self.assertEqual((component.licences, component.texts), (["MIT"], [MIT]))

    def test_falls_back_to_the_next_option_when_a_text_is_missing(self):
        crate = Crate(self, {"LICENSE": ISC})
        component = crate.component("MIT OR ISC")
        notices.resolve(component, [crate.dir])
        self.assertEqual((component.licences, component.texts), (["ISC"], [ISC]))

    def test_conjunction_needs_every_text(self):
        crate = Crate(self, {"LICENSE-MIT": MIT})
        with self.assertRaisesRegex(notices.Unresolved, "used under ISC, but none of its licence files"):
            notices.resolve(crate.component("MIT AND ISC"), [crate.dir])

    def test_crate_without_licence_text_is_unresolved(self):
        crate = Crate(self, {"README.md": "MIT licensed"})
        with self.assertRaisesRegex(notices.Unresolved, r"demo 1.0.0 is used under MIT, but none of its licence files \(none\)"):
            notices.resolve(crate.component("MIT"), [crate.dir])

    def test_apache_ships_the_notice_file(self):
        crate = Crate(self, {"LICENSE": APACHE, "NOTICE": "Demo\nCopyright 2024 Demo Inc."})
        component = crate.component("Apache-2.0")
        notices.resolve(component, [crate.dir])
        self.assertEqual(component.texts, [notices.normalise(APACHE), "Demo\nCopyright 2024 Demo Inc."])

    def test_clarified_texts_must_hold_the_licence(self):
        crate = Crate(self, {"src.js": "// Copyright 2008 Google Inc.\n// Redistribution and use in source and binary forms. Neither the name\n// ANY EXPRESS OR IMPLIED WARRANTIES\ncode()"})
        component = crate.component("Apache-2.0 AND BSD-3-Clause")
        with self.assertRaisesRegex(notices.Unresolved, "do not hold the Apache-2.0 text"):
            notices.resolve(component, [], [("lines", "src.js", 1, 3)])
        notices.TEXTS, saved = crate.dir, notices.TEXTS
        self.addCleanup(setattr, notices, "TEXTS", saved)
        (crate.dir / "apache").write_text(APACHE, encoding="utf-8")
        pinned = hashlib.sha256(APACHE.encode()).hexdigest()
        notices.resolve(component, [], [("vendored", "apache", pinned), ("lines", "src.js", 1, 3)])
        self.assertEqual(component.texts[1], "Copyright 2008 Google Inc.\nRedistribution and use in source and binary forms. Neither the name\nANY EXPRESS OR IMPLIED WARRANTIES")
        (crate.dir / "apache").write_text(APACHE.splitlines()[1], encoding="utf-8")
        with self.assertRaisesRegex(notices.Unresolved, "does not have the SHA-256 CLARIFIED pins"):
            notices.resolve(component, [], [("vendored", "apache", pinned), ("lines", "src.js", 1, 3)])

    def test_a_truncated_licence_file_is_not_that_licence_text(self):
        crate = Crate(self, {"LICENSE-APACHE": "\n".join(APACHE.splitlines()[:2])})
        with self.assertRaisesRegex(notices.Unresolved, "used under Apache-2.0, but none of its licence files"):
            notices.resolve(crate.component("Apache-2.0"), [crate.dir])


class BundledSources(unittest.TestCase):
    def package(self, crate, build_script):
        kinds = [["lib"]] + ([["custom-build"]] if build_script else [])
        return {"manifest_path": str(crate.dir / "Cargo.toml"), "targets": [{"kind": kind} for kind in kinds]}

    def test_c_sources_need_a_build_script_to_count(self):
        crate = Crate(self, {"Cargo.toml": "", "compare/nftw.c": "int main;"})
        self.assertFalse(notices.compiles_bundled_sources(self.package(crate, build_script=False)))
        self.assertTrue(notices.compiles_bundled_sources(self.package(crate, build_script=True)))

    def test_bundled_entry_reads_its_version_and_must_name_files_that_hold_its_licence(self):
        crate = Crate(self, {"c_src/LICENSE": MIT, "c_src/demo.h": "#define DEMO_VERSION 30302\n"})
        parent = crate.component("MIT", name="demo-sys")
        version = ("c_src/demo.h", r"DEMO_VERSION (\d)(\d\d)(\d\d)")
        entry = lambda expression, path: {("demo-sys", "1.0.0"): [("libdemo", version, expression, [("file", path)])]}
        with mock.patch.dict(notices.BUNDLED, entry("MIT", "c_src/LICENSE")):
            [bundled] = notices.bundled_components(parent)
            self.assertEqual((bundled.label(), bundled.texts), ("libdemo 3.3.2 (bundled in demo-sys 1.0.0)", [MIT]))
        with mock.patch.dict(notices.BUNDLED, entry("ISC", "c_src/LICENSE")):
            with self.assertRaisesRegex(notices.Unresolved, "used under ISC, but BUNDLED lists no file"):
                notices.bundled_components(parent)
        with mock.patch.dict(notices.BUNDLED, entry("MIT", "c_src/COPYING")):
            with self.assertRaisesRegex(notices.Unresolved, "names c_src/COPYING"):
                notices.bundled_components(parent)
        (crate.dir / "c_src/demo.h").write_text("#define DEMO_VERSION_STRING \"3.3.2\"\n", encoding="utf-8")
        with mock.patch.dict(notices.BUNDLED, entry("MIT", "c_src/LICENSE")):
            with self.assertRaisesRegex(notices.Unresolved, "reads the version of libdemo from c_src/demo.h"):
                notices.bundled_components(parent)


class Copyrights(unittest.TestCase):
    def test_source_copyright_lines_the_licence_file_lacks_are_shipped(self):
        bare_mit = "Permission is hereby granted, free of charge, to any person obtaining a copy\nWITHOUT WARRANTY OF ANY KIND"
        crate = Crate(self, {
            "LICENSE-MIT": bare_mit,
            "src/lib.rs": "// Copyright 2016 lazy-static.rs Developers\n//\n// Licensed under MIT\nfn f() {}\n",
            "src/inner.rs": "/* Copyright (c) 2019 Someone Else */\n",
            "tests/fixture.rs": "// Copyright 2001 Test Fixture Author\n",
        })
        component = crate.component("MIT")
        notices.resolve(component, [crate.dir])
        self.assertEqual(component.texts, [bare_mit, "Copyright notices in the sources of demo 1.0.0:\n\nCopyright (c) 2019 Someone Else */\nCopyright 2016 lazy-static.rs Developers"])

    def test_copyright_lines_already_in_the_licence_text_are_not_repeated(self):
        crate = Crate(self, {"LICENSE": MIT, "src/lib.rs": "// Copyright (c) 2020 Someone\n"})
        component = crate.component("MIT")
        notices.resolve(component, [crate.dir])
        self.assertEqual(component.texts, [MIT])

    def test_third_party_notice_files_ship_whatever_licence_is_chosen(self):
        crate = Crate(self, {"LICENSE-MIT": MIT, "LICENSE-THIRD-PARTY": "Copyright (c) 2016 Alex Crichton\nApache License"})
        component = crate.component("MIT OR Apache-2.0")
        notices.resolve(component, [crate.dir])
        self.assertEqual(component.texts, [MIT, "Copyright (c) 2016 Alex Crichton\nApache License"])


class Commands(unittest.TestCase):
    def run_main(self, *args, generated="notices\n", committed=None):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        path = Path(tmp.name) / "THIRD-PARTY-NOTICES"
        if committed is not None:
            path.write_text(committed, encoding="utf-8")
        stderr = io.StringIO()
        render = mock.Mock(side_effect=generated) if isinstance(generated, Exception) else mock.Mock(return_value=generated)
        with mock.patch.object(notices, "NOTICES", path), mock.patch.object(notices, "render", render), mock.patch("sys.argv", ["third-party-notices.py", *args]), contextlib.redirect_stderr(stderr), contextlib.redirect_stdout(io.StringIO()):
            code = notices.main()
        return code, stderr.getvalue(), path

    def test_check_fails_on_a_stale_or_missing_file_and_passes_on_a_current_one(self):
        self.assertEqual(self.run_main("check", committed="old\n")[0], 1)
        self.assertEqual(self.run_main("check")[0], 1)
        self.assertEqual(self.run_main("check", committed="notices\n")[0], 0)

    def test_check_fails_when_a_component_is_unresolved(self):
        code, stderr, _ = self.run_main("check", generated=notices.Unresolved("demo has no text"), committed="notices\n")
        self.assertEqual((code, stderr.strip()), (1, "::error::demo has no text"))

    def test_write_regenerates_the_file(self):
        code, _, path = self.run_main("write", committed="old\n")
        self.assertEqual((code, path.read_text(encoding="utf-8")), (0, "notices\n"))

    def test_covers_accepts_only_the_listed_targets(self):
        for target in notices.TARGETS:
            self.assertEqual(self.run_main("covers", target)[0], 0)
        self.assertEqual(self.run_main("covers", "x86_64-pc-windows-msvc")[0], 1)


class CommittedFile(unittest.TestCase):
    def test_every_component_line_points_to_a_text_or_needs_no_notice(self):
        text = notices.NOTICES.read_text(encoding="utf-8")
        listing = text.split("Licence texts\n=============")[0]
        entries = [line for line in listing.splitlines() if ": " in line and (", text " in line or "no notice required" in line)]
        self.assertGreater(len(entries), 200)
        numbers = {int(n) for line in entries if ", text " in line for n in line.split(", text ")[1].split(" [")[0].split(", ")}
        self.assertEqual(numbers, set(range(1, max(numbers) + 1)))
        for number in numbers:
            self.assertIn(f"\nText {number}: ", text)


if __name__ == "__main__":
    unittest.main()
