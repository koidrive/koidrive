#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import argparse
import difflib
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
IMPORT = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:use|extern\s+crate|mod)\b", re.M)
CONDITIONAL = re.compile(r"#\s*!?\s*\[\s*cfg(?:_attr)?\s*\([^]]*\)\s*\]", re.S)
SUPPRESSION = re.compile(r"\b(?:allow|expect)\s*\([^)]*(?:clippy::(?:disallowed_methods|await_holding_lock|await_holding_refcell_ref|all|style|suspicious)|\bwarnings)\b", re.S)
LEXER_SPEC = importlib.util.spec_from_file_location("rust_comments", ROOT / "scripts/check-new-comments.py")
LEXER = importlib.util.module_from_spec(LEXER_SPEC)
LEXER_SPEC.loader.exec_module(LEXER)


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


def new_suppressions(before, after):
    old = [match.group() for match in SUPPRESSION.finditer(before)]
    for match in SUPPRESSION.finditer(after):
        if match.group() in old:
            old.remove(match.group())
        else:
            yield after.count("\n", 0, match.start()) + 1


def introduced(diagnostic, files):
    code = (diagnostic.get("code") or {}).get("code")
    if code in LOCK_LINTS:
        return True
    if code != IO_LINT:
        return False
    spans = [span for span in diagnostic["spans"] if span["is_primary"]]
    if not spans:
        raise ValueError("A state-I/O diagnostic has no source span")
    for span in spans:
        path = span["file_name"]
        if path == HELPER or Path(path).name == "build.rs":
            continue
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
        "--message-format=json", "--", "-W", IO_LINT,
        "-D", "clippy::await_holding_lock", "-D", "clippy::await_holding_refcell_ref",
    ]
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
        is_new = introduced(diagnostic, files)
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
        Path(inventory).write_text(json.dumps(recorded, indent=2) + "\n")
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
