// SPDX-License-Identifier: Apache-2.0

use std::fs;

use std::io::{self, BufRead, BufReader, Read, Write};

use std::os::unix::fs::PermissionsExt;

use std::os::unix::net::UnixListener;

use std::process::{Command, Stdio};

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use prost::Message;

struct Profile(tempfile::TempDir);

struct Fixture(Option<std::thread::JoinHandle<()>>);

impl Fixture {
    fn join(mut self) -> std::thread::Result<()> {
        self.0.take().unwrap().join()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(thread) = self.0.take() {
            let _ = thread.join();
        }
    }
}

struct Capture {
    exit: i32,
    stdout: String,
    stderr: String,
    terminal: String,
}

impl Profile {
    fn new() -> Self {
        let home = tempfile::tempdir_in("/tmp").unwrap();
        fs::create_dir_all(home.path().join("config/koidrive")).unwrap();
        fs::write(
            home.path().join("config/koidrive/config.toml"),
            "[service]\nbackground = false\n",
        )
        .unwrap();
        Self(home)
    }

    fn http(&self, status: u16, body: Vec<u8>) -> Fixture {
        self.http_responses(vec![("POST /v1/token ", status, body)])
    }

    fn http_responses(&self, replies: Vec<(&'static str, u16, Vec<u8>)>) -> Fixture {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let server = Fixture(Some(std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(15);
            for (expected, status, body) in replies {
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "missing HTTP fixture request");
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("{error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                assert!(first.starts_with(expected), "{first}");
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    assert!(
                        reader.read_line(&mut line).unwrap() > 0,
                        "HTTP fixture request ended before its headers"
                    );
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                }
                reader.read_exact(&mut vec![0; length]).unwrap();
                write!(
                stream,
                "HTTP/1.1 {status} Fixture\r\nContent-Type: application/x-protobuf\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
                stream.write_all(&body).unwrap();
            }
        })));
        self.save_credential(&format!("http://{address}/"));
        server
    }

    fn save_credential(&self, server: &str) {
        let state = self.0.path().join("state/koidrive");
        fs::create_dir_all(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let credential = state.join("credential.json");
        fs::write(&credential, json!({"server":server,"user_id":"owner","device_id":"device-internal","device_name":"Laptop","refresh_credential":"fixture-only-refresh"}).to_string()).unwrap();
        fs::set_permissions(&credential, fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn run(&self, args: &[&str], tty: bool, pipe_stdout: bool, vars: &[(&str, &str)]) -> Capture {
        self.run_at_width(args, tty, pipe_stdout, vars, 100)
    }

    fn run_at_width(
        &self,
        args: &[&str],
        tty: bool,
        pipe_stdout: bool,
        vars: &[(&str, &str)],
        width: u16,
    ) -> Capture {
        let mut command = Command::new(env!("CARGO_BIN_EXE_koi"));
        command
            .args(args)
            .current_dir(self.0.path())
            .env("HOME", self.0.path())
            .env("XDG_CONFIG_HOME", self.0.path().join("config"))
            .env("XDG_STATE_HOME", self.0.path().join("state"))
            .env_remove("XDG_RUNTIME_DIR")
            .env_remove("KOI_PLAIN")
            .env_remove("KOIDRIVE_LOG")
            .env_remove("NO_COLOR")
            .env_remove("CLICOLOR")
            .env_remove("CLICOLOR_FORCE")
            .env("TERM", "xterm-256color")
            .env("CI", "1")
            .env("DO_NOT_TRACK", "1")
            .env("KOIDRIVE_TELEMETRY", "0")
            .env("KOIDRIVE_UPDATE_CHECK", "0")
            .envs(vars.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut terminal = tty.then(|| Terminal::open(width, 32));
        if let Some(terminal) = &terminal {
            command.stderr(terminal.slave());
            if !pipe_stdout {
                command.stdout(terminal.slave());
            }
        }
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        match &mut terminal {
            Some(terminal) => while terminal.read(&mut child, deadline) {},
            None => {
                while child.try_wait().unwrap().is_none() {
                    if Instant::now() >= deadline {
                        child.kill().unwrap();
                        panic!("koi {args:?} did not exit");
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
        let output = child.wait_with_output().unwrap();
        let terminal = terminal
            .map(|terminal| {
                String::from_utf8(terminal.output().to_vec())
                    .unwrap()
                    .replace("\r\n", "\n")
            })
            .unwrap_or_default();
        Capture {
            exit: output.status.code().unwrap(),
            stdout: String::from_utf8(output.stdout).unwrap(),
            stderr: String::from_utf8(output.stderr).unwrap(),
            terminal,
        }
    }

    fn fixture(&self, count: usize, reply: impl Fn(Value) -> Value + Send + 'static) -> Fixture {
        let state = self.0.path().join("state/koidrive");
        fs::create_dir_all(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let listener = UnixListener::bind(state.join("daemon.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        Fixture(Some(std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(15);
            for _ in 0..count {
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "missing fixture request");
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("{error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                let response = reply(request);
                writeln!(stream, "{response}").unwrap();
            }
        })))
    }
}
