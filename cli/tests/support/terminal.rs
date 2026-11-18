// SPDX-License-Identifier: Apache-2.0

#![allow(dead_code)]

use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};

pub trait Program {
    fn exited(&mut self) -> bool;
    fn stop(&mut self);
}

impl Program for Child {
    fn exited(&mut self) -> bool {
        self.try_wait().unwrap().is_some()
    }

    fn stop(&mut self) {
        let leader = rustix::process::Pid::from_child(self);
        if rustix::process::kill_process_group(leader, rustix::process::Signal::KILL).is_err() {
            let _ = self.kill();
        }
        let _ = self.wait();
    }
}

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

    pub fn read(&mut self, program: &mut impl Program, deadline: Instant) -> bool {
        let exited = program.exited();
        let before = self.output.len();
        self.drain(program, deadline);
        if exited {
            return false;
        }
        if self.output.len() == before {
            self.stop_after(program, deadline);
            let interval = deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(10));
            let mut ready = [PollFd::new(&self.master, PollFlags::IN)];
            let _ = poll(&mut ready, Some(&Timespec::try_from(interval).unwrap()));
        }
        true
    }

    pub fn wait(&mut self, child: &mut Child, within: Duration) -> ExitStatus {
        let deadline = Instant::now() + within;
        while self.read(child, deadline) {}
        child.wait().unwrap()
    }

    fn drain(&mut self, program: &mut impl Program, deadline: Instant) {
        let mut buffer = [0; 8192];
        loop {
            self.stop_after(program, deadline);
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

    fn stop_after(&self, program: &mut impl Program, deadline: Instant) {
        if Instant::now() <= deadline {
            return;
        }
        program.stop();
        panic!(
            "the program on the terminal did not exit: {}",
            String::from_utf8_lossy(&self.output)
        );
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::process::{Child, ChildStdin, Command, Stdio};
    use std::time::{Duration, Instant};

    use super::{Program, Terminal};

    struct FinishesWhenChecked {
        child: Child,
        answer: Option<ChildStdin>,
    }

    impl Program for FinishesWhenChecked {
        fn exited(&mut self) -> bool {
            if let Some(mut answer) = self.answer.take() {
                answer.write_all(b"\n").unwrap();
            }
            self.child.wait().unwrap();
            true
        }

        fn stop(&mut self) {
            self.child.stop();
        }
    }

    #[test]
    fn a_line_written_just_before_the_exit_check_is_read() {
        let mut terminal = Terminal::open(80, 24);
        let mut child = Command::new("/bin/sh")
            .args(["-c", "read answer; printf 'last line'"])
            .stdin(Stdio::piped())
            .stdout(terminal.slave())
            .stderr(terminal.slave())
            .spawn()
            .unwrap();
        let answer = child.stdin.take();
        let mut program = FinishesWhenChecked { child, answer };
        let deadline = Instant::now() + Duration::from_secs(10);
        assert!(!terminal.read(&mut program, deadline));
        assert_eq!(terminal.output(), b"last line");
    }
}
