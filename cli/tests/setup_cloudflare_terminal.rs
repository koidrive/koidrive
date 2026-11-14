// SPDX-License-Identifier: Apache-2.0

#![cfg(unix)]

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use koidrive_cloudflare::SCOPES;
use koidrive_driver::{Credential, Driver, FolderState, FolderStatus, Paths};
use koidrive_proto::v1::{
    AccessToken, CreateSpaceRequest, DeviceCredentials, ServerInfo, SpaceInfo, SpaceList,
};
use prost::Message;
use serde_json::{Value, json};
use url::Url;

const ROUTE: &str = "How do you want to connect?";
const SPACES: &str = "Which shared folder should sync here?";
const SERVERS: &str = "Which server should this device use?";
const CONSENT: &str = "Send anonymous usage reports?";
const ACCOUNT: &str = "0123456789abcdef";
const DEVICE: &str = "0190a0b0-0000-7000-8000-000000000001";

#[derive(Clone, Copy)]
enum Browser {
    Opens,
    Missing,
    Fails,
}

#[derive(Default)]
struct State {
    scripts: Vec<String>,
    spaces: Vec<SpaceInfo>,
    created: Vec<SpaceInfo>,
    buckets: Vec<String>,
    linked: Vec<FolderStatus>,
    links: Vec<(PathBuf, String)>,
    requests: Vec<String>,
    paused: bool,
    folder_state: Option<FolderState>,
    denied: bool,
    r2_disabled: usize,
    base: String,
    collide_create: bool,
}

struct Home {
    dir: tempfile::TempDir,
    base: Url,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = rustix::process::kill_process_group(
                rustix::process::Pid::from_child(&self.0),
                rustix::process::Signal::KILL,
            );
            let _ = self.0.wait();
        }
    }
}

impl Home {
    fn new(scripts: &[&str], spaces: &[&str]) -> Self {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let path = dir.path();
        let state_dir = path.join("state/koidrive");
        fs::create_dir_all(&state_dir).unwrap();
        fs::set_permissions(&state_dir, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir_all(path.join("config/koidrive")).unwrap();
        fs::create_dir_all(path.join("bin")).unwrap();
        fs::write(
            path.join("config/koidrive/config.toml"),
            "[service]\nbackground = true\n",
        )
        .unwrap();
        bundle(&path.join("bundle"));
        executable(
            &path.join("bin/launchctl"),
            r#"printf '%s\n' "$*" >> "$HOME/service-calls"
marker="$HOME/state/koidrive/service-running"
case "$1:$2" in
  print:gui/*/*) [ -f "$marker" ] || exit 1; printf 'state = running\n' ;;
  print:gui/*|print-disabled:gui/*) ;;
  bootstrap:*|kickstart:*) : > "$marker" ;;
  bootout:*) /bin/rm -f "$marker" ;;
  *) exit 88 ;;
esac
"#,
        );
        executable(
            &path.join("bin/systemctl"),
            r#"printf '%s\n' "$*" >> "$HOME/service-calls"
marker="$HOME/state/koidrive/service-running"
[ "$1" = --user ] || exit 88
case "$2" in
  show-environment|enable|daemon-reload) ;;
  is-active) [ -f "$marker" ] || exit 1 ;;
  start|restart) : > "$marker" ;;
  cat) exit 1 ;;
  disable) /bin/rm -f "$marker" ;;
  *) exit 88 ;;
esac
"#,
        );
        executable(
            &path.join("bin/loginctl"),
            "[ \"$1\" = show-user ] || exit 88\nprintf 'yes\\n'\n",
        );
        let state = Arc::new(Mutex::new(State {
            scripts: scripts.iter().map(|name| (*name).into()).collect(),
            spaces: spaces
                .iter()
                .enumerate()
                .map(|(index, name)| space(index, name))
                .collect(),
            ..State::default()
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let http = TcpListener::bind("127.0.0.1:0").unwrap();
        http.set_nonblocking(true).unwrap();
        let base = Url::parse(&format!("http://{}/", http.local_addr().unwrap())).unwrap();
        let socket = UnixListener::bind(state_dir.join("daemon.sock")).unwrap();
        socket.set_nonblocking(true).unwrap();
        let mut home = Self {
            dir,
            base,
            state,
            stop,
            threads: Vec::new(),
        };
        home.state.lock().unwrap().base = home.base.to_string();
        let (state, stop) = (home.state.clone(), home.stop.clone());
        home.threads.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match http.accept() {
                    Ok((stream, _)) => serve(stream, &state),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("fake HTTP accept: {error}"),
                }
            }
        }));
        let (state, stop, paths) = (home.state.clone(), home.stop.clone(), home.paths());
        home.threads.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match socket.accept() {
                    Ok((stream, _)) => daemon(stream, &paths, &state),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("fake daemon accept: {error}"),
                }
            }
        }));
        home
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn paths(&self) -> Paths {
        Paths::from_vars(|name| match name {
            "HOME" => Some(self.path().into()),
            "XDG_CONFIG_HOME" => Some(self.path().join("config").into()),
            "XDG_STATE_HOME" => Some(self.path().join("state").into()),
            _ => None,
        })
        .unwrap()
    }

    fn real_driver(&self) {
        fs::remove_file(self.paths().socket()).unwrap();
        fs::write(
            self.path().join("config/koidrive/config.toml"),
            "[service]\nbackground = false\n",
        )
        .unwrap();
        let mut credential: Credential = serde_json::from_value(json!({
            "server": "http://127.0.0.1/", "user_id": "owner", "device_id": DEVICE,
            "device_name": "Laptop", "refresh_credential": "synthetic-refresh"
        }))
        .unwrap();
        credential.server = self.base.as_str().trim_end_matches('/').to_owned();
        credential.save_new(&self.paths().credential()).unwrap();
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_koi"));
        command
            .current_dir(self.path())
            .env("HOME", self.path())
            .env("XDG_CONFIG_HOME", self.path().join("config"))
            .env("XDG_STATE_HOME", self.path().join("state"))
            .env("PATH", self.path().join("bin"))
            .env("TERM", "xterm-256color")
            .env("CLICOLOR_FORCE", "1")
            .env("NO_PROXY", "*")
            .env("KOIDRIVE_TEST_SETUP_URL", self.base.as_str())
            .env("KOIDRIVE_TEST_SETUP_BUNDLE", self.path().join("bundle"))
            .env(
                "KOIDRIVE_TELEMETRY_URL",
                self.base.join("telemetry").unwrap().as_str(),
            );
        for name in [
            "CI",
            "NO_COLOR",
            "CLICOLOR",
            "KOI_PLAIN",
            "DO_NOT_TRACK",
            "KOIDRIVE_TELEMETRY",
            "XDG_RUNTIME_DIR",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            command.env_remove(name);
        }
        command
    }

    fn run(&self, rich: bool, browser: Browser, choices: &[(&str, usize)]) -> String {
        let steps: Vec<(&str, String)> = choices
            .iter()
            .map(|(question, index)| {
                let answer = if *question == CONSENT {
                    if rich { "n".into() } else { "n\n".into() }
                } else {
                    pick(rich, *index)
                };
                (*question, answer)
            })
            .collect();
        self.run_steps(rich, browser, &steps)
    }

    fn run_steps(&self, rich: bool, browser: Browser, steps: &[(&str, String)]) -> String {
        let page = self.path().join("browser-url");
        if page.exists() {
            fs::remove_file(page).unwrap();
        }
        if !matches!(browser, Browser::Missing) {
            executable(
                &self.path().join(if cfg!(target_os = "macos") {
                    "bin/open"
                } else {
                    "bin/xdg-open"
                }),
                &format!(
                    "printf '%s\\n' \"$1\" > \"$HOME/browser-url\"\nexit {}\n",
                    if matches!(browser, Browser::Fails) {
                        17
                    } else {
                        0
                    }
                ),
            );
        }
        let mut command = self.command();
        command.args(["setup", "--interactive", "--no-app", "--name", "Laptop"]);
        if !rich {
            command.arg("--plain");
        }
        self.finish_run(rich, command, steps)
    }

    fn finish_run(&self, rich: bool, command: Command, steps: &[(&str, String)]) -> String {
        let output = terminal(command, steps, Some(self.path()));
        let raw = String::from_utf8(output.stdout).unwrap();
        fs::write(self.path().join("terminal.ansi"), &raw).unwrap();
        let text = console::strip_ansi_codes(&raw).into_owned();
        assert!(output.status.success(), "{text}");
        assert_eq!(raw.contains("\u{1b}["), rich, "{raw}");
        let colors = raw.split("\u{1b}[").skip(1).any(|part| {
            part.find('m').is_some_and(|end| {
                part[..end]
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || byte == b';')
            })
        });
        assert_eq!(colors, rich, "{raw}");
        if !rich {
            assert!(!raw.contains('\u{1b}'), "{raw}");
        }
        text
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let mut failed = false;
        for thread in self.threads.drain(..) {
            failed |= thread.join().is_err();
        }
        assert!(
            !failed || std::thread::panicking(),
            "a fixture thread failed"
        );
    }
}

fn executable(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn bundle(root: &Path) {
    fs::create_dir_all(root.join("worker")).unwrap();
    fs::create_dir_all(root.join("assets")).unwrap();
    fs::write(root.join("worker/worker.js"), "export default {};\n").unwrap();
    fs::write(root.join("assets/index.html"), "test bundle\n").unwrap();
    fs::write(root.join("manifest.json"), json!({
        "version": env!("CARGO_PKG_VERSION"), "main_module": "worker.js",
        "compatibility_date": "2026-11-01", "default_bucket": "koidrive", "r2_binding": "BUCKET",
        "durable_objects": [{"name":"ACCOUNT","class_name":"AccountDO"},{"name":"SPACE","class_name":"SpaceDO"}],
        "migrations": [{"tag":"v1","new_sqlite_classes":["AccountDO","SpaceDO"]}],
        "assets": {"binding":"ASSETS"}
    }).to_string()).unwrap();
}

fn space(index: usize, name: &str) -> SpaceInfo {
    SpaceInfo {
        space_id: format!("0190a0b0-0000-7000-8000-0000000{:05}", index + 10),
        name: name.into(),
        format_version: 1,
        ..Default::default()
    }
}

fn serve(mut stream: TcpStream, state: &Mutex<State>) {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut first = String::new();
    reader.read_line(&mut first).unwrap();
    if first.is_empty() {
        return;
    }
    let mut length = 0;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).unwrap();
        if header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap();
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    let mut words = first.split_whitespace();
    let (method, target) = (words.next().unwrap(), words.next().unwrap());
    let path = target.split('?').next().unwrap();
    let mut state = state.lock().unwrap();
    state.requests.push(format!("{method} {path}"));
    let cf = |result: Value| {
        (
            "application/json",
            json!({"success":true,"errors":[],"result":result})
                .to_string()
                .into_bytes(),
        )
    };
    let proto = |bytes| ("application/x-protobuf", bytes);
    let server_path = path.find("/v1/").map(|start| &path[start..]);
    let mut status = "200 OK";
    let (kind, answer) = match (method, path, server_path) {
        ("POST", "/oauth2/token", _) => ("application/json", json!({
            "access_token":"synthetic-access", "refresh_token":"synthetic-refresh", "expires_in":3600, "scope":SCOPES.join(" ")
        }).to_string().into_bytes()),
        ("POST", "/oauth2/revoke" | "/telemetry", _) => ("application/json", b"{}".to_vec()),
        (_, _, Some("/v1/server")) => proto(ServerInfo {
            server_version: env!("CARGO_PKG_VERSION").into(), proto_min: 1, proto_max: 1,
            data_path: 1, claimed: true, setup_attempts: true,
        }.encode_to_vec()),
        ("POST", _, Some("/v1/claim")) => proto(DeviceCredentials {
            user_id: "owner".into(), device_id: DEVICE.into(),
            refresh_credential: "synthetic-refresh-credential".into(), user_name: "Owner".into(),
            ..Default::default()
        }.encode_to_vec()),
        ("POST", _, Some("/v1/token")) => proto(AccessToken { token:"synthetic-token".into(), expires_at_ms:i64::MAX, ..Default::default() }.encode_to_vec()),
        ("GET", _, Some("/v1/spaces")) => proto(SpaceList { spaces:state.spaces.clone(), ..Default::default() }.encode_to_vec()),
        ("POST", _, Some("/v1/spaces")) => {
            let request = CreateSpaceRequest::decode(body.as_slice()).unwrap();
            let created = space(state.spaces.len(), &request.name);
            state.spaces.push(created.clone());
            if std::mem::take(&mut state.collide_create) {
                status = "409 Conflict";
                proto(koidrive_proto::v1::Error {
                    code: koidrive_proto::v1::error::Code::InvalidArgument.into(),
                    message: "shared folder already exists".into(),
                    ..Default::default()
                }.encode_to_vec())
            } else {
                state.created.push(created.clone());
                proto(created.encode_to_vec())
            }
        },
        ("GET", "/client/v4/accounts", _) => cf(json!([{"id":ACCOUNT,"name":"Personal"}])),
        _ => {
            let suffix = path.strip_prefix(&format!("/client/v4/accounts/{ACCOUNT}")).unwrap_or("");
            match (method, suffix) {
                ("GET", "/r2/buckets") if state.r2_disabled > 0 => {
                    state.r2_disabled -= 1;
                    status = "403 Forbidden";
                    ("application/json", json!({"success":false,"errors":[{"code":10042,"message":"R2 disabled"}],"result":null}).to_string().into_bytes())
                }
                ("GET", "/r2/buckets") => cf(json!({"buckets":[]})),
                ("POST", "/r2/buckets") => {
                    let request: Value = serde_json::from_slice(&body).unwrap();
                    state.buckets.push(request["name"].as_str().unwrap().into());
                    cf(json!({}))
                }
                ("GET", "/workers/scripts") => cf(json!(state.scripts.iter().map(|name| json!({"id":name,"tags":["koidrive-server"],"has_assets":true})).collect::<Vec<_>>())),
                ("GET", "/workers/subdomain") => cf(json!({"subdomain":"synthetic"})),
                ("GET", _) if suffix.ends_with("/settings") => cf(json!({"bindings":[{"type":"r2_bucket","name":"BUCKET","bucket_name":"synthetic-bucket"}]})),
                ("POST", _) if suffix.ends_with("/assets-upload-session") => cf(json!({"jwt":"synthetic-upload-token","buckets":[]})),
                ("GET", _) if suffix.ends_with("/secrets") => cf(json!([])),
                ("PUT", _) if suffix.ends_with("/secrets") => cf(json!({})),
                ("DELETE", _) if suffix.contains("/secrets/") => cf(json!({})),
                ("POST", _) if suffix.ends_with("/subdomain") => cf(json!({})),
                ("PUT", _) if suffix.starts_with("/workers/scripts/") => cf(json!({"id":suffix.rsplit('/').next().unwrap(),"tags":["koidrive-server"]})),
                _ => panic!("unexpected HTTP request: {method} {target}"),
            }
        }
    };
    write!(stream, "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", answer.len()).unwrap();
    stream.write_all(&answer).unwrap();
}

fn daemon(mut stream: UnixStream, paths: &Paths, state: &Mutex<State>) {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut line = String::new();
    BufReader::new(stream.try_clone().unwrap())
        .read_line(&mut line)
        .unwrap();
    if line.is_empty() {
        return;
    }
    let request: Value = serde_json::from_str(&line).unwrap();
    let mut state = state.lock().unwrap();
    let response = match request["request"].as_str().unwrap() {
        "save_credential" => {
            let credential: Credential =
                serde_json::from_value(request["credential"].clone()).unwrap();
            credential.save_new(&paths.credential()).unwrap();
            json!({"response":"paired","account":credential.account()})
        }
        "pair" => {
            let mut credential: Credential = serde_json::from_value(json!({
                "server": "http://127.0.0.1/", "user_id": "owner", "device_id": DEVICE,
                "device_name": "Laptop", "refresh_credential": "secret"
            }))
            .unwrap();
            credential.server = state.base.trim_end_matches('/').to_owned();
            credential.save_new(&paths.credential()).unwrap();
            json!({"response":"paired","account":credential.account()})
        }
        "status" => {
            let folders: Vec<_> = state
                .linked
                .iter()
                .cloned()
                .map(|mut folder| {
                    if let Some(blocked) = state.folder_state {
                        folder.state = blocked;
                    }
                    folder
                })
                .collect();
            json!({"response":"status","status":{
                "account":null,"running":true,"folders":folders,"paused":state.paused,"version":env!("CARGO_PKG_VERSION")
            }})
        }
        "questions" => json!({"response":"questions","questions":[]}),
        "access" => {
            let blocked: Vec<_> = state
                .linked
                .iter()
                .filter(|_| state.denied)
                .map(|folder| json!({"folder":folder.path,"waiting":false}))
                .collect();
            json!({"response":"access","blocked":blocked})
        }
        "link_approved" => {
            let folder = PathBuf::from(request["folder"].as_str().unwrap());
            let id = request["space"].as_str().unwrap().to_owned();
            let space = state
                .spaces
                .iter()
                .find(|space| space.space_id == id)
                .unwrap();
            let status: FolderStatus = serde_json::from_value(json!({
                "space_id": id,
                "space_name": space.name,
                "path": folder.display().to_string(),
                "state": FolderState::Connecting,
                "history_id": null,
                "last_seq": null,
                "head_seq": null,
                "remote_nodes": 0,
                "local_entries": null,
                "questions": 0,
                "problem": null,
                "wait": null
            }))
            .unwrap();
            state.links.push((folder, id));
            state.linked.push(status.clone());
            json!({"response":"linked","folder":status})
        }
        other => panic!("unexpected IPC request: {other}"),
    };
    writeln!(stream, "{response}").unwrap();
}

fn callback(page: &str) {
    let page = Url::parse(page).unwrap();
    assert_eq!(page.host_str(), Some("127.0.0.1"));
    let pairs: std::collections::HashMap<_, _> = page.query_pairs().into_owned().collect();
    let mut redirect = Url::parse(&pairs["redirect_uri"]).unwrap();
    assert_eq!(redirect.host_str(), Some("127.0.0.1"));
    redirect
        .query_pairs_mut()
        .append_pair("state", &pairs["state"])
        .append_pair("code", "synthetic-code");
    let mut stream = TcpStream::connect(("127.0.0.1", redirect.port().unwrap())).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET {}?{} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        redirect.path(),
        redirect.query().unwrap()
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
}

fn terminal(mut command: Command, steps: &[(&str, String)], home: Option<&Path>) -> Output {
    let master = rustix::pty::openpt(rustix::pty::OpenptFlags::RDWR).unwrap();
    rustix::pty::grantpt(&master).unwrap();
    rustix::pty::unlockpt(&master).unwrap();
    let name = rustix::pty::ptsname(&master, Vec::new()).unwrap();
    let slave = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(name.to_str().unwrap())
        .unwrap();
    rustix::termios::tcsetwinsize(
        &master,
        rustix::termios::Winsize {
            ws_row: 30,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    )
    .unwrap();
    let flags = rustix::fs::fcntl_getfl(&master).unwrap();
    rustix::fs::fcntl_setfl(&master, flags | rustix::fs::OFlags::NONBLOCK).unwrap();
    let mut master = File::from(master);
    command
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave);
    let mut child = Running(command.process_group(0).spawn().unwrap());
    let mut raw = Vec::new();
    let mut answered = 0;
    let mut seen = 0;
    let mut called_back = false;
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        let mut bytes = [0; 8192];
        match master.read(&mut bytes) {
            Ok(0) => break,
            Ok(count) => raw.extend_from_slice(&bytes[..count]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if child.0.try_wait().unwrap().is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) if error.raw_os_error() == Some(5) => break,
            Err(error) => panic!("PTY read: {error}"),
        }
        let raw_text = String::from_utf8_lossy(&raw);
        let text = console::strip_ansi_codes(&raw_text);
        if !called_back && let Some(home) = home {
            let page = fs::read_to_string(home.join("browser-url"))
                .ok()
                .or_else(|| {
                    text.split_whitespace()
                        .find(|word| {
                            word.starts_with("http://127.0.0.1:") && word.contains("/oauth2/auth?")
                        })
                        .map(str::to_owned)
                });
            if let Some(page) = page.filter(|page| !page.trim().is_empty()) {
                callback(page.trim());
                called_back = true;
            }
        }
        if let Some((question, answer)) = steps.get(answered)
            && let Some(at) = text.get(seen..).and_then(|text| text.find(question))
        {
            seen += at + question.len();
            std::thread::sleep(Duration::from_millis(50));
            master.write_all(answer.as_bytes()).unwrap();
            answered += 1;
        }
        if Instant::now() > deadline {
            panic!("setup timed out after answering {answered} prompts: {text}");
        }
    }
    let status = child.0.wait().unwrap();
    assert_eq!(answered, steps.len(), "{}", String::from_utf8_lossy(&raw));
    Output {
        status,
        stdout: raw,
        stderr: Vec::new(),
    }
}

fn rendered(raw: &[u8]) -> String {
    let mut terminal = vt100::Parser::new(
        30_u16.try_into().unwrap(),
        100_u16.try_into().unwrap(),
        1000,
    );
    terminal.process(raw);
    let mut rendered = String::new();
    for offset in (1..=terminal.screen().scrollback_len()).rev() {
        terminal.screen_mut().set_scrollback(offset);
        rendered.push_str(&terminal.screen().rows(0, 100).next().unwrap());
        rendered.push('\n');
    }
    terminal.screen_mut().set_scrollback(0);
    rendered.push_str(&terminal.screen().contents());
    rendered
}

fn complete(home: &Home, text: &str) {
    let rendered = rendered(&fs::read(home.path().join("terminal.ansi")).unwrap());
    for visible in [
        "Preview: koi telemetry preview. Turn off: koi telemetry off.",
        "Reports include usage counts, crash locations, hourly sync totals, OS, and Koidrive version.",
        "No file names, paths, contents, error messages, server address, or account.",
        "This device is connected",
        "~/Koidrive is linked",
        "Background sync is running",
        "Setup complete. Add files to ~/Koidrive.",
        "Check sync: koi status",
    ] {
        assert!(
            rendered.contains(visible),
            "terminal erased {visible}:\n{rendered}"
        );
    }
    assert_eq!(rendered.matches(ROUTE).count(), 1, "{rendered}");
    assert!(
        !text.contains("http://") && !text.contains("https://"),
        "{text}"
    );
    assert!(home.path().join("Koidrive").is_dir(), "{text}");
    assert!(
        home.path().join("state/koidrive/service-running").exists(),
        "{text}"
    );
    assert!(text.contains("Background sync is running"), "{text}");
    assert!(
        text.contains("Setup complete. Add files to ~/Koidrive."),
        "{text}"
    );
    assert!(text.contains("Check sync: koi status"), "{text}");
    let consent = text.rfind(CONSENT).unwrap();
    assert!(
        text.find("~/Koidrive is linked").unwrap() < consent,
        "{text}"
    );
    assert!(
        text.find("Background sync is running").unwrap() < consent,
        "{text}"
    );
    assert!(text.find("Setup complete.").unwrap() > consent, "{text}");
    for hidden in [
        DEVICE,
        ACCOUNT,
        "credential.json",
        "Creating the R2 bucket",
        "Uploading Koidrive",
        "Press Enter to use this folder",
        "Create a shared folder named",
    ] {
        assert!(!text.contains(hidden), "leaked {hidden}: {text}");
    }
    let state = home.state.lock().unwrap();
    for id in state
        .spaces
        .iter()
        .map(|space| &space.space_id)
        .chain(state.buckets.iter())
    {
        assert!(!text.contains(id), "leaked {id}: {text}");
    }
}

#[test]
fn default_enter_deploys_and_links_the_exact_created_space() {
    for rich in [true, false] {
        let home = Home::new(&[], &[]);
        let text = home.run(rich, Browser::Opens, &[(ROUTE, 0), (CONSENT, 0)]);
        complete(&home, &text);
        if let Some(directory) = std::env::var_os("KOIDRIVE_SETUP_CAPTURE_DIR") {
            let directory = PathBuf::from(directory);
            fs::create_dir_all(&directory).unwrap();
            let mode = if rich { "rich" } else { "plain" };
            fs::copy(
                home.path().join("terminal.ansi"),
                directory.join(format!("new-server-{mode}.ansi")),
            )
            .unwrap();
            fs::write(directory.join(format!("new-server-{mode}.txt")), &text).unwrap();
        }
        assert!(
            !text.contains("http://") && !text.contains("https://"),
            "{text}"
        );
        assert!(
            !text.contains(SPACES) && !text.contains("Folder to sync"),
            "{text}"
        );
        let state = home.state.lock().unwrap();
        assert_eq!(state.created.len(), 1);
        assert_eq!(state.created[0].name, "Koidrive");
        assert_eq!(
            state.links,
            vec![(
                home.path().join("Koidrive"),
                state.created[0].space_id.clone()
            )]
        );
        assert!(state.requests.iter().any(|request| request
            == &format!("PUT /client/v4/accounts/{ACCOUNT}/workers/scripts/koidrive")));
        assert!(
            state
                .requests
                .iter()
                .any(|request| request == "POST /oauth2/revoke")
        );
    }
}

#[test]
fn existing_servers_and_spaces_follow_arrow_selection() {
    let home = Home::new(&["koidrive", "koidrive-work"], &["Personal", "Work"]);
    let text = home.run(
        true,
        Browser::Opens,
        &[(ROUTE, 0), (SERVERS, 1), (SPACES, 1), (CONSENT, 0)],
    );
    complete(&home, &text);
    let state = home.state.lock().unwrap();
    assert!(state.created.is_empty());
    assert_eq!(
        state.links,
        vec![(
            home.path().join("Koidrive"),
            state.spaces[1].space_id.clone()
        )]
    );
    assert!(state.requests.iter().any(|request| request
        == &format!("PUT /client/v4/accounts/{ACCOUNT}/workers/scripts/koidrive-work/secrets")));
    assert!(
        !state
            .requests
            .iter()
            .any(|request| request.contains("assets-upload-session"))
    );
}

#[test]
fn a_single_server_still_offers_its_existing_space() {
    let home = Home::new(&["koidrive"], &["Koidrive"]);
    let text = home.run(
        true,
        Browser::Opens,
        &[(ROUTE, 0), (SPACES, 0), (CONSENT, 0)],
    );
    complete(&home, &text);
    assert!(!text.contains(SERVERS), "{text}");
    let state = home.state.lock().unwrap();
    assert!(state.created.is_empty());
    assert_eq!(state.links[0].1, state.spaces[0].space_id);
}

#[test]
fn browser_spawn_failure_and_unsuccessful_exit_show_the_full_login_url() {
    for browser in [Browser::Missing, Browser::Fails] {
        for rich in [true, false] {
            let home = Home::new(&[], &[]);
            let text = home.run(rich, browser, &[(ROUTE, 0), (CONSENT, 0)]);
            assert!(
                text.contains("Couldn't open your browser. Open this page:"),
                "{text}"
            );
            assert!(
                text.contains(home.base.join("oauth2/auth").unwrap().as_str()),
                "{text}"
            );
            assert!(text.contains("code_challenge_method=S256"), "{text}");
            assert!(
                text.contains("Setup complete. Add files to ~/Koidrive."),
                "{text}"
            );
        }
    }
}

#[test]
fn rerun_keeps_the_existing_link_and_telemetry_preference() {
    let home = Home::new(&[], &[]);
    let first = home.run(true, Browser::Opens, &[(ROUTE, 0), (CONSENT, 0)]);
    complete(&home, &first);
    let before = home.state.lock().unwrap().requests.clone();
    let text = home.run(true, Browser::Opens, &[]);
    assert!(
        text.contains("Setup is already complete. Your folders have not changed."),
        "{text}"
    );
    assert!(text.contains("Background sync is running"), "{text}");
    assert!(
        !text.contains(ROUTE) && !text.contains(SPACES) && !text.contains(CONSENT),
        "{text}"
    );
    let state = home.state.lock().unwrap();
    assert_eq!(state.requests, before);
    assert_eq!(state.links.len(), 1);
}

#[test]
fn test_setup_rejects_remote_or_ambiguous_endpoints_before_opening_a_browser() {
    let home = Home::new(&[], &[]);
    for endpoint in [
        "https://127.0.0.1:9999/",
        "http://192.0.2.1:9999/",
        "http://localhost:9999/",
        "http://127.0.0.1/",
        "http://user@127.0.0.1:9999/",
        "http://:password@127.0.0.1:9999/",
        "http://127.0.0.1:9999/path",
        "http://127.0.0.1:9999/?query",
        "http://127.0.0.1:9999/#fragment",
    ] {
        let mut command = home.command();
        command
            .args(["server", "deploy", "--interactive"])
            .env("KOIDRIVE_TEST_SETUP_URL", endpoint);
        let output = terminal(command, &[], None);
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(!output.status.success(), "{endpoint}: {text}");
        assert!(
            text.contains("test setup requires an HTTP loopback IP URL"),
            "{endpoint}: {text}"
        );
    }
    let mut command = home.command();
    command
        .args(["server", "deploy", "--interactive"])
        .env_remove("KOIDRIVE_TEST_SETUP_URL");
    let output = terminal(command, &[], None);
    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("test setup needs both its loopback URL and bundle")
    );
    let mut command = home.command();
    command
        .args(["server", "deploy", "--interactive"])
        .env_remove("KOIDRIVE_TEST_SETUP_BUNDLE");
    let output = terminal(command, &[], None);
    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("test setup needs both its loopback URL and bundle")
    );
    for (name, value, expected) in [
        (
            "KOIDRIVE_TEST_SETUP_URL",
            "not a URL",
            "invalid test setup URL",
        ),
        (
            "KOIDRIVE_TEST_SETUP_BUNDLE",
            "relative-bundle",
            "the test setup bundle must be an absolute directory",
        ),
    ] {
        let mut command = home.command();
        command
            .args(["server", "deploy", "--interactive"])
            .env(name, value);
        let output = terminal(command, &[], None);
        assert!(!output.status.success());
        assert!(String::from_utf8(output.stdout).unwrap().contains(expected));
    }
    assert!(home.state.lock().unwrap().requests.is_empty());
    assert!(!home.path().join("browser-url").exists());
}

#[test]
fn checkout_waits_for_r2_before_completing_the_same_setup() {
    let home = Home::new(&[], &[]);
    home.state.lock().unwrap().r2_disabled = 2;
    let text = home.run(true, Browser::Opens, &[(ROUTE, 0), (CONSENT, 0)]);
    complete(&home, &text);
    assert!(
        text.contains("Complete the R2 checkout in your browser to continue."),
        "{text}"
    );
    assert!(text.contains("R2 is ready"), "{text}");
    let state = home.state.lock().unwrap();
    assert_eq!(
        state
            .requests
            .iter()
            .filter(|request| request.ends_with("/r2/buckets") && request.starts_with("GET "))
            .count(),
        3
    );
}

#[test]
fn off_paused_and_blocked_folders_never_report_successful_setup() {
    for (background, paused, blocked, expected) in [
        (false, false, None, "Background sync is off"),
        (true, true, None, "Sync is paused"),
        (
            true,
            false,
            Some(FolderState::Halted),
            "Sync needs attention",
        ),
        (
            true,
            false,
            Some(FolderState::Stopped),
            "Sync needs attention",
        ),
        (
            true,
            false,
            Some(FolderState::Offline),
            "Sync needs attention",
        ),
    ] {
        let home = Home::new(&[], &[]);
        fs::write(
            home.path().join("config/koidrive/config.toml"),
            format!("[service]\nbackground = {background}\n"),
        )
        .unwrap();
        {
            let mut state = home.state.lock().unwrap();
            state.paused = paused;
            state.folder_state = blocked;
        }
        let text = home.run(true, Browser::Opens, &[(ROUTE, 0), (CONSENT, 0)]);
        assert!(text.contains(expected), "{text}");
        assert!(
            !text.contains("Setup complete") && !text.contains("Background sync is running"),
            "{text}"
        );
        assert!(
            text.contains(if paused {
                "Resume sync: koi resume"
            } else if background {
                "Check sync: koi status"
            } else {
                "Start sync: koi service start"
            }),
            "{text}"
        );
        assert_eq!(
            home.path().join("state/koidrive/service-running").exists(),
            background
        );
        assert_eq!(home.state.lock().unwrap().links.len(), 1);
    }
}

#[cfg(target_os = "macos")]
#[test]
fn denied_folder_access_does_not_claim_that_running_service_means_syncing() {
    let home = Home::new(&[], &[]);
    home.state.lock().unwrap().denied = true;
    let text = home.run(true, Browser::Opens, &[(ROUTE, 0), (CONSENT, 0)]);
    assert!(home.path().join("state/koidrive/service-running").exists());
    assert!(text.contains("Background sync needs attention"), "{text}");
    assert!(
        !text.contains("Setup complete") && !text.contains("Background sync is running"),
        "{text}"
    );
}

fn pick(rich: bool, index: usize) -> String {
    if rich {
        format!("{}\r", "\u{1b}[B".repeat(index))
    } else {
        format!("{}\n", index + 1)
    }
}

fn typed(rich: bool, text: &str) -> String {
    format!("{text}{}", if rich { "\r" } else { "\n" })
}

fn consent(rich: bool) -> String {
    if rich { "\r".into() } else { "y\n".into() }
}

fn golden(home: &Home, name: &str, rich: bool) {
    let raw = fs::read(home.path().join("terminal.ansi")).unwrap();
    let mut actual = if rich {
        rendered(&raw)
    } else {
        String::from_utf8(raw.clone()).unwrap()
    };
    actual = actual.replace(
        &format!("Koidrive {}", env!("CARGO_PKG_VERSION")),
        "Koidrive <VERSION>",
    );
    let endpoint = home.base.join("oauth2/auth").unwrap();
    if let Some(start) = actual.find(endpoint.as_str()) {
        let end = start
            + actual[start..].find("code_challenge_method=S256").unwrap()
            + "code_challenge_method=S256".len();
        actual.replace_range(start..end, "<AUTHORIZATION_URL>");
    }
    let actual = actual
        .lines()
        .map(|line| line.trim_end_matches(' '))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let mode = if rich { "rich" } else { "plain" };
    let file = format!("{name}.{mode}.txt");
    if let Some(directory) = std::env::var_os("KOIDRIVE_SETUP_CAPTURE_DIR") {
        let directory = PathBuf::from(directory);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join(format!("{name}.{mode}.ansi")), raw).unwrap();
        fs::write(directory.join(format!("{name}.{mode}.actual.txt")), &actual).unwrap();
    }
    let expected = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/goldens/setup")
            .join(file),
    )
    .unwrap();
    assert_eq!(
        actual, expected,
        "{name} {mode}\nexpected:\n{expected}\nactual:\n{actual}"
    );
}

#[test]
fn golden_new_server() {
    for rich in [true, false] {
        let home = Home::new(&[], &[]);
        home.run_steps(
            rich,
            Browser::Opens,
            &[(ROUTE, pick(rich, 0)), (CONSENT, consent(rich))],
        );
        golden(&home, "new-server", rich);
    }
}

#[test]
fn golden_existing_single_server() {
    for rich in [true, false] {
        let home = Home::new(&["koidrive"], &["Koidrive", "Work"]);
        home.run_steps(
            rich,
            Browser::Opens,
            &[
                (ROUTE, pick(rich, 0)),
                (SPACES, pick(rich, 0)),
                (CONSENT, consent(rich)),
            ],
        );
        golden(&home, "existing-single", rich);
    }
}

#[test]
fn golden_existing_multiple_servers() {
    for rich in [true, false] {
        let home = Home::new(&["koidrive", "koidrive-studio"], &["Koidrive", "Work"]);
        home.run_steps(
            rich,
            Browser::Opens,
            &[
                (ROUTE, pick(rich, 0)),
                (SERVERS, pick(rich, 0)),
                (SPACES, pick(rich, 1)),
                (CONSENT, consent(rich)),
            ],
        );
        golden(&home, "existing-multiple", rich);
    }
}

#[test]
fn golden_pairing() {
    for rich in [true, false] {
        let home = Home::new(&[], &[]);
        home.run_steps(
            rich,
            Browser::Opens,
            &[
                (ROUTE, pick(rich, 1)),
                ("Pairing code", typed(rich, "CODE@files.example.com")),
                (CONSENT, consent(rich)),
            ],
        );
        golden(&home, "pairing", rich);
        assert!(home.path().join("state/koidrive/service-running").exists());
        assert_eq!(home.state.lock().unwrap().links.len(), 1);
    }
}

#[test]
fn golden_rerun() {
    for rich in [true, false] {
        let home = Home::new(&[], &[]);
        home.run(rich, Browser::Opens, &[(ROUTE, 0), (CONSENT, 0)]);
        home.run_steps(rich, Browser::Opens, &[]);
        golden(&home, "rerun", rich);
        assert_eq!(home.state.lock().unwrap().links.len(), 1);
    }
}

#[test]
fn golden_r2_checkout() {
    for rich in [true, false] {
        let home = Home::new(&[], &[]);
        home.state.lock().unwrap().r2_disabled = 1;
        home.run_steps(
            rich,
            Browser::Opens,
            &[(ROUTE, pick(rich, 0)), (CONSENT, consent(rich))],
        );
        golden(&home, "r2-checkout", rich);
    }
}

#[test]
fn golden_browser_failure() {
    for rich in [true, false] {
        for browser in [Browser::Missing, Browser::Fails] {
            let home = Home::new(&[], &[]);
            home.run_steps(
                rich,
                browser,
                &[(ROUTE, pick(rich, 0)), (CONSENT, consent(rich))],
            );
            golden(&home, "browser-failure", rich);
        }
    }
}

#[test]
fn golden_existing_files_sync() {
    for rich in [true, false] {
        let home = Home::new(&[], &["Work"]);
        fs::create_dir(home.path().join("Koidrive")).unwrap();
        fs::write(home.path().join("Koidrive/report.docx"), "draft").unwrap();
        home.run_steps(
            rich,
            Browser::Opens,
            &[
                (ROUTE, pick(rich, 1)),
                ("Pairing code", typed(rich, "CODE@files.example.com")),
                (SPACES, pick(rich, 0)),
                ("What should Koidrive do?", pick(rich, 1)),
                (CONSENT, consent(rich)),
            ],
        );
        golden(&home, "files-sync", rich);
    }
}

#[test]
fn golden_existing_files_another_folder() {
    for rich in [true, false] {
        let home = Home::new(&[], &["Work"]);
        fs::create_dir(home.path().join("Koidrive")).unwrap();
        fs::write(home.path().join("Koidrive/report.docx"), "draft").unwrap();
        home.run_steps(
            rich,
            Browser::Opens,
            &[
                (ROUTE, pick(rich, 1)),
                ("Pairing code", typed(rich, "CODE@files.example.com")),
                (SPACES, pick(rich, 0)),
                ("What should Koidrive do?", pick(rich, 0)),
                ("Folder to sync", typed(rich, "~/Work")),
                (CONSENT, consent(rich)),
            ],
        );
        golden(&home, "files-another", rich);
    }
}

#[test]
fn golden_existing_files_new_space() {
    for rich in [true, false] {
        let home = Home::new(&[], &[]);
        fs::create_dir(home.path().join("Koidrive")).unwrap();
        fs::write(home.path().join("Koidrive/report.docx"), "draft").unwrap();
        home.run_steps(
            rich,
            Browser::Opens,
            &[
                (ROUTE, pick(rich, 1)),
                ("Pairing code", typed(rich, "CODE@files.example.com")),
                ("What should Koidrive do?", pick(rich, 1)),
                (CONSENT, consent(rich)),
            ],
        );
        golden(&home, "files-new-space", rich);
    }
}

#[test]
fn golden_default_collision() {
    for rich in [true, false] {
        let home = Home::new(&[], &[]);
        home.state.lock().unwrap().collide_create = true;
        fs::create_dir(home.path().join("Koidrive")).unwrap();
        fs::write(home.path().join("Koidrive/report.docx"), b"draft").unwrap();
        home.run_steps(
            rich,
            Browser::Opens,
            &[
                (ROUTE, pick(rich, 1)),
                ("Pairing code", typed(rich, "CODE@files.example.com")),
                (SPACES, pick(rich, 0)),
                ("What should Koidrive do?", pick(rich, 1)),
                (CONSENT, consent(rich)),
            ],
        );
        golden(&home, "default-collision", rich);
        let screen = rendered(&fs::read(home.path().join("terminal.ansi")).unwrap());
        assert_eq!(
            screen.matches("What should Koidrive do?").count(),
            1,
            "{screen}"
        );
        assert!(
            !screen.contains("new shared folder named Koidrive"),
            "{screen}"
        );
        let state = home.state.lock().unwrap();
        assert!(state.created.is_empty());
        assert_eq!(state.spaces.len(), 1);
        assert_eq!(state.links.len(), 1);
        assert_eq!(state.links[0].1, state.spaces[0].space_id);
    }
}

#[test]
fn golden_create_shared_folder() {
    for rich in [true, false] {
        let home = Home::new(&["koidrive"], &["Koidrive"]);
        home.run_steps(
            rich,
            Browser::Opens,
            &[
                (ROUTE, pick(rich, 0)),
                (SPACES, pick(rich, 1)),
                ("Shared folder name", typed(rich, "Photos")),
                (CONSENT, consent(rich)),
            ],
        );
        golden(&home, "create-shared-folder", rich);
    }
}

#[test]
fn creating_a_shared_folder_reprompts_for_an_existing_name() {
    for rich in [true, false] {
        for spaces in [&["Work"][..], &["Work", "Work"][..]] {
            let home = Home::new(&[], spaces);
            let text = home.run_steps(
                rich,
                Browser::Opens,
                &[
                    (ROUTE, pick(rich, 1)),
                    ("Pairing code", typed(rich, "CODE@files.example.com")),
                    (SPACES, pick(rich, spaces.len())),
                    ("Shared folder name", typed(rich, "Work")),
                    (
                        "A shared folder named Work already exists. Choose another name.",
                        String::new(),
                    ),
                    ("Shared folder name", typed(rich, "Photos")),
                    (CONSENT, consent(rich)),
                ],
            );
            assert!(
                text.contains("A shared folder named Work already exists. Choose another name."),
                "{text}"
            );
            assert!(text.contains("~/Koidrive is linked to Photos"), "{text}");
            let screen = rendered(&fs::read(home.path().join("terminal.ansi")).unwrap());
            assert!(
                screen.contains("A shared folder named Work already exists. Choose another name."),
                "{screen}"
            );
            assert!(
                screen.contains("~/Koidrive is linked to Photos"),
                "{screen}"
            );
            let state = home.state.lock().unwrap();
            assert_eq!(state.created.len(), 1);
            assert_eq!(state.created[0].name, "Photos");
            assert_eq!(state.links.len(), 1);
            assert_eq!(state.links[0].1, state.created[0].space_id);
        }
    }
}

#[test]
fn a_concurrent_creation_collision_never_links_the_existing_shared_folder() {
    for rich in [true, false] {
        let home = Home::new(&[], &["Work"]);
        home.state.lock().unwrap().collide_create = true;
        fs::create_dir(home.path().join("Koidrive")).unwrap();
        fs::write(home.path().join("Koidrive/report.docx"), b"draft").unwrap();
        let text = home.run_steps(
            rich,
            Browser::Opens,
            &[
                (ROUTE, pick(rich, 1)),
                ("Pairing code", typed(rich, "CODE@files.example.com")),
                (SPACES, pick(rich, 1)),
                ("Shared folder name", typed(rich, "Photos")),
                (
                    "A shared folder named Photos already exists. Choose another name.",
                    String::new(),
                ),
                ("Shared folder name", typed(rich, "Travel")),
                ("What should Koidrive do?", pick(rich, 1)),
                (CONSENT, consent(rich)),
            ],
        );
        assert!(
            text.contains("A shared folder named Photos already exists. Choose another name."),
            "{text}"
        );
        assert!(text.contains("~/Koidrive is linked to Travel"), "{text}");
        let screen = rendered(&fs::read(home.path().join("terminal.ansi")).unwrap());
        assert!(
            screen.contains("A shared folder named Photos already exists. Choose another name."),
            "{screen}"
        );
        assert!(
            screen.contains("~/Koidrive is linked to Travel"),
            "{screen}"
        );
        assert!(
            !text.contains("They will upload to a new shared folder named Photos."),
            "{text}"
        );
        assert!(
            text.contains("They will upload to a new shared folder named Travel."),
            "{text}"
        );
        assert_eq!(
            screen.matches("What should Koidrive do?").count(),
            1,
            "{screen}"
        );
        assert!(
            screen.find("already exists").unwrap()
                < screen.find("What should Koidrive do?").unwrap(),
            "{screen}"
        );
        let state = home.state.lock().unwrap();
        assert_eq!(state.created.len(), 1);
        assert_eq!(state.created[0].name, "Travel");
        assert_eq!(state.links.len(), 1);
        assert_eq!(state.links[0].1, state.created[0].space_id);
    }
}

#[test]
fn metadata_only_setup_links_with_the_real_driver_without_upload_consent() {
    for rich in [true, false] {
        let home = Home::new(&[], &[]);
        home.real_driver();
        let root = home.path().join("Koidrive");
        fs::create_dir(&root).unwrap();
        for name in [".DS_Store", "THUMBS.DB", "desktop.ini", "Icon\r"] {
            fs::write(root.join(name), b"metadata").unwrap();
        }
        let text = home.run_steps(rich, Browser::Missing, &[(CONSENT, consent(rich))]);
        assert!(!text.contains("contains files"), "{text}");
        assert!(!text.contains("What should Koidrive do?"), "{text}");
        assert!(text.contains("is linked to Koidrive"), "{text}");
        let driver = Driver::open(home.paths()).unwrap();
        let status = driver.status();
        assert_eq!(status.folders.len(), 1);
        assert_eq!(
            Path::new(&status.folders[0].path),
            fs::canonicalize(&root).unwrap()
        );
        assert!(root.join(".koidrive/space.json").is_file());
        assert!(root.join(".koidrive/device.json").is_file());
        let state = home.state.lock().unwrap();
        assert!(
            state.links.is_empty(),
            "the fake daemon must not perform this link"
        );
        assert_eq!(status.folders[0].space_id, state.created[0].space_id);
    }
}

#[test]
fn a_default_creation_collision_requires_a_choice_and_fresh_file_consent() {
    for rich in [true, false] {
        for sync_files in [false, true] {
            let home = Home::new(&[], &[]);
            home.real_driver();
            home.state.lock().unwrap().collide_create = true;
            let root = home.path().join("Koidrive");
            fs::create_dir(&root).unwrap();
            fs::write(root.join("report.docx"), b"draft").unwrap();
            let mut command = home.command();
            command.args(["setup", "--interactive", "--no-app", "--telemetry", "off"]);
            if !rich {
                command.arg("--plain");
            }
            let mut steps = vec![
                (SPACES, pick(rich, 0)),
                (
                    "What should Koidrive do?",
                    pick(rich, usize::from(sync_files)),
                ),
            ];
            if !sync_files {
                steps.push(("Folder to sync", typed(rich, "~/Empty")));
            }
            let text = home.finish_run(rich, command, &steps);
            assert!(!text.contains("new shared folder named Koidrive"), "{text}");
            let collision = text
                .find("A shared folder named Koidrive is available.")
                .unwrap();
            let choice = text.find(SPACES).unwrap();
            let existing = text
                .find("Files in Koidrive will download into ~/Koidrive.")
                .unwrap();
            assert!(collision < choice && choice < existing, "{text}");
            let screen = rendered(&fs::read(home.path().join("terminal.ansi")).unwrap());
            assert_eq!(
                screen.matches("What should Koidrive do?").count(),
                1,
                "{screen}"
            );
            assert!(screen.contains(SPACES), "{screen}");
            assert!(
                screen.contains("A shared folder named Koidrive is available."),
                "{screen}"
            );
            assert!(
                screen.contains("~/Koidrive contains files. They will upload to Koidrive."),
                "{screen}"
            );
            assert!(
                screen.contains("Files in Koidrive will download into ~/Koidrive."),
                "{screen}"
            );
            let driver = Driver::open(home.paths()).unwrap();
            let status = driver.status();
            let linked = if sync_files {
                root.clone()
            } else {
                home.path().join("Empty")
            };
            assert_eq!(status.folders.len(), 1);
            assert_eq!(
                Path::new(&status.folders[0].path),
                fs::canonicalize(linked).unwrap()
            );
            assert_eq!(root.join(".koidrive/device.json").exists(), sync_files);
            assert_eq!(root.join(".koidrive/space.json").exists(), sync_files);
            assert_eq!(fs::read(root.join("report.docx")).unwrap(), b"draft");
            let state = home.state.lock().unwrap();
            assert_eq!(state.spaces.len(), 1);
            assert!(state.created.is_empty());
            assert!(
                state.links.is_empty(),
                "the fake daemon must not perform this link"
            );
            assert_eq!(status.folders[0].space_id, state.spaces[0].space_id);
            assert_eq!(
                state
                    .requests
                    .iter()
                    .filter(|request| request.as_str() == "POST /v1/spaces")
                    .count(),
                1
            );
        }
    }
}

#[test]
fn scripted_metadata_only_folders_still_require_existing_files_consent() {
    let home = Home::new(&[], &["Work"]);
    home.real_driver();
    let root = home.path().join("Koidrive");
    fs::create_dir(&root).unwrap();
    fs::write(root.join(".DS_Store"), b"metadata").unwrap();
    let id = home.state.lock().unwrap().spaces[0].space_id.clone();
    let output = home
        .command()
        .args(["setup", "--json", "--no-app", "--folder"])
        .arg(&root)
        .args(["--space", &id])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        envelope.to_string().contains("--accept-existing-files"),
        "{envelope}"
    );
    assert!(
        Driver::open(home.paths())
            .unwrap()
            .status()
            .folders
            .is_empty()
    );
    assert!(!root.join(".koidrive/space.json").exists());
    assert!(!root.join(".koidrive/device.json").exists());
}

#[test]
fn metadata_does_not_hide_ordinary_files_from_real_driver_setup_consent() {
    for rich in [true, false] {
        for sync_files in [false, true] {
            let home = Home::new(&[], &[]);
            home.real_driver();
            let root = home.path().join("Koidrive");
            fs::create_dir(&root).unwrap();
            fs::write(root.join(".DS_Store"), b"metadata").unwrap();
            fs::write(root.join("report.docx"), b"draft").unwrap();
            let mut steps = vec![(
                "What should Koidrive do?",
                pick(rich, usize::from(sync_files)),
            )];
            if !sync_files {
                steps.push(("Folder to sync", typed(rich, "~/Empty")));
            }
            steps.push((CONSENT, consent(rich)));
            let text = home.run_steps(rich, Browser::Missing, &steps);
            assert!(
                text.contains(
                    "contains files. They will upload to a new shared folder named Koidrive."
                ),
                "{text}"
            );
            let linked = if sync_files {
                root.clone()
            } else {
                home.path().join("Empty")
            };
            let driver = Driver::open(home.paths()).unwrap();
            let status = driver.status();
            assert_eq!(status.folders.len(), 1);
            assert_eq!(
                Path::new(&status.folders[0].path),
                fs::canonicalize(&linked).unwrap()
            );
            assert_eq!(root.join(".koidrive/device.json").exists(), sync_files);
            assert_eq!(root.join(".koidrive/space.json").exists(), sync_files);
            assert_eq!(fs::read(root.join("report.docx")).unwrap(), b"draft");
            assert!(home.state.lock().unwrap().links.is_empty());
        }
    }
}
