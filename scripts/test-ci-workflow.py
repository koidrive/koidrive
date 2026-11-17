#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
import itertools
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import textwrap
import unittest

ROOT = Path(__file__).resolve().parent.parent
WORKFLOW = ROOT / ".github/workflows/ci.yml"
SELECTED = ("rust", "rust_driver", "rust_engine", "rust_sim", "server", "server_deploy", "net_integration",
            "driver_integration", "cli_integration", "ffi_integration")


def inline_script(job):
    body = WORKFLOW.read_text().split(f"\n  {job}:\n", 1)[1]
    body = re.split(r"\n  \S", body, maxsplit=1)[0]
    block = re.search(r"(?m)^        run: \|\n((?:          .*\n|\n)+)", body)
    return textwrap.dedent(block[1])


class ChangesJob(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix=".test-ci-workflow-", dir=ROOT)
        self.addCleanup(self.temp.cleanup)
        self.dir = Path(self.temp.name)
        self.bin = self.dir / "bin"
        self.bin.mkdir()
        self.write_executable("curl", """
            import json
            import os
            from pathlib import Path
            import sys
            url = sys.argv[-1].split('/actions/', 1)[1]
            with open(os.environ['REQUESTS'], 'a') as log:
                log.write(url + '\\n')
            fixtures = json.loads(Path(os.environ['FIXTURES']).read_text())
            if url not in fixtures:
                sys.exit('Unexpected API request: ' + url)
            if fixtures[url] is None:
                sys.exit('Fixture API failure: ' + url)
            print(json.dumps(fixtures[url]))
        """)

    def write_executable(self, name, source):
        path = self.bin / name
        path.write_text(f"#!{sys.executable}\n" + textwrap.dedent(source))
        path.chmod(0o755)

    def run_script(self, script, env):
        output = self.dir / "output"
        output.write_text("")
        result = subprocess.run(
            ["bash", "--noprofile", "--norc", "-e", "-o", "pipefail", "-c", script],
            cwd=self.dir,
            env={**os.environ, "PATH": f"{self.bin}:{os.environ['PATH']}",
                 "GITHUB_OUTPUT": str(output), "GITHUB_STEP_SUMMARY": str(self.dir / "summary"),
                 "GITHUB_API_URL": "https://fixture.invalid", "GITHUB_TOKEN": "fixture-token",
                 "RUNNER_TEMP": str(self.dir), **env},
            capture_output=True, text=True, timeout=15,
        )
        return result, dict(line.split("=", 1) for line in output.read_text().splitlines())

    def test_push_reuses_rust_only_when_all_macos_shards_pass(self):
        names = ("rust (macos)", "rust tests (macos, driver)", "rust tests (macos, engine)", "rust tests (macos, sim)")
        for missing in (None, *names):
            with self.subTest(missing=missing):
                fixtures = {
                    "workflows/ci.yml/runs?event=merge_group&head_sha=fixture-sha&status=success&per_page=1": {
                        "workflow_runs": [{"id": 1, "html_url": "https://fixture.invalid/run"}]},
                    "runs/1/jobs?filter=latest&per_page=100": {
                        "jobs": [{"name": name, "conclusion": "success"} for name in names if name != missing]},
                }
                (self.dir / "fixtures").write_text(json.dumps(fixtures))
                (self.dir / "requests").write_text("")
                result, outputs = self.run_script(inline_script("changes"), {
                    "EVENT": "push", "GITHUB_SHA": "fixture-sha", "GITHUB_REPOSITORY": "koidrive/koidrive",
                    "REQUESTS": str(self.dir / "requests"), "FIXTURES": str(self.dir / "fixtures"),
                })
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(outputs["rust_macos"], "covered" if missing is None else "true")

    def test_push_runs_every_linux_job(self):
        (self.dir / "fixtures").write_text(json.dumps({
            "workflows/ci.yml/runs?event=merge_group&head_sha=fixture-sha&status=success&per_page=1": {"workflow_runs": []},
        }))
        (self.dir / "requests").write_text("")
        result, outputs = self.run_script(inline_script("changes"), {
            "EVENT": "push", "GITHUB_SHA": "fixture-sha", "GITHUB_REPOSITORY": "koidrive/koidrive",
            "REQUESTS": str(self.dir / "requests"), "FIXTURES": str(self.dir / "fixtures"),
        })
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        for job in SELECTED:
            self.assertEqual(outputs[job], "true", job)
        self.assertEqual(json.loads(outputs["rust_suites"]), ["driver", "engine", "sim"])
        for job in ("rust_macos", "server_macos", "spec_macos", "macos_app"):
            self.assertEqual(outputs[job], "true", job)

    def run_changes(self, changed, selected=(), event="pull_request", selector_fails=False, selector_build="success"):
        self.write_executable("git", """
            import os
            import sys
            if sys.argv[1:] == ['rev-parse', 'HEAD^1']:
                print('fixture-base')
            elif sys.argv[1:] == ['diff', '-z', '--name-only', '--no-renames', 'fixture-base', 'HEAD']:
                sys.stdout.write(os.environ['CHANGED'] + '\\0')
            elif sys.argv[1:] == ['fetch', '--no-tags', '--depth=1', 'origin', 'fixture-base']:
                pass
            elif sys.argv[1:] == ['worktree', 'add', '--detach', os.environ['RUNNER_TEMP'] + '/base', 'fixture-base']:
                pass
            else:
                sys.exit('Unexpected git call: ' + ' '.join(sys.argv[1:]))
        """)
        scripts = self.dir / "scripts"
        scripts.mkdir(exist_ok=True)
        for name in ("test-macos-changes.sh", "macos-changes.sh"):
            path = scripts / name
            path.write_text("#!/bin/bash\nexit 0\n")
            path.chmod(0o755)
        selector = scripts / "ci-select/target/debug/ci-select"
        selector.parent.mkdir(parents=True, exist_ok=True)
        lines = "".join(f"{job}={'true' if job in selected else 'false'}\\n" for job in (*SELECTED, "macos_app"))
        selector.write_text("#!/bin/bash\n" + ("echo 'selector broke' >&2; exit 1\n" if selector_fails else
                            f'[ "$1 $2 $3" = ".github/ci-select.toml $RUNNER_TEMP/base ." ] || exit 9\nprintf "{lines}"\n'))
        selector.chmod(0o755)
        return self.run_script(inline_script("changes"), {
            "EVENT": event, "CHANGED": changed, "BASE": "fixture-base", "SELECTOR_BUILD": selector_build,
        })

    def test_workflow_changes_run_server_and_spec_but_ordinary_paths_skip(self):
        for changed in (".github/workflows/ci.yml", "server/src/index.ts", "spec/proto/api.proto", "site/index.html"):
            with self.subTest(changed=changed):
                result, outputs = self.run_changes(changed)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                wanted = "true" if changed == ".github/workflows/ci.yml" else "false"
                self.assertEqual(outputs["server_macos"], wanted)
                self.assertEqual(outputs["spec_macos"], wanted)
                self.assertEqual(outputs["site_only"], "true" if changed.startswith("site/") else "false")

    def test_the_selection_becomes_the_job_outputs(self):
        for event in ("pull_request", "merge_group"):
            with self.subTest(event=event):
                result, outputs = self.run_changes("core/sim/tests/devices.rs", ("rust_engine", "rust_sim", "server"), event)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                for job in SELECTED:
                    self.assertEqual(outputs[job], "true" if job in ("rust_engine", "rust_sim", "server") else "false", job)
                self.assertEqual(json.loads(outputs["rust_suites"]), ["engine", "sim"])
                self.assertEqual(outputs["macos_app"], "false")
        result, outputs = self.run_changes("docs/deploy.md")
        self.assertEqual(json.loads(outputs["rust_suites"]), [])

    def test_the_merge_queue_runs_the_app_only_when_selected(self):
        for selected, expected in (((), "false"), (("macos_app",), "true")):
            with self.subTest(selected=selected):
                result, outputs = self.run_changes("apps/macos/Koidrive/App.swift", selected, "merge_group")
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(outputs["macos_app"], expected)
                self.assertEqual(outputs["rust_macos"], "true")

    def test_a_selector_failure_runs_every_job(self):
        for event, failure in itertools.product(("pull_request", "merge_group"), ("run", "build")):
            with self.subTest(event=event, failure=failure):
                result, outputs = self.run_changes(
                    "docs/deploy.md", event=event, selector_fails=failure == "run",
                    selector_build="failure" if failure == "build" else "success")
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn("The job selector failed", result.stdout)
                for job in SELECTED:
                    self.assertEqual(outputs[job], "true", job)
                self.assertEqual(json.loads(outputs["rust_suites"]), ["driver", "engine", "sim"])
                if event == "merge_group":
                    self.assertEqual(outputs["macos_app"], "true")

    def test_a_failed_selector_build_reaches_the_decision_step(self):
        body = WORKFLOW.read_text().split("\n  changes:\n", 1)[1]
        build = re.search(r"(?m)^      - name: Build the job selector\n((?:        .*\n)+)", body)[1]
        self.assertIn("id: selector\n", build)
        self.assertIn("continue-on-error: true\n", build)
        self.assertIn("SELECTOR_BUILD: ${{ steps.selector.outcome }}", body)


class Runners(unittest.TestCase):
    def test_no_workflow_targets_self_hosted_runners(self):
        for path in sorted((ROOT / ".github/workflows").glob("*.yml")):
            with self.subTest(workflow=path.name):
                self.assertNotRegex(path.read_text().lower(), r"self-hosted|flarebox-mini|macos_runs_on")


if __name__ == "__main__":
    unittest.main()
