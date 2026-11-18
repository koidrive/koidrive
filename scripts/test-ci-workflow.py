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
SELECTED = ("rust", "rust_lint", "rust_driver", "rust_engine", "rust_sim", "server", "server_deploy", "net_integration",
            "driver_integration", "cli_integration", "ffi_integration", "test_lines")


def inline_script(job):
    body = WORKFLOW.read_text().split(f"\n  {job}:\n", 1)[1]
    body = re.split(r"\n  \S", body, maxsplit=1)[0]
    block = re.search(r"(?m)^        run: \|\n((?:          .*\n|\n)+)", body)
    return textwrap.dedent(block[1])


def inline_job(job):
    body = WORKFLOW.read_text().split(f"\n  {job}:\n", 1)[1]
    return re.split(r"\n  \S", body, maxsplit=1)[0]


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

    def push_with_runs(self, runs):
        (self.dir / "fixtures").write_text(json.dumps({
            "workflows/ci.yml/runs?event=merge_group&head_sha=fixture-sha&status=success&per_page=1": {"workflow_runs": runs},
        }))
        (self.dir / "requests").write_text("")
        result, outputs = self.run_script(inline_script("changes"), {
            "EVENT": "push", "GITHUB_SHA": "fixture-sha", "GITHUB_REPOSITORY": "koidrive/koidrive",
            "REQUESTS": str(self.dir / "requests"), "FIXTURES": str(self.dir / "fixtures"),
        })
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return outputs

    def test_push_skips_every_macos_job_a_green_merge_queue_run_ran_or_did_not_need(self):
        outputs = self.push_with_runs([{"id": 1, "html_url": "https://fixture.invalid/run", "head_sha": "fixture-sha",
                                        "event": "merge_group", "conclusion": "success"}])
        for job in SELECTED:
            self.assertEqual(outputs[job], "true", job)
        for job in ("rust_macos", "server_macos", "spec_macos", "macos_app"):
            self.assertEqual(outputs[job], "covered", job)

    def test_push_runs_every_macos_job_when_the_returned_run_is_not_a_green_queue_run_of_this_commit(self):
        green = {"id": 1, "html_url": "https://fixture.invalid/run", "head_sha": "fixture-sha", "event": "merge_group", "conclusion": "success"}
        for field, value in (("head_sha", "other-sha"), ("event", "push"), ("conclusion", "failure")):
            with self.subTest(field=field):
                outputs = self.push_with_runs([{**green, field: value}])
                for job in ("rust_macos", "server_macos", "spec_macos", "macos_app"):
                    self.assertEqual(outputs[job], "true", job)

    def test_push_runs_every_macos_job_when_the_merge_queue_lookup_fails(self):
        (self.dir / "fixtures").write_text(json.dumps({
            "workflows/ci.yml/runs?event=merge_group&head_sha=fixture-sha&status=success&per_page=1": None,
        }))
        (self.dir / "requests").write_text("")
        result, outputs = self.run_script(inline_script("changes"), {
            "EVENT": "push", "GITHUB_SHA": "fixture-sha", "GITHUB_REPOSITORY": "koidrive/koidrive",
            "REQUESTS": str(self.dir / "requests"), "FIXTURES": str(self.dir / "fixtures"),
        })
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        for job in ("rust_macos", "server_macos", "spec_macos", "macos_app"):
            self.assertEqual(outputs[job], "true", job)

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
        for job in ("rust_macos", "server_macos", "spec_macos", "macos_app"):
            self.assertEqual(outputs[job], "true", job)

    def test_scheduled_and_manual_runs_run_every_job_on_both_systems(self):
        for event in ("schedule", "workflow_dispatch"):
            with self.subTest(event=event):
                result, outputs = self.run_script(inline_script("changes"), {"EVENT": event})
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                for job in (*SELECTED, "rust_macos", "server_macos", "spec_macos", "macos_app"):
                    self.assertEqual(outputs[job], "true", job)
                self.assertEqual(outputs["site_only"], "false")

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
                self.assertEqual(outputs["macos_app"], "false")
        result, outputs = self.run_changes("docs/deploy.md")
        for job in SELECTED:
            self.assertEqual(outputs[job], "false", job)

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
                if event == "merge_group":
                    self.assertEqual(outputs["macos_app"], "true")

    def test_a_failed_selector_build_reaches_the_decision_step(self):
        body = WORKFLOW.read_text().split("\n  changes:\n", 1)[1]
        build = re.search(r"(?m)^      - name: Build the job selector\n((?:        .*\n)+)", body)[1]
        self.assertIn("id: selector\n", build)
        self.assertIn("continue-on-error: true\n", build)
        self.assertIn("SELECTOR_BUILD: ${{ steps.selector.outcome }}", body)


class Caches(unittest.TestCase):
    FROM_MAIN = "github.ref == 'refs/heads/main'"
    FILES = sorted([*(ROOT / ".github/workflows").glob("*.yml"), *(ROOT / ".github/actions").glob("*/action.yml")])

    def steps(self, text):
        found = []
        for indent, first, rest in re.findall(r"(?m)^( *)- (.*)\n((?:\1  .*\n)*)", text):
            step = f"{indent}  {first}\n{rest}"
            uses = re.search(rf"(?m)^{indent}  uses: (\S+)", step)
            if uses:
                cond = re.search(rf"(?m)^{indent}  if: (.*)$", step)
                found.append((indent, cond and cond[1], uses[1], step))
        return found

    def test_only_runs_on_main_save_caches(self):
        for path in self.FILES:
            for _, cond, action, body in self.steps(path.read_text()):
                with self.subTest(file=path.relative_to(ROOT).as_posix(), action=action):
                    if action.startswith("Swatinem/rust-cache@"):
                        saves = re.findall(r"(?m)^ *save-if: (.*)$", body)
                        self.assertIn(saves, ([f"${{{{ {self.FROM_MAIN} }}}}"], ["false"]))
                    elif action.startswith(("actions/cache@", "actions/cache/save@")):
                        self.assertEqual(cond, self.FROM_MAIN)
                    elif action.startswith("actions/setup-"):
                        self.assertNotRegex(body, r"(?m)^ *cache:")
                    else:
                        self.assertTrue(action.endswith("/.github/actions/pnpm-store") or action.startswith("actions/cache/restore@")
                                        or "cache" not in action.lower(), "unreviewed cache action")

    def test_rust_caches_change_with_the_workspace_profiles(self):
        for path in self.FILES:
            for _, _, action, body in self.steps(path.read_text()):
                if not action.startswith("Swatinem/rust-cache@"):
                    continue
                workspace = re.search(r"(?m)^ *workspaces: (\S+)", body)
                if workspace and workspace[1].startswith("scripts/"):
                    continue
                manifest = f"{workspace[1]}/Cargo.toml" if workspace else "Cargo.toml"
                with self.subTest(file=path.relative_to(ROOT).as_posix(), body=body):
                    field = "shared-key" if re.search(r"(?m)^ *shared-key: ", body) else "key"
                    key = re.search(rf"(?m)^ *{field}: (.*)$", body)
                    self.assertIsNotNone(key, "rust-cache ignores profile changes, so its key must hash the manifest")
                    self.assertIn(f"hashFiles('{manifest}')", key[1])

    def test_a_branch_run_restores_the_cache_a_main_run_saves(self):
        text = (ROOT / ".github/actions/pnpm-store/action.yml").read_text()
        caches = {action.split("@")[0]: (cond, body) for _, cond, action, body in self.steps(text)}
        self.assertEqual(caches["actions/cache"][0], self.FROM_MAIN)
        self.assertEqual(caches["actions/cache/restore"][0], "github.ref != 'refs/heads/main'")
        inputs = {name: body.split("  with:\n", 1)[1] for name, (_, body) in caches.items()}
        self.assertEqual(inputs["actions/cache"], inputs["actions/cache/restore"])
        self.assertIn("restore-keys:", inputs["actions/cache"])

    def test_local_actions_come_from_the_workflow_commit(self):
        for path in sorted((ROOT / ".github/workflows").glob("*.yml")):
            steps = self.steps(path.read_text())
            checkouts = [body for _, _, a, body in steps if a.startswith("actions/checkout@")]
            root_moves = any(re.search(r"(?m)^ *ref: ", body) and not re.search(r"(?m)^ *path: ", body)
                             and "ref: ${{ github.sha }}" not in body for body in checkouts)
            harness = {}
            for body in checkouts:
                where = re.search(r"(?m)^ *path: (\S+)$", body)
                if where and "ref: ${{ github.sha }}" in body:
                    sparse = body.split("sparse-checkout:", 1)[1] if "sparse-checkout:" in body else ""
                    harness[where[1]] = [p.strip() for p in sparse.replace("|", "").split("\n") if p.strip()] or None
            for _, _, action, _ in steps:
                if not action.startswith("./") or not root_moves:
                    continue
                with self.subTest(workflow=path.name, action=action):
                    top, _, rest = action[2:].partition("/")
                    self.assertIn(top, harness, "a local action resolves from the checkout under test, which may predate it")
                    self.assertTrue(harness[top] is None or any(rest.startswith(p) for p in harness[top]))

    def test_workflows_that_restore_on_pull_requests_run_on_main_every_day(self):
        for path in sorted((ROOT / ".github/workflows").glob("*.yml")):
            text = path.read_text()
            triggers = text.split("\non:\n", 1)[1].split("\n\n", 1)[0]
            cached = any(a.startswith("Swatinem/rust-cache@") or a.endswith("/.github/actions/pnpm-store") for _, _, a, _ in self.steps(text))
            if not cached or "  pull_request:" not in triggers:
                continue
            with self.subTest(workflow=path.name):
                self.assertRegex(triggers, r"\n  schedule:\n    - cron: '\d+ \d+ \* \* \*'")


class RustShards(unittest.TestCase):
    def run_step(self, **env):
        body = WORKFLOW.read_text().split("\n      - name: Plan the Rust test archive and its shards\n", 1)[1]
        script = textwrap.dedent(re.search(r"(?m)^        run: \|\n((?:          .*\n|\n)+)", body)[1])
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp) / "output"
            output.write_text("")
            result = subprocess.run(["bash", "--noprofile", "--norc", "-e", "-o", "pipefail", "-c", script],
                                    env={**os.environ, "GITHUB_OUTPUT": str(output), **env}, capture_output=True, text=True, timeout=15)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            return dict(line.split("=", 1) for line in output.read_text().splitlines())

    def test_a_selected_partition_builds_the_archive_and_shards_it_on_each_system(self):
        outputs = self.run_step(RUST="false", RUST_DRIVER="false", RUST_ENGINE="true")
        self.assertEqual(outputs["rust_archive"], "true")
        for system, os_name, count in (("linux", "ubuntu-latest", 5), ("macos", "macos", 6)):
            shards = json.loads(outputs[f"rust_shards_{system}"])["include"]
            self.assertEqual(shards, [{"os": os_name, "shard": shard, "of": count} for shard in range(1, count + 1)])

    def test_no_archive_without_a_selected_partition(self):
        outputs = self.run_step(RUST="false", RUST_DRIVER="false", RUST_ENGINE="false")
        self.assertEqual(outputs["rust_archive"], "false")

    def test_each_shard_result_keeps_its_own_folder_however_many_shards_upload(self):
        workflow = WORKFLOW.read_text()
        self.assertIn('cp "target/nextest/shard-$SHARD/junit.xml" "$RUNNER_TEMP/junit/rust-junit-$OS-$SHARD/"', workflow)
        self.assertIn("name: rust-junit-${{ env.OS }}-${{ matrix.shard }}\n          path: ${{ runner.temp }}/junit\n", workflow)
        self.assertIn("pattern: rust-junit-*\n          path: ${{ runner.temp }}/junit\n          merge-multiple: true\n", workflow)

class CiReport(unittest.TestCase):
    def test_a_job_that_skips_on_push_is_not_needed_on_push(self):
        jobs = re.split(r"(?m)^  (?=[a-z][a-z-]*:\n)", WORKFLOW.read_text().split("\njobs:\n", 1)[1])
        push_skipped = {job.split(":", 1)[0] for job in jobs if re.search(r"\n    if: [^\n]*github\.event_name\s*!=\s*'push'", job)}
        self.assertIn("test-lines", push_skipped)
        results = re.search(r"(?m)^          RESULTS: (.*)$", inline_job("ci-report"))[1]
        entries = re.findall(r"([a-z-]+)=(\$\{\{.*?\}\})", results)
        self.assertIn("test-lines", dict(entries))
        for name, expression in entries:
            if name in push_skipped:
                with self.subTest(job=name):
                    self.assertIn("github.event_name != 'push' &&", expression)
                    self.assertIn("|| 'not-needed'", expression)


class Runners(unittest.TestCase):
    def test_no_workflow_targets_self_hosted_runners(self):
        for path in sorted((ROOT / ".github/workflows").glob("*.yml")):
            with self.subTest(workflow=path.name):
                self.assertNotRegex(path.read_text().lower(), r"self-hosted|flarebox-mini|macos_runs_on")


if __name__ == "__main__":
    unittest.main()
