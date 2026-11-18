#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

from contextlib import redirect_stderr, redirect_stdout
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
import unittest
from unittest.mock import patch


sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("stress_test_servers", ROOT / "scripts/stress-test-servers.py")
stress = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = stress
spec.loader.exec_module(stress)


class StressTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="stress-unit-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.probes = self.root / "probes"
        self.probes.mkdir()
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.executable(self.bin / "cargo", r'''
import json, os, sys, time
from pathlib import Path
probe = Path(os.environ['PROBES'])
seed = os.environ['PROPTEST_RNG_SEED']
record = dict(pid=os.getpid(), args=sys.argv[1:], cwd=os.getcwd(),
              seed=seed, ordinary=os.environ.get('PROPTEST_CASES'),
              cases=os.environ.get('KOIDRIVE_BATTERY_CASES'),
              server=os.environ.get('KOIDRIVE_TEST_SERVER'))
(probe / (seed + '.start')).write_text(json.dumps(record))
deadline = time.monotonic() + 5
while len(list(probe.glob('*.start'))) < int(os.environ.get('BARRIER', '1')):
    if time.monotonic() > deadline:
        sys.exit(24)
    time.sleep(0.01)
mode = os.environ.get('PROBE_MODE', 'pass')
if mode == 'hang':
    time.sleep(60)
print('     Running tests/probe.rs (target/debug/deps/probe)')
if mode == 'zero':
    print('test result: ok. 0 passed; 0 failed; 3 ignored; 0 measured; 0 filtered out')
elif mode == 'fail':
    print('test probe ... FAILED')
    print('test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out')
    sys.exit(101)
elif mode == 'startup':
    sys.exit(17)
else:
    print('test probe ... ok')
    print('test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out')
''')
        self.environment = dict(os.environ, PATH=str(self.bin) + os.pathsep + os.environ["PATH"],
                                PROBES=str(self.probes), PROPTEST_CASES="999", KOIDRIVE_BATTERY_CASES="888")

    def executable(self, path, code):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("#!" + sys.executable + "\n" + code)
        path.chmod(0o755)

    def args(self, *extra):
        return stress.parse_args(["--root", str(self.root), "--out", str(self.root / "out"),
                                  "--crate", "koidrive-net", "--seed", "17", *extra])

    def run_tests(self, args, **environment):
        with patch.dict(os.environ, {**self.environment, **environment}, clear=True), \
                patch.object(stress, "save_regressions"):
            return stress.run_tests(args)

    def worker_fixture(self, crate="koidrive-net"):
        (self.root / "Cargo.toml").write_text("[workspace.package]\nversion = '0.1.0'\n")
        (self.root / "scripts").mkdir(exist_ok=True)
        shutil.copyfile(ROOT / "scripts/worker_ready.py", self.root / "scripts/worker_ready.py")
        source = Path(stress.WORKERS[crate][0])
        (self.root / source).parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(ROOT / source, self.root / source)
        (self.root / "server/.wrangler").mkdir(parents=True, exist_ok=True)
        (self.root / "server/.wrangler/stale").write_text("old state")
        (self.root / "server/wrangler.jsonc").write_text('''// license
{
  "name": "copied-target",
  "main": "src/worker.ts",
  "vars": {"URL": "http://example.com/a//b",},
  /* custom build */ "build": {"command": "pnpm build:web"},
}
''')
        (self.root / "server/web/node_modules").mkdir(parents=True, exist_ok=True)
        (self.root / "server/web/dist").mkdir(exist_ok=True)
        (self.root / "server/web/dist/prebuilt").write_text("built once")
        self.executable(self.root / "server/node_modules/.bin/wrangler", r'''
import json, os, signal, socket, subprocess, sys, time
from pathlib import Path
args = sys.argv[1:]
state = Path(args[args.index('--persist-to') + 1])
port = int(args[args.index('--port') + 1])
server = Path.cwd()
assert not (server / '.wrangler/stale').exists()
config = json.loads((server / 'wrangler.jsonc').read_text())
assert 'command' not in config['build']
assert config['main'] == 'src/worker.ts'
assert config['vars']['URL'] == 'http://example.com/a//b'
web = server / 'web/dist'
assert (web / 'prebuilt').read_text() == 'built once'
(web / 'build').write_text(str(os.getpid()))
listener = socket.socket()
listener.bind(('127.0.0.1', port))
listener.listen()
state.mkdir(parents=True)
(state / 'new').write_text('fresh state')
child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])
def stop(*_):
    child.wait(timeout=5)
    sys.exit(0)
signal.signal(signal.SIGTERM, stop)
record = dict(server=str(server), state=str(state), port=listener.getsockname()[1],
              web=str(web / 'build'), pid=os.getpid(), child=child.pid)
(Path(os.environ['PROBES']) / (os.environ['PROPTEST_RNG_SEED'] + '.worker')).write_text(json.dumps(record))
print('[wrangler:info] Ready on http://127.0.0.1:' + str(listener.getsockname()[1]), flush=True)
while True:
    time.sleep(1)
''')

    def test_parallel_bounds_and_worker_selection(self):
        for extra in (("--parallel", "0"), ("--parallel", "9"), ("--repeat", "101"),
                      ("--mode", "worker", "--test", "wrong"), ("--filter=-option",)):
            with self.subTest(extra=extra), redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                self.args(*extra)
        self.assertEqual(self.args("--mode", "worker").test, "server")
        self.assertEqual(self.args("--mode", "worker", "--crate", "koidrive-driver").test, "sync")

    def test_real_processes_overlap_and_forward_selection_and_budgets(self):
        args = self.args("--repeat", "3", "--parallel", "3", "--test", "send",
                         "--filter", "two words", "--features", "serde", "--cases", "0", "--ignored", "true")
        records = self.run_tests(args, BARRIER="3")
        self.assertEqual([row["result"] for row in records], ["pass"] * 3)
        probes = [json.loads(path.read_text()) for path in self.probes.glob("*.start")]
        self.assertEqual(len({probe["pid"] for probe in probes}), 3)
        self.assertEqual({probe["seed"] for probe in probes}, {"17", "18", "19"})
        for probe in probes:
            self.assertIsNone(probe["ordinary"])
            self.assertEqual(probe["cases"], "0")
            self.assertEqual(probe["args"], stress.cargo_command(args)[1:])
            self.assertIn("two words", probe["args"])
            self.assertNotIn("--no-run", probe["args"])

    def test_optional_budget_is_unset_and_zero_tests_and_startup_fail_closed(self):
        for mode, expected, code in (("pass", "pass", 0), ("zero", "error", 0),
                                     ("startup", "error", 17), ("fail", "fail", 101)):
            with self.subTest(mode=mode):
                record = self.run_tests(self.args(), PROBE_MODE=mode)[0]
                self.assertEqual(record["result"], expected)
                self.assertEqual(record["exit_code"], code)
                probe = json.loads((self.probes / "17.start").read_text())
                self.assertIsNone(probe["cases"])
                self.assertTrue((self.root / "out/run-1.json").exists())

    def test_partial_results_do_not_pass(self):
        text = ("Running one\nRunning two\n"
                "test result: ok. 2 passed; 0 failed; 0 ignored\n")
        self.assertEqual(stress.result(1, 17, 0, text, 0)["result"], "error")
        self.assertEqual(stress.result(1, 17, 17, text, 0)["result"], "error")

    def test_worker_repetitions_use_target_launcher_and_isolate_builds_and_state(self):
        for crate in stress.WORKERS:
            with self.subTest(crate=crate):
                for path in self.probes.iterdir():
                    path.unlink()
                self.worker_fixture(crate)
                args = self.args("--mode", "worker", "--crate", crate, "--repeat", "3", "--parallel", "3",
                                 "--features", "serde", "--filter", "selected test")
                records = self.run_tests(args, BARRIER="3", KOIDRIVE_TEST_PORT="1234")
                self.assertEqual([row["result"] for row in records], ["pass"] * 3)
                workers = [json.loads(path.read_text()) for path in self.probes.glob("*.worker")]
                for field in ("server", "state", "port", "web"):
                    self.assertEqual(len({worker[field] for worker in workers}), 3)
                self.assertFalse((self.root / "server/web/dist/build").exists())
                self.assertIn('"command": "pnpm build:web"', (self.root / "server/wrangler.jsonc").read_text())
                for worker in workers:
                    self.assertFalse(Path(worker["server"]).exists())
                    self.assertFalse(Path(worker["state"]).exists())
                for path in self.probes.glob("*.start"):
                    probe = json.loads(path.read_text())
                    self.assertEqual(probe["cwd"], str(self.root))
                    self.assertIn("--ignored", probe["args"])
                    self.assertEqual(probe["args"][-2:], ["--ignored", "selected test"])
                    self.assertEqual(probe["args"][probe["args"].index("--features") + 1], "serde")
                    self.assertEqual(probe["args"][probe["args"].index("--test") + 1], stress.WORKERS[crate][1])
                    self.assertIn("selected test", probe["args"])
                    self.assertIn("--test", probe["args"])

    def test_timeout_kills_processes(self):
        record = self.run_tests(self.args("--timeout-seconds", "1"), PROBE_MODE="hang")[0]
        self.assertEqual(record["result"], "timeout")
        pid = json.loads((self.probes / "17.start").read_text())["pid"]
        with self.assertRaises(ProcessLookupError):
            os.kill(pid, 0)

    def test_worker_timeout_kills_nested_process_groups(self):
        self.worker_fixture()
        record = self.run_tests(self.args("--mode", "worker", "--timeout-seconds", "3"), PROBE_MODE="hang")[0]
        self.assertEqual(record["result"], "timeout")
        worker = json.loads((self.probes / "17.worker").read_text())
        for pid in (worker["pid"], worker["child"]):
            with self.subTest(pid=pid), self.assertRaises(ProcessLookupError):
                os.kill(pid, 0)
        self.assertFalse(Path(worker["server"]).exists())
        self.assertFalse(Path(worker["state"]).exists())

    def test_zero_tests_fail_main_and_write_artifacts(self):
        record = stress.result(1, 17, 0, "test result: ok. 0 passed; 0 failed; 2 ignored\n", 0)
        (self.root / "out").mkdir()
        with patch.object(stress, "run_tests", return_value=[record]), \
                patch.object(stress.subprocess, "check_output", return_value="target-sha\n"), \
                redirect_stdout(io.StringIO()):
            self.assertEqual(stress.main(["--root", str(self.root), "--out", str(self.root / "out"),
                                          "--crate", "koidrive-net", "--seed", "17"]), 1)
        self.assertEqual(json.loads((self.root / "out/results.json").read_text())[0]["result"], "error")
        self.assertIn("0 of 1 runs passed", (self.root / "out/summary.md").read_text())

    def test_workflow_keeps_serial_contract_and_pins_dispatch_harness(self):
        workflow = (ROOT / ".github/workflows/heavy-tests.yml").read_text()
        self.assertIn("ref: ${{ github.sha }}\n          path: .heavy-harness", workflow)
        self.assertIn("ref: ${{ needs.plan.outputs.sha }}", workflow)
        self.assertIn("if: env.MODE == 'cargo' && env.PARALLEL == '1'", workflow)
        self.assertIn("${TEST:+--test", workflow)
        ci = (ROOT / ".github/workflows/ci.yml").read_text()
        for line in workflow.splitlines():
            if "uses:" in line:
                action = line.split("uses:")[1].split(" #")[0].strip()
                if action.startswith("./.heavy-harness/"):
                    self.assertTrue((ROOT / action.removeprefix("./.heavy-harness/") / "action.yml").is_file(), action)
                else:
                    self.assertIn(action, ci)


if __name__ == "__main__":
    unittest.main(verbosity=2)
