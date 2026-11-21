#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import argparse
import difflib
import hashlib
import importlib.metadata
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
NOTICES = ROOT / "cli" / "THIRD-PARTY-NOTICES"
TEXTS = Path(__file__).resolve().with_name("third-party-notices")
PACKAGE = "koidrive-cli"
TARGETS = ("aarch64-apple-darwin", "aarch64-unknown-linux-musl", "x86_64-unknown-linux-musl")
MUSL_TARGETS = tuple(t for t in TARGETS if t.endswith("-linux-musl"))
CRATES, RUST, ZIG, SERVER = range(4)

NO_NOTICE = {
    "0BSD",
    "Apache-2.0 WITH LLVM-exception",
    "BSL-1.0",
    "CC0-1.0",
    "MIT-0",
    "Unlicense",
    "Zlib",
    "blessing",
}
NOTICE_MARKERS = {
    "MIT": (("permission is hereby granted, free of charge", "without warranty of any kind"),),
    "ISC": (("permission to use, copy, modify, and/or distribute", "provided \"as is\""), ("permission to use, copy, modify, and distribute", "provided \"as is\"")),
    "BSD-2-Clause": (("redistribution and use in source and binary forms", "any express or implied warranties"),),
    "BSD-3-Clause": (("redistribution and use in source and binary forms", "neither the name", "any express or implied warranties"),),
    "Apache-2.0": (("apache license version 2.0", "grant of copyright license", "end of terms and conditions"),),
    "Unicode-3.0": (("unicode license v3", "permission is hereby granted, free of charge", "without warranty of any kind"),),
}
PREFERENCE = ("MIT", "ISC", "BSD-2-Clause", "BSD-3-Clause", "Apache-2.0", "Unicode-3.0")
LICENCE_FILE = re.compile(r"^(LICEN[CS]E|COPYING|COPYRIGHT|UNLICENSE)", re.IGNORECASE)
EXTRA_NOTICE_FILE = re.compile(r"^NOTICE|THIRD[-_]?PARTY", re.IGNORECASE)
COPYRIGHT_SIGN = r"(?:\(c\)|[©ⓒⒸ])"
COPYRIGHT_ANYWHERE = re.compile(rf"copyright\s*:?\s*(?:{COPYRIGHT_SIGN}|\d{{4}})|{COPYRIGHT_SIGN}\s*\d{{4}}", re.IGNORECASE)
COPYRIGHT_STATEMENT = re.compile(
    rf"^(?:(?:portions|parts|modifications)\s+)?(?:copyright\b|{COPYRIGHT_SIGN})\s*{COPYRIGHT_SIGN}?\s*[:-]?\s*"
    r"(?!(?:notices?|holders?|owners?|and|law|laws|on|assignment|violation|statement|licen[cs]e|to|of|in|is|are|for|by|the|or|with|you)\b)\w",
    re.IGNORECASE,
)
NEEDS_COPYRIGHT = {"MIT", "ISC", "BSD-2-Clause", "BSD-3-Clause"}
UNSHIPPED_DIRS = {"tests", "benches", "examples", "target", ".git"}
COMPILED_SOURCES = (".c", ".cc", ".cpp", ".cxx", ".s", ".S", ".asm", ".m", ".mm")

BUNDLED = {
    ("aws-lc-sys", "0.47.0"): [
        (
            "AWS-LC",
            ("aws-lc/include/openssl/base.h", r'AWSLC_VERSION_NUMBER_STRING "(\d+)\.(\d+)\.(\d+)"'),
            "ISC AND Apache-2.0 AND MIT AND BSD-3-Clause",
            [("file", "aws-lc/LICENSE"), ("file", "aws-lc/third_party/fiat/LICENSE")],
        ),
    ],
    ("libmimalloc-sys", "0.1.49"): [
        ("mimalloc", ("c_src/mimalloc/v3/include/mimalloc.h", r"MI_MALLOC_VERSION (\d)(\d\d)(\d\d)"), "MIT", [("file", "c_src/mimalloc/v3/LICENSE")]),
    ],
    ("libsqlite3-sys", "0.39.1"): [
        ("SQLite", ("sqlite3/sqlite3.h", r'#define SQLITE_VERSION\s+"(\d+)\.(\d+)\.(\d+)"'), "blessing", [("lines", "sqlite3/sqlite3.h", 2, 9)]),
    ],
}
CLARIFIED = {
    ("@bufbuild/protobuf", "2.17.0"): [
        ("vendored", "bufbuild-protobuf-2.17.0-LICENSE", "c04b4216f1cd4c5a4f7fb2f2a1b0ae70d847e9e0cac7c9dee9bf8cc03177c449"),
        ("lines", "dist/esm/wire/varint.js", 1, 32),
    ],
    ("rust", "1.100.0"): [("vendored", "rust-1.100.0-LICENSE-MIT", "b71bd43a069ca0641a9ecfe585ca7b3c53b5cc1608f8b68321168698e28b5ea1")],
}


class Unresolved(Exception):
    pass


class Component:
    def __init__(self, group, name, version, expression, directory, bundled_in=None):
        self.group = group
        self.name = name
        self.version = version
        self.expression = expression
        self.directory = directory
        self.bundled_in = bundled_in
        self.targets = set()
        self.licences = []
        self.texts = []

    def label(self):
        name = f"{self.name} {self.version}" if self.version else self.name
        return f"{name} (bundled in {self.bundled_in})" if self.bundled_in else name


def run(args):
    result = subprocess.run(args, cwd=ROOT, capture_output=True, text=True)
    if result.returncode != 0:
        raise Unresolved(f"{' '.join(args)} failed:\n{result.stderr.strip()}")
    return result.stdout


def parse(expression):
    items = re.findall(r"\(|\)|[^\s()]+", expression.replace("/", " OR "))
    position = 0

    def peek():
        return items[position] if position < len(items) else None

    def take():
        nonlocal position
        if position == len(items):
            raise Unresolved(f"licence expression {expression!r} ends early")
        position += 1
        return items[position - 1]

    def factor():
        if peek() == "(":
            take()
            options = either()
            if take() != ")":
                raise Unresolved(f"unbalanced licence expression {expression!r}")
            return options
        name = take()
        if peek() == "WITH":
            take()
            name = f"{name} WITH {take()}"
        return [frozenset([name])]

    def both():
        options = factor()
        while peek() == "AND":
            take()
            right = factor()
            options = [left | other for left in options for other in right]
        return options

    def either():
        options = both()
        while peek() == "OR":
            take()
            options += both()
        return options

    options = either()
    if position != len(items):
        raise Unresolved(f"cannot parse licence expression {expression!r}")
    return options


def unknown_licences(option):
    return sorted(licence for licence in option if licence not in NO_NOTICE and licence not in NOTICE_MARKERS)


def needing_notice(option):
    unknown = unknown_licences(option)
    if unknown:
        raise Unresolved(f"{', '.join(unknown)} is a licence this script does not know; read its terms and add it to NO_NOTICE or NOTICE_MARKERS")
    return sorted((licence for licence in option if licence in NOTICE_MARKERS), key=PREFERENCE.index)


def flat(text):
    return " ".join(text.split())


def holds(text, licence):
    lowered = flat(text).lower()
    return any(all(marker in lowered for marker in markers) for markers in NOTICE_MARKERS[licence])


def normalise(text):
    lines = [line.rstrip() for line in text.replace("\r\n", "\n").replace("\r", "\n").split("\n")]
    while lines and not lines[0]:
        lines.pop(0)
    while lines and not lines[-1]:
        lines.pop()
    return "\n".join(lines)


def read(path):
    return normalise(path.read_text(encoding="utf-8", errors="replace"))


def files_in(directory, pattern):
    return sorted(entry for entry in directory.iterdir() if entry.is_file() and pattern.search(entry.name))


def licence_texts(component, licences, searched):
    files = next((found for found in (files_in(d, LICENCE_FILE) for d in searched if d.is_dir()) if found), [])
    files = [path for path in files if not EXTRA_NOTICE_FILE.search(path.name)]
    texts = {path: read(path) for path in files}
    chosen = []
    for licence in licences:
        matching = [path for path in files if holds(texts[path], licence)]
        if not matching:
            names = ", ".join(path.name for path in files) or "none"
            raise Unresolved(f"{component.label()} is used under {licence}, but none of its licence files ({names}) holds that licence's text; add the upstream text to CLARIFIED")
        best = min(matching, key=lambda path: (sum(holds(texts[path], other) for other in NOTICE_MARKERS), len(path.name), path.name))
        if best not in chosen:
            chosen.append(best)
    if files:
        chosen += files_in(files[0].parent, EXTRA_NOTICE_FILE)
    return [texts.get(path) or read(path) for path in chosen]


def uncommented(line):
    return re.sub(r"^\s*(?:/\*+|//[/!]?|\*+/?|#+|--|;+|@)\s?", "", line).rstrip()


def source_copyrights(directory):
    found = set()
    for root, dirs, names in os.walk(directory):
        dirs[:] = sorted(d for d in dirs if d not in UNSHIPPED_DIRS)
        for name in names:
            path = Path(root) / name
            if Path(root) == directory and (LICENCE_FILE.search(name) or EXTRA_NOTICE_FILE.search(name)):
                continue
            if path.stat().st_size > 4_000_000:
                continue
            try:
                lines = path.read_text(encoding="utf-8").splitlines()
            except (UnicodeDecodeError, OSError):
                continue
            for line in lines:
                text = " ".join(uncommented(line).split())
                if COPYRIGHT_ANYWHERE.search(text) or COPYRIGHT_STATEMENT.search(text):
                    found.add(text)
    return sorted(found)


def clarified_texts(component, sources):
    texts = []
    for source in sources:
        if source[0] == "vendored":
            _, name, sha256 = source
            data = (TEXTS / name).read_bytes()
            if hashlib.sha256(data).hexdigest() != sha256:
                raise Unresolved(f"scripts/third-party-notices/{name} does not have the SHA-256 CLARIFIED pins for it; restore the upstream text or update the pin after checking it")
            texts.append(normalise(data.decode("utf-8")))
            continue
        if source[0] == "file":
            texts.append(read(component.directory / source[1]))
            continue
        _, relative, first, last = source
        lines = (component.directory / relative).read_text(encoding="utf-8").splitlines()[first - 1 : last]
        texts.append(normalise("\n".join(uncommented(line) for line in lines)))
    return texts


def resolve(component, searched, clarified=None):
    options = parse(component.expression)
    known = [option for option in options if not unknown_licences(option)]
    if not known:
        needing_notice(options[0])
    ranked = sorted(known, key=lambda option: [len(needing_notice(option))] + [PREFERENCE.index(licence) for licence in needing_notice(option)])
    errors = []
    for option in ranked:
        licences = needing_notice(option)
        component.licences = sorted(option)
        if not licences:
            return
        try:
            texts = clarified_texts(component, clarified) if clarified else licence_texts(component, licences, searched)
        except Unresolved as error:
            errors.append(str(error))
            continue
        missing = [licence for licence in licences if not any(holds(text, licence) for text in texts)]
        if missing:
            errors.append(f"the CLARIFIED texts of {component.label()} do not hold the {', '.join(missing)} text")
            continue
        if component.group != ZIG and NEEDS_COPYRIGHT & set(licences):
            shipped = flat("\n".join(texts))
            notices = [line for line in source_copyrights(component.directory) if line not in shipped]
            if notices:
                texts = texts + ["Copyright notices in the sources of " + component.label() + ":\n\n" + "\n".join(notices)]
        component.texts = texts
        return
    raise Unresolved(errors[0])


def compiles_bundled_sources(package):
    if not any("custom-build" in target["kind"] for target in package["targets"]):
        return False
    return any(name.endswith(COMPILED_SOURCES) for _, _, names in os.walk(Path(package["manifest_path"]).parent) for name in names)


def bundled_components(crate):
    components = []
    for name, (version_file, version_pattern), expression, sources in BUNDLED[(crate.name, crate.version)]:
        named = {version_file} | {source[1] for source in sources}
        missing = sorted(path for path in named if not (crate.directory / path).is_file())
        if missing:
            raise Unresolved(f"BUNDLED names {', '.join(missing)} in {crate.label()}, which it does not contain")
        match = re.search(version_pattern, (crate.directory / version_file).read_text(encoding="utf-8", errors="replace"))
        if not match:
            raise Unresolved(f"BUNDLED reads the version of {name} from {version_file} in {crate.label()}, which has no match for {version_pattern}")
        component = Component(CRATES, name, ".".join(str(int(part)) for part in match.groups()), expression, crate.directory, bundled_in=crate.label())
        component.targets = set(crate.targets)
        component.licences = [expression]
        component.texts = clarified_texts(component, sources)
        unmatched = [licence for option in parse(expression) for licence in needing_notice(option) if not any(holds(text, licence) for text in component.texts)]
        if unmatched:
            raise Unresolved(f"{name} in {crate.label()} is used under {', '.join(unmatched)}, but BUNDLED lists no file with that text")
        components.append(component)
    return components


def crates():
    run(["cargo", "fetch", "--locked"])
    metadata = json.loads(run(["cargo", "metadata", "--locked", "--format-version", "1"]))
    packages = {(p["name"], p["version"]): p for p in metadata["packages"]}
    found = {}
    for target in TARGETS:
        tree = run(["cargo", "tree", "--locked", "-p", PACKAGE, "--target", target, "-e", "normal,no-proc-macro", "--prefix", "none", "--no-dedupe", "-f", "{p}"])
        for line in set(tree.split("\n")) - {""}:
            name, version = line.split()[:2]
            package = packages[(name, version[1:])]
            if package["source"] is None:
                continue
            key = (package["name"], package["version"])
            if key not in found:
                if not package.get("license"):
                    raise Unresolved(f"{name} {package['version']} declares no SPDX licence in its Cargo.toml; add its upstream text to CLARIFIED")
                found[key] = (package, Component(CRATES, package["name"], package["version"], package["license"], Path(package["manifest_path"]).parent))
            found[key][1].targets.add(target)
    components = []
    for key, (package, crate) in found.items():
        resolve(crate, [crate.directory], CLARIFIED.get(key))
        components.append(crate)
        if key in BUNDLED:
            components += bundled_components(crate)
        elif compiles_bundled_sources(package):
            raise Unresolved(f"{crate.label()} has a build script and C, C++ or assembly sources; list the licences of what it compiles in BUNDLED")
    return components


def cargo_lock(path):
    packages = []
    for block in path.read_text(encoding="utf-8").split("[[package]]")[1:]:
        fields = dict(re.findall(r'^(name|version|source) = "([^"]*)"', block, re.MULTILINE))
        listed = block.split("dependencies = [", 1)[1].split("]", 1)[0] if "dependencies = [" in block else ""
        packages.append({**fields, "dependencies": re.findall(r'"([^"]+)"', listed)})
    return packages


def manifest_licence(directory):
    match = re.search(r"""^license\s*=\s*["']([^"']+)["']""", (directory / "Cargo.toml").read_text(encoding="utf-8"), re.MULTILINE)
    if not match:
        raise Unresolved(f"{directory / 'Cargo.toml'} declares no licence")
    return match.group(1)


def rust_standard_library():
    sysroot = Path(run(["rustc", "--print", "sysroot"]).strip())
    release = run(["rustc", "--version"]).split()[1]
    library = sysroot / "lib" / "rustlib" / "src" / "rust" / "library"
    if not (library / "Cargo.lock").is_file():
        raise Unresolved("the Rust standard library sources are missing; run `rustup component add rust-src`")
    in_tree = {}
    for manifest in sorted(library.rglob("Cargo.toml")):
        if "vendor" not in manifest.relative_to(library).parts:
            match = re.search(r'^\[package\][^\[]*?^name\s*=\s*"([^"]+)"', manifest.read_text(encoding="utf-8"), re.MULTILINE | re.DOTALL)
            if match:
                in_tree.setdefault(match.group(1), manifest.parent)
    locked = {}
    for package in cargo_lock(library / "Cargo.lock"):
        locked.setdefault(package["name"], []).append(package)
    reachable = {}
    pending = ["std"]
    while pending:
        name, _, version = pending.pop().partition(" ")
        package = next(p for p in locked[name] if not version or p["version"] == version)
        if (package["name"], package["version"]) not in reachable:
            reachable[(package["name"], package["version"])] = package
            pending += package["dependencies"]
    found = {}
    for target in TARGETS:
        lib = sysroot / "lib" / "rustlib" / target / "lib"
        if not lib.is_dir():
            raise Unresolved(f"the Rust standard library for {target} is missing; run `rustup target add {target}`")
        linked = {re.sub(r"-[0-9a-f]+$", "", entry.stem[3:]) for entry in lib.glob("lib*.rlib")}
        for (name, version), package in reachable.items():
            if name.replace("-", "_") not in linked:
                continue
            if (name, version) not in found:
                if package.get("source"):
                    directory = library / "vendor" / f"{name}-{version}"
                    found[(name, version)] = Component(RUST, name, version, manifest_licence(directory), directory)
                else:
                    found[(name, version)] = Component(RUST, f"{name} (Rust {release} standard library)", "", manifest_licence(in_tree[name]), in_tree[name])
            found[(name, version)].targets.add(target)
    for component in found.values():
        if component.version:
            resolve(component, [component.directory])
            continue
        searched = [component.directory] + [p for p in component.directory.parents if p == library or library in p.parents]
        try:
            resolve(component, searched)
        except Unresolved:
            if ("rust", release) not in CLARIFIED:
                raise Unresolved(f"add the Rust {release} LICENSE-MIT from rust-lang/rust to CLARIFIED")
            resolve(component, [], CLARIFIED[("rust", release)])
    return list(found.values())


def zig_toolchain():
    pin = re.search(r"^ziglang==(\S+)", (ROOT / ".github" / "requirements-musl.txt").read_text(encoding="utf-8"), re.MULTILINE).group(1)
    spec = importlib.util.find_spec("ziglang")
    if spec is None:
        raise Unresolved(f"run this script with a Python that has ziglang {pin} from .github/requirements-musl.txt installed")
    installed = importlib.metadata.version("ziglang")
    if installed != pin:
        raise Unresolved(f"ziglang {installed} is installed, but .github/requirements-musl.txt pins {pin}")
    root = Path(spec.origin).parent
    components = [
        Component(ZIG, f"Zig {pin} compiler-rt and libc", "", "MIT", root),
        Component(ZIG, f"musl libc, as bundled with Zig {pin}", "", "MIT", root / "lib" / "libc" / "musl"),
    ]
    for component in components:
        component.targets = set(MUSL_TARGETS)
        resolve(component, [component.directory])
    return components


def server_bundle():
    server = ROOT / "server"
    pending = [(server, name) for name in json.loads((server / "package.json").read_text(encoding="utf-8")).get("dependencies", {})]
    found = {}
    while pending:
        start, name = pending.pop()
        directory = next((d / "node_modules" / name for d in [start, *start.parents] if (d / "node_modules" / name / "package.json").is_file()), None)
        if directory is None:
            raise Unresolved(f"{name}, which the embedded server bundle includes, is not installed; run `pnpm --dir server install --frozen-lockfile`")
        directory = directory.resolve()
        manifest = json.loads((directory / "package.json").read_text(encoding="utf-8"))
        key = (manifest["name"], manifest["version"])
        if key in found:
            continue
        if not manifest.get("license"):
            raise Unresolved(f"{manifest['name']} {manifest['version']} declares no licence in its package.json")
        found[key] = Component(SERVER, manifest["name"], manifest["version"], manifest["license"], directory)
        found[key].targets = set(TARGETS)
        pending += [(directory, dependency) for dependency in manifest.get("dependencies", {})]
    for key, component in found.items():
        resolve(component, [component.directory], CLARIFIED.get(key))
    return list(found.values())


def render():
    components = crates() + rust_standard_library() + zig_toolchain() + server_bundle()
    components.sort(key=lambda c: (c.group, (c.bundled_in or c.label()).lower(), c.bundled_in is not None, c.name.lower()))
    texts = {}
    for component in components:
        for text in component.texts:
            texts.setdefault(text, []).append(component)
    numbers = {text: index for index, text in enumerate(texts, 1)}
    headings = {CRATES: "Rust crates", RUST: "Rust standard library", ZIG: "Zig toolchain runtime (Linux)", SERVER: "Embedded server bundle"}
    out = [
        "Third-party notices for koi, the Koidrive command-line client",
        "",
        "koi includes the third-party software listed below. Each entry names the",
        "licence koi uses it under and, when that licence asks for its copyright",
        "and licence notice to ship with copies, the number of the text below.",
        "",
        f"Release targets: {', '.join(TARGETS)}.",
        "An entry that names targets is linked into those targets only.",
        "",
        "scripts/third-party-notices.py generates this file from Cargo.lock, the",
        "Rust and Zig toolchains and server/package.json.",
    ]
    group = None
    for component in components:
        if component.group != group:
            group = component.group
            out += ["", headings[group], "=" * len(headings[group]), ""]
        used = " AND ".join(component.licences)
        offered = "" if used == component.expression else f" (offered as {component.expression})"
        refs = sorted({numbers[text] for text in component.texts})
        notice = f", text {', '.join(map(str, refs))}" if refs else ", no notice required"
        targets = "" if component.targets == set(TARGETS) else f" [{', '.join(sorted(component.targets))}]"
        out.append(f"{component.label()}: {used}{offered}{notice}{targets}")
    out += ["", "Licence texts", "============="]
    for text, users in texts.items():
        names = ", ".join(sorted({user.label() for user in users}, key=str.lower))
        out += ["", "-" * 72, f"Text {numbers[text]}: {names}", "-" * 72, "", text]
    return "\n".join(out) + "\n"


def main():
    parser = argparse.ArgumentParser(description="Generate or check cli/THIRD-PARTY-NOTICES.")
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("write", help="regenerate cli/THIRD-PARTY-NOTICES")
    commands.add_parser("check", help="fail when cli/THIRD-PARTY-NOTICES is stale or a component has no resolvable notice")
    commands.add_parser("covers", help="fail unless cli/THIRD-PARTY-NOTICES covers a release target").add_argument("target")
    args = parser.parse_args()
    if args.command == "covers":
        if args.target in TARGETS:
            return 0
        print(f"::error::cli/THIRD-PARTY-NOTICES covers {', '.join(TARGETS)}, not {args.target}; add it to TARGETS in scripts/third-party-notices.py and run `python3 scripts/third-party-notices.py write`.", file=sys.stderr)
        return 1
    try:
        generated = render()
    except Unresolved as error:
        print(f"::error::{error}", file=sys.stderr)
        return 1
    if args.command == "write":
        NOTICES.write_text(generated, encoding="utf-8")
        return 0
    committed = NOTICES.read_text(encoding="utf-8") if NOTICES.is_file() else ""
    if committed != generated:
        diff = difflib.unified_diff(committed.splitlines(), generated.splitlines(), "committed", "generated", lineterm="", n=1)
        print("\n".join(list(diff)[:80]), file=sys.stderr)
        print("::error::cli/THIRD-PARTY-NOTICES does not match what Cargo.lock and the toolchains give; run `python3 scripts/third-party-notices.py write` with ziglang installed and commit the result.", file=sys.stderr)
        return 1
    print(f"cli/THIRD-PARTY-NOTICES is current for {', '.join(TARGETS)}.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
