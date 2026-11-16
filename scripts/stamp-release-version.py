#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import json
from pathlib import Path
import re
import sys


def stamp(root: Path, tag: str, commit: str = "") -> str:
    number = r"(?:0|[1-9][0-9]*)"
    if not re.fullmatch(rf"v{number}\.{number}\.{number}(?:-(?:alpha|rc)\.{number})?", tag):
        raise ValueError("expected vX.Y.Z, vX.Y.Z-alpha.N or vX.Y.Z-rc.N")
    if commit and not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("expected a full lowercase commit id")
    version = tag[1:]
    manifest = root / "Cargo.toml"
    cargo = manifest.read_text()
    workspace = cargo.split("[workspace]", 1)[1].split("\n[", 1)[0]
    members = re.search(r"^members\s*=\s*\[([^]]+)\]", workspace, re.M)
    if members is None:
        raise ValueError("Cargo.toml needs explicit workspace members")
    names = {
        re.search(r'^name = "([^"]+)"', (root / member / "Cargo.toml").read_text(), re.M)[1]
        for member in re.findall(r'"([^"]+)"', members[1])
    }
    lock = root / "Cargo.lock"
    sections = lock.read_text().split("[[package]]")
    found = set()
    for index, section in enumerate(sections[1:], 1):
        name = re.search(r'^name = "([^"]+)"', section, re.M)[1]
        if name in names and not re.search(r"^source\s*=", section, re.M):
            sections[index], count = re.subn(r'^version = "[^"]+"', f'version = "{version}"', section, count=1, flags=re.M)
            if count != 1:
                raise ValueError(f"Cargo.lock needs a version for {name}")
            found.add(name)
    if found != names:
        raise ValueError(f"Cargo.lock is missing workspace packages: {sorted(names - found)}")
    sections_cargo = cargo.split("[workspace.package]")
    if len(sections_cargo) != 2:
        raise ValueError("Cargo.toml needs one [workspace.package] section")
    package = sections_cargo[1].split("\n[", 1)[0]
    stamped, count = re.subn(r'^version = "[^"]+"', f'version = "{version}"', package, count=1, flags=re.M)
    if count != 1:
        raise ValueError("Cargo.toml needs a workspace version")
    sections_cargo[1] = stamped + sections_cargo[1][len(package):]
    manifest.write_text("[workspace.package]".join(sections_cargo))
    lock.write_text("[[package]]".join(sections))
    release = {"version": version, "commit": commit} if commit else {"version": version}
    (root / "server" / "release.json").write_text(json.dumps(release, indent=2) + "\n")
    return version


if __name__ == "__main__":
    if len(sys.argv) not in (2, 3):
        sys.exit("usage: stamp-release-version.py <tag> [<commit>]")
    try:
        print(stamp(Path(__file__).resolve().parent.parent, *sys.argv[1:]))
    except ValueError as error:
        sys.exit(str(error))
