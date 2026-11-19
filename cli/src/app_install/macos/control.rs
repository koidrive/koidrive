use crate::app_install::error::{AppInstallError, AppInstallFailure, failure};
use crate::app_install::macos::constants::MAX_OUTPUT;
use crate::program::{Arg, Confined};
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::sync::watch;
use tokio::time::Instant;

pub(super) struct CancelOnDrop(pub(super) watch::Sender<bool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let _ = self.0.send(true);
    }
}
pub(super) struct Active {
    cancel: watch::Sender<bool>,
    finished: watch::Receiver<bool>,
}
#[derive(Clone, Default)]
pub(in crate::app_install) struct Registry(pub(super) Arc<Mutex<Vec<Arc<Active>>>>);
impl Registry {
    pub(super) fn register(&self, cancel: watch::Sender<bool>) -> Registration {
        let (finished, rx) = watch::channel(false);
        let active = Arc::new(Active {
            cancel,
            finished: rx,
        });
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(active.clone());
        Registration {
            registry: self.clone(),
            active,
            finished,
        }
    }

    pub(in crate::app_install) async fn cancel(&self) {
        let transactions = {
            let active = self.0.lock().unwrap_or_else(|error| error.into_inner());
            for transaction in active.iter() {
                transaction.cancel.send_replace(true);
            }
            active.clone()
        };
        for transaction in transactions {
            let mut finished = transaction.finished.clone();
            while !*finished.borrow_and_update() {
                if finished.changed().await.is_err() {
                    break;
                }
            }
        }
    }
}
pub(super) struct Registration {
    registry: Registry,
    active: Arc<Active>,
    finished: watch::Sender<bool>,
}
impl Drop for Registration {
    fn drop(&mut self) {
        self.registry
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retain(|entry| !Arc::ptr_eq(entry, &self.active));
        self.finished.send_replace(true);
    }
}
#[derive(Clone)]
pub(super) struct Control {
    pub(super) deadline: Instant,
    pub(super) cancel: watch::Receiver<bool>,
}
impl Control {
    pub(super) fn check(&self) -> Result<(), AppInstallError> {
        if *self.cancel.borrow() {
            return Err(failure(
                AppInstallFailure::Cancelled,
                "App installation cancelled",
            ));
        }
        if Instant::now() >= self.deadline {
            return Err(failure(
                AppInstallFailure::Deadline,
                "The app update ran out of time between steps",
            ));
        }
        Ok(())
    }

    async fn cancelled(&self) {
        let mut rx = self.cancel.clone();
        if !*rx.borrow() {
            let _ = rx.changed().await;
        }
    }

    pub(super) async fn bounded<T>(
        &self,
        step: &str,
        within: Duration,
        future: impl std::future::Future<Output = Result<T, AppInstallError>>,
    ) -> Result<T, AppInstallError> {
        self.check()?;
        let stalled = Instant::now() + within;
        tokio::select! {
            biased;
            _ = self.cancelled() => Err(failure(AppInstallFailure::Cancelled, "App installation cancelled")),
            _ = tokio::time::sleep_until(stalled.min(self.deadline)) => Err(match stalled < self.deadline {
                true => failure(AppInstallFailure::Stalled, format!("{step} did not finish within {} seconds", within.as_secs())),
                false => failure(AppInstallFailure::Deadline, format!("{step} was still running when the app update ran out of time")),
            }),
            result = future => result,
        }
    }

    pub(super) fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
}
static JOURNAL: Mutex<Option<File>> = Mutex::new(None);

pub(crate) fn journal(file: File) {
    *JOURNAL.lock().unwrap_or_else(|error| error.into_inner()) = Some(file);
}

pub(super) fn record(line: &str) {
    if let Some(file) = JOURNAL
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_mut()
    {
        let _ = writeln!(file, "{line}");
    }
}

pub(super) async fn step<T>(
    name: &str,
    work: impl std::future::Future<Output = Result<T, AppInstallError>>,
) -> Result<T, AppInstallError> {
    record(&format!("{name}…"));
    let started = Instant::now();
    let result = work.await;
    let took = started.elapsed().as_secs_f32();
    match &result {
        Ok(_) => record(&format!("  done in {took:.1}s")),
        Err(error) => record(&format!("  failed after {took:.1}s: {error}")),
    }
    result
}

pub(in crate::app_install) fn active() -> &'static Registry {
    static ACTIVE: OnceLock<Registry> = OnceLock::new();
    ACTIVE.get_or_init(Registry::default)
}
pub(super) fn io_error(error: impl ToString) -> AppInstallError {
    failure(AppInstallFailure::Io, error)
}
pub(super) async fn process(
    name: &str,
    within: Duration,
    program: &Path,
    args: &[Arg],
    control: &Control,
) -> Result<Output, AppInstallError> {
    step(name, run(name, within, program, args, control)).await
}

pub(super) struct Output {
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
}

async fn run(
    name: &str,
    within: Duration,
    program: &Path,
    args: &[Arg],
    control: &Control,
) -> Result<Output, AppInstallError> {
    control.check()?;
    record(&format!("  $ {}", program.display()));
    let mut child = crate::program::configured_async(program)
        .args(args.iter().cloned())
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(io_error)?;
    let process_group = child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .and_then(rustix::process::Pid::from_raw);
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| io_error("Missing child stdout"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| io_error("Missing child stderr"))?;
    let read = async {
        async fn read_output(
            reader: &mut (impl tokio::io::AsyncRead + Unpin),
        ) -> Result<Vec<u8>, AppInstallError> {
            let mut bytes = Vec::new();
            reader
                .take(MAX_OUTPUT + 1)
                .read_to_end(&mut bytes)
                .await
                .map_err(io_error)?;
            if bytes.len() as u64 > MAX_OUTPUT {
                return Err(failure(
                    AppInstallFailure::Command,
                    "Installer tool output exceeds limit",
                ));
            }
            Ok(bytes)
        }
        let (status, out, err) = tokio::try_join!(
            async { child.wait().await.map_err(io_error) },
            read_output(&mut stdout),
            read_output(&mut stderr),
        )?;
        if !status.success() {
            return Err(failure(
                AppInstallFailure::Command,
                format!(
                    "{} failed ({status}): {}",
                    program.display(),
                    String::from_utf8_lossy(&err)
                ),
            ));
        }
        Ok(Output {
            stdout: out,
            stderr: err,
        })
    };
    let result = control.bounded(name, within, read).await;
    if result.is_err() {
        if let Some(pid) = process_group {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    result
}
pub(super) fn passed(path: &Path) -> Result<Arg, AppInstallError> {
    Confined::new(path).map(Arg::from).map_err(io_error)
}
