#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

from contextlib import redirect_stderr, redirect_stdout
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
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
import os, subprocess, sys
from pathlib import Path
probes = Path(os.environ['PROBES'])
if 'STRESS_TEST_CAPTURE' not in os.environ:
    (probes / 'cargo.called').write_text(' '.join(sys.argv[1:]))
    sys.exit(99)
(probes / 'cargo.capture').write_text(' '.join(sys.argv[1:]))
(probes / 'cargo.fallback').write_text(os.environ.get('FALLBACK_ENV', 'unset'))
runner = os.environ['CARGO_TARGET_AARCH64_APPLE_DARWIN_RUNNER']
package = os.environ['FAKE_PACKAGE']
environment = dict(os.environ, CARGO_MANIFEST_DIR=package, CONFIG_ENV='from-config',
                   KOIDRIVE_TEST_SERVER_LOG='from-config')
for name in ('PROPTEST_RNG_SEED', 'FALLBACK_ENV', 'KOIDRIVE_TEST_SERVER'):
    environment.setdefault(name, '42' if name == 'PROPTEST_RNG_SEED' else 'from-config')
for test in filter(None, os.environ['FAKE_TESTS'].split(os.pathsep)):
    subprocess.check_call([runner, test], cwd=package, env=environment)
    capture = Path(os.environ['STRESS_TEST_CAPTURE'])
    if capture.exists():
        (probes / 'capture.jsonl').write_text(capture.read_text())
''')
        self.executable(self.bin / "rustc", "print('rustc 1.99.0\\nhost: aarch64-apple-darwin')\n")
        (self.root / ".cargo").mkdir()
        (self.root / ".cargo/config.toml").write_text("[env]\nCONFIG_ENV = { value = 'from-config', force = true }\n"
                                                       "FALLBACK_ENV = 'from-config'\nPROPTEST_RNG_SEED = '42'\n"
                                                       "KOIDRIVE_TEST_SERVER = 'from-config'\n"
                                                       "KOIDRIVE_TEST_SERVER_LOG = { value = 'from-config' }\n")
        (self.root / "cargo-home").mkdir()
        (self.root / "cargo-home/config.toml").write_text("[env]\nKOIDRIVE_TEST_SERVER_LOG = { value = 'home', force = true }\n"
                                                           "CARGO_REGISTRY_TOKEN = { value = 'home', force = true }\n")
        self.package = self.root / "crate"
        self.package.mkdir()
        self.probe = r'''
import json, os, sys, time
from pathlib import Path
probe = Path(os.environ['PROBES'])
seed = os.environ['PROPTEST_RNG_SEED']
second = Path(sys.argv[0]).name.startswith('second-')
record = dict(pid=os.getpid(), args=sys.argv[1:], cwd=os.getcwd(),
              seed=seed, ordinary=os.environ.get('PROPTEST_CASES'),
              cases=os.environ.get('KOIDRIVE_BATTERY_CASES'),
              server=os.environ.get('KOIDRIVE_TEST_SERVER'),
              manifest=os.environ.get('CARGO_MANIFEST_DIR'), config=os.environ.get('CONFIG_ENV'),
              fallback=os.environ.get('FALLBACK_ENV'), server_log=os.environ.get('KOIDRIVE_TEST_SERVER_LOG'),
              capture=os.environ.get('STRESS_TEST_CAPTURE'))
(probe / (seed + ('-2' if second else '') + '.start')).write_text(json.dumps(record))
deadline = time.monotonic() + 5
while len(list(probe.glob('*.start'))) < int(os.environ.get('BARRIER', '1')):
    if time.monotonic() > deadline:
        sys.exit(24)
    time.sleep(0.01)
mode = os.environ.get('PROBE_MODE_2' if second else 'PROBE_MODE', 'pass')
if mode == 'hang':
    time.sleep(60)
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
'''
        self.environment = dict(os.environ, PATH=str(self.bin) + os.pathsep + os.environ["PATH"],
                                PROBES=str(self.probes), PROPTEST_CASES="999", KOIDRIVE_BATTERY_CASES="888",
                                FAKE_PACKAGE=str(self.package), CONFIG_ENV="inherited", FALLBACK_ENV="inherited",
                                CARGO_HOME=str(self.root / "cargo-home"),
                                REVIEW_CANARY_TOKEN="synthetic", CARGO_REGISTRY_TOKEN="synthetic")
        self.build("server")

    def build(self, *names):
        tests = [self.root / f"target/debug/deps/{name}-{index:016x}" for index, name in enumerate(names)]
        for test in tests:
            self.executable(test, self.probe)
        self.environment["FAKE_TESTS"] = os.pathsep.join(map(str, tests))

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
            self.assertEqual(probe["args"], ["two words", "--include-ignored"])
            self.assertEqual(probe["cwd"], str(self.package))
            self.assertEqual((probe["manifest"], probe["config"], probe["fallback"], probe["capture"]),
                             (str(self.package), "from-config", "inherited", None))
        self.assertEqual((self.probes / "cargo.fallback").read_text(), "inherited")
        self.assertNotIn(b"FALLBACK_ENV", (self.probes / "capture.jsonl").read_bytes())
        self.assertEqual((self.probes / "cargo.capture").read_text(), "test --locked -p koidrive-net --features serde --test send")
        self.assertFalse((self.probes / "cargo.called").exists())
        for path in [*(self.root / "out").rglob("*"), self.probes / "capture.jsonl"]:
            for secret in (b"REVIEW_CANARY_TOKEN", b"CARGO_REGISTRY_TOKEN"):
                self.assertNotIn(secret, path.read_bytes() if path.is_file() else b"", path)
        self.assertIn(b"CARGO_MANIFEST_DIR", (self.probes / "capture.jsonl").read_bytes())

    def test_each_repetition_runs_every_built_test_executable_in_turn(self):
        self.build("server", "second")
        for mode, expected, code in (("pass", "pass", 0), ("fail", "fail", 101)):
            with self.subTest(mode=mode):
                for path in self.probes.iterdir():
                    path.unlink()
                record = self.run_tests(self.args(), PROBE_MODE=mode)[0]
                self.assertEqual((record["result"], record["exit_code"]), (expected, code))
                self.assertEqual((record["test_binaries"], record["finished_binaries"]), (2, 2))
                first, second = (json.loads((self.probes / name).read_text()) for name in ("17.start", "17-2.start"))
                self.assertNotEqual(first["pid"], second["pid"])
                self.assertFalse((self.probes / "cargo.called").exists())

    def test_the_recorder_writes_only_allowed_variables(self):
        recorder = self.root / "record-test"
        self.executable(recorder, stress.RECORDER)
        capture = self.root / "capture.jsonl"
        environment = dict(PATH=os.environ["PATH"], REVIEW_CANARY_TOKEN="synthetic", CARGO_REGISTRY_TOKEN="synthetic",
                           STRESS_TEST_CAPTURE=str(capture),
                           CARGO_PKG_NAME="probe", STRESS_TEST_ALLOWED=json.dumps([*stress.CARGO_SETS, "FALLBACK_ENV"]),
                           FALLBACK_ENV="from-config")
        subprocess.check_call([str(recorder), "/bin/test-executable"], cwd=self.package, env=environment)
        record = json.loads(capture.read_text())
        self.assertEqual(record["environment"], {"CARGO_PKG_NAME": "probe", "FALLBACK_ENV": "from-config"})
        self.assertNotIn("REVIEW_CANARY_TOKEN", capture.read_text())
        self.assertNotIn("CARGO_REGISTRY_TOKEN", capture.read_text())

    def test_a_build_without_test_executables_is_refused(self):
        self.build()
        with self.assertRaisesRegex(RuntimeError, "ran 0 test executables"):
            self.run_tests(self.args())
        self.assertEqual((self.probes / "cargo.capture").read_text(), "test --locked -p koidrive-net --tests")

    def test_a_configured_target_runner_is_refused(self):
        with (self.root / ".cargo/config.toml").open("a") as config:
            config.write("[target.aarch64-apple-darwin]\nrunner = 'qemu'\n")
        with self.assertRaisesRegex(RuntimeError, "target runner is configured"):
            self.run_tests(self.args())
        self.assertFalse((self.probes / "cargo.capture").exists())

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
                self.build(stress.WORKERS[crate][1])
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
                    self.assertEqual(probe["cwd"], str(self.package))
                    self.assertEqual(probe["args"], ["--ignored", "selected test"])
                    self.assertEqual((probe["manifest"], probe["config"]), (str(self.package), "from-config"))
                    self.assertTrue(probe["server"].startswith("http://127.0.0.1:"), probe["server"])
                    self.assertEqual(probe["server_log"], "from-config")
                self.assertFalse((self.probes / "cargo.called").exists())

    def test_worker_repetitions_run_the_target_the_launcher_asks_for(self):
        self.worker_fixture("koidrive-driver")
        self.build("first_sync_bench")
        with patch.dict(os.environ, KOIDRIVE_DRIVER_TEST="first_sync_bench"):
            args = self.args("--mode", "worker", "--crate", "koidrive-driver")
        self.assertEqual(args.test, "first_sync_bench")
        record = self.run_tests(args, KOIDRIVE_DRIVER_TEST="first_sync_bench")[0]
        self.assertEqual(record["result"], "pass")
        self.assertIn("--test first_sync_bench", (self.probes / "cargo.capture").read_text())
        for path in self.probes.iterdir():
            path.unlink()
        self.build("sync")
        record = self.run_tests(self.args("--mode", "worker", "--crate", "koidrive-driver"),
                                KOIDRIVE_DRIVER_TEST="first_sync_bench")[0]
        self.assertEqual(record["result"], "error")
        self.assertFalse(list(self.probes.glob("*.start")))
        self.assertIn("no prebuilt test target first_sync_bench", (self.root / "out/logs/run-1.log").read_text())

    def test_worker_repetitions_run_every_executable_after_a_failure(self):
        self.worker_fixture()
        self.build("server", "server")
        record = self.run_tests(self.args("--mode", "worker"), PROBE_MODE="fail", BARRIER="1")[0]
        self.assertEqual((record["result"], record["exit_code"]), ("fail", 101))
        self.assertEqual((record["test_binaries"], record["finished_binaries"]), (2, 2))

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
