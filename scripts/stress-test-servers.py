#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import argparse
from dataclasses import dataclass
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time


WORKERS = {
    "koidrive-net": ("core/net/scripts/integration-test.sh", "server"),
    "koidrive-driver": ("core/driver/scripts/integration-test.sh", "sync"),
}


@dataclass
class Run:
    number: int
    seed: int
    process: subprocess.Popen
    started: float
    log: object
    sandbox: object


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description="Repeat prebuilt Cargo tests or isolated Worker launchers on one host.")
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--mode", choices=("cargo", "worker"), default="cargo")
    parser.add_argument("--crate", required=True)
    parser.add_argument("--test", default="")
    parser.add_argument("--filter", default="")
    parser.add_argument("--features", default="")
    parser.add_argument("--cases", default="")
    parser.add_argument("--ignored", choices=("true", "false", ""), default="false")
    parser.add_argument("--repeat", type=int, default=1)
    parser.add_argument("--parallel", type=int, default=1)
    parser.add_argument("--seed", type=int, required=True)
    parser.add_argument("--timeout-seconds", type=int, default=10800)
    args = parser.parse_args(argv)
    if not 1 <= args.parallel <= 8 or not 1 <= args.repeat <= 100:
        parser.error("parallel must be 1..8 and repeat must be 1..100")
    if args.seed < 0 or args.timeout_seconds < 1:
        parser.error("seed must be nonnegative and timeout-seconds must be positive")
    if not re.fullmatch(r"[A-Za-z0-9_-]+", args.crate):
        parser.error("crate must be a package name")
    if not re.fullmatch(r"[A-Za-z0-9_-]*", args.test) or args.filter.startswith("-"):
        parser.error("test and filter must not be options")
    if not re.fullmatch(r"[A-Za-z0-9_/,-]*", args.features) or not re.fullmatch(r"[0-9]{0,7}", args.cases):
        parser.error("invalid features or cases")
    if args.mode == "worker":
        if args.crate not in WORKERS:
            parser.error("worker mode supports koidrive-net and koidrive-driver")
        target = WORKERS[args.crate][1]
        if args.test and args.test != target:
            parser.error(f"{args.crate} worker launcher runs --test {target}")
        args.test = target
    args.root = args.root.resolve()
    args.out = args.out.resolve()
    return args


def cargo_command(args):
    command = ["cargo", "test", "--locked", "--no-fail-fast", "-p", args.crate]
    if args.features:
        command += ["--features", args.features]
    if args.test:
        command += ["--test", args.test]
    command += ["--"]
    if args.filter:
        command += [args.filter]
    if args.ignored == "true":
        command += ["--include-ignored"]
    return command


def worker_sandbox(root, destination, crate, cargo, features):
    launcher = Path(WORKERS[crate][0])
    shutil.copytree(root / "server", destination / "server",
                    ignore=shutil.ignore_patterns("node_modules", ".wrangler", ".git"), symlinks=True)
    config = destination / "server/wrangler.jsonc"
    text = re.sub(r'"(?:\\.|[^"\\])*"|//[^\r\n]*|/\*.*?\*/',
                  lambda match: match[0] if match[0].startswith('"') else "", config.read_text(), flags=re.S)
    text = re.sub(r'"(?:\\.|[^"\\])*"|,\s*(?=[}\]])',
                  lambda match: match[0] if match[0].startswith('"') else "", text)
    settings = json.loads(text)
    build = settings.get("build", {})
    if build.get("command") not in (None, "pnpm build:web"):
        raise ValueError("Worker stress mode only prebuilds the pnpm build:web custom build")
    build.pop("command", None)
    config.write_text(json.dumps(settings, indent=2) + "\n")
    for directory, names, _ in os.walk(root / "server"):
        if "node_modules" in names:
            dependencies = Path(directory) / "node_modules"
            (destination / dependencies.relative_to(root)).symlink_to(dependencies, target_is_directory=True)
        names[:] = [name for name in names if name not in ("node_modules", ".wrangler", ".git")]
    shutil.copyfile(root / "Cargo.toml", destination / "Cargo.toml")
    shutil.copytree(root / "scripts", destination / "scripts", ignore=shutil.ignore_patterns("__pycache__"))
    (destination / launcher).parent.mkdir(parents=True)
    shutil.copyfile(root / launcher, destination / launcher)
    binary = destination / "bin/cargo"
    binary.parent.mkdir()
    binary.write_text(
        "#!" + sys.executable + "\n"
        "import os, sys\n"
        f"os.chdir({str(root)!r})\n"
        "args = sys.argv[1:]\n"
        f"features = {features!r}\n"
        "if features:\n"
        "    index = args.index('--') if '--' in args else len(args)\n"
        "    args[index:index] = ['--features', features]\n"
        f"os.execv({cargo!r}, [{cargo!r}, *args])\n"
    )
    binary.chmod(0o755)
    return ["bash", str(destination / launcher)]


def terminate(process):
    groups = {process.pid}
    try:
        rows = subprocess.check_output(["ps", "-eo", "pid=,ppid=,pgid="], text=True)
        tree = [tuple(map(int, row.split())) for row in rows.splitlines()]
        descendants = {process.pid}
        while True:
            children = {pid for pid, parent, _ in tree if parent in descendants}
            if children <= descendants:
                break
            descendants |= children
        groups |= {group for pid, _, group in tree if pid in descendants}
    except (OSError, subprocess.SubprocessError, ValueError):
        pass
    groups.discard(os.getpgrp())
    for group in groups:
        try:
            os.killpg(group, signal.SIGTERM)
        except ProcessLookupError:
            pass
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        pass
    for group in groups:
        try:
            os.killpg(group, signal.SIGKILL)
        except ProcessLookupError:
            pass
    process.wait()


def result(number, seed, code, text, elapsed, timed_out=False):
    plain = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", text)
    counts = re.findall(r"^test result: \w+\. (\d+) passed; (\d+) failed; (\d+) ignored", plain, re.M)
    passed, failed, ignored = (sum(int(row[i]) for row in counts) for i in range(3))
    binaries = len(re.findall(r"^\s*(?:Running |Doc-tests )", plain, re.M))
    if timed_out:
        status = "timeout"
    elif code == 0 and passed > 0 and failed == 0 and len(counts) >= binaries:
        status = "pass"
    elif code != 0 and failed > 0 and len(counts) >= binaries:
        status = "fail"
    else:
        status = "error"
    return dict(run=number, seed=seed, exit_code=code, result=status,
                passed=passed, failed=failed, ignored=ignored, test_binaries=binaries,
                finished_binaries=len(counts), seconds=round(elapsed, 2),
                failed_tests=re.findall(r"^test (.+) \.\.\. FAILED$", plain, re.M))


def save_regressions(root, out):
    paths = subprocess.check_output(
        ["git", "status", "--porcelain", "-z", "--untracked-files=all", "--", "*.proptest-regressions",
         "*/proptest-regressions/*.txt", "*.cases/*.txt"], cwd=root)
    for entry in paths.split(b"\0"):
        if entry:
            relative = Path(os.fsdecode(entry[3:]))
            source = root / relative
            if source.is_file():
                target = out / "regressions" / relative
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(source, target)


def run_tests(args):
    (args.out / "logs").mkdir(parents=True, exist_ok=True)
    cargo = shutil.which("cargo")
    if cargo is None:
        raise RuntimeError("cargo is missing; build the tests before running this script")
    active = []
    results = []
    next_run = 1
    try:
        while active or next_run <= args.repeat:
            while len(active) < args.parallel and next_run <= args.repeat:
                sandbox = tempfile.TemporaryDirectory(prefix=f"heavy-run-{next_run}-")
                log = (args.out / f"logs/run-{next_run}.log").open("w")
                seed = args.seed + next_run - 1
                environment = dict(os.environ, PROPTEST_RNG_SEED=str(seed))
                for variable in ("PROPTEST_CASES", "KOIDRIVE_BATTERY_CASES"):
                    environment.pop(variable, None)
                if args.cases:
                    environment["KOIDRIVE_BATTERY_CASES"] = args.cases
                try:
                    command = cargo_command(args)
                    if args.mode == "worker":
                        command = worker_sandbox(args.root, Path(sandbox.name), args.crate, cargo, args.features)
                        if args.filter:
                            command += [args.filter]
                        environment["PATH"] = str(Path(sandbox.name) / "bin") + os.pathsep + environment["PATH"]
                        environment.pop("KOIDRIVE_TEST_PORT", None)
                        environment["WRANGLER_SEND_METRICS"] = "false"
                        environment["XDG_CONFIG_HOME"] = str(Path(sandbox.name) / "config")
                    process = subprocess.Popen(command, cwd=args.root, env=environment,
                                               stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
                except Exception:
                    log.close()
                    sandbox.cleanup()
                    raise
                active.append(Run(next_run, seed, process, time.monotonic(), log, sandbox))
                print(f"Started run {next_run}/{args.repeat}, seed {seed}, PID {process.pid}", flush=True)
                next_run += 1
            for run in active[:]:
                elapsed = time.monotonic() - run.started
                timed_out = elapsed >= args.timeout_seconds and run.process.poll() is None
                if timed_out:
                    terminate(run.process)
                if run.process.poll() is None:
                    continue
                run.log.close()
                text = (args.out / f"logs/run-{run.number}.log").read_text(errors="replace")
                record = result(run.number, run.seed, run.process.returncode, text, elapsed, timed_out)
                results.append(record)
                (args.out / f"run-{run.number}.json").write_text(json.dumps(record, indent=2) + "\n")
                print(f"Run {run.number}: {record['result']}, exit {record['exit_code']}, "
                      f"{record['passed']} passed, {record['failed']} failed", flush=True)
                if record["result"] != "pass":
                    print("\n".join(text.splitlines()[-200:]), flush=True)
                run.sandbox.cleanup()
                active.remove(run)
            if active:
                time.sleep(0.1)
    finally:
        for run in active:
            terminate(run.process)
            run.log.close()
            run.sandbox.cleanup()
        save_regressions(args.root, args.out)
    return sorted(results, key=lambda record: record["run"])


def main(argv=None):
    args = parse_args(argv)
    records = run_tests(args)
    sha = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=args.root, text=True).strip()
    rows = [f"### {os.environ.get('RUNNER_OS', sys.platform)}: {args.mode}, {args.parallel} parallel repetitions",
            "", f"Target commit {sha}. Harness commit {os.environ.get('GITHUB_SHA', 'local')}.", "",
            "| Run | Seed | Result | Exit | Passed | Failed | Ignored | Seconds |",
            "|---|---|---|---|---|---|---|---|"]
    for record in records:
        rows.append("| " + " | ".join(str(record[key]) for key in
                    ("run", "seed", "result", "exit_code", "passed", "failed", "ignored", "seconds")) + " |")
    green = sum(record["result"] == "pass" for record in records)
    repro = [sys.executable, ".heavy-harness/scripts/stress-test-servers.py", "--root", ".", "--out", "heavy-repro",
             "--mode", args.mode, "--crate", args.crate, "--test", args.test, "--filter", args.filter,
             "--features", args.features, "--cases", args.cases, "--ignored", args.ignored,
             "--repeat", str(args.repeat), "--parallel", str(args.parallel), "--seed", str(args.seed)]
    rows += ["", f"**{green} of {args.repeat} runs passed.**", "",
             "Cargo tests and web assets are built before repetitions. Worker runs use the target commit's launcher, copied web assets, and a fresh server copy with isolated Wrangler builds and local state.",
             "Parallel Cargo runs share the target checkout. Saved regressions are collected after all runs and can affect other repetitions. Replay saved cases before relying on a seed alone.",
             "", "Keep the dispatch commit in .heavy-harness and check out the target commit at the repository root, then run:",
             "", "```sh", shlex.join(repro), "```", ""]
    summary = "\n".join(rows)
    (args.out / "summary.md").write_text(summary)
    (args.out / "results.json").write_text(json.dumps(records, indent=2) + "\n")
    print(summary)
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as stream:
            stream.write(summary)
    return 0 if green == args.repeat else 1


def interrupted(signum, frame):
    raise KeyboardInterrupt


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, interrupted)
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(130)
