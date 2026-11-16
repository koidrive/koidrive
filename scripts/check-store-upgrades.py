#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import argparse
import hashlib
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
CASES = "core/store/tests/upgrade_cases/"
HARNESS = "core/store/tests/upgrade.rs"
SCHEMA = {"core/store/build.rs"}
NAME = r"[a-z][a-z0-9_]*"
FIXTURE_SHA256 = "37482c95055651b30460f9a6e56fb85ed2453aa6b717d9f0a60445a821a17b66"


def check(changes, harness, case_files, read_case, previous_harness="", migration_exists=lambda name: False):
    additions = {path for status, path in changes if status == "A"}
    modified = [path for status, path in changes if status != "A" and path.startswith("core/store/migrations/")]
    if modified:
        raise ValueError("do not change or remove merged migrations: " + ", ".join(modified))
    unrecognized = sorted(
        path for path in additions
        if path.startswith("core/store/migrations/")
        and not re.fullmatch(r"core/store/migrations/" + NAME + r"\.sql", path)
    )
    if unrecognized:
        raise ValueError("add a named SQL migration outside legacy/: " + ", ".join(unrecognized))
    migrations = {
        pathlib.PurePosixPath(path).stem
        for path in additions
        if re.fullmatch(r"core/store/migrations/" + NAME + r"\.sql", path)
    }
    cases = {
        pathlib.PurePosixPath(path).stem
        for path in additions
        if re.fullmatch(CASES + NAME + r"\.rs", path)
    }
    missing = migrations - cases
    if missing:
        raise ValueError("add one upgrade case file per migration: " + ", ".join(sorted(missing)))
    if any(path in SCHEMA or path.startswith(("core/store/build/", "core/store/src/")) for _, path in changes) and not migrations and not any(
        case.startswith("schema_") for case in cases
    ):
        raise ValueError("add a new schema_ upgrade case for the changed store schema code")
    included = re.findall(r'^include!\("upgrade_cases/(' + NAME + r')\.rs"\);$', harness, re.M)
    if len(included) != len(set(included)):
        raise ValueError("duplicate upgrade case includes")
    previous = re.findall(r'^include!\("upgrade_cases/(' + NAME + r')\.rs"\);$', previous_harness, re.M)
    removed = sorted(name for name in previous if name not in included and migration_exists(name))
    if removed:
        raise ValueError("cannot remove upgrade coverage for migrations that remain: " + ", ".join(removed))
    if set(case_files) != set(included):
        raise ValueError(f"include each case file from {HARNESS}; files={sorted(case_files)}, includes={sorted(included)}")
    for name in included:
        text = read_case(name)
        pattern = r'\s*(?://[^\n]*\n\s*)*upgrade_case!\(\s*' + re.escape(name) + r'\s*,\s*\|(' + NAME + r')\|\s*\{(.+)\}\s*\);\s*'
        match = re.fullmatch(pattern, text, re.S)
        if not match:
            raise ValueError(f"{CASES}{name}.rs must contain exactly upgrade_case!({name}, |store| {{ ... }});")
        body = re.sub(r'//[^\n]*|/\*.*?\*/', '', match.group(2), flags=re.S)
        if not re.search(r'\b' + re.escape(match.group(1)) + r'\b', body) or not re.search(r'\bassert(?:_eq|_ne)?!', body):
            raise ValueError(f"{CASES}{name}.rs must use the upgraded store and assert its behavior")
    return sorted(case_files)


def git(*args):
    return subprocess.check_output(["git", *args], cwd=ROOT)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("base")
    parser.add_argument("head")
    args = parser.parse_args()
    fixture = ROOT / "core/store/tests/fixtures/alpha6.db"
    if hashlib.sha256(fixture.read_bytes()).hexdigest() != FIXTURE_SHA256:
        raise ValueError("alpha6 fixture does not match the pinned release fixture")
    raw = git("diff", "--no-renames", "--name-status", "-z", args.base, args.head).decode().split("\0")
    changes = list(zip(raw[0:-1:2], raw[1:-1:2]))
    prior = subprocess.run(["git", "cat-file", "-e", f"{args.base}:{HARNESS}"], cwd=ROOT, capture_output=True)
    previous_harness = git("show", f"{args.base}:{HARNESS}").decode() if prior.returncode == 0 else ""
    case_paths = list((ROOT / CASES).glob("*.rs"))
    if any(not re.fullmatch(NAME, path.stem) for path in case_paths):
        raise ValueError("upgrade case filenames must be lowercase identifiers")
    names = check(changes, (ROOT / HARNESS).read_text(), [path.stem for path in case_paths],
                  lambda name: (ROOT / CASES / f"{name}.rs").read_text(), previous_harness,
                  lambda name: (ROOT / "core/store/migrations" / f"{name}.sql").is_file())
    for name in ["schema_alpha6_baseline", *names]:
        result = subprocess.run(
            ["nice", "-n", "10", "cargo", "test", "--locked", "-p", "koidrive-store", "--test", "upgrade", "--", "--exact", name, "--nocapture"],
            cwd=ROOT, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=900,
        )
        print(result.stdout, flush=True)
        if result.returncode or not re.search(r"test result: ok\. 1 passed; 0 failed; 0 ignored;", result.stdout):
            raise ValueError(f"upgrade case {name} did not run exactly once and pass")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
        print(f"store upgrade guard: {error}", file=sys.stderr)
        sys.exit(1)
