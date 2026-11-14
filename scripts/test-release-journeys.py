#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import argparse
import errno
import hashlib
import json
import os
from pathlib import Path
import platform
import plistlib
import re
import select
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import urllib.request
import uuid


REPO = Path(__file__).resolve().parents[1]
RELEASE = "https://github.com/koidrive/koidrive/releases/download"
HELD_COMMIT = "2ffddfa8472a87f858698681551cc663e4579f8d"
HELD_SHA = "5811e418e8b896542327585e19c7f0626ee1d5b64f69ed1c88df2c5f1e1ac522"
HELD_URL = f"https://raw.githubusercontent.com/koidrive/koidrive/{HELD_COMMIT}/install.sh"
INSTALLER_SOURCES = {"held": HELD_URL, "live": "https://koidrive.com/install.sh"}
APP = Path("/Applications/Koidrive.app")
TAP = "koidrive/tap/koidrive"
SYSTEM_PATH = "/usr/bin:/bin:/usr/sbin:/sbin"
SCENARIOS = ("verify", "cask", "formula-conflict", "installer")
REQUIRED = {
    "verify": {"release assets verified", "signed static verification"},
    "cask": {"release assets verified", "clean account", "immediate version", "bundled helper link",
             "app quarantine", "plain uninstall", "plain uninstall preserves documents",
             "zap native unregister", "zap removes state", "zap preserves documents"},
    "formula-conflict": {"release assets verified", "clean account", "formula version",
                         "binary conflict", "formula unchanged", "cask conflict state"},
    "installer": {"release assets verified", "clean account", "held script verified", "bootstrap version",
                  "unlinked before setup", "attended setup", "installed version", "installed app",
                  "xattrs preserved", "healthy native service", "exact byte sync"},
}


def tag_value(value):
    if not re.fullmatch(r"v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)(?:-(?:alpha|rc)\.(?:0|[1-9][0-9]*))?", value):
        raise argparse.ArgumentTypeError("Expected vX.Y.Z, vX.Y.Z-alpha.N or vX.Y.Z-rc.N")
    return value


def hosted_guard(env=None, system=None, machine=None, home=None, account_home=None):
    env = os.environ if env is None else env
    system = platform.system() if system is None else system
    machine = platform.machine() if machine is None else machine
    home = Path.home() if home is None else Path(home)
    if (env.get("GITHUB_ACTIONS") != "true" or env.get("RUNNER_ENVIRONMENT") != "github-hosted"
            or system != "Darwin" or machine != "arm64"):
        raise RuntimeError("Runtime requires a GitHub-hosted Darwin arm64 account with its real HOME")
    if account_home is None:
        import pwd
        account_home = pwd.getpwuid(os.getuid()).pw_dir
    if home.resolve() != Path(account_home).resolve():
        raise RuntimeError("Runtime requires the runner account's real HOME")
    forbidden = [key for key in env if key.startswith(("XDG_", "KOIDRIVE_"))]
    forbidden += [key for key in ("HOMEBREW_CASK_OPTS", "HOMEBREW_NO_CASK_QUARANTINE", "ZDOTDIR") if env.get(key)]
    if forbidden:
        raise RuntimeError("Remove runtime/profile/release overrides: " + ", ".join(sorted(forbidden)))


def sha256(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def checksums(text):
    entries = {}
    for line in text.splitlines():
        match = re.fullmatch(r"([0-9a-f]{64}) [ *]([A-Za-z0-9][A-Za-z0-9._-]*)", line)
        if not match or match[2] in entries:
            raise ValueError("Malformed or duplicate SHA256SUMS entry")
        entries[match[2]] = match[1]
    if not entries:
        raise ValueError("SHA256SUMS is empty")
    return entries


def sanitized(text, secrets=()):
    for secret in sorted((s for s in secrets if s), key=len, reverse=True):
        text = text.replace(secret, "[REDACTED]")
    text = re.sub(r"(Koidrive claim code \(valid for 1 hour\): )[^\r\n]+", r"\1[REDACTED]", text)
    text = re.sub(r'(?i)("(?:[^"]*(?:token|secret|credential|claim|code|recovery|join_argv)[^"]*)"\s*:\s*)("(?:\\.|[^"\\])*"|\[[^\]]*\])', r'\1"[REDACTED]"', text)
    text = re.sub(r"(?i)(bearer\s+)[A-Za-z0-9._~-]+", r"\1[REDACTED]", text)
    text = re.sub(r"\b[A-Z0-9]{4}-[A-Z0-9]{4}\b", "[REDACTED]", text)
    text = re.sub(r"\b[ABCDEFGHJKMNPQRSTVWXYZ0-9]{8,}\b", "[REDACTED]", text)
    text = re.sub(r"\b[A-Za-z0-9_-]{40,}\b", lambda m: m[0] if re.fullmatch(r"[0-9a-f]{40}|[0-9a-f]{64}", m[0]) else "[REDACTED]", text)
    return text


def passed(report):
    assertions = report["assertions"]
    observed = {a["name"] for a in assertions if a["passed"] is True}
    return (REQUIRED[report["scenario"]] <= observed and bool(report["cleanup"])
            and all(a["passed"] is True for a in assertions)
            and all(a["passed"] is True for a in report["cleanup"])
            and not report["errors"])


class OwnedProcess(subprocess.Popen):
    def __init__(self, *args, **kwargs):
        kwargs["start_new_session"] = True
        super().__init__(*args, **kwargs)
        self.group_closed = False
        self.group_uid = os.getuid()
        self.group_observations = []


def group_census(process, phase, error):
    observation = {"phase": phase, "errno": error.errno, "members": []}
    process.group_observations.append(observation)
    try:
        if error.errno != errno.EPERM:
            raise RuntimeError("Unexpected owned-group permission error")
        result = subprocess.run(["/bin/ps", "-axo", "pid=,pgid=,uid=,stat="],
                                capture_output=True, text=True, timeout=3)
        observation.update(status=result.returncode, stderr=result.stderr)
        if result.returncode != 0 or result.stderr.strip() or not result.stdout.strip():
            raise RuntimeError("Process-table census did not complete successfully")
        seen = set()
        for line in result.stdout.splitlines():
            fields = line.split()
            if (len(fields) != 4 or not all(re.fullmatch(r"[0-9]+", value) for value in fields[:3])
                    or not re.fullmatch(r"[RSDTtXZWIU?][+<>=ANLSEVWXsl]*", fields[3])):
                raise RuntimeError("Malformed process-table census")
            pid, pgid, uid = map(int, fields[:3])
            if pid in seen:
                raise RuntimeError("Duplicate process in process-table census")
            seen.add(pid)
            if pgid == process.pid:
                observation["members"].append({"pid": pid, "pgid": pgid, "uid": uid, "stat": fields[3]})
        observation["rows"] = len(seen)
        if any(member["uid"] != process.group_uid for member in observation["members"]):
            raise RuntimeError("Foreign process in owned group census")
        if any(not member["stat"].startswith("Z") for member in observation["members"]):
            raise RuntimeError("Live process remains in owned group census")
        if phase != "signal-zero":
            raise RuntimeError("Permission failure in owned-group " + phase)
    except Exception as failure:
        observation.update(decision="rejected", reason=str(failure))
        raise RuntimeError("Could not verify owned process group " + str(process.pid) + ": " + str(failure)) from failure
    observation["decision"] = "no-live-members"
    process.group_closed = True
    process.poll()
    return False


def group_present(process):
    if not isinstance(process, OwnedProcess) or process.pid <= 1 or process.pid == os.getpgrp():
        raise RuntimeError("Refusing to signal a process outside its owned session")
    if process.group_closed:
        return False
    exited = process.poll() is not None
    phase = "getpgid"
    try:
        pgid = os.getpgid(process.pid)
        phase = "getsid"
        sid = os.getsid(process.pid)
    except PermissionError as error:
        group_census(process, phase, error)
        raise
    except ProcessLookupError:
        if not exited:
            try:
                process.wait(timeout=1)
            except subprocess.TimeoutExpired as error:
                raise RuntimeError("Could not verify the owned session leader") from error
    else:
        if exited or pgid != process.pid or sid != process.pid:
            raise RuntimeError("Refusing to signal a changed or reused session leader")
    try:
        os.killpg(process.pid, 0)
    except PermissionError as error:
        return group_census(process, "signal-zero", error)
    except ProcessLookupError:
        process.group_closed = True
        process.poll()
        return False
    return True


def stop_group(process, term_timeout=8, kill_timeout=8):
    for signum, timeout in ((signal.SIGTERM, term_timeout), (signal.SIGKILL, kill_timeout)):
        if not group_present(process):
            return
        try:
            os.killpg(process.pid, signum)
        except PermissionError as error:
            group_census(process, signal.Signals(signum).name, error)
            raise
        except ProcessLookupError:
            process.group_closed = True
            process.poll()
            return
        deadline = time.monotonic() + timeout
        while group_present(process):
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                break
            time.sleep(min(0.05, remaining))
        else:
            return
    raise RuntimeError("Owned process group survived SIGKILL: " + str(process.pid))


class Capture:
    def __init__(self, stream, path, secrets):
        self.raw = ""
        self.error = None
        self.stream = stream
        self.path = path
        self.secrets = secrets
        self.thread = threading.Thread(target=self.read, daemon=True)
        self.thread.start()

    def read(self):
        try:
            with self.path.open("w") as destination:
                while line := self.stream.readline():
                    text = line.decode("utf-8", errors="replace")
                    self.raw = (self.raw + text)[-4_000_000:]
                    destination.write(sanitized(text, self.secrets))
                    destination.flush()
        except Exception as error:
            self.error = error
        finally:
            self.stream.close()


class Evidence:
    def __init__(self, directory, tag, scenario):
        self.directory = Path(directory).resolve()
        self.directory.mkdir(parents=True, exist_ok=False)
        self.report = {"tag": tag, "scenario": scenario, "source": RELEASE + "/" + tag,
                       "inputs": {}, "commands": [], "assertions": [], "cleanup": [],
                       "errors": [], "coverage": {}, "passed": False}
        self.secrets = []
        self.processes = []
        self.env = {**os.environ, "DO_NOT_TRACK": "1", "WRANGLER_SEND_METRICS": "false",
                    "HOMEBREW_NO_AUTO_UPDATE": "1", "NO_COLOR": "1"}

    def check(self, name, condition, detail=""):
        self.report["assertions"].append({"name": name, "passed": bool(condition),
                                           "detail": sanitized(str(detail), self.secrets)})
        if not condition:
            raise RuntimeError(name + ": " + sanitized(str(detail), self.secrets))

    def start(self, argv, env=None):
        argv = list(map(str, argv))
        number = len(self.report["commands"]) + 1
        stem = self.directory / f"{number:03d}"
        record = {"argv": [sanitized(arg, self.secrets) for arg in argv], "status": None,
                  "stdout": stem.with_suffix(".stdout").name,
                  "stderr": stem.with_suffix(".stderr").name}
        self.report["commands"].append(record)
        process = OwnedProcess(argv, env=self.env if env is None else env, cwd=REPO,
                               stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE)
        record["group"] = {"pgid": process.pid, "uid": process.group_uid, "observations": process.group_observations}
        out = Capture(process.stdout, self.directory / record["stdout"], self.secrets)
        err = Capture(process.stderr, self.directory / record["stderr"], self.secrets)
        self.processes.append((process, record, out, err))
        return process, record, out, err

    def finish(self, item, timeout=180):
        process, record, out, err = item
        try:
            process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            record["status"] = 124
        else:
            record["status"] = process.returncode
        stop_group(process)
        for capture in (out, err):
            capture.thread.join(timeout=10)
            if capture.thread.is_alive() or capture.error:
                raise RuntimeError("Could not safely persist sanitized process output")
        return record["status"], out.raw, err.raw

    def run(self, argv, *, env=None, required=True, timeout=180):
        status, out, err = self.finish(self.start(argv, env), timeout)
        if required and status != 0:
            raise RuntimeError(f"Command {len(self.report['commands'])} exited {status}; see sanitized evidence")
        return status, out, err

    def cleanup(self, name, action, group=None):
        try:
            action()
        except Exception as error:
            outcome = {"name": name, "passed": False, "detail": sanitized(str(error), self.secrets)}
        else:
            outcome = {"name": name, "passed": True}
        if group is not None:
            outcome["group"] = group
        self.report["cleanup"].append(outcome)
        return outcome["passed"]

    def close(self):
        for item in reversed(self.processes):
            process, record, _, _ = item
            if not process.group_closed:
                self.cleanup("owned process group", lambda p=process: stop_group(p), group=record["group"])
            if record["status"] is None:
                self.cleanup("owned process logs", lambda i=item: self.finish(i, 10))
        self.report["passed"] = passed(self.report)
        (self.directory / "report.json").write_text(json.dumps(self.report, indent=2) + "\n")
        conflict = self.report.get("conflict", "")
        summary = (conflict + "\n\n" if conflict else "") + json.dumps({
            "scenario": self.report["scenario"], "tag": self.report["tag"],
            "passed": self.report["passed"], "errors": self.report["errors"],
            "coverage": self.report["coverage"]}, indent=2) + "\n"
        (self.directory / "summary.txt").write_text(summary)
        print(summary, end="")


class Ownership:
    def __init__(self):
        self.paths = {}

    def claim(self, path):
        path = Path(path).absolute()
        if path.exists() or path.is_symlink():
            raise RuntimeError("Refusing existing state: " + str(path))
        if path.parent.resolve() != path.parent:
            raise RuntimeError("Refusing a symlinked state parent: " + str(path))
        self.paths[path] = path.parent.resolve()
        return path

    def remove(self, path):
        path = Path(path).absolute()
        if path not in self.paths:
            raise RuntimeError("Refusing cleanup of unowned state: " + str(path))
        if path.parent.resolve() != self.paths[path]:
            raise RuntimeError("Refusing cleanup through a changed parent: " + str(path))
        if path.is_symlink() or path.is_file():
            path.unlink()
        elif path.exists():
            shutil.rmtree(path)


def until(predicate, timeout=60):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(0.25)
    return False


def download(evidence, url, path):
    evidence.run(["/usr/bin/curl", "-fsSL", "--retry", "3", "--output", path, "--", url])


def quarantine(evidence, path, required=True):
    status, out, err = evidence.run(["/usr/bin/xattr", "-lr", path], required=False)
    evidence.check("xattrs readable " + str(path), status == 0, out + err)
    if required:
        status, value, error = evidence.run(["/usr/bin/xattr", "-p", "com.apple.quarantine", path], required=False)
        evidence.check("app quarantine" if Path(path).suffix == ".app" else "download quarantine",
                       status == 0 and bool(value.strip()), value + error)
    return out


def quarantine_paths(text):
    return {match[1] or "." for match in re.finditer(r"(?m)^(?:(.+): )?com\.apple\.quarantine:", text)}


def release_assets(evidence, root):
    tag = evidence.report["tag"]
    base = RELEASE + "/" + tag
    sums = root / "SHA256SUMS"
    download(evidence, base + "/SHA256SUMS", sums)
    evidence.report["inputs"]["SHA256SUMS"] = {"url": base + "/SHA256SUMS", "sha256": sha256(sums)}
    entries = checksums(sums.read_text())
    assets = {}
    for name in ("koidrive-aarch64-apple-darwin.tar.gz", f"Koidrive-{tag[1:]}-arm64.dmg"):
        if name not in entries:
            raise RuntimeError("SHA256SUMS omits " + name)
        path = root / name
        download(evidence, base + "/" + name, path)
        actual = sha256(path)
        evidence.report["inputs"][name] = {"url": base + "/" + name, "sha256": actual, "expected_sha256": entries[name]}
        evidence.check("checksum " + name, actual == entries[name], "actual SHA-256 " + actual)
        quarantine(evidence, path, required=False)
        assets[name] = path
    evidence.check("release assets verified", True)
    return assets, entries


def launchd_state(evidence):
    _, agents, _ = evidence.run(["/bin/launchctl", "print", f"gui/{os.getuid()}"])
    return agents


def state_paths(home):
    return [APP, home / "Applications/Koidrive.app", home / ".local/bin/koi",
            home / ".config/koidrive", home / ".local/state/koidrive",
            home / "Library/Application Support/Koidrive",
            home / "Library/Preferences/dev.koidrive.Koidrive.plist",
            home / "Library/Logs/Koidrive", home / "Library/Caches/dev.koidrive.app-install",
            Path("/Applications/.koidrive-app-install.lock"), home / "Applications/.koidrive-app-install.lock"]


def clean_account(evidence, ownership):
    home = Path.home()
    for path in state_paths(home):
        ownership.claim(path)
    agents = home / "Library/LaunchAgents"
    evidence.check("no existing service plists", not list(agents.glob("*koidrive*")))
    evidence.check("no existing executable", shutil.which("koi") is None)
    for kind in ("--formula", "--cask"):
        _, installed, _ = evidence.run(["brew", "list", kind, "--versions"])
        evidence.check("no existing brew " + kind, not re.search(r"(?m)^(?:koidrive/tap/)?koidrive\s", installed))
    _, processes, _ = evidence.run(["/bin/ps", "-axo", "pid=,uid=,comm=,args="])
    evidence.check("no existing koi processes", not re.search(r"(?mi)(?:/|\s)(?:koi(?:\s+_daemon)?|Koidrive\.app/Contents/MacOS/Koidrive)(?:\s|$)", processes))
    agents = launchd_state(evidence)
    evidence.check("no existing Koidrive launchd jobs", "dev.koidrive" not in agents.lower())
    evidence.check("clean account", True)


def brew_prepare(evidence, entries, kind, wait_seconds=180):
    _, version, _ = evidence.run(["brew", "--version"])
    evidence.report["brew_version"] = version.strip()
    _, taps, _ = evidence.run(["brew", "tap"])
    if "koidrive/tap" not in taps.splitlines():
        evidence.run(["brew", "tap", "koidrive/tap"])
    if re.search(r"Homebrew ([7-9]|[1-9][0-9]+)\.", version):
        evidence.run(["brew", "trust", "--" + kind, TAP])
        evidence.report["brew_prerequisites"] = "Explicit tap and Homebrew " + kind + " trust; no quarantine bypass"
    deadline = time.monotonic() + wait_seconds
    while True:
        status, info, _ = evidence.run(["brew", "info", "--json=v2", "--" + kind, TAP], required=False)
        current = None
        if status == 0:
            data = json.loads(info)
            current = data["casks"][0]["version"] if kind == "cask" else data["formulae"][0]["versions"]["stable"]
        if current == evidence.report["tag"][1:]:
            break
        if time.monotonic() >= deadline:
            raise RuntimeError(f"Tap {kind} is {current}, not requested {evidence.report['tag']}; publication wait expired")
        time.sleep(min(15, max(0, deadline - time.monotonic())))
        evidence.run(["brew", "update", "--quiet"], timeout=120)
    _, source, _ = evidence.run(["brew", "cat", "--" + kind, TAP])
    evidence.report["inputs"]["tap-" + kind] = {"source": TAP, "sha256": hashlib.sha256(source.encode()).hexdigest()}
    if kind == "cask":
        _, info, _ = evidence.run(["brew", "info", "--json=v2", "--cask", TAP])
        spec = json.loads(info)["casks"][0]
        name = f"Koidrive-{evidence.report['tag'][1:]}-arm64.dmg"
        evidence.check("tap cask matches release", spec["version"] == evidence.report["tag"][1:]
                       and spec["sha256"] == entries[name] and spec["url"] == RELEASE + "/" + evidence.report["tag"] + "/" + name)
        evidence.check("cask has no formula dependency", not spec.get("depends_on", {}).get("formula"))
        hook = native_zap_hook(spec)
        evidence.check("native zap hook metadata", hook is not None)
        evidence.report["zap_hook"] = hook
    else:
        _, info, _ = evidence.run(["brew", "info", "--json=v2", "--formula", TAP])
        spec = json.loads(info)["formulae"][0]
        stable = spec["urls"]["stable"]
        name = "koidrive-aarch64-apple-darwin.tar.gz"
        evidence.check("tap formula matches release", spec["versions"]["stable"] == evidence.report["tag"][1:]
                       and stable["checksum"] == entries[name]
                       and stable["url"] == RELEASE + "/" + evidence.report["tag"] + "/" + name)


def version(evidence, binary, assertion, env=None):
    status, out, _ = evidence.run([binary, "--version"], env=env, required=False)
    evidence.check(assertion, status == 0 and out == "koi " + evidence.report["tag"][1:] + "\n", out)
    return out.rstrip("\n")


def native_zap_hook(spec):
    scripts = [entry["script"] for artifact in spec.get("artifacts", [])
               for entry in artifact.get("zap", []) if isinstance(entry, dict) and "script" in entry]
    expected = {"executable": str(APP / "Contents/MacOS/Koidrive"),
                "args": ["--unregister-login-item"], "sudo": False}
    if scripts == [expected] and scripts[0].get("sudo") is False:
        return scripts[0]
    return None


def zap_invoked(status, output, executable):
    return status == 0 and "==> Running uninstall script " + executable in output.splitlines()


def absent(*paths):
    if any(Path(path).exists() or Path(path).is_symlink() for path in paths):
        raise RuntimeError("Owned installation still exists")


def binary_conflict(status, output, binary):
    return status > 0 and status != 124 and conflict_text(output, binary) is not None


def conflict_text(output, binary):
    match = re.search(
        r"(?im)^(?:Error: )?It seems there is already a Binary at ['\"]"
        + re.escape(str(binary)) + r"['\"]\.?[^\S\r\n]*$", output)
    return match[0] if match else None


def skipped_link_text(output, binary):
    line = "::warning::It seems there is already a Binary at '" + str(binary) + "' from formula koidrive; skipping link."
    return line if line in output.splitlines() else None


def conflict_state(status, output, binary):
    warning = skipped_link_text(output, binary)
    error = conflict_text(output, binary)
    if status == 0 and warning and not error:
        return "installed-with-skipped-link"
    if binary_conflict(status, output, binary) and not warning:
        return "failed-with-rollback"
    return None


def formula_identity(evidence, link, assertion):
    reported = version(evidence, "koi", assertion)
    return {"resolved": str(link.resolve(strict=True)), "sha256": sha256(link), "version": reported}


def brew_scenario(evidence, root, ownership, entries, conflict):
    brew_prepare(evidence, entries, "cask")
    home = Path.home()
    _, prefix, _ = evidence.run(["brew", "--prefix"])
    link = Path(prefix.strip()) / "bin/koi"
    ownership.claim(link)
    document = ownership.claim(home / "Documents" / ("KoidriveAcceptance-" + uuid.uuid4().hex + ".txt"))
    document.parent.mkdir(exist_ok=True)
    document.write_bytes(b"Preserve this user's document.\n")
    formula_attempted = False
    cask_attempted = False
    try:
        if conflict:
            brew_prepare(evidence, entries, "formula")
            formula_attempted = True
            evidence.run(["brew", "install", "--formula", TAP], timeout=600)
            before = formula_identity(evidence, link, "formula version")
            evidence.report["formula_before"] = before
            cask_attempted = True
            status, out, err = evidence.run(["brew", "install", "--cask", TAP], required=False, timeout=600)
            outcome = conflict_state(status, out + err, link)
            evidence.report["conflict"] = sanitized(skipped_link_text(out + err, link) or conflict_text(out + err, link) or out + err, evidence.secrets).strip()
            evidence.report["conflict_status"] = status
            evidence.report["conflict_state"] = outcome
            evidence.check("binary conflict", outcome is not None, out + err)
            after = formula_identity(evidence, link, "formula version after conflict")
            evidence.report["formula_after"] = after
            evidence.check("formula unchanged", before == after)
            _, casks, _ = evidence.run(["brew", "list", "--cask", "--versions"])
            _, caskroom, _ = evidence.run(["brew", "--caskroom"])
            room = Path(caskroom.strip()) / "koidrive"
            evidence.report["conflict_installation"] = {"app_exists": APP.exists(), "casks": casks,
                                             "caskroom_entries": sorted(str(p.relative_to(room)) for p in room.rglob("*"))}
            if outcome == "installed-with-skipped-link":
                evidence.check("cask conflict state", APP.is_dir() and re.search(
                    r"(?m)^koidrive\s+" + re.escape(evidence.report["tag"][1:]) + r"\s*$", casks), casks)
            else:
                evidence.check("cask conflict state", not APP.exists() and not re.search(r"(?m)^koidrive\s", casks), casks)
                cask_attempted = False
            return
        cask_attempted = True
        evidence.run(["brew", "install", "--cask", TAP], timeout=600)
        version(evidence, "koi", "immediate version")
        evidence.check("bundled helper link", link.is_symlink() and link.resolve() == APP / "Contents/Helpers/koi")
        quarantine(evidence, APP)
        quarantine(evidence, link, required=False)
        state = home / "Library/Application Support/Koidrive"
        state.mkdir(parents=True)
        (state / "acceptance-marker").write_text("owned acceptance state\n")
        evidence.run(["/usr/bin/defaults", "write", "dev.koidrive.Koidrive", "AcceptanceMarker", "-string", "owned"])
        evidence.run(["brew", "uninstall", "--cask", TAP], timeout=180)
        cask_attempted = False
        evidence.check("plain uninstall", not APP.exists() and not link.exists() and not link.is_symlink())
        evidence.check("plain uninstall preserves documents", document.read_bytes() == b"Preserve this user's document.\n")
        evidence.check("plain uninstall retains app state", state.exists())
        cask_attempted = True
        evidence.run(["brew", "install", "--cask", TAP], timeout=600)
        version(evidence, "koi", "reinstalled version")
        quarantine(evidence, APP)
        status, out, err = evidence.run(["brew", "uninstall", "--zap", "--cask", "--verbose", TAP], timeout=180)
        cask_attempted = False
        evidence.check("zap native unregister", zap_invoked(status, out + err, evidence.report["zap_hook"]["executable"]), out + err)
        assert_native_absent(evidence)
        evidence.check("zap removes state", not state.exists()
                       and not (home / "Library/Preferences/dev.koidrive.Koidrive.plist").exists()
                       and not APP.exists() and not link.exists() and not link.is_symlink())
        evidence.check("zap preserves documents", document.read_bytes() == b"Preserve this user's document.\n")
    finally:
        if cask_attempted:
            evidence.cleanup("cask uninstall and zap", lambda: evidence.run(["brew", "uninstall", "--zap", "--cask", TAP]))
            if conflict and formula_attempted:
                def formula_survives():
                    identity = formula_identity(evidence, link, "formula version after cask uninstall")
                    evidence.report["formula_after_cask_uninstall"] = identity
                    evidence.check("formula survives cask uninstall", before == identity)

                evidence.cleanup("formula survives cask uninstall", formula_survives)
        if formula_attempted:
            evidence.cleanup("formula uninstall", lambda: evidence.run(["brew", "uninstall", "--formula", TAP]))
        evidence.cleanup("app and binary removed", lambda: absent(APP, link))
        evidence.cleanup("owned fake document", lambda: ownership.remove(document))
        for path in state_paths(home)[3:]:
            evidence.cleanup("owned state " + str(path), lambda p=path: ownership.remove(p))


def koi_json(evidence, binary, *args, env=None):
    _, out, _ = evidence.run([binary, "--json", *args], env=env, timeout=90)
    envelope = json.loads(out)
    if envelope.get("outcome") not in ("ok", "partial", "unchanged"):
        raise RuntimeError("Public CLI command did not report success")
    return envelope["data"]


def worker(evidence, root):
    assets = REPO / "server/web/dist"
    wrangler = REPO / "server/node_modules/wrangler/bin/wrangler.js"
    if not assets.is_dir() or not wrangler.is_file():
        raise RuntimeError("Installer scenario needs installed server dependencies and built web assets")
    config = json.loads("\n".join(line for line in (REPO / "server/wrangler.jsonc").read_text().splitlines()
                                  if not line.lstrip().startswith("//")))
    config.pop("build", None)
    config["main"] = str(REPO / "server/src/worker.ts")
    config["assets"]["directory"] = str(assets)
    path = root / "wrangler.json"
    path.write_text(json.dumps(config, indent=2) + "\n")
    evidence.report["inputs"]["worker-config"] = {"sha256": sha256(path)}
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", 0))
        port = reservation.getsockname()[1]
    item = evidence.start(["node", wrangler, "dev", "--config", path, "--ip", "127.0.0.1",
                           "--port", port, "--persist-to", root / "worker-state",
                           "--show-interactive-dev-session=false"])
    process, _, out, err = item
    if not until(lambda: "Ready on" in out.raw + err.raw or process.poll() is not None, 90) or process.poll() is not None:
        raise RuntimeError("Real local Worker did not become ready")
    url = f"http://127.0.0.1:{port}"
    with urllib.request.build_opener(urllib.request.ProxyHandler({})).open(url + "/setup", timeout=15) as response:
        response.read()
    pattern = r"Koidrive claim code \(valid for 1 hour\): ([A-Z0-9-]+)"
    if not until(lambda: re.search(pattern, out.raw + err.raw), 20):
        raise RuntimeError("Worker did not emit its public claim code")
    claim = re.search(pattern, out.raw + err.raw)[1]
    evidence.secrets.append(claim)
    return url, claim


def pipeline(script):
    return "curl -fsSL -- " + shlex.quote(Path(script).resolve().as_uri()) + " | sh"


def attended(evidence, command, folder, timeout=360):
    import pty

    expected = [(r"Which shared folder should sync here\?\r?\n  1\. Journey\r?\n  2\. Create a shared folder\r?\nChoose 1 to 2: ", "1\n"),
                (re.escape("Press Enter to use this folder, or type another path: "), str(folder) + "\n"),
                (re.escape(f"Create {folder}? [Y/n] "), "y\n")]
    master, slave = pty.openpty()
    process = None
    transcript = ""
    answers = 0
    consumed = 0
    number = len(evidence.report["commands"]) + 1
    record = {"argv": ["/bin/sh", "-c", command], "status": None,
              "stdout": f"{number:03d}.stdout", "stderr": f"{number:03d}.stderr",
              "terminal": "controlling PTY; installer sh stdin is curl pipe"}
    evidence.report["commands"].append(record)
    (evidence.directory / record["stderr"]).write_text("")
    env = {**evidence.env, "TERM": "dumb", "KOIDRIVE_VERSION": evidence.report["tag"],
           "SHELL": "/bin/zsh", "PATH": SYSTEM_PATH}
    record["initial_path"] = SYSTEM_PATH
    env.pop("CI", None)
    bootstrap = "import fcntl,os,sys,termios; fcntl.ioctl(0,termios.TIOCSCTTY,0); os.execv('/bin/sh',['/bin/sh','-c',sys.argv[1]])"
    try:
        process = OwnedProcess([sys.executable, "-c", bootstrap, command], env=env, cwd=REPO,
                               stdin=slave, stdout=slave, stderr=slave)
        record["group"] = {"pgid": process.pid, "uid": process.group_uid, "observations": process.group_observations}
        os.close(slave)
        slave = None
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            ready, _, _ = select.select([master], [], [], 0.1)
            if not ready:
                if process.poll() is not None:
                    break
                continue
            try:
                chunk = os.read(master, 65536)
            except OSError as error:
                if error.errno == errno.EIO:
                    break
                raise
            if not chunk:
                break
            transcript += chunk.decode("utf-8", errors="replace")
            if answers < len(expected):
                pattern, answer = expected[answers]
                match = re.search(pattern, transcript[consumed:])
                if match:
                    consumed += match.end()
                    os.write(master, answer.encode())
                    answers += 1
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            stop_group(process)
            record["status"] = 124
        else:
            record["status"] = process.returncode
        return record["status"], transcript, answers
    finally:
        try:
            if process is not None:
                stop_group(process)
        finally:
            if slave is not None:
                os.close(slave)
            os.close(master)
            (evidence.directory / record["stdout"]).write_text(sanitized(transcript, evidence.secrets))


def service_cleanup(evidence, profiles, binaries, timeout=15):
    agents = Path.home() / "Library/LaunchAgents"
    owned = []
    for path in sorted(agents.glob("dev.koidrive.koi*.plist")):
        spec = plistlib.loads(path.read_bytes())
        state = spec.get("EnvironmentVariables", {}).get("XDG_STATE_HOME")
        label = spec.get("Label")
        if (state not in profiles or spec.get("ProgramArguments") not in [[str(b), "_daemon"] for b in binaries]
                or not isinstance(label, str) or not re.fullmatch(r"dev\.koidrive\.koi(?:\.[0-9a-f]{8})?", label)
                or path.name != label + ".plist"):
            raise RuntimeError("Refusing foreign launch agent " + str(path))
        owned.append((path, label))
    errors = []
    for path, label in owned:
        try:
            status, _, err = evidence.run(["/bin/launchctl", "bootout", f"gui/{os.getuid()}/{label}"], required=False)
            if status != 0 and not unloaded(status, err):
                raise RuntimeError("Owned service bootout failed: " + label)
            wait_unloaded(evidence, label, timeout)
            path.unlink()
        except Exception as error:
            errors.append(str(error))
    if errors:
        raise RuntimeError("; ".join(errors))
    return [label for _, label in owned]


def wait_unloaded(evidence, label, timeout=15):
    deadline = time.monotonic() + timeout
    while True:
        query_timeout = max(0.01, min(5, deadline - time.monotonic()))
        status, _, error = evidence.run(["/bin/launchctl", "print", f"gui/{os.getuid()}/{label}"], required=False,
                                        timeout=query_timeout)
        if unloaded(status, error):
            return
        remaining = deadline - time.monotonic()
        if status != 0 or remaining <= 0:
            raise RuntimeError("Could not prove owned service unloaded: " + label)
        time.sleep(min(0.25, remaining))


def stop_app(evidence, app):
    executable = str(app / "Contents/MacOS/Koidrive")
    _, listing, _ = evidence.run(["/bin/ps", "-ww", "-axo", "pid=,comm="])
    for line in listing.splitlines():
        fields = line.strip().split(maxsplit=1)
        if len(fields) == 2 and fields[1] == executable:
            os.kill(int(fields[0]), signal.SIGTERM)
    if not until(lambda: not any(line.strip().endswith(executable) for line in evidence.run(
            ["/bin/ps", "-ww", "-axo", "pid=,comm="])[1].splitlines()), 15):
        raise RuntimeError("Owned installed app did not terminate")


def verified_installer(evidence, root, source="held", reference="accepted"):
    url = INSTALLER_SOURCES[source]
    expected = HELD_SHA
    commit = HELD_COMMIT
    if reference == "release":
        evidence.check("live release installer", source == "live")
        tag_ref = "refs/tags/" + evidence.report["tag"]
        _, head, _ = evidence.run(["git", "rev-parse", "HEAD"])
        _, listing, _ = evidence.run(["git", "ls-remote", "--exit-code", "https://github.com/koidrive/koidrive.git", tag_ref, tag_ref + "^{}"])
        refs = {}
        for line in listing.splitlines():
            fields = line.split()
            if (len(fields) != 2 or not re.fullmatch(r"[0-9a-f]{40}", fields[0])
                    or fields[1] not in (tag_ref, tag_ref + "^{}") or fields[1] in refs):
                raise RuntimeError("Malformed release-ref resolution")
            refs[fields[1]] = fields[0]
        commit = refs.get(tag_ref + "^{}", refs.get(tag_ref))
        evidence.check("release ref checked out", tag_ref in refs and bool(commit) and head.strip() == commit)
        expected = sha256(REPO / "install.sh")
    script = root / "install.sh"
    download(evidence, url, script)
    evidence.report["inputs"]["held-installer"] = {"source": source, "reference": reference, "url": url, "commit": commit,
                                                 "sha256": sha256(script), "expected_sha256": expected, "transport": "https"}
    evidence.check("held script verified", evidence.report["inputs"]["held-installer"]["sha256"] == expected)
    return script


def installer(evidence, root, ownership, assets, source="held", reference="accepted"):
    home = Path.home()
    binary = home / ".local/bin/koi"
    profile = home / ".zprofile"
    if profile.is_symlink():
        raise RuntimeError("Refusing symlinked shell profile")
    original = profile.read_bytes() if profile.exists() else None
    mode = profile.stat().st_mode & 0o777 if profile.exists() else 0o600
    script = verified_installer(evidence, root, source, reference)
    bootstrap = root / "bootstrap"
    bootstrap.mkdir()
    with tarfile.open(assets["koidrive-aarch64-apple-darwin.tar.gz"], "r:gz") as archive:
        members = [member for member in archive.getmembers() if member.name == "koi"]
        if len(members) != 1 or not members[0].isfile() or members[0].size == 0:
            raise RuntimeError("Release tarball must contain exactly one nonempty regular koi")
        with archive.extractfile(members[0]) as stream:
            (bootstrap / "koi").write_bytes(stream.read())
    (bootstrap / "koi").chmod(0o755)
    evidence.report["inputs"]["released-cli"] = {"archive": "koidrive-aarch64-apple-darwin.tar.gz", "sha256": sha256(bootstrap / "koi")}
    quarantine(evidence, bootstrap / "koi", required=False)
    version(evidence, bootstrap / "koi", "bootstrap version")
    peer_env = {**evidence.env, "XDG_CONFIG_HOME": str(root / "peer/config"),
                "XDG_STATE_HOME": str(root / "peer/state"), "XDG_RUNTIME_DIR": str(root / "peer/runtime")}
    profiles = {str(home / ".local/state"), peer_env["XDG_STATE_HOME"]}
    owned_labels = {"dev.koidrive.koi"}
    try:
        url, claim = worker(evidence, root)
        koi_json(evidence, bootstrap / "koi", "server", "claim", url, claim, "--user", "Journey", "--name", "Journey")
        space = koi_json(evidence, bootstrap / "koi", "space", "create", "Journey")
        status = koi_json(evidence, bootstrap / "koi", "status")
        evidence.check("unlinked before setup", bool(space.get("space_id")) and not status.get("folders"))
        folder = root / "Sync"
        code, transcript, answers = attended(evidence, pipeline(script), folder)
        evidence.check("attended setup", code == 0 and answers == 3
                       and f"Done. Linked {folder} to Journey." in transcript,
                       f"status={code}, answered={answers}")
        version(evidence, binary, "installed version")
        evidence.check("installed CLI matches released bytes", sha256(binary) == sha256(bootstrap / "koi"))
        _, fresh_version, _ = evidence.run(["/bin/zsh", "-d", "-l", "-c", "command -v koi; koi --version"],
                                           env={**evidence.env, "PATH": SYSTEM_PATH})
        evidence.report["fresh_shell_initial_path"] = SYSTEM_PATH
        evidence.check("fresh login shell version", fresh_version == str(binary) + "\nkoi " + evidence.report["tag"][1:] + "\n")
        app = APP if APP.exists() else home / "Applications/Koidrive.app"
        evidence.check("installed app", app.is_dir() and (app / "Contents/Helpers/koi").is_file())
        initial_attrs = {str(path): quarantine(evidence, path, required=False) for path in (binary, app)}
        evidence.report["coverage"]["shell_quarantine_observed"] = {
            path: bool(quarantine_paths(attributes)) for path, attributes in initial_attrs.items()}
        _, app_version, _ = evidence.run(["/usr/libexec/PlistBuddy", "-c", "Print :KoidriveVersion", app / "Contents/Info.plist"])
        evidence.check("installed app version", app_version.strip() == evidence.report["tag"][1:])
        status = koi_json(evidence, binary, "status")
        agents = launchd_state(evidence)
        evidence.check("healthy native service", status.get("running") is True
                       and status.get("service", {}).get("state") == "running"
                       and "dev.koidrive.koi" in agents
                       and any(f.get("path") == str(folder) for f in status.get("folders", [])))
        invite = koi_json(evidence, binary, "device", "invite")
        join = invite["join_argv"]
        if (not isinstance(join, list) or len(join) != 5 or join[:2] != ["koi", "join"]
                or join[3] != "--server" or join[4].rstrip("/") != url):
            raise RuntimeError("Public device invite returned an unexpected command")
        evidence.secrets.append(join[2])
        koi_json(evidence, binary, *join[1:], "--name", "Peer", env=peer_env)
        peer_folder = root / "PeerSync"
        peer_folder.mkdir()
        koi_json(evidence, binary, "folder", "link", peer_folder, "--space", "Journey", env=peer_env)
        transfers = []
        for source, destination, name in ((folder, peer_folder, "outbound.bin"), (peer_folder, folder, "inbound.bin")):
            content = os.urandom(4096)
            (source / name).write_bytes(content)
            target = destination / name
            received = until(lambda: target.is_file() and target.read_bytes() == content, 90)
            transfers.append({"name": name, "sha256": hashlib.sha256(content).hexdigest(), "passed": received})
        evidence.check("exact byte sync", all(t["passed"] for t in transfers), json.dumps(transfers))
        final_attrs = {path: quarantine(evidence, path, required=False) for path in initial_attrs}
        evidence.check("xattrs preserved", all(quarantine_paths(initial) <= quarantine_paths(final_attrs[path])
                       for path, initial in initial_attrs.items()))
        evidence.report["coverage"]["menu_attachment"] = "Not asserted; no GUI automation or menu-bar probe. App installation, native service and released-byte sync are required."
    finally:
        safe = True
        for app in (APP, home / "Applications/Koidrive.app"):
            if app.exists():
                safe &= evidence.cleanup("terminate exact installed app", lambda a=app: stop_app(evidence, a))
                safe &= evidence.cleanup("native login unregister", lambda a=app: evidence.run([a / "Contents/MacOS/Koidrive", "--unregister-login-item"]))
        binaries = [binary, APP / "Contents/Helpers/koi", home / "Applications/Koidrive.app/Contents/Helpers/koi"]
        safe &= evidence.cleanup("owned native services", lambda: owned_labels.update(service_cleanup(evidence, profiles, binaries)))
        if safe:
            for path in state_paths(home):
                evidence.cleanup("owned state " + str(path), lambda p=path: ownership.remove(p))

        def restore_profile():
            if original is None:
                profile.unlink(missing_ok=True)
            else:
                profile.write_bytes(original)
                profile.chmod(mode)

        evidence.cleanup("restore shell profile", restore_profile)
        evidence.cleanup("no loaded owned services", lambda: assert_native_absent(evidence, owned_labels))


def assert_native_absent(evidence, labels=("dev.koidrive.koi",)):
    for label in labels:
        status, _, error = evidence.run(["/bin/launchctl", "print", f"gui/{os.getuid()}/{label}"], required=False)
        if not unloaded(status, error):
            raise RuntimeError("Could not prove owned service unloaded: " + label)


def unloaded(status, error):
    return status > 0 and status != 124 and ("Could not find service" in error or "No such process" in error)


def main(argv=None):
    parser = argparse.ArgumentParser()
    parser.add_argument("--tag", required=True, type=tag_value)
    parser.add_argument("--scenario", required=True, choices=SCENARIOS)
    parser.add_argument("--evidence", required=True, type=Path)
    parser.add_argument("--installer-source", choices=INSTALLER_SOURCES, default="held")
    parser.add_argument("--installer-reference", choices=("accepted", "release"), default="accepted")
    args = parser.parse_args(argv)
    evidence = Evidence(args.evidence, args.tag, args.scenario)
    root = None
    try:
        hosted_guard()
        _, version_output, _ = evidence.run(["/usr/bin/sw_vers", "-productVersion"])
        evidence.check("macOS 26", version_output.strip().split(".")[0] == "26", version_output)
        root = Path(tempfile.mkdtemp(prefix="koidrive-release-", dir=os.environ["RUNNER_TEMP"]))
        assets, entries = release_assets(evidence, root)
        if args.scenario == "verify":
            evidence.run(["/bin/bash", REPO / "apps/macos/scripts/verify-release.sh",
                          assets[f"Koidrive-{args.tag[1:]}-arm64.dmg"], "signed-static"],
                         env={**evidence.env, "VERSION": args.tag[1:]}, timeout=600)
            evidence.check("signed static verification", True)
        else:
            ownership = Ownership()
            clean_account(evidence, ownership)
            if args.scenario == "installer":
                installer(evidence, root, ownership, assets, args.installer_source, args.installer_reference)
            else:
                brew_scenario(evidence, root, ownership, entries, args.scenario == "formula-conflict")
    except (Exception, KeyboardInterrupt) as error:
        evidence.report["errors"].append(sanitized(str(error), evidence.secrets))
    finally:
        for process, record, _, _ in evidence.processes:
            if not process.group_closed:
                evidence.cleanup("stop owned process group", lambda p=process: stop_group(p), group=record["group"])
        if root is not None:
            if all(item["passed"] for item in evidence.report["cleanup"]):
                evidence.cleanup("owned temporary directory", lambda: shutil.rmtree(root))
            else:
                evidence.report["retained_state"] = str(root)
        else:
            evidence.report["cleanup"].append({"name": "no runtime mutation", "passed": True})
        evidence.close()
    return 0 if evidence.report["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
