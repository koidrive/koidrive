use super::*;

pub(in crate::app_install) fn active() -> &'static Registry {
    static ACTIVE: OnceLock<Registry> = OnceLock::new();
    ACTIVE.get_or_init(Registry::default)
}

#[cfg(test)]
pub(super) async fn ensure_with(
    config: Config,
    target: Version,
    launch: AppLaunch,
    deadline: Instant,
) -> Result<AppInstall, AppInstallError> {
    ensure_registered(config, target, launch, deadline, &Registry::default()).await
}

pub(in crate::app_install) async fn ensure_registered(
    config: Config,
    target: Version,
    launch: AppLaunch,
    deadline: Instant,
    registry: &Registry,
) -> Result<AppInstall, AppInstallError> {
    let deadline = deadline.min(Instant::now() + Duration::from_secs(300));
    if deadline <= Instant::now() {
        return Err(failure(
            AppInstallFailure::Deadline,
            "App installation deadline exceeded",
        ));
    }
    let (tx, rx) = watch::channel(false);
    let registration = registry.register(tx.clone());
    let guard = CancelOnDrop(tx);
    let task = tokio::spawn(async move {
        let _registration = registration;
        let control = Control {
            deadline,
            cancel: rx,
        };
        transaction(config, target, launch, control, deadline).await
    });
    let result = task
        .await
        .map_err(|error| failure(AppInstallFailure::Io, error))?;
    drop(guard);
    result
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
