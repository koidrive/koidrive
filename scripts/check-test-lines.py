#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
import argparse
import fnmatch
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

TOKEN = re.compile(r"""//[^\n]*|/\*|(?<![A-Za-z0-9_])[bc]?r(#*)"[\s\S]*?"\1|"(?:\\[\s\S]|[^"\\])*"|'(?:\\(?:u\{[0-9a-fA-F]*\}|x[0-9a-fA-F]{2}|.)|[^\\'\n])'""")
CFG = re.compile(r"#(!?)\s*\[\s*cfg\s*\(")
ATTRIBUTE = re.compile(r"#\s*\[")
PATH_ATTRIBUTE = re.compile(r'#\s*\[\s*path\s*=\s*"([^"]*)"\s*\]')
MOD_DECLARATION = re.compile(r"\b(?:pub(?:\s*\([^)]*\))?\s+)?mod\s+(?:r#)?([A-Za-z_][A-Za-z0-9_]*)\s*;")
INLINE_MODULE = re.compile(r"\bmod\s+(?:r#)?([A-Za-z_][A-Za-z0-9_]*)\s*\{")
RECORD_NAME = re.compile(r"pr-([0-9]+)\.txt")
VISIBILITY = re.compile(r"(?:pub(?:\s*\([^)]*\))?\s+)?")
UNTIL_SEMICOLON = re.compile(r"(?:const|static|let|type)\b(?!\s+(?:fn|unsafe|async|extern)\b)")
ITEM = re.compile(r"(?:(?:fn|impl|struct|enum|union|mod|use|trait|extern|unsafe|async)\b|[A-Za-z_][A-Za-z0-9_]*!)")
EXPRESSION_CONTINUES = re.compile(r"\s*[,;.?)\]}]|\s*else\b|\s*as\b")
FILE_ATTRIBUTES_ONLY = re.compile(r"\s*(?:#!\s*\[[^\]]*\]\s*)*")
TEST_DIR = re.compile(r"(^|/)(tests|benches)/")
TEST_CRATE = re.compile(r"(^|/)test-support/")
CRATE_ROOT = re.compile(r"(^|/)((lib|main|mod|build)\.rs|(tests|benches|examples|bin)/[^/]+\.rs)$")
REGRESSION = re.compile(r"\.proptest-regressions$|(^|/)proptest-regressions/|\.cases/")
SNAPSHOT = re.compile(r"\.snap(\.new)?$|(^|/)(goldens|snapshots)/")
FIXTURE = re.compile(r"(^|/)(tests|fixtures|testdata|test-data)/")
RUST_FIXTURE = re.compile(r"(^|/)(fixtures|testdata|test-data)/")
ANY_MODULE = re.compile(r"\bmod\b")
GENERIC_BEFORE = re.compile(r"[A-Za-z0-9_:]")
TEST_ATTRIBUTE = re.compile(r"#\s*\[\s*(?:[A-Za-z_][A-Za-z0-9_]*\s*::\s*)*test\s*\]")


def git(*args, input=None):
    return subprocess.run(["git", *args], check=True, capture_output=True, input=input).stdout


def mask(src):
    out = []
    pos = 0
    while True:
        m = TOKEN.search(src, pos)
        if not m:
            out.append(src[pos:])
            return "".join(out)
        out.append(src[pos:m.start()])
        end = m.end()
        if m.group() == "/*":
            depth = 1
            while depth and end < len(src):
                if src.startswith("/*", end):
                    depth += 1
                    end += 2
                elif src.startswith("*/", end):
                    depth -= 1
                    end += 2
                else:
                    end += 1
        out.append(re.sub(r"[^\n]", " ", src[m.start():end]))
        pos = end


def balanced(code, start, open_char, close_char):
    depth = 0
    for i in range(start, len(code)):
        if code[i] == open_char:
            depth += 1
        elif code[i] == close_char:
            depth -= 1
            if depth == 0:
                return i + 1
    return len(code)


def top_level_args(predicate):
    args, depth, current = [], 0, ""
    for c in predicate:
        if c == "," and depth == 0:
            args.append(current.strip())
            current = ""
            continue
        depth += c == "("
        depth -= c == ")"
        current += c
    return [*args, current.strip()]


def test_only(predicate):
    predicate = re.sub(r"\s+", "", predicate)
    if predicate == "test":
        return True
    if predicate.startswith("all(") and predicate.endswith(")"):
        return any(test_only(arg) for arg in top_level_args(predicate[4:-1]))
    return False


def item_end(code, start):
    pos = start
    while True:
        while pos < len(code) and code[pos].isspace():
            pos += 1
        if not ATTRIBUTE.match(code, pos):
            break
        pos = balanced(code, code.index("[", pos), "[", "]")
    keyword = VISIBILITY.match(code, pos).end()
    until_semicolon = bool(UNTIL_SEMICOLON.match(code, keyword))
    item = until_semicolon or bool(ITEM.match(code, keyword))
    opening, closing = ("([", ")]") if item else ("([{", ")]}")
    depth = angle = 0
    while pos < len(code):
        c = code[pos]
        if c in "->" and code.startswith(("->", "=>"), pos - (c == ">")):
            pass
        elif not item and c == "<" and GENERIC_BEFORE.match(code[pos - 1]):
            angle += 1
        elif not item and c == ">" and angle:
            angle -= 1
        elif c in opening:
            depth += 1
        elif c in closing:
            depth -= 1
            if depth < 0:
                return pos
            if c == "}" and depth == 0 and not EXPRESSION_CONTINUES.match(code, pos + 1):
                return pos + 1
        elif c == ";" and depth == 0:
            return pos + 1
        elif c == "," and depth == 0 and angle == 0 and not item:
            return pos + 1
        elif c == "{" and depth == 0:
            pos = balanced(code, pos, "{", "}")
            if not until_semicolon:
                return pos
            continue
        pos += 1
    return pos


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


def path_override(code, src, start):
    end = start
    while True:
        close = code.rfind("]", 0, end)
        if close < 0 or code[close + 1:end].strip():
            return None
        depth, opening = 0, close
        while opening >= 0:
            depth += {"]": 1, "[": -1}.get(code[opening], 0)
            if depth == 0:
                break
            opening -= 1
        hash_mark = code.rfind("#", 0, opening)
        if hash_mark < 0 or code[hash_mark + 1:opening].strip():
            return None
        attribute = PATH_ATTRIBUTE.match(src, hash_mark)
        if attribute:
            return attribute.group(1)
        end = hash_mark


def scan_rust(src):
    code = mask(src)
    line_of = lambda offset: code.count("\n", 0, offset)
    ranges = []
    whole_file = False
    for m in CFG.finditer(code):
        close = balanced(code, m.end() - 1, "(", ")")
        if not test_only(code[m.end():close - 1]):
            continue
        if m.group(1):
            if FILE_ATTRIBUTES_ONLY.fullmatch(code, 0, m.start()):
                whole_file = True
            continue
        ranges.append((m.start(), item_end(code, code.index("]", close) + 1)))
    ranges.extend((m.start(), item_end(code, m.end())) for m in TEST_ATTRIBUTE.finditer(code))
    blocks = [(m.start(), balanced(code, m.end() - 1, "{", "}"), path_override(code, src, m.start()) or m.group(1))
              for m in INLINE_MODULE.finditer(code)]
    declarations, test_declarations = [], []
    for m in MOD_DECLARATION.finditer(code):
        inline = tuple(name for start, end, name in blocks if start < m.start() < end)
        in_test = any(start <= m.start() < end for start, end in ranges)
        (test_declarations if in_test else declarations).append((inline, m.group(1), path_override(code, src, m.start())))
    covered = set()
    for start, end in ranges:
        covered.update(range(line_of(start), line_of(end - 1) + 1))
    return {"whole_file": whole_file, "test_lines": len(covered), "modules": declarations, "test_modules": test_declarations}


EMPTY_SCAN = {"whole_file": False, "test_lines": 0, "modules": [], "test_modules": []}


def line_count(data):
    if b"\0" in data:
        return 0
    return data.count(b"\n") + (1 if data and not data.endswith(b"\n") else 0)


def workspace_members(cargo_toml, paths):
    block = re.search(r"(?s)\[workspace\].*?members\s*=\s*\[(.*?)\]", cargo_toml)
    patterns = [member.rstrip("/") for member in re.findall(r'"([^"]+)"', block.group(1))] if block else []
    packages = {posixpath.dirname(path) for path in paths if path.endswith("/Cargo.toml")}
    members = set()
    for pattern in patterns:
        if not re.search(r"[*?\[]", pattern):
            members.add(pattern)
            continue
        depth = pattern.count("/")
        members.update(p for p in packages if p.count("/") == depth and fnmatch.fnmatchcase(p, pattern))
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
    scans, modules, test_modules = {}, {}, {}
    for path in rust:
        blob = tree.blobs[path]
        if blob not in Tree.scans:
            src = data[path].decode("utf-8", "replace")
            Tree.scans[blob] = scan_rust(src) if "test" in src or ANY_MODULE.search(src) else EMPTY_SCAN
        scans[path] = Tree.scans[blob]
        modules[path] = {module_file(path, d, tree.blobs) for d in scans[path]["modules"]}
        test_modules[path] = {module_file(path, d, tree.blobs) for d in scans[path]["test_modules"]}
    test_files = test_only_files(scans, modules, test_modules)
    totals = Counter({category: 0 for category in CATEGORIES})
    for path in paths:
        lines = line_count(data[path])
        if path.endswith(".rs") and RUST_FIXTURE.search(path):
            totals["fixture"] += lines
        elif path.endswith(".rs"):
            test = lines if path in test_files else min(lines, scans[path]["test_lines"])
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
    if over > 0:
        messages.append(
            f"This change adds {delta['test']} net test lines; the allowance is {allowance}"
            + (f" plus {granted} from records" if granted else "")
            + f". Remove or consolidate {over} test lines, or add {RECORDS}pr-{pr if pr is not None else '<number>'}.txt holding the line count and # lines that say why these tests are needed."
        )
    for message in messages:
        print(f"::warning title=Test-line budget::{message}")
        summary += f"\n> [!WARNING]\n> {message}\n"
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
