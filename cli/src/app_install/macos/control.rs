use crate::app_install::error::{AppInstallError, AppInstallFailure, failure};
use crate::app_install::macos::constants::MAX_OUTPUT;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
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
                "App installation deadline exceeded",
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
        future: impl std::future::Future<Output = Result<T, AppInstallError>>,
    ) -> Result<T, AppInstallError> {
        self.check()?;
        tokio::select! {
            biased;
            _ = self.cancelled() => Err(failure(AppInstallFailure::Cancelled, "App installation cancelled")),
            _ = tokio::time::sleep_until(self.deadline) => Err(failure(AppInstallFailure::Deadline, "App installation deadline exceeded")),
            result = future => result,
        }
    }
}
pub(in crate::app_install) fn active() -> &'static Registry {
    static ACTIVE: OnceLock<Registry> = OnceLock::new();
    ACTIVE.get_or_init(Registry::default)
}
pub(super) fn io_error(error: impl ToString) -> AppInstallError {
    failure(AppInstallFailure::Io, error)
}
pub(super) async fn process(
    program: &Path,
    args: &[OsString],
    control: &Control,
) -> Result<Vec<u8>, AppInstallError> {
    control.check()?;
    let mut child = Command::new(program)
        .args(args)
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
        let mut output = out;
        if !err.is_empty() {
            output.push(b'\n');
            output.extend(err);
        }
        Ok(output)
    };
    let result = control.bounded(read).await;
    if result.is_err() {
        if let Some(pid) = process_group {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    result
}
pub(super) fn args(parts: &[&OsStr]) -> Vec<OsString> {
    parts.iter().map(|part| part.to_os_string()).collect()
}
