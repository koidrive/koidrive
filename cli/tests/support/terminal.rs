// SPDX-License-Identifier: Apache-2.0

#![allow(dead_code)]

use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};

pub struct Terminal {
    master: File,
    slave: File,
    output: Vec<u8>,
}

impl Terminal {
    pub fn open(columns: u16, rows: u16) -> Self {
        use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};

        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
        rustix::io::fcntl_setfd(&master, rustix::io::FdFlags::CLOEXEC).unwrap();
        grantpt(&master).unwrap();
        unlockpt(&master).unwrap();
        let name = ptsname(&master, Vec::new()).unwrap();
        let slave = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(name.to_str().unwrap())
            .unwrap();
        let flags = rustix::fs::fcntl_getfl(&master).unwrap();
        rustix::fs::fcntl_setfl(&master, flags | rustix::fs::OFlags::NONBLOCK).unwrap();
        rustix::termios::tcsetwinsize(
            &master,
            rustix::termios::Winsize {
                ws_row: rows,
                ws_col: columns,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
        )
        .unwrap();
        Self {
            master: File::from(master),
            slave,
            output: Vec::new(),
        }
    }

    pub fn slave(&self) -> File {
        self.slave.try_clone().unwrap()
    }

    pub fn write(&self, bytes: &[u8]) {
        (&self.master).write_all(bytes).unwrap();
    }

    pub fn attributes(&self) -> rustix::termios::Termios {
        rustix::termios::tcgetattr(&self.master).unwrap()
    }

    pub fn output(&self) -> &[u8] {
        &self.output
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.output).replace("\r\n", "\n")
    }

    pub fn read(&mut self, child: &mut Child, deadline: Instant) -> bool {
        let exited = child.try_wait().unwrap().is_some();
        let before = self.output.len();
        self.drain();
        if exited {
            return false;
        }
        if Instant::now() > deadline {
            let leader = rustix::process::Pid::from_child(child);
            if rustix::process::kill_process_group(leader, rustix::process::Signal::KILL).is_err() {
                let _ = child.kill();
            }
            let _ = child.wait();
            panic!(
                "the program on the terminal did not exit: {}",
                String::from_utf8_lossy(&self.output)
            );
        }
        if self.output.len() == before {
            let mut ready = [PollFd::new(&self.master, PollFlags::IN)];
            let interval = Timespec::try_from(Duration::from_millis(10)).unwrap();
            let _ = poll(&mut ready, Some(&interval));
        }
        true
    }

    pub fn wait(&mut self, child: &mut Child, within: Duration) -> ExitStatus {
        let deadline = Instant::now() + within;
        while self.read(child, deadline) {}
        child.wait().unwrap()
    }

    fn drain(&mut self) {
        let mut buffer = [0; 8192];
        loop {
            match self.master.read(&mut buffer) {
                Ok(0) => return,
                Ok(count) => self.output.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error)
                    if error.kind() == ErrorKind::WouldBlock
                        || error.raw_os_error() == Some(rustix::io::Errno::IO.raw_os_error()) =>
                {
                    return;
                }
                Err(error) => panic!("terminal read failed: {error}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::time::{Duration, Instant};

    use super::Terminal;

    #[test]
    fn output_written_after_the_last_read_by_a_child_that_then_exits_is_kept() {
        let mut terminal = Terminal::open(80, 24);
        let mut child = Command::new("/bin/sh")
            .args(["-c", "read answer; printf 'last line'"])
            .stdin(terminal.slave())
            .stdout(terminal.slave())
            .stderr(terminal.slave())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        assert!(terminal.read(&mut child, deadline));
        terminal.write(b"n\n");
        assert!(child.wait().unwrap().success());
        assert!(!terminal.read(&mut child, deadline));
        assert_eq!(terminal.text(), "n\nlast line");
    }
}
