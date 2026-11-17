#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import unittest


REPO = Path(__file__).resolve().parents[1]
CONFIG = {
    "observability": {"enabled": True, "redact_query_string": True},
    "migrations": [{"tag": "v1", "new_sqlite_classes": ["AccountDO", "SpaceDO"]}],
    "durable_objects": {"bindings": [{"name": "ACCOUNT", "class_name": "AccountDO"}, {"name": "SPACE", "class_name": "SpaceDO"}]},
    "r2_buckets": [{"binding": "BUCKET", "bucket_name": "test"}],
}


class CheckDeploy(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="check-deploy-unit-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.server = self.root / "server"
        (self.server / "scripts").mkdir(parents=True)
        shutil.copyfile(REPO / "server/scripts/check-deploy.mjs", self.server / "scripts/check-deploy.mjs")
        (self.server / "scripts/release-version.mjs").write_text('export const releaseVersion = () => "1.2.3";\n')
        (self.server / "package.json").write_text(json.dumps({
            "scripts": {"deploy": "wrangler deploy"},
            "cloudflare": {"bindings": {name: {"description": name} for name in ("ACCOUNT", "SPACE", "BUCKET")}},
        }))
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.executable(self.bin / "git", "import sys; sys.stdout.write('package.json\\0scripts/release-version.mjs\\0')\n")
        dependencies = self.root / "dependencies"
        (dependencies / "wrangler").mkdir(parents=True)
        (dependencies / "wrangler/package.json").write_text('{"type":"module","main":"index.mjs"}')
        (dependencies / "wrangler/index.mjs").write_text("export const unstable_readConfig = () => (" + json.dumps(CONFIG) + ");\n")
        self.executable(dependencies / ".bin/wrangler", """
from http.server import BaseHTTPRequestHandler, HTTPServer
import json, os, sys
from pathlib import Path
class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass
    def do_GET(self):
        with Path(os.environ['REQUESTS']).open('a') as log:
            log.write(self.path + '\\n')
        self.send_response(200)
        self.send_header('content-security-policy', "default-src 'none'")
        self.end_headers()
        if self.path == '/v1/server':
            body = json.dumps({'claimed': self.server.claimed, 'serverVersion': '1.2.3'})
        elif self.path == '/setup':
            print('Koidrive claim code (valid for 1 hour): ABCD-1234', flush=True)
            body = 'claim code'
        else:
            body = '<script src="/viewer/viewer.js"></script>'
        self.wfile.write(body.encode())
    def do_POST(self):
        self.rfile.read(int(self.headers['content-length']))
        self.server.claimed = True
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b'{}')
server = HTTPServer(('127.0.0.1', 0), Handler)
server.claimed = False
url = 'http://127.0.0.1:' + str(server.server_port)
Path(os.environ['RESULT']).write_text(json.dumps({'pid': os.getpid(), 'argv': sys.argv[1:], 'cwd': os.getcwd(), 'url': url}))
mode = os.environ.get('WORKER_MODE', 'ready')
if mode == 'exit':
    sys.exit(17)
print('\\x1b[32m[wrangler:info] Ready on ' + os.environ.get('READY', url) + '\\x1b[0m', flush=True)
server.serve_forever()
""")
        self.executable(self.bin / "pnpm", """
import json, os, shutil, sys
from pathlib import Path
if sys.argv[1] == 'install':
    shutil.copytree(os.environ['DEPENDENCIES'], 'node_modules')
else:
    Path('release.json').write_text(json.dumps({'version': '1.2.3'}))
    out = Path(sys.argv[sys.argv.index('--outdir') + 1])
    out.mkdir()
    (out / 'worker.js').write_text('"1.2.3"')
    viewer = Path('web/dist/viewer/index.html')
    viewer.parent.mkdir(parents=True)
    viewer.touch()
""")

    def executable(self, path, code):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("#!" + sys.executable + "\n" + code)
        path.chmod(0o755)

    def launch(self, **overrides):
        result = self.root / "result.json"
        requests = self.root / "requests"
        result.unlink(missing_ok=True)
        requests.unlink(missing_ok=True)
        env = {**os.environ, "PATH": str(self.bin) + os.pathsep + os.environ["PATH"],
               "DEPENDENCIES": str(self.root / "dependencies"), "RESULT": str(result), "REQUESTS": str(requests)}
        env.update(overrides)
        try:
            run = subprocess.run(["node", self.server / "scripts/check-deploy.mjs"], env=env,
                                 capture_output=True, text=True, timeout=20)
        except subprocess.TimeoutExpired:
            if result.exists():
                try:
                    os.killpg(json.loads(result.read_text())["pid"], signal.SIGKILL)
                except ProcessLookupError:
                    pass
            raise
        self.assertTrue(result.exists(), run.stdout + run.stderr)
        worker = json.loads(result.read_text())
        try:
            os.killpg(worker["pid"], 0)
        except ProcessLookupError:
            pass
        else:
            os.killpg(worker["pid"], signal.SIGKILL)
            self.fail("Deployment check left its Worker process group running")
        self.assertFalse(Path(worker["cwd"]).parent.exists(), "Deployment workspace survived cleanup")
        return run, worker, requests

    def test_uses_child_assigned_url_and_waits_for_shutdown(self):
        run, worker, requests = self.launch()
        self.assertEqual(run.returncode, 0, run.stdout + run.stderr)
        self.assertIn("check-deploy: server/ deploys on its own", run.stdout)
        self.assertIn("/setup", requests.read_text())
        self.assertEqual(worker["argv"][worker["argv"].index("--port") + 1], "0")
        self.assertEqual(worker["argv"][worker["argv"].index("--inspector-port") + 1], "0")

    def test_rejects_malformed_or_nonloopback_readiness_without_probing(self):
        for url in ("http://example.com:43219", "http://127.0.0.1:0", "http://localhost:65536", "http://localhost:nope", "http://localhost:43219/path"):
            with self.subTest(url=url):
                run, _, requests = self.launch(READY=url)
                self.assertEqual(run.returncode, 1, run.stdout + run.stderr)
                self.assertIn("Wrangler ready URL", run.stderr)
                self.assertFalse(requests.exists())

    def test_early_exit_does_not_probe(self):
        run, _, requests = self.launch(WORKER_MODE="exit")
        self.assertEqual(run.returncode, 1, run.stdout + run.stderr)
        self.assertIn("exited with 17", run.stderr)
        self.assertFalse(requests.exists())


if __name__ == "__main__":
    unittest.main(verbosity=2)
