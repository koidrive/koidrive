use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};

use crate::terminal::{Progress, Terminal};

pub(super) struct Echo {
    json: bool,
    quiet: bool,
    pub(super) held: Vec<String>,
    log: Option<File>,
}

pub(super) struct Steps(Option<Terminal>);

const KEPT_LOG: u64 = 1024 * 1024;

#[derive(Clone, Copy)]
pub(super) enum Child {
    Progress,
    Reconcile,
}

impl Echo {
    pub(super) fn open(path: &Path, json: bool, verbose: bool) -> Echo {
        if fs::metadata(path).is_ok_and(|metadata| metadata.len() > KEPT_LOG) {
            let _ = fs::rename(path, path.with_extension("log.1"));
        }
        let log = (path.parent().map(fs::create_dir_all).transpose().ok())
            .and_then(|_| OpenOptions::new().create(true).append(true).open(path).ok());
        Echo {
            log,
            ..Echo::console(json, verbose)
        }
    }

    #[cfg(target_os = "macos")]
    pub(super) fn journal(&self) -> Option<File> {
        self.log.as_ref().and_then(|log| log.try_clone().ok())
    }

    pub(super) fn console(json: bool, verbose: bool) -> Echo {
        Echo {
            json,
            quiet: !json && !verbose,
            held: Vec::new(),
            log: None,
        }
    }

    pub(super) fn log(&mut self, line: &str) {
        if let Some(log) = &mut self.log {
            let _ = writeln!(log, "{line}");
        }
    }

    pub(super) fn line(&mut self, line: &str, stderr: bool) {
        if let Some(log) = &mut self.log {
            let _ = writeln!(log, "{line}");
        }
        if self.quiet {
            self.held.push(line.to_owned());
            return;
        }
        match self.json || stderr {
            true => eprintln!("{line}"),
            false => println!("{line}"),
        }
    }

    pub(super) fn replay(&mut self) {
        for line in self.held.drain(..) {
            eprintln!("{line}");
        }
    }

    pub(super) fn finish(&mut self, step: Option<Progress<'_>>, message: &str) {
        self.held.clear();
        self.log(message);
        if let Some(step) = step {
            step.finish(message);
        }
    }

    pub(super) fn fail(&mut self, step: Option<Progress<'_>>) {
        if let Some(step) = step {
            step.fail();
        }
        self.replay();
    }

    pub(super) async fn run(
        &mut self,
        mut command: crate::program::Tokio,
        within: Duration,
        kind: Child,
    ) -> io::Result<Option<(ExitStatus, Vec<u8>, String)>> {
        let shown = format!("{command:?}");
        if let Some(log) = &mut self.log {
            let _ = writeln!(log, "$ {shown}");
        }
        let kill_on_drop = match kind {
            Child::Progress => true,
            Child::Reconcile => {
                command.process_group(0);
                false
            }
        };
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(kill_on_drop)
            .spawn()?;
        let mut stdout = BufReader::new(child.stdout.take().expect("stdout is piped")).lines();
        let mut stderr = BufReader::new(child.stderr.take().expect("stderr is piped")).lines();
        let finished = tokio::time::timeout(within, async {
            let (mut printed, mut errors) = (Vec::new(), String::new());
            let (mut out_open, mut err_open) = (true, true);
            while out_open || err_open {
                tokio::select! {
                    line = stdout.next_line(), if out_open => match line? {
                        Some(line) => {
                            match kind {
                                Child::Progress => self.line(&line, false),
                                Child::Reconcile => {
                                    if let Some(log) = &mut self.log {
                                        let _ = writeln!(log, "{line}");
                                    }
                                }
                            }
                            printed.extend_from_slice(line.as_bytes());
                            printed.push(b'\n');
                        }
                        None => out_open = false,
                    },
                    line = stderr.next_line(), if err_open => match line? {
                        Some(line) => {
                            self.line(&line, true);
                            errors.push_str(&line);
                            errors.push('\n');
                        }
                        None => err_open = false,
                    },
                }
            }
            let status = child.wait().await?;
            io::Result::Ok((status, printed, errors))
        })
        .await;
        match finished {
            Ok(finished) => finished.map(Some),
            Err(_) => {
                if let Some(log) = &mut self.log {
                    let _ = writeln!(log, "did not finish within {} seconds", within.as_secs());
                }
                Ok(None)
            }
        }
    }
}

impl Steps {
    pub(super) fn new(json: bool, verbose: bool) -> Steps {
        Steps((!json).then(|| Terminal::new(verbose, io::stderr().is_terminal())))
    }

    pub(super) fn start(&self, message: &str) -> Option<Progress<'_>> {
        self.0.as_ref().map(|terminal| terminal.progress(message))
    }

    pub(super) fn still(&self, message: &str) -> Option<Progress<'_>> {
        self.0
            .as_ref()
            .map(|terminal| terminal.still_progress(message))
    }
}

pub(super) fn minutes(within: Duration) -> u64 {
    within.as_secs().div_ceil(60)
}

pub(super) fn exit(status: ExitStatus) -> String {
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exit status {code}"),
        (None, Some(signal)) => format!("signal {signal}"),
        _ => status.to_string(),
    }
}
