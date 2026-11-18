// SPDX-License-Identifier: Apache-2.0

use std::io::{BufRead, BufReader};
use std::process::Stdio;
use std::time::{Duration, Instant};

use koidrive_test_support::Sandbox;
use rustix::process::{Pid, Signal, kill_process};
use rustix::termios::LocalModes;

#[path = "support/terminal.rs"]
mod terminal;

use terminal::Terminal;

#[path = "support/process.rs"]
mod process;

#[test]
fn ctrl_c_at_either_password_prompt_exits_and_restores_terminal_echo() {
    for confirm in [false, true] {
        let home = Sandbox::new();
        let terminal = Terminal::open(80, 24);
        let before = terminal.attributes();
        assert!(before.local_modes.contains(LocalModes::ECHO));
        let mut child = process::koi(home.path())
            .args(["share", "create", ".", "--password", "--json"])
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path().join("config"))
            .env("XDG_STATE_HOME", home.path().join("state"))
            .env_remove("XDG_RUNTIME_DIR")
            .env("DO_NOT_TRACK", "1")
            .stdin(terminal.slave())
            .stderr(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while terminal.attributes().local_modes.contains(LocalModes::ECHO) {
            if Instant::now() > deadline || child.try_wait().unwrap().is_some() {
                let _ = child.kill();
                panic!("the password prompt did not hide input");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if confirm {
            let stderr = child.stderr.take().unwrap();
            let (sender, prompt) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut stderr = BufReader::new(stderr);
                let mut text = Vec::new();
                while stderr.read_until(b':', &mut text).unwrap_or(0) != 0 {
                    let _ = sender.send(std::mem::take(&mut text));
                }
            });
            terminal.write(b"a password\n");
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let text = prompt.recv_timeout(deadline.saturating_duration_since(Instant::now()));
                match text {
                    Ok(text)
                        if String::from_utf8_lossy(&text).contains("The same password again") =>
                    {
                        break;
                    }
                    Ok(_) => {}
                    Err(_) => {
                        let _ = child.kill();
                        child.wait().unwrap();
                        panic!("the confirmation prompt did not appear");
                    }
                }
            }
        }
        kill_process(Pid::from_child(&child), Signal::INT).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() > deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("Ctrl-C did not end the password prompt");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(130));
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(json["error"]["code"], "interrupted");
        assert_eq!(terminal.attributes().local_modes, before.local_modes);
        terminal.write(b"\n");
    }
}
