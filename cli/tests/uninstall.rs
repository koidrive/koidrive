// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};

mod schema;

#[path = "support/executable.rs"]
mod fixture;

#[path = "support/terminal.rs"]
mod terminal;

use terminal::Terminal;

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
        if let Some(at) = rel.find("state/koidrive/") {
            let state = self.path(&rel[..at + "state/koidrive".len()]);
            koidrive_state_file::create_dir(&state, &state).unwrap();
        }
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
        fs::create_dir_all(self.path("state/koidrive/spaces")).unwrap();
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
            "kind": "unrecorded_state",
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
fn a_linked_state_folder_koidrive_did_not_make_keeps_the_users_files() {
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
        let linked = json!({ "kind": "unrecorded_state", "path": machine.path("st/koidrive") });
        assert!(
            state != machine.path("st") || kept(&envelope).contains(&linked),
            "{envelope:#}"
        );
    }
    assert!(fs::symlink_metadata(machine.path("st/koidrive")).is_ok());
}

#[test]
fn a_state_folder_left_with_one_staged_file_is_removed() {
    for name in [
        "credential.next",
        ".credential.next.123.tmp",
        "leaving.next",
        "uninstall.json",
    ] {
        let machine = Machine::new();
        let koi = machine.copy_koi(".local/bin/koi");
        machine.write(&format!("state/koidrive/{name}"), "{}");

        let (output, envelope) = machine.uninstall(&koi);

        assert!(output.status.success(), "{name}: {envelope:#}");
        assert!(
            !machine.path("state/koidrive").exists(),
            "{name}: {envelope:#}"
        );
        assert!(!koi.exists(), "{name}: {envelope:#}");
        assert!(
            !kept(&envelope)
                .iter()
                .any(|kept| kept["kind"] == "unrecognized"),
            "{name}: {envelope:#}"
        );
        let (output, envelope) = machine.uninstall(Path::new(env!("CARGO_BIN_EXE_koi")));
        assert!(output.status.success(), "{name}: {envelope:#}");
        assert_eq!(
            removed(&envelope),
            &Vec::<Value>::new(),
            "{name}: {envelope:#}"
        );
    }
}

#[test]
fn a_folder_with_only_subfolders_of_the_same_names_is_kept_whole() {
    let machine = Machine::new();
    let koi = machine.copy_koi(".local/bin/koi");
    let plan = machine.write("state/koidrive/spaces/plan.pdf", "pdf");
    let notes = machine.write("state/koidrive/telemetry/notes.txt", "notes");

    let (output, envelope) = machine.uninstall(&koi);

    assert!(output.status.success(), "{envelope:#}");
    assert_eq!(fs::read_to_string(&plan).unwrap(), "pdf", "{envelope:#}");
    assert_eq!(fs::read_to_string(&notes).unwrap(), "notes", "{envelope:#}");
    assert!(
        !machine.path("state/koidrive/lock").exists(),
        "{envelope:#}"
    );
    assert!(
        !machine.path("state/koidrive/uninstall.json").exists(),
        "{envelope:#}"
    );
    let shown = machine.path("state/koidrive");
    for rel in ["spaces", "telemetry"] {
        assert!(
            kept(&envelope).contains(&json!({
                "kind": "unrecognized",
                "path": shown.join(rel),
            })),
            "{envelope:#}"
        );
    }
}

#[test]
fn a_state_folder_of_the_users_keeps_their_files_in_subfolders_named_like_koidrives() {
    let machine = Machine::new();
    let koi = Path::new(env!("CARGO_BIN_EXE_koi"));
    let ours = [
        format!("requests/{}.json", "ab".repeat(32)),
        "telemetry/settings.json".to_owned(),
        format!("telemetry/held/{}.json", "cd".repeat(16)),
        "systemd/koidrive.service".to_owned(),
    ];
    let theirs = [
        "spaces/plan.pdf".to_owned(),
        "requests/letter.docx".to_owned(),
        "telemetry/held/photo.jpg".to_owned(),
        "systemd/backup.service".to_owned(),
    ];
    let backup = machine.path("Backup");
    koidrive_state_file::create_dir(&backup, &backup).unwrap();
    for rel in &ours {
        machine.write(&format!("Backup/{rel}"), "{}");
    }
    for rel in &theirs {
        machine.write(&format!("Backup/{rel}"), rel);
    }
    fs::create_dir_all(machine.path("st")).unwrap();
    std::os::unix::fs::symlink(machine.path("Backup"), machine.path("st/koidrive")).unwrap();
    let mut status = machine.command(koi, &["status", "--json"]);
    status.env("XDG_STATE_HOME", machine.path("st"));
    assert!(status.output().unwrap().status.success());
    let mut command = machine.command(koi, &["uninstall", "--yes", "--json"]);
    command.env("XDG_STATE_HOME", machine.path("st"));

    let (output, envelope) = machine.envelope(command);

    assert!(output.status.success(), "{envelope:#}");
    for rel in &theirs {
        let text = fs::read_to_string(machine.path(&format!("Backup/{rel}")));
        assert_eq!(text.ok().as_ref(), Some(rel), "{envelope:#}");
    }
    for rel in &ours {
        let path = machine.path(&format!("Backup/{rel}"));
        assert!(!path.exists(), "{rel}: {envelope:#}");
    }
    let mut named: Vec<_> = (kept(&envelope).iter())
        .filter(|kept| kept["kind"] == "unrecognized")
        .map(|kept| kept["path"].as_str().unwrap().to_owned())
        .collect();
    named.sort();
    let shown = machine.path("st/koidrive");
    let mut expected: Vec<_> = ["spaces", "requests", "telemetry/held", "systemd"]
        .iter()
        .map(|rel| shown.join(rel).display().to_string())
        .collect();
    expected.sort();
    assert_eq!(named, expected, "{envelope:#}");
}

#[test]
fn records_koidrive_leaves_in_its_subfolders_go_with_the_state_folder() {
    let space = "0190f0a0-0000-7000-8000-00000000000a";
    let request = format!("requests/{}.json", "82".repeat(32));
    let lifecycle = format!("spaces/{space}/lifecycle.json");
    let link = format!("spaces/{space}/link.json");
    let unlinking = format!("spaces/{space}/unlinking");
    let left: [&[&str]; 2] = [
        &[&request],
        &["update-check.json", &lifecycle, &link, &unlinking],
    ];
    for files in left {
        let machine = Machine::new();
        let koi = machine.copy_koi(".local/bin/koi");
        for file in files {
            machine.write(&format!("state/koidrive/{file}"), "{}");
        }

        let (output, envelope) = machine.uninstall(&koi);

        assert!(output.status.success(), "{files:?}: {envelope:#}");
        assert!(
            !machine.path("state/koidrive").exists(),
            "{files:?}: {envelope:#}"
        );
        assert!(
            !kept(&envelope)
                .iter()
                .any(|kept| kept["kind"] == "unrecognized"),
            "{files:?}: {envelope:#}"
        );
    }
}

#[test]
fn a_state_folder_koidrive_did_not_make_keeps_everything_in_it() {
    let database = "spaces/0190f0a0-0000-7000-8000-00000000000a/store.sqlite";
    let stage = ".koi-state-0123456789abcdef0123456789abcdef.tmp";
    let made = tempfile::tempdir().unwrap();
    let expenses = made.path().join("expenses.sqlite");
    rusqlite::Connection::open(&expenses)
        .unwrap()
        .execute_batch(
            "CREATE TABLE expenses(label TEXT); INSERT INTO expenses VALUES ('January rent');",
        )
        .unwrap();
    let expenses = fs::read(&expenses).unwrap();
    type Files<'a> = &'a [(&'a str, &'a [u8])];
    let cases: [(&str, &str, Files); 7] = [
        (
            "notes",
            "its own folder",
            &[("update.log", &b"my notes"[..])],
        ),
        (
            "notes",
            "below a linked folder",
            &[("update.log", &b"my notes"[..])],
        ),
        (
            "an expenses database",
            "a linked folder",
            &[(database, &expenses)],
        ),
        (
            "a stage-named file",
            "a linked folder",
            &[
                ("update-check.json", &br#"{"checked_at":5}"#[..]),
                (stage, &b"my notes"[..]),
            ],
        ),
        (
            "a backup",
            "a linked folder",
            &[
                ("update-check.json", &b"{}\n"[..]),
                ("update.log", &b"mine\n"[..]),
                ("telemetry/settings.json", &b"mine\n"[..]),
                ("notes.txt", &b"mine\n"[..]),
            ],
        ),
        (
            "an older release's state",
            "its own folder",
            &[
                ("update-check.json", &br#"{"checked_at":5}"#[..]),
                ("uninstall.json", &br#"{"folders":[],"owner":null}"#[..]),
                ("lock", &b""[..]),
                (database, &b"SQLite format 3\0"[..]),
                (stage, &b"{}"[..]),
            ],
        ),
        (
            "a signed-in older release",
            "its own folder",
            &[("credential.json", &b"{}"[..]), ("notes.txt", &b"mine"[..])],
        ),
    ];
    let mut lost = Vec::new();
    for (what, setup, files) in cases {
        let machine = Machine::new();
        let koi = machine.copy_koi(".local/bin/koi");
        let home = machine.path("custom");
        match setup {
            "its own folder" => fs::create_dir_all(home.join("koidrive")).unwrap(),
            "below a linked folder" => {
                fs::create_dir_all(machine.path("disk/koidrive")).unwrap();
                std::os::unix::fs::symlink(machine.path("disk"), &home).unwrap();
            }
            _ => {
                fs::create_dir_all(machine.path("Backup")).unwrap();
                fs::create_dir_all(&home).unwrap();
                std::os::unix::fs::symlink(machine.path("Backup"), home.join("koidrive")).unwrap();
            }
        }
        let state = home.join("koidrive");
        for (rel, text) in files {
            let path = state.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, text).unwrap();
        }
        let mut command = machine.command(&koi, &["uninstall", "--yes", "--json"]);
        command.env("XDG_STATE_HOME", &home);

        let (output, envelope) = machine.envelope(command);

        let case = format!("{what} in {setup}");
        assert!(output.status.success(), "{case}: {envelope:#}");
        lost.extend(
            (files.iter())
                .filter(|(rel, text)| fs::read(state.join(rel)).ok().as_deref() != Some(*text))
                .map(|(rel, _)| format!("{case}: {rel}")),
        );
        let mut tops: Vec<_> = (files.iter())
            .map(|(rel, _)| rel.split('/').next().unwrap())
            .collect();
        tops.sort();
        tops.dedup();
        let mut entries: Vec<_> = (fs::read_dir(&state).into_iter().flatten())
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        if entries != tops {
            lost.push(format!("{case}: holds {entries:?}"));
        }
        let reported = json!({ "kind": "unrecorded_state", "path": state });
        if !kept(&envelope).contains(&reported) {
            lost.push(format!("{case}: not reported"));
        }
    }
    assert_eq!(lost, Vec::<String>::new());
}

#[test]
fn a_state_folder_it_cannot_write_to_stops_before_anything_is_removed() {
    use std::os::unix::fs::PermissionsExt;
    let machine = Machine::new();
    machine.profile();
    let koi = machine.copy_koi(".local/bin/koi");
    let state = machine.path("state/koidrive");
    fs::set_permissions(&state, fs::Permissions::from_mode(0o555)).unwrap();

    let (output, envelope) = machine.uninstall(&koi);

    fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!output.status.success(), "{envelope:#}");
    assert!(koi.is_file(), "{envelope:#}");
    assert!(state.join("update-check.json").is_file(), "{envelope:#}");
    assert!(
        machine.path("config/koidrive/config.toml").is_file(),
        "{envelope:#}"
    );
    assert_eq!(removed(&envelope), &Vec::<Value>::new(), "{envelope:#}");
}

#[test]
fn a_config_folder_that_is_the_state_folder_loses_only_what_koidrive_wrote() {
    let machine = Machine::new();
    let koi = machine.copy_koi(".local/bin/koi");
    let plan = machine.write("shared/koidrive/spaces/plan.pdf", "pdf");
    let notes = machine.write("shared/koidrive/telemetry/notes.txt", "notes");
    let shared = |machine: &Machine, koi: &Path| {
        let mut command = machine.command(koi, &["uninstall", "--yes", "--json"]);
        command
            .env("XDG_CONFIG_HOME", machine.path("shared"))
            .env("XDG_STATE_HOME", machine.path("shared"));
        machine.envelope(command)
    };

    let (output, envelope) = shared(&machine, &koi);

    assert!(output.status.success(), "{envelope:#}");
    assert_eq!(fs::read_to_string(&plan).unwrap(), "pdf", "{envelope:#}");
    assert_eq!(fs::read_to_string(&notes).unwrap(), "notes", "{envelope:#}");

    let machine = Machine::new();
    let koi = machine.copy_koi(".local/bin/koi");
    let state = machine.path("shared/koidrive");
    koidrive_state_file::create_dir(&state, &state).unwrap();
    machine.write("shared/koidrive/update-check.json", "{}");
    machine.write(
        "shared/koidrive/config.toml",
        "[service]\nbackground = false\n",
    );

    let (output, envelope) = shared(&machine, &koi);

    assert!(output.status.success(), "{envelope:#}");
    assert!(!machine.path("shared/koidrive").exists(), "{envelope:#}");
}

#[test]
fn a_synced_folder_inside_the_state_folder_keeps_its_files() {
    let machine = Machine::new();
    machine.profile();
    let koi = machine.copy_koi(".local/bin/koi");
    let folder = machine.path("state/koidrive/telemetry");
    machine.write("state/koidrive/telemetry/.koidrive/space.json", "{}");
    let document = machine.write("state/koidrive/telemetry/q3.docx", "q3");

    let (output, envelope) = machine.uninstall(&koi);

    assert!(output.status.success(), "{envelope:#}");
    assert_eq!(fs::read_to_string(&document).unwrap(), "q3", "{envelope:#}");
    let folder = folder.display().to_string();
    assert!(
        kept(&envelope).contains(&json!({ "kind": "in_folder", "path": folder, "folder": folder })),
        "{envelope:#}"
    );
}

#[test]
fn a_state_folder_reached_through_a_link_into_a_synced_folder_keeps_every_file() {
    let machine = Machine::new();
    let koi = machine.copy_koi(".local/bin/koi");
    let folder = machine.path("Sync");
    machine.write("Sync/.koidrive/space.json", "{}");
    let plan = machine.write("Sync/inner/koidrive/spaces/plan.pdf", "pdf");
    machine.write("Sync/inner/koidrive/update-check.json", "{}");
    machine.write(
        "Sync/inner/koidrive/uninstall.json",
        &json!({ "folders": [folder], "owner": null }).to_string(),
    );
    std::os::unix::fs::symlink(machine.path("Sync/inner"), machine.path("st")).unwrap();
    let mut command = machine.command(&koi, &["uninstall", "--yes", "--json"]);
    command.env("XDG_STATE_HOME", machine.path("st"));

    let (output, envelope) = machine.envelope(command);

    assert!(output.status.success(), "{envelope:#}");
    assert_eq!(fs::read_to_string(&plan).unwrap(), "pdf", "{envelope:#}");
    assert!(
        kept(&envelope).contains(&json!({
            "kind": "unrecorded_state",
            "path": machine.path("st/koidrive").display().to_string(),
        })),
        "{envelope:#}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn unreadable_launch_agents_keep_the_shared_command() {
    use std::os::unix::fs::PermissionsExt;
    let machine = Machine::new();
    machine.profile();
    let koi = machine.copy_koi(".local/bin/koi");
    let agents = machine.path("Library/LaunchAgents");
    fs::create_dir_all(&agents).unwrap();
    fs::set_permissions(&agents, fs::Permissions::from_mode(0o000)).unwrap();

    let (output, envelope) = machine.uninstall(&koi);

    fs::set_permissions(&agents, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!output.status.success(), "{envelope:#}");
    assert!(koi.is_file(), "{envelope:#}");
    assert!(
        machine.path("config/koidrive/config.toml").is_file(),
        "{envelope:#}"
    );
    for path in [koi.clone(), machine.path("config/koidrive")] {
        assert!(
            kept(&envelope)
                .contains(&json!({ "kind": "shared", "path": path.display().to_string() })),
            "{envelope:#}"
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn a_failed_app_removal_keeps_the_command_that_finishes_it() {
    use std::os::unix::fs::PermissionsExt;
    let machine = Machine::new();
    let koi = machine.copy_koi(".local/bin/koi");
    let line = path_line(koi.parent().unwrap());
    let zprofile = machine.write(".zprofile", &format!("{line}\n"));
    machine.copy_koi("Applications/Koidrive.app/Contents/Helpers/koi");
    machine.script(
        "Applications/Koidrive.app/Contents/MacOS/Koidrive",
        "exit 0\n",
    );
    let applications = machine.path("Applications");
    fs::set_permissions(&applications, fs::Permissions::from_mode(0o555)).unwrap();

    let (output, envelope) = machine.uninstall(&koi);

    fs::set_permissions(&applications, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!output.status.success(), "{envelope:#}");
    assert!(koi.is_file(), "{envelope:#}");
    assert_eq!(fs::read_to_string(&zprofile).unwrap(), format!("{line}\n"));
}

#[cfg(target_os = "macos")]
#[test]
fn a_config_another_profile_uses_stays() {
    for layout in ["same", "state", "alias"] {
        let machine = Machine::new();
        machine.profile();
        let koi = machine.copy_koi(".local/bin/koi");
        let ours = match layout {
            "state" => machine.path("state"),
            _ => machine.path("config"),
        };
        if layout == "state" {
            machine.write(
                "state/koidrive/config.toml",
                "[service]\nbackground = false\n",
            );
        }
        let config = match layout {
            "alias" => {
                std::os::unix::fs::symlink(&ours, machine.path("alias")).unwrap();
                machine.path("alias")
            }
            _ => ours.clone(),
        };
        let state = machine.path("other/state");
        let other = koidrive_driver::Paths::from_vars(|name| match name {
            "XDG_CONFIG_HOME" => Some(config.clone().into()),
            "XDG_STATE_HOME" => Some(state.clone().into()),
            _ => None,
        })
        .unwrap();
        let label = format!("dev.koidrive.koi.{}", other.state_key());
        machine.write("other/state/koidrive/credential.json", "");
        machine.write(
            &format!("Library/LaunchAgents/{label}.plist"),
            &format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict><key>Label</key><string>{label}</string><key>ProgramArguments</key><array><string>{}</string><string>_daemon</string></array><key>EnvironmentVariables</key><dict><key>XDG_CONFIG_HOME</key><string>{}</string><key>XDG_STATE_HOME</key><string>{}</string></dict></dict></plist>\n",
                koi.display(),
                config.display(),
                state.display()
            ),
        );
        let mut command = machine.command(&koi, &["uninstall", "--yes", "--json"]);
        command.env("XDG_CONFIG_HOME", &ours);

        let (output, envelope) = machine.envelope(command);

        assert!(output.status.success(), "{layout}: {envelope:#}");
        assert!(
            ours.join("koidrive/config.toml").is_file(),
            "{layout}: {envelope:#}"
        );
        assert!(
            kept(&envelope).contains(&json!({
                "kind": "shared",
                "path": ours.join("koidrive").display().to_string(),
            })),
            "{layout}: {envelope:#}"
        );
    }
}

#[test]
fn a_linked_state_folder_goes_with_its_link() {
    let machine = Machine::new();
    let koi = machine.copy_koi(".local/bin/koi");
    let elsewhere = machine.path("disk/koidrive-state");
    koidrive_state_file::create_dir(&elsewhere, &elsewhere).unwrap();
    machine.write("disk/koidrive-state/credential.json", "{}");
    machine.write(
        "disk/koidrive-state/spaces/0190f0a0-0000-7000-8000-00000000000a/store.sqlite",
        "x",
    );
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
fn a_linked_folder_whose_target_is_gone_loses_its_link() {
    let machine = Machine::new();
    let koi = machine.copy_koi(".local/bin/koi");
    for (link, target) in [
        ("state/koidrive", "disk/state-gone"),
        ("config/koidrive", "disk/config-gone"),
    ] {
        fs::create_dir_all(machine.path(link).parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(machine.path(target), machine.path(link)).unwrap();
    }

    let (output, envelope) = machine.uninstall(&koi);

    assert!(output.status.success(), "{envelope:#}");
    for link in ["state/koidrive", "config/koidrive"] {
        assert!(
            fs::symlink_metadata(machine.path(link)).is_err(),
            "{link}: {envelope:#}"
        );
    }
    assert!(!koi.exists(), "{envelope:#}");
}

#[test]
fn a_link_to_a_file_keeps_the_file_and_the_link() {
    let machine = Machine::new();
    let koi = machine.copy_koi(".local/bin/koi");
    let report = machine.write("Docs/report.docx", "report");
    let notes = machine.write("Docs/notes.txt", "notes");
    for (link, target) in [("state/koidrive", &report), ("config/koidrive", &notes)] {
        fs::create_dir_all(machine.path(link).parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(target, machine.path(link)).unwrap();
    }

    let (output, envelope) = machine.uninstall(&koi);

    assert!(output.status.success(), "{envelope:#}");
    assert_eq!(
        fs::read_to_string(&report).unwrap(),
        "report",
        "{envelope:#}"
    );
    assert_eq!(fs::read_to_string(&notes).unwrap(), "notes", "{envelope:#}");
    assert!(!koi.exists(), "{envelope:#}");
    for link in ["state/koidrive", "config/koidrive"] {
        assert!(
            fs::symlink_metadata(machine.path(link)).is_ok(),
            "{link}: {envelope:#}"
        );
        assert!(
            kept(&envelope).contains(&json!({
                "kind": "unrecognized",
                "path": machine.path(link),
            })),
            "{link}: {envelope:#}"
        );
    }
    let (output, envelope) = machine.uninstall(Path::new(env!("CARGO_BIN_EXE_koi")));
    assert!(output.status.success(), "{envelope:#}");
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

#[cfg(target_os = "macos")]
#[test]
fn a_cask_app_outside_the_applications_folders_is_handed_to_homebrew() {
    let machine = Machine::new();
    machine.profile();
    let app = machine.path("Apps/Koidrive.app");
    let helper = machine.copy_koi("Apps/Koidrive.app/Contents/Helpers/koi");
    machine.script("Apps/Koidrive.app/Contents/MacOS/Koidrive", "exit 0\n");
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
    let label = format!("dev.koidrive.koi.{}", machine.paths().state_key());
    machine.write(
        &format!("Library/LaunchAgents/{label}.plist"),
        &format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict><key>Label</key><string>{label}</string><key>ProgramArguments</key><array><string>{}</string><string>_daemon</string></array></dict></plist>\n",
            helper.display()
        ),
    );

    let (output, envelope) = machine.uninstall(Path::new(env!("CARGO_BIN_EXE_koi")));

    assert!(output.status.success(), "{envelope:#}");
    assert_eq!(
        fs::read_to_string(machine.path("brew.calls")).unwrap_or_default(),
        "uninstall --cask koidrive\n",
        "{envelope:#}"
    );
    assert!(!app.exists(), "{envelope:#}");
    let homebrew = json!({
        "kind": "homebrew",
        "argv": [fs::canonicalize(&brew).unwrap().display().to_string(), "uninstall", "--cask", "koidrive"],
    });
    assert!(removed(&envelope).contains(&homebrew), "{envelope:#}");
}

#[cfg(target_os = "macos")]
#[test]
fn a_service_started_through_a_cask_link_hands_the_app_to_homebrew() {
    let machine = Machine::new();
    machine.profile();
    let app = machine.path("Applications/Koidrive.app");
    let helper = machine.copy_koi("Applications/Koidrive.app/Contents/Helpers/koi");
    machine.script(
        "Applications/Koidrive.app/Contents/MacOS/Koidrive",
        "exit 0\n",
    );
    let owned = machine.path("custom/Caskroom/koidrive/0.1.0/Koidrive.app");
    fs::create_dir_all(owned.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&app, &owned).unwrap();
    let alias = machine.path("custom/bin/koi");
    fs::create_dir_all(alias.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&helper, &alias).unwrap();
    machine.script(
        "custom/bin/brew",
        &format!(
            "printf '%s\\n' \"$*\" >> \"$HOME/brew.calls\"\nrm -rf '{}'\n",
            app.display()
        ),
    );
    let label = format!("dev.koidrive.koi.{}", machine.paths().state_key());
    machine.write(
        &format!("Library/LaunchAgents/{label}.plist"),
        &format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict><key>Label</key><string>{label}</string><key>ProgramArguments</key><array><string>{}</string><string>_daemon</string></array></dict></plist>\n",
            alias.display()
        ),
    );

    let (output, envelope) = machine.uninstall(&helper);

    assert!(output.status.success(), "{envelope:#}");
    assert_eq!(
        fs::read_to_string(machine.path("brew.calls")).unwrap_or_default(),
        "uninstall --cask koidrive\n",
        "{envelope:#}"
    );
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
    let mut terminal = Terminal::open(160, 40);
    let mut child = terminal::spawn(
        machine
            .command(koi, args)
            .env_remove("CI")
            .env("TERM", "xterm-256color")
            .stdin(terminal.slave())
            .stdout(terminal.slave())
            .stderr(terminal.slave()),
    );
    let mut answered = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while terminal.read(&mut child, deadline) {
        if !answered
            && console::strip_ansi_codes(&String::from_utf8_lossy(terminal.output()))
                .contains("Remove Koidrive from this computer?")
        {
            terminal.write(answer.as_bytes());
            answered = true;
        }
    }
    let status = child.wait().unwrap();
    let text = console::strip_ansi_codes(&String::from_utf8_lossy(terminal.output()))
        .replace("\r\n", "\n");
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
    fs::create_dir_all(machine.path("other/state/koidrive/spaces")).unwrap();
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
