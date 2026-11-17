#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import importlib.util
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import unittest


sys.dont_write_bytecode = True
REPO = Path(__file__).resolve().parents[1]
LAUNCHERS = [Path(f"core/{crate}/scripts/integration-test.sh") for crate in ("driver", "net", "ffi")]
LAUNCHERS.append(Path("cli/scripts/integration-test.sh"))


class WorkerReady(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        spec = importlib.util.spec_from_file_location("worker_ready", REPO / "scripts/worker_ready.py")
        cls.parser = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.parser)

    def test_exact_colored_loopback_urls(self):
        for url in ("http://127.0.0.1:43219", "http://localhost:45123", "http://[::1]:54321", "http://127.12.34.56:65535"):
            with self.subTest(url=url):
                self.assertEqual(self.parser.worker_ready_url("other log\n\x1b[32m[wrangler:info] Ready on \x1b[1m" + url + "\x1b[0m\r\n"), url)

    def test_missing_partial_and_unrelated_readiness(self):
        for text in ("", "Ready on http://127.0.0.1:12", "not Ready on http://127.0.0.1:12\n", "Error: Ready on http://localhost:12\n"):
            with self.subTest(text=text):
                self.assertIsNone(self.parser.worker_ready_url(text))

    def test_malformed_or_nonloopback_readiness_is_rejected(self):
        for url in ("http://127.0.0.1:0", "http://localhost:65536", "http://localhost:nope", "http://localhost:-2",
                    "http://localhost:\uff11\uff12", "http://0.0.0.0:1234", "http://example.com:1234", "http://127.300.0.1:1234",
                    "http://localhost.evil:1234", "http://user@localhost:1234", "https://localhost:1234", "http://localhost:1234/path",
                    "http://localhost:1234?x=y", "http://localhost:1234#x", "http://localhost:1234 extra", ""):
            with self.subTest(url=url), self.assertRaises(ValueError):
                self.parser.worker_ready_url("[wrangler:info] Ready on " + url + "\n")


class WorkerLaunchers(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="worker-launchers-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        for launcher in LAUNCHERS:
            destination = self.root / launcher
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(REPO / launcher, destination)
        (self.root / "scripts").mkdir()
        parser = REPO / "scripts/worker_ready.py"
        if parser.exists():
            shutil.copyfile(parser, self.root / "scripts" / parser.name)
        self.executable(self.bin / "cargo", """
import json, os, sys
from pathlib import Path
if 'KOIDRIVE_TEST_SERVER' in os.environ:
    Path(os.environ['RESULT']).write_text(json.dumps({'KOIDRIVE_TEST_SERVER': os.environ['KOIDRIVE_TEST_SERVER']}))
    sys.exit(int(os.environ.get('CARGO_STATUS', '0')))
""")
        self.executable(self.root / "server/node_modules/.bin/wrangler", """
import json, os, signal, socket, subprocess, sys, time
from pathlib import Path
result = Path(os.environ['RESULT'])
child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])
record = {'argv': sys.argv[1:], 'pid': os.getpid(), 'child': child.pid}
result.with_suffix('.worker').write_text(json.dumps(record))
def stop(*_):
    child.wait(timeout=5)
    sys.exit(0)
signal.signal(signal.SIGTERM, stop)
mode = os.environ.get('WORKER_MODE', 'ready')
if mode == 'exit':
    child.terminate()
    child.wait(timeout=5)
    sys.exit(17)
ready = os.environ.get('READY', '\\x1b[32m[wrangler:info] Ready on http://127.0.0.1:43219\\x1b[0m')
if os.environ.get('STEAL_SELECTED_PORT') == '1':
    port = int(sys.argv[sys.argv.index('--port') + 1])
    if port:
        thief = socket.socket()
        try:
            thief.bind(('127.0.0.1', port))
            thief.listen()
        except OSError:
            pass
    listener = socket.socket()
    try:
        listener.bind(('127.0.0.1', port))
    except OSError:
        print('EADDRINUSE: selected port was stolen before Wrangler bound it', flush=True)
        child.terminate()
        child.wait(timeout=5)
        sys.exit(18)
    listener.listen()
    record['url'] = 'http://127.0.0.1:' + str(listener.getsockname()[1])
    result.with_suffix('.worker').write_text(json.dumps(record))
    ready = '[wrangler:info] Ready on ' + record['url']
if mode != 'silent':
    print(ready, flush=True)
while True:
    time.sleep(1)
""")

    def executable(self, path, code):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("#!" + sys.executable + "\n" + code)
        path.chmod(0o755)

    def launch(self, launcher, **overrides):
        result = self.root / "result.json"
        result.unlink(missing_ok=True)
        result.with_suffix(".worker").unlink(missing_ok=True)
        env = {**os.environ, "PATH": str(self.bin) + os.pathsep + os.environ["PATH"],
               "RESULT": str(result), "PYTHONDONTWRITEBYTECODE": "1"}
        env.pop("KOIDRIVE_TEST_PORT", None)
        env.update(overrides)
        worker_path = result.with_suffix(".worker")
        try:
            run = subprocess.run(["bash", self.root / launcher, "focused-filter"], env=env,
                                 capture_output=True, text=True, timeout=20)
        except subprocess.TimeoutExpired:
            if worker_path.exists():
                try:
                    os.killpg(json.loads(worker_path.read_text())["pid"], signal.SIGKILL)
                except ProcessLookupError:
                    pass
            raise
        if worker_path.exists():
            worker = json.loads(worker_path.read_text())
            try:
                os.killpg(worker["pid"], 0)
            except ProcessLookupError:
                pass
            else:
                os.killpg(worker["pid"], signal.SIGKILL)
                self.fail("Launcher left its Worker process group running")
            state = Path(worker["argv"][worker["argv"].index("--persist-to") + 1])
            self.assertFalse(state.parent.exists(), "Launcher left its temporary directory")
        return run, result, json.loads(worker_path.read_text()) if worker_path.exists() else None

    def test_assigned_port_is_forwarded_and_process_group_is_cleaned(self):
        for launcher in LAUNCHERS:
            with self.subTest(launcher=launcher):
                run, result, worker = self.launch(launcher)
                self.assertEqual(run.returncode, 0, run.stderr)
                self.assertEqual(json.loads(result.read_text())["KOIDRIVE_TEST_SERVER"], "http://127.0.0.1:43219")
                self.assertEqual(worker["argv"][worker["argv"].index("--port") + 1], "0")
                self.assertEqual(worker["argv"][worker["argv"].index("--inspector-port") + 1], "0")

    def test_explicit_port_and_test_failure_preserve_cleanup(self):
        for launcher in LAUNCHERS:
            with self.subTest(launcher=launcher):
                run, result, worker = self.launch(launcher, KOIDRIVE_TEST_PORT="45123", CARGO_STATUS="23",
                                                  READY="[wrangler:info] Ready on http://localhost:45123")
                self.assertEqual(run.returncode, 23, run.stderr)
                self.assertEqual(json.loads(result.read_text())["KOIDRIVE_TEST_SERVER"], "http://localhost:45123")
                self.assertEqual(worker["argv"][worker["argv"].index("--port") + 1], "45123")

    def test_selected_port_stolen_before_wrangler_binds_is_avoided(self):
        for launcher in LAUNCHERS:
            with self.subTest(launcher=launcher):
                run, result, worker = self.launch(launcher, STEAL_SELECTED_PORT="1")
                self.assertEqual(run.returncode, 0, run.stderr)
                self.assertEqual(json.loads(result.read_text())["KOIDRIVE_TEST_SERVER"], worker["url"])

    def test_explicit_occupied_port_fails_without_fallback(self):
        with socket.socket() as owner:
            owner.bind(("127.0.0.1", 0))
            owner.listen()
            for launcher in LAUNCHERS:
                with self.subTest(launcher=launcher):
                    run, result, _ = self.launch(launcher, STEAL_SELECTED_PORT="1", KOIDRIVE_TEST_PORT=str(owner.getsockname()[1]))
                    self.assertNotEqual(run.returncode, 0)
                    self.assertIn("EADDRINUSE", run.stderr)
                    self.assertFalse(result.exists())

    def test_malformed_readiness_and_early_exit_do_not_run_tests(self):
        for launcher in LAUNCHERS:
            for overrides in ({"READY": "[wrangler:info] Ready on http://example.com:43219"}, {"WORKER_MODE": "exit"}):
                with self.subTest(launcher=launcher, overrides=overrides):
                    run, result, _ = self.launch(launcher, **overrides)
                    self.assertNotEqual(run.returncode, 0)
                    self.assertFalse(result.exists())

    def test_timeout_does_not_run_tests_and_cleans_process_group(self):
        self.executable(self.bin / "seq", "print(1)\n")
        run, result, worker = self.launch(LAUNCHERS[0], WORKER_MODE="silent")
        self.assertIsNotNone(worker)
        self.assertNotEqual(run.returncode, 0)
        self.assertIn("after 60 seconds", run.stderr)
        self.assertFalse(result.exists())


if __name__ == "__main__":
    unittest.main(verbosity=2)
