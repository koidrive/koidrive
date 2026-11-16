#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile

spec = importlib.util.spec_from_file_location("stamp", Path(__file__).with_name("stamp-release-version.py"))
stamp = importlib.util.module_from_spec(spec)
spec.loader.exec_module(stamp)

USAGE = "usage: release-commit.py make <tag> [<main commit>] | release-commit.py check <tag>"


def git(repo: Path, *args: str) -> str:
    run = subprocess.run(["git", "-C", str(repo), *args], capture_output=True, text=True)
    if run.returncode != 0:
        raise ValueError(f"git {' '.join(args)}: {run.stderr.strip()}")
    return run.stdout.strip()


def on(repo: Path, commit: str, main: str) -> bool:
    return subprocess.run(["git", "-C", str(repo), "merge-base", "--is-ancestor", commit, main]).returncode == 0


def stamped(repo: Path, tag: str, base: str, then) -> str:
    with tempfile.TemporaryDirectory() as work:
        tree = Path(work) / "tree"
        git(repo, "worktree", "add", "--quiet", "--detach", str(tree), base)
        try:
            stamp.stamp(tree, tag, base)
            return then(tree)
        finally:
            git(repo, "worktree", "remove", "--force", str(tree))


def make(repo: Path, tag: str, ref: str = "origin/main", main: str = "origin/main") -> str:
    base = git(repo, "rev-parse", "--verify", "--end-of-options", ref + "^{commit}")
    if not on(repo, base, main):
        raise ValueError(f"{ref} is not on {main}")
    if git(repo, "tag", "--list", tag):
        raise ValueError(f"{tag} already exists")

    def commit(tree: Path) -> str:
        git(tree, "commit", "--quiet", "--all", "--signoff", "--message", f"chore(release): {tag}")
        return git(tree, "rev-parse", "HEAD")

    release = stamped(repo, tag, base, commit)
    git(repo, "tag", tag, release)
    return release


def check(repo: Path, tag: str, main: str = "origin/main") -> str:
    release = git(repo, "rev-parse", "--verify", "--end-of-options", tag + "^{commit}")
    if on(repo, release, main):
        raise ValueError(f"{tag} points to {release} on {main}; a release tag points to the release commit that release-commit.py make creates")
    parents = git(repo, "rev-list", "--parents", "--max-count=1", release).split()[1:]
    if len(parents) != 1 or not on(repo, parents[0], main):
        raise ValueError(f"{tag} points to {release}, whose parent is not one commit on {main}")
    diff = stamped(repo, tag, parents[0], lambda tree: git(tree, "diff", "--stat", "--patch", release))
    if diff:
        raise ValueError(f"{tag} differs from {parents[0]} stamped with its version:\n{diff}")
    return parents[0]


if __name__ == "__main__":
    repo = Path(__file__).resolve().parent.parent
    command, arguments = (sys.argv[1], sys.argv[2:]) if len(sys.argv) > 1 else ("", [])
    try:
        if command == "make" and len(arguments) in (1, 2):
            release = make(repo, *arguments)
            print(f"Tagged {arguments[0]} at the release commit {release}. Push the tag with: git push origin {arguments[0]}")
        elif command == "check" and len(arguments) == 1:
            print(f"{arguments[0]} is {check(repo, arguments[0])} with its release version stamped.")
        else:
            sys.exit(USAGE)
    except ValueError as error:
        sys.exit(str(error))
