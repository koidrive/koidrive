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
    r"^\s*(PASS|FAIL(?:\s+\+\s+LEAK)?|TIMEOUT|LEAK|LEAK-FAIL|ABORT|SIG[A-Z]+)\s+\[\s*([0-9.]+)s\]\s+(?:\(\s*\d+/\s*\d+\)\s+)?(\S+)\s+(\S+)\s*$")
BUILD_FAILED = 101
TESTS_FAILED = 100
PASSING = ("PASS", "LEAK")
ATTRIBUTED = ("killed", "survived")
BASELINE_PERIOD = 600
TERMINATE_GRACE = 60
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


def stop(process):
    interrupted = []
    previous = signal.signal(signal.SIGINT, lambda *_: interrupted.append(True))
    try:
        os.killpg(process.pid, signal.SIGTERM)
        try:
            return process.communicate(timeout=TERMINATE_GRACE)[0]
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            return process.communicate()[0]
    finally:
        signal.signal(signal.SIGINT, previous)
        if interrupted:
            raise KeyboardInterrupt


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
        output, incomplete = stop(process), True
    except BaseException:
        stop(process)
        raise
    if log:
        log.write(output)
    return process.returncode, parse_status(output), incomplete, time.monotonic() - started


def clean(root):
    subprocess.run(["git", "checkout", "-q", "--", "."], cwd=root, check=True)
    untracked = subprocess.run(["git", "ls-files", "-z", "--others", "--exclude-standard", "--", *CASE_FILES],
                               cwd=root, capture_output=True, text=True, check=True).stdout.split("\0")
    for path in filter(None, untracked):
        (root / path).unlink()


def restore(root, file):
    clean(root)
    (root / file).touch()


def require_clean(root):
    dirty = subprocess.run(["git", "status", "--porcelain", "--untracked-files=no"], cwd=root, capture_output=True,
                           text=True, check=True).stdout
    cases = subprocess.run(["git", "ls-files", "--others", "--exclude-standard", "--", *CASE_FILES], cwd=root,
                           capture_output=True, text=True, check=True).stdout
    if dirty or cases:
        sys.exit("The checkout has uncommitted changes or untracked regression case files. Commit or move them first: "
                 "this script restores files with git and deletes the case files a run writes.")


def baseline(args):
    require_clean(args.root)
    sample = SAMPLES[args.sample]
    runs = []
    with tempfile.TemporaryDirectory() as directory, open(args.out.with_suffix(".log"), "w") as log:
        config = nextest_config(directory, {}, BASELINE_PERIOD)
        for run in range(args.runs):
            try:
                code, results, _, seconds = nextest(args.root, sample, sample.tests, config, "all", None, log)
            finally:
                clean(args.root)
            if not results:
                sys.exit(f"Baseline run {run + 1} reported no tests (exit {code}); see {log.name}.")
            runs.append((results, seconds))
    tests = sorted(set().union(*(results for results, _ in runs)))
    flaky = [test for test in tests if any(results.get(test, ("missing",))[0] not in PASSING for results, _ in runs)]
    durations = {test: statistics.median(results[test][1] for results, _ in runs if test in results) for test in tests}
    args.out.write_text(json.dumps({"sample": args.sample, "runs": args.runs,
                                    "walls": [round(seconds, 1) for _, seconds in runs],
                                    "durations": durations, "flaky": flaky}, indent=1) + "\n")
    print(f"{len(tests)} tests, {len(flaky)} not passing in every run, walls {[round(s) for _, s in runs]} s")


def tier(root, sample, expression, config, limit, log):
    code, results, incomplete, _ = nextest(root, sample, expression, config, "fail", limit, log)
    killers = sorted(test for test, (status, _) in results.items() if status not in PASSING)
    timeouts = sorted(test for test, (status, _) in results.items() if status == "TIMEOUT")
    if code == BUILD_FAILED and not results:
        status = "unviable"
    elif incomplete:
        status = "incomplete"
    elif code == 0 and not killers:
        status = "survived"
    elif code == TESTS_FAILED and killers:
        status = "killed"
    else:
        status = "error"
    return {"status": status, "killers": killers, "timeouts": timeouts, "exit": code}


def run(args):
    require_clean(args.root)
    sample = SAMPLES[args.sample]
    planned = json.loads(args.mutants.read_text())
    base = json.loads(args.baseline.read_text())
    if planned["sample"] != args.sample or base["sample"] != args.sample:
        sys.exit("The mutant list and baseline belong to another sample.")
    durations = {test: seconds for test, seconds in base["durations"].items() if test not in base["flaky"]}
    if not durations:
        sys.exit("The baseline has no eligible tests: every test failed in at least one baseline run.")
    slow = {test for test, seconds in durations.items() if seconds >= args.slow}
    fast_expression = selection(sample, set(base["flaky"]) | slow, include=False)
    slow_expression = selection(sample, slow, include=True)
    limit = max(900, 3 * max(base["walls"]))
    index, count = (int(part) for part in args.shard.split("/"))
    with tempfile.TemporaryDirectory() as directory, open(args.out, "w") as out, \
            open(args.out.with_suffix(".log"), "w") as log:
        config = nextest_config(directory, durations, args.period)
        for mutant in shard(planned["mutants"], index, count):
            log.write(f"\n===== mutant {mutant['id']}: {mutant['name']}\n")
            log.flush()
            started = time.monotonic()
            applied = subprocess.run(["git", "apply", "-"], cwd=args.root, input=mutant["diff"], text=True,
                                     capture_output=True)
            record = {"mutant": mutant["id"], "killers": [], "slow_tier": False}
            if applied.returncode != 0:
                record["status"] = "stale"
            else:
                try:
                    record.update(tier(args.root, sample, fast_expression, config, limit, log))
                    if record["status"] == "survived" and slow:
                        record.update(tier(args.root, sample, slow_expression, config, limit, log), slow_tier=True)
                finally:
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
        text = re.sub(r'//[^\n]*|/\*.*?\*/|"(?:\\.|[^"\\])*"|\'(?:\\.|[^\'\\])\'',
                      lambda literal: re.sub(r"[^\n]", " ", literal.group()), path.read_text(errors="replace"),
                      flags=re.DOTALL)
        for block in re.finditer(r"proptest!\s*\{", text):
            depth, end = 0, block.end() - 1
            for end in range(block.end() - 1, len(text)):
                depth += {"{": 1, "}": -1}.get(text[end], 0)
                if depth == 0:
                    break
            names.update(re.findall(r"\bfn\s+(\w+)\s*\(", text[block.end():end]))
    return names


def cover(kills, killed, candidates, durations):
    left, chosen = set(killed), []
    while left and candidates:
        best = max(sorted(candidates), key=lambda other: (len(left & kills[other]), -durations.get(other, 0.0)))
        if not left & kills[best]:
            break
        chosen.append(best)
        left -= kills[best]
    return chosen


def scope(mutants):
    files = sorted({mutant["file"] for mutant in mutants})
    return ", ".join(files) if len(files) <= 3 else f"{os.path.commonpath(files)} ({len(files)} files)"


def analyse(runs, properties, slow_seconds, few):
    durations, timings, flaky, mutants, by_mutant = {}, {}, {}, {}, {}
    for planned, base, records in runs:
        for test, seconds in base["durations"].items():
            durations[test] = max(seconds, durations.get(test, 0.0))
            timings.setdefault(test, {})[planned["sample"]] = seconds
        for test in base["flaky"]:
            flaky.setdefault(test, set()).add(planned["sample"])
        for mutant in planned["mutants"]:
            mutants[(planned["sample"], mutant["id"])] = mutant
        for record in records:
            by_mutant[(planned["sample"], record["mutant"])] = record
    measured = scope(mutants.values())
    attributed = {key: record for key, record in by_mutant.items() if record["status"] in ATTRIBUTED}
    provisional = len(attributed) + sum(record["status"] == "unviable" for record in by_mutant.values()) < len(mutants)
    kills = {test: set() for test in durations}
    for key, record in attributed.items():
        for test in record["killers"]:
            kills.setdefault(test, set()).add(key)
    killers = {key: set(record["killers"]) for key, record in attributed.items()}
    unique = {test: {key for key in killed if len(killers[key]) == 1} for test, killed in kills.items()}

    def kind(test):
        return "property" if test.split(" ", 1)[-1].rsplit("::", 1)[-1] in properties else "unit"

    def protected(test):
        return test in flaky or kind(test) == "property"

    merge_path = set(kills)
    verdicts, lost, losses = {}, {}, {}
    fast = [test for test in kills if durations.get(test, 0.0) < slow_seconds and kills[test] and not protected(test)]
    for test in sorted(fast, key=lambda test: (-durations.get(test, 0.0), test)):
        if all(killers[key] & merge_path - {test} for key in kills[test]):
            merge_path.discard(test)
            lost[test] = set()
            verdicts[test] = f"redundant within {measured}"
    slow = [test for test in kills if durations.get(test, 0.0) >= slow_seconds and not protected(test)]
    moved = set()

    def alone(test, path):
        return {key for key in kills[test] if not killers[key] & path}

    for test in sorted(slow, key=lambda test: (-durations.get(test, 0.0), test)):
        if all(len(alone(other, merge_path - {test})) <= few for other in moved | {test}):
            merge_path.discard(test)
            moved.add(test)
    for test in slow:
        losses[test] = len(alone(test, merge_path - {test}))
    for test in moved:
        lost[test] = alone(test, merge_path)
        verdicts[test] = (f"slow, alone catches {len(lost[test])} within {measured}" if lost[test]
                          else f"slow, catches nothing else within {measured}")
    covers = {test: cover(kills, kills[test] - lost[test], merge_path, durations) for test in lost}
    def ran_on(test):
        tiers = {sample: "no mutant" if sample in flaky.get(test, ()) else
                 "mutants the faster tests missed" if seconds >= slow_seconds else "every mutant"
                 for sample, seconds in sorted(timings.get(test, {}).items())}
        if len(set(tiers.values())) <= 1:
            return next(iter(tiers.values()), "every mutant")
        return "; ".join(f"{tier} in {sample}" for sample, tier in tiers.items())

    rows = []
    for test in sorted(kills):
        seconds = durations.get(test, 0.0)
        if test in flaky and flaky[test] >= set(timings.get(test, {})):
            verdict = "flaky in baseline: not measured"
        elif test in flaky:
            verdict = f"flaky in the {', '.join(sorted(flaky[test]))} baseline: not measured there"
        elif kind(test) == "property":
            verdict = "keep: property test"
        elif test in verdicts:
            verdict = verdicts[test]
        elif not kills[test] and seconds < slow_seconds:
            verdict = f"no kills within {measured}"
        else:
            verdict = "keep"
        if provisional:
            verdict += " (provisional)"
        rows.append({"test": test, "kind": kind(test), "seconds": round(seconds, 3),
                     "ran_on": ran_on(test),
                     "kills": len(kills[test]), "unique_kills": len(unique[test]),
                     "merge_path_loses": losses.get(test, ""), "covered_by": "; ".join(covers.get(test, [])),
                     "verdict": verdict, "mutated": measured})
    mutant_rows = []
    for (sample, number), mutant in sorted(mutants.items()):
        record = by_mutant.get((sample, number))
        mutant_rows.append({"sample": sample, "id": number, "name": mutant["name"], "file": mutant["file"],
                            "line": mutant["line"], "function": mutant["function"] or "", "genre": mutant["genre"],
                            "status": record["status"] if record else "not run",
                            "slow_tier": bool(record and record.get("slow_tier")),
                            "killers": len(record["killers"]) if record else 0,
                            "killed_by": "; ".join(record["killers"][:5]) if record else ""})
    return rows, mutant_rows, measured, provisional, len(set().union(*lost.values()))


TEST_FIELDS = ["test", "kind", "seconds", "ran_on", "kills", "unique_kills", "merge_path_loses", "covered_by", "verdict",
               "mutated"]
MUTANT_FIELDS = ["sample", "id", "name", "file", "line", "function", "genre", "status", "slow_tier", "killers",
                 "killed_by"]


def write_csv(path, fields, rows):
    with open(path, "w", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=fields, lineterminator="\n")
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
    samples = [planned["sample"] for planned, _, _ in runs]
    if len(set(samples)) != len(samples):
        sys.exit(f"Each run must measure a different sample, got {', '.join(samples)}.")
    name = "-".join(planned["sample"] for planned, _, _ in runs)
    rows, mutant_rows, measured, provisional, lost = analyse(runs, property_tests(args.root), args.slow, args.few)
    args.out.mkdir(parents=True, exist_ok=True)
    write_csv(args.out / f"{name}-tests.csv", TEST_FIELDS, rows)
    write_csv(args.out / f"{name}-mutants.csv", MUTANT_FIELDS, mutant_rows)
    write_csv(args.out / f"{name}-survivors.csv", MUTANT_FIELDS, [row for row in mutant_rows if row["status"] == "survived"])
    statuses, verdicts = {}, {}
    for row in mutant_rows:
        statuses[row["status"]] = statuses.get(row["status"], 0) + 1
    for row in rows:
        verdicts[row["verdict"]] = verdicts.get(row["verdict"], 0) + 1
    hours = sum(record["seconds"] for _, _, records in runs for record in records) / 3600
    summary = [f"### Test value: {name}", "",
               f"Mutated code: {measured}. A verdict covers only what a test catches there; its value elsewhere is not measured.",
               "",
               *(["**Provisional:** some mutants have no result (not run, error or incomplete), so verdicts may change.", ""]
                 if provisional else []),
               f"{len(mutant_rows)} mutants: " + ", ".join(f"{count} {status}" for status, count in sorted(statuses.items())),
               "",
               f"{len(rows)} tests: " + ", ".join(f"{count} {verdict}" for verdict, count in sorted(verdicts.items())),
               "",
               f"Killed mutants no test left on the merge path would catch, if every slow candidate moved: {lost}",
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
    commands.choices["report"].add_argument("--few", type=int, default=0, help="mutants a slow test may alone catch and still move off the merge path")
    commands.choices["report"].add_argument("--out", type=Path, required=True)
    args = parser.parse_args(argv)
    if args.command == "run" and not re.fullmatch(r"(\d+)/([1-9]\d*)", args.shard):
        parser.error("--shard takes K/N, for example 0/8")
    if args.command == "run" and int(args.shard.split("/")[0]) >= int(args.shard.split("/")[1]):
        parser.error("--shard K/N needs K below N")
    {"plan": plan, "baseline": baseline, "run": run, "report": report}[args.command](args)


if __name__ == "__main__":
    main()
