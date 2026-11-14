// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};

mod schema;

#[path = "support/executable.rs"]
mod fixture;

const PATH_LINE: &str = "; : 'koidrive PATH'";

struct Machine {
    _dir: tempfile::TempDir,
    home: PathBuf,
}

impl Machine {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(dir.path()).unwrap();
        Machine { _dir: dir, home }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.home.join(rel)
    }

    fn write(&self, rel: &str, text: &str) -> PathBuf {
        let path = self.path(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, text).unwrap();
        path
    }

    fn copy_koi(&self, rel: &str) -> PathBuf {
        let path = self.path(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let status = Command::new("/bin/cp")
            .arg(env!("CARGO_BIN_EXE_koi"))
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success(), "fixture copy failed: {status}");
        path
    }

    fn script(&self, rel: &str, body: &str) -> PathBuf {
        let path = self.path(rel);
        fixture::executable(&path, format!("#!/bin/sh\n{body}").as_bytes());
        path
    }

    #[cfg(target_os = "macos")]
    fn paths(&self) -> koidrive_driver::Paths {
        let vars = [
            ("XDG_CONFIG_HOME", self.path("config")),
            ("XDG_STATE_HOME", self.path("state")),
            ("XDG_RUNTIME_DIR", self.path("run")),
        ];
        koidrive_driver::Paths::from_vars(|name| {
            (vars.iter())
                .find(|(key, _)| *key == name)
                .map(|(_, path)| path.clone().into())
        })
        .unwrap()
    }

    fn command(&self, koi: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(koi);
        command
            .args(args)
            .current_dir(&self.home)
            .stdin(Stdio::null())
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("XDG_STATE_HOME", self.path("state"))
            .env("XDG_RUNTIME_DIR", self.path("run"))
            .env("HOMEBREW_PREFIX", self.path("brew"))
            .env("KOIDRIVE_TEST_INSTALL_INVENTORY", &self.home)
            .env("KOIDRIVE_UPDATE_CHECK", "0")
            .env("KOIDRIVE_TELEMETRY", "off")
            .env("PATH", "/usr/bin:/bin")
            .env_remove("ZDOTDIR")
            .env_remove("KOIDRIVE_LOG");
        command
    }

    fn uninstall(&self, koi: &Path) -> (Output, Value) {
        self.envelope(self.command(koi, &["uninstall", "--yes", "--json"]))
    }

    fn envelope(&self, mut command: Command) -> (Output, Value) {
        let output = command.output().unwrap();
        let envelope: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "{error}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        schema::check(&envelope);
        (output, envelope)
    }

    fn profile(&self) {
        self.write("state/koidrive/update-check.json", "{}");
        self.write("state/koidrive/spaces/.keep", "");
        self.write(
            "config/koidrive/config.toml",
            "[service]\nbackground = false\n",
        );
    }
}

fn path_line(dir: &Path) -> String {
    format!(
        "case \":$PATH:\" in *:'{0}':*) ;; *) export PATH='{0}':\"$PATH\" ;; esac{PATH_LINE}",
        dir.display()
    )
}

fn removed(envelope: &Value) -> &Vec<Value> {
    envelope["data"]["removed"].as_array().unwrap()
}

fn kept(envelope: &Value) -> &Vec<Value> {
    envelope["data"]["kept"].as_array().unwrap()
}

fn record(machine: &Machine, folder: &Path) {
    machine.write(
        "state/koidrive/uninstall.json",
        &json!({ "folders": [folder], "owner": null }).to_string(),
    );
}

#[test]
fn a_command_linked_into_a_synced_folder_loses_only_its_link() {
    let machine = Machine::new();
    machine.profile();
    let folder = machine.path("Koidrive");
    let target = machine.copy_koi("Koidrive/koi");
    machine.write("Koidrive/.koidrive/space.json", "{}");
    record(&machine, &folder);
    let link = machine.path(".local/bin/koi");
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let (output, envelope) = machine.uninstall(&link);

    assert!(output.status.success(), "{envelope:#}");
    assert!(fs::symlink_metadata(&link).is_err(), "{envelope:#}");
    assert!(target.is_file(), "{envelope:#}");
    assert!(
        removed(&envelope)
            .contains(&json!({ "kind": "command", "path": link.display().to_string() })),
        "{envelope:#}"
    );
    assert!(
        kept(&envelope)
            .contains(&json!({ "kind": "link_target", "path": target.display().to_string() })),
        "{envelope:#}"
    );
}

#[test]
fn a_profile_folder_that_lands_on_a_synced_folder_keeps_every_file() {
    let machine = Machine::new();
    let koi = machine.copy_koi(".local/bin/koi");
    let document = machine.write("koidrive/q3.docx", "q3");
    let mut command = machine.command(&koi, &["uninstall", "--yes", "--json"]);
    command.env("XDG_STATE_HOME", &machine.home);

    let (output, envelope) = machine.envelope(command);

    assert!(output.status.success(), "{envelope:#}");
    assert_eq!(fs::read_to_string(&document).unwrap(), "q3", "{envelope:#}");
    assert!(
        kept(&envelope).contains(&json!({
            "kind": "unrecognized",
            "path": machine.path("koidrive").display().to_string()
        })),
        "{envelope:#}"
    );

    let machine = Machine::new();
    machine.profile();
    let koi = machine.copy_koi(".local/bin/koi");
    let folder = machine.path("koidrive");
    let document = machine.write("koidrive/q3.docx", "q3");
    machine.write("koidrive/.koidrive/space.json", "{}");
    record(&machine, &folder);
    let mut command = machine.command(&koi, &["uninstall", "--yes", "--json"]);
    command.env("XDG_CONFIG_HOME", &machine.home);

    let (output, envelope) = machine.envelope(command);

    assert!(output.status.success(), "{envelope:#}");
    assert_eq!(fs::read_to_string(&document).unwrap(), "q3", "{envelope:#}");
    let folder = folder.display().to_string();
    assert!(
        kept(&envelope).contains(&json!({ "kind": "in_folder", "path": folder, "folder": folder })),
        "{envelope:#}"
    );
    assert!(!machine.path("state/koidrive").exists(), "{envelope:#}");
}

#[test]
fn a_state_folder_that_holds_other_files_loses_only_what_koidrive_wrote() {
    let machine = Machine::new();
    let koi = Path::new(env!("CARGO_BIN_EXE_koi"));
    let photo = machine.write("Backup/wedding.jpg", "jpg");
    fs::create_dir_all(machine.path("st")).unwrap();
    std::os::unix::fs::symlink(machine.path("Backup"), machine.path("st/koidrive")).unwrap();
    let document = machine.write("Koidrive/q3.docx", "q3");
    for state in [machine.path("st"), machine.home.clone()] {
        let mut status = machine.command(koi, &["status", "--json"]);
        status.env("XDG_STATE_HOME", &state);
        assert!(status.output().unwrap().status.success());
        let mut command = machine.command(koi, &["uninstall", "--yes", "--json"]);
        command.env("XDG_STATE_HOME", &state);

        let (output, envelope) = machine.envelope(command);

        assert!(output.status.success(), "{envelope:#}");
        assert_eq!(fs::read_to_string(&photo).unwrap(), "jpg", "{envelope:#}");
        assert_eq!(fs::read_to_string(&document).unwrap(), "q3", "{envelope:#}");
    }
    assert!(!machine.path("Backup/lock").exists());
    assert!(fs::symlink_metadata(machine.path("st/koidrive")).is_ok());
}

#[test]
fn a_linked_state_folder_goes_with_its_link() {
    let machine = Machine::new();
    let koi = machine.copy_koi(".local/bin/koi");
    let elsewhere = machine.path("disk/koidrive-state");
    machine.write("disk/koidrive-state/credential.json", "{}");
    machine.write("disk/koidrive-state/spaces/s/store.sqlite", "x");
    fs::create_dir_all(machine.path("state")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, machine.path("state/koidrive")).unwrap();

    let (output, envelope) = machine.uninstall(&koi);

    assert!(output.status.success(), "{envelope:#}");
    assert!(!elsewhere.exists(), "{envelope:#}");
    assert!(
        fs::symlink_metadata(machine.path("state/koidrive")).is_err(),
        "{envelope:#}"
    );
}

#[test]
fn without_a_terminal_it_asks_for_yes_and_changes_nothing() {
    let machine = Machine::new();
    machine.profile();
    let koi = machine.copy_koi(".local/bin/koi");

    let output = machine
        .command(&koi, &["uninstall", "--json"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
    schema::check(&envelope);
    assert_eq!(envelope["error"]["code"], "usage", "{envelope:#}");
    assert_eq!(
        envelope["next_commands"],
        json!([{ "argv": ["koi", "uninstall", "--yes"] }]),
        "{envelope:#}"
    );
    assert!(koi.exists());
    assert!(machine.path("state/koidrive/update-check.json").exists());
    assert!(machine.path("config/koidrive/config.toml").exists());
}

#[test]
fn an_installer_copy_leaves_nothing_of_koidrive_and_every_file_of_the_user() {
    let machine = Machine::new();
    machine.profile();
    let koi = machine.copy_koi(".local/bin/koi");
    let zprofile = machine.write(
        ".zprofile",
        &format!("export EDITOR=vim\n{}\n", path_line(koi.parent().unwrap())),
    );
    let fish = machine.write(
        "config/fish/conf.d/koidrive.fish",
        "contains -- '/x' $PATH; or set -gx PATH '/x' $PATH; true 'koidrive PATH'\n",
    );
    let mac = [
        "Library/Logs/Koidrive/update.log",
        "Library/Application Support/Koidrive/window.json",
        "Library/Preferences/dev.koidrive.Koidrive.plist",
        "Library/Caches/dev.koidrive.app-install/stage/x",
    ];
    if cfg!(target_os = "macos") {
        for rel in mac {
            machine.write(rel, "x");
        }
    }
    let document = machine.write("Koidrive/Quarterly report.docx", "the user's words");

    let (output, envelope) = machine.uninstall(&koi);

    assert!(output.status.success(), "{envelope:#}");
    assert_eq!(envelope["outcome"], "ok", "{envelope:#}");
    assert_eq!(envelope["data"]["device"], json!({ "state": "not_paired" }));
    for gone in [
        koi.clone(),
        machine.path("state/koidrive"),
        machine.path("config/koidrive"),
        fish.clone(),
    ] {
        assert!(!gone.exists(), "{} remains: {envelope:#}", gone.display());
    }
    if cfg!(target_os = "macos") {
        for rel in [
            "Library/Logs/Koidrive",
            "Library/Application Support/Koidrive",
            "Library/Preferences/dev.koidrive.Koidrive.plist",
            "Library/Caches/dev.koidrive.app-install",
        ] {
            assert!(!machine.path(rel).exists(), "{rel} remains");
        }
    }
    assert_eq!(
        fs::read_to_string(&zprofile).unwrap(),
        "export EDITOR=vim\n"
    );
    assert_eq!(fs::read_to_string(&document).unwrap(), "the user's words");
    let removed = removed(&envelope);
    assert!(
        removed.contains(&json!({ "kind": "command", "path": koi.display().to_string() })),
        "{envelope:#}"
    );
    assert!(
        removed
            .contains(&json!({ "kind": "shell_profile", "path": zprofile.display().to_string() })),
        "{envelope:#}"
    );

    let (output, again) = machine.uninstall(Path::new(env!("CARGO_BIN_EXE_koi")));
    assert!(output.status.success(), "{again:#}");
    assert_eq!(again["outcome"], "unchanged", "{again:#}");
    assert_eq!(again["data"]["removed"], json!([]), "{again:#}");
}

#[test]
fn a_cask_install_hands_the_app_to_homebrew_after_its_login_item_is_gone() {
    let machine = Machine::new();
    machine.profile();
    let app = machine.path("Applications/Koidrive.app");
    let helper = machine.copy_koi("Applications/Koidrive.app/Contents/Helpers/koi");
    let login = machine.script(
        "Applications/Koidrive.app/Contents/MacOS/Koidrive",
        "printf '%s\\n' \"$*\" >> \"$HOME/login.calls\"\n",
    );
    let owned = machine.path("brew/Caskroom/koidrive/0.1.0/Koidrive.app");
    fs::create_dir_all(owned.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&app, &owned).unwrap();
    let brew = machine.script(
        "brew/bin/brew",
        &format!(
            "printf '%s\\n' \"$*\" >> \"$HOME/brew.calls\"\nrm -rf '{}'\n",
            app.display()
        ),
    );

    let (output, envelope) = machine.uninstall(&helper);

    assert!(output.status.success(), "{envelope:#}");
    assert_eq!(
        fs::read_to_string(machine.path("brew.calls")).unwrap(),
        "uninstall --cask koidrive\n"
    );
    assert_eq!(
        fs::read_to_string(machine.path("login.calls")).unwrap(),
        "--unregister-login-item\n"
    );
    assert!(!login.exists());
    assert!(!machine.path("state/koidrive").exists());
    let homebrew = json!({
        "kind": "homebrew",
        "argv": [fs::canonicalize(&brew).unwrap().display().to_string(), "uninstall", "--cask", "koidrive"],
    });
    assert!(removed(&envelope).contains(&homebrew), "{envelope:#}");
}

#[test]
fn a_failed_homebrew_handoff_names_the_command_that_finishes_it() {
    let machine = Machine::new();
    machine.profile();
    let cellar = machine.copy_koi("brew/Cellar/koidrive/0.1.0/bin/koi");
    let opt = machine.path("brew/opt/koidrive");
    fs::create_dir_all(opt.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(machine.path("brew/Cellar/koidrive/0.1.0"), &opt).unwrap();
    let link = machine.path("brew/bin/koi");
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&cellar, &link).unwrap();
    let brew = machine.script(
        "brew/bin/brew",
        "echo 'Error: Permission denied' >&2\nexit 1\n",
    );

    let (output, envelope) = machine.uninstall(&link);

    assert_eq!(output.status.code(), Some(1), "{envelope:#}");
    assert_eq!(envelope["error"]["code"], "failed", "{envelope:#}");
    let brew = fs::canonicalize(&brew).unwrap().display().to_string();
    assert_eq!(
        envelope["next_commands"],
        json!([{ "argv": [brew, "uninstall", "--formula", "koidrive"] }]),
        "{envelope:#}"
    );
    assert!(!machine.path("state/koidrive").exists());
    assert!(cellar.exists());
}

#[test]
fn an_app_in_applications_is_unregistered_and_removed() {
    let machine = Machine::new();
    let app = machine.path("Applications/Koidrive.app");
    let helper = machine.copy_koi("Applications/Koidrive.app/Contents/Helpers/koi");
    machine.script(
        "Applications/Koidrive.app/Contents/MacOS/Koidrive",
        "printf '%s\\n' \"$*\" >> \"$HOME/login.calls\"\n",
    );
    let lock = machine.write("Applications/.koidrive-app-install.lock", "");

    let (output, envelope) = machine.uninstall(&helper);

    assert!(output.status.success(), "{envelope:#}");
    assert!(!app.exists());
    assert_eq!(
        lock.exists(),
        cfg!(not(target_os = "macos")),
        "{envelope:#}"
    );
    assert_eq!(
        fs::read_to_string(machine.path("login.calls")).unwrap(),
        "--unregister-login-item\n"
    );
    assert!(
        removed(&envelope).contains(&json!({ "kind": "app", "path": app.display().to_string() })),
        "{envelope:#}"
    );
}

fn on_terminal(machine: &Machine, koi: &Path, args: &[&str], answer: &str) -> (i32, String) {
    use std::io::{Read, Write};

    let master = rustix::pty::openpt(rustix::pty::OpenptFlags::RDWR).unwrap();
    rustix::pty::grantpt(&master).unwrap();
    rustix::pty::unlockpt(&master).unwrap();
    let name = rustix::pty::ptsname(&master, Vec::new()).unwrap();
    let slave = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(name.to_str().unwrap())
        .unwrap();
    let flags = rustix::fs::fcntl_getfl(&master).unwrap();
    rustix::fs::fcntl_setfl(&master, flags | rustix::fs::OFlags::NONBLOCK).unwrap();
    let size = rustix::termios::Winsize {
        ws_row: 40,
        ws_col: 160,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    rustix::termios::tcsetwinsize(&master, size).unwrap();
    let mut terminal = fs::File::from(master);
    let mut child = machine
        .command(koi, args)
        .env_remove("CI")
        .env("TERM", "xterm-256color")
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave)
        .spawn()
        .unwrap();
    let mut raw = Vec::new();
    let mut answered = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let mut bytes = [0; 4096];
        match terminal.read(&mut bytes) {
            Ok(0) => break,
            Ok(count) => {
                raw.extend_from_slice(&bytes[..count]);
                let text = String::from_utf8_lossy(&raw);
                if !answered
                    && console::strip_ansi_codes(&text)
                        .contains("Remove Koidrive from this computer?")
                {
                    terminal.write_all(answer.as_bytes()).unwrap();
                    answered = true;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if child.try_wait().unwrap().is_some() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(error) if error.raw_os_error() == Some(5) => break,
            Err(error) => panic!("terminal read failed: {error}"),
        }
        if std::time::Instant::now() > deadline {
            child.kill().unwrap();
            panic!(
                "koi uninstall did not finish: {}",
                String::from_utf8_lossy(&raw)
            );
        }
    }
    let status = child.wait().unwrap();
    let text = console::strip_ansi_codes(&String::from_utf8_lossy(&raw)).replace("\r\n", "\n");
    assert!(answered, "no question: {text}");
    (status.code().unwrap(), text)
}

#[test]
fn on_a_terminal_it_shows_what_goes_and_what_stays_and_asks_first() {
    let machine = Machine::new();
    machine.profile();
    let koi = machine.copy_koi(".local/bin/koi");

    let (code, text) = on_terminal(&machine, &koi, &["uninstall", "--plain"], "n\n");

    assert_ne!(code, 0, "{text}");
    assert!(text.contains("This removes:"), "{text}");
    assert!(text.contains("~/.local/bin/koi"), "{text}");
    assert!(text.contains("~/state/koidrive"), "{text}");
    assert!(text.contains("stopped; nothing was changed"), "{text}");
    assert!(koi.exists());
    assert!(machine.path("state/koidrive/update-check.json").exists());

    let (code, text) = on_terminal(&machine, &koi, &["uninstall"], "y");

    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains("Removed Koidrive from this computer."),
        "{text}"
    );
    assert!(text.contains("Removed ~/.local/bin/koi."), "{text}");
    assert!(!koi.exists());
    assert!(!machine.path("state/koidrive").exists());
}

#[cfg(target_os = "macos")]
struct Agent {
    label: String,
    plist: PathBuf,
}

#[cfg(target_os = "macos")]
impl Agent {
    fn target(&self) -> String {
        format!("gui/{}/{}", rustix::process::getuid().as_raw(), self.label)
    }

    fn loaded(&self) -> bool {
        Command::new("/bin/launchctl")
            .args(["print", &self.target()])
            .output()
            .unwrap()
            .status
            .success()
    }
}

#[cfg(target_os = "macos")]
impl Drop for Agent {
    fn drop(&mut self) {
        let _ = Command::new("/bin/launchctl")
            .args(["bootout", &self.target()])
            .output();
        let _ = fs::remove_file(&self.plist);
    }
}

#[cfg(target_os = "macos")]
#[test]
fn the_launch_agent_of_a_temporary_profile_is_stopped_and_removed() {
    let machine = Machine::new();
    machine.profile();
    let koi = machine.copy_koi(".local/bin/koi");
    let label = format!("dev.koidrive.koi.{}", machine.paths().state_key());
    assert_ne!(label, "dev.koidrive.koi");
    let plist = machine.path(&format!("Library/LaunchAgents/{label}.plist"));
    let env: String = [
        ("HOME", machine.home.clone()),
        ("XDG_CONFIG_HOME", machine.path("config")),
        ("XDG_STATE_HOME", machine.path("state")),
        ("XDG_RUNTIME_DIR", machine.path("run")),
    ]
    .iter()
    .map(|(key, value)| format!("<key>{key}</key><string>{}</string>", value.display()))
    .chain(["<key>KOIDRIVE_TELEMETRY</key><string>off</string><key>KOIDRIVE_UPDATE_CHECK</key><string>0</string>".to_owned()])
    .collect();
    machine.write(
        &format!("Library/LaunchAgents/{label}.plist"),
        &format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>Label</key><string>{label}</string><key>ProgramArguments</key><array><string>{}</string><string>_daemon</string></array><key>EnvironmentVariables</key><dict>{env}</dict><key>RunAtLoad</key><true/><key>KeepAlive</key><true/></dict></plist>\n",
            koi.display()
        ),
    );
    let agent = Agent { label, plist };
    let uid = rustix::process::getuid().as_raw();
    let bootstrap = Command::new("/bin/launchctl")
        .args(["bootstrap", &format!("gui/{uid}")])
        .arg(&agent.plist)
        .output()
        .unwrap();
    assert!(bootstrap.status.success(), "{bootstrap:?}");
    assert!(agent.loaded());

    let (output, envelope) = machine.uninstall(&koi);

    assert!(output.status.success(), "{envelope:#}");
    assert!(
        removed(&envelope).contains(&json!({ "kind": "service", "name": agent.label })),
        "{envelope:#}"
    );
    assert!(!agent.loaded());
    assert!(!agent.plist.exists());
    assert!(!machine.path("state/koidrive").exists());
    assert!(!koi.exists());
}

#[cfg(target_os = "macos")]
#[test]
fn another_profiles_service_keeps_the_shared_command_and_names_how_to_remove_it() {
    let machine = Machine::new();
    machine.profile();
    let koi = machine.copy_koi(".local/bin/koi");
    let line = path_line(koi.parent().unwrap());
    let zprofile = machine.write(".zprofile", &format!("{line}\n"));
    let support = machine.write("Library/Application Support/Koidrive/window.json", "x");
    let (config, state) = (machine.path("other/config"), machine.path("other/state"));
    let other = koidrive_driver::Paths::from_vars(|name| match name {
        "XDG_CONFIG_HOME" => Some(config.clone().into()),
        "XDG_STATE_HOME" => Some(state.clone().into()),
        _ => None,
    })
    .unwrap();
    let label = format!("dev.koidrive.koi.{}", other.state_key());
    machine.write("other/state/koidrive/credential.json", "");
    machine.write("other/state/koidrive/spaces/.keep", "");
    machine.write(
        &format!("Library/LaunchAgents/{label}.plist"),
        &format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict><key>Label</key><string>{label}</string><key>ProgramArguments</key><array><string>{}</string><string>_daemon</string></array><key>EnvironmentVariables</key><dict><key>XDG_CONFIG_HOME</key><string>{}</string><key>XDG_STATE_HOME</key><string>{}</string><key>XDG_RUNTIME_DIR</key><string></string></dict></dict></plist>\n",
            koi.display(),
            config.display(),
            state.display()
        ),
    );

    let (output, envelope) = machine.uninstall(&koi);

    assert!(output.status.success(), "{envelope:#}");
    assert!(!machine.path("state/koidrive").exists(), "{envelope:#}");
    assert!(koi.is_file(), "{envelope:#}");
    assert!(support.is_file(), "{envelope:#}");
    assert_eq!(fs::read_to_string(&zprofile).unwrap(), format!("{line}\n"));
    let argv = json!([
        "env",
        "-u",
        "XDG_RUNTIME_DIR",
        format!("XDG_CONFIG_HOME={}", config.display()),
        format!("XDG_STATE_HOME={}", state.display()),
        koi.display().to_string(),
        "uninstall"
    ]);
    let shared = kept(&envelope);
    for item in [
        json!({ "kind": "other_profile", "service": label, "argv": argv }),
        json!({ "kind": "shared", "path": koi.display().to_string() }),
        json!({ "kind": "path_line", "path": zprofile.display().to_string() }),
    ] {
        assert!(shared.contains(&item), "{item} in {envelope:#}");
    }
    assert_eq!(
        envelope["next_commands"],
        json!([{ "argv": argv }]),
        "{envelope:#}"
    );

    let argv: Vec<String> = serde_json::from_value(argv).unwrap();
    let mut args: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
    args.extend(["--yes", "--json"]);
    let (output, last) = machine.envelope(machine.command(Path::new(&argv[0]), &args));

    assert!(output.status.success(), "{last:#}");
    assert!(
        kept(&last)
            .iter()
            .all(|item| item["kind"] != "other_profile"),
        "{last:#}"
    );
    for gone in [
        koi.clone(),
        support,
        machine.path(&format!("Library/LaunchAgents/{label}.plist")),
        state.join("koidrive"),
    ] {
        assert!(!gone.exists(), "{} remains: {last:#}", gone.display());
    }
    assert!(!fs::read_to_string(&zprofile).is_ok_and(|text| text.contains(PATH_LINE)));
}
