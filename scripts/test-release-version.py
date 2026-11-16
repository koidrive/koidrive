#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest


def load(name, file):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(file))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


stamp = load("stamp", "stamp-release-version.py")
release = load("release", "release-commit.py")
COMMIT = "0123456789abcdef0123456789abcdef01234567"


class ReleaseVersion(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        (self.root / "cli").mkdir()
        (self.root / "server").mkdir()
        (self.root / "Cargo.toml").write_text('[workspace]\nmembers = ["cli"]\n[workspace.package]\nversion = "0.0.0-dev"\n')
        (self.root / "cli/Cargo.toml").write_text('[package]\nname = "koidrive-cli"\nversion.workspace = true\n')
        (self.root / "Cargo.lock").write_text('version = 4\n[[package]]\nname = "koidrive-cli"\nversion = "0.0.0-dev"\n[[package]]\nname = "koidrive-cli"\nversion = "0.0.0-dev"\nsource = "registry+https://example.com"\n')
        (self.root / "server/release.json").write_text('{"version":"0.0.0-dev"}\n')

    def test_release_tags_stamp_every_version_source(self):
        for tag in ["v1.2.3", "v1.2.3-alpha.2", "v1.2.3-rc.4", "v0.0.0-rc.0"]:
            with self.subTest(tag=tag):
                self.assertEqual(stamp.stamp(self.root, tag), tag[1:])
                cargo = (self.root / "Cargo.toml").read_text()
                lock = (self.root / "Cargo.lock").read_text()
                self.assertIn(f'version = "{tag[1:]}"', cargo)
                self.assertIn(f'name = "koidrive-cli"\nversion = "{tag[1:]}"', lock)
                self.assertIn('name = "koidrive-cli"\nversion = "0.0.0-dev"\nsource =', lock)
                self.assertEqual(stamp.json.loads((self.root / "server/release.json").read_text())["version"], tag[1:])
                before = [path.read_bytes() for path in [self.root / "Cargo.toml", self.root / "Cargo.lock", self.root / "server/release.json"]]
                stamp.stamp(self.root, tag)
                self.assertEqual([path.read_bytes() for path in [self.root / "Cargo.toml", self.root / "Cargo.lock", self.root / "server/release.json"]], before)

    def test_a_commit_is_stamped_next_to_the_server_version(self):
        stamp.stamp(self.root, "v1.2.3", COMMIT)
        self.assertEqual(json.loads((self.root / "server/release.json").read_text()), {"version": "1.2.3", "commit": COMMIT})
        stamp.stamp(self.root, "v1.2.4")
        self.assertEqual(json.loads((self.root / "server/release.json").read_text()), {"version": "1.2.4"})

    def test_an_abbreviated_commit_does_not_change_manifests(self):
        paths = [self.root / name for name in ["Cargo.toml", "Cargo.lock", "server/release.json"]]
        before = [path.read_bytes() for path in paths]
        for commit in [COMMIT[:12], COMMIT.upper(), COMMIT + "\n"]:
            with self.subTest(commit=commit), self.assertRaises(ValueError):
                stamp.stamp(self.root, "v1.2.3", commit)
            self.assertEqual([path.read_bytes() for path in paths], before)

    def test_invalid_tags_do_not_change_manifests(self):
        paths = [self.root / name for name in ["Cargo.toml", "Cargo.lock", "server/release.json"]]
        before = [path.read_bytes() for path in paths]
        for tag in ["1.2.3", "v01.2.3", "v1.2.3-alpha.01", "v1.2.3-beta.1", "v1.2.3+build", "v1.2.3\ninjected"]:
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                stamp.stamp(self.root, tag)
            self.assertEqual([path.read_bytes() for path in paths], before)

    def test_missing_workspace_lock_entry_stops_before_writing(self):
        (self.root / "Cargo.lock").write_text('version = 4\n')
        before = (self.root / "Cargo.toml").read_bytes()
        with self.assertRaisesRegex(ValueError, "missing workspace packages"):
            stamp.stamp(self.root, "v1.2.3")
        self.assertEqual((self.root / "Cargo.toml").read_bytes(), before)


class ReleaseCommit(unittest.TestCase):
    def setUp(self):
        ReleaseVersion.setUp(self)
        self.git("init", "--quiet", "--initial-branch=main")
        self.git("config", "user.name", "Release test")
        self.git("config", "user.email", "release@example.com")
        self.git("config", "commit.gpgsign", "false")
        self.git("config", "tag.gpgsign", "false")
        self.git("add", "--all")
        self.git("commit", "--quiet", "--message", "main")
        self.main = self.git("rev-parse", "HEAD")
        self.git("update-ref", "refs/remotes/origin/main", self.main)

    def git(self, *args):
        return subprocess.run(["git", "-C", str(self.root), *args], check=True, capture_output=True, text=True).stdout.strip()

    def show(self, revision, path):
        return self.git("show", f"{revision}:{path}")

    def by_hand(self, tag, change=lambda root: None, commit=None):
        self.git("switch", "--quiet", "--detach", self.main)
        stamp.stamp(self.root, tag, self.main if commit is None else commit)
        change(self.root)
        self.git("add", "--all")
        self.git("commit", "--quiet", "--message", "by hand")
        self.git("tag", tag)
        self.git("switch", "--quiet", "main")

    def test_make_tags_a_release_commit_on_main_that_check_accepts(self):
        made = release.make(self.root, "v1.2.3")
        self.assertEqual(self.git("rev-parse", "v1.2.3^{commit}"), made)
        self.assertEqual(self.git("rev-parse", f"{made}^"), self.main)
        self.assertEqual(json.loads(self.show("v1.2.3", "server/release.json")), {"version": "1.2.3", "commit": self.main})
        self.assertIn('version = "1.2.3"', self.show("v1.2.3", "Cargo.toml"))
        self.assertIn('name = "koidrive-cli"\nversion = "1.2.3"', self.show("v1.2.3", "Cargo.lock"))
        message = self.git("log", "-1", "--format=%B", made)
        self.assertTrue(message.startswith("chore(release): v1.2.3"), message)
        self.assertIn("Signed-off-by: Release test <release@example.com>", message)
        self.assertEqual(release.check(self.root, "v1.2.3"), self.main)
        self.assertEqual(self.git("rev-parse", "HEAD"), self.main)
        self.assertEqual(self.git("status", "--porcelain"), "")
        self.assertEqual(len(self.git("worktree", "list").splitlines()), 1)

    def test_make_refuses_an_existing_tag_and_a_commit_off_main(self):
        release.make(self.root, "v1.2.3")
        with self.assertRaisesRegex(ValueError, "already exists"):
            release.make(self.root, "v1.2.3")
        self.git("switch", "--quiet", "--detach")
        (self.root / "branch").write_text("off main\n")
        self.git("add", "--all")
        self.git("commit", "--quiet", "--message", "off main")
        with self.assertRaisesRegex(ValueError, "is not on origin/main"):
            release.make(self.root, "v1.2.4", "HEAD")
        self.assertEqual(self.git("tag", "--list", "v1.2.4"), "")

    def test_check_refuses_a_tag_on_main(self):
        self.git("tag", "v1.2.3", self.main)
        with self.assertRaisesRegex(ValueError, "points to the release commit"):
            release.check(self.root, "v1.2.3")

    def test_check_refuses_a_release_commit_that_changes_more_than_the_version(self):
        self.by_hand("v1.2.3", lambda root: (root / "cli/src.rs").write_text("unreviewed\n"))
        with self.assertRaisesRegex(ValueError, "cli/src.rs"):
            release.check(self.root, "v1.2.3")

    def test_check_refuses_a_release_commit_with_another_version_or_commit(self):
        self.by_hand("v1.2.3", commit=COMMIT)
        with self.assertRaisesRegex(ValueError, "server/release.json"):
            release.check(self.root, "v1.2.3")
        self.by_hand("v1.2.4")
        self.git("tag", "--force", "v1.2.5", "v1.2.4")
        with self.assertRaisesRegex(ValueError, "Cargo.toml"):
            release.check(self.root, "v1.2.5")

    def test_check_refuses_a_release_commit_whose_parent_is_off_main(self):
        self.git("switch", "--quiet", "--detach")
        (self.root / "branch").write_text("off main\n")
        self.git("add", "--all")
        self.git("commit", "--quiet", "--message", "off main")
        self.git("update-ref", "refs/remotes/origin/side", "HEAD")
        self.git("switch", "--quiet", "main")
        release.make(self.root, "v1.2.3", "origin/side", "origin/side")
        with self.assertRaisesRegex(ValueError, "whose parent is not one commit on origin/main"):
            release.check(self.root, "v1.2.3")


if __name__ == "__main__":
    unittest.main(verbosity=2)
