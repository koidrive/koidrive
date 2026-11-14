// SPDX-License-Identifier: Apache-2.0

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

mod schema;

#[path = "support/executable.rs"]
mod fixture;

fn asset() -> String {
    let os = match std::env::consts::OS {
        "macos" => "apple-darwin",
        _ => "unknown-linux-musl",
    };
    format!("koidrive-{}-{os}.tar.gz", std::env::consts::ARCH)
}

struct GitHub {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
}

fn github(tag: &str) -> GitHub {
    let body = serde_json::json!([{
        "tag_name": tag,
        "html_url": format!("https://github.com/koidrive/koidrive/releases/tag/{tag}"),
        "draft": false,
        "assets": [{ "name": asset() }, { "name": "SHA256SUMS" }],
    }])
    .to_string();
    http(body)
}

fn http(body: String) -> GitHub {
    http_before_reply(body, || {})
}

fn http_before_reply(body: String, before_reply: impl Fn() + Send + 'static) -> GitHub {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                request.push_str(&line);
            }
            seen.lock().unwrap().push(request);
            before_reply();
            let response = format!(
                "HTTP/1.1 200 OK\r\netag: \"e1\"\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    GitHub { base, requests }
}

fn executable(path: &Path, script: &str) {
    fixture::executable(path, script.as_bytes());
}

fn koi(home: &Path, github: &GitHub, args: &[&str]) -> Command {
    koi_at(Path::new(env!("CARGO_BIN_EXE_koi")), home, github, args)
}

fn koi_at(binary: &Path, home: &Path, github: &GitHub, args: &[&str]) -> Command {
    let mut command = Command::new(binary);
    command
        .args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_RUNTIME_DIR", home.join("run"))
        .env("HOMEBREW_PREFIX", home.join("brew"))
        .env("KOIDRIVE_TEST_INSTALL_INVENTORY", home)
        .env("KOIDRIVE_UPDATE_CHECK_URL", &github.base)
        .env_remove("KOIDRIVE_UPDATE_CHECK")
        .env_remove("KOIDRIVE_LOG");
    command
}

fn envelope(output: &Output) -> Value {
    let envelope: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{error}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    schema::check(&envelope);
    envelope
}

fn development_version() -> bool {
    koidrive_driver::update::is_development_version(env!("CARGO_PKG_VERSION"))
}

fn development_update(output: &Output, home: &Path, github: &GitHub) -> bool {
    if !development_version() {
        return false;
    }
    let envelope = envelope(output);
    assert!(output.status.success(), "{envelope:#}");
    assert_eq!(envelope["outcome"], "unchanged");
    assert_eq!(envelope["next_commands"], serde_json::json!([]));
    assert_eq!(
        envelope["data"],
        serde_json::json!({
            "from": env!("CARGO_PKG_VERSION"),
            "to": env!("CARGO_PKG_VERSION"),
            "result": "unchanged",
        })
    );
    assert!(github.requests.lock().unwrap().is_empty());
    assert!(!home.join("brew/bin/brew.calls").exists());
    assert!(!home.join("brew/opt/koidrive/bin/koi.calls").exists());
    true
}

const REPORT: &str = r#"{"schema_version":1,"command":"_reconcile","outcome":"ok","data":{"service":{"manager":"launchd","name":"dev.koidrive.koi","state":"running"}}}"#;

fn copy_koi(path: &Path) -> PathBuf {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let status = Command::new("/bin/cp")
        .arg(env!("CARGO_BIN_EXE_koi"))
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success(), "fixture copy failed: {status}");
    path.to_path_buf()
}

fn script_install(home: &Path, version: &str, report: &str) -> (PathBuf, GitHub) {
    script_install_saying(home, version, report, "")
}

fn script_install_saying(
    home: &Path,
    version: &str,
    report: &str,
    said: &str,
) -> (PathBuf, GitHub) {
    let binary = copy_koi(&home.join(".local/bin/koi"));
    let helper = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\ncase \"$1\" in\n--version) echo 'koi {version}' ;;\n_reconcile) echo '{report}' ;;\nesac\n"
    );
    let installer = http(format!(
        "{said}printf '%s|%s\\n' \"$KOIDRIVE_INSTALL_DIR\" \"$KOIDRIVE_VERSION\" >> \"$KOIDRIVE_INSTALL_DIR/installer.calls\"\nprintf '%s' '{}' > \"$KOIDRIVE_INSTALL_DIR/koi.new\"\nchmod +x \"$KOIDRIVE_INSTALL_DIR/koi.new\"\nmv \"$KOIDRIVE_INSTALL_DIR/koi.new\" \"$KOIDRIVE_INSTALL_DIR/koi\"\n",
        helper.replace('\'', "'\\''")
    ));
    (binary, installer)
}

fn homebrew_binary(home: &Path, cask: bool) -> PathBuf {
    let prefix = home.join("brew");
    let binary = if cask {
        let app = home.join("Applications/Koidrive.app");
        let binary = copy_koi(&app.join("Contents/Helpers/koi"));
        let owned = prefix.join("Caskroom/koidrive/0.1.0/Koidrive.app");
        std::fs::create_dir_all(owned.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&app, owned).unwrap();
        binary
    } else {
        let binary = copy_koi(&prefix.join("Cellar/koidrive/0.1.0/bin/koi"));
        let opt = prefix.join("opt/koidrive");
        std::fs::create_dir_all(opt.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(binary.parent().unwrap().parent().unwrap(), opt).unwrap();
        binary
    };
    let link = prefix.join("bin/koi");
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(binary, &link).unwrap();
    executable(
        &prefix.join("bin/brew"),
        "#!/bin/sh\necho called >> \"$0.calls\"\nexit 97\n",
    );
    link
}

fn update_log(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Logs/Koidrive/update.log")
    } else {
        home.join("state/koidrive/update.log")
    }
}

fn homebrew(home: &Path, version: &str) -> PathBuf {
    let opt = home.join("brew/opt/koidrive/bin/koi");
    let koi = |version: &str| {
        format!(
            "#!/bin/sh\ncase \"$1\" in\n--version) echo 'koi {version}' ;;\n_reconcile) echo reconciled >> \"$0.calls\"; echo '{REPORT}' ;;\nesac\n"
        )
    };
    executable(&opt, &koi(version));
    let upgraded = koi("99.0.0").replace('\'', "'\\''");
    executable(
        &home.join("brew/bin/brew"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\n[ \"$1\" = upgrade ] && printf '%s' '{upgraded}' > '{}'\nexit 0\n",
            opt.display()
        ),
    );
    home.join("brew/bin/brew.calls")
}

fn upgrading_brew(prefix: &Path, koi: &Path, version: &str, status: i32) -> PathBuf {
    let tap = prefix.join("Library/Taps/koidrive/homebrew-tap");
    std::fs::create_dir_all(tap.join(".git")).unwrap();
    executable(
        &prefix.join("bin/git"),
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\n",
    );
    let upgraded = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\ncase \"$1\" in\n--version) echo 'koi {version}' ;;\n_reconcile) echo '{REPORT}' ;;\nesac\n"
    )
    .replace('\'', "'\\''");
    executable(
        &prefix.join("bin/brew"),
        &format!(
            "#!/bin/sh\nprintf '%s|%s\\n' \"$*\" \"$HOMEBREW_NO_AUTO_UPDATE\" >> \"$0.calls\"\ncase \"$1\" in\n--repository) echo '{tap}' ;;\nupgrade)\n  echo '==> Upgrading koidrive'\n  if [ {status} -ne 0 ]; then echo 'Error: koidrive did not download' >&2; exit {status}; fi\n  printf '%s' '{upgraded}' > '{koi}.new' && /bin/chmod +x '{koi}.new' && /bin/mv '{koi}.new' '{koi}'\n  ;;\nesac\n",
            tap = tap.display(),
            koi = koi.display(),
        ),
    );
    prefix.join("bin/brew.calls")
}

#[test]
fn development_update_never_contacts_github_or_runs_an_installer() {
    if !development_version() {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    homebrew(home.path(), "0.0.1");
    let script = home.path().join(".local/bin/koi");
    let app = home
        .path()
        .join("Applications/Koidrive.app/Contents/Helpers/koi");
    for path in [&script, &app] {
        executable(
            path,
            "#!/bin/sh\necho called >> \"$0.calls\"\necho 'koi 0.0.1'\n",
        );
    }
    for switch in ["on", "off"] {
        let output = koi(home.path(), &github, &["update", "--json"])
            .env("KOIDRIVE_UPDATE_CHECK", switch)
            .output()
            .unwrap();
        assert!(development_update(&output, home.path(), &github));
        assert!(!script.with_extension("calls").exists());
        assert!(!app.with_extension("calls").exists());
    }
    let output = koi(home.path(), &github, &["update"])
        .env("KOIDRIVE_UPDATE_CHECK_URL", "not a URL")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "Development builds do not update to releases.\n"
    );
    assert!(github.requests.lock().unwrap().is_empty());
}

#[test]
fn koi_update_installs_a_release_and_hands_the_service_to_the_new_koi() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let (binary, installer) = script_install(home.path(), "99.0.0", REPORT);
    let calls = homebrew(home.path(), "100.0.0");
    let output = koi_at(&binary, home.path(), &github, &["update", "--json"])
        .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
        .output()
        .unwrap();
    if development_update(&output, home.path(), &github) {
        return;
    }
    let envelope = envelope(&output);
    assert!(output.status.success(), "{envelope:#}");
    assert_eq!(
        envelope["data"],
        serde_json::json!({
            "from": env!("CARGO_PKG_VERSION"),
            "to": "99.0.0",
            "via": "installer",
            "result": "updated",
            "service": { "manager": "launchd", "name": "dev.koidrive.koi", "state": "running" },
        })
    );
    assert_eq!(
        std::fs::read_to_string(binary.parent().unwrap().join("installer.calls")).unwrap(),
        format!(
            "{}|v99.0.0\n",
            std::fs::canonicalize(binary.parent().unwrap())
                .unwrap()
                .display()
        )
    );
    assert!(!calls.exists());
    assert_eq!(installer.requests.lock().unwrap().len(), 1);
    assert!(
        std::fs::read_to_string(binary.with_extension("calls"))
            .unwrap()
            .contains("_reconcile --json\n")
    );
    let requests = github.requests.lock().unwrap();
    assert!(
        requests[0].starts_with("GET /repos/koidrive/koidrive/releases?per_page=20 HTTP/1.1\r\n"),
        "{requests:?}"
    );
}

#[test]
fn koi_update_reconciles_a_concurrently_installed_newest_release_without_an_installer() {
    let home = tempfile::tempdir().unwrap();
    let (binary, installer) = script_install(home.path(), "99.0.0", REPORT);
    let replacement = binary.clone();
    let releases = serde_json::json!([{
        "tag_name": "v99.0.0",
        "html_url": "https://example.com/v99.0.0",
        "assets": [{ "name": asset() }, { "name": "SHA256SUMS" }],
    }]);
    let github = http_before_reply(releases.to_string(), move || {
        let staged = replacement.with_extension("new");
        executable(
            &staged,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\ncase \"$1\" in\n--version) echo 'koi 99.0.0' ;;\n_reconcile) echo '{REPORT}' ;;\nesac\n"
            ),
        );
        std::fs::rename(staged, &replacement).unwrap();
    });
    let output = koi_at(&binary, home.path(), &github, &["update", "--json"])
        .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
        .output()
        .unwrap();
    if development_update(&output, home.path(), &github) {
        return;
    }
    let envelope = envelope(&output);
    assert!(output.status.success(), "{envelope:#}");
    assert_eq!(envelope["outcome"], "unchanged");
    assert_eq!(
        envelope["data"],
        serde_json::json!({
            "from": "99.0.0", "to": "99.0.0", "via": "installer", "result": "unchanged",
            "service": { "manager": "launchd", "name": "dev.koidrive.koi", "state": "running" },
        })
    );
    assert!(installer.requests.lock().unwrap().is_empty());
    assert!(
        std::fs::read_to_string(binary.with_extension("calls"))
            .unwrap()
            .contains("_reconcile --json\n")
    );
}

#[test]
fn koi_update_rechecks_foreign_and_legacy_caches_but_keeps_same_target_conditionals() {
    for cached_asset in [
        Some("koidrive-foreign-target.tar.gz".to_owned()),
        None,
        Some(asset()),
    ] {
        let home = tempfile::tempdir().unwrap();
        let (binary, installer) = script_install(home.path(), "99.0.0", REPORT);
        let state = home.path().join("state/koidrive");
        std::fs::create_dir_all(&state).unwrap();
        let mut cached = serde_json::json!({
            "checked_at": 1763006400000_i64,
            "successful_at": 1763006400000_i64,
            "etag": "\"cached\"",
            "release": { "version": "99.0.0", "url": "https://example.com/v99.0.0" },
        });
        if let Some(asset) = &cached_asset {
            cached["asset"] = asset.clone().into();
        }
        std::fs::write(state.join("update-check.json"), cached.to_string()).unwrap();
        if development_version() {
            let github = github("v99.0.0");
            let output = koi_at(&binary, home.path(), &github, &["update", "--json"])
                .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
                .output()
                .unwrap();
            assert!(development_update(&output, home.path(), &github));
            continue;
        }
        let same_target = cached_asset.as_deref() == Some(asset().as_str());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let github = GitHub {
            base: format!("http://{}", listener.local_addr().unwrap()),
            requests: Arc::new(Mutex::new(Vec::new())),
        };
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
                request.push_str(&line);
            }
            let body = serde_json::json!([{
                "tag_name": "v99.0.0",
                "html_url": "https://example.com/v99.0.0",
                "assets": [{ "name": "koidrive-foreign-target.tar.gz" }],
            }])
            .to_string();
            let response = if same_target {
                "HTTP/1.1 304 Not Modified\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    .to_owned()
            } else {
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
            };
            stream.write_all(response.as_bytes()).unwrap();
            request
        });
        let output = koi_at(&binary, home.path(), &github, &["update", "--json"])
            .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
            .output()
            .unwrap();
        let request = server.join().unwrap().to_ascii_lowercase();
        assert_eq!(
            request.contains("if-none-match: \"cached\""),
            same_target,
            "{request}"
        );
        let envelope = envelope(&output);
        assert_eq!(output.status.success(), same_target, "{envelope:#}");
        if same_target {
            assert_eq!(envelope["outcome"], "ok");
            assert_eq!(envelope["data"]["to"], "99.0.0");
            assert_eq!(envelope["data"]["result"], "updated");
        } else {
            assert_eq!(envelope["outcome"], "error");
            assert_eq!(
                envelope["error"]["message"],
                "GitHub lists no Koidrive release for this system"
            );
        }
        assert_eq!(
            installer.requests.lock().unwrap().len(),
            usize::from(same_target)
        );
    }
}

#[test]
fn koi_update_preserves_disabled_login_items_and_only_fails_for_a_real_problem() {
    for problem in [None, Some("the service could not be registered")] {
        let home = tempfile::tempdir().unwrap();
        let github = github("v99.0.0");
        let mut report: Value = serde_json::from_str(REPORT).unwrap();
        report["data"]["service"]["state"] = Value::from("switched_off");
        if let Some(problem) = problem {
            report["data"]["service"]["problem"] = Value::from(problem);
        }
        let (binary, installer) = script_install(home.path(), "99.0.0", &report.to_string());
        let output = koi_at(&binary, home.path(), &github, &["update", "--json"])
            .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
            .output()
            .unwrap();
        if development_update(&output, home.path(), &github) {
            continue;
        }
        let envelope = envelope(&output);
        assert_eq!(output.status.success(), problem.is_none(), "{envelope:#}");
        assert_eq!(envelope["data"]["result"], "updated");
        assert_eq!(envelope["data"]["service"]["state"], "switched_off");
        copy_koi(&binary);
        let output = koi_at(&binary, home.path(), &github, &["update"])
            .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
            .output()
            .unwrap();
        assert_eq!(output.status.success(), problem.is_none());
        let text = String::from_utf8(output.stdout).unwrap();
        match problem {
            None => assert!(text.contains("Login Items"), "{text}"),
            Some(problem) => assert!(text.contains(problem), "{text}"),
        }
    }
}

#[test]
fn schema_one_status_without_an_update_notice_is_still_valid() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let output = koi(home.path(), &github, &["status", "--json"])
        .output()
        .unwrap();
    let mut envelope = envelope(&output);
    assert!(
        envelope["data"]
            .as_object_mut()
            .unwrap()
            .remove("update")
            .is_some()
    );
    schema::check(&envelope);
}

#[test]
fn koi_update_text_hides_the_reconcile_envelope_but_keeps_it_in_the_log() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let (binary, installer) = script_install(home.path(), "99.0.0", REPORT);
    let output = koi_at(&binary, home.path(), &github, &["update"])
        .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
        .output()
        .unwrap();
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{text}");
    if development_version() {
        assert_eq!(text, "Development builds do not update to releases.\n");
        assert!(github.requests.lock().unwrap().is_empty());
        assert!(!home.path().join("brew/bin/brew.calls").exists());
        assert!(!home.path().join("brew/opt/koidrive/bin/koi.calls").exists());
        return;
    }
    assert!(
        text.contains(&format!(
            "Updated Koidrive from {} to 99.0.0.",
            env!("CARGO_PKG_VERSION")
        )),
        "{text}"
    );
    assert!(!text.contains("schema_version"), "{text}");
    let log = update_log(home.path());
    assert!(std::fs::read_to_string(log).unwrap().contains(REPORT));
}

const CHECKS: &str =
    "echo 'Signature verified for SHA256SUMS'\necho 'SHA256 verified for koidrive.tar.gz'\n";

fn update_text(said: &str, args: &[&str], report: &str) -> Option<(Output, String)> {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let (binary, installer) = script_install_saying(home.path(), "99.0.0", report, said);
    let output = koi_at(&binary, home.path(), &github, args)
        .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
        .output()
        .unwrap();
    if development_version() {
        assert!(output.status.success());
        return None;
    }
    let log = std::fs::read_to_string(update_log(home.path())).unwrap();
    Some((output, log))
}

#[test]
fn koi_update_shows_its_steps_and_keeps_the_installer_lines_in_the_log() {
    let Some((output, log)) = update_text(CHECKS, &["update"], REPORT) else {
        return;
    };
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(output.status.success(), "{stdout}{stderr}");
    assert_eq!(
        stderr,
        "Downloading Koidrive 99.0.0\nDownloaded, checked and installed Koidrive 99.0.0.\nChecking the background service\nChecked the background service.\n"
    );
    assert!(
        stdout.starts_with(&format!(
            "Updated Koidrive from {} to 99.0.0.\n",
            env!("CARGO_PKG_VERSION")
        )),
        "{stdout}"
    );
    assert!(!stdout.contains("verified"), "{stdout}");
    assert!(log.contains("Signature verified for SHA256SUMS"), "{log}");
}

#[test]
fn koi_update_verbose_prints_the_installer_lines() {
    let Some((output, _)) = update_text(CHECKS, &["update", "--verbose"], REPORT) else {
        return;
    };
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.starts_with(
            "Signature verified for SHA256SUMS\nSHA256 verified for koidrive.tar.gz\n"
        ),
        "{stdout}"
    );
}

#[test]
fn a_failed_install_shows_what_the_installer_said() {
    let said = "echo 'Downloaded koidrive.tar.gz'\necho 'SHA256 mismatch for koidrive.tar.gz' >&2\nexit 1\n";
    let Some((output, _)) = update_text(said, &["update"], REPORT) else {
        return;
    };
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.starts_with("Downloading Koidrive 99.0.0\n"),
        "{stderr}"
    );
    for said in [
        "Downloaded koidrive.tar.gz\n",
        "SHA256 mismatch for koidrive.tar.gz\n",
    ] {
        assert!(stderr.contains(said), "{stderr}");
    }
    assert!(
        stderr.contains("install.sh failed (exit status 1)"),
        "{stderr}"
    );
    assert!(!stderr.contains("Downloaded, checked"), "{stderr}");
}

#[test]
fn koi_update_on_a_terminal_shows_setup_style_steps_without_the_checks() {
    let home = tempfile::tempdir_in("/tmp").unwrap();
    let github = github("v99.0.0");
    let (binary, installer) = script_install_saying(home.path(), "99.0.0", REPORT, CHECKS);
    let mut command = koi_at(&binary, home.path(), &github, &["update"]);
    command
        .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
        .env("NO_COLOR", "1")
        .env("TERM", "xterm-256color")
        .env_remove("KOI_PLAIN");
    let terminal = on_a_terminal(command, None);
    if development_version() {
        assert!(terminal.contains("Development builds do not update to releases."));
        return;
    }
    assert!(
        terminal.contains("◇  Downloaded, checked and installed Koidrive 99.0.0\n"),
        "{terminal}"
    );
    assert!(
        terminal.contains("◇  Checked the background service\n"),
        "{terminal}"
    );
    assert!(
        terminal.contains(&format!(
            "Updated Koidrive from {} to 99.0.0.",
            env!("CARGO_PKG_VERSION")
        )),
        "{terminal}"
    );
    assert!(!terminal.contains("verified"), "{terminal}");
}

fn on_a_terminal(mut command: Command, typed: Option<(Duration, &[u8])>) -> String {
    use std::io::Read;
    let master =
        rustix::pty::openpt(rustix::pty::OpenptFlags::RDWR | rustix::pty::OpenptFlags::NOCTTY)
            .unwrap();
    rustix::pty::grantpt(&master).unwrap();
    rustix::pty::unlockpt(&master).unwrap();
    let name = rustix::pty::ptsname(&master, Vec::new()).unwrap();
    let slave = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(name.to_str().unwrap())
        .unwrap();
    let size = rustix::termios::Winsize {
        ws_row: 32,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    rustix::termios::tcsetwinsize(&slave, size).unwrap();
    command
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave);
    unsafe {
        command.pre_exec(|| {
            rustix::process::setsid()?;
            rustix::process::ioctl_tiocsctty(std::io::stdin())?;
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    drop(command);
    let master = std::fs::File::from(master);
    let mut reading = master.try_clone().unwrap();
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            match reading.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => bytes.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&bytes).replace("\r\n", "\n")
    });
    if let Some((after, bytes)) = typed {
        std::thread::sleep(after);
        (&master).write_all(bytes).unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    while child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "koi update did not exit");
        std::thread::sleep(Duration::from_millis(10));
    }
    reader.join().unwrap()
}

const STOPPED: &str = r#"{"schema_version":1,"command":"_reconcile","outcome":"ok","data":{"service":{"manager":"launchd","name":"dev.koidrive.koi","state":"stopped"}}}"#;

#[test]
fn a_failed_service_check_shows_only_its_own_output() {
    let Some((output, _)) = update_text(CHECKS, &["update"], STOPPED) else {
        return;
    };
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.starts_with("Downloading Koidrive 99.0.0\nDownloaded, checked and installed Koidrive 99.0.0.\nChecking the background service\n"),
        "{stderr}"
    );
    assert!(!stderr.contains("verified"), "{stderr}");
}

#[test]
fn an_installer_that_leaves_the_old_koi_fails_its_step() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let binary = copy_koi(&home.path().join(".local/bin/koi"));
    let installer = http("echo 'Installed koi'\nexit 0\n".to_owned());
    let output = koi_at(&binary, home.path(), &github, &["update"])
        .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
        .output()
        .unwrap();
    if development_version() {
        assert!(output.status.success());
        return;
    }
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.starts_with("Downloading Koidrive 99.0.0\nInstalled koi\n"),
        "{stderr}"
    );
    assert!(stderr.contains("is still Koidrive"), "{stderr}");
    assert!(!stderr.contains("Downloaded, checked"), "{stderr}");
    assert!(
        !stderr.contains("Checking the background service"),
        "{stderr}"
    );
}

#[test]
fn a_homebrew_password_prompt_stays_on_screen_until_it_is_answered() {
    if development_version() {
        return;
    }
    let home = tempfile::tempdir_in("/tmp").unwrap();
    let github = github("v99.0.0");
    let link = homebrew_binary(home.path(), false);
    let koi = std::fs::canonicalize(&link).unwrap();
    let prefix = std::fs::canonicalize(home.path().join("brew")).unwrap();
    let typed = home.path().join("typed");
    scripted_brew(
        &prefix,
        0,
        &format!(
            "  printf 'Password:' > /dev/tty\n  IFS= read -r answer < /dev/tty\n  printf '%s' \"$answer\" > '{}'\n{}",
            typed.display(),
            replacing(&koi, "99.0.0", REPORT)
        ),
    );
    let mut command = koi_at(&link, home.path(), &github, &["update"]);
    command
        .env("PATH", home.path().join("empty"))
        .env("NO_COLOR", "1")
        .env("TERM", "xterm-256color")
        .env_remove("KOI_PLAIN");
    let terminal = on_a_terminal(command, Some((Duration::from_secs(3), b"secret\n")));
    assert_eq!(std::fs::read_to_string(&typed).unwrap(), "secret");
    let (_, after) = terminal.split_once("Password:").unwrap();
    let waiting = after
        .split_once("secret")
        .map_or(after, |(before, _)| before);
    assert!(
        !waiting.contains('\r') && !waiting.contains('\u{1b}'),
        "{waiting:?}"
    );
}

#[test]
fn koi_update_rejects_a_successful_installer_that_leaves_the_old_binary_installed() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let binary = copy_koi(&home.path().join(".local/bin/koi"));
    let installer = http("exit 0\n".to_owned());
    let started = Instant::now();
    let output = koi_at(&binary, home.path(), &github, &["update", "--json"])
        .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
        .output()
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(10));
    if development_update(&output, home.path(), &github) {
        return;
    }
    let envelope = envelope(&output);
    assert!(!output.status.success(), "{envelope:#}");
    assert_eq!(envelope["outcome"], "error");
    assert!(
        envelope["error"]["message"]
            .as_str()
            .unwrap()
            .contains(env!("CARGO_PKG_VERSION"))
    );
    assert_ne!(envelope["data"]["result"], "updated");
    assert!(!binary.with_extension("calls").exists());
    assert_eq!(installer.requests.lock().unwrap().len(), 1);
}

#[test]
fn koi_update_reports_the_version_actually_installed_when_it_is_above_the_target() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let (binary, installer) = script_install(home.path(), "100.0.0", REPORT);
    let output = koi_at(&binary, home.path(), &github, &["update", "--json"])
        .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
        .output()
        .unwrap();
    if development_update(&output, home.path(), &github) {
        return;
    }
    let envelope = envelope(&output);
    assert!(output.status.success(), "{envelope:#}");
    assert_eq!(envelope["data"]["to"], "100.0.0");
    assert_eq!(envelope["data"]["result"], "updated");
}

#[test]
fn koi_update_through_a_script_alias_installs_into_the_canonical_directory() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let (original, installer) = script_install(home.path(), "99.0.0", REPORT);
    let directory = home.path().join("standalone install/bin");
    std::fs::create_dir_all(directory.parent().unwrap()).unwrap();
    std::fs::rename(original.parent().unwrap(), &directory).unwrap();
    let binary = directory.join("koi");
    let first = home.path().join("first-alias");
    let alias = home.path().join("second-alias");
    std::os::unix::fs::symlink(&binary, &first).unwrap();
    std::os::unix::fs::symlink(&first, &alias).unwrap();
    let calls = homebrew(home.path(), "100.0.0");
    let output = koi_at(&alias, home.path(), &github, &["update", "--json"])
        .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
        .output()
        .unwrap();
    if development_update(&output, home.path(), &github) {
        return;
    }
    let envelope = envelope(&output);
    assert!(output.status.success(), "{envelope:#}");
    assert_eq!(envelope["data"]["via"], "installer");
    assert_eq!(envelope["data"]["result"], "updated");
    assert_eq!(envelope["data"]["to"], "99.0.0");
    assert_eq!(
        std::fs::read_to_string(directory.join("installer.calls")).unwrap(),
        format!(
            "{}|v99.0.0\n",
            std::fs::canonicalize(&directory).unwrap().display()
        )
    );
    assert!(
        std::fs::read_to_string(binary.with_extension("calls"))
            .unwrap()
            .contains("_reconcile --json\n")
    );
    assert_eq!(std::fs::read_link(&alias).unwrap(), first);
    assert_eq!(std::fs::read_link(&first).unwrap(), binary);
    assert!(!calls.exists());
    assert!(!home.path().join("koi").exists());
    assert!(!original.exists());
}

#[test]
fn an_app_outside_the_installer_inventory_never_updates_another_bundle() {
    custom_app_update("v99.0.0", true);
}

#[test]
fn an_app_outside_the_installer_inventory_is_unchanged_at_the_target() {
    custom_app_update(&format!("v{}", env!("CARGO_PKG_VERSION")), false);
}

#[test]
fn an_app_outside_the_installer_inventory_is_unchanged_above_the_target() {
    custom_app_update("v0.0.1", false);
}

fn custom_app_update(tag: &str, upgrading: bool) {
    let home = tempfile::tempdir().unwrap();
    let github = github(tag);
    let binary = copy_koi(
        &home
            .path()
            .join("Custom Apps/Koidrive.app/Contents/Helpers/koi"),
    );
    let other = home
        .path()
        .join("Applications/Koidrive.app/Contents/Helpers/koi");
    executable(
        &other,
        "#!/bin/sh\necho called >> \"$0.calls\"\necho 'koi 99.0.0'\n",
    );
    let before = std::fs::read(&binary).unwrap();
    let other_before = std::fs::read(&other).unwrap();
    let output = koi_at(&binary, home.path(), &github, &["update", "--json"])
        .output()
        .unwrap();
    if development_update(&output, home.path(), &github) {
        return;
    }
    let response = envelope(&output);
    if upgrading {
        assert!(!output.status.success(), "{response:#}");
        assert_eq!(response["outcome"], "error");
        assert_eq!(
            response["error"]["message"],
            "Download Koidrive 99.0.0 from https://github.com/koidrive/koidrive/releases/tag/v99.0.0"
        );
    } else {
        assert!(output.status.success(), "{response:#}");
        assert_eq!(response["outcome"], "unchanged");
        assert_eq!(
            response["data"],
            serde_json::json!({
                "from": env!("CARGO_PKG_VERSION"),
                "to": env!("CARGO_PKG_VERSION"),
                "via": "app",
                "result": "unchanged",
            })
        );
    }
    assert_eq!(response["next_commands"], serde_json::json!([]));
    assert_eq!(std::fs::read(&binary).unwrap(), before);
    assert_eq!(std::fs::read(&other).unwrap(), other_before);
    assert!(!other.with_extension("calls").exists());
    assert!(!update_log(home.path()).exists());
    assert_eq!(github.requests.lock().unwrap().len(), 1);
}

#[test]
fn an_app_outside_the_installer_inventory_uses_the_running_version_without_probing_its_path() {
    if development_version() {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let binary = copy_koi(
        &home
            .path()
            .join("Custom Apps/Koidrive.app/Contents/Helpers/koi"),
    );
    let replacement = binary.clone();
    let releases = serde_json::json!([{
        "tag_name": format!("v{}", env!("CARGO_PKG_VERSION")),
        "html_url": "https://example.com/current",
        "assets": [{ "name": asset() }, { "name": "SHA256SUMS" }],
    }]);
    let github = http_before_reply(releases.to_string(), move || {
        let staged = replacement.with_extension("new");
        executable(
            &staged,
            "#!/bin/sh\necho called >> \"$0.calls\"\necho 'koi 0.0.1'\n",
        );
        std::fs::rename(staged, &replacement).unwrap();
    });
    let output = koi_at(&binary, home.path(), &github, &["update", "--json"])
        .output()
        .unwrap();
    let response = envelope(&output);
    assert!(output.status.success(), "{response:#}");
    assert_eq!(response["outcome"], "unchanged");
    assert_eq!(
        response["data"],
        serde_json::json!({
            "from": env!("CARGO_PKG_VERSION"),
            "to": env!("CARGO_PKG_VERSION"),
            "via": "app",
            "result": "unchanged",
        })
    );
    assert!(!binary.with_extension("calls").exists());
    assert!(!update_log(home.path()).exists());
    assert_eq!(github.requests.lock().unwrap().len(), 1);
}

#[test]
fn an_app_outside_the_installer_inventory_status_names_the_download_not_an_update_command() {
    if development_version() {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let binary = copy_koi(
        &home
            .path()
            .join("Custom Apps/Koidrive.app/Contents/Helpers/koi"),
    );
    let other = home
        .path()
        .join("Applications/Koidrive.app/Contents/Helpers/koi");
    executable(
        &other,
        "#!/bin/sh\necho called >> \"$0.calls\"\necho 'koi 99.0.0'\n",
    );
    let before = std::fs::read(&binary).unwrap();
    let other_before = std::fs::read(&other).unwrap();
    let state = home.path().join("state/koidrive");
    std::fs::create_dir_all(&state).unwrap();
    let url = "https://github.com/koidrive/koidrive/releases/tag/v99.0.0";
    std::fs::write(
        state.join("update-check.json"),
        serde_json::json!({
            "asset": asset(),
            "checked_at": 1763006400000_i64,
            "release": { "version": "99.0.0", "url": url },
        })
        .to_string(),
    )
    .unwrap();
    let output = koi_at(&binary, home.path(), &github, &["status", "--json"])
        .output()
        .unwrap();
    let response = envelope(&output);
    let update = &response["data"]["update"];
    assert_eq!(update["state"], "available");
    assert_eq!(update["url"], url);
    assert!(update.get("command").is_none(), "{update:#}");
    assert_eq!(
        response["next_commands"],
        serde_json::json!([{ "argv": ["koi", "setup"] }])
    );
    let output = koi_at(&binary, home.path(), &github, &["status"])
        .output()
        .unwrap();
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains(&format!("Download: {url}")), "{text}");
    assert!(!text.contains("Run:"), "{text}");
    assert!(github.requests.lock().unwrap().is_empty());
    assert!(!update_log(home.path()).exists());
    assert_eq!(std::fs::read(&binary).unwrap(), before);
    assert_eq!(std::fs::read(&other).unwrap(), other_before);
    assert!(!other.with_extension("calls").exists());
}

#[test]
fn koi_update_keeps_a_direct_app_helper_on_the_manual_download_path() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let binary = copy_koi(
        &home
            .path()
            .join("Applications/Koidrive.app/Contents/Helpers/koi"),
    );
    let before = std::fs::read(&binary).unwrap();
    let (script, installer) = script_install(home.path(), "99.0.0", REPORT);
    let script_before = std::fs::read(&script).unwrap();
    let calls = homebrew(home.path(), "100.0.0");
    let output = koi_at(&binary, home.path(), &github, &["update", "--json"])
        .env("KOIDRIVE_INSTALL_SCRIPT_URL", &installer.base)
        .output()
        .unwrap();
    if development_update(&output, home.path(), &github) {
        return;
    }
    let response = envelope(&output);
    assert!(!output.status.success(), "{response:#}");
    assert_eq!(response["outcome"], "error");
    let message = response["error"]["message"].as_str().unwrap();
    assert!(
        message.ends_with(
            "Download Koidrive 99.0.0 from https://github.com/koidrive/koidrive/releases/tag/v99.0.0"
        ),
        "{message}"
    );
    if cfg!(target_os = "macos") {
        assert!(
            message.starts_with("App signature verification failed"),
            "{message}"
        );
    }
    assert_eq!(response["next_commands"], serde_json::json!([]));
    assert_eq!(github.requests.lock().unwrap().len(), 1);
    assert!(installer.requests.lock().unwrap().is_empty());
    assert!(!calls.exists());
    assert!(!script.with_extension("calls").exists());
    assert!(!binary.with_extension("calls").exists());
    assert_eq!(std::fs::read(&binary).unwrap(), before);
    assert_eq!(std::fs::read(&script).unwrap(), script_before);
    let state = home.path().join("state/koidrive");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        state.join("update-check.json"),
        serde_json::json!({
            "asset": asset(),
            "checked_at": 1763006400000_i64,
            "release": {
                "version": "99.0.0",
                "url": "https://github.com/koidrive/koidrive/releases/tag/v99.0.0",
            },
        })
        .to_string(),
    )
    .unwrap();
    let output = koi_at(&binary, home.path(), &github, &["status", "--json"])
        .output()
        .unwrap();
    let update = envelope(&output)["data"]["update"].clone();
    assert_eq!(update["state"], "available");
    let command = format!(
        "{} update",
        std::fs::canonicalize(&binary).unwrap().display()
    );
    assert_eq!(update["command"], command, "{update:#}");
    let output = koi_at(&binary, home.path(), &github, &["status"])
        .output()
        .unwrap();
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains(&format!("Run: {command}")), "{text}");
    assert!(!text.contains("Run: koi update"), "{text}");
    assert_eq!(github.requests.lock().unwrap().len(), 1);
}

#[test]
fn koi_update_upgrades_a_formula_or_cask_with_the_brew_that_owns_it() {
    for cask in [false, true] {
        for alias in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let github = github("v99.0.0");
            let link = homebrew_binary(home.path(), cask);
            let koi = std::fs::canonicalize(&link).unwrap();
            let prefix = std::fs::canonicalize(home.path().join("brew")).unwrap();
            let calls = upgrading_brew(&prefix, &koi, "99.0.0", 0);
            let elsewhere = home.path().join("elsewhere/bin");
            executable(
                &elsewhere.join("brew"),
                "#!/bin/sh\necho selected >> \"$0.calls\"\nexit 99\n",
            );
            let invoked = if alias {
                let first = home.path().join("alias-one");
                let second = home.path().join("alias-two");
                std::os::unix::fs::symlink(&link, &first).unwrap();
                std::os::unix::fs::symlink(first, &second).unwrap();
                second
            } else {
                link.clone()
            };
            let output = koi_at(&invoked, home.path(), &github, &["update", "--json"])
                .env("PATH", &elsewhere)
                .output()
                .unwrap();
            if development_update(&output, home.path(), &github) {
                continue;
            }
            let envelope = envelope(&output);
            assert!(output.status.success(), "{envelope:#}");
            assert_eq!(
                envelope["data"],
                serde_json::json!({
                    "from": env!("CARGO_PKG_VERSION"),
                    "to": "99.0.0",
                    "via": "homebrew",
                    "result": "updated",
                    "service": { "manager": "launchd", "name": "dev.koidrive.koi", "state": "running" },
                })
            );
            let upgrade = if cask {
                "upgrade --cask koidrive"
            } else {
                "upgrade koidrive"
            };
            assert_eq!(
                std::fs::read_to_string(&calls).unwrap(),
                format!("--repository koidrive/tap|1\n{upgrade}|1\n")
            );
            assert_eq!(
                std::fs::read_to_string(prefix.join("bin/git.calls")).unwrap(),
                format!(
                    "-C {} pull --ff-only --quiet\n",
                    prefix.join("Library/Taps/koidrive/homebrew-tap").display()
                )
            );
            assert!(
                std::fs::read_to_string(koi.with_extension("calls"))
                    .unwrap()
                    .contains("_reconcile --json\n")
            );
            assert!(String::from_utf8_lossy(&output.stderr).contains("==> Upgrading koidrive\n"));
            assert!(!elsewhere.join("brew.calls").exists());
            assert_eq!(github.requests.lock().unwrap().len(), 1);
        }
    }
}

#[test]
fn koi_update_through_homebrew_says_when_koidrive_is_already_the_newest() {
    for json in [true, false] {
        let home = tempfile::tempdir().unwrap();
        let link = homebrew_binary(home.path(), false);
        let koi = std::fs::canonicalize(&link).unwrap();
        let prefix = std::fs::canonicalize(home.path().join("brew")).unwrap();
        let calls = upgrading_brew(&prefix, &koi, env!("CARGO_PKG_VERSION"), 0);
        let tag = concat!("v", env!("CARGO_PKG_VERSION"));
        let releases = serde_json::json!([{
            "tag_name": tag,
            "html_url": format!("https://example.com/{tag}"),
            "assets": [{ "name": asset() }, { "name": "SHA256SUMS" }],
        }]);
        let replacement = koi.clone();
        let github = http_before_reply(releases.to_string(), move || {
            let staged = replacement.with_extension("new");
            executable(
                &staged,
                &format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\ncase \"$1\" in\n--version) echo 'koi {}' ;;\n_reconcile) echo '{REPORT}' ;;\nesac\n",
                    env!("CARGO_PKG_VERSION")
                ),
            );
            std::fs::rename(staged, &replacement).unwrap();
        });
        let args: &[&str] = if json {
            &["update", "--json"]
        } else {
            &["update"]
        };
        let output = koi_at(&link, home.path(), &github, args)
            .env("PATH", home.path().join("empty"))
            .output()
            .unwrap();
        if json {
            if development_update(&output, home.path(), &github) {
                return;
            }
            let envelope = envelope(&output);
            assert!(output.status.success(), "{envelope:#}");
            assert_eq!(envelope["outcome"], "unchanged");
            assert_eq!(envelope["data"]["result"], "unchanged");
            assert!(!calls.exists());
            assert!(
                std::fs::read_to_string(koi.with_extension("calls"))
                    .unwrap()
                    .contains("_reconcile --json\n")
            );
            continue;
        }
        assert!(output.status.success(), "{output:?}");
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(
            text.starts_with(&format!(
                "Koidrive {} is the newest release.\n",
                env!("CARGO_PKG_VERSION")
            )),
            "{text}"
        );
        assert!(!calls.exists());
    }
}

#[test]
fn koi_update_through_homebrew_shows_steps_and_brew_output_only_on_failure() {
    for status in [0, 1] {
        let home = tempfile::tempdir().unwrap();
        let github = github("v99.0.0");
        let link = homebrew_binary(home.path(), false);
        let koi = std::fs::canonicalize(&link).unwrap();
        let prefix = std::fs::canonicalize(home.path().join("brew")).unwrap();
        upgrading_brew(&prefix, &koi, "99.0.0", status);
        let output = koi_at(&link, home.path(), &github, &["update"])
            .env("PATH", home.path().join("empty"))
            .output()
            .unwrap();
        if development_version() {
            assert!(output.status.success());
            continue;
        }
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        if status == 0 {
            assert!(output.status.success(), "{stdout}{stderr}");
            assert_eq!(
                stderr,
                "Updating Koidrive with Homebrew\nHomebrew installed Koidrive 99.0.0.\nChecking the background service\nChecked the background service.\n"
            );
            assert!(!stdout.contains("==>"), "{stdout}");
        } else {
            assert!(!output.status.success(), "{stdout}{stderr}");
            assert!(
                stderr.starts_with("Updating Koidrive with Homebrew\n"),
                "{stderr}"
            );
            for said in [
                "==> Upgrading koidrive\n",
                "Error: koidrive did not download\n",
            ] {
                assert!(stderr.contains(said), "{stderr}");
            }
            assert!(
                stderr.contains("brew upgrade failed (exit status 1)"),
                "{stderr}"
            );
        }
    }
}

#[test]
fn an_incomplete_homebrew_update_shows_why_the_tap_did_not_refresh() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let link = homebrew_binary(home.path(), false);
    let prefix = std::fs::canonicalize(home.path().join("brew")).unwrap();
    scripted_brew(&prefix, 1, "  exit 0");
    let output = koi_at(&link, home.path(), &github, &["update"])
        .env("PATH", home.path().join("empty"))
        .output()
        .unwrap();
    if development_version() {
        assert!(output.status.success());
        return;
    }
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.starts_with("Updating Koidrive with Homebrew\n"),
        "{stderr}"
    );
    assert!(
        stderr.contains("fatal: unable to access https://github.com/koidrive/homebrew-tap/: Could not resolve host: github.com\n"),
        "{stderr}"
    );
    assert!(!stderr.contains("Homebrew installed"), "{stderr}");
}

#[test]
fn koi_update_names_the_brew_command_when_homebrew_fails_or_is_missing() {
    for cask in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let github = github("v99.0.0");
        let link = homebrew_binary(home.path(), cask);
        let koi = std::fs::canonicalize(&link).unwrap();
        let prefix = std::fs::canonicalize(home.path().join("brew")).unwrap();
        upgrading_brew(&prefix, &koi, "99.0.0", 1);
        let before = std::fs::read(&koi).unwrap();
        let brew = prefix.join("bin/brew");
        let command = format!(
            "{} {}",
            brew.display(),
            if cask {
                "upgrade --cask koidrive"
            } else {
                "upgrade koidrive"
            }
        );
        let output = koi_at(&link, home.path(), &github, &["update", "--json"])
            .env("PATH", home.path().join("empty"))
            .output()
            .unwrap();
        if development_update(&output, home.path(), &github) {
            continue;
        }
        let envelope = envelope(&output);
        assert!(!output.status.success(), "{envelope:#}");
        assert_eq!(
            envelope["error"]["message"],
            format!("brew upgrade failed (exit status 1). To update, run: {command}")
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("Error: koidrive did not download\n")
        );
        assert_eq!(std::fs::read(&koi).unwrap(), before);
        std::fs::remove_file(&brew).unwrap();
        let output = koi_at(&link, home.path(), &github, &["update"])
            .env("PATH", home.path().join("empty"))
            .output()
            .unwrap();
        assert!(!output.status.success(), "{output:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains(&format!(
                "Homebrew is not at {}. To update, run: {command}",
                brew.display()
            )),
            "{stderr}"
        );
        assert_eq!(std::fs::read(&koi).unwrap(), before);
    }
}

fn scripted_brew(prefix: &Path, git_status: i32, upgrade: &str) -> PathBuf {
    let tap = prefix.join("Library/Taps/koidrive/homebrew-tap");
    std::fs::create_dir_all(tap.join(".git")).unwrap();
    executable(
        &prefix.join("bin/git"),
        &format!(
            "#!/bin/sh\nprintf '%s|%s|%s\\n' \"$*\" \"$GIT_TERMINAL_PROMPT\" \"$GIT_DIR\" >> \"$0.calls\"\n[ {git_status} -ne 0 ] && echo 'fatal: unable to access https://github.com/koidrive/homebrew-tap/: Could not resolve host: github.com' >&2\nexit {git_status}\n"
        ),
    );
    executable(
        &prefix.join("bin/brew"),
        &format!(
            "#!/bin/sh\nprintf '%s|%s\\n' \"$*\" \"$HOMEBREW_ASK\" >> \"$0.calls\"\ncase \"$1\" in\n--repository) echo '{tap}' ;;\nupgrade)\n{upgrade}\n  ;;\nesac\n",
            tap = tap.display(),
        ),
    );
    prefix.join("bin/brew.calls")
}

fn replacing(koi: &Path, version: &str, report: &str) -> String {
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\ncase \"$1\" in\n--version) echo 'koi {version}' ;;\n_reconcile) echo '{report}' ;;\nesac\n"
    )
    .replace('\'', "'\\''");
    format!(
        "  printf '%s' '{script}' > '{koi}.new' && /bin/chmod +x '{koi}.new' && /bin/mv '{koi}.new' '{koi}'",
        koi = koi.display()
    )
}

#[test]
fn koi_update_through_homebrew_is_incomplete_when_the_tap_lags_or_cannot_refresh() {
    for git_status in [0, 1] {
        for json in [true, false] {
            let home = tempfile::tempdir().unwrap();
            let github = github("v99.0.0");
            let link = homebrew_binary(home.path(), false);
            let koi = std::fs::canonicalize(&link).unwrap();
            let prefix = std::fs::canonicalize(home.path().join("brew")).unwrap();
            let calls = scripted_brew(
                &prefix,
                git_status,
                &format!(
                    "{}\n  echo 'Warning: koidrive/tap/koidrive {} already installed' >&2\n  exit 0",
                    replacing(&koi, env!("CARGO_PKG_VERSION"), REPORT),
                    env!("CARGO_PKG_VERSION")
                ),
            );
            let args: &[&str] = if json {
                &["update", "--json"]
            } else {
                &["update"]
            };
            let output = koi_at(&link, home.path(), &github, args)
                .env("PATH", home.path().join("empty"))
                .env("HOMEBREW_ASK", "1")
                .env("GIT_DIR", home.path().join("another-repository"))
                .output()
                .unwrap();
            if development_version() {
                assert!(output.status.success(), "{output:?}");
                return;
            }
            assert!(!output.status.success(), "{output:?}");
            let tap = prefix.join("Library/Taps/koidrive/homebrew-tap");
            let why = match git_status {
                0 => "The koidrive/tap tap does not have Koidrive 99.0.0 yet".to_owned(),
                _ => format!(
                    "Could not refresh the koidrive/tap tap at {} (git exit status 1)",
                    tap.display()
                ),
            };
            let message = format!(
                "Koidrive update is incomplete: the newest release is 99.0.0, and Homebrew has {}. {why}. To update, run: {} upgrade koidrive",
                env!("CARGO_PKG_VERSION"),
                prefix.join("bin/brew").display()
            );
            let stdout = String::from_utf8(output.stdout.clone()).unwrap();
            assert!(!stdout.contains("is the newest release"), "{stdout}");
            if json {
                let envelope = envelope(&output);
                assert_eq!(envelope["outcome"], "error");
                assert_eq!(envelope["error"]["message"], message);
            } else {
                let stderr = String::from_utf8(output.stderr).unwrap();
                assert!(stderr.contains(&message), "{stderr}");
                assert!(
                    stdout.starts_with("Koidrive update is incomplete"),
                    "{stdout}"
                );
            }
            assert_eq!(
                std::fs::read_to_string(&calls).unwrap(),
                "--repository koidrive/tap|1\nupgrade koidrive|\n"
            );
            assert_eq!(
                std::fs::read_to_string(prefix.join("bin/git.calls")).unwrap(),
                format!("-C {} pull --ff-only --quiet|0|\n", tap.display())
            );
        }
    }
}

#[test]
fn koi_update_through_homebrew_is_incomplete_when_the_tap_lags_and_the_service_stopped() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let link = homebrew_binary(home.path(), false);
    let koi = std::fs::canonicalize(&link).unwrap();
    let prefix = std::fs::canonicalize(home.path().join("brew")).unwrap();
    scripted_brew(
        &prefix,
        0,
        &format!(
            "{}\n  exit 0",
            replacing(
                &koi,
                env!("CARGO_PKG_VERSION"),
                &REPORT.replace("running", "stopped")
            )
        ),
    );
    let output = koi_at(&link, home.path(), &github, &["update", "--json"])
        .env("PATH", home.path().join("empty"))
        .output()
        .unwrap();
    if development_update(&output, home.path(), &github) {
        return;
    }
    let envelope = envelope(&output);
    assert!(!output.status.success(), "{envelope:#}");
    assert_eq!(
        envelope["error"]["message"],
        format!(
            "Koidrive update is incomplete: the newest release is 99.0.0, and Homebrew has {}. The koidrive/tap tap does not have Koidrive 99.0.0 yet. To update, run: {} upgrade koidrive",
            env!("CARGO_PKG_VERSION"),
            prefix.join("bin/brew").display()
        )
    );
    assert_eq!(envelope["data"]["service"]["state"], "stopped");
    assert!(!output.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("is the newest release"));
}

#[test]
fn koi_update_names_the_brew_failure_when_brew_removed_koi_before_it_failed() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let link = homebrew_binary(home.path(), false);
    let koi = std::fs::canonicalize(&link).unwrap();
    let prefix = std::fs::canonicalize(home.path().join("brew")).unwrap();
    scripted_brew(
        &prefix,
        0,
        &format!("  /bin/rm '{}'\n  exit 1", koi.display()),
    );
    let output = koi_at(&link, home.path(), &github, &["update", "--json"])
        .env("PATH", home.path().join("empty"))
        .output()
        .unwrap();
    if development_update(&output, home.path(), &github) {
        return;
    }
    let envelope = envelope(&output);
    assert!(!output.status.success(), "{envelope:#}");
    assert_eq!(
        envelope["error"]["message"],
        format!(
            "brew upgrade failed (exit status 1). To update, run: {} upgrade koidrive",
            prefix.join("bin/brew").display()
        )
    );
}

#[test]
fn koi_update_hands_the_service_to_a_koi_brew_upgraded_before_it_failed() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let link = homebrew_binary(home.path(), false);
    let koi = std::fs::canonicalize(&link).unwrap();
    let prefix = std::fs::canonicalize(home.path().join("brew")).unwrap();
    scripted_brew(
        &prefix,
        0,
        &format!(
            "{}\n  echo 'Error: The brew link step did not complete successfully' >&2\n  exit 1",
            replacing(&koi, "99.0.0", REPORT)
        ),
    );
    let output = koi_at(&link, home.path(), &github, &["update", "--json"])
        .env("PATH", home.path().join("empty"))
        .output()
        .unwrap();
    if development_update(&output, home.path(), &github) {
        return;
    }
    let envelope = envelope(&output);
    assert!(!output.status.success(), "{envelope:#}");
    assert_eq!(
        envelope["error"]["message"],
        format!(
            "brew upgrade failed (exit status 1) after installing Koidrive 99.0.0. To update, run: {} upgrade koidrive",
            prefix.join("bin/brew").display()
        )
    );
    assert_eq!(envelope["data"]["to"], "99.0.0");
    assert_eq!(envelope["data"]["result"], "updated");
    assert!(
        std::fs::read_to_string(koi.with_extension("calls"))
            .unwrap()
            .contains("_reconcile --json\n")
    );
}

#[test]
fn koi_update_uses_the_custom_homebrew_of_the_running_koi_without_a_prefix_variable() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let prefix = home.path().join("custombrew");
    let keg = prefix.join("Cellar/koidrive/0.1.0/bin/koi");
    copy_koi(&keg);
    let opt = prefix.join("opt/koidrive");
    std::fs::create_dir_all(opt.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(keg.parent().unwrap().parent().unwrap(), &opt).unwrap();
    let canonical = std::fs::canonicalize(&prefix).unwrap();
    let calls = upgrading_brew(
        &canonical,
        &std::fs::canonicalize(&keg).unwrap(),
        "99.0.0",
        0,
    );
    let output = koi_at(&keg, home.path(), &github, &["update", "--json"])
        .env("PATH", home.path().join("empty"))
        .env_remove("HOMEBREW_PREFIX")
        .output()
        .unwrap();
    if development_update(&output, home.path(), &github) {
        assert!(!calls.exists());
        return;
    }
    let envelope = envelope(&output);
    assert!(output.status.success(), "{envelope:#}");
    assert_eq!(envelope["data"]["via"], "homebrew");
    assert_eq!(envelope["data"]["to"], "99.0.0");
    assert_eq!(
        std::fs::read_to_string(calls).unwrap(),
        "--repository koidrive/tap|1\nupgrade koidrive|1\n"
    );
    assert_eq!(github.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn the_daemon_checks_for_a_release_and_koi_status_names_it() {
    use tokio::io::AsyncBufReadExt;

    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let binary = copy_koi(&home.path().join(".local/bin/koi"));
    let mut daemon = tokio::process::Command::from(koi_at(
        &binary,
        home.path(),
        &github,
        &["_daemon", "--json"],
    ))
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::null())
    .kill_on_drop(true)
    .spawn()
    .unwrap();
    let mut events = tokio::io::BufReader::new(daemon.stdout.take().unwrap()).lines();
    tokio::time::timeout(Duration::from_secs(30), async {
        let started = events.next_line().await.unwrap().unwrap();
        let started: Value = serde_json::from_str(&started).unwrap();
        schema::check(&started);
        assert_eq!(started["event"], "started", "{started}");
        let mut changes = koidrive_ipc::watch(
            Path::new(started["socket"].as_str().unwrap()),
            rustix::process::getuid().as_raw(),
        )
        .await
        .unwrap()
        .unwrap();
        loop {
            let response = changes.next().await.unwrap().unwrap();
            let koidrive_ipc::Response::State { status, .. } = response else {
                panic!("{response:?}");
            };
            if matches!(
                (status.update, development_version()),
                (koidrive_driver::Update::Off, true)
                    | (koidrive_driver::Update::Available { .. }, false)
            ) {
                break;
            }
        }
    })
    .await
    .expect("the daemon starts and publishes its release check");
    let output = koi_at(&binary, home.path(), &github, &["status", "--json"])
        .output()
        .unwrap();
    let status = envelope(&output);
    let text = koi_at(&binary, home.path(), &github, &["status"])
        .output()
        .unwrap();
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
    assert!(output.status.success(), "{status:#}");
    assert_eq!(status["data"]["running"], true, "{status:#}");
    let update = &status["data"]["update"];
    if development_version() {
        assert_eq!(
            update,
            &serde_json::json!({ "state": "off", "version": env!("CARGO_PKG_VERSION") })
        );
        assert!(!String::from_utf8(text.stdout).unwrap().contains("Update:"));
        assert!(github.requests.lock().unwrap().is_empty());
        return;
    }
    assert_eq!(update["state"], "available", "{status:#}");
    assert_eq!(update["latest"], "99.0.0");
    assert_eq!(update["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        update["url"],
        "https://github.com/koidrive/koidrive/releases/tag/v99.0.0"
    );
    let command = format!(
        "{} update",
        std::fs::canonicalize(&binary).unwrap().display()
    );
    assert_eq!(update["command"], command);
    assert!(
        update["checked_at"].as_str().unwrap().ends_with('Z'),
        "{update}"
    );
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(
        text.contains(&format!(
            "Update: Koidrive 99.0.0 is out (this device runs {}). Run: {command}\n",
            env!("CARGO_PKG_VERSION")
        )),
        "{text}"
    );
    let requests = github.requests.lock().unwrap();
    assert_eq!(requests.len(), 1, "one check a day: {requests:?}");
    let mut headers: Vec<String> = (requests[0].lines().skip(1))
        .map(|line| line.split(':').next().unwrap().to_ascii_lowercase())
        .collect();
    headers.sort();
    assert_eq!(
        headers,
        ["accept", "host", "user-agent", "x-github-api-version"]
    );
}

#[test]
fn without_a_daemon_status_reads_the_last_check_and_the_switch_turns_it_off() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let binary = copy_koi(&home.path().join(".local/bin/koi"));
    let state = home.path().join("state/koidrive");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        state.join("update-check.json"),
        serde_json::json!({
            "asset": asset(),
            "checked_at": 1763006400000_i64,
            "etag": "\"e1\"",
            "release": { "version": "99.0.0", "url": "https://example.com/v99.0.0" },
        })
        .to_string(),
    )
    .unwrap();
    let output = koi_at(&binary, home.path(), &github, &["status", "--json"])
        .output()
        .unwrap();
    let update = envelope(&output)["data"]["update"].clone();
    if development_version() {
        assert_eq!(
            update,
            serde_json::json!({ "state": "off", "version": env!("CARGO_PKG_VERSION") })
        );
    } else {
        assert_eq!(update["state"], "available");
        assert_eq!(update["checked_at"], "2025-11-13T04:00:00Z");
    }

    let output = koi_at(&binary, home.path(), &github, &["status", "--json"])
        .env("KOIDRIVE_UPDATE_CHECK", "off")
        .output()
        .unwrap();
    let update = envelope(&output)["data"]["update"].clone();
    assert_eq!(
        update,
        serde_json::json!({ "state": "off", "version": env!("CARGO_PKG_VERSION") })
    );
    assert!(
        github.requests.lock().unwrap().is_empty(),
        "status asks nothing"
    );
}

#[test]
fn cached_status_uses_the_current_homebrew_formula_or_cask_upgrade_command() {
    for cask in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let github = github("v99.0.0");
        let binary = homebrew_binary(home.path(), cask);
        let script = home.path().join(".local/bin/koi");
        executable(
            &script,
            "#!/bin/sh\necho called >> \"$0.calls\"\necho 'koi 100.0.0'\n",
        );
        let alias = home.path().join("status-alias");
        std::os::unix::fs::symlink(&binary, &alias).unwrap();
        let state = home.path().join("state/koidrive");
        std::fs::create_dir_all(&state).unwrap();
        let cached = serde_json::json!({
            "asset": asset(),
            "checked_at": 1763006400000_i64,
            "etag": "\"e1\"",
            "release": { "version": "99.0.0", "url": "https://example.com/v99.0.0" },
        })
        .to_string();
        std::fs::write(state.join("update-check.json"), &cached).unwrap();
        let output = koi_at(&alias, home.path(), &github, &["status", "--json"])
            .env("PATH", home.path().join("brew/bin"))
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let update = envelope(&output)["data"]["update"].clone();
        let output = koi_at(&alias, home.path(), &github, &["status"])
            .env("PATH", home.path().join("brew/bin"))
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let text = String::from_utf8(output.stdout).unwrap();
        if development_version() {
            assert_eq!(
                update,
                serde_json::json!({ "state": "off", "version": env!("CARGO_PKG_VERSION") })
            );
            assert!(!text.contains("Update:"), "{text}");
        } else {
            let command = if cask {
                "brew upgrade --cask koidrive"
            } else {
                "brew upgrade koidrive"
            };
            assert_eq!(
                update,
                serde_json::json!({
                    "state": "available", "version": env!("CARGO_PKG_VERSION"),
                    "latest": "99.0.0", "url": "https://example.com/v99.0.0",
                    "checked_at": "2025-11-13T04:00:00Z", "command": command,
                })
            );
            assert!(
                text.contains(&format!(
                    "Update: Koidrive 99.0.0 is out (this device runs {}). Run: {command}\n",
                    env!("CARGO_PKG_VERSION")
                )),
                "{text}"
            );
            assert!(!text.contains("Run: koi update"), "{text}");
        }
        assert!(github.requests.lock().unwrap().is_empty());
        assert!(!script.with_extension("calls").exists());
        assert!(!home.path().join("brew/bin/brew.calls").exists());
        assert_eq!(
            std::fs::read_to_string(state.join("update-check.json")).unwrap(),
            cached
        );
    }
}

#[test]
fn an_installer_notice_names_its_real_binary_despite_another_koi_on_path() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let binary = copy_koi(&home.path().join("standalone copy's/bin/koi"));
    let alias = home.path().join("installer-alias");
    std::os::unix::fs::symlink(&binary, &alias).unwrap();
    let bin = home.path().join("path/bin");
    let other = bin.join("koi");
    executable(
        &other,
        "#!/bin/sh\necho selected >> \"$0.calls\"\nexit 99\n",
    );
    let state = home.path().join("state/koidrive");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        state.join("update-check.json"),
        serde_json::json!({
            "asset": asset(),
            "checked_at": 1763006400000_i64,
            "release": { "version": "99.0.0", "url": "https://example.com/v99.0.0" },
        })
        .to_string(),
    )
    .unwrap();
    let status = koi_at(&alias, home.path(), &github, &["status", "--json"])
        .env("PATH", &bin)
        .output()
        .unwrap();
    let update = envelope(&status)["data"]["update"].clone();
    if development_version() {
        assert_eq!(update["state"], "off");
        assert!(update.get("command").is_none());
        return;
    }
    let resolved = std::fs::canonicalize(&binary).unwrap();
    let expected = format!(
        "'{}' update",
        resolved.to_str().unwrap().replace('\'', "'\\''")
    );
    assert_eq!(update["command"], expected);
    let status = koi_at(&alias, home.path(), &github, &["status"])
        .env("PATH", &bin)
        .output()
        .unwrap();
    assert!(
        String::from_utf8(status.stdout)
            .unwrap()
            .contains(&format!("Run: {expected}\n"))
    );
    let output = koi_at(
        Path::new("/bin/sh"),
        home.path(),
        &github,
        &["-c", &expected],
    )
    .env("PATH", &bin)
    .env("KOIDRIVE_INSTALL_SCRIPT_URL", "not a URL")
    .output()
    .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("not a URL")
    );
    assert!(!other.with_extension("calls").exists());
    assert_eq!(std::fs::read_link(alias).unwrap(), binary);
}

#[test]
fn homebrew_guidance_targets_its_owner_when_path_selects_another_brew() {
    for cask in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("owning copy's home");
        let github = github("v99.0.0");
        let binary = homebrew_binary(&root, cask);
        let helper_before = std::fs::read(&binary).unwrap();
        let link_before = std::fs::read_link(&binary).unwrap();
        let owner = std::fs::canonicalize(root.join("brew/bin/brew")).unwrap();
        executable(&owner, "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\n");
        let other_bin = home.path().join("other/bin");
        let other = other_bin.join("brew");
        executable(
            &other,
            "#!/bin/sh\necho selected >> \"$0.calls\"\nexit 99\n",
        );
        let run = |args: &[&str]| {
            let mut command = koi_at(&binary, &root, &github, args);
            command
                .env("PATH", &other_bin)
                .env("XDG_RUNTIME_DIR", home.path().join("run"));
            command
        };
        if development_version() {
            continue;
        }
        let args = if cask {
            vec!["upgrade", "--cask", "koidrive"]
        } else {
            vec!["upgrade", "koidrive"]
        };
        let mut argv = vec![owner.to_str().unwrap()];
        argv.extend(args.iter().copied());
        let command = format!(
            "'{}' {}",
            owner.to_str().unwrap().replace('\'', "'\\''"),
            args.join(" ")
        );
        let state = root.join("state/koidrive");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(
            state.join("update-check.json"),
            serde_json::json!({
                "asset": asset(), "checked_at": 1763006400000_i64,
                "release": { "version": "99.0.0", "url": "https://example.com/v99.0.0" },
            })
            .to_string(),
        )
        .unwrap();
        let status = run(&["status", "--json"]).output().unwrap();
        assert!(
            status.status.success(),
            "{}",
            String::from_utf8_lossy(&status.stdout)
        );
        assert_eq!(envelope(&status)["data"]["update"]["command"], command);
        let status = run(&["status"]).output().unwrap();
        assert!(
            String::from_utf8(status.stdout)
                .unwrap()
                .contains(&format!("Run: {command}\n"))
        );
        assert!(!owner.with_extension("calls").exists());
        assert!(!other.with_extension("calls").exists());
        assert!(github.requests.lock().unwrap().is_empty());
        std::fs::remove_file(&other).unwrap();
        std::os::unix::fs::symlink(&owner, &other).unwrap();
        let status = run(&["status", "--json"]).output().unwrap();
        assert_eq!(envelope(&status)["data"]["update"]["command"], command);
        let status = Command::new(argv[0])
            .args(&argv[1..])
            .env("PATH", &other_bin)
            .status()
            .unwrap();
        assert!(status.success());
        let output = Command::new("/bin/sh")
            .args(["-c", &command])
            .env("PATH", &other_bin)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            std::fs::read_to_string(owner.with_extension("calls")).unwrap(),
            format!("{}\n", args.join(" ")).repeat(2)
        );
        assert!(!other.with_extension("calls").exists());
        assert_eq!(std::fs::read(&binary).unwrap(), helper_before);
        assert_eq!(std::fs::read_link(&binary).unwrap(), link_before);
        assert!(!update_log(&root).exists());
    }
}

#[test]
fn a_relocated_formula_is_found_from_the_process_invocation() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let link = homebrew_binary(home.path(), false);
    let prefix = home.path().join("brew");
    let version = prefix.join("Cellar/koidrive/0.1.0");
    let external = home.path().join("external/koidrive/0.1.0");
    std::fs::create_dir_all(external.parent().unwrap()).unwrap();
    std::fs::rename(&version, &external).unwrap();
    std::fs::write(external.join("INSTALL_RECEIPT.json"), "{}").unwrap();
    let opt = prefix.join("opt/koidrive");
    std::fs::remove_file(&opt).unwrap();
    std::os::unix::fs::symlink(&external, &opt).unwrap();
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink("../opt/koidrive/bin/koi", &link).unwrap();
    let alias = home.path().join("alias");
    std::os::unix::fs::symlink(&link, &alias).unwrap();
    let before = std::fs::read(&alias).unwrap();
    let calls = upgrading_brew(
        &std::fs::canonicalize(&prefix).unwrap(),
        &std::fs::canonicalize(&alias).unwrap(),
        "99.0.0",
        1,
    );
    for invoked in [alias.as_path(), Path::new("./alias"), Path::new("koi")] {
        let path = if invoked == Path::new("koi") {
            prefix.join("bin")
        } else {
            home.path().join("no-tools")
        };
        let output = koi_at(invoked, home.path(), &github, &["update", "--json"])
            .current_dir(home.path())
            .env_remove("HOMEBREW_PREFIX")
            .env("PATH", &path)
            .output()
            .unwrap();
        if development_update(&output, home.path(), &github) {
            continue;
        }
        let response = envelope(&output);
        assert!(!output.status.success(), "{response:#}");
        let owner = std::fs::canonicalize(prefix.join("bin"))
            .unwrap()
            .join("brew");
        let brew = if invoked == Path::new("koi") {
            "brew"
        } else {
            owner.to_str().unwrap()
        };
        assert_eq!(
            response["error"]["message"],
            format!("brew upgrade failed (exit status 1). To update, run: {brew} upgrade koidrive")
        );
    }
    assert_eq!(std::fs::read(&alias).unwrap(), before);
    assert_eq!(std::fs::read_link(&alias).unwrap(), link);
    match development_version() {
        true => assert!(!calls.exists()),
        false => assert_eq!(
            std::fs::read_to_string(&calls).unwrap(),
            "--repository koidrive/tap|1\nupgrade koidrive|1\n".repeat(3)
        ),
    }
    assert_eq!(
        github.requests.lock().unwrap().len(),
        if development_version() { 0 } else { 3 }
    );
}

#[test]
fn a_formula_receipt_with_no_discoverable_prefix_never_self_replaces() {
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let keg = home.path().join("external/package/0.1.0");
    let binary = copy_koi(&keg.join("bin/koi"));
    std::fs::write(keg.join("INSTALL_RECEIPT.json"), "{}").unwrap();
    let alias = home.path().join("direct-store-alias");
    std::os::unix::fs::symlink(&binary, &alias).unwrap();
    let before = std::fs::read(&binary).unwrap();
    for invoked in [&binary, &alias] {
        let output = koi_at(invoked, home.path(), &github, &["update", "--json"])
            .env_remove("HOMEBREW_PREFIX")
            .env("PATH", home.path().join("no-tools"))
            .env("KOIDRIVE_UPDATE_CHECK_URL", "not-a-URL")
            .output()
            .unwrap();
        if development_update(&output, home.path(), &github) {
            continue;
        }
        let response = envelope(&output);
        assert!(!output.status.success(), "{response:#}");
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Homebrew"),
            "{response:#}"
        );
    }
    assert_eq!(std::fs::read(&binary).unwrap(), before);
    assert!(!update_log(home.path()).exists());
    assert!(github.requests.lock().unwrap().is_empty());
}

#[test]
fn an_unverified_invocation_never_enters_installer_replacement() {
    for relative in [
        "standalone/koi",
        "Applications/Koidrive.app/Contents/Helpers/koi",
        "Custom Apps/Koidrive.app/Contents/Helpers/koi",
    ] {
        let home = tempfile::tempdir().unwrap();
        let github = github("v99.0.0");
        let binary = copy_koi(&home.path().join(relative));
        let before = std::fs::read(&binary).unwrap();
        let output = koi_at(&binary, home.path(), &github, &["update", "--json"])
            .arg0("missing-invocation")
            .env("PATH", home.path().join("no-tools"))
            .env("KOIDRIVE_UPDATE_CHECK_URL", "not-a-URL")
            .output()
            .unwrap();
        if !development_update(&output, home.path(), &github) {
            let response = envelope(&output);
            assert!(!output.status.success(), "{response:#}");
            assert!(
                response["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("run its full path"),
                "{response:#}"
            );
        }
        assert_eq!(std::fs::read(&binary).unwrap(), before);
        assert!(!update_log(home.path()).exists());
        assert!(github.requests.lock().unwrap().is_empty());
    }
}

fn adversarial_brew(prefix: &Path, git_status: i32, brew_upgrade: &str) -> PathBuf {
    let tap = prefix.join("Library/Taps/koidrive/homebrew-tap");
    std::fs::create_dir_all(tap.join(".git")).unwrap();
    executable(
        &prefix.join("bin/git"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\n[ {git_status} -ne 0 ] && echo 'fatal: unable to access https://github.com/koidrive/homebrew-tap/: Could not resolve host: github.com' >&2\nexit {git_status}\n"
        ),
    );
    executable(
        &prefix.join("bin/brew"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\ncase \"$1\" in\n--repository) echo '{tap}' ;;\nupgrade)\n{brew_upgrade}\n  ;;\nesac\n",
            tap = tap.display(),
        ),
    );
    prefix.join("bin/brew.calls")
}

#[test]
fn adversarial_a_failed_tap_refresh_is_never_reported_as_the_newest_release() {
    if development_version() {
        return;
    }
    let mut claims = Vec::new();
    for json in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let github = github("v99.0.0");
        let link = homebrew_binary(home.path(), false);
        let koi = std::fs::canonicalize(&link).unwrap();
        let prefix = std::fs::canonicalize(home.path().join("brew")).unwrap();
        let same = format!(
            "#!/bin/sh\ncase \"$1\" in\n--version) echo 'koi {}' ;;\n_reconcile) echo '{REPORT}' ;;\nesac\n",
            env!("CARGO_PKG_VERSION")
        )
        .replace('\'', "'\\''");
        adversarial_brew(
            &prefix,
            1,
            &format!(
                "  printf '%s' '{same}' > '{koi}.new' && /bin/chmod +x '{koi}.new' && /bin/mv '{koi}.new' '{koi}'\n  echo 'Warning: koidrive/tap/koidrive {} already installed' >&2\n  exit 0",
                env!("CARGO_PKG_VERSION"),
                koi = koi.display()
            ),
        );
        let args: &[&str] = if json {
            &["update", "--json"]
        } else {
            &["update"]
        };
        let output = koi_at(&link, home.path(), &github, args)
            .env("PATH", home.path().join("empty"))
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        eprintln!(
            "json={json} exit: {:?}\n--- stdout\n{stdout}--- stderr\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        if output.status.success()
            && (stdout.contains("is the newest release")
                || stdout.contains("\"outcome\": \"unchanged\""))
        {
            claims.push(json);
        }
    }
    assert!(
        claims.is_empty(),
        "koi update succeeds as up to date after the tap refresh failed, while v99.0.0 is published (json modes: {claims:?})"
    );
}

#[test]
fn adversarial_b_brew_failing_after_it_upgraded_koi_still_moves_the_service() {
    if development_version() {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let github = github("v99.0.0");
    let link = homebrew_binary(home.path(), false);
    let koi = std::fs::canonicalize(&link).unwrap();
    let prefix = std::fs::canonicalize(home.path().join("brew")).unwrap();
    let upgraded = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$0.calls\"\ncase \"$1\" in\n--version) echo 'koi 99.0.0' ;;\n_reconcile) echo '{REPORT}' ;;\nesac\n"
    )
    .replace('\'', "'\\''");
    adversarial_brew(
        &prefix,
        0,
        &format!(
            "  printf '%s' '{upgraded}' > '{koi}.new' && /bin/chmod +x '{koi}.new' && /bin/mv '{koi}.new' '{koi}'\n  echo 'Error: The brew link step did not complete successfully' >&2\n  exit 1",
            koi = koi.display()
        ),
    );
    let output = koi_at(&link, home.path(), &github, &["update"])
        .env("PATH", home.path().join("empty"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!(
        "exit: {:?}\n--- stdout\n{stdout}--- stderr\n{stderr}",
        output.status.code()
    );
    let version = Command::new(&koi).arg("--version").output().unwrap();
    eprintln!(
        "installed koi now: {}",
        String::from_utf8_lossy(&version.stdout)
    );
    let calls = std::fs::read_to_string(koi.with_extension("calls")).unwrap_or_default();
    assert!(
        calls.contains("_reconcile"),
        "koi 99.0.0 is installed but the service was never handed to it; koi calls: {calls:?}"
    );
}
