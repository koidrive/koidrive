#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import argparse
from collections import Counter
import difflib
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import sys


ROOT = Path(__file__).resolve().parent.parent
CONFIG = ROOT / "scripts/state-io"
HELPER = "core/state-file/src/lib.rs"
LOCK_LINTS = {"clippy::await_holding_lock", "clippy::await_holding_refcell_ref"}
IO_LINT = "clippy::disallowed_methods"
FORCED_LINTS = [IO_LINT, *sorted(LOCK_LINTS)]
IDENTIFIER = re.compile(r"\b[A-Za-z_]\w*\b")
SHADOWABLE = {"fs", "tempfile"}
ROOTS = {"crate", "self", "super", "Self"}
CONDITIONAL = re.compile(r"#\s*!?\s*\[\s*cfg(?:_attr)?\s*\([^]]*\)\s*\]", re.S)
LEXER_SPEC = importlib.util.spec_from_file_location("rust_comments", ROOT / "scripts/check-new-comments.py")
LEXER = importlib.util.module_from_spec(LEXER_SPEC)
LEXER_SPEC.loader.exec_module(LEXER)

# Lifecycle and historical setup cleanup follow-ups: GUN-928 and GUN-930.
LIFECYCLE_EXCEPTION = {
    "name": "lifecycle checkpoint publication",
    "file": "core/driver/src/lifecycle.rs",
    "line": 233,
    "function_start": 222,
    "function_end": 240,
    "function_sha256": "71663a7a398a485e912b332633084934293b16a20ece23968efa1fc9d8e60182",
    "follow_up": "GUN-928",
    "reason": "The restart regression faults the legacy staging name; migrate both after the protected driver changes land.",
}
SETUP_LEGACY_EXCEPTION = {
    "name": "historical setup stage",
    "file": "cli/src/setup/state.rs",
    "artifact": "setup-state.json.tmp",
    "follow_up": "GUN-930",
    "reason": "Setup state has no cross-process lease, so an old CLI's temp file cannot be distinguished from an orphan.",
}


def git(*arguments):
    return subprocess.check_output(["git", *arguments], cwd=ROOT)


def source(ref, path):
    result = subprocess.run(["git", "show", f"{ref}:{path}"], cwd=ROOT, capture_output=True)
    if result.returncode == 0:
        return result.stdout.decode("utf-8")
    exists = subprocess.run(["git", "cat-file", "-e", ref], cwd=ROOT, capture_output=True)
    if exists.returncode:
        raise ValueError(f"Cannot read base {ref}: {exists.stderr.decode().strip()}")
    return ""


def unchanged_lines(before, after):
    old, new = before.splitlines(), after.splitlines()
    unchanged = {}
    for match in difflib.SequenceMatcher(None, old, new, autojunk=False).get_matching_blocks():
        unchanged.update({match.b + offset + 1: match.a + offset + 1 for offset in range(match.size)})
    return unchanged


def call_tokens(text, line, column):
    lines = text.splitlines(keepends=True)
    offset = sum(map(len, lines[:line - 1])) + column - 1
    tokens = []
    LEXER.comment_spans(text[offset:], "rust", tokens)
    call, closing, macro = [], [], False
    delimiters = {"(": ")", "[": "]", "{": "}"}
    for token, _, _ in tokens:
        if not closing and token in (";", ",", "=", "}"):
            return None
        call.append(token)
        if not closing and token == "!":
            macro = True
        if token in delimiters and (closing or token == "(" or macro):
            closing.append(delimiters[token])
        elif closing and token in delimiters.values():
            if closing.pop() != token:
                return None
            if not closing:
                return call
    return None


def import_items(text):
    tokens = []
    LEXER.comment_spans(text, "rust", tokens)
    words = []
    for token, _, _ in tokens:
        if len(words) > 1 and words[-2:] == ["r", "#"]:
            words[-2:] = [token]
        else:
            words.append(token)
    for index, word in enumerate(words):
        if word == "mod":
            yield tuple(words[index:index + 2])
        elif word == "use" or word == "extern" and words[index + 1:index + 2] == ["crate"]:
            end = words.index(";", index) if ";" in words[index:] else len(words)
            yield tuple(words[index:end])


def import_bindings(item):
    bound, roots = set(), set()

    def tree(i, prefix):
        path = list(prefix)
        while i < len(rest) and rest[i] not in (",", "}", "as"):
            token = rest[i]
            i += 1
            if token == "*":
                roots.update(local_root(path))
                bound.add("*")
                return i
            if token == "{":
                while rest[i] != "}":
                    i = tree(i, path)
                    i += rest[i] == ","
                return i + 1
            if token != ":":
                path.append(token)
        if i < len(rest) and rest[i] == "as":
            bound.add(rest[i + 1])
            i += 2
        elif path:
            bound.add(path[-2] if path[-1] == "self" and len(path) > 1 else path[-1])
        roots.update(local_root(path))
        return i

    try:
        kind, rest = item[0], item[1:]
        if kind == "mod":
            return {rest[0]}, set()
        if kind == "extern":
            return {rest[-1]}, set()
        tree(0, [])
    except IndexError:
        return {"*"}, set()
    if "_" in bound:
        bound.add("*")
    return bound, roots


def local_root(path):
    return [segment for segment in path if segment not in ("self", "super")][:1]


def shadowing_imports(text, call):
    names = SHADOWABLE | {token for token in call if IDENTIFIER.fullmatch(token)} - ROOTS
    items = [(item, *import_bindings(item)) for item in import_items(text)]
    selected = set()
    while True:
        found = {index for index, (_, bound, _) in enumerate(items)
                 if index not in selected and ("*" in bound or bound & names)}
        if not found:
            return sorted(items[index][0] for index in selected)
        selected |= found
        names |= set().union(*(items[index][2] for index in found)) - ROOTS


def suppression_records(text):
    tokens = []
    comments = LEXER.comment_spans(text, "rust", tokens)
    masked = list(text)
    for start, end in comments:
        masked[start:end] = ["\n" if char == "\n" else " " for char in text[start:end]]
    masked = "".join(masked)
    words, locations, cursor = [], [], 0
    for word, _, _ in tokens:
        offset = masked.find(word, cursor)
        if offset < 0:
            raise ValueError("Cannot locate a Rust token while checking suppressions")
        cursor = offset + len(word)
        words.append(word)
        locations.append(text.count("\n", 0, offset) + 1)
    for index, word in enumerate(words):
        if word != "#":
            continue
        opening = index + 1
        if opening < len(words) and words[opening] == "!":
            opening += 1
        if opening >= len(words) or words[opening] != "[":
            continue
        end, depth = opening + 1, 1
        while end < len(words) and depth:
            depth += (words[end] == "[") - (words[end] == "]")
            end += 1
        for position in range(opening + 1, end - 1):
            if words[position] not in ("allow", "expect") or words[position + 1] != "(":
                continue
            closing, depth = position + 2, 1
            while closing < end and depth:
                depth += (words[closing] == "(") - (words[closing] == ")")
                closing += 1
            arguments = words[position + 2:closing - 1]
            blocked = set()
            for item, argument in enumerate(arguments):
                if argument == "warnings":
                    blocked.add("warnings")
                if arguments[item:item + 3] == ["clippy", ":", ":"] and item + 3 < len(arguments):
                    lint = arguments[item + 3]
                    if lint == "disallowed_method":
                        lint = "disallowed_methods"
                    if lint in {"disallowed_methods", "await_holding_lock", "await_holding_refcell_ref", "all", "style", "suspicious"}:
                        blocked.add("clippy::" + lint)
            if blocked:
                yield tuple(sorted(blocked)), locations[position]


def new_suppressions(before, after):
    old = [record for record, _ in suppression_records(before)]
    for record, line in suppression_records(after):
        if record in old:
            old.remove(record)
        else:
            yield line


def lifecycle_exception(span, diagnostic, text):
    policy = LIFECYCLE_EXCEPTION
    if span["file_name"] != policy["file"] or span["line_start"] != policy["line"]:
        return False
    if diagnostic.get("message") != "use of a disallowed method `std::fs::rename`":
        return False
    function = "\n".join(text.splitlines()[policy["function_start"] - 1:policy["function_end"]])
    return hashlib.sha256(function.encode()).hexdigest() == policy["function_sha256"]


def span_chains(diagnostic):
    for span in diagnostic["spans"]:
        if not span["is_primary"]:
            continue
        chain = []
        while span is not None:
            chain.append(span)
            span = (span.get("expansion") or {}).get("span")
        yield chain


def introduced(diagnostic, files, build_script=False):
    code = (diagnostic.get("code") or {}).get("code")
    if code in LOCK_LINTS:
        return True
    if code != IO_LINT:
        return False
    spans = [span for chain in span_chains(diagnostic) for span in chain]
    if not spans:
        raise ValueError("A state-I/O diagnostic has no source span")
    for span in spans:
        path = span["file_name"]
        if path == HELPER or build_script:
            continue
        if path == LIFECYCLE_EXCEPTION["file"]:
            text = files[path][1] if path in files else (ROOT / path).read_text()
            if lifecycle_exception(span, diagnostic, text):
                continue
            return True
        if path not in files:
            continue
        before, after = files[path]
        if CONDITIONAL.findall(before) != CONDITIONAL.findall(after):
            return True
        unchanged = unchanged_lines(before, after)
        if any(line not in unchanged for line in range(span["line_start"], span["line_end"] + 1)):
            return True
        column = span.get("column_start", 1)
        old_call = call_tokens(before, unchanged[span["line_start"]], column)
        new_call = call_tokens(after, span["line_start"], column)
        if old_call is None or new_call is None or old_call != new_call:
            return True
        if shadowing_imports(before, new_call) != shadowing_imports(after, new_call):
            return True
    return False


def relative_path(path, root):
    path = Path(path)
    if path.is_absolute():
        try:
            return path.resolve().relative_to(root.resolve()).as_posix()
        except ValueError:
            return path.as_posix()
    return path.as_posix()


def diagnostic_key(message, files=None, root=ROOT):
    diagnostic, target = message["message"], message["target"]
    chains = []
    for chain in span_chains(diagnostic):
        locations = []
        for span in chain:
            path = relative_path(span["file_name"], root)
            start, end = span["line_start"], span["line_end"]
            if files is not None and path in files:
                unchanged = unchanged_lines(*files[path])
                mapped = [unchanged.get(line) for line in range(start, end + 1)]
                if not mapped or None in mapped or mapped != list(range(mapped[0], mapped[0] + len(mapped))):
                    return None
                start, end = mapped[0], mapped[-1]
            locations.append((path, start, end, span.get("column_start", 1), span.get("column_end", 1),
                              (span.get("expansion") or {}).get("macro_decl_name", "")))
        chains.append(tuple(locations))
    if not chains:
        raise ValueError("A state-I/O diagnostic has no source span")
    return (diagnostic["code"]["code"], diagnostic["message"], target["name"], tuple(target["kind"]),
            relative_path(target["src_path"], root), tuple(sorted(chains)))


def compiler_introduced(message, files, baseline):
    diagnostic = message["message"]
    build_script = "custom-build" in message["target"]["kind"]
    if introduced(diagnostic, files, build_script):
        return True
    if (diagnostic.get("code") or {}).get("code") != IO_LINT or build_script:
        return False
    if all(span["file_name"] in {HELPER, LIFECYCLE_EXCEPTION["file"]}
           for chain in span_chains(diagnostic) for span in chain):
        return False
    key = diagnostic_key(message, files)
    if key is None or not baseline[key]:
        return True
    baseline[key] -= 1
    return False


def compile_inventory(root, env, label):
    command = [
        "nice", "-n", "10", "cargo", "clippy", "--workspace", "--lib", "--bins", "--locked",
        "--features", "koidrive-cli/test-install-inventory,koidrive-cli/test-setup",
        "--message-format=json", "--",
    ]
    for lint in FORCED_LINTS:
        command.extend(["--force-warn", lint])
    process = subprocess.run(command, cwd=root, env=env, stdout=subprocess.PIPE, text=True, timeout=1800)
    finished = False
    messages, errors = [], []
    for line in process.stdout.splitlines():
        message = json.loads(line)
        if message.get("reason") == "build-finished":
            finished = message["success"]
        if message.get("reason") != "compiler-message":
            continue
        diagnostic = message["message"]
        if diagnostic["level"] == "error":
            errors.append(diagnostic.get("rendered") or diagnostic["message"])
        if (diagnostic.get("code") or {}).get("code") not in FORCED_LINTS:
            continue
        for chain in span_chains(diagnostic):
            for span in chain:
                span["file_name"] = relative_path(span["file_name"], root)
        message["target"]["src_path"] = relative_path(message["target"]["src_path"], root)
        messages.append(message)
    if process.returncode or not finished or errors:
        raise ValueError(f"Clippy failed for the {label} production build.\n" + "\n".join(errors))
    return messages


def check(base, inventory=None):
    base = git("rev-parse", "--verify", f"{base}^{{commit}}").decode().strip()
    paths = git("diff", "--name-only", "--no-renames", "-z", base, "HEAD").decode().split("\0")
    files = {
        path: (source(base, path), source("HEAD", path))
        for path in paths if path.endswith(".rs")
    }
    failures = []
    for path, (before, after) in files.items():
        failures.extend(
            f"{path}:{line}: Release the guard before I/O or use koidrive_state_file; do not suppress the guardrail."
            for line in new_suppressions(before, after)
        )
    env = dict(os.environ, CLIPPY_CONF_DIR=str(CONFIG))
    env["RUSTUP_TOOLCHAIN"] = subprocess.check_output(
        ["rustup", "show", "active-toolchain"], cwd=ROOT, env=env, text=True,
    ).split()[0]
    target = Path(env.get("CARGO_TARGET_DIR", ROOT / "target"))
    if not target.is_absolute():
        target = ROOT / target
    baseline_dir = target / "state-io-baseline" / base
    baseline_root = baseline_dir / "source"
    baseline_root.mkdir(parents=True, exist_ok=True)
    subprocess.run(["tar", "-xm", "-C", str(baseline_root)], input=git("archive", base), check=True)
    baseline_env = dict(env, CARGO_TARGET_DIR=str(baseline_dir / "target"))
    baseline = Counter(
        diagnostic_key(message)
        for message in compile_inventory(baseline_root, baseline_env, "baseline")
    )
    diagnostics = legacy = 0
    recorded = []
    for message in compile_inventory(ROOT, env, "head"):
        diagnostic = message["message"]
        is_new = compiler_introduced(message, files, baseline)
        if (diagnostic.get("code") or {}).get("code") == IO_LINT:
            recorded.append({
                "message": diagnostic["message"],
                "introduced": is_new,
                "spans": [
                    {key: span[key] for key in ("file_name", "line_start", "line_end")}
                    for span in diagnostic["spans"] if span["is_primary"]
                ],
            })
        if is_new:
            diagnostics += 1
            failures.append(diagnostic.get("rendered") or diagnostic["message"])
        elif (diagnostic.get("code") or {}).get("code") == IO_LINT:
            legacy += 1
    for failure in failures:
        print(failure, file=sys.stderr)
    if inventory is not None:
        Path(inventory).write_text(json.dumps({
            "diagnostics": recorded,
            "exceptions": [LIFECYCLE_EXCEPTION, SETUP_LEGACY_EXCEPTION],
        }, indent=2) + "\n")
    print(f"State I/O: {diagnostics} introduced violations; {legacy} existing or helper/build-output diagnostics.")
    return bool(failures)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="Reject new direct state-file I/O and async lock borrows with Clippy.")
    parser.add_argument("base", help="PR base commit (the first parent of a GitHub PR merge commit)")
    parser.add_argument("--inventory", help="Write the complete compiler-resolved direct-I/O inventory as JSON")
    arguments = parser.parse_args()
    try:
        sys.exit(check(arguments.base, arguments.inventory))
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        sys.exit(f"State-I/O guard failed: {error}")
