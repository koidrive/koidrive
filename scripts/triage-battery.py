#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path

SECTION = re.compile(r"^---- (\S+) stdout ----$", re.M)
FAILURES = re.compile(r"^failures:$", re.M)
SAVED = re.compile(r"^proptest regression saved in (\S+): cc ([0-9a-f]+)$", re.M)
CASE = re.compile(r"^cc-([0-9a-f]+)\.txt$")
LOCATION = re.compile(r"\.rs:\d+(?::\d+)?")
CHECKED_PROFILE = re.compile(r"(?ms)^\[profile\.checked\]\n.*?(?=^\[|\Z)")
RUNTIME_CHECKS = Path(__file__).resolve().parent / "require-runtime-checks.sh"
PROBE_SOURCE = Path(__file__).resolve().parent.parent / "core/test-support/src/lib.rs"
KNOWN_FAILURES = Path(__file__).resolve().parent.parent / ".github/battery-known-failures.toml"
ID = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
POSITION = re.compile(r"\.rs:\d+(?::\d+)?")
NUMBER = re.compile(r"\d+")
UNCLASSIFIED = "unclassified"
FIELDS = {"class", "issue", "tests", "message", "examples"}
CASE_PREFIX = re.compile(r"[0-9a-f]{12,64}")
PROBE_TEST = "the_build_keeps_debug_assertions_and_overflow_checks"
TRIAGE_PROBE = f"triage_runtime_probe::{PROBE_TEST}"
DISPLAY_LIMIT = 300
REPLAYS = 2
SAME = "fails on base with the same message"
MOVED = "same message, different location (unconfirmed)"
SILENT = "a side crashed without a message (unconfirmed)"
VERDICTS = ("PR-caused", "unstable", "unknown", SILENT, MOVED, "flaky or fixed", SAME)
HARNESS_OUTPUT = re.compile(r"^(running \d+ tests?$|test \S+ \.\.\.)")
SILENT_CRASH = re.compile(r"^killed by signal \d+$")


def sections(log):
    headers = list(SECTION.finditer(log))
    for index, header in enumerate(headers):
        end = headers[index + 1].start() if index + 1 < len(headers) else len(log)
        closing = FAILURES.search(log, header.end(), end)
        yield header[1], log[header.end():closing.start() if closing else end]


def failure_message(output):
    lines = output.splitlines()
    start = next((index for index, line in enumerate(lines) if line.startswith("Test failed: ")), None)
    if start is None:
        start = next((index + 1 for index, line in enumerate(lines[:-1]) if " panicked at " in line), None)
    if start is None:
        return None
    message = [lines[start].removeprefix("Test failed: ")]
    for line in lines[start + 1:]:
        if not line.strip() or line.startswith(("minimal failing input:", "note: ", "thread '", "---- ")):
            break
        message.append(line)
    return "\n".join(message)


def comparable(message):
    return LOCATION.sub(".rs:N", message or "")


def normalized(message):
    message = POSITION.sub(".rs:<line>", ID.sub("<id>", message))
    return " ".join(NUMBER.sub("<n>", message).split())


def known_failures(text):
    entries = tomllib.loads(text).get("failure")
    if not isinstance(entries, list) or not entries:
        raise ValueError("it has no [[failure]] entries")
    for number, entry in enumerate(entries, 1):
        if not isinstance(entry, dict):
            raise ValueError(f"[[failure]] {number} is not a table")
        if not FIELDS <= set(entry) <= FIELDS | {"cases"}:
            raise ValueError(f"[[failure]] {number} has the fields {sorted(entry)}, not {sorted(FIELDS)} and optionally cases")
        for field in ("class", "issue", "message"):
            if not isinstance(entry[field], str) or not entry[field]:
                raise ValueError(f"[[failure]] {number} needs {field} as a non-empty string")
        for field in ("tests", "examples"):
            values = entry[field]
            if not isinstance(values, list) or not values or not all(isinstance(value, str) for value in values):
                raise ValueError(f"[[failure]] {number} needs {field} as a non-empty list of strings")
        cases = entry.get("cases")
        if "cases" in entry and not (isinstance(cases, list) and cases and all(
                isinstance(case, str) and CASE_PREFIX.fullmatch(case) for case in cases)):
            raise ValueError(f"[[failure]] {number} needs cases as a non-empty list of 12 to 64 lowercase hex digits")
        try:
            entry["pattern"] = re.compile(entry["message"])
        except re.error as error:
            raise ValueError(f"[[failure]] {number} has a message regex that doesn't compile: {error}") from error
    return entries


def matches(entries, test, message, case=""):
    text = normalized(message)
    found = [entry for entry in entries if test in entry["tests"] and entry["pattern"].match(text)
             and case.startswith(tuple(entry.get("cases", [""])))]
    return [entry for entry in found if "cases" in entry] or found


def classify(rows, entries):
    ambiguous = []
    for row in rows:
        row["class"] = row["issue"] = None
        if entries is None or row["verdict"] != SAME:
            continue
        found = matches(entries, row["test"], settle(row["head"])["message"], row["case"])
        if len(found) > 1:
            ambiguous.append(row)
        row["class"] = ", ".join(entry["class"] for entry in found) or UNCLASSIFIED
        row["issue"] = ", ".join(entry["issue"] for entry in found) or None
    return ambiguous


def shard_number(directory):
    return int(directory.name.rsplit("-", 1)[1])


def collect(failures):
    cases = {}
    others = {}

    def add(path, test, binary, shard, message, shared):
        case = cases.setdefault((path, test), {
            "case": CASE.match(Path(path).name)[1],
            "path": path,
            "binary": binary,
            "test": test,
            "text": files[path].read_text(),
            "battery_message": message,
            "shared": shared,
            "shards": [],
        })
        case["shared"] = case["shared"] and shared
        if shard not in case["shards"]:
            case["shards"].append(shard)

    for shard_dir in sorted((path for path in failures.glob("failure-shard-*") if path.is_dir()), key=shard_number):
        shard = shard_number(shard_dir)
        regressions = shard_dir / "regressions"
        files = {path.relative_to(regressions).as_posix(): path
                 for path in regressions.rglob("cc-*.txt") if path.parent.suffix == ".cases"}
        mapped = set()
        for log in sorted((shard_dir / "logs").glob("*.log")):
            binary = log.stem.rsplit("-", 1)[0]
            saved_here = []
            unsaved = []
            for test, body in sections(log.read_text(errors="replace")):
                saved = [(match, path) for match in SAVED.finditer(body)
                         for path in files if match[1].endswith("/" + path)]
                if not saved:
                    unsaved.append((test, failure_message(body)))
                for match, path in saved:
                    saved_here.append(path)
                    add(path, test, binary, shard, failure_message(body[match.end():]), False)
            mapped.update(saved_here)
            for test, message in unsaved:
                other = others.setdefault((binary, test), {"shards": set(), "cases": set()})
                other["shards"].add(shard)
                other["cases"].update(saved_here)
                for path in saved_here:
                    add(path, test, binary, shard, message, True)
        for path in sorted(set(files) - mapped):
            add(path, None, None, shard, None, False)
    return (sorted(cases.values(), key=lambda case: (case["path"], case["test"] or "")),
            [{"binary": binary, "test": test, "shards": sorted(other["shards"]), "cases": sorted(other["cases"])}
             for (binary, test), other in sorted(others.items())])


def checked_profile(manifest):
    section = CHECKED_PROFILE.search(manifest)
    return section[0].rstrip("\n") + "\n" if section else None


def is_regression_file(path):
    try:
        lines = path.read_text().splitlines()
    except UnicodeDecodeError:
        return False
    return all(not line.strip() or line.startswith("#") or line.startswith("cc ") for line in lines)


def isolate(checkout, case):
    directory = (checkout / case["path"]).parent
    for path in directory.glob("*.txt"):
        path.unlink()
    for path in directory.parent.glob(directory.stem + ".*"):
        if path.is_file() and is_regression_file(path):
            path.unlink()
    directory.mkdir(parents=True, exist_ok=True)
    (directory / Path(case["path"]).name).write_text(case["text"])


def add_probes(checkout, source):
    target = checkout / "core/test-support/src/lib.rs"
    text = target.read_text()
    probe = re.search(rf"(?ms)^    #\[test\]\n    fn {PROBE_TEST}\(\) \{{\n.*?^    \}}\n", source)[0]
    missing = f"fn {PROBE_TEST}(" not in text
    if missing:
        tests = re.search(r"(?m)^mod tests \{\n", text)
        if tests:
            text = text[:tests.end()] + probe + "\n" + text[tests.end():]
        else:
            text = text.rstrip("\n") + "\n\n#[cfg(test)]\nmod tests {\n" + probe + "}\n"
    target.write_text(text.rstrip("\n") + f"\n\n#[cfg(test)]\nmod {TRIAGE_PROBE.split('::')[0]} {{\n" + probe + "}\n")
    return missing


def resolve(metadata, checkout, case):
    packages = sorted(metadata["packages"], key=lambda package: -len(package["manifest_path"]))
    for package in packages:
        directory = Path(package["manifest_path"]).parent.relative_to(checkout.resolve()).as_posix()
        if not case["path"].startswith(directory + "/"):
            continue
        for target in package["targets"]:
            if target["name"] != case["binary"]:
                continue
            if "test" in target["kind"]:
                return package["name"], ["--test", target["name"]], directory
            if "bin" in target["kind"]:
                return package["name"], ["--bin", target["name"]], directory
            if "lib" in target["kind"]:
                return package["name"], ["--lib"], directory
        return None
    return None


def replay_environment(package_dir):
    environment = {name: value for name, value in os.environ.items()
                   if not name.startswith("PROPTEST_")
                   and name not in ("KOIDRIVE_BATTERY_CASES", "KOIDRIVE_PROPTEST_DETERMINISTIC")}
    environment.update(PROPTEST_CASES="0", CARGO_MANIFEST_DIR=str(package_dir))
    return environment


def outcome(returncode, output):
    if returncode == 0 and re.search(r"^test result: ok\. 1 passed;", output, re.M):
        return {"result": "pass", "message": None}
    if returncode == 0 and re.search(r"^test result: ok\. 0 passed; 0 failed; 1 ignored;", output, re.M):
        return {"result": "ignored", "message": None}
    if returncode < 0:
        last = next(filter(None, (HARNESS_OUTPUT.sub("", line.strip()).strip()
                                  for line in reversed(output.splitlines()))), None)
        return {"result": "fail", "message": failure_message(output)
                or f"killed by signal {-returncode}" + (f" after: {last}" if last else "")}
    if returncode != 0 and re.search(r"^test result: FAILED\.", output, re.M):
        return {"result": "fail", "message": failure_message(output) or f"exit code {returncode}"}
    return {"result": "error", "message": f"exit code {returncode} without a test result"}


def replay_one(executable, package_dir, test, timeout):
    environment = replay_environment(package_dir)
    try:
        listing = subprocess.run([executable, "--exact", test, "--list"], cwd=package_dir, env=environment,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, errors="replace",
                                 timeout=timeout)
    except subprocess.TimeoutExpired:
        return {"result": "error", "message": f"--list gave no answer within {timeout} s"}
    if listing.returncode != 0:
        return {"result": "error", "message": f"--list exited with code {listing.returncode}"}
    if not re.search(rf"^{re.escape(test)}: test$", listing.stdout, re.M):
        return {"result": "missing", "message": None}
    try:
        run = subprocess.run([executable, "--exact", test], cwd=package_dir, env=environment,
                             stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, errors="replace",
                             timeout=timeout)
    except subprocess.TimeoutExpired:
        return {"result": "timeout", "message": f"no result within {timeout} s"}
    sys.stdout.write(run.stdout)
    return outcome(run.returncode, run.stdout)


def cargo_no_run(checkout, package, selector):
    command = ["cargo", "test", "--locked", "--profile", "checked", "--no-run",
               "--message-format=json-render-diagnostics", "-p", package, *selector]
    print("::group::" + " ".join(command), flush=True)
    run = subprocess.run(command, cwd=checkout, stdout=subprocess.PIPE, text=True)
    print("::endgroup::", flush=True)
    return run.stdout if run.returncode == 0 else None


def build(checkout, package, selector):
    target = " ".join([package, *selector])
    output = cargo_no_run(checkout, package, selector)
    if output is None:
        return None, f"cargo test --no-run for {target} failed"
    probe = cargo_no_run(checkout, "koidrive-test-support", ["--lib"])
    if probe is None:
        return None, "cargo test --no-run for koidrive-test-support --lib, the runtime-check probe, failed"
    with tempfile.TemporaryDirectory() as directory:
        files = [Path(directory) / "target.json", Path(directory) / "probe.json"]
        for path, text in zip(files, (output, probe)):
            path.write_text(text)
        checks = subprocess.run(["bash", str(RUNTIME_CHECKS), *map(str, files)],
                                stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    print(checks.stdout, end="", flush=True)
    if checks.returncode != 0:
        return None, f"the build of {target} fails require-runtime-checks.sh: " + " ".join(
            line.removeprefix("::error::") for line in checks.stdout.splitlines())
    probe_executable = next((message["executable"] for message in map(json.loads, probe.splitlines())
                             if message.get("reason") == "compiler-artifact" and message.get("executable")
                             and message["target"]["name"] == "koidrive_test_support"), None)
    triage_probe = subprocess.run([probe_executable, "--exact", TRIAGE_PROBE], stdout=subprocess.PIPE,
                                  stderr=subprocess.STDOUT, text=True) if probe_executable else None
    if triage_probe is None or triage_probe.returncode != 0 or not re.search(
            r"^test result: ok\. 1 passed;", triage_probe.stdout, re.M):
        return None, f"{TRIAGE_PROBE}, the runtime probe the triage added, did not pass in the build of {target}"
    executable = next((message["executable"] for message in map(json.loads, output.splitlines())
                       if message.get("reason") == "compiler-artifact" and message.get("executable")), None)
    return (executable, None) if executable else (None, f"cargo test --no-run for {target} built no test binary")


def replay(spec, side, checkout, timeout):
    manifest = checkout / "Cargo.toml"
    text = manifest.read_text()
    if checked_profile(text) is None:
        manifest.write_text(text.rstrip("\n") + "\n\n" + spec["checked_profile"])
        print(f"{side}: Cargo.toml has no [profile.checked], so the triage added the one from cases.json.", flush=True)
    if add_probes(checkout, PROBE_SOURCE.read_text()):
        print(f"{side}: koidrive-test-support has no tests::{PROBE_TEST}, so the triage added this checkout's.", flush=True)
    print(f"{side}: the triage added {TRIAGE_PROBE} from this checkout and runs it after every build.", flush=True)
    metadata = json.loads(subprocess.run(["cargo", "metadata", "--locked", "--format-version", "1", "--no-deps"],
                                         cwd=checkout, check=True, stdout=subprocess.PIPE, text=True).stdout)
    executables = {}
    results = []
    for case in spec["cases"]:
        entry = {"path": case["path"], "test": case["test"], "target": None}
        results.append(entry)
        if case["test"] is None:
            entry["runs"] = [{"result": "unmapped", "message": "no battery log names the test that saved this case"}]
            continue
        target = resolve(metadata, checkout, case)
        if target is None:
            entry["runs"] = [{"result": "missing", "message": None}]
            continue
        package, selector, directory = target
        entry["target"] = " ".join([package, *selector])
        if entry["target"] not in executables:
            executables[entry["target"]] = build(checkout, package, selector)
        executable, problem = executables[entry["target"]]
        if problem:
            entry["runs"] = [{"result": "error", "message": problem}]
            continue
        entry["runs"] = []
        for attempt in range(1, REPLAYS + 1):
            isolate(checkout, case)
            print(f"::group::{side} {case['case'][:12]} {entry['target']} {case['test']} run {attempt}", flush=True)
            entry["runs"].append(replay_one(executable, checkout / directory, case["test"], timeout))
            print("::endgroup::", flush=True)
        print(f"{side} {case['case'][:12]} {case['test']}: {', '.join(run['result'] for run in entry['runs'])}",
              flush=True)
    return {"side": side, "sha": spec[side], "results": results}


def settle(runs):
    first = runs[0]
    if all((run["result"], run["message"]) == (first["result"], first["message"]) for run in runs):
        return first
    return {"result": "unstable", "message": None}


def verdict(head_runs, base_runs):
    head, base = settle(head_runs), settle(base_runs)
    if "unstable" in (head["result"], base["result"]):
        return "unstable"
    if head["result"] == "pass":
        return "flaky or fixed"
    if head["result"] != "fail":
        return "unknown"
    if base["result"] in ("pass", "missing", "ignored"):
        return "PR-caused"
    if base["result"] != "fail":
        return "unknown"
    if any(SILENT_CRASH.match(side["message"] or "") for side in (head, base)):
        return SILENT
    if head["message"] == base["message"]:
        return SAME
    if comparable(head["message"]) == comparable(base["message"]):
        return MOVED
    return "PR-caused"


def combine(spec, replays):
    rows = []
    for index, case in enumerate(spec["cases"]):
        sides = {side: replays[side]["results"][index] if side in replays
                 else {"runs": [{"result": "error", "message": f"the {side} replay left no result"}], "target": None}
                 for side in ("head", "base")}
        if case["shared"] and settle(sides["head"]["runs"])["result"] == "pass":
            continue
        rows.append({
            "case": case["case"],
            "path": case["path"],
            "target": sides["head"]["target"] or sides["base"]["target"] or case["binary"],
            "test": case["test"],
            "shared": case["shared"],
            "shards": case["shards"],
            "head": sides["head"]["runs"],
            "base": sides["base"]["runs"],
            "verdict": verdict(sides["head"]["runs"], sides["base"]["runs"]),
        })
    rows.sort(key=lambda row: (VERDICTS.index(row["verdict"]), row["target"] or "", row["test"] or "", row["case"]))
    return rows


def caseless(spec, rows):
    reproduced = {(row["path"], row["test"]) for row in rows}
    return [other for other in spec["others"]
            if not any((path, other["test"]) in reproduced for path in other["cases"])]


def cell(text):
    if text is None:
        return ""
    text = text.replace("\n", " ")
    if len(text) > DISPLAY_LIMIT:
        text = text[:DISPLAY_LIMIT] + "…"
    return text.replace("|", "\\|")


def results_cell(runs):
    results = [run["result"] for run in runs]
    return results[0] if len(set(results)) == 1 else " / ".join(results)


def messages_cell(runs):
    messages = [run["message"] for run in runs]
    if len(set(messages)) == 1:
        return cell(messages[0])
    return " / ".join(f"run {number}: {cell(message) or '(none)'}" for number, message in enumerate(messages, 1))


def markdown(spec, rows, others, run_url, problem=None):
    counts = ", ".join(f"{sum(row['verdict'] == name for row in rows)} {name}" for name in VERDICTS)
    same = [row for row in rows if row["verdict"] == SAME]
    unclassified = sum(row["class"] == UNCLASSIFIED for row in same)
    lines = [
        "## Battery triage",
        "",
        f"[Run {spec['run']}]({run_url}) saved {len({row['path'] for row in rows if not row['shared']})} new cases; "
        f"the table has {len(rows)} rows: {counts}. "
        f"Each case replayed alone {REPLAYS} times on each side, without the committed cases, with "
        f"`PROPTEST_CASES=0` and the checked profile, on the head `{spec['head']}` and on its merge base with main "
        f"`{spec['base']}`.",
        "",
        "PR-caused means the case fails on the head and passes on the base, fails on both with a different message, "
        "or the base has no such test. "
        f"{SAME[0].upper() + SAME[1:]} means both sides fail with exactly the same message. "
        f"{MOVED[0].upper() + MOVED[1:]} means the messages match only once their `file.rs:line:column` positions "
        "are ignored, so they may come from different assertions. "
        f"{SILENT[0].upper() + SILENT[1:]} means both sides fail but one was killed by a signal before it printed \
anything of its own, so the two failures can't be compared. "
        "Unstable means the runs on one side disagree on the result or the message, and the table shows each run. "
        "Messages are compared as they are, with nothing normalised. "
        "Proptest saves a seed once, so when two tests of one binary fail on the same seed in a shard, only the first "
        "saves it. The other test gets a row, marked as saved by another test, for each case that shard saved in the "
        "same binary, unless that case passes it on every head run. "
        f"This table cuts messages at {DISPLAY_LIMIT} characters; triage.json holds them in full.",
        "",
        f"No row has a class: {problem}" if problem else
        f"Class and issue name the known failure on main, from `.github/battery-known-failures.toml`, \
that a row which {SAME} matches by its test name and its message with ids, numbers and `file.rs:line` positions \
masked, and for an entry that lists cases, only by those cases, which come before the entries that list none. \
{len(same) - unclassified} of the {len(same)} such rows match one; {unclassified} {UNCLASSIFIED} match none.",
        "",
        "| Case | Binary and test | Head | Base | Head message | Base message | Verdict | Class | Issue |",
        "|---|---|---|---|---|---|---|---|---|",
    ]
    lines += [f"| `{row['case'][:12]}`{' (saved by another test)' if row['shared'] else ''} | "
              f"{cell(row['target'])} `{row['test'] or '?'}` | {results_cell(row['head'])} | "
              f"{results_cell(row['base'])} | {messages_cell(row['head'])} | {messages_cell(row['base'])} | "
              f"{row['verdict']} | {row['class'] or ''} | {row['issue'] or ''} |"
              for row in rows]
    if others:
        lines += ["", "These tests failed without saving a new case, and every case their shards saved in the same "
                  "binary passes them on every head run, so they are not in the table:", ""]
        lines += [f"- {other['binary']} `{other['test']}` on shards {', '.join(map(str, other['shards']))}"
                  for other in others]
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description="Replay a red property battery's new cases on its head and base.")
    commands = parser.add_subparsers(dest="command", required=True)
    collect_parser = commands.add_parser("collect", help="map the cases in failure-shard-* artifacts to their tests")
    collect_parser.add_argument("--failures", type=Path, required=True)
    collect_parser.add_argument("--run", required=True)
    collect_parser.add_argument("--head", required=True)
    collect_parser.add_argument("--base", required=True)
    collect_parser.add_argument("--head-manifest", type=Path, required=True)
    collect_parser.add_argument("--fallback-manifest", type=Path, required=True)
    collect_parser.add_argument("--out", type=Path, required=True)
    replay_parser = commands.add_parser("replay", help="replay each case alone in one checkout")
    replay_parser.add_argument("--cases", type=Path, required=True)
    replay_parser.add_argument("--side", choices=("head", "base"), required=True)
    replay_parser.add_argument("--checkout", type=Path, required=True)
    replay_parser.add_argument("--timeout", type=int, default=900)
    replay_parser.add_argument("--out", type=Path, required=True)
    report_parser = commands.add_parser("report", help="compare the replays and write triage.json and triage.md")
    report_parser.add_argument("--cases", type=Path, required=True)
    report_parser.add_argument("--replays", type=Path, nargs="*", default=[])
    report_parser.add_argument("--run-url", required=True)
    report_parser.add_argument("--known-failures", type=Path, default=KNOWN_FAILURES)
    report_parser.add_argument("--out", type=Path, required=True)
    arguments = parser.parse_args()

    if arguments.command == "collect":
        cases, others = collect(arguments.failures)
        profile = (checked_profile(arguments.head_manifest.read_text())
                   or checked_profile(arguments.fallback_manifest.read_text()))
        if profile is None:
            sys.exit("Neither the head's Cargo.toml nor this checkout's has a [profile.checked] section.")
        spec = {"run": arguments.run, "head": arguments.head, "base": arguments.base,
                "checked_profile": profile, "cases": cases, "others": others}
        arguments.out.write_text(json.dumps(spec, indent=2) + "\n")
        print(f"{len(cases)} new cases; {len(others)} failing tests saved no new case.")
    elif arguments.command == "replay":
        spec = json.loads(arguments.cases.read_text())
        arguments.out.write_text(json.dumps(replay(spec, arguments.side, arguments.checkout, arguments.timeout),
                                            indent=2) + "\n")
    else:
        spec = json.loads(arguments.cases.read_text())
        replays = {replayed["side"]: replayed for replayed in (json.loads(path.read_text()) for path in arguments.replays)}
        rows = combine(spec, replays)
        others = caseless(spec, rows)
        try:
            entries, problem = known_failures(arguments.known_failures.read_text()), None
        except (OSError, ValueError) as error:
            entries, problem = None, f"{arguments.known_failures} can't be read: {error}."
        ambiguous = classify(rows, entries)
        arguments.out.mkdir(parents=True, exist_ok=True)
        (arguments.out / "triage.json").write_text(json.dumps(
            {"run": spec["run"], "head": spec["head"], "base": spec["base"], "cases": rows, "others": others},
            indent=2) + "\n")
        (arguments.out / "triage.md").write_text(markdown(spec, rows, others, arguments.run_url, problem))
        if problem:
            sys.exit(f"::error::No row has a class: {problem}")
        if ambiguous:
            sys.exit(f"::error::{arguments.known_failures} is wrong: " + "; ".join(
                f"{row['case'][:12]} {row['test']} matches {row['class']}" for row in ambiguous))


if __name__ == "__main__":
    main()
