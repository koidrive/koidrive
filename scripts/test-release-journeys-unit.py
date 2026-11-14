#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import argparse
import copy
import errno
import importlib.util
import json
import os
from pathlib import Path
import plistlib
import re
import shlex
import signal
import subprocess
import sys
import tempfile
import textwrap
import unittest
from unittest.mock import patch


sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location("release_journeys", Path(__file__).with_name("test-release-journeys.py"))
DRIVER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(DRIVER)


class ReleaseJourneys(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="release-unit-", dir=DRIVER.REPO)
        self.root = Path(self.temporary.name)
        self.addCleanup(self.temporary.cleanup)

    def evidence(self, scenario="verify"):
        return DRIVER.Evidence(self.root / "evidence", "v0.1.0-alpha.3", scenario)

    def passing_report(self, scenario):
        return {"scenario": scenario, "assertions": [{"name": name, "passed": True}
                for name in DRIVER.REQUIRED[scenario]], "cleanup": [{"name": "owned state", "passed": True}], "errors": []}

    def zap_spec(self):
        return {"artifacts": [{"zap": [{"script": {
            "executable": str(DRIVER.APP / "Contents/MacOS/Koidrive"),
            "args": ["--unregister-login-item"], "sudo": False},
            "trash": ["~/Library/Application Support/Koidrive", "~/Library/Preferences/dev.koidrive.Koidrive.plist"]}]}]}

    def service_plist(self, label, state=None, binary=None):
        path = self.root / "Library/LaunchAgents" / (label + ".plist")
        path.parent.mkdir(parents=True, exist_ok=True)
        binary = self.root / ".local/bin/koi" if binary is None else binary
        state = str(self.root / ".local/state") if state is None else state
        path.write_bytes(plistlib.dumps({"Label": label, "ProgramArguments": [str(binary), "_daemon"],
                                         "EnvironmentVariables": {"XDG_STATE_HOME": state}}))
        return path

    def stubborn_group(self, leader_exits):
        evidence = self.evidence()
        ready = self.root / "child.json"
        child_code = "\n".join([
            "import json,os,pathlib,signal,time",
            "signal.signal(signal.SIGTERM,signal.SIG_IGN)",
            "pathlib.Path(" + repr(str(ready)) + ").write_text(json.dumps({'pid':os.getpid(),'pgid':os.getpgrp(),'sid':os.getsid(0)}))",
            "while True: time.sleep(1)",
        ])
        leader_code = "\n".join([
            "import os,pathlib,subprocess,sys,time",
            "subprocess.Popen([sys.executable,'-c'," + repr(child_code) + "],stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)",
            "while not pathlib.Path(" + repr(str(ready)) + ").exists(): time.sleep(0.01)",
            "os._exit(0)" if leader_exits else "time.sleep(60)",
        ])
        item = evidence.start([sys.executable, "-c", leader_code])
        process = item[0]

        def cleanup():
            if not process.group_closed:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            process.wait(timeout=3)
            for capture in item[2:]:
                capture.thread.join(timeout=3)
            self.assertTrue(DRIVER.until(lambda: not self.group_exists(process.pid), 3), "Owned regression group survived cleanup")

        self.addCleanup(cleanup)
        self.assertTrue(DRIVER.until(lambda: ready.exists(), 3), "Regression child did not start")
        child = json.loads(ready.read_text())
        self.assertEqual(child["pgid"], process.pid)
        self.assertEqual(child["sid"], process.pid)
        self.assertNotEqual(process.pid, os.getpgrp())
        if leader_exits:
            process.wait(timeout=3)
        return evidence, item, child

    def group_exists(self, pgid):
        try:
            os.killpg(pgid, 0)
        except ProcessLookupError:
            return False
        except PermissionError:
            return True
        return True

    def census_process(self):
        process = unittest.mock.Mock(pid=12345)
        process.group_uid = 501
        process.group_closed = False
        process.group_observations = []
        return process

    def test_strict_tags(self):
        for tag in ("v0.1.0-alpha.3", "v1.2.3", "v10.20.30-rc.0"):
            with self.subTest(tag=tag):
                self.assertEqual(DRIVER.tag_value(tag), tag)
        for tag in ("0.1.0", "v01.2.3", "v1.2.3-alpha.01", "v1.2.3-beta.1", "v1.2.3\n",
                    "v1.2.3/../../main", "v1.2.3;touch nope", "v1.2.3+meta", "v1.2.3-rc.-1"):
            with self.subTest(tag=tag), self.assertRaises(argparse.ArgumentTypeError):
                DRIVER.tag_value(tag)

    def test_fixed_installer_sources_share_the_accepted_digest_and_held_default(self):
        payload = b"set -eu\n"
        digest = DRIVER.hashlib.sha256(payload).hexdigest()
        self.assertEqual(DRIVER.INSTALLER_SOURCES, {"held": DRIVER.HELD_URL, "live": "https://koidrive.com/install.sh"})
        for selected in (None, "held", "live"):
            evidence = self.evidence()
            source = selected or "held"
            with self.subTest(source=selected), patch.object(DRIVER, "HELD_SHA", digest), \
                    patch.object(DRIVER, "download", side_effect=lambda _, url, path: path.write_bytes(payload)) as download:
                script = DRIVER.verified_installer(evidence, self.root, *([] if selected is None else [selected]))
            download.assert_called_once_with(evidence, DRIVER.INSTALLER_SOURCES[source], self.root / "install.sh")
            self.assertEqual(script.read_bytes(), payload)
            self.assertEqual(evidence.report["inputs"]["held-installer"]["source"], source)
            self.assertEqual(evidence.report["inputs"]["held-installer"]["sha256"], digest)
            evidence.directory.rename(self.root / ("evidence-" + str(selected)))

    def test_both_installer_sources_refuse_changed_bytes_before_execution(self):
        for source in DRIVER.INSTALLER_SOURCES:
            evidence = self.evidence()
            with self.subTest(source=source), patch.object(DRIVER, "download", side_effect=lambda _, url, path: path.write_bytes(b"changed")), \
                    patch.object(evidence, "run") as run:
                with self.assertRaises(RuntimeError):
                    DRIVER.verified_installer(evidence, self.root, source)
            run.assert_not_called()
            self.assertFalse(evidence.report["assertions"][-1]["passed"])
            evidence.directory.rename(self.root / ("evidence-" + source))

    def test_installer_source_rejects_arbitrary_urls_before_creating_evidence(self):
        for source in ("https://example.com/install.sh", "file:///tmp/install.sh", "LIVE", "held;exit 0"):
            with self.subTest(source=source), patch.object(DRIVER, "Evidence") as evidence, patch("sys.stderr"):
                with self.assertRaises(SystemExit) as error:
                    DRIVER.main(["--tag", "v0.1.0-alpha.3", "--scenario", "installer", "--evidence", str(self.root), "--installer-source", source])
            self.assertEqual(error.exception.code, 2)
            evidence.assert_not_called()

    def test_installer_source_is_wired_through_both_dispatch_routes(self):
        installer = (DRIVER.REPO / ".github/workflows/installer.yml").read_text()
        journeys = (DRIVER.REPO / ".github/workflows/release-journeys.yml").read_text()
        self.assertIn("installer_source: ${{ inputs.installer_source }}", installer)
        for workflow in (installer, journeys):
            self.assertRegex(workflow, r"(?s)installer_source:\n.*?default: held\n.*?options: \[held, live\]")
        self.assertIn("INSTALLER_SOURCE: ${{ inputs.installer_source || 'live' }}", journeys)
        self.assertIn('--installer-source "$INSTALLER_SOURCE"', journeys)

    def release_downloads(self, live, mains=None):
        live = iter(live)
        mains = mains or {"c" * 40: b"main bytes"}

        def write(_, url, path):
            raw = re.fullmatch(r"https://raw\.githubusercontent\.com/koidrive/koidrive/([0-9a-f]{40})/install\.sh", url)
            path.write_bytes(mains[raw[1]] if raw else next(live))
        return write

    def test_release_installer_uses_live_bytes_matching_the_release_checkout(self):
        payload = b"release installer bytes\n"
        (self.root / "install.sh").write_bytes(payload)
        work = self.root / "download"
        work.mkdir()
        commit = "a" * 40
        tag_ref = "refs/tags/v0.1.0-alpha.3"
        for index, listing in enumerate((commit + "\t" + tag_ref + "\n",
                                        "b" * 40 + "\t" + tag_ref + "\n" + commit + "\t" + tag_ref + "^{}\n")):
            evidence = DRIVER.Evidence(self.root / str(index), "v0.1.0-alpha.3", "installer")
            with patch.object(DRIVER, "REPO", self.root), \
                    patch.object(evidence, "run", side_effect=[(0, commit + "\n", ""), (0, listing, "")]) as run, \
                    patch.object(DRIVER, "download", side_effect=self.release_downloads([payload])) as download:
                DRIVER.verified_installer(evidence, work, "live", "release")
            download.assert_called_once_with(evidence, "https://koidrive.com/install.sh", work / "install.sh")
            self.assertEqual(run.call_args_list, [unittest.mock.call(["git", "rev-parse", "HEAD"]),
                unittest.mock.call(["git", "ls-remote", "--exit-code", "https://github.com/koidrive/koidrive.git", tag_ref, tag_ref + "^{}"])])
            record = evidence.report["inputs"]["held-installer"]
            self.assertEqual(record["reference"], "release")
            self.assertEqual(record["commit"], commit)
            self.assertEqual(record["expected_sha256"], DRIVER.hashlib.sha256(payload).hexdigest())
            self.assertNotEqual(record["expected_sha256"], DRIVER.HELD_SHA)

    def test_release_installer_accepts_live_bytes_that_main_deployed_after_the_tag(self):
        (self.root / "install.sh").write_bytes(b"bytes at the tag")
        work = self.root / "download"
        work.mkdir()
        commit, main = "a" * 40, "c" * 40
        evidence = self.evidence()
        with patch.object(DRIVER, "REPO", self.root), \
                patch.object(evidence, "run", side_effect=[(0, commit, ""), (0, commit + "\trefs/tags/v0.1.0-alpha.3\n", ""),
                                                           (0, main + "\trefs/heads/main\n", "")]), \
                patch.object(DRIVER, "download", side_effect=self.release_downloads([b"bytes on main"], {main: b"bytes on main"})) as download, \
                patch.object(DRIVER.time, "sleep") as sleep:
            DRIVER.verified_installer(evidence, work, "live", "release")
        sleep.assert_not_called()
        self.assertEqual(download.call_args_list, [
            unittest.mock.call(evidence, "https://koidrive.com/install.sh", work / "install.sh"),
            unittest.mock.call(evidence, "https://raw.githubusercontent.com/koidrive/koidrive/" + main + "/install.sh", work / "install-main.sh")])
        record = evidence.report["inputs"]["held-installer"]
        self.assertEqual(record["commit"], main)
        self.assertEqual(record["expected_sha256"], DRIVER.hashlib.sha256(b"bytes on main").hexdigest())
        self.assertEqual(set(record["accepted"]), {commit, main})

    def test_release_installer_waits_for_a_deploy_of_main_in_progress(self):
        (self.root / "install.sh").write_bytes(b"bytes at the tag")
        work = self.root / "download"
        work.mkdir()
        commit, main = "a" * 40, "c" * 40
        evidence = self.evidence()
        main_listing = (0, main + "\trefs/heads/main\n", "")
        with patch.object(DRIVER, "REPO", self.root), \
                patch.object(evidence, "run", side_effect=[(0, commit, ""), (0, commit + "\trefs/tags/v0.1.0-alpha.3\n", ""), main_listing, main_listing]), \
                patch.object(DRIVER, "download", side_effect=self.release_downloads([b"previous deploy", b"bytes on main"], {main: b"bytes on main"})) as download, \
                patch.object(DRIVER.time, "sleep") as sleep:
            DRIVER.verified_installer(evidence, work, "live", "release")
        sleep.assert_called_once_with(15)
        self.assertEqual(download.call_count, 3)
        self.assertEqual(evidence.report["inputs"]["held-installer"]["commit"], main)

    def test_release_installer_accepts_only_the_current_head_of_main(self):
        (self.root / "install.sh").write_bytes(b"bytes at the tag")
        work = self.root / "download"
        work.mkdir()
        commit, earlier, later = "a" * 40, "c" * 40, "d" * 40
        evidence = self.evidence()
        listing = lambda main: (0, main + "\trefs/heads/main\n", "")
        with patch.object(DRIVER, "REPO", self.root), \
                patch.object(evidence, "run", side_effect=[(0, commit, ""), (0, commit + "\trefs/tags/v0.1.0-alpha.3\n", ""),
                                                           listing(earlier), listing(later), listing(later)]), \
                patch.object(DRIVER, "download", side_effect=self.release_downloads(
                    [b"older deploy", b"earlier main", b"later main"], {earlier: b"earlier main", later: b"later main"})), \
                patch.object(DRIVER.time, "sleep") as sleep:
            DRIVER.verified_installer(evidence, work, "live", "release")
        self.assertEqual(sleep.call_count, 2)
        record = evidence.report["inputs"]["held-installer"]
        self.assertEqual(record["commit"], later)
        self.assertEqual(set(record["accepted"]), {commit, later})

    def test_release_installer_rejects_unbound_or_malformed_release_refs(self):
        commit = "a" * 40
        tag_ref = "refs/tags/v0.1.0-alpha.3"
        cases = (("b" * 40, commit + "\t" + tag_ref + "\n"),
                 (commit, ""), (commit, "invalid\t" + tag_ref + "\n"),
                 (commit, commit + "\trefs/heads/main\n"),
                 (commit, (commit + "\t" + tag_ref + "\n") * 2),
                 (commit, commit + "\t" + tag_ref + "^{}\n"))
        for index, (head, listing) in enumerate(cases):
            evidence = DRIVER.Evidence(self.root / str(index), "v0.1.0-alpha.3", "installer")
            with self.subTest(head=head, listing=listing), patch.object(evidence, "run", side_effect=[(0, head, ""), (0, listing, "")]), \
                    patch.object(DRIVER, "download") as download:
                with self.assertRaises(RuntimeError):
                    DRIVER.verified_installer(evidence, self.root, "live", "release")
            download.assert_not_called()

    def test_automatic_release_cannot_select_the_held_script(self):
        evidence = self.evidence()
        with patch.object(evidence, "run") as run, patch.object(DRIVER, "download") as download:
            with self.assertRaisesRegex(RuntimeError, "live release installer"):
                DRIVER.verified_installer(evidence, self.root, "held", "release")
        run.assert_not_called()
        download.assert_not_called()

    def test_release_installer_rejects_live_bytes_different_from_release_checkout(self):
        (self.root / "install.sh").write_bytes(b"release bytes")
        work = self.root / "download"
        work.mkdir()
        commit, main = "a" * 40, "c" * 40
        evidence = self.evidence()
        with patch.object(DRIVER, "REPO", self.root), \
                patch.object(evidence, "run", side_effect=[(0, commit, ""), (0, commit + "\trefs/tags/v0.1.0-alpha.3\n", ""),
                                                           (0, main + "\trefs/heads/main\n", "")]), \
                patch.object(DRIVER, "download", side_effect=self.release_downloads([b"different live bytes"])), \
                patch.object(DRIVER.time, "sleep") as sleep:
            with self.assertRaisesRegex(RuntimeError, "held script verified: downloaded SHA-256 [0-9a-f]{64} matches install.sh at none of " + commit + " .*, " + main):
                DRIVER.verified_installer(evidence, work, "live", "release", wait=0)
        sleep.assert_not_called()
        self.assertFalse(evidence.report["assertions"][-1]["passed"])

    def test_installer_reference_rejects_arbitrary_inputs(self):
        for reference in ("/tmp/install.sh", "https://example.com/install.sh", DRIVER.HELD_SHA):
            with self.subTest(reference=reference), patch.object(DRIVER, "Evidence") as evidence, patch("sys.stderr"):
                with self.assertRaises(SystemExit) as error:
                    DRIVER.main(["--tag", "v0.1.0-alpha.3", "--scenario", "installer", "--evidence", str(self.root), "--installer-reference", reference])
            self.assertEqual(error.exception.code, 2)
            evidence.assert_not_called()

    def test_release_and_distribution_routes_resolve_to_live_with_release_reference(self):
        journeys = (DRIVER.REPO / ".github/workflows/release-journeys.yml").read_text()
        release = (DRIVER.REPO / ".github/workflows/release-cli.yml").read_text()
        reusable = journeys.split("  workflow_call:\n", 1)[1].split("  pull_request:\n", 1)[0]
        self.assertRegex(reusable, r"(?s)installer_source:\n.*?default: live")
        self.assertIn("INSTALLER_SOURCE: ${{ inputs.installer_source || 'live' }}", journeys)
        self.assertIn("INSTALLER_REFERENCE: ${{ github.event_name == 'workflow_dispatch' && 'accepted' || 'release' }}", journeys)
        self.assertIn('--installer-reference "$INSTALLER_REFERENCE"', journeys)
        self.assertRegex(release, r"(?s)  release-journeys:\n.*?installer_source: live")

    def test_release_order_ranks_alphas_then_candidates_then_the_release(self):
        ordered = ["0.1.0-alpha.4", "0.1.0-alpha.10", "0.1.0-rc.1", "0.1.0", "0.1.1-alpha.1", "0.2.0"]
        self.assertEqual(sorted(reversed(ordered), key=DRIVER.release_order), ordered)
        for version in ("0.1", "v0.1.0", "0.1.0-beta.1", "0.1.0-alpha.01"):
            with self.subTest(version=version), self.assertRaises(ValueError):
                DRIVER.release_order(version)

    def test_previous_tap_release_is_the_newest_older_release_below_the_tag(self):
        log = "\n".join(sha * 40 + " " + subject for sha, subject in (
            ("e", "koidrive 0.1.0-alpha.6"), ("d", "koidrive 0.1.0-alpha.5"), ("c", "Fix the cask caveats"),
            ("b", "koidrive 0.1.0-alpha.4"), ("a", "Initial commit"))) + "\n"
        self.assertEqual(DRIVER.previous_tap_release(log, "0.1.0-alpha.5"), ("b" * 40, "0.1.0-alpha.4"))
        self.assertEqual(DRIVER.previous_tap_release(log, "0.1.0-alpha.6"), ("d" * 40, "0.1.0-alpha.5"))
        for current in ("0.1.0-alpha.4", "0.1.0-alpha.7"):
            with self.subTest(current=current), self.assertRaises(RuntimeError):
                DRIVER.previous_tap_release(log, current)

    def test_homebrew_update_runs_with_the_worker_and_in_the_full_matrix(self):
        journeys = (DRIVER.REPO / ".github/workflows/release-journeys.yml").read_text()
        self.assertEqual(journeys.count("if: matrix.scenario == 'installer' || matrix.scenario == 'homebrew-update'"), 3)
        self.assertLessEqual(DRIVER.release_order("0.1.0-alpha.5"), DRIVER.release_order(DRIVER.HOMEBREW_UPDATE_SINCE))

    def test_checksums_allow_standard_text_and_binary_markers(self):
        result = DRIVER.checksums("a" * 64 + "  koi.tar.gz\n" + "b" * 64 + " *Koidrive.dmg\n")
        self.assertEqual(result, {"koi.tar.gz": "a" * 64, "Koidrive.dmg": "b" * 64})

    def test_checksums_reject_duplicates_and_ambiguous_names(self):
        valid = "a" * 64 + "  koi.tar.gz\n"
        for text in ("", valid + valid, valid + "b" * 64 + "  koi.tar.gz\n", "a" * 63 + "  koi.tar.gz",
                     "A" * 64 + "  koi.tar.gz", "a" * 64 + "  ../koi", "a" * 64 + "  /koi",
                     "a" * 64 + "  koi extra", valid + "\n", "a" * 64 + "\tkoi.tar.gz"):
            with self.subTest(text=text), self.assertRaises(ValueError):
                DRIVER.checksums(text)

    def test_checksum_mismatch_cannot_reach_execution(self):
        evidence = self.evidence()

        def fake_download(_, url, path):
            path.write_text("a" * 64 + "  koidrive-aarch64-apple-darwin.tar.gz\n" if url.endswith("SHA256SUMS") else "wrong bytes")

        with patch.object(DRIVER, "download", side_effect=fake_download), patch.object(evidence, "run") as run:
            with self.assertRaisesRegex(RuntimeError, "checksum"):
                DRIVER.release_assets(evidence, self.root)
            run.assert_not_called()
        self.assertFalse(DRIVER.passed(evidence.report))

    def test_each_hosted_guard_condition_is_required(self):
        valid = {"env": {"GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "github-hosted"},
                 "system": "Darwin", "machine": "arm64", "home": self.root, "account_home": self.root}
        DRIVER.hosted_guard(**valid)
        for change in ({"env": {}}, {"env": {"GITHUB_ACTIONS": "true"}},
                       {"env": {"GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "self-hosted"}},
                       {"system": "Linux"}, {"machine": "x86_64"}, {"home": self.root / "fake-home"}):
            with self.subTest(change=change), self.assertRaises(RuntimeError):
                DRIVER.hosted_guard(**{**valid, **change})

    def test_hosted_guard_rejects_profile_and_release_overrides(self):
        for key in ("XDG_STATE_HOME", "XDG_CONFIG_HOME", "KOIDRIVE_RELEASE_BASE_URL", "KOIDRIVE_JOURNEY_PROBE", "KOIDRIVE_HELD_INSTALLER_FILE",
                    "HOMEBREW_CASK_OPTS", "HOMEBREW_NO_CASK_QUARANTINE", "ZDOTDIR"):
            with self.subTest(key=key), self.assertRaisesRegex(RuntimeError, "overrides"):
                DRIVER.hosted_guard(env={"GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "github-hosted", key: "fake"},
                                    system="Darwin", machine="arm64", home=self.root, account_home=self.root)

    def test_main_refuses_local_runtime_before_starting_any_command(self):
        destination = self.root / "evidence"
        with patch.dict(os.environ, {}, clear=True), patch.object(DRIVER, "OwnedProcess") as popen, patch("builtins.print"):
            status = DRIVER.main(["--tag", "v0.1.0-alpha.3", "--scenario", "installer", "--evidence", str(destination)])
        popen.assert_not_called()
        report = json.loads((destination / "report.json").read_text())
        self.assertEqual(status, 1)
        self.assertFalse(report["passed"])
        self.assertIn("GitHub-hosted", report["errors"][0])

    def test_evidence_directory_must_be_new(self):
        self.evidence()
        with self.assertRaises(FileExistsError):
            self.evidence()

    def test_empty_report_cannot_pass(self):
        for scenario in DRIVER.SCENARIOS:
            with self.subTest(scenario=scenario):
                report = {"scenario": scenario, "assertions": [], "cleanup": [], "errors": []}
                self.assertFalse(DRIVER.passed(report))

    def test_every_required_assertion_and_cleanup_is_required(self):
        for scenario in DRIVER.SCENARIOS:
            report = self.passing_report(scenario)
            self.assertTrue(DRIVER.passed(report))
            for index in range(len(report["assertions"])):
                changed = copy.deepcopy(report)
                changed["assertions"].pop(index)
                self.assertFalse(DRIVER.passed(changed))
                changed = copy.deepcopy(report)
                changed["assertions"][index]["passed"] = False
                self.assertFalse(DRIVER.passed(changed))
            for change in ({"errors": ["failed"]}, {"cleanup": []}, {"cleanup": [{"passed": False}]}):
                self.assertFalse(DRIVER.passed({**report, **change}))

    def test_later_passing_assertion_cannot_hide_earlier_failure(self):
        report = self.passing_report("installer")
        report["assertions"].insert(0, {"name": "attended setup", "passed": False})
        self.assertFalse(DRIVER.passed(report))

    def test_assertion_requires_true_not_truthy_serialized_status(self):
        report = self.passing_report("cask")
        report["assertions"][0]["passed"] = "SKIP"
        self.assertFalse(DRIVER.passed(report))

    def test_cleanup_failure_is_recorded_and_later_cleanup_runs(self):
        evidence = self.evidence()
        self.assertFalse(evidence.cleanup("failing", lambda: (_ for _ in ()).throw(RuntimeError("failed"))))
        self.assertTrue(evidence.cleanup("later", lambda: None))
        self.assertEqual([item["passed"] for item in evidence.report["cleanup"]], [False, True])

    def test_cleanup_refuses_existing_or_unclaimed_state(self):
        ownership = DRIVER.Ownership()
        foreign = self.root / "foreign"
        foreign.write_text("keep")
        with self.assertRaises(RuntimeError):
            ownership.claim(foreign)
        with self.assertRaises(RuntimeError):
            ownership.remove(foreign)
        self.assertEqual(foreign.read_text(), "keep")

    @unittest.skipUnless(os.name == "posix", "Symlink ownership probe requires POSIX")
    def test_cleanup_removes_owned_symlink_not_its_target(self):
        ownership = DRIVER.Ownership()
        foreign = self.root / "foreign"
        foreign.mkdir()
        (foreign / "document").write_text("keep")
        owned = ownership.claim(self.root / "owned")
        owned.symlink_to(foreign, target_is_directory=True)
        ownership.remove(owned)
        self.assertFalse(owned.is_symlink())
        self.assertEqual((foreign / "document").read_text(), "keep")

    @unittest.skipUnless(os.name == "posix", "Symlink ownership probe requires POSIX")
    def test_cleanup_refuses_changed_parent_symlink(self):
        ownership = DRIVER.Ownership()
        parent = self.root / "parent"
        parent.mkdir()
        owned = ownership.claim(parent / "owned")
        parent.rmdir()
        foreign = self.root / "foreign"
        foreign.mkdir()
        (foreign / "owned").write_text("keep")
        parent.symlink_to(foreign, target_is_directory=True)
        with self.assertRaises(RuntimeError):
            ownership.remove(owned)
        self.assertEqual((foreign / "owned").read_text(), "keep")

    @unittest.skipUnless(os.name == "posix", "Symlink ownership probe requires POSIX")
    def test_cleanup_refuses_claim_through_symlinked_parent(self):
        ownership = DRIVER.Ownership()
        target = self.root / "target"
        target.mkdir()
        parent = self.root / "parent"
        parent.symlink_to(target, target_is_directory=True)
        with self.assertRaises(RuntimeError):
            ownership.claim(parent / "new-state")

    def test_tap_waits_for_exact_version_and_checks_real_url_and_digest(self):
        evidence = self.evidence("cask")
        digest = "a" * 64
        name = "Koidrive-0.1.0-alpha.3-arm64.dmg"
        versions = iter(["0.1.0-alpha.2", "0.1.0-alpha.3", "0.1.0-alpha.3"])

        def brew(argv, **_):
            if argv == ["brew", "--version"]:
                return 0, "Homebrew 7.0.0\n", ""
            if argv == ["brew", "tap"]:
                return 0, "koidrive/tap\n", ""
            if argv[:2] == ["brew", "info"]:
                data = {"casks": [{"version": next(versions), "sha256": digest,
                        "url": DRIVER.RELEASE + "/v0.1.0-alpha.3/" + name, "depends_on": {}, **self.zap_spec()}]}
                return 0, json.dumps(data), ""
            return 0, "real tap source", ""

        with patch.object(evidence, "run", side_effect=brew) as run, patch.object(DRIVER.time, "sleep"):
            DRIVER.brew_prepare(evidence, {name: digest}, "cask")
        self.assertIn(unittest.mock.call(["brew", "update", "--quiet"], timeout=120), run.call_args_list)
        self.assertIn(unittest.mock.call(["brew", "trust", "--cask", DRIVER.TAP]), run.call_args_list)

    def test_tap_wrong_version_does_not_fall_back_to_a_fixture(self):
        evidence = self.evidence("cask")

        def brew(argv, **_):
            if argv[:2] == ["brew", "info"]:
                return 0, json.dumps({"casks": [{"version": "9.9.9"}]}), ""
            return 0, "Homebrew 6.0.0\n" if argv[1] == "--version" else "koidrive/tap\n", ""

        with patch.object(evidence, "run", side_effect=brew) as run:
            with self.assertRaisesRegex(RuntimeError, "publication wait expired"):
                DRIVER.brew_prepare(evidence, {}, "cask", wait_seconds=0)
        self.assertTrue(all(call.args[0][:2] != ["brew", "install"] for call in run.call_args_list))

    def test_formula_trust_is_granted_for_formula_not_cask(self):
        evidence = self.evidence("formula-conflict")
        digest = "a" * 64
        name = "koidrive-aarch64-apple-darwin.tar.gz"
        data = {"formulae": [{"versions": {"stable": "0.1.0-alpha.3"}, "urls": {"stable": {
            "checksum": digest, "url": DRIVER.RELEASE + "/v0.1.0-alpha.3/" + name}}}]}

        def brew(argv, **_):
            if argv[:2] == ["brew", "info"]:
                return 0, json.dumps(data), ""
            return 0, "Homebrew 7.0.0\n" if argv[1] == "--version" else "koidrive/tap\n", ""

        with patch.object(evidence, "run", side_effect=brew) as run:
            DRIVER.brew_prepare(evidence, {name: digest}, "formula")
        self.assertIn(unittest.mock.call(["brew", "trust", "--formula", DRIVER.TAP]), run.call_args_list)
        self.assertNotIn(unittest.mock.call(["brew", "trust", "--cask", DRIVER.TAP]), run.call_args_list)

    def test_conflict_summary_starts_with_exact_collision_line(self):
        evidence = self.evidence("formula-conflict")
        collision = "Error: It seems there is already a Binary at '/opt/homebrew/bin/koi'."
        evidence.report["conflict"] = collision
        with patch("builtins.print"):
            evidence.close()
        self.assertTrue((evidence.directory / "summary.txt").read_text().startswith(collision + "\n"))

    def test_quarantine_preservation_allows_os_flag_and_provenance_changes(self):
        before = "/Applications/Koidrive.app: com.apple.quarantine: 0083;initial\n"
        after = ("/Applications/Koidrive.app: com.apple.quarantine: 00c3;approved\n"
                 "/Applications/Koidrive.app: com.apple.provenance: changed\n")
        self.assertLessEqual(DRIVER.quarantine_paths(before), DRIVER.quarantine_paths(after))
        self.assertFalse(DRIVER.quarantine_paths(before) <= DRIVER.quarantine_paths("com.apple.provenance: changed\n"))
        self.assertEqual(DRIVER.quarantine_paths(""), set())
        self.assertEqual(DRIVER.quarantine_paths("com.apple.quarantine: 0083;initial\n"), {"."})

    def test_redaction_handles_unknown_claim_pairing_and_json_credentials(self):
        for text, secret in (("Koidrive claim code (valid for 1 hour): ABCD12345678\n", "ABCD12345678"),
                             ('"token": "secret-token"\n', "secret-token"),
                             ('"recovery_key": "secret-key"\n', "secret-key"),
                             ('  "K7QM-2XPD",\n', "K7QM-2XPD"),
                             ('  "K7QM01PD",\n', "K7QM01PD"),
                             ("Authorization: Bearer secret-token\n", "secret-token"),
                             ('"join_argv": ["koi", "join", "short-secret"]', "short-secret")):
            with self.subTest(text=text):
                self.assertNotIn(secret, DRIVER.sanitized(text))
        digest = "a" * 64
        self.assertIn(digest, DRIVER.sanitized("SHA256 " + digest))
        self.assertEqual(DRIVER.sanitized("SHA256SUMS"), "SHA256SUMS")

    def test_only_the_expected_binary_collision_counts_as_conflict(self):
        binary = self.root / "bin/koi"
        collision = f"Error: It seems there is already a Binary at '{binary}'.\n"
        self.assertTrue(DRIVER.binary_conflict(1, collision, binary))
        for status, output in ((0, collision), (124, collision), (-9, collision),
                               (1, "Error: network conflict installing koi"),
                               (1, "Error: koidrive/tap is not trusted"),
                               (1, "Error: It seems there is already a Binary at '/foreign/koi'."),
                               (1, f"Already installed formula {binary}; network error")):
            with self.subTest(status=status, output=output):
                self.assertFalse(DRIVER.binary_conflict(status, output, binary))

    def test_formula_conflict_distinguishes_skipped_link_from_failed_rollback(self):
        binary = self.root / "bin/koi"
        warning = f"::warning::It seems there is already a Binary at '{binary}' from formula koidrive; skipping link."
        error = f"Error: It seems there is already a Binary at '{binary}'."
        self.assertEqual(DRIVER.conflict_state(0, warning, binary), "installed-with-skipped-link")
        self.assertEqual(DRIVER.conflict_state(1, error, binary), "failed-with-rollback")
        self.assertEqual(DRIVER.skipped_link_text("downloaded\n" + warning + "\n", binary), warning)
        for status, output in ((1, warning), (124, warning), (-9, warning), (0, error),
                               (0, warning.replace(str(binary), "/foreign/koi")),
                               (0, warning.replace("formula koidrive", "formula other")),
                               (0, warning.replace("::warning::", "Warning: ")),
                               (0, warning.replace("; skipping link.", ".")),
                               (0, warning + "\n" + error), (1, "network failure"),
                               (0, "skipping link"), (1, "tap is not trusted")):
            with self.subTest(status=status, output=output):
                self.assertIsNone(DRIVER.conflict_state(status, output, binary))

    def test_native_zap_hook_rejects_missing_changed_or_duplicate_metadata(self):
        valid = self.zap_spec()
        self.assertEqual(DRIVER.native_zap_hook(valid), valid["artifacts"][0]["zap"][0]["script"])
        self.assertIsNone(DRIVER.native_zap_hook({}))
        for change in ({"executable": "/foreign/Koidrive"}, {"args": []},
                       {"args": ["--unregister-login-item", "--other"]}, {"sudo": True}, {"sudo": 0}):
            altered = copy.deepcopy(valid)
            altered["artifacts"][0]["zap"][0]["script"].update(change)
            with self.subTest(change=change):
                self.assertIsNone(DRIVER.native_zap_hook(altered))
        missing = copy.deepcopy(valid)
        del missing["artifacts"][0]["zap"][0]["script"]["sudo"]
        self.assertIsNone(DRIVER.native_zap_hook(missing))
        duplicate = copy.deepcopy(valid)
        duplicate["artifacts"] *= 2
        self.assertIsNone(DRIVER.native_zap_hook(duplicate))

    def test_tap_preflight_refuses_missing_or_changed_native_hook(self):
        evidence = self.evidence("cask")
        digest = "a" * 64
        name = "Koidrive-0.1.0-alpha.3-arm64.dmg"
        changed = self.zap_spec()
        changed["artifacts"][0]["zap"][0]["script"]["args"] = []
        for metadata in ({}, changed):
            spec = {"version": "0.1.0-alpha.3", "sha256": digest,
                    "url": DRIVER.RELEASE + "/v0.1.0-alpha.3/" + name, **metadata}

            def brew(argv, **_):
                if argv[:2] == ["brew", "info"]:
                    return 0, json.dumps({"casks": [spec]}), ""
                return 0, "Homebrew 6.0.0\n" if argv[1] == "--version" else "koidrive/tap\n", ""

            with self.subTest(metadata=metadata), patch.object(evidence, "run", side_effect=brew):
                with self.assertRaisesRegex(RuntimeError, "native zap hook metadata"):
                    DRIVER.brew_prepare(evidence, {name: digest}, "cask")

    def test_zap_evidence_requires_exact_validated_executable_and_success(self):
        executable = str(DRIVER.APP / "Contents/MacOS/Koidrive")
        line = "==> Running uninstall script " + executable
        self.assertTrue(DRIVER.zap_invoked(0, "==> Zapping Cask koidrive\n" + line + "\n", executable))
        for status, output in ((1, line), (124, line), (0, line + "-other"),
                               (0, line.replace(executable, "/foreign/Koidrive")),
                               (0, "--unregister-login-item"), (0, "Mentioned " + line)):
            self.assertFalse(DRIVER.zap_invoked(status, output, executable))

    def formula_collision(self, unlink_on_zap=False, rollback=False):
        evidence = DRIVER.Evidence(self.root / ("evidence-" + str(unlink_on_zap)), "v0.1.0-alpha.3", "formula-conflict")
        home = self.root / "home"
        home.mkdir()
        app = home / "Applications/Koidrive.app"
        prefix = self.root / "brew"
        link = prefix / "bin/koi"
        link.parent.mkdir(parents=True)
        formula = prefix / "Cellar/koidrive/koi"
        formula.parent.mkdir(parents=True)
        formula.write_bytes(b"unit-only formula bytes")
        warning = f"::warning::It seems there is already a Binary at '{link}' from formula koidrive; skipping link."
        collision = f"Error: It seems there is already a Binary at '{link}'." if rollback else warning

        def command(argv, **_):
            if argv == ["brew", "--prefix"]:
                return 0, str(prefix) + "\n", ""
            if argv == ["brew", "install", "--formula", DRIVER.TAP]:
                link.symlink_to(formula)
            elif argv == ["koi", "--version"]:
                return (0, "koi 0.1.0-alpha.3\n", "") if link.exists() else (127, "", "not found")
            elif argv == ["brew", "install", "--cask", DRIVER.TAP]:
                if rollback:
                    return 1, "", collision + "\n"
                app.mkdir(parents=True)
                return 0, "Cask installed\n", warning + "\n"
            elif argv == ["brew", "list", "--cask", "--versions"]:
                return 0, "" if rollback else "koidrive 0.1.0-alpha.3\n", ""
            elif argv == ["brew", "--caskroom"]:
                return 0, str(prefix / "Caskroom") + "\n", ""
            elif argv == ["brew", "uninstall", "--zap", "--cask", DRIVER.TAP]:
                app.rmdir()
                if unlink_on_zap:
                    link.unlink()
            elif argv == ["brew", "uninstall", "--formula", DRIVER.TAP]:
                link.unlink(missing_ok=True)
            else:
                raise AssertionError(argv)
            return 0, "", ""

        with patch.object(DRIVER.Path, "home", return_value=home), patch.object(DRIVER, "APP", app), \
                patch.object(DRIVER, "brew_prepare"), patch.object(DRIVER, "state_paths", return_value=[app, home / "user-app", home / "koi"]), \
                patch.object(evidence, "run", side_effect=command) as run:
            DRIVER.brew_scenario(evidence, self.root, DRIVER.Ownership(), {}, True)
        return evidence, run, collision

    def test_warning_install_proves_registered_app_and_cleans_it_with_zap(self):
        evidence, run, warning = self.formula_collision()
        self.assertEqual(evidence.report["conflict_state"], "installed-with-skipped-link")
        self.assertEqual(evidence.report["conflict_status"], 0)
        self.assertEqual(evidence.report["conflict"], warning)
        self.assertEqual(evidence.report["formula_before"], evidence.report["formula_after"])
        self.assertEqual(evidence.report["formula_before"], evidence.report["formula_after_cask_uninstall"])
        self.assertTrue(evidence.report["conflict_installation"]["app_exists"])
        self.assertIn(unittest.mock.call(["brew", "uninstall", "--zap", "--cask", DRIVER.TAP]), run.call_args_list)
        self.assertTrue(all(item["passed"] for item in evidence.report["cleanup"]))

    def test_cask_cleanup_cannot_hide_deleting_a_skipped_formula_link(self):
        evidence, run, _ = self.formula_collision(unlink_on_zap=True)
        checks = {item["name"]: item["passed"] for item in evidence.report["cleanup"]}
        self.assertFalse(checks["formula survives cask uninstall"])
        self.assertTrue(checks["formula uninstall"])
        self.assertIn(unittest.mock.call(["brew", "uninstall", "--formula", DRIVER.TAP]), run.call_args_list)

    def test_nonzero_binary_collision_still_requires_rollback(self):
        evidence, run, error = self.formula_collision(rollback=True)
        self.assertEqual(evidence.report["conflict_state"], "failed-with-rollback")
        self.assertEqual(evidence.report["conflict_status"], 1)
        self.assertEqual(evidence.report["conflict"], error)
        self.assertFalse(evidence.report["conflict_installation"]["app_exists"])
        self.assertNotIn(unittest.mock.call(["brew", "uninstall", "--zap", "--cask", DRIVER.TAP]), run.call_args_list)
        self.assertTrue(all(item["passed"] for item in evidence.report["cleanup"]))

    def test_capture_redacts_before_persisting_claim_stream(self):
        evidence = self.evidence()
        text = "Koidrive claim code (valid for 1 hour): ABCD12345678\n"
        _, raw, _ = evidence.run([sys.executable, "-c", "print(" + repr(text) + ",end='')"])
        self.assertIn("ABCD12345678", raw)
        persisted = (evidence.directory / evidence.report["commands"][0]["stdout"]).read_text()
        self.assertNotIn("ABCD12345678", persisted)
        self.assertIn("[REDACTED]", persisted)

    def test_native_cleanup_probes_only_owned_labels_and_rejects_unknown_or_loaded_state(self):
        evidence = self.evidence("installer")
        label = "dev.koidrive.koi.abcdef12"
        expected = ["/bin/launchctl", "print", f"gui/{os.getuid()}/{label}"]
        with patch.object(evidence, "run", return_value=(113, "", "Could not find service")) as run:
            DRIVER.assert_native_absent(evidence, [label])
        run.assert_called_once_with(expected, required=False)
        for status, output, error in ((0, "state = running", ""), (124, "", ""), (1, "", "permission denied")):
            with self.subTest(status=status, error=error), patch.object(evidence, "run", return_value=(status, output, error)):
                with self.assertRaisesRegex(RuntimeError, "Could not prove owned service unloaded"):
                    DRIVER.assert_native_absent(evidence, [label])

    def test_clean_account_snapshot_queries_launchd_without_a_global_btm_probe(self):
        evidence = self.evidence("cask")
        with patch.object(evidence, "run", return_value=(0, "GUI domain", "")) as run:
            self.assertEqual(DRIVER.launchd_state(evidence), "GUI domain")
        run.assert_called_once_with(["/bin/launchctl", "print", f"gui/{os.getuid()}"])

    def test_cask_requires_quarantine_on_the_bundle_not_only_a_nested_file(self):
        evidence = self.evidence("cask")
        app = self.root / "Koidrive.app"
        nested = str(app / "Contents/Helpers/koi") + ": com.apple.quarantine: 0081;test\n"
        with patch.object(evidence, "run", side_effect=[(0, nested, ""), (1, "", "No such xattr")]):
            with self.assertRaisesRegex(RuntimeError, "app quarantine"):
                DRIVER.quarantine(evidence, app)

    def test_failed_command_is_not_silently_accepted(self):
        evidence = self.evidence()
        with self.assertRaises(RuntimeError):
            evidence.run([sys.executable, "-c", "raise SystemExit(7)"])
        self.assertEqual(evidence.report["commands"][0]["status"], 7)
        status, _, _ = evidence.run([sys.executable, "-c", "raise SystemExit(7)"], required=False)
        self.assertEqual(status, 7)

    def test_version_rejects_wrong_extra_or_missing_output(self):
        evidence = self.evidence()
        for status, output in ((1, "koi 0.1.0-alpha.3\n"), (0, "koi 0.1.0-alpha.2\n"),
                               (0, "warning\nkoi 0.1.0-alpha.3\n"), (0, "koi 0.1.0-alpha.3\n\n"), (0, "")):
            with self.subTest(status=status, output=output), patch.object(evidence, "run", return_value=(status, output, "")):
                with self.assertRaises(RuntimeError):
                    DRIVER.version(evidence, "never-executed-koi", "exact version")

    @unittest.skipUnless(os.name == "posix", "Controlling PTYs require POSIX")
    def test_pty_keeps_installer_stdin_a_pipe_and_answers_real_tty(self):
        evidence = self.evidence("installer")
        probe = self.root / "harmless-probe.sh"
        python = "\n".join([
            "import os,stat,sys",
            "assert stat.S_ISFIFO(os.fstat(0).st_mode)",
            "assert not os.isatty(0) and os.isatty(1) and os.isatty(2)",
            "assert os.environ.get('CI') is None and os.environ['TERM']=='dumb'",
            "assert os.environ['DO_NOT_TRACK']=='1'",
            "assert os.environ['PATH']==" + repr(DRIVER.SYSTEM_PATH),
            "tty=open('/dev/tty','r')",
            "assert os.tcgetpgrp(tty.fileno())==os.getpgrp()",
            "print('Which shared folder should sync here?\\n  1. Journey\\n  2. Create a shared folder\\nChoose 1 to 2: ',end='',flush=True)",
            "assert tty.readline().strip()=='1'",
            "print('~/Koidrive is linked to Journey.')",
            "print('Setup complete. Add files to ~/Koidrive.')",
        ])
        probe.write_text("exec " + shlex.quote(sys.executable) + " -c " + shlex.quote(python) + "\n")
        code, transcript, answers = DRIVER.attended(evidence, DRIVER.pipeline(probe), timeout=10)
        self.assertEqual(code, 0, transcript)
        self.assertEqual(answers, 1)
        self.assertIn("~/Koidrive is linked to Journey.", transcript)

    @unittest.skipUnless(os.name == "posix", "Controlling PTYs require POSIX")
    def test_pty_without_prompts_does_not_synthesize_completion(self):
        evidence = self.evidence("installer")
        code, transcript, answers = DRIVER.attended(evidence, "printf 'no setup happened\\n'", timeout=5)
        self.assertEqual(code, 0)
        self.assertEqual(answers, 0)
        self.assertNotIn("is linked to", transcript)

    def test_pipeline_quotes_file_url_without_options_or_setup_bypass(self):
        command = DRIVER.pipeline(self.root / "a 'b; c.sh")
        self.assertTrue(command.startswith("curl -fsSL -- file://"))
        self.assertTrue(command.endswith(" | sh"))
        self.assertNotIn("--no-app", command)
        self.assertNotIn("--no-quarantine", command)

    def test_stop_group_refuses_foreign_process_group(self):
        process = unittest.mock.Mock(pid=12345)
        process.poll.return_value = None
        with patch.object(DRIVER.os, "getpgid", return_value=999, create=True), patch.object(DRIVER.os, "killpg", create=True) as killpg:
            with self.assertRaises(RuntimeError):
                DRIVER.stop_group(process)
        killpg.assert_not_called()

    @unittest.skipUnless(os.name == "posix", "Process groups require POSIX")
    def test_stop_group_removes_stubborn_child_after_leader_already_exited(self):
        _, item, child = self.stubborn_group(leader_exits=True)
        process = item[0]
        self.assertEqual(process.returncode, 0)
        self.assertEqual(os.getpgid(child["pid"]), process.pid)
        DRIVER.stop_group(process, term_timeout=0.1, kill_timeout=2)
        self.assertTrue(DRIVER.until(lambda: not self.group_exists(process.pid), 1), "Child survived cleanup after its leader exited")

    @unittest.skipUnless(os.name == "posix", "Process groups require POSIX")
    def test_stop_group_removes_stubborn_child_when_term_exits_only_leader(self):
        _, item, child = self.stubborn_group(leader_exits=False)
        process = item[0]
        self.assertEqual(os.getpgid(child["pid"]), process.pid)
        DRIVER.stop_group(process, term_timeout=0.1, kill_timeout=2)
        self.assertTrue(DRIVER.until(lambda: not self.group_exists(process.pid), 1), "TERM killed only the group leader")

    @unittest.skipUnless(os.name == "posix", "Process groups require POSIX")
    def test_finished_command_reaps_its_group_before_accepting_redirected_output(self):
        evidence, item, _ = self.stubborn_group(leader_exits=True)
        stop = DRIVER.stop_group
        with patch.object(DRIVER, "stop_group", side_effect=lambda process: stop(process, term_timeout=0.1, kill_timeout=2)):
            status, _, _ = evidence.finish(item)
        self.assertEqual(status, 0)
        self.assertTrue(item[0].group_closed)
        self.assertFalse(self.group_exists(item[0].pid))

    @unittest.skipUnless(os.name == "posix", "Process groups require POSIX")
    def test_evidence_close_checks_group_even_when_leader_has_exited(self):
        evidence, item, _ = self.stubborn_group(leader_exits=True)
        item[1]["status"] = 0
        stop = DRIVER.stop_group
        with patch.object(DRIVER, "stop_group", side_effect=lambda process: stop(process, term_timeout=0.1, kill_timeout=2)), patch("builtins.print"):
            evidence.close()
        self.assertTrue(item[0].group_closed)
        self.assertFalse(self.group_exists(item[0].pid))
        self.assertTrue(evidence.report["cleanup"])
        self.assertTrue(all(item["passed"] for item in evidence.report["cleanup"]))

    @unittest.skipUnless(os.name == "posix", "Process groups require POSIX")
    def test_stop_group_refuses_changed_group_session_or_reused_leader(self):
        _, item, _ = self.stubborn_group(leader_exits=False)
        process = item[0]
        for pgid, sid in ((os.getpgrp(), process.pid), (process.pid, os.getsid(0))):
            with self.subTest(pgid=pgid, sid=sid), patch.object(DRIVER.os, "getpgid", return_value=pgid), \
                    patch.object(DRIVER.os, "getsid", return_value=sid), patch.object(DRIVER.os, "killpg") as killpg:
                with self.assertRaisesRegex(RuntimeError, "changed or reused"):
                    DRIVER.stop_group(process)
            killpg.assert_not_called()
        os.kill(process.pid, signal.SIGTERM)
        process.wait(timeout=3)
        with patch.object(DRIVER.os, "getpgid", return_value=process.pid), \
                patch.object(DRIVER.os, "getsid", return_value=process.pid), patch.object(DRIVER.os, "killpg") as killpg:
            with self.assertRaisesRegex(RuntimeError, "changed or reused"):
                DRIVER.stop_group(process)
        killpg.assert_not_called()

    @unittest.skipUnless(os.name == "posix", "Process groups require POSIX")
    def test_closed_group_is_not_probed_again_after_pid_can_be_reused(self):
        evidence = self.evidence()
        evidence.run([sys.executable, "-c", "pass"])
        process = evidence.processes[0][0]
        self.assertTrue(process.group_closed)
        with patch.object(DRIVER.os, "getpgid") as getpgid, patch.object(DRIVER.os, "killpg") as killpg:
            DRIVER.stop_group(process)
        getpgid.assert_not_called()
        killpg.assert_not_called()

    @unittest.skipUnless(os.name == "posix", "Process groups require POSIX")
    def test_unknown_group_probe_failure_cannot_mark_cleanup_successful(self):
        evidence, item, _ = self.stubborn_group(leader_exits=False)
        process = item[0]
        with patch.object(DRIVER.os, "killpg", side_effect=PermissionError("group probe denied")) as killpg:
            self.assertFalse(evidence.cleanup("owned group", lambda: DRIVER.stop_group(process)))
        killpg.assert_called_once_with(process.pid, 0)
        self.assertFalse(process.group_closed)
        self.assertFalse(evidence.report["cleanup"][0]["passed"])

    def test_group_cleanup_escalates_and_fails_if_sigkill_cannot_remove_group(self):
        evidence = self.evidence()
        process = unittest.mock.Mock(pid=12345)
        with patch.object(DRIVER, "group_present", return_value=True), patch.object(DRIVER.os, "killpg", create=True) as killpg:
            self.assertFalse(evidence.cleanup("owned group", lambda: DRIVER.stop_group(process, term_timeout=0, kill_timeout=0)))
        self.assertEqual(killpg.call_args_list, [unittest.mock.call(12345, signal.SIGTERM), unittest.mock.call(12345, signal.SIGKILL)])
        self.assertIn("survived SIGKILL", evidence.report["cleanup"][0]["detail"])

    def test_missing_leader_waits_for_confirmed_exit_before_probing_descendants(self):
        process = unittest.mock.Mock(spec=DRIVER.OwnedProcess, pid=12345, group_closed=False)
        process.poll.return_value = None
        with patch.object(DRIVER.os, "getpgid", side_effect=ProcessLookupError), patch.object(DRIVER.os, "killpg") as killpg:
            self.assertTrue(DRIVER.group_present(process))
        process.wait.assert_called_once_with(timeout=1)
        killpg.assert_called_once_with(process.pid, 0)
        self.assertFalse(process.group_closed)

    def test_missing_leader_without_confirmed_exit_cannot_signal_or_close_group(self):
        process = unittest.mock.Mock(spec=DRIVER.OwnedProcess, pid=12345, group_closed=False)
        process.poll.return_value = None
        process.wait.side_effect = subprocess.TimeoutExpired("owned", 1)
        with patch.object(DRIVER.os, "getpgid", side_effect=ProcessLookupError), patch.object(DRIVER.os, "killpg") as killpg:
            with self.assertRaisesRegex(RuntimeError, "Could not verify the owned session leader"):
                DRIVER.group_present(process)
        killpg.assert_not_called()
        self.assertFalse(process.group_closed)

    def test_eperm_census_accepts_only_empty_or_owned_zombie_groups(self):
        for rows in ("1 1 0 S\n", "1 1 0 ?Es\n", "1 1 0 S\n12346 12345 501 ZN\n12347 12345 501 Z\n"):
            process = self.census_process()
            result = subprocess.CompletedProcess([], 0, rows, "")
            with self.subTest(rows=rows), patch.object(DRIVER.subprocess, "run", return_value=result) as run:
                self.assertFalse(DRIVER.group_census(process, "signal-zero", PermissionError(errno.EPERM, "probe denied")))
            run.assert_called_once_with(["/bin/ps", "-axo", "pid=,pgid=,uid=,stat="], capture_output=True, text=True, timeout=3)
            self.assertTrue(process.group_closed)
            self.assertEqual(process.group_observations[0]["decision"], "no-live-members")
            self.assertTrue(all(set(member) == {"pid", "pgid", "uid", "stat"}
                                for member in process.group_observations[0]["members"]))

    def test_eperm_census_rejects_live_foreign_and_malformed_members(self):
        rows = ("1 1 0 S\n12346 12345 501 S\n", "1 1 0 S\n12346 12345 0 Z\n",
                "1 1 0 S\n12346 12345 501 ?Es\n",
                "1 1 0 S\n12346 12345 0 R\n", "PID PGID UID STAT\n", "\n", "",
                "1 1 0 S\n12346 12345 501 Z extra-secret-argv\n", "1 1 0 S\n12346 12345 user Z\n",
                "1 1 0 S\n12346 12345 501 Zinvalid\n", "1 1 0 S\n1 1 0 S\n",
                "1 1 0 S\n12346 -12345 501 Z\n")
        for text in rows:
            process = self.census_process()
            with self.subTest(rows=text), patch.object(DRIVER.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, text, "")):
                with self.assertRaisesRegex(RuntimeError, "Could not verify owned process group"):
                    DRIVER.group_census(process, "signal-zero", PermissionError(errno.EPERM, "probe denied"))
            self.assertFalse(process.group_closed)
            self.assertEqual(process.group_observations[0]["decision"], "rejected")
            self.assertNotIn("extra-secret-argv", json.dumps(process.group_observations))

    def test_eperm_census_failure_or_timeout_never_confirms_absence(self):
        failures = [subprocess.CompletedProcess([], 1, "1 1 0 S\n", ""),
                    subprocess.CompletedProcess([], 0, "1 1 0 S\n", "permission denied"),
                    PermissionError(errno.EPERM, "census denied"),
                    subprocess.TimeoutExpired("ps", 3)]
        for failure in failures:
            process = self.census_process()
            behavior = {"side_effect": failure} if isinstance(failure, Exception) else {"return_value": failure}
            with self.subTest(failure=str(failure)), patch.object(DRIVER.subprocess, "run", **behavior):
                with self.assertRaisesRegex(RuntimeError, "Could not verify owned process group"):
                    DRIVER.group_census(process, "signal-zero", PermissionError(errno.EPERM, "probe denied"))
            self.assertFalse(process.group_closed)
            self.assertEqual(process.group_observations[0]["decision"], "rejected")

    def test_only_eperm_can_use_the_process_table_fallback(self):
        for failure in (PermissionError(errno.EACCES, "access denied"), PermissionError("unknown denial")):
            process = self.census_process()
            with self.subTest(error=str(failure)), patch.object(DRIVER.subprocess, "run") as run:
                with self.assertRaisesRegex(RuntimeError, "Unexpected owned-group permission error"):
                    DRIVER.group_census(process, "signal-zero", failure)
            run.assert_not_called()
            self.assertFalse(process.group_closed)

    def test_non_probe_eperm_is_not_permission_to_close_a_group(self):
        result = subprocess.CompletedProcess([], 0, "1 1 0 S\n", "")
        for phase in ("getpgid", "getsid", "SIGTERM", "SIGKILL"):
            process = self.census_process()
            with self.subTest(phase=phase), patch.object(DRIVER.subprocess, "run", return_value=result):
                with self.assertRaisesRegex(RuntimeError, "Permission failure in owned-group " + phase):
                    DRIVER.group_census(process, phase, PermissionError(errno.EPERM, "denied"))
            self.assertFalse(process.group_closed)
            self.assertEqual(process.group_observations[0]["phase"], phase)
            self.assertEqual(process.group_observations[0]["status"], 0)

    @unittest.skipUnless(os.name == "posix", "Process groups require POSIX")
    def test_eperm_census_is_persisted_in_command_and_cleanup_evidence(self):
        evidence = self.evidence()
        item = evidence.start([sys.executable, "-c", "pass"])
        item[0].wait(timeout=3)
        item[1]["status"] = 0
        result = subprocess.CompletedProcess([], 0, "1 1 0 S\n", "")
        with patch.object(DRIVER.os, "killpg", side_effect=PermissionError(errno.EPERM, "probe denied")), \
                patch.object(DRIVER.subprocess, "run", return_value=result), patch("builtins.print"):
            evidence.close()
        report = json.loads((evidence.directory / "report.json").read_text())
        command = report["commands"][0]["group"]
        cleanup = report["cleanup"][0]["group"]
        self.assertEqual(command, cleanup)
        self.assertEqual(command["pgid"], item[0].pid)
        self.assertEqual(command["observations"][0]["phase"], "signal-zero")
        self.assertEqual(command["observations"][0]["decision"], "no-live-members")
        self.assertTrue(report["cleanup"][0]["passed"])

    @unittest.skipUnless(DRIVER.platform.system() == "Darwin", "Darwin zombie-only EPERM reproduction")
    def test_real_darwin_zombie_group_requires_successful_census_before_closing(self):
        process = DRIVER.OwnedProcess(["/usr/bin/true"], stdin=subprocess.DEVNULL,
                                      stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.addCleanup(process.wait, timeout=3)

        def zombie_ready():
            result = subprocess.run(["/bin/ps", "-p", str(process.pid), "-o", "stat="],
                                    capture_output=True, text=True, timeout=3)
            return result.returncode == 0 and result.stdout.strip().startswith("Z")

        self.assertTrue(DRIVER.until(zombie_ready, 3))
        with self.assertRaises(PermissionError) as caught:
            os.killpg(process.pid, 0)
        self.assertEqual(caught.exception.errno, errno.EPERM)
        self.assertFalse(DRIVER.group_census(process, "signal-zero", caught.exception))
        self.assertTrue(process.group_closed)
        members = process.group_observations[0]["members"]
        self.assertEqual([member["pid"] for member in members], [process.pid])
        self.assertTrue(members[0]["stat"].startswith("Z"))
        self.assertEqual(members[0]["uid"], os.getuid())
        self.assertTrue(DRIVER.until(lambda: not self.group_exists(process.pid), 3))

    @unittest.skipUnless(os.name == "posix", "Workflow shell selection requires POSIX")
    def test_release_tap_selection_runs_actual_workflow_shell_on_final_tap_state(self):
        release = (DRIVER.REPO / ".github/workflows/release-cli.yml").read_text()
        blocks = re.findall(r'(?ms)^          selected=true\n.*?^          echo "selected=\$selected" >> "\$GITHUB_OUTPUT"$', release)
        self.assertEqual(len(blocks), 1)
        script = "tap=$1\n" + textwrap.dedent(blocks[0])
        tap = self.root / "tap"
        for kind in ("Formula", "Casks"):
            (tap / kind).mkdir(parents=True)
        output = self.root / "github-output"
        requested = "v0.1.0-alpha.3"
        for formula, cask, expected in ((requested, requested, "true"),
                                         ("v0.1.0-alpha.2", requested, "false"),
                                         (requested, "v0.1.0-alpha.2", "false"),
                                         ("v0.1.0", "v0.1.0", "false")):
            for kind, tag in (("Formula", formula), ("Casks", cask)):
                (tap / kind / "koidrive.rb").write_text(f'url "https://github.com/koidrive/koidrive/releases/download/{tag}/asset"\n')
            output.unlink(missing_ok=True)
            with self.subTest(formula=formula, cask=cask):
                result = subprocess.run(["/bin/bash", "-eu", "-c", script, "tap-selection", str(tap)],
                                        env={"PATH": DRIVER.SYSTEM_PATH, "TAG": requested, "GITHUB_OUTPUT": str(output)},
                                        capture_output=True, text=True, timeout=5)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(output.read_text(), "selected=" + expected + "\n")

    def test_reusable_workflow_keeps_installer_when_final_tap_does_not_select_tag(self):
        release = (DRIVER.REPO / ".github/workflows/release-cli.yml").read_text()
        journeys = (DRIVER.REPO / ".github/workflows/release-journeys.yml").read_text()
        matrix = re.search(r"scenario: \$\{\{ fromJSON\(inputs\.skip_homebrew && '([^']+)' \|\| '([^']+)'\) \}\}", journeys)
        self.assertIsNotNone(matrix)
        self.assertEqual(json.loads(matrix[1]), ["installer"])
        self.assertEqual(json.loads(matrix[2]), ["formula-conflict", "cask", "installer", "homebrew-update"])
        self.assertIn("selected: ${{ steps.tap.outputs.selected }}", release)
        self.assertRegex(release, r"(?s)  release-journeys:\n.*?needs: homebrew.*?skip_homebrew: \$\{\{ needs\.homebrew\.outputs\.selected != 'true' \}\}")

    @patch.object(DRIVER.os, "getuid", return_value=501, create=True)
    def test_native_service_cleanup_refuses_foreign_profile_or_binary(self, _uid):
        evidence = self.evidence("installer")
        agents = self.root / "Library/LaunchAgents"
        agents.mkdir(parents=True)
        path = agents / "dev.koidrive.koi.plist"
        binary = self.root / ".local/bin/koi"
        own_state = str(self.root / ".local/state")
        spec = {"Label": "dev.koidrive.koi", "ProgramArguments": [str(binary), "_daemon"],
                "EnvironmentVariables": {"XDG_STATE_HOME": own_state}}
        for change in ({"ProgramArguments": ["/foreign/koi", "_daemon"]},
                       {"EnvironmentVariables": {"XDG_STATE_HOME": "/foreign/state"}},
                       {"Label": "dev.koidrive.koi.foreign"}):
            path.write_bytes(plistlib.dumps({**spec, **change}))
            with patch.object(DRIVER.Path, "home", return_value=self.root), patch.object(evidence, "run") as run:
                with self.assertRaisesRegex(RuntimeError, "foreign launch agent"):
                    DRIVER.service_cleanup(evidence, {own_state}, [binary], timeout=0)
            run.assert_not_called()
            self.assertTrue(path.exists())

    @patch.object(DRIVER.os, "getuid", return_value=501, create=True)
    def test_native_service_cleanup_only_removes_after_confirmed_bootout(self, _uid):
        evidence = self.evidence("installer")
        agents = self.root / "Library/LaunchAgents"
        agents.mkdir(parents=True)
        path = agents / "dev.koidrive.koi.plist"
        binary = self.root / ".local/bin/koi"
        own_state = str(self.root / ".local/state")
        spec = {"Label": "dev.koidrive.koi", "ProgramArguments": [str(binary), "_daemon"],
                "EnvironmentVariables": {"XDG_STATE_HOME": own_state}}
        path.write_bytes(plistlib.dumps(spec))
        with patch.object(DRIVER.Path, "home", return_value=self.root), patch.object(evidence, "run", return_value=(0, "", "")):
            with self.assertRaisesRegex(RuntimeError, "prove owned service unloaded"):
                DRIVER.service_cleanup(evidence, {own_state}, [binary], timeout=0)
        self.assertTrue(path.exists())
        with patch.object(DRIVER.Path, "home", return_value=self.root), patch.object(evidence, "run", side_effect=[(0, "", ""), (1, "", "Could not find service")]):
            labels = DRIVER.service_cleanup(evidence, {own_state}, [binary])
        self.assertFalse(path.exists())
        self.assertEqual(labels, ["dev.koidrive.koi"])

    @patch.object(DRIVER.os, "getuid", return_value=501, create=True)
    def test_peer_service_cleanup_uses_its_produced_label(self, _uid):
        evidence = self.evidence("installer")
        agents = self.root / "Library/LaunchAgents"
        agents.mkdir(parents=True)
        label = "dev.koidrive.koi.1234abcd"
        path = agents / (label + ".plist")
        binary = self.root / ".local/bin/koi"
        state = str(self.root / "peer/state")
        spec = {"Label": label, "ProgramArguments": [str(binary), "_daemon"],
                "EnvironmentVariables": {"XDG_STATE_HOME": state}}
        path.write_bytes(plistlib.dumps(spec))
        with patch.object(DRIVER.Path, "home", return_value=self.root), patch.object(evidence, "run", side_effect=[(0, "", ""), (1, "", "Could not find service")]) as run:
            labels = DRIVER.service_cleanup(evidence, {state}, [binary])
        self.assertEqual(labels, [label])
        self.assertIn(label, run.call_args_list[0].args[0][-1])
        self.assertFalse(path.exists())

    def test_launchctl_permission_or_timeout_failure_is_not_proof_of_cleanup(self):
        self.assertTrue(DRIVER.unloaded(113, "Could not find service"))
        for status, error in ((0, "Could not find service"), (124, "Could not find service"),
                              (-9, "Could not find service"), (1, "Operation not permitted"), (1, "")):
            self.assertFalse(DRIVER.unloaded(status, error))

    @patch.object(DRIVER.os, "getuid", return_value=501, create=True)
    def test_service_cleanup_waits_for_sigtermed_job_to_become_absent(self, _uid):
        evidence = self.evidence("installer")
        label = "dev.koidrive.koi"
        path = self.service_plist(label)
        responses = [(0, "", ""), (0, "state = SIGTERMed\nexit timeout = 5", ""),
                     (113, "", "Could not find service")]
        with patch.object(DRIVER.Path, "home", return_value=self.root), patch.object(evidence, "run", side_effect=responses) as run, \
                patch.object(DRIVER.time, "monotonic", side_effect=[0, 0, 0.1, 0.1]), patch.object(DRIVER.time, "sleep") as sleep:
            labels = DRIVER.service_cleanup(evidence, {str(self.root / ".local/state")}, [self.root / ".local/bin/koi"])
        self.assertEqual(labels, [label])
        self.assertFalse(path.exists())
        self.assertEqual(run.call_count, 3)
        sleep.assert_called_once_with(0.25)

    @patch.object(DRIVER.os, "getuid", return_value=501, create=True)
    def test_service_cleanup_rejects_persistent_loaded_and_unknown_probe_failure(self, _uid):
        evidence = self.evidence("installer")
        for probe in ((0, "state = SIGTERMed", ""), (1, "", "Operation not permitted"), (124, "", "")):
            path = self.service_plist("dev.koidrive.koi")
            with self.subTest(probe=probe), patch.object(DRIVER.Path, "home", return_value=self.root), \
                    patch.object(evidence, "run", side_effect=[(0, "", ""), probe]), patch.object(DRIVER.time, "sleep") as sleep:
                with self.assertRaisesRegex(RuntimeError, "Could not prove owned service unloaded"):
                    DRIVER.service_cleanup(evidence, {str(self.root / ".local/state")}, [self.root / ".local/bin/koi"], timeout=0)
            self.assertTrue(path.exists())
            sleep.assert_not_called()

    @patch.object(DRIVER.os, "getuid", return_value=501, create=True)
    def test_service_cleanup_validates_every_plist_before_any_bootout(self, _uid):
        evidence = self.evidence("installer")
        peer = self.service_plist("dev.koidrive.koi.1234abcd")
        primary = self.service_plist("dev.koidrive.koi", binary=Path("/foreign/koi"))
        with patch.object(DRIVER.Path, "home", return_value=self.root), patch.object(evidence, "run") as run:
            with self.assertRaisesRegex(RuntimeError, "foreign launch agent"):
                DRIVER.service_cleanup(evidence, {str(self.root / ".local/state")}, [self.root / ".local/bin/koi"])
        run.assert_not_called()
        self.assertTrue(peer.exists() and primary.exists())

    @patch.object(DRIVER.os, "getuid", return_value=501, create=True)
    def test_service_cleanup_attempts_primary_after_peer_failure_and_aggregates(self, _uid):
        evidence = self.evidence("installer")
        peer_label = "dev.koidrive.koi.1234abcd"
        primary_label = "dev.koidrive.koi"
        peer = self.service_plist(peer_label)
        primary = self.service_plist(primary_label)
        responses = [(0, "", ""), (0, "state = SIGTERMed", ""),
                     (0, "", ""), (113, "", "Could not find service")]
        with patch.object(DRIVER.Path, "home", return_value=self.root), patch.object(evidence, "run", side_effect=responses) as run:
            with self.assertRaisesRegex(RuntimeError, re.escape(peer_label)):
                DRIVER.service_cleanup(evidence, {str(self.root / ".local/state")}, [self.root / ".local/bin/koi"], timeout=0)
        self.assertTrue(peer.exists())
        self.assertFalse(primary.exists())
        self.assertEqual([call.args[0][1:] for call in run.call_args_list if call.args[0][1] == "bootout"],
                         [["bootout", "gui/501/" + peer_label], ["bootout", "gui/501/" + primary_label]])

    @patch.object(DRIVER.os, "getuid", return_value=501, create=True)
    def test_service_cleanup_reports_every_owned_service_failure(self, _uid):
        evidence = self.evidence("installer")
        labels = ["dev.koidrive.koi.1234abcd", "dev.koidrive.koi"]
        paths = [self.service_plist(label) for label in labels]
        responses = [(0, "", ""), (1, "", "permission denied"),
                     (0, "", ""), (0, "state = SIGTERMed", "")]
        with patch.object(DRIVER.Path, "home", return_value=self.root), patch.object(evidence, "run", side_effect=responses):
            with self.assertRaises(RuntimeError) as caught:
                DRIVER.service_cleanup(evidence, {str(self.root / ".local/state")}, [self.root / ".local/bin/koi"], timeout=0)
        self.assertTrue(all(label in str(caught.exception) for label in labels))
        self.assertTrue(all(path.exists() for path in paths))

    def test_byte_sync_passes_only_when_each_file_arrives_with_its_bytes(self):
        with tempfile.TemporaryDirectory() as tmp:
            folder, peer = Path(tmp) / "a", Path(tmp) / "b"
            folder.mkdir()
            peer.mkdir()

            def copying(predicate, timeout=60):
                for source, destination in ((folder, peer), (peer, folder)):
                    for path in source.glob("*.bin"):
                        if not (destination / path.name).exists():
                            (destination / path.name).write_bytes(path.read_bytes())
                return predicate()

            with patch.object(DRIVER, "until", copying):
                transfers = DRIVER.byte_sync(folder, peer, "before-")
            self.assertEqual([t["name"] for t in transfers], ["before-outbound.bin", "before-inbound.bin"])
            self.assertTrue(all(t["passed"] for t in transfers))
            self.assertTrue(DRIVER.preserved(transfers, folder, peer))
            with patch.object(DRIVER, "until", lambda predicate, timeout=60: predicate()):
                stalled = DRIVER.byte_sync(folder, peer, "after-")
            self.assertEqual([t["passed"] for t in stalled], [False, False])

    def test_preserved_fails_for_changed_or_missing_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            folder, peer = Path(tmp) / "a", Path(tmp) / "b"
            folder.mkdir()
            peer.mkdir()
            (folder / "kept.bin").write_bytes(b"kept")
            (peer / "kept.bin").write_bytes(b"kept")
            transfers = [{"name": "kept.bin", "sha256": DRIVER.sha256(folder / "kept.bin"), "passed": True}]
            self.assertTrue(DRIVER.preserved(transfers, folder, peer))
            (peer / "kept.bin").write_bytes(b"changed")
            self.assertFalse(DRIVER.preserved(transfers, folder, peer))
            (peer / "kept.bin").unlink()
            self.assertFalse(DRIVER.preserved(transfers, folder, peer))


if __name__ == "__main__":
    unittest.main(verbosity=2)
