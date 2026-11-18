// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

#[derive(Clone)]
pub(crate) struct Confined(PathBuf);

impl Confined {
    pub(crate) fn new(path: &Path) -> io::Result<Confined> {
        crate::lifeline::confine_passed(path)?;
        Ok(Confined(path.to_path_buf()))
    }
}

#[derive(Clone)]
pub(crate) enum Arg {
    Text(String),
    Path(Confined),
}

impl Arg {
    fn os(&self) -> &OsStr {
        match self {
            Arg::Text(text) => OsStr::new(text),
            Arg::Path(Confined(path)) => path.as_os_str(),
        }
    }
}

impl fmt::Display for Arg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.os().to_string_lossy().fmt(f)
    }
}

impl From<&str> for Arg {
    fn from(text: &str) -> Self {
        Arg::Text(text.to_owned())
    }
}

impl From<&&str> for Arg {
    fn from(text: &&str) -> Self {
        Arg::Text((*text).to_owned())
    }
}

impl From<String> for Arg {
    fn from(text: String) -> Self {
        Arg::Text(text)
    }
}

impl From<&String> for Arg {
    fn from(text: &String) -> Self {
        Arg::Text(text.clone())
    }
}

impl From<&Arg> for Arg {
    fn from(arg: &Arg) -> Self {
        arg.clone()
    }
}

impl From<Confined> for Arg {
    fn from(path: Confined) -> Self {
        Arg::Path(path)
    }
}

impl From<&Confined> for Arg {
    fn from(path: &Confined) -> Self {
        Arg::Path(path.clone())
    }
}

pub(crate) trait Raw {
    fn push(&mut self, arg: &OsStr);
    fn set(&mut self, key: &str, value: &OsStr);
    fn unset(&mut self, key: &str);
}

impl Raw for std::process::Command {
    fn push(&mut self, arg: &OsStr) {
        self.arg(arg);
    }
    fn set(&mut self, key: &str, value: &OsStr) {
        self.env(key, value);
    }
    fn unset(&mut self, key: &str) {
        self.env_remove(key);
    }
}

impl Raw for tokio::process::Command {
    fn push(&mut self, arg: &OsStr) {
        self.arg(arg);
    }
    fn set(&mut self, key: &str, value: &OsStr) {
        self.env(key, value);
    }
    fn unset(&mut self, key: &str) {
        self.env_remove(key);
    }
}

pub(crate) struct Invocation<C>(C);

pub(crate) type Blocking = Invocation<std::process::Command>;
pub(crate) type Tokio = Invocation<tokio::process::Command>;

impl<C: Raw> Invocation<C> {
    pub(crate) fn arg(&mut self, arg: impl Into<Arg>) -> &mut Self {
        self.0.push(arg.into().os());
        self
    }

    pub(crate) fn args<A: Into<Arg>>(&mut self, args: impl IntoIterator<Item = A>) -> &mut Self {
        for arg in args {
            self.arg(arg);
        }
        self
    }

    pub(crate) fn env(&mut self, key: &str, value: impl Into<Arg>) -> &mut Self {
        self.0.set(key, value.into().os());
        self
    }

    pub(crate) fn env_remove(&mut self, key: &str) -> &mut Self {
        self.0.unset(key);
        self
    }
}

impl Blocking {
    pub(crate) fn stdin(&mut self, stdio: Stdio) -> &mut Self {
        self.0.stdin(stdio);
        self
    }

    pub(crate) fn stdout(&mut self, stdio: Stdio) -> &mut Self {
        self.0.stdout(stdio);
        self
    }

    pub(crate) fn stderr(&mut self, stdio: Stdio) -> &mut Self {
        self.0.stderr(stdio);
        self
    }

    pub(crate) fn spawn(&mut self) -> io::Result<std::process::Child> {
        self.0.spawn()
    }

    pub(crate) fn output(&mut self) -> io::Result<Output> {
        self.0.output()
    }
}

impl Tokio {
    pub(crate) fn stdin(&mut self, stdio: impl Into<Stdio>) -> &mut Self {
        self.0.stdin(stdio);
        self
    }

    pub(crate) fn stdout(&mut self, stdio: impl Into<Stdio>) -> &mut Self {
        self.0.stdout(stdio);
        self
    }

    pub(crate) fn stderr(&mut self, stdio: impl Into<Stdio>) -> &mut Self {
        self.0.stderr(stdio);
        self
    }

    pub(crate) fn process_group(&mut self, group: i32) -> &mut Self {
        self.0.process_group(group);
        self
    }

    pub(crate) fn kill_on_drop(&mut self, kill: bool) -> &mut Self {
        self.0.kill_on_drop(kill);
        self
    }

    pub(crate) fn spawn(&mut self) -> io::Result<tokio::process::Child> {
        self.0.spawn()
    }

    pub(crate) async fn output(&mut self) -> io::Result<Output> {
        self.0.output().await
    }

    pub(crate) async fn status(&mut self) -> io::Result<std::process::ExitStatus> {
        self.0.status().await
    }
}

impl fmt::Debug for Tokio {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.as_std().fmt(f)
    }
}

pub(crate) fn system(name: &'static str) -> Blocking {
    Invocation(std::process::Command::new(name))
}

pub(crate) fn system_async(name: &'static str) -> Tokio {
    Invocation(tokio::process::Command::new(name))
}

#[cfg(target_os = "macos")]
pub(crate) fn configured_async(tool: &Path) -> Tokio {
    Invocation(tokio::process::Command::new(tool))
}

pub(crate) fn found(program: &Path) -> io::Result<Blocking> {
    crate::lifeline::confine_program(program)?;
    Ok(Invocation(std::process::Command::new(program)))
}

pub(crate) fn found_async(program: &Path) -> io::Result<Tokio> {
    crate::lifeline::confine_program(program)?;
    Ok(Invocation(tokio::process::Command::new(program)))
}
