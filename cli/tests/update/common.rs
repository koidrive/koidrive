// SPDX-License-Identifier: Apache-2.0

use std::io::{BufRead, BufReader, Write};

use std::net::TcpListener;

use std::os::unix::process::CommandExt;

use std::path::{Path, PathBuf};

use std::process::{Command, Output};

use std::sync::{Arc, Mutex};

use std::time::{Duration, Instant};

use serde_json::Value;

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

fn on_a_terminal(mut command: Command, typed: Option<(Duration, &[u8])>) -> String {
    let mut terminal = Terminal::open(100, 32);
    command
        .stdin(terminal.slave())
        .stdout(terminal.slave())
        .stderr(terminal.slave());
    unsafe {
        command.pre_exec(|| {
            rustix::process::setsid()?;
            rustix::process::ioctl_tiocsctty(std::io::stdin())?;
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    if let Some((after, bytes)) = typed {
        let typing = Instant::now() + after;
        while Instant::now() < typing && terminal.read(&mut child, deadline) {}
        terminal.write(bytes);
    }
    while terminal.read(&mut child, deadline) {}
    terminal.text()
}

const STOPPED: &str = r#"{"schema_version":1,"command":"_reconcile","outcome":"ok","data":{"service":{"manager":"launchd","name":"dev.koidrive.koi","state":"stopped"}}}"#;

const APP_FAILED: &str = r#"{"schema_version":1,"command":"_reconcile","outcome":"ok","data":{"service":{"manager":"launchd","name":"dev.koidrive.koi","state":"running"},"app":{"state":"failed","problem":"Koidrive.app did not start"}}}"#;

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
