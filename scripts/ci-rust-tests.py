#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
import argparse
import json
import math
from pathlib import Path
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
            if case.find("flakyFailure") is not None:
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
        entry["total"] = round(entry["total"] + seconds, 1)
        entry["count"] += 1
        if seconds >= SLOW:
            entry["tests"][name] = round(seconds, 1)
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
    args = parser.parse_args(argv)
    if args.command == "archive-args":
        print("\n".join(archive_args(set(args.selected))))
        return 0
    if args.command == "plan":
        command_plan(args)
        return 0
    return command_report(args)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
