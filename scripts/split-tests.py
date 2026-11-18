#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import argparse
from collections import Counter
import json
from pathlib import Path
import re
import subprocess
import tempfile


ROOT = Path(__file__).resolve().parent.parent
MAPPING = ROOT / "scripts/test-split/mapping.json"
LICENSE = b"// SPDX-License-Identifier: Apache-2.0\n\n"


def git_source(revision, path):
    return subprocess.check_output(["git", "show", f"{revision}:{path}"], cwd=ROOT)


def include_path(path, name):
    path = Path(path)
    included = path.parent / name
    if included.parent != path.parent / path.stem or included.stem == "root":
        raise ValueError(f"unexpected include path: {included}")
    return included


def read_source(revision, path):
    return git_source(revision, path) if revision else (ROOT / path).read_bytes()


def load_source(executable, path, revision=None):
    source = read_source(revision, path)

    for item in reversed(includes(executable, source)):
        included = include_path(path, item["name"].removeprefix("include:"))
        text = read_source(revision, str(included)).removeprefix(LICENSE).rstrip()
        source = source[:item["start"]] + text + source[item["end"]:]
    return source


def parser():
    manifest = ROOT / "scripts/test-split/Cargo.toml"
    subprocess.run(
        ["nice", "-n", "10", "cargo", "build", "--quiet", "--locked", "--manifest-path", str(manifest)],
        cwd=ROOT,
        check=True,
    )
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1", "--manifest-path", str(manifest)],
        cwd=ROOT,
    ))
    return Path(metadata["target_directory"]) / "debug/test-split"


def parsed(executable, source, fragment=False):
    command = [str(executable)] + (["--fragment"] if fragment else [])
    result = subprocess.run(command, input=source, capture_output=True)
    if result.returncode:
        raise ValueError(result.stderr.decode().strip())
    found = json.loads(result.stdout)
    names = [item["name"] for item in found]
    if len(names) != len(set(names)):
        raise ValueError(f"duplicate item names: {names}")
    return found


def items(executable, source, fragment=False):
    return [item for item in parsed(executable, source, fragment) if item["kind"] != "include"]


def includes(executable, source):
    return [item for item in parsed(executable, source) if item["kind"] == "include"]


def output_path(source, scenario):
    path = Path(source)
    return ROOT / path.parent / path.stem / f"{scenario}.rs"


def scenario_file(source, scenario):
    return ROOT / source if scenario == "root" else output_path(source, scenario)


def separated(header):
    header = header.rstrip()
    return header + b"\n\n" if header else b""


def render(executable, mapping, sources):
    planned = {}
    for path, assignments in mapping.items():
        source = sources[path]
        if includes(executable, source):
            raise ValueError(f"{path} is already split; use --source-ref with an unsplit revision")
        found = items(executable, source)
        unknown = [item["name"] for item in found if item["name"] not in assignments]
        if unknown:
            raise ValueError(f"assign these items in {MAPPING.relative_to(ROOT)} for {path}: {unknown}")
        scenarios = {}
        root = separated(source[:found[0]["start"]])
        previous = found[0]["start"]
        for item in found:
            scenario = assignments[item["name"]]
            if not re.fullmatch(r"[a-z][a-z0-9_]*", scenario):
                raise ValueError(f"invalid scenario: {scenario}")
            if item["kind"] == "module" and scenario != "root":
                raise ValueError("keep modules in root to preserve relative paths")
            prefix = source[previous:item["start"]].strip(b"\n")
            text = (prefix + b"\n" if prefix else b"") + source[item["start"]:item["end"]]
            scenarios.setdefault(scenario, []).append(text)
            previous = item["end"]
        tail = source[previous:].strip()
        if tail:
            raise ValueError(f"unassigned trailing text in {path}")
        for scenario in sorted(set(scenarios) - {"root"}):
            root += f'include!("{Path(path).stem}/{scenario}.rs");\n'.encode()
            planned[output_path(path, scenario)] = LICENSE + b"\n\n".join(scenarios[scenario]) + b"\n"
        if "root" in scenarios:
            root += b"\n" + b"\n\n".join(scenarios["root"]) + b"\n"
        planned[ROOT / path] = root
        print(f"{path}: {len(found)} unchanged items, {len(scenarios)} files")
    return planned


def write(executable, planned):
    obsolete = set()
    for path in planned:
        if path.exists():
            for included in includes(executable, path.read_bytes()):
                old = include_path(path, included["name"].removeprefix("include:"))
                if old not in planned:
                    if not old.read_bytes().startswith(LICENSE):
                        raise ValueError(f"not a generated scenario file: {old}")
                    obsolete.add(old)
    for path, source in planned.items():
        path.parent.mkdir(parents=True, exist_ok=True)
        if not path.exists() or path.read_bytes() != source:
            path.write_bytes(source)
    for path in obsolete:
        path.unlink()


def property_files(executable, path, revision=None):
    root = read_source(revision, path)
    files = {item["name"]: Path(path) for item in items(executable, root) if item["kind"] == "property"}
    for included in includes(executable, root):
        scenario = include_path(path, included["name"].removeprefix("include:"))
        for item in items(executable, read_source(revision, str(scenario)), fragment=True):
            if item["kind"] == "property":
                files[item["name"]] = scenario
    return files


def require_properties_in_place(path, assignments, files):
    for name, held in files.items():
        if name not in assignments:
            continue
        if held != scenario_file(path, assignments[name]).relative_to(ROOT):
            raise ValueError(f"{name} must stay in {held}: a proptest block never changes file")


def split(executable, mapping, revision):
    for path, assignments in mapping.items():
        require_properties_in_place(path, assignments, property_files(executable, path, revision))
    sources = {path: load_source(executable, path, revision) for path in mapping}
    write(executable, render(executable, mapping, sources))


def records(executable, source):
    found = items(executable, source)
    header = source[:found[0]["start"]] if found else b""
    previous = len(header)
    result = {}
    for item in found:
        prefix = source[previous:item["start"]].strip(b"\n")
        result[item["name"]] = (prefix + b"\n" if prefix else b"") + source[item["start"]:item["end"]]
        previous = item["end"]
    return header, result


def merge(base, branch, current, name):
    if branch == base:
        return current
    if current == base or branch == current:
        return branch
    if None in (base, branch, current):
        raise ValueError(f"concurrent addition or deletion of {name}; resolve it before porting")
    with tempfile.TemporaryDirectory(prefix="test-split-") as directory:
        paths = [Path(directory) / suffix for suffix in ("current", "base", "branch")]
        for path, text in zip(paths, (current, base, branch)):
            path.write_bytes(text)
        result = subprocess.run(["git", "merge-file", "-p", *map(str, paths)], capture_output=True)
    if result.returncode:
        raise ValueError(f"overlapping edits in {name}:\n{result.stdout.decode()}")
    return result.stdout


def port(executable, mapping, base_ref, branch_ref):
    sources = {}
    for path in mapping:
        base_header, base = records(executable, load_source(executable, path, base_ref))
        branch_header, branch = records(executable, load_source(executable, path, branch_ref))
        current_header, current = records(executable, load_source(executable, path))
        blocks = [{name for name in side if name.startswith("proptest:")} for side in (base, branch, current)]
        changed = sorted(set.union(*blocks) - set.intersection(*blocks))
        if changed:
            raise ValueError(f"{path}: proptest blocks {changed} differ between base, branch and current; "
                             "apply that change by hand")
        files = property_files(executable, path, branch_ref) | property_files(executable, path)
        require_properties_in_place(path, mapping[path], files)
        order = list(current) + [name for name in branch if name not in current]
        merged = []
        for name in order:
            text = merge(base.get(name), branch.get(name), current.get(name), name)
            if text is not None:
                merged.append(text)
        header = merge(base_header, branch_header, current_header, f"{path} header")
        sources[path] = separated(header) + b"\n\n".join(merged) + b"\n"
    write(executable, render(executable, mapping, sources))


def require_assigned(path, assignments, found):
    unknown = [item["name"] for item in found if item["name"] not in assignments]
    if unknown:
        raise ValueError(f"assign these items in {MAPPING.relative_to(ROOT)} for {path}: {unknown}")


def verify(executable, mapping, revision):
    total = 0
    for path, assignments in mapping.items():
        original = load_source(executable, path, revision)
        root = (ROOT / path).read_bytes()
        root_items = parsed(executable, root)
        original_items = items(executable, original)
        require_assigned(path, assignments, original_items)
        reference_files = property_files(executable, path, revision)
        require_properties_in_place(path, assignments, reference_files)
        scenarios = {assignments[item["name"]] for item in original_items} - {"root"}
        expected_includes = Counter(f'{Path(path).stem}/{scenario}.rs' for scenario in scenarios)
        actual_includes = Counter(item["name"].removeprefix("include:")
                                  for item in root_items if item["kind"] == "include")
        if actual_includes != expected_includes:
            raise ValueError(f"{path}: include list differs from the mapping")
        header = separated(original[:original_items[0]["start"]])
        if not root_items or root[:root_items[0]["start"]] != header:
            raise ValueError(f"{path}: module header changed")
        before = {item["name"]: original[item["start"]:item["end"]]
                  for item in original_items}
        after = {}
        paths = [ROOT / path] + sorted(output_path(path, "common").parent.rglob("*.rs"))
        if set(paths[1:]) != {output_path(path, scenario) for scenario in scenarios}:
            raise ValueError(f"{path}: scenario file set differs from the mapping")
        found = {moved: items(executable, moved.read_bytes(), fragment=moved != ROOT / path) for moved in paths}
        require_assigned(path, assignments, [item for moved_items in found.values() for item in moved_items])
        for moved, moved_items in found.items():
            source = moved.read_bytes()
            for item in moved_items:
                name = item["name"]
                if name in after:
                    raise ValueError(f"duplicated item in {path}: {name}")
                if name in reference_files and moved.relative_to(ROOT) != reference_files[name]:
                    raise ValueError(f"{name} must stay in {reference_files[name]}: a proptest block never changes file")
                if moved != scenario_file(path, assignments[name]):
                    raise ValueError(f"wrong scenario for {name}: {moved}")
                after[name] = source[item["start"]:item["end"]]
        if before != after:
            missing = set(before) - set(after)
            extra = set(after) - set(before)
            changed = [name for name in before.keys() & after.keys() if before[name] != after[name]]
            raise ValueError(f"{path}: missing={missing}, extra={extra}, changed={changed}")
        total += len(before)
        print(f"PASS {path}: {len(before)} items byte-identical, including attributes and function bodies")
    print(f"PASS {total} items, no additions, deletions, duplicates, or changed bodies")


def compare_lists(before, after):
    def tests(path):
        return Counter(line for line in Path(path).read_text().splitlines()
                       if line.endswith(": test") or line.endswith(": benchmark"))

    old, new = tests(before), tests(after)
    if not old or old != new:
        raise ValueError(f"test inventory differs: removed={old - new}, added={new - old}")
    print(f"PASS cargo --list: {sum(old.values())} tests before and after, identical names and multiplicities")


def main():
    cli = argparse.ArgumentParser()
    commands = cli.add_subparsers(dest="command", required=True)
    apply = commands.add_parser("split")
    apply.add_argument("--source-ref")
    check = commands.add_parser("verify")
    check.add_argument("--source-ref", required=True)
    replay = commands.add_parser("port")
    replay.add_argument("--base-ref", required=True)
    replay.add_argument("--branch-ref", required=True)
    replay.add_argument("--assign", action="append", default=[], metavar="PATH::ITEM=SCENARIO")
    listing = commands.add_parser("compare-lists")
    listing.add_argument("before")
    listing.add_argument("after")
    args = cli.parse_args()
    if args.command == "compare-lists":
        compare_lists(args.before, args.after)
        return
    executable = parser()
    mapping = json.loads(MAPPING.read_text())
    if args.command == "split":
        split(executable, mapping, args.source_ref)
    elif args.command == "port":
        for assignment in args.assign:
            key, scenario = assignment.rsplit("=", 1)
            path, name = key.split("::", 1)
            mapping[path][name] = scenario
        port(executable, mapping, args.base_ref, args.branch_ref)
        MAPPING.write_text(json.dumps(mapping, indent=2) + "\n")
    else:
        verify(executable, mapping, args.source_ref)


if __name__ == "__main__":
    main()
