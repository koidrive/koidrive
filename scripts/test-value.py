#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import argparse
import csv
from dataclasses import dataclass
import difflib
import json
import os
from pathlib import Path
import random
import re
import signal
import statistics
import subprocess
import sys
import tempfile
import time


@dataclass(frozen=True)
class Sample:
    mutate: tuple
    hand: bool
    cargo: tuple
    tests: str


SAMPLES = {
    "driver": Sample(
        mutate=("--package", "koidrive-driver"),
        hand=False,
        cargo=("--package", "koidrive-driver", "--lib"),
        tests="all()",
    ),
    "guard": Sample(
        mutate=("--file", "core/engine/src/guard.rs"),
        hand=False,
        cargo=("--package", "koidrive-engine", "--features", "serde", "--lib", "--tests"),
        tests="all()",
    ),
    "commit": Sample(
        mutate=("--file", "core/engine/src/commit.rs"),
        hand=True,
        cargo=("--package", "koidrive-engine", "--package", "koidrive-sim", "--features", "koidrive-engine/serde",
               "--cargo-profile", "checked", "--lib", "--tests"),
        tests="package(koidrive-engine) | binary_id(koidrive-sim::engine_sync) | binary_id(koidrive-sim::devices)",
    ),
}

STATUS = re.compile(
    r"^\s*(PASS|FAIL|TIMEOUT|LEAK|LEAK-FAIL|ABORT|SIG[A-Z]+)\s+\[\s*([0-9.]+)s\]\s+(?:\(\s*\d+/\s*\d+\)\s+)?(\S+)\s+(\S+)\s*$")
BUILD_FAILED = 101
BASELINE_PERIOD = 600
CASE_FILES = ("*.proptest-regressions", "*/proptest-regressions/*.txt", "*.cases/*.txt")
TEST_ENV = {
    "KOIDRIVE_PROPTEST_DETERMINISTIC": "true",
    "PROPTEST_RNG_SEED": "0",
    "CARGO_TERM_COLOR": "never",
}


def test_env():
    env = {key: value for key, value in os.environ.items() if key not in ("PROPTEST_CASES", "KOIDRIVE_BATTERY_CASES")}
    return {**env, **TEST_ENV}


def parse_status(output):
    results = {}
    for line in output.splitlines():
        match = STATUS.match(line)
        if match:
            status, seconds, binary, name = match.groups()
            results[f"{binary} {name}"] = (status, float(seconds))
    return results


def hand_mutants(root):
    listed = subprocess.run(
        ["bash", "-c", 'mutation() { printf "%s\\0%s\\0%s\\0" "$1" "$2" "$3"; }; source scripts/engine-mutations.sh'],
        cwd=root, capture_output=True, text=True, check=True).stdout.split("\0")[:-1]
    mutants, stale = [], []
    for name, file, expression in zip(listed[0::3], listed[1::3], listed[2::3]):
        source = (root / file).read_text()
        mutated = subprocess.run(["perl", "-0pe", expression], input=source, capture_output=True, text=True,
                                 check=True).stdout
        if mutated == source:
            stale.append(name)
            continue
        before, after = source.splitlines(keepends=True), mutated.splitlines(keepends=True)
        line = next(i for i, (a, b) in enumerate(zip(before + [""], after + [""])) if a != b) + 1
        diff = "".join(difflib.unified_diff(before, after, f"a/{file}", f"b/{file}"))
        mutants.append({"name": f"{name} (scripts/engine-mutations.sh)", "file": file, "line": line,
                        "function": None, "genre": "hand", "diff": diff})
    return mutants, stale


def generated_mutants(root, sample):
    listed = subprocess.run(["cargo", "mutants", "--list", "--json", "--diff", *sample.mutate],
                            cwd=root, capture_output=True, text=True, check=True).stdout
    mutants = []
    for mutant in json.loads(listed):
        diff = mutant["diff"].split("\n", 2)[2]
        function = mutant["function"]["function_name"] if mutant.get("function") else None
        mutants.append({"name": mutant["name"], "file": mutant["file"], "line": mutant["span"]["start"]["line"],
                        "function": function, "genre": mutant["genre"],
                        "diff": f"--- a/{mutant['file']}\n+++ b/{mutant['file']}\n{diff}"})
    return mutants


def plan(args):
    sample = SAMPLES[args.sample]
    hand, stale = hand_mutants(args.root) if sample.hand else ([], [])
    mutants = generated_mutants(args.root, sample) + hand
    for number, mutant in enumerate(mutants):
        mutant["id"] = number
    args.out.write_text(json.dumps({"sample": args.sample, "mutants": mutants, "stale": stale}, indent=1) + "\n")
    print(f"{len(mutants)} mutants for the {args.sample} sample")
    for name in stale:
        print(f"::warning::Hand mutant {name} in scripts/engine-mutations.sh no longer matches its file; it is left out.")


def shard(mutants, index, count):
    order = list(mutants)
    random.Random(0).shuffle(order)
    return sorted(order[index::count], key=lambda mutant: mutant["id"])


def selection(sample, tests, include):
    terms = " | ".join(f"(binary_id(={test.split(' ', 1)[0]}) & test(={test.split(' ', 1)[1]}))" for test in sorted(tests))
    if not terms:
        return sample.tests if include is False else "none()"
    return f"({sample.tests}) & ({terms})" if include else f"({sample.tests}) & !({terms})"


def nextest_config(directory, baseline, period):
    lines = ["[profile.test-value]", "fail-fast = false", "retries = 0",
             f'slow-timeout = {{ period = "{period}s", terminate-after = 2 }}']
    for test, seconds in sorted(baseline.items()):
        if seconds * 1.5 > period:
            binary, name = test.split(" ", 1)
            lines += ["", "[[profile.test-value.overrides]]",
                      f"filter = 'binary_id(={binary}) & test(={name})'",
                      f'slow-timeout = {{ period = "{int(seconds * 1.5) + 1}s", terminate-after = 2 }}']
    path = Path(directory) / "nextest.toml"
    path.write_text("\n".join(lines) + "\n")
    return path


def nextest(root, sample, expression, config, status_level, limit, log):
    command = ["cargo", "nextest", "run", "--locked", "--no-fail-fast", "--no-tests", "pass", "--color", "never",
               "--hide-progress-bar", "--config-file", str(config), "--profile", "test-value",
               "--status-level", status_level, "--final-status-level", "none", *sample.cargo, "-E", expression]
    started = time.monotonic()
    process = subprocess.Popen(command, cwd=root, env=test_env(), stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                               text=True, start_new_session=True)
    try:
        output, _ = process.communicate(timeout=limit)
        incomplete = False
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        output, _ = process.communicate()
        incomplete = True
    if log:
        log.write(output)
    return process.returncode, parse_status(output), incomplete, time.monotonic() - started


def restore(root, file):
    subprocess.run(["git", "checkout", "-q", "--", "."], cwd=root, check=True)
    untracked = subprocess.run(["git", "ls-files", "-z", "--others", "--exclude-standard", "--", *CASE_FILES],
                               cwd=root, capture_output=True, text=True, check=True).stdout.split("\0")
    for path in filter(None, untracked):
        (root / path).unlink()
    (root / file).touch()


def require_clean(root):
    dirty = subprocess.run(["git", "status", "--porcelain", "--untracked-files=no"], cwd=root, capture_output=True,
                           text=True, check=True).stdout
    if dirty:
        sys.exit("The checkout has uncommitted changes. Commit them first: this script restores files with git.")


def baseline(args):
    require_clean(args.root)
    sample = SAMPLES[args.sample]
    runs = []
    with tempfile.TemporaryDirectory() as directory, open(args.out.with_suffix(".log"), "w") as log:
        config = nextest_config(directory, {}, BASELINE_PERIOD)
        for run in range(args.runs):
            code, results, _, seconds = nextest(args.root, sample, sample.tests, config, "all", None, log)
            if not results:
                sys.exit(f"Baseline run {run + 1} reported no tests (exit {code}); see {log.name}.")
            runs.append((results, seconds))
    tests = sorted(set().union(*(results for results, _ in runs)))
    flaky = [test for test in tests if any(results.get(test, ("missing",))[0] != "PASS" for results, _ in runs)]
    durations = {test: statistics.median(results[test][1] for results, _ in runs if test in results) for test in tests}
    args.out.write_text(json.dumps({"sample": args.sample, "runs": args.runs,
                                    "walls": [round(seconds, 1) for _, seconds in runs],
                                    "durations": durations, "flaky": flaky}, indent=1) + "\n")
    print(f"{len(tests)} tests, {len(flaky)} not passing in every run, walls {[round(s) for _, s in runs]} s")


def run(args):
    require_clean(args.root)
    sample = SAMPLES[args.sample]
    planned = json.loads(args.mutants.read_text())
    base = json.loads(args.baseline.read_text())
    if planned["sample"] != args.sample or base["sample"] != args.sample:
        sys.exit("The mutant list and baseline belong to another sample.")
    durations = {test: seconds for test, seconds in base["durations"].items() if test not in base["flaky"]}
    slow = {test for test, seconds in durations.items() if seconds >= args.slow}
    fast_expression = selection(sample, set(base["flaky"]) | slow, include=False)
    slow_expression = selection(sample, slow, include=True)
    limit = max(900, 3 * max(base["walls"]))
    index, count = (int(part) for part in args.shard.split("/"))
    with tempfile.TemporaryDirectory() as directory, open(args.out, "a") as out, \
            open(args.out.with_suffix(".log"), "a") as log:
        config = nextest_config(directory, durations, args.period)
        done = {json.loads(line)["mutant"] for line in args.out.read_text().splitlines()} if args.out.stat().st_size else set()
        for mutant in shard(planned["mutants"], index, count):
            if mutant["id"] in done:
                continue
            log.write(f"\n===== mutant {mutant['id']}: {mutant['name']}\n")
            log.flush()
            started = time.monotonic()
            applied = subprocess.run(["git", "apply", "-"], cwd=args.root, input=mutant["diff"], text=True,
                                     capture_output=True)
            record = {"mutant": mutant["id"], "killers": [], "incomplete": False, "slow_tier": False}
            if applied.returncode != 0:
                record["status"] = "stale"
            else:
                code, results, incomplete, _ = nextest(args.root, sample, fast_expression, config, "fail", limit, log)
                killers = sorted(test for test, (status, _) in results.items() if status != "PASS")
                if code == BUILD_FAILED and not results:
                    record["status"] = "unviable"
                else:
                    if not killers and slow:
                        record["slow_tier"] = True
                        code, results, incomplete, _ = nextest(args.root, sample, slow_expression, config, "fail",
                                                               limit, log)
                        killers = sorted(test for test, (status, _) in results.items() if status != "PASS")
                    record.update(status="killed" if killers or incomplete else "survived", killers=killers,
                                  timeouts=sorted(t for t, (status, _) in results.items() if status == "TIMEOUT"),
                                  incomplete=incomplete)
                restore(args.root, mutant["file"])
            record["seconds"] = round(time.monotonic() - started, 1)
            out.write(json.dumps(record) + "\n")
            out.flush()
            print(f"{mutant['id']}: {record['status']} by {len(record['killers'])} tests in {record['seconds']} s "
                  f"({mutant['name']})", flush=True)


def property_tests(root):
    names = set()
    for path in root.glob("core/**/*.rs"):
        if "/target/" in str(path):
            continue
        text = path.read_text(errors="replace")
        for block in re.finditer(r"proptest!\s*\{", text):
            depth, end = 0, block.end() - 1
            for end in range(block.end() - 1, len(text)):
                depth += {"{": 1, "}": -1}.get(text[end], 0)
                if depth == 0:
                    break
            names.update(re.findall(r"\bfn\s+(\w+)\s*\(", text[block.end():end]))
    return names


def cover(test, kills, others):
    exact = [other for other in others if other != test and kills[test] <= kills[other]]
    if exact:
        return [min(exact, key=lambda other: (others[other], other))]
    left, chosen = set(kills[test]), []
    while left:
        best = max((other for other in others if other != test), key=lambda other: (len(left & kills[other]), -others[other]))
        if not left & kills[best]:
            break
        chosen.append(best)
        left -= kills[best]
    return chosen


def analyse(runs, properties, slow_seconds, few):
    durations, flaky, mutants, by_mutant = {}, set(), {}, {}
    for planned, base, records in runs:
        for test, seconds in base["durations"].items():
            durations.setdefault(test, seconds)
        flaky.update(base["flaky"])
        for mutant in planned["mutants"]:
            mutants[(planned["sample"], mutant["id"])] = mutant
        for record in records:
            by_mutant[(planned["sample"], record["mutant"])] = record
    kills = {test: set() for test in durations}
    for key, record in by_mutant.items():
        for test in record["killers"]:
            kills.setdefault(test, set()).add(key)
    unique = {test: {key for key in killed if len(by_mutant[key]["killers"]) == 1} for test, killed in kills.items()}
    rows = []
    for test in sorted(kills):
        seconds = durations.get(test, 0.0)
        kind = "property" if test.split(" ", 1)[-1].rsplit("::", 1)[-1] in properties else "unit"
        covered = cover(test, kills, durations) if kills[test] and not unique[test] else []
        if test in flaky:
            verdict = "flaky in baseline: not measured"
        elif kind == "property":
            verdict = "keep: property test"
        elif not kills[test]:
            verdict = "no kills in the mutated code"
        elif not unique[test]:
            verdict = "delete or merge"
        elif seconds >= slow_seconds and len(unique[test]) <= few:
            verdict = "move off the merge path"
        else:
            verdict = "keep"
        rows.append({"test": test, "kind": kind, "seconds": round(seconds, 3), "kills": len(kills[test]),
                     "unique_kills": len(unique[test]), "covered_by": "; ".join(covered), "verdict": verdict})
    mutant_rows = []
    for (sample, number), mutant in sorted(mutants.items()):
        record = by_mutant.get((sample, number))
        mutant_rows.append({"sample": sample, "id": number, "name": mutant["name"], "file": mutant["file"],
                            "line": mutant["line"], "function": mutant["function"] or "", "genre": mutant["genre"],
                            "status": record["status"] if record else "not run",
                            "slow_tier": bool(record and record.get("slow_tier")),
                            "killers": len(record["killers"]) if record else 0,
                            "killed_by": "; ".join(record["killers"][:5]) if record else ""})
    return rows, mutant_rows


def write_csv(path, rows):
    with open(path, "w", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=list(rows[0]), lineterminator="\n")
        writer.writeheader()
        writer.writerows(rows)


def load_run(directory):
    planned = json.loads((directory / "mutants.json").read_text())
    base = json.loads((directory / "baseline.json").read_text())
    records = [json.loads(line) for path in sorted(directory.glob("results-*.jsonl"))
               for line in path.read_text().splitlines() if line]
    return planned, base, records


def report(args):
    runs = [load_run(directory) for directory in args.runs]
    name = "-".join(planned["sample"] for planned, _, _ in runs)
    rows, mutant_rows = analyse(runs, property_tests(args.root), args.slow, args.few)
    args.out.mkdir(parents=True, exist_ok=True)
    write_csv(args.out / f"{name}-tests.csv", rows)
    write_csv(args.out / f"{name}-mutants.csv", mutant_rows)
    write_csv(args.out / f"{name}-survivors.csv", [row for row in mutant_rows if row["status"] == "survived"])
    statuses, verdicts = {}, {}
    for row in mutant_rows:
        statuses[row["status"]] = statuses.get(row["status"], 0) + 1
    for row in rows:
        verdicts[row["verdict"]] = verdicts.get(row["verdict"], 0) + 1
    hours = sum(record["seconds"] for _, _, records in runs for record in records) / 3600
    summary = [f"### Test value: {name}", "",
               f"{len(mutant_rows)} mutants: " + ", ".join(f"{count} {status}" for status, count in sorted(statuses.items())),
               "",
               f"{len(rows)} tests: " + ", ".join(f"{count} {verdict}" for verdict, count in sorted(verdicts.items())),
               "",
               f"Mutant runtime: {hours:.1f} runner-hours"]
    (args.out / f"{name}-summary.md").write_text("\n".join(summary) + "\n")
    print("\n".join(summary))


def main(argv=None):
    parser = argparse.ArgumentParser(description="Measure each test's unique mutant kills against its runtime.")
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parent.parent)
    commands = parser.add_subparsers(dest="command", required=True)
    for name in ("plan", "baseline", "run", "report"):
        command = commands.add_parser(name)
        command.add_argument("--sample", choices=sorted(SAMPLES), required=True)
        command.add_argument("--slow", type=float, default=10.0, help="baseline seconds from which a test runs only on mutants the faster tests miss")
        command.add_argument("--period", type=int, default=30, help="seconds after which nextest flags a test as slow; twice that ends it")
    commands.choices["plan"].add_argument("--out", type=Path, required=True)
    commands.choices["baseline"].add_argument("--out", type=Path, required=True)
    commands.choices["baseline"].add_argument("--runs", type=int, default=3)
    commands.choices["run"].add_argument("--mutants", type=Path, required=True)
    commands.choices["run"].add_argument("--baseline", type=Path, required=True)
    commands.choices["run"].add_argument("--shard", default="0/1")
    commands.choices["run"].add_argument("--out", type=Path, required=True)
    commands.choices["report"].add_argument("--runs", type=Path, nargs="+", required=True,
                                            help="directories that each hold one run's mutants.json, baseline.json and results-*.jsonl")
    commands.choices["report"].add_argument("--few", type=int, default=2, help="unique kills at or below which a slow test may move off the merge path")
    commands.choices["report"].add_argument("--out", type=Path, required=True)
    args = parser.parse_args(argv)
    if args.command == "run" and not re.fullmatch(r"(\d+)/([1-9]\d*)", args.shard):
        parser.error("--shard takes K/N, for example 0/8")
    if args.command == "run":
        args.out.touch()
    {"plan": plan, "baseline": baseline, "run": run, "report": report}[args.command](args)


if __name__ == "__main__":
    main()
