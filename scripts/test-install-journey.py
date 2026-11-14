#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
import errno
import hashlib
import os
import platform
import plistlib
from pathlib import Path
import pty
import select
import shlex
import shutil
import signal
import subprocess
import sys
import tarfile
import tempfile
import textwrap
import time
import unittest

ROOT = Path(__file__).resolve().parent.parent


class InstallJourney(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.certificates = tempfile.TemporaryDirectory(prefix="koidrive-install-tls-")
        cls.tls = Path(cls.certificates.name)
        cls.keys = tempfile.TemporaryDirectory(prefix="koidrive-install-keys-")
        keys = Path(cls.keys.name)
        for name in ("release", "other"):
            subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", name, "-f", str(keys / name)], check=True)
        cls.release_key, cls.other_key = keys / "release", keys / "other"
        pinned = (ROOT / ".github/release-allowed-signers").read_text().split()[3]
        cls.installer = keys / "install.sh"
        cls.installer.write_text((ROOT / "install.sh").read_text().replace(pinned, (keys / "release.pub").read_text().split()[1]))

    @classmethod
    def tearDownClass(cls):
        cls.certificates.cleanup()
        cls.keys.cleanup()

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="koidrive-install-journey-")
        self.addCleanup(self.temp.cleanup)
        self.dir = Path(self.temp.name)
        self.home = self.dir / "home"
        self.home.mkdir()
        self.bin = self.dir / "tools"
        self.bin.mkdir()
        self.env = {
            "HOME": str(self.home),
            "PATH": f"{self.bin}:/usr/bin:/bin",
            "SHELL": "/bin/zsh",
            "KOIDRIVE_VERSION": "0.0.0",
            "KOIDRIVE_RELEASE_BASE_URL": self.serve("releases", "--tls", str(self.tls)) + "/releases",
            "CURL_CA_BUNDLE": str(self.tls / "cert.pem"),
            "TERM": "xterm",
        }
        self.tool("uname", 'case "$1" in -s) echo "${TEST_OS:-Darwin}";; -m) echo arm64;; esac')
        for tool in ("launchctl", "systemctl", "ps"):
            self.tool(tool, f'printf "%s\\n" "{tool} $*" >> "$HOME/services.log"\nexit 1')
        self.tool("codesign", 'printf "%s\\n" "$*" >> "$HOME/codesign.log"')
        self.fixture = self.dir / "koi"
        self.fixture.write_text(textwrap.dedent("""\
            #!/bin/sh
            set -eu
            if [ "${1:-}" = --version ]; then
                echo 'koi 0.0.0 Koidrive sync client'
                exit
            fi
            printf '%s\\n' "$0" "$@" > "$HOME/setup.args"
            [ -t 0 ] && [ -t 2 ] || exit 91
            printf 'SETUP_READY\\n'
            IFS= read -r answer
            printf '%s\\n' "$answer" > "$HOME/setup.answer"
            exit "${TEST_SETUP_EXIT:-0}"
        """))
        self.fixture.chmod(0o755)
        self.publish(self.fixture)

    def serve(self, name, *options):
        state = self.dir / "servers" / name
        state.mkdir(parents=True)
        with (state / "stderr").open("wb") as stderr:
            server = subprocess.Popen(
                [sys.executable, "-X", "faulthandler", str(ROOT / "scripts/release-fixture-server.py"), str(self.dir), str(state), *options],
                stdin=subprocess.DEVNULL, stderr=stderr,
            )
        self.addCleanup(server.wait)
        self.addCleanup(server.kill)
        deadline = time.monotonic() + 10
        while not (state / "origin").exists():
            if server.poll() is None and time.monotonic() > deadline:
                server.send_signal(signal.SIGABRT)
                server.wait()
            if server.poll() is not None:
                self.fail(f"release server {name} did not start:\n{(state / 'stderr').read_text()}")
            time.sleep(0.02)
        return (state / "origin").read_text()

    def requests(self, name):
        return (self.dir / "servers" / name / "requests").read_text().splitlines()

    def publish(self, koi):
        release = self.dir / "releases/download/v0.0.0"
        release.mkdir(parents=True, exist_ok=True)
        sums = []
        for platform in ("aarch64-apple-darwin", "aarch64-unknown-linux-musl"):
            archive = release / f"koidrive-{platform}.tar.gz"
            with tarfile.open(archive, "w:gz") as tar:
                tar.add(koi, arcname="koi")
            sums.append(f"{hashlib.sha256(archive.read_bytes()).hexdigest()}  {archive.name}\n")
        sums.append(f"{'0' * 64}  Koidrive-0.0.0-arm64.dmg\n")
        self.sums = release / "SHA256SUMS"
        self.sums.write_text("".join(sums))
        self.sign()

    def sign(self, key=None, namespace="koidrive-release"):
        self.sums.with_name("SHA256SUMS.sig").unlink(missing_ok=True)
        subprocess.run(
            ["ssh-keygen", "-q", "-Y", "sign", "-f", str(key or self.release_key), "-n", namespace, str(self.sums)],
            check=True, capture_output=True,
        )

    def tool(self, name, body):
        file = self.bin / name
        file.write_text("#!/bin/sh\n" + body + "\n")
        file.chmod(0o755)

    def install(self, *args, terminal=True, piped=True, env=None, redirect=None):
        environment = self.env | (env or {})
        installer = shlex.quote(str(self.installer))
        options = " ".join(shlex.quote(arg) for arg in args)
        command = f"cat {installer} | /bin/sh -s -- {options}" if piped else f"/bin/sh {installer} {options}"
        if redirect:
            command += f' {"2>" if redirect == "stderr" else ">"} "$HOME/redirect.log"'
        if not terminal:
            result = subprocess.run(
                ["/bin/sh", "-c", command], env=environment, input="",
                capture_output=True, text=True, timeout=15,
            )
            return result.returncode, result.stdout + result.stderr
        pid, fd = pty.fork()
        if pid == 0:
            os.execve("/bin/sh", ["/bin/sh", "-c", command], environment)
        output = bytearray()
        answered = False
        deadline = time.monotonic() + 15
        try:
            while time.monotonic() < deadline:
                if not select.select([fd], [], [], 0.1)[0]:
                    continue
                try:
                    chunk = os.read(fd, 65536)
                except OSError as error:
                    if error.errno == errno.EIO:
                        break
                    raise
                if not chunk:
                    break
                output.extend(chunk)
                if not answered and b"SETUP_READY" in output:
                    os.write(fd, b"continue\n")
                    answered = True
            else:
                self.fail("installer did not finish before the terminal timeout")
            _, status = os.waitpid(pid, 0)
            pid = None
            return os.waitstatus_to_exitcode(status), output.decode().replace("\r\n", "\n")
        finally:
            os.close(fd)
            if pid is not None:
                os.killpg(pid, signal.SIGKILL)
                os.waitpid(pid, 0)

    def successful_install(self, *args, **kwargs):
        code, output = self.install(*args, **kwargs)
        self.assertEqual(code, 0, output)
        self.assertEqual((self.home / ".local/bin/koi").read_bytes(), self.fixture.read_bytes())
        return output

    def refused_install(self, message, *args, **kwargs):
        installed = self.home / ".local/bin/koi"
        installed.parent.mkdir(parents=True, exist_ok=True)
        installed.write_text("old Koidrive sync client")
        code, output = self.install(*args, **kwargs)
        self.assertNotEqual(code, 0, output)
        self.assertIn(message, output)
        self.assertEqual(installed.read_text(), "old Koidrive sync client")
        self.assertFalse((self.home / "setup.args").exists())
        self.assertFalse((self.home / ".zprofile").exists())
        return output

    def test_piped_mac_installer_enters_setup_with_terminal_stdin(self):
        output = self.successful_install()
        self.assertEqual((self.home / "setup.args").read_text().splitlines(), [str(self.home / ".local/bin/koi"), "setup"])
        self.assertEqual((self.home / "setup.answer").read_text(), "continue\n")
        profile = self.home / ".zprofile"
        self.assertIn(f"Updated {profile}:\n+ {profile.read_text()}", output)
        self.assertIn("Signature verified for SHA256SUMS\nSHA256 verified", output)

    def test_no_app_is_forwarded_to_setup(self):
        self.successful_install("--no-app")
        self.assertEqual((self.home / "setup.args").read_text().splitlines()[-2:], ["setup", "--no-app"])

    def test_setup_failure_is_not_reported_as_success(self):
        code, _ = self.install(env={"TEST_SETUP_EXIT": "23"})
        self.assertEqual(code, 23)
        self.assertTrue((self.home / ".local/bin/koi").is_file())

    def test_profiles_are_idempotent_and_preserve_existing_text(self):
        for shell, profile in (("zsh", ".zprofile"), ("bash", ".bash_profile"), ("fish", ".config/fish/conf.d/koidrive.fish")):
            with self.subTest(shell=shell):
                file = self.home / profile
                file.parent.mkdir(parents=True, exist_ok=True)
                file.write_text("export EXISTING=kept\n" if shell != "fish" else "set -gx EXISTING kept\n")
                file.chmod(0o600)
                self.successful_install("--no-setup", env={"SHELL": f"/bin/{shell}"})
                first = file.read_text()
                output = self.successful_install("--no-setup", env={"SHELL": f"/bin/{shell}"})
                self.assertEqual(file.read_text(), first)
                self.assertEqual(first.count("koidrive PATH"), 1)
                self.assertIn("EXISTING", first)
                self.assertEqual(file.stat().st_mode & 0o777, 0o600)
                self.assertNotIn(f"Updated {file}", output)

    def test_profiles_make_koi_available_without_duplicate_entries(self):
        for shell, profile in (("zsh", ".zprofile"), ("bash", ".bash_profile")):
            executable = shutil.which(shell)
            self.assertIsNotNone(executable, f"missing shell: {shell}")
            with self.subTest(shell=shell):
                self.successful_install("--no-setup", env={"SHELL": f"/bin/{shell}"})
                file = shlex.quote(str(self.home / profile))
                command = f"source {file}; source {file}; command -v koi"
                result = subprocess.run([executable, "-c", command], env=self.env, capture_output=True, text=True, timeout=5)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.strip(), str(self.home / ".local/bin/koi"))
                command = f"source {file}; source {file}; printf '%s\\n' \"$PATH\""
                result = subprocess.run([executable, "-c", command], env=self.env, capture_output=True, text=True, timeout=5)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.count(str(self.home / ".local/bin")), 1)

    def test_profile_short_write_preserves_contents_and_permissions(self):
        for linked in (False, True):
            with self.subTest(linked=linked):
                profile = self.home / ".zprofile"
                profile.unlink(missing_ok=True)
                target = self.home / "saved-profile" if linked else profile
                target.write_text("export EXISTING=kept\nexport SECOND=also_kept\n")
                target.chmod(0o640)
                before = target.read_bytes()
                if linked:
                    profile.symlink_to(target.name)
                self.tool("cat", '''case "$1" in */profile) head -c 7 "$1"; exit 74;; esac
exec /bin/cat "$@"''')
                output = self.successful_install("--no-setup")
                self.assertEqual(target.read_bytes(), before)
                self.assertEqual(target.stat().st_mode & 0o777, 0o640)
                self.assertEqual(profile.is_symlink(), linked)
                if linked:
                    self.assertEqual(os.readlink(profile), target.name)
                self.assertIn("could not update PATH", output)
                self.assertNotIn("Updated ", output)
                self.assertEqual(list(self.home.rglob(".koidrive*")), [])

    def test_profile_rename_failure_preserves_contents_and_permissions(self):
        for linked in (False, True):
            with self.subTest(linked=linked):
                profile = self.home / ".zprofile"
                profile.unlink(missing_ok=True)
                target = self.home / "saved-profile" if linked else profile
                target.write_text("export EXISTING=kept\n")
                target.chmod(0o640)
                before = target.read_bytes()
                if linked:
                    profile.symlink_to(target.name)
                self.tool("mv", '''case "$3" in */.zprofile|*/saved-profile) exit 75;; esac
exec /bin/mv "$@"''')
                output = self.successful_install("--no-setup")
                self.assertEqual(target.read_bytes(), before)
                self.assertEqual(target.stat().st_mode & 0o777, 0o640)
                self.assertEqual(profile.is_symlink(), linked)
                self.assertIn("could not update PATH", output)
                self.assertNotIn("Updated ", output)
                self.assertEqual(list(self.home.rglob(".koidrive*")), [])

    def test_profile_symlink_chain_updates_target_and_preserves_links_and_mode(self):
        target = self.home / "dotfiles/login-profile"
        target.parent.mkdir()
        target.write_text("export EXISTING=kept\n")
        target.chmod(0o640)
        middle = self.home / "profile-link"
        middle.symlink_to(target)
        profile = self.home / ".zprofile"
        profile.symlink_to(middle.name)
        output = self.successful_install("--no-setup")
        self.assertTrue(profile.is_symlink())
        self.assertTrue(middle.is_symlink())
        self.assertEqual(os.readlink(profile), middle.name)
        self.assertEqual(os.readlink(middle), str(target))
        self.assertTrue(target.read_text().startswith("export EXISTING=kept\n"))
        self.assertEqual(target.read_text().count("koidrive PATH"), 1)
        self.assertEqual(target.stat().st_mode & 0o777, 0o640)
        self.assertIn(f"Updated {target}:\n", output)
        self.assertEqual(list(self.home.rglob(".koidrive*")), [])

    @unittest.skipUnless(shutil.which("fish"), "fish is not available")
    def test_fish_profile_quotes_custom_paths_and_is_idempotent(self):
        install_dir = self.home / "custom 'quoted' $bin `test` \\bin"
        code, output = self.install("--no-setup", env={"SHELL": "/bin/fish", "KOIDRIVE_INSTALL_DIR": str(install_dir)})
        self.assertEqual(code, 0, output)
        file = shlex.quote(str(self.home / ".config/fish/conf.d/koidrive.fish"))
        result = subprocess.run(
            [shutil.which("fish"), "--no-config", "-c", f"source {file}; source {file}; command -v koi; printf '%s\\n' $PATH"],
            env=self.env, capture_output=True, text=True, timeout=5,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines()[0], str(install_dir / "koi"))
        self.assertEqual(result.stdout.splitlines()[1:].count(str(install_dir)), 1)

    def test_path_opt_out_flags_and_environment_leave_profiles_alone(self):
        for options, env in ((("--no-modify-path",), {}), ((), {"KOIDRIVE_NO_MODIFY_PATH": "1"})):
            with self.subTest(options=options, env=env):
                output = self.successful_install(*options, env=env)
                self.assertFalse((self.home / ".zprofile").exists())
                self.assertTrue((self.home / "setup.answer").exists())
                self.assertIn("not on PATH", output)

    def test_setup_opt_out_flags_and_environment_still_configure_path(self):
        for options, env in ((("--no-setup",), {}), ((), {"KOIDRIVE_NO_SETUP": "1"})):
            with self.subTest(options=options, env=env):
                output = self.successful_install(*options, env=env)
                self.assertFalse((self.home / "setup.args").exists())
                self.assertTrue((self.home / ".zprofile").exists())
                self.assertIn("To set up Koidrive, run:", output)

    def test_unattended_and_ci_do_not_modify_profiles_or_run_setup(self):
        for terminal, env in ((False, {}), (True, {"CI": "true"}), (True, {"CI": ""}), (True, {"CI": "0"})):
            with self.subTest(terminal=terminal, env=env):
                self.successful_install(terminal=terminal, env=env)
                self.assertFalse((self.home / "setup.args").exists())
                self.assertFalse((self.home / ".zprofile").exists())

    def test_redirecting_either_output_stream_skips_setup_and_profiles(self):
        for stream in ("stdout", "stderr"):
            with self.subTest(stream=stream):
                self.successful_install(redirect=stream)
                self.assertFalse((self.home / "setup.args").exists())
                self.assertFalse((self.home / ".zprofile").exists())

    def test_terminal_output_without_a_controlling_terminal_is_unattended(self):
        master, slave = pty.openpty()
        try:
            result = subprocess.run(
                ["/bin/sh", str(self.installer)], stdin=subprocess.DEVNULL,
                stdout=slave, stderr=slave, start_new_session=True, env=self.env, timeout=15,
            )
        finally:
            os.close(slave)
            os.close(master)
        self.assertEqual(result.returncode, 0)
        self.assertFalse((self.home / "setup.args").exists())
        self.assertFalse((self.home / ".zprofile").exists())

    def test_linux_prints_setup_instructions_without_starting_setup(self):
        output = self.successful_install(env={"TEST_OS": "Linux"})
        self.assertFalse((self.home / "setup.args").exists())
        self.assertIn("To set up Koidrive, run:", output)

    def test_path_already_present_leaves_profiles_alone(self):
        self.successful_install(env={"PATH": f"{self.home}/.local/bin:{self.env['PATH']}"})
        self.assertFalse((self.home / ".zprofile").exists())

    def test_bash_login_preserves_existing_profile_precedence(self):
        for fallback in (".profile", ".bash_login"):
            with self.subTest(fallback=fallback):
                (self.home / ".bash_profile").unlink(missing_ok=True)
                (self.home / fallback).write_text(f"export EXISTING={fallback}\n")
                output = self.successful_install("--no-setup", env={"SHELL": "/bin/bash"})
                profile = (self.home / ".bash_profile").read_text()
                self.assertEqual(profile.count("koidrive PATH"), 1)
                for line in profile.splitlines():
                    self.assertIn(f"+ {line}\n", output)
                result = subprocess.run(
                    ["/bin/bash", "-lc", 'printf "%s\\n" "$EXISTING"; command -v koi'],
                    env=self.env, capture_output=True, text=True, timeout=5,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.splitlines(), [fallback, str(self.home / ".local/bin/koi")])

    def test_zsh_login_respects_exported_zdotdir(self):
        zdotdir = self.home / "zsh-config"
        self.successful_install("--no-setup", env={"ZDOTDIR": str(zdotdir)})
        self.assertFalse((self.home / ".zprofile").exists())
        result = subprocess.run(
            [shutil.which("zsh"), "-lc", "command -v koi"],
            env=self.env | {"ZDOTDIR": str(zdotdir)}, capture_output=True, text=True, timeout=5,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), str(self.home / ".local/bin/koi"))

    def test_symlinked_install_directory_on_path_leaves_profiles_alone(self):
        installed = self.home / ".local/bin"
        installed.mkdir(parents=True)
        linked = self.dir / "linked"
        linked.symlink_to(installed)
        self.successful_install(env={"PATH": f"{linked}:{self.env['PATH']}"})
        self.assertFalse((self.home / ".zprofile").exists())

    def test_other_koi_on_path_is_never_used_for_setup(self):
        self.tool("koi", 'echo WRONG_PROGRAM; exit 92')
        output = self.successful_install()
        self.assertNotIn("WRONG_PROGRAM", output)
        self.assertIn("another program", output)
        self.assertEqual((self.home / "setup.args").read_text().splitlines()[0], str(self.home / ".local/bin/koi"))

    def test_custom_path_is_quoted_and_one_marked_entry_is_replaced(self):
        self.successful_install("--no-setup")
        previous = (self.home / ".zprofile").read_text().strip()
        install_dir = self.home / "custom 'quoted' $bin `test` \\bin"
        code, output = self.install("--no-setup", env={"KOIDRIVE_INSTALL_DIR": str(install_dir)})
        self.assertEqual(code, 0, output)
        profile = (self.home / ".zprofile").read_text()
        self.assertEqual(profile.count("koidrive PATH"), 1)
        self.assertIn(f"- {previous}\n+ {profile}", output)
        for shell in ("/bin/bash", "/bin/zsh"):
            result = subprocess.run([shell, "-c", 'source "$HOME/.zprofile"; command -v koi'], env=self.env, capture_output=True, text=True, timeout=5)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout.strip(), str(install_dir / "koi"))

    def test_fish_uses_xdg_config_home(self):
        config = self.home / "custom-config"
        self.successful_install("--no-setup", env={"SHELL": "/usr/bin/fish", "XDG_CONFIG_HOME": str(config)})
        self.assertTrue((config / "fish/conf.d/koidrive.fish").exists())

    def test_unknown_shell_prints_instructions_and_still_runs_setup(self):
        output = self.successful_install(env={"SHELL": "/bin/unknown"})
        self.assertFalse((self.home / ".zprofile").exists())
        self.assertIn("not on PATH", output)
        self.assertTrue((self.home / "setup.answer").exists())

    def test_checksum_failure_preserves_cli_and_does_not_start_setup(self):
        installed = self.home / ".local/bin/koi"
        installed.parent.mkdir(parents=True)
        installed.write_text("old Koidrive sync client")
        self.sums.write_text(self.sums.read_text().replace(self.sums.read_text()[:64], "0" * 64))
        self.sign()
        code, output = self.install()
        self.assertNotEqual(code, 0)
        self.assertIn("Checksum mismatch", output)
        self.assertEqual(installed.read_text(), "old Koidrive sync client")
        self.assertFalse((self.home / "setup.args").exists())
        self.assertFalse((self.home / ".zprofile").exists())

    def test_installer_pins_the_published_release_key(self):
        pinned = (ROOT / ".github/release-allowed-signers").read_text().strip()
        self.assertEqual((ROOT / "install.sh").read_text().count(f"'{pinned}'"), 1)
        self.assertNotIn(pinned.split()[3], self.installer.read_text())

    def test_release_signed_with_another_key_is_refused_before_entries_are_read(self):
        for key, namespace in ((self.other_key, "koidrive-release"), (self.release_key, "file")):
            with self.subTest(key=key.name, namespace=namespace):
                self.sums.write_text("not a checksum entry\n")
                self.sign(key, namespace)
                output = self.refused_install("SHA256SUMS is not signed with the Koidrive release key")
                self.assertNotIn("entry", output)

    def test_signed_checksums_of_another_version_are_refused(self):
        self.sums.write_text(self.sums.read_text().replace("Koidrive-0.0.0-arm64.dmg", "Koidrive-0.0.1-arm64.dmg"))
        self.sign()
        self.refused_install("SHA256SUMS does not belong to v0.0.0")

    def test_release_without_signature_is_refused(self):
        self.sums.with_name("SHA256SUMS.sig").unlink()
        self.refused_install("Could not download SHA256SUMS.sig, so the release cannot be verified; the existing installation has not changed. Retry, or set KOIDRIVE_VERSION to v0.1.0-alpha.4 or later")

    def test_missing_or_old_ssh_keygen_warns_and_still_installs(self):
        self.sign(self.other_key)
        system = self.dir / "system"
        system.mkdir()
        for directory in ("/usr/bin", "/bin"):
            for entry in os.scandir(directory):
                if entry.name != "ssh-keygen" and not os.path.lexists(system / entry.name):
                    (system / entry.name).symlink_to(entry.path)
        warning = "Warning: ssh-keygen from OpenSSH 8.1 or later was not found, so the release signature was not checked"
        with self.subTest(ssh_keygen="missing"):
            output = self.successful_install("--no-setup", env={"PATH": f"{self.bin}:{system}"})
            self.assertIn(warning, output)
        self.tool("ssh-keygen", 'echo "usage: ssh-keygen [-q] [-b bits] [-t dsa | ecdsa | ed25519 | rsa]" >&2\nexit 1')
        with self.subTest(ssh_keygen="without -Y"):
            output = self.successful_install("--no-setup")
            self.assertIn(warning, output)
            self.assertNotIn("Signature verified", output)

    def test_release_base_must_be_https(self):
        plain = self.serve("plain")
        for base in (plain + "/releases", (self.dir / "releases").as_uri()):
            with self.subTest(base=base):
                self.refused_install("KOIDRIVE_RELEASE_BASE_URL must be an https:// URL", env={"KOIDRIVE_RELEASE_BASE_URL": base})
        self.assertEqual(self.requests("plain"), [])
        self.assertEqual(self.requests("releases"), [])

    def test_https_to_http_redirect_of_either_download_is_refused(self):
        plain = self.serve("plain")
        for name in ("koidrive-aarch64-apple-darwin.tar.gz", "SHA256SUMS", "SHA256SUMS.sig"):
            with self.subTest(redirected=name):
                origin = self.serve(f"redirect-{name}", "--tls", str(self.tls), "--redirect", plain, "--only", name)
                self.refused_install("curl: (1)", env={"KOIDRIVE_RELEASE_BASE_URL": origin + "/releases"})
                self.assertIn(f"/releases/download/v0.0.0/{name}", self.requests(f"redirect-{name}"))
        self.assertEqual(self.requests("plain"), [])

    def test_https_redirects_are_followed(self):
        mirror = self.serve("mirror", "--tls", str(self.tls))
        origin = self.serve("redirect", "--tls", str(self.tls), "--redirect", mirror)
        self.successful_install("--no-setup", env={"KOIDRIVE_RELEASE_BASE_URL": origin + "/releases"})
        self.assertEqual(sorted(self.requests("mirror")), ["/releases/download/v0.0.0/SHA256SUMS", "/releases/download/v0.0.0/SHA256SUMS.sig", "/releases/download/v0.0.0/koidrive-aarch64-apple-darwin.tar.gz"])

    def test_version_must_be_one_whole_version(self):
        for version in ("0.0.0\n../..", "0.0.0\n", "\n0.0.0", "../../bad", "0.0", "0.0.0 ", "0.0.0/../x"):
            with self.subTest(version=version):
                self.refused_install("KOIDRIVE_VERSION must be a version such as v0.1.0", env={"KOIDRIVE_VERSION": version})
        self.assertEqual(self.requests("releases"), [])
        for version in ("v0.0.0", "0.0.0"):
            with self.subTest(version=version):
                self.successful_install("--no-setup", env={"KOIDRIVE_VERSION": version})

    def test_mac_refuses_koi_that_fails_the_signature_check(self):
        self.tool("codesign", 'printf "%s\\n" "$*" >> "$HOME/codesign.log"\nexit 1')
        self.refused_install("not signed with Koidrive's Developer ID")
        self.assertTrue((self.home / "codesign.log").read_text().startswith("--verify --strict -R ="))

    def test_linux_install_does_not_run_codesign(self):
        self.tool("codesign", 'echo CODESIGN_RAN; exit 1')
        output = self.successful_install(env={"TEST_OS": "Linux"})
        self.assertNotIn("CODESIGN_RAN", output)

    @unittest.skipUnless(platform.system() == "Darwin", "codesign runs only on macOS")
    def test_mac_refuses_koi_without_the_koidrive_developer_id(self):
        (self.bin / "codesign").unlink()
        apple = self.dir / "apple-signed"
        shutil.copy("/usr/bin/true", apple)
        ad_hoc = self.dir / "ad-hoc"
        shutil.copy("/usr/bin/true", ad_hoc)
        subprocess.run(["codesign", "--force", "--sign", "-", str(ad_hoc)], check=True, capture_output=True)
        for koi in (ad_hoc, apple, self.fixture):
            with self.subTest(koi=koi.name):
                self.publish(koi)
                self.refused_install("not signed with Koidrive's Developer ID")

    @unittest.skipUnless(platform.system() == "Darwin", "launchd runs only on macOS")
    def test_install_leaves_a_running_launch_agent_of_another_koi_alone(self):
        domain = f"gui/{os.getuid()}"
        if subprocess.run(["launchctl", "print", domain], capture_output=True).returncode != 0:
            self.skipTest("this session has no launchd GUI domain")
        other = self.dir / "other install"
        other.mkdir()
        (other / "koi").symlink_to("/bin/sleep")
        label = f"dev.koidrive.koi.{os.urandom(4).hex()}"
        plist = self.dir / f"{label}.plist"
        plist.write_bytes(plistlib.dumps({"Label": label, "ProgramArguments": [str(other / "koi"), "600"], "RunAtLoad": True}))
        subprocess.run(["launchctl", "bootstrap", domain, str(plist)], check=True, capture_output=True)
        self.addCleanup(subprocess.run, ["launchctl", "bootout", f"{domain}/{label}"], capture_output=True)

        def agent_pid():
            listing = subprocess.run(["launchctl", "print", f"{domain}/{label}"], capture_output=True, text=True).stdout
            return next((line.split("=", 1)[1].strip() for line in listing.splitlines() if line.strip().startswith("pid =")), None)

        deadline = time.monotonic() + 10
        while (pid := agent_pid()) is None and time.monotonic() < deadline:
            time.sleep(0.1)
        self.assertIsNotNone(pid, "the launch agent did not start")
        for tool in ("launchctl", "ps"):
            (self.bin / tool).unlink()
        output = self.successful_install(terminal=False)
        self.assertNotIn(label, output)
        self.assertEqual(agent_pid(), pid)

    @unittest.skipUnless(platform.system() == "Darwin", "launchd runs only on macOS")
    def test_install_restarts_its_own_launch_agent_in_a_folder_with_format_characters(self):
        domain = f"gui/{os.getuid()}"
        if subprocess.run(["launchctl", "print", domain], capture_output=True).returncode != 0:
            self.skipTest("this session has no launchd GUI domain")
        folder = self.dir / "Tools \U0001F468\u200D\U0001F4BB \u0645\u06CC\u200C\u062E\u0648\u0627\u0647\u0645"
        folder.mkdir()
        self.fixture.write_text(textwrap.dedent("""\
            #!/bin/sh
            if [ "${1:-}" = --version ]; then
                echo 'koi 0.0.0 Koidrive sync client'
                exit
            fi
            exec /bin/sleep 600
        """))
        self.publish(self.fixture)
        (folder / "koi").write_text("#!/bin/sh\n: Koidrive sync client\nexec /bin/sleep 600\n")
        (folder / "koi").chmod(0o755)
        label = f"dev.koidrive.koi.{os.urandom(4).hex()}"
        plist = self.dir / f"{label}.plist"
        plist.write_bytes(plistlib.dumps({"Label": label, "ProgramArguments": [str(folder / "koi"), "_daemon"], "RunAtLoad": True}))
        subprocess.run(["launchctl", "bootstrap", domain, str(plist)], check=True, capture_output=True)
        self.addCleanup(subprocess.run, ["launchctl", "bootout", f"{domain}/{label}"], capture_output=True)

        def agent_pid():
            listing = subprocess.run(["launchctl", "print", f"{domain}/{label}"], capture_output=True, text=True).stdout
            return next((line.split("=", 1)[1].strip() for line in listing.splitlines() if line.strip().startswith("pid =")), None)

        deadline = time.monotonic() + 10
        while (old := agent_pid()) is None and time.monotonic() < deadline:
            time.sleep(0.1)
        self.assertIsNotNone(old, "the launch agent did not start")
        for tool in ("launchctl", "ps"):
            (self.bin / tool).unlink()
        code, output = self.install(terminal=False, env={"KOIDRIVE_INSTALL_DIR": str(folder), "LC_ALL": "C"})
        self.assertEqual(code, 0, output)
        self.assertEqual((folder / "koi").read_bytes(), self.fixture.read_bytes())
        self.assertIn(f"Restarted the background service {label}", output)
        deadline = time.monotonic() + 10
        while agent_pid() in (None, old) and time.monotonic() < deadline:
            time.sleep(0.1)
        self.assertNotIn(agent_pid(), (None, old))

    def test_update_style_captured_child_never_starts_setup(self):
        self.successful_install(terminal=False, piped=False)
        self.assertFalse((self.home / "setup.args").exists())
        self.assertFalse((self.home / ".zprofile").exists())

    def test_help_and_unknown_option_do_not_install(self):
        code, output = self.install("--help")
        self.assertEqual(code, 0, output)
        self.assertIn("KOIDRIVE_NO_SETUP=1", output)
        self.assertFalse((self.home / ".local").exists())
        code, output = self.install("--not-an-option")
        self.assertNotEqual(code, 0)
        self.assertIn("Unknown option", output)
        self.assertFalse((self.home / ".local").exists())

    def test_existing_installer_regressions(self):
        arch = "aarch64" if platform.machine() in ("arm64", "aarch64") else "x86_64"
        os_target = "apple-darwin" if platform.system() == "Darwin" else "unknown-linux-musl"
        result = subprocess.run(
            ["/bin/sh", str(ROOT / "scripts/test-install.sh"), str(self.fixture), f"{arch}-{os_target}"],
            env=self.env | {"PATH": "/usr/bin:/bin"}, capture_output=True, text=True, timeout=60,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("Passed ", result.stdout)
        print(result.stdout.splitlines()[-1])


if __name__ == "__main__":
    unittest.main()
