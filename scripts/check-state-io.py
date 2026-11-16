#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import argparse
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
IMPORT = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:use|extern\s+crate|mod)\b", re.M)
CONDITIONAL = re.compile(r"#\s*!?\s*\[\s*cfg(?:_attr)?\s*\([^]]*\)\s*\]", re.S)
LEXER_SPEC = importlib.util.spec_from_file_location("rust_comments", ROOT / "scripts/check-new-comments.py")
LEXER = importlib.util.module_from_spec(LEXER_SPEC)
LEXER_SPEC.loader.exec_module(LEXER)

# Lifecycle and historical setup cleanup follow-ups: GUN-928 and GUN-930.
LIFECYCLE_EXCEPTION = {
    "name": "lifecycle checkpoint publication",
    "file": "core/driver/src/lifecycle.rs",
    "line": 183,
    "function_start": 172,
    "function_end": 190,
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
    call, depth = [], 0
    for token, _, _ in tokens:
        if depth == 0 and token in (";", ",", "=", "}"):
            return None
        call.append(token)
        if token == "(":
            depth += 1
        elif token == ")":
            depth -= 1
            if depth == 0:
                return call
    return None


def imports(text):
    return [text[match.start():text.find(";", match.start()) + 1] for match in IMPORT.finditer(text)]


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


def introduced(diagnostic, files, build_script=False):
    code = (diagnostic.get("code") or {}).get("code")
    if code in LOCK_LINTS:
        return True
    if code != IO_LINT:
        return False
    spans = []
    for span in diagnostic["spans"]:
        if not span["is_primary"]:
            continue
        while span is not None:
            spans.append(span)
            span = (span.get("expansion") or {}).get("span")
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
        if imports(before) != imports(after) or CONDITIONAL.findall(before) != CONDITIONAL.findall(after):
            return True
        unchanged = unchanged_lines(before, after)
        if any(line not in unchanged for line in range(span["line_start"], span["line_end"] + 1)):
            return True
        column = span.get("column_start", 1)
        old_call = call_tokens(before, unchanged[span["line_start"]], column)
        new_call = call_tokens(after, span["line_start"], column)
        if old_call is None or new_call is None or old_call != new_call:
            return True
    return False


def check(base, inventory=None):
    git("rev-parse", "--verify", f"{base}^{{commit}}")
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
    command = [
        "nice", "-n", "10", "cargo", "clippy", "--workspace", "--lib", "--bins", "--locked",
        "--features", "koidrive-cli/test-install-inventory,koidrive-cli/test-setup",
        "--message-format=json", "--",
    ]
    for lint in FORCED_LINTS:
        command.extend(["--force-warn", lint])
    process = subprocess.run(command, cwd=ROOT, env=env, stdout=subprocess.PIPE, text=True, timeout=1800)
    diagnostics = legacy = 0
    finished = False
    recorded = []
    for line in process.stdout.splitlines():
        message = json.loads(line)
        if message.get("reason") == "build-finished":
            finished = message["success"]
        if message.get("reason") != "compiler-message":
            continue
        diagnostic = message["message"]
        is_new = introduced(diagnostic, files, "custom-build" in message["target"]["kind"])
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
        elif diagnostic["level"] == "error":
            failures.append(diagnostic.get("rendered") or diagnostic["message"])
    if process.returncode or not finished:
        failures.append("Clippy failed. Release MutexGuard and RefCell borrows before awaiting network I/O, then rerun this command.")
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
