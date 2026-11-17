// SPDX-License-Identifier: Apache-2.0

use std::io::{self, PipeWriter};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};
use std::thread::{self, JoinHandle};

/// A child process stopped after its fork and before its exec. Until it is dropped, it holds a copy of every
/// descriptor this process had open at the fork, close-on-exec ones included, as any child does while it starts.
pub struct Forked {
    release: PipeWriter,
    spawn: Option<JoinHandle<io::Result<Child>>>,
}

impl Forked {
    pub fn start() -> Self {
        let (ready, forked) = io::pipe().unwrap();
        let (exec, release) = io::pipe().unwrap();
        let mut command = Command::new("true");
        // SAFETY: the hook only calls write(2) and read(2), which are async-signal-safe.
        unsafe {
            command.pre_exec(move || {
                rustix::io::write(&forked, &[0])?;
                rustix::io::read(&exec, &mut [0; 1])?;
                Ok(())
            });
        }
        let spawn = thread::spawn(move || command.spawn());
        rustix::io::read(&ready, &mut [0; 1]).unwrap();
        Self {
            release,
            spawn: Some(spawn),
        }
    }
}

impl Drop for Forked {
    fn drop(&mut self) {
        let _ = rustix::io::write(&self.release, &[0]);
        if let Some(spawn) = self.spawn.take()
            && let Ok(Ok(mut child)) = spawn.join()
        {
            let _ = child.wait();
        }
    }
}
