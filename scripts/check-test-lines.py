#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
import argparse
import fnmatch
import json
import os
import posixpath
import re
import subprocess
import sys
from collections import Counter

ALLOWANCE = 400
RECORDS = ".github/test-line-allowances/"
CATEGORIES = ("test", "implementation", "fixture", "regression", "snapshot")
SCOPE_EXTRA = ("spec/testdata/", "spec/vectors/")
FN_DOCS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "fn-docs", "Cargo.toml")

RECORD_NAME = re.compile(r"pr-([0-9]+)\.txt")
TEST_DIR = re.compile(r"(^|/)(tests|benches)/")
TEST_CRATE = re.compile(r"(^|/)test-support/")
CRATE_ROOT = re.compile(r"(^|/)((lib|main|mod|build)\.rs|(tests|benches|examples|bin)/[^/]+\.rs)$")
REGRESSION = re.compile(r"\.proptest-regressions$|(^|/)proptest-regressions/|\.cases/")
SNAPSHOT = re.compile(r"\.snap(\.new)?$|(^|/)(goldens|snapshots)/")
FIXTURE = re.compile(r"(^|/)(tests|fixtures|testdata|test-data)/")
RUST_FIXTURE = re.compile(r"(^|/)(fixtures|testdata|test-data)/")


def git(*args, input=None):
    return subprocess.run(["git", *args], check=True, capture_output=True, input=input).stdout


def fn_docs():
    if os.environ.get("FN_DOCS"):
        return os.environ["FN_DOCS"]
    build = subprocess.run(
        ["cargo", "build", "--quiet", "--release", "--locked", "--message-format=json", "--manifest-path", FN_DOCS],
        cwd=os.path.dirname(FN_DOCS), check=True, capture_output=True, text=True,
    ).stdout
    os.environ["FN_DOCS"] = next(
        message["executable"]
        for message in map(json.loads, build.splitlines())
        if message.get("reason") == "compiler-artifact" and message.get("executable")
    )
    return os.environ["FN_DOCS"]


def scan_rust(blobs):
    if not blobs:
        return {}
    request = b"".join(b"%s %d\n%s" % (blob.encode(), len(data), data) for blob, data in blobs.items())
    output = subprocess.run([fn_docs(), "test-spans"], input=request, check=True, capture_output=True).stdout.decode()
    scans = {}
    for row in output.splitlines():
        kind, _, rest = row.partition("\t")
        if kind == "file":
            scan = scans[rest] = {"whole_file": False, "test_lines": 0, "modules": [], "test_modules": [], "error": None}
            covered = set()
        elif kind == "end":
            scan["test_lines"] = len(covered)
        elif kind == "whole":
            scan["whole_file"] = True
        elif kind == "range":
            first, last = map(int, rest.split("\t"))
            covered.update(range(first, last + 1))
        elif kind == "mod":
            test, name, explicit, inline = rest.split("\t")
            declaration = (tuple(filter(None, inline.split("\x1f"))), name, None if explicit == "-" else explicit)
            scan["test_modules" if test == "1" else "modules"].append(declaration)
        elif kind == "error":
            scan["error"] = rest
    return scans


def module_file(path, declaration, files):
    inline, name, explicit = declaration
    directory = posixpath.dirname(path)
    if explicit is not None and not inline:
        return posixpath.normpath(posixpath.join(directory, explicit))
    if not CRATE_ROOT.search(path):
        directory = posixpath.join(directory, posixpath.basename(path)[:-3])
    directory = posixpath.join(directory, *inline)
    if explicit is not None:
        return posixpath.normpath(posixpath.join(directory, explicit))
    flat = posixpath.join(directory, name + ".rs")
    nested = posixpath.join(directory, name, "mod.rs")
    return nested if nested in files and flat not in files else flat



def line_count(data):
    if b"\0" in data:
        return 0
    return data.count(b"\n") + (1 if data and not data.endswith(b"\n") else 0)


def toml_strings(cargo_toml, key):
    workspace = re.search(r"(?ms)^\[workspace\]\s*$(.*?)(?=^\[|\Z)", cargo_toml)
    start = workspace and re.search(rf"(?m)^\s*{key}\s*=\s*\[", workspace.group(1))
    if not start:
        return []
    token = r'#[^\n]*|"((?:[^"\\]|\\.)*)"|\'([^\'\n]*)\''
    array = re.match(rf"(?:\s|,|{token})*\]", workspace.group(1)[start.end():])
    if not array:
        return []
    return [basic or literal for basic, literal in re.findall(token, array.group(0)) if basic or literal]


def workspace_members(cargo_toml, paths):
    packages = {posixpath.dirname(path) for path in paths if path.endswith("/Cargo.toml")}
    members = set()
    for pattern in (member.rstrip("/") for member in toml_strings(cargo_toml, "members")):
        if not re.search(r"[*?\[]", pattern):
            members.add(pattern)
            continue
        depth = pattern.count("/")
        members.update(p for p in packages if p.count("/") == depth and fnmatch.fnmatchcase(p, pattern))
    members -= {member.rstrip("/") for member in toml_strings(cargo_toml, "exclude")}
    return tuple(sorted(member + "/" for member in members))


class Tree:
    cache = {}
    scans = {}

    def __init__(self, rev):
        listing = git("ls-tree", "-r", "-z", rev).split(b"\0")
        self.blobs = {}
        for entry in filter(None, listing):
            meta, path = entry.split(b"\t", 1)
            mode, kind, sha = meta.split()
            if kind == b"blob" and mode != b"120000":
                self.blobs[path.decode()] = sha.decode()
        self.rev = rev
        members = workspace_members(self.read("Cargo.toml").decode(), self.blobs) if "Cargo.toml" in self.blobs else ()
        self.scope = members + SCOPE_EXTRA
        self.unparsed = set()

    def read(self, path):
        return self.contents([path])[path]

    def contents(self, paths):
        missing = [p for p in paths if self.blobs[p] not in Tree.cache]
        if missing:
            out = git("cat-file", "--batch", input="".join(self.blobs[p] + "\n" for p in missing).encode())
            pos = 0
            for _ in missing:
                header_end = out.index(b"\n", pos)
                sha, _, size = out[pos:header_end].split()
                body_start = header_end + 1
                Tree.cache[sha.decode()] = out[body_start:body_start + int(size)]
                pos = body_start + int(size) + 1
        return {p: Tree.cache[self.blobs[p]] for p in paths}


def test_only_files(scans, modules, test_modules):
    test = {}

    def walk(path, under_test):
        if path not in scans:
            return
        under_test = under_test or bool(TEST_DIR.search(path) or TEST_CRATE.search(path) or scans[path]["whole_file"])
        if path in test and (not test[path] or under_test):
            return
        test[path] = under_test
        for child in modules[path]:
            walk(child, under_test)
        for child in test_modules[path]:
            walk(child, True)

    declared = set().union(*modules.values(), *test_modules.values())
    for root in sorted(scans, key=lambda path: path in declared):
        if root not in test:
            walk(root, False)
    return {path for path, is_test in test.items() if is_test}


def count(tree):
    paths = [p for p in tree.blobs if p.startswith(tree.scope)]
    data = tree.contents(paths)
    rust = [p for p in paths if p.endswith(".rs")]
    Tree.scans.update(scan_rust({tree.blobs[p]: data[p] for p in rust if tree.blobs[p] not in Tree.scans}))
    scans, modules, test_modules = {}, {}, {}
    for path in rust:
        scans[path] = Tree.scans[tree.blobs[path]]
        if scans[path]["error"] and not RUST_FIXTURE.search(path):
            tree.unparsed.add(f"{path}:{scans[path]['error']}")
        modules[path] = {module_file(path, d, tree.blobs) for d in scans[path]["modules"]}
        test_modules[path] = {module_file(path, d, tree.blobs) for d in scans[path]["test_modules"]}
    test_files = test_only_files(scans, modules, test_modules)
    totals = Counter({category: 0 for category in CATEGORIES})
    for path in paths:
        lines = line_count(data[path])
        if path.endswith(".rs") and RUST_FIXTURE.search(path):
            totals["fixture"] += lines
        elif path.endswith(".rs"):
            test = 0 if scans[path]["error"] else lines if path in test_files else min(lines, scans[path]["test_lines"])
            totals["test"] += test
            totals["implementation"] += lines - test
        elif REGRESSION.search(path):
            totals["regression"] += lines
        elif SNAPSHOT.search(path):
            totals["snapshot"] += lines
        elif FIXTURE.search(path) or path.startswith(SCOPE_EXTRA):
            totals["fixture"] += lines
    return totals


def records(tree):
    found = {}
    for path in tree.blobs:
        if not path.startswith(RECORDS):
            continue
        data = tree.read(path)
        text = data.decode("utf-8", "replace")
        values = [line.strip() for line in text.splitlines() if line.strip() and not line.strip().startswith("#")]
        reason = " ".join(line.strip().lstrip("#").strip() for line in text.splitlines() if line.strip().startswith("#")).strip()
        lines = int(values[0]) if len(values) == 1 and re.fullmatch(r"[0-9]+", values[0]) else None
        found[path] = (lines, reason, data)
    return found


def credit(base, head, pr):
    granted, problems = 0, []
    before = records(base)
    moved = {data for _, _, data in before.values()}
    for path, (lines, reason, data) in sorted(records(head).items()):
        if path in before and before[path][2] == data:
            continue
        name = RECORD_NAME.fullmatch(path[len(RECORDS):])
        if not name:
            problems.append(f"{path} gives no credit. Name a record after its pull request: {RECORDS}pr-<number>.txt.")
            continue
        if pr is not None and int(name.group(1)) != pr:
            continue
        if path not in before and data in moved:
            problems.append(f"{path} repeats a record already on the base, so it gives no credit.")
            continue
        if lines is None:
            problems.append(f"{path} must hold exactly one non-negative integer line, plus # lines that give the reason.")
            continue
        if not reason:
            problems.append(f"{path} grants {lines} lines but gives no reason. Add # lines that say why this change needs the tests.")
            continue
        granted += max(0, lines - (before.get(path, (None,))[0] or 0))
    return granted, problems


def report(base, head, allowance, pr=None):
    base_tree, head_tree = Tree(base), Tree(head)
    before, after = count(base_tree), count(head_tree)
    delta = {category: after[category] - before[category] for category in CATEGORIES}
    granted, problems = credit(base_tree, head_tree, pr)
    over = delta["test"] - allowance - granted
    rows = "\n".join(f"| {category} | {delta[category]:+d} | {after[category]} |" for category in CATEGORIES)
    total = after["test"] + after["implementation"]
    share = 100 * after["test"] / total if total else 0
    summary = (
        f"## Rust test lines\n\n| Lines | This change | Tree |\n|---|---|---|\n{rows}\n\n"
        f"Test code is {share:.1f}% of Rust source. This change adds {delta['test']:+d} net test lines "
        f"against an allowance of {allowance}"
        + (f" plus {granted} from allowance records" if granted else "")
        + ". Fixtures, proptest regressions and snapshots are listed but do not count against it.\n"
    )
    messages = list(problems)
    for unparsed in sorted(head_tree.unparsed - base_tree.unparsed):
        messages.append(f"{unparsed}. The file does not parse, so its lines count as implementation.")
    if over > 0:
        messages.append(
            f"This change adds {delta['test']} net test lines; the allowance is {allowance}"
            + (f" plus {granted} from records" if granted else "")
            + f". Remove or consolidate {over} test lines, or add {RECORDS}pr-{pr if pr is not None else '<number>'}.txt holding the line count and # lines that say why these tests are needed."
        )
    for message in messages:
        encoded = message.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")
        print(f"::warning title=Test-line budget::{encoded}")
        summary += f"\n> [!WARNING]\n> {' '.join(message.splitlines())}\n"
    print(summary)
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as f:
            f.write(summary)
    return delta, granted, over


def main():
    parser = argparse.ArgumentParser(description="Report the net Rust test lines a change adds. Warns above the allowance and never fails.")
    parser.add_argument("base", nargs="?", help="defaults to the merge base of HEAD and origin/main")
    parser.add_argument("head", nargs="?", default="HEAD")
    parser.add_argument("--pr", type=int, help="credit only the pull request's own record, pr-<number>.txt")
    args = parser.parse_args()
    base = args.base or git("merge-base", args.head, "origin/main").decode().strip()
    report(base, args.head, ALLOWANCE, args.pr)


if __name__ == "__main__":
    main()
