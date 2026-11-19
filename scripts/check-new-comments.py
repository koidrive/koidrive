#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
import argparse
import bisect
import functools
import json
import os
import re
import subprocess
import sys

LANGUAGES = {
    ".rs": "rust",
    ".swift": "swift",
    ".ts": "js",
    ".tsx": "js",
    ".mts": "js",
    ".cts": "js",
    ".js": "js",
    ".mjs": "js",
    ".cjs": "js",
}
PRAGMA = re.compile(r"^[/*!\s]*(SPDX-License-Identifier|SAFETY:|@generated|eslint-|prettier-ignore|swiftlint:|swift-tools-version:)")
LINK = re.compile(r"https?://")
CLAP_DERIVE = re.compile(r"derive\([^)]*\b(Parser|Subcommand|Args|ValueEnum)\b")
MARKER = re.compile(r"\b(TODO|FIXME|XXX|HACK)\b|\bfor now\b", re.IGNORECASE)
INTERNAL = re.compile(r"^(server|apps/macos/Koidrive|cli/tests)/|(^|/)(tests?|Tests|examples|benches)/|(^|/)src/bin/|(^|/)(tests|build)\.rs$|\.test\.[cm]?[jt]sx?$")
HUNK = re.compile(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@")
WORD = re.compile(r"\w+")
IDENT = re.compile(r"[A-Za-z0-9_$]+")
RUST_RAW = re.compile(r'[bc]?r(#*)"')
RUST_CHAR = re.compile(r"'(\\.[^'\n]*|[^\\'\n])'")
SWIFT_RAW = re.compile(r'(#+)("""|")')
FN_DOCS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "fn-docs", "Cargo.toml")
REGEX_BEFORE = set("(,=:[!&|?{};+-*%<>~^") | {"", "return", "typeof", "case", "in", "of", "delete", "void", "throw", "yield", "await"}

MESSAGES = {
    "marker": "TODO, FIXME, XXX, HACK and 'for now' belong in an issue, not in the code.",
    "doc": "New doc comment on internal code (tests, server/, the macOS app, cli/tests). Doc comments go only on API that other packages call, so let the name or the type say it.",
    "private-doc": "A doc comment now sits on a private fn. Doc comments go only on public API. If a helper went in between a pub fn's doc comment and that fn, move the helper below the documented fn.",
    "lost-doc": "This pub fn lost its doc comment. If a helper went in between the doc comment and this fn, move the helper below this fn.",
    "comment": "New comment. Say it in code: a name, a type, a test or an assertion message. A comment may stay only as a license header, a SAFETY: note, a tool pragma, or a line that links the source of an external constraint.",
}


def comment_spans(src, lang, tokens=None):
    found = []
    n = len(src)
    tokens_end = [None]

    def add_token(start, end):
        text = src[start:end]
        joint = bool(tokens and tokens_end[0] == start)
        tokens.append((text, lang == "js" and tokens_end[0] is not None and "\n" in src[tokens_end[0]:start], joint))
        tokens_end[0] = end

    def block(i):
        depth, j = 1, i + 2
        while j < n and depth:
            if lang != "js" and src.startswith("/*", j):
                depth, j = depth + 1, j + 2
            elif src.startswith("*/", j):
                depth, j = depth - 1, j + 2
            else:
                j += 1
        found.append((i, j))
        return j

    def quoted(i, quote, multiline, interpolation):
        j = i + len(quote)
        segment, interpolated = i, False
        while j < n and not src.startswith(quote, j):
            if src[j] == "\n" and not multiline:
                return j
            if interpolation and src.startswith(interpolation, j):
                if tokens is not None:
                    add_token(segment, j + len(interpolation))
                j = code(j + len(interpolation), interpolation[-1].translate(str.maketrans("({", ")}")))
                if tokens is not None:
                    add_token(j - 1, j)
                segment, interpolated = j, True
            elif src[j] == "\\":
                j += 2
            else:
                j += 1
        if tokens is not None and interpolated:
            add_token(segment, j + len(quote))
        return j + len(quote)

    def regex(i):
        j, in_class = i + 1, False
        while j < n and src[j] != "\n":
            if src[j] == "\\":
                j += 2
                continue
            if src[j] == "[":
                in_class = True
            elif src[j] == "]":
                in_class = False
            elif src[j] == "/" and not in_class:
                return j + 1
            j += 1
        return i + 1

    def token(i, prev):
        c = src[i]
        if lang == "rust":
            m = RUST_RAW.match(src, i)
            if m and (i == 0 or not (src[i - 1].isalnum() or src[i - 1] == "_")):
                end = src.find('"' + m.group(1), m.end())
                return n if end < 0 else end + 1 + len(m.group(1))
            if c in "bc" and src.startswith('"', i + 1):
                return quoted(i + 1, '"', True, None)
            if c == '"':
                return quoted(i, '"', True, None)
            if c == "'":
                m = RUST_CHAR.match(src, i)
                return m.end() if m else i + 1
        if lang == "swift":
            m = SWIFT_RAW.match(src, i)
            if m:
                end = src.find(m.group(2) + m.group(1), m.end())
                return n if end < 0 else end + len(m.group(2)) + len(m.group(1))
            if src.startswith('"""', i):
                return quoted(i, '"""', True, "\\(")
            if c == '"':
                return quoted(i, '"', False, "\\(")
        if lang == "js":
            if c in "\"'":
                return quoted(i, c, False, None)
            if c == "\x60":
                return quoted(i, "\x60", True, "${")
            if c == "/" and prev in REGEX_BEFORE:
                return regex(i)
        m = IDENT.match(src, i)
        return m.end() if m else i + 1

    def code(i, closer):
        depth, prev = 0, ""
        while i < n:
            c = src[i]
            if c.isspace():
                i += 1
            elif src.startswith("//", i):
                j = src.find("\n", i)
                j = n if j < 0 else j
                found.append((i, j))
                i = j
            elif src.startswith("/*", i):
                i = block(i)
            elif closer and c == closer and depth == 0:
                return i + 1
            else:
                if c in "([{":
                    depth += 1
                elif c in ")]}":
                    depth -= 1
                start = i
                before = len(tokens) if tokens is not None else 0
                i = token(i, prev)
                prev = src[start:i]
                if tokens is not None and len(tokens) == before:
                    add_token(start, i)
        return i

    code(src.find("\n") if src.startswith("#!") and not src.startswith("#![") else 0, None)
    return found


def is_doc(text, lang):
    if lang == "rust":
        return (text.startswith("///") and not text.startswith("////")) or text.startswith(("//!", "/*!")) or (
            text.startswith("/**") and not text.startswith(("/**/", "/***"))
        )
    if lang == "swift":
        return text.startswith(("///", "/**"))
    return text.startswith("/**") and not text.startswith("/**/")


def comment_lines(src, lang):
    newlines = [k for k, ch in enumerate(src) if ch == "\n"]
    lines = {}
    for start, end in comment_spans(src, lang):
        text = src[start:end]
        doc = is_doc(text, lang)
        first = bisect.bisect_left(newlines, start) + 1
        for offset, piece in enumerate(text.split("\n")):
            lines.setdefault(first + offset, []).append((doc, piece))
    return lines


@functools.cache
def fn_docs():
    if os.environ.get("FN_DOCS"):
        return os.environ["FN_DOCS"]
    build = subprocess.run(
        ["cargo", "build", "--quiet", "--release", "--message-format=json", "--manifest-path", FN_DOCS],
        cwd=os.path.dirname(FN_DOCS),
        check=True,
        capture_output=True,
        text=True,
    )
    return next(
        message["executable"]
        for message in map(json.loads, build.stdout.splitlines())
        if message.get("reason") == "compiler-artifact" and message.get("executable")
    )


def rust_fns(src):
    run = subprocess.run([fn_docs()], input=src.encode(), capture_output=True)
    if run.returncode:
        return None
    lines = src.split("\n")
    fns = {}
    for record in run.stdout.decode().splitlines():
        scope, name, public, number, top, docs = record.split("\t")
        fns.setdefault((scope, name, public == "true"), []).append((docs, int(number), lines[int(number) - 1], int(top)))
    return fns


def changed_docs(old, new):
    added, removed = list(new), list(old)
    for same in (str.__eq__, lambda after, before: bool(before) and after.startswith(before)):
        for docs in sorted(removed, key=len, reverse=True):
            match = next((fn for fn in added if same(fn[0], docs)), None)
            if match:
                added.remove(match)
                removed.remove(docs)
    return removed, added


def moved_docs(path, old_src, new_src):
    old = rust_fns(old_src) if old_src is not None else {}
    new = rust_fns(new_src)
    if old is None or new is None:
        return []
    changes = {key: changed_docs([fn[0] for fn in old.get(key, [])], new.get(key, [])) for key in old.keys() | new.keys()}
    lost = {key: {docs for docs in removed if docs} for key, (removed, _) in changes.items() if key[2]}
    gained = [fn for _, added in changes.values() for fn in added]
    moved = {docs for docs in set().union(*lost.values()) if any(fn[0].startswith(docs) for fn in gained)}
    problems = []
    for key, (_, added) in changes.items():
        for docs, number, text, top in added:
            if not key[2] and any(docs.startswith(m) for m in moved):
                problems.append((path, number, "private-doc", text, top))
            if key[2] and any(not docs.startswith(m) for m in lost[key] & moved):
                problems.append((path, number, "lost-doc", text, number))
    return problems


def git(*args):
    return subprocess.run(["git", *args], check=True, capture_output=True).stdout.decode("utf-8", "replace")


def is_subsequence(words, run):
    it = iter(run)
    return all(w in it for w in words)


def clap_items(src, comments):
    starts = dict(comments)
    code, i, n = [], 0, len(src)
    while i < n:
        if i in starts:
            i = starts[i]
            continue
        m = RUST_RAW.match(src, i)
        if m and (i == 0 or not (src[i - 1].isalnum() or src[i - 1] == "_")):
            end = src.find('"' + m.group(1), m.end())
            i = n if end < 0 else end + 1 + len(m.group(1))
            continue
        if src[i] == '"':
            j = i + 1
            while j < n and src[j] != '"':
                j += 2 if src[j] == "\\" else 1
            i = j + 1
            continue
        if src[i] == "'" and (m := RUST_CHAR.match(src, i)):
            i = m.end()
            continue
        code.append(i)
        i += 1

    def close(k):
        depth = 0
        while k < len(code):
            c = src[code[k]]
            depth += c in "([{"
            depth -= c in ")]}"
            if depth == 0:
                return k
            k += 1
        return len(code) - 1

    attrs, k = {}, 0
    while k < len(code):
        if src[code[k]] == "#" and k + 1 < len(code) and src[code[k + 1]] in "[!":
            b = k + 1 if src[code[k + 1]] == "[" else k + 2
            e = close(b)
            attrs[code[e] + 1] = (code[k], k, e)
            k = e
        k += 1
    ends = {e: s for s, e in comments if is_doc(src[s:e], "rust")}
    items = []
    for end, (start, _, last) in attrs.items():
        if not CLAP_DERIVE.search(src[start:end]):
            continue
        while True:
            j = start
            while j > 0 and src[j - 1].isspace():
                j -= 1
            if j in ends:
                start = ends[j]
            elif j in attrs:
                start = attrs[j][0]
            else:
                break
        k = last + 1
        while k < len(code) and not (src.startswith(("struct", "enum"), code[k]) and IDENT.match(src, code[k]).group() in ("struct", "enum") and not IDENT.match(src[code[k] - 1])):
            k += 1
        while k < len(code) and src[code[k]] not in "{(;":
            k += 1
        if k < len(code):
            items.append((start, code[k] + 1 if src[code[k]] == ";" else code[close(k)] + 1))
    return items


def macro_lines(src, lang):
    if lang != "rust" or not CLAP_DERIVE.search(src):
        return set()
    newlines = [k for k, ch in enumerate(src) if ch == "\n"]
    lines = [(bisect.bisect_left(newlines, s) + 1, bisect.bisect_left(newlines, e - 1) + 1) for s, e in clap_items(src, comment_spans(src, lang))]
    return {n for first, last in lines for n in range(first, last + 1)}


def code_tokens(src, lang):
    tokens = []
    spans = comment_spans(src, lang, tokens)
    if lang != "rust":
        return tokens, []
    items = clap_items(src, spans) if CLAP_DERIVE.search(src) else []
    docs, end = [], None
    for s, e in spans:
        text = src[s:e]
        if not is_doc(text, lang):
            end = None
            continue
        if end is not None and re.fullmatch(r"\n[ \t]*", src[end:s]) and text[:3] == docs[-1][1][:3] and text.startswith(("///", "//!")):
            docs[-1][1] += "\n" + text
        else:
            docs.append([s, text])
        end = e
    return tokens, [d for s, d in docs if any(a <= s < b for a, b in items) or "```" in d]


def differences_beyond_comments(base, head):
    changed = git("-c", "core.quotePath=false", "diff", "--name-status", "--no-renames", base, head).splitlines()
    problems = []
    for entry in changed:
        status, path = entry.split("	", 1)
        lang = LANGUAGES.get("." + path.rsplit(".", 1)[-1])
        if status != "M" or not lang:
            problems.append(f"{path}: {status} on a file that is not Rust, Swift, TypeScript or JavaScript" if status == "M" else f"{path}: status {status}")
        else:
            (old_code, old_docs), (new_code, new_docs) = (code_tokens(git("show", f"{rev}:{path}"), lang) for rev in (base, head))
            if old_code != new_code:
                problems.append(f"{path}: code differs once comments are stripped")
            elif old_docs != new_docs:
                problems.append(f"{path}: a doc comment that is code changed (clap --help text in a file with clap derives, or a doctest)")
    return len(changed), problems


def comment_run(lines, number):
    first, last = number, number
    while first - 1 in lines:
        first -= 1
    while last + 1 in lines:
        last += 1
    return [piece for n in range(first, last + 1) for _, piece in lines[n]]


def check(base, head):
    diff = git("-c", "core.quotePath=false", "diff", "-U0", "-M", "--no-color", "--no-ext-diff", base, head, "--", *("*" + s for s in LANGUAGES))
    files, current = [], None
    for line in diff.split("\n"):
        if line.startswith("--- "):
            current = {"old": None if line == "--- /dev/null" else line[6:], "removed": [], "added": []}
            files.append(current)
        elif line.startswith("+++ "):
            current["new"] = None if line == "+++ /dev/null" else line[6:]
        elif current and (m := HUNK.match(line)):
            old, old_count, new, new_count = (int(g) if g is not None else 1 for g in m.groups())
            current["removed"].extend(range(old, old + old_count))
            current["added"].extend(range(new, new + new_count))

    runs = []
    for f in files:
        if not f["old"] or not f["removed"]:
            continue
        lang = LANGUAGES["." + f["old"].rsplit(".", 1)[-1]]
        old_lines = comment_lines(git("show", f"{base}:{f['old']}"), lang)
        previous = None
        for number in f["removed"]:
            for _, piece in old_lines.get(number, []):
                if previous is None or number > previous + 1:
                    runs.append([])
                runs[-1].extend(w.lower() for w in WORD.findall(piece))
                previous = number

    problems, moved = [], []
    for f in files:
        path = f.get("new")
        if not path or "/gen/" in path:
            continue
        lang = LANGUAGES["." + path.rsplit(".", 1)[-1]]
        new_src = git("show", f"{head}:{path}")
        if lang == "rust":
            moved.extend(moved_docs(path, git("show", f"{base}:{f['old']}") if f["old"] else None, new_src))
        if not f["added"]:
            continue
        new_lines = comment_lines(new_src, lang)
        help_lines = macro_lines(new_src, lang)
        for number in f["added"]:
            for doc, piece in new_lines.get(number, []):
                words = [w.lower() for w in WORD.findall(piece)]
                if not words:
                    continue
                if MARKER.search(piece):
                    problems.append((path, number, "marker", piece))
                elif doc and number in help_lines:
                    continue
                elif any(is_subsequence(words, run) for run in runs):
                    continue
                elif PRAGMA.search(piece) or any(LINK.search(p) for p in comment_run(new_lines, number)):
                    continue
                elif not doc:
                    problems.append((path, number, "comment", piece))
                elif INTERNAL.search(path):
                    problems.append((path, number, "doc", piece))
    flagged = {(path, number) for path, number, _, _ in problems}
    for path, number, kind, text, top in sorted(moved):
        if not any((path, line) in flagged for line in range(top, number)):
            problems.append((path, number, kind, text))
    return problems


def main():
    parser = argparse.ArgumentParser(
        description="Fail on comment lines that the diff from BASE to HEAD adds to Rust, Swift, TypeScript and JavaScript files. A comment line that keeps only words of a comment the same diff removes counts as a trim, not as new."
    )
    parser.add_argument("base", metavar="BASE", help="commit or tree to compare against, such as the merge base with main")
    parser.add_argument("head", metavar="HEAD", nargs="?", default="HEAD", help="commit or tree to check (default: HEAD)")
    parser.add_argument("--comments-only", action="store_true", help="instead, fail unless every changed file is a modified source file whose code is identical once comments are stripped")
    args = parser.parse_args()
    if args.comments_only:
        count, problems = differences_beyond_comments(args.base, args.head)
        for problem in problems:
            print(problem)
        if problems:
            print(f"{len(problems)} of {count} changed files change more than comments between {args.base} and {args.head}.")
            return 1
        print(f"All {count} changed files between {args.base} and {args.head} have identical code once comments are stripped.")
        return 0
    problems = check(args.base, args.head)
    for path, number, kind, piece in problems:
        print(f"::error file={path},line={number}::{MESSAGES[kind]}")
        print(f"    {piece.strip()}")
    if problems:
        print(f"{len(problems)} comment problems between {args.base} and {args.head}. Remove the comments, or reshape the code so it reads without them.")
        return 1
    print(f"No new comments between {args.base} and {args.head}.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
