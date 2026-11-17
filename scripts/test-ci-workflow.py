#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
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

    def test_workflow_changes_run_server_and_spec_but_ordinary_paths_skip(self):
        self.write_executable("git", """
            import os
            import sys
            if sys.argv[1:] == ['rev-parse', 'HEAD^1']:
                print('fixture-base')
            elif sys.argv[1:] == ['diff', '-z', '--name-only', '--no-renames', 'fixture-base', 'HEAD']:
                sys.stdout.write(os.environ['CHANGED'] + '\\0')
            else:
                sys.exit('Unexpected git call')
        """)
        scripts = self.dir / "scripts"
        scripts.mkdir()
        for name in ("test-macos-changes.sh", "macos-changes.sh"):
            path = scripts / name
            path.write_text("#!/bin/bash\nexit 0\n")
            path.chmod(0o755)
        for changed in (".github/workflows/ci.yml", "server/src/index.ts", "spec/proto/api.proto", "site/index.html"):
            with self.subTest(changed=changed):
                result, outputs = self.run_script(inline_script("changes"), {"EVENT": "pull_request", "CHANGED": changed})
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                wanted = "true" if changed == ".github/workflows/ci.yml" else "false"
                self.assertEqual(outputs["server_macos"], wanted)
                self.assertEqual(outputs["spec_macos"], wanted)
                self.assertEqual(outputs["site_only"], "true" if changed.startswith("site/") else "false")


class Runners(unittest.TestCase):
    def test_no_workflow_targets_self_hosted_runners(self):
        for path in sorted((ROOT / ".github/workflows").glob("*.yml")):
            with self.subTest(workflow=path.name):
                self.assertNotRegex(path.read_text().lower(), r"self-hosted|flarebox-mini|macos_runs_on")


if __name__ == "__main__":
    unittest.main()
