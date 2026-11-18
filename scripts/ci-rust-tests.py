#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
import argparse
import hashlib
from concurrent.futures import ThreadPoolExecutor
import json
import math
import os
from pathlib import Path
import subprocess
import sys
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parent.parent
TIMES = ROOT / ".github/rust-test-times.json"
CONFIG = ROOT / ".config/nextest.toml"
FEATURES = "koidrive-cli/test-install-inventory,koidrive-cli/test-setup"
SUITES = {"rust_driver": "koidrive-driver", "rust_engine": "koidrive-engine"}
SLOW = 2.0
UNKNOWN_TEST = 0.5


def archive_args(selected):
    excluded = [package for job, package in SUITES.items() if job not in selected]
    if "rust" in selected:
        args = ["--workspace", "--exclude", "koidrive-sim"]
        for package in excluded:
            args += ["--exclude", package]
        return args + ["--features", FEATURES]
    packages = [package for job, package in SUITES.items() if job in selected]
    if not packages:
        raise ValueError("No Rust test partition is selected")
    args = []
    for package in packages:
        args += ["-p", package]
    return args + ["--features", "koidrive-engine/serde"]


def listed(listing):
    tests = {}
    for binary, suite in listing["rust-suites"].items():
        names = [name for name, case in suite["testcases"].items() if case["filter-match"]["status"] == "matches"]
        if names:
            tests[binary] = names
    return tests


def items(tests, times, shards):
    def weight(binary, names):
        known = times.get(binary)
        if known is None:
            return UNKNOWN_TEST * len(names)
        return known["total"] * len(names) / max(known["count"], 1)

    target = sum(weight(binary, names) for binary, names in tests.items()) / shards
    found = []
    for binary, names in sorted(tests.items()):
        total = weight(binary, names)
        if total <= target / 2:
            found.append((total, f"binary_id(={binary})", len(names)))
            continue
        slow = {name: seconds for name, seconds in times.get(binary, {}).get("tests", {}).items() if name in names}
        for name, seconds in sorted(slow.items()):
            found.append((seconds, f"(binary_id(={binary}) & test(={name}))", 1))
        rest = sorted(set(names) - slow.keys())
        if not rest:
            continue
        seconds = max(total - sum(slow.values()), 0.0)
        chunks = max(1, min(len(rest), math.ceil(seconds / (target / 4))))
        for index in range(chunks):
            chunk = rest[index::chunks]
            names_in = " | ".join(f"test(={name})" for name in chunk)
            found.append((seconds * len(chunk) / len(rest), f"(binary_id(={binary}) & ({names_in}))", len(chunk)))
    return found


def plan(tests, times, shards):
    loads = [[0.0, [], 0] for _ in range(shards)]
    for seconds, expression, count in sorted(items(tests, times, shards), key=lambda item: (-item[0], item[1])):
        shard = min(loads, key=lambda load: load[0])
        shard[0] += seconds
        shard[1].append(expression)
        shard[2] += count
    return loads


def command_plan(args):
    tests = listed(json.loads(Path(args.list).read_text()))
    times = json.loads(TIMES.read_text()).get(args.os, {})
    loads = plan(tests, times, args.shards)
    config = [CONFIG.read_text()]
    for index, (seconds, expressions, count) in enumerate(loads, 1):
        filterset = " | ".join(expressions) if expressions else "none()"
        config.append(f"\n[profile.shard-{index}]\ninherits = \"ci\"\ndefault-filter = '''{filterset}'''\n")
        print(f"shard {index}/{args.shards}: {count} tests, {seconds:.0f} s of measured test time")
    Path(args.out).write_text("".join(config))


def cases(junit):
    for case in ET.parse(junit).getroot().iter("testcase"):
        yield case.get("classname"), case.get("name"), case


def command_report(args):
    expected = {}
    for listing in args.list:
        for binary, names in listed(json.loads(Path(listing).read_text())).items():
            expected.setdefault(binary, set()).update(names)
    ran, times, flaky, failed = {}, {}, [], []
    for junit in sorted(Path(args.junit).rglob("*.xml")):
        for binary, name, case in cases(junit):
            ran[(binary, name)] = ran.get((binary, name), 0) + 1
            times[(binary, name)] = float(case.get("time") or 0)
            if case.find("flakyFailure") is not None or case.find("flakyError") is not None:
                flaky.append((binary, name, junit.parent.name))
            elif case.find("failure") is not None or case.find("error") is not None:
                failed.append((binary, name, junit.parent.name))
    wanted = {(binary, name) for binary, names in expected.items() for name in names}
    missing = sorted(wanted - ran.keys())
    twice = sorted(key for key, count in ran.items() if count > 1)
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    grouped = {}
    for (binary, name), seconds in times.items():
        entry = grouped.setdefault(binary, {"total": 0.0, "count": 0, "tests": {}})
        entry["total"] += seconds
        entry["count"] += 1
        if seconds >= SLOW:
            entry["tests"][name] = round(seconds, 1)
    for entry in grouped.values():
        entry["total"] = round(entry["total"], 1)
    (out / "rust-test-times.json").write_text(json.dumps({args.os: grouped}, indent=1, sort_keys=True) + "\n")
    with (out / "test-times.tsv").open("w") as tsv:
        tsv.write("seconds\tbinary\ttest\n")
        for (binary, name), seconds in sorted(times.items(), key=lambda item: -item[1]):
            tsv.write(f"{seconds:.3f}\t{binary}\t{name}\n")
    lines = [f"## Rust tests on {args.os}", "",
             f"{len(ran)} of {len(wanted)} listed tests ran, {len(failed)} failed, {len(flaky)} passed only on retry (flaky).", ""]
    for title, rows in (("Flaky (failed, then passed on retry; the run still fails)", flaky), ("Failed", failed)):
        if rows:
            lines += [f"### {title}", "", "| binary | test | shard |", "|---|---|---|"]
            lines += [f"| {binary} | {name} | {shard} |" for binary, name, shard in rows]
            lines.append("")
    lines += ["### Slowest tests", "", "| seconds | binary | test |", "|---|---|---|"]
    lines += [f"| {seconds:.1f} | {binary} | {name} |" for (binary, name), seconds in sorted(times.items(), key=lambda item: -item[1])[:15]]
    (out / "summary.md").write_text("\n".join(lines) + "\n")
    print("\n".join(lines))
    if missing or twice:
        for binary, name in missing:
            print(f"::error::{binary} {name} is in the archive's test list but no shard ran it.")
        for binary, name in twice:
            print(f"::error::{binary} {name} ran in more than one shard.")
        return 1
    return 0


def failed(case):
    return any(case.find(tag) is not None for tag in ("failure", "error", "flakyFailure", "flakyError"))


def command_shadow(args):
    selection = Path(args.selection)
    record = json.loads(selection.read_text()) if selection.exists() else {"map": None, "everything": "the changes job recorded no selection", "skipped": []}
    skipped = set(record["skipped"])
    ran, failures = set(), set()
    for folder in args.junit:
        for junit in sorted(Path(folder).rglob("*.xml")):
            for binary, name, case in cases(junit):
                ran.add(f"{binary} {name}")
                if failed(case):
                    failures.add(f"{binary} {name}")
    misses = sorted(failures & skipped)
    lines = ["## Shadow test selection", ""]
    if record["everything"]:
        lines.append(f"It kept every test, because {record['everything']}.")
    else:
        lines.append(f"Against the coverage map of {record['map']} it would skip {len(skipped & ran)} of the {len(ran)} tests that ran. "
                     f"{len(misses)} of the {len(failures)} tests that failed or were flaky are among them. Nothing was skipped.")
    if misses:
        lines += ["", "### Failed tests it would have skipped (misses)", "", "| test |", "|---|"] + [f"| {test} |" for test in misses]
    out = Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps({"map": record["map"], "everything": record["everything"], "ran": len(ran), "skipped": len(skipped & ran),
                               "failed": sorted(failures), "misses": misses}, indent=1) + "\n")
    print("\n".join(lines))
    return 0


def covered_files(report, root):
    prefix = root.rstrip("/") + "/"
    return {entry["filename"][len(prefix):] for data in report["data"] for entry in data["files"]
            if entry["filename"].startswith(prefix) and entry["summary"]["lines"]["covered"] > 0}


def function_spans(report, root):
    prefix = root.rstrip("/") + "/"
    spans = {}
    for data in report["data"]:
        for function in data["functions"]:
            filename = function["filenames"][0]
            lines = [(region[0], region[2]) for region in function["regions"] if region[5] == 0]
            if filename.startswith(prefix) and lines:
                spans.setdefault(filename[len(prefix):], set()).add((min(start for start, _ in lines), max(end for _, end in lines)))
    return spans


def blob(path):
    data = path.read_bytes()
    return hashlib.sha1(b"blob %d\0" % len(data) + data).hexdigest()


def coverage_map(tests, spans, blobs, sha, created):
    files = sorted(set().union(*tests.values()))
    index = {file: number for number, file in enumerate(files)}
    return {"sha": sha, "created": created,
            "files": [{"path": file, "blob": blobs[file], "functions": sorted(map(list, spans.get(file, ())))} for file in files],
            "tests": {test: sorted(index[file] for file in covered) for test, covered in sorted(tests.items())}}


def command_coverage_map(args):
    passed, failures = set(), set()
    for binary, name, case in cases(args.junit):
        (failures if failed(case) else passed).add(f"{binary} {name}")
    passed -= failures
    objects = [line for line in Path(args.objects).read_text().splitlines() if line]

    def measure(folder):
        binary_id, name, binary = (folder / "test").read_text().splitlines()
        test = f"{binary_id} {name}"
        profiles = sorted(folder.rglob("*.profdata"))
        if test not in passed or not profiles:
            return None
        merged = folder / "merged.profdata"
        subprocess.run([f"{args.llvm}/llvm-profdata", "merge", "-sparse", "-o", str(merged), *map(str, profiles)], check=True)
        report = subprocess.run([f"{args.llvm}/llvm-cov", "export", "-summary-only", f"-instr-profile={merged}", binary,
                                 *(f"-object={item}" for item in objects if item != binary)],
                                check=True, capture_output=True, text=True).stdout
        return test, binary, merged, covered_files(json.loads(report), args.root)

    folders = sorted(folder for folder in Path(args.profiles).iterdir() if (folder / "test").exists())
    with ThreadPoolExecutor(os.cpu_count()) as pool:
        results = [result for result in pool.map(measure, folders) if result]
    tests = {test: covered for test, _, _, covered in results}
    total = Path(args.profiles) / "total.profdata"
    listing = Path(args.profiles) / "profiles.txt"
    listing.write_text("".join(f"{merged}\n" for _, _, merged, _ in results))
    subprocess.run([f"{args.llvm}/llvm-profdata", "merge", "-sparse", "-o", str(total), "-f", str(listing)], check=True)

    def spans_of(binary):
        report = subprocess.run([f"{args.llvm}/llvm-cov", "export", "-skip-expansions", f"-instr-profile={total}", binary],
                                check=True, capture_output=True, text=True).stdout
        return function_spans(json.loads(report), args.root)

    spans = {}
    with ThreadPoolExecutor(os.cpu_count()) as pool:
        for found in pool.map(spans_of, sorted({binary for _, binary, _, _ in results} | set(objects))):
            for file, functions in found.items():
                spans.setdefault(file, set()).update(functions)
    files = set().union(*tests.values())
    blobs = {file: blob(Path(args.root) / file) for file in files}
    Path(args.out).write_text(json.dumps(coverage_map(tests, spans, blobs, args.sha, args.created)) + "\n")
    print(f"The coverage map of {args.sha} records the files each of {len(tests)} tests ran. "
          f"It leaves out {len(failures)} tests that failed or were flaky and {len(passed - tests.keys())} passing tests with no profile, "
          f"so pull requests never skip them.")
    return 0

def term(text):
    binary, _, test = text.partition("=")
    return binary, test


def command_filter(args):
    clauses = []
    for binary, test in map(term, args.terms):
        clauses.append(f"(binary_id(={binary}) & test({test}))" if test else f"binary_id(={binary})")
    print(" | ".join(clauses))
    return 0


def command_matched(args):
    ran = [(binary, name) for binary, name, _ in cases(args.junit)]
    unmatched = [text for text in args.terms
                 if not any(binary == term(text)[0] and term(text)[1] in name for binary, name in ran)]
    for text in unmatched:
        print(f"::error::The filter term {text} matched no test, so a test it should run was renamed or moved. Update the term.")
    return 1 if unmatched else 0


def main(argv):
    parser = argparse.ArgumentParser(description="Build arguments, shard plans and reports for the Rust test jobs in ci.yml.")
    commands = parser.add_subparsers(dest="command", required=True)
    archive = commands.add_parser("archive-args", help="Print the cargo arguments that build the selected partitions, one per line.")
    archive.add_argument("selected", nargs="*")
    plans = commands.add_parser("plan", help="Write a nextest config with one profile per shard, balanced by measured test time.")
    plans.add_argument("--os", required=True)
    plans.add_argument("--shards", type=int, required=True)
    plans.add_argument("--list", required=True)
    plans.add_argument("--out", required=True)
    report = commands.add_parser("report", help="Check that the shards ran every listed test once and summarize timings and flaky tests.")
    report.add_argument("--os", required=True)
    report.add_argument("--list", action="append", required=True)
    report.add_argument("--junit", required=True)
    report.add_argument("--out", required=True)
    shadow = commands.add_parser("shadow", help="Count the failed tests that the shadow test selection would have skipped.")
    shadow.add_argument("--selection", required=True)
    shadow.add_argument("--junit", action="append", required=True)
    shadow.add_argument("--out", required=True)
    maps = commands.add_parser("coverage-map", help="Write the files each passing test ran, from one coverage profile folder per test.")
    maps.add_argument("--profiles", required=True)
    maps.add_argument("--junit", required=True)
    maps.add_argument("--objects", required=True)
    maps.add_argument("--llvm", required=True)
    maps.add_argument("--root", required=True)
    maps.add_argument("--sha", required=True)
    maps.add_argument("--created", type=int, required=True)
    maps.add_argument("--out", required=True)
    terms_help = "BINARY_ID, or BINARY_ID=SUBSTRING to select that binary's tests whose names contain SUBSTRING"
    filters = commands.add_parser("filter", help="Print the nextest filter expression that runs every term.")
    filters.add_argument("terms", nargs="+", help=terms_help)
    matched = commands.add_parser("matched", help="Fail unless every term matched at least one test in a nextest JUnit file.")
    matched.add_argument("--junit", required=True)
    matched.add_argument("terms", nargs="+", help=terms_help)
    args = parser.parse_args(argv)
    if args.command == "filter":
        return command_filter(args)
    if args.command == "matched":
        return command_matched(args)
    if args.command == "archive-args":
        print("\n".join(archive_args(set(args.selected))))
        return 0
    if args.command == "plan":
        command_plan(args)
        return 0
    if args.command == "shadow":
        return command_shadow(args)
    if args.command == "coverage-map":
        return command_coverage_map(args)
    return command_report(args)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
