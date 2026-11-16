// SPDX-License-Identifier: Apache-2.0

use crate::error::CliError;
use crate::report::{Done, FolderData, Lifecycle};
use crate::{ipc, notify, report, telemetry, update};
use koidrive_driver::{Driver, DriverError, FolderState, FolderStatus, Paths};
use koidrive_telemetry::schema::{DaemonStart, Failure};
use tokio::signal::unix::{SignalKind, signal};

/// The sync daemon, until SIGTERM or SIGINT. With `json` it prints one JSON object per line on stdout as it starts,
/// as a folder changes state, as it stops and once it stopped, and its logs on stderr are JSON lines too.
pub(crate) async fn daemon(paths: Paths, json: bool) -> Result<Done, CliError> {
    let emit = |event: Lifecycle| {
        if json {
            println!("{}", event.line());
        }
    };
    let failed = |why| telemetry::failed(Failure::DaemonStart(why));
    let driver = daemon_driver(paths).await.map_err(|error| match error {
        DriverError::Busy(_) => {
            failed(DaemonStart::Busy);
            CliError::from("another Koidrive daemon or command is using this profile")
        }
        other => {
            failed(DaemonStart::Profile);
            CliError::from(other)
        }
    })?;
    let socket = driver.paths().socket();
    let listener = koidrive_ipc::bind(&socket).map_err(|error| {
        failed(DaemonStart::Socket);
        format!("cannot listen on {}: {error}", socket.display())
    })?;
    let signals = signal(SignalKind::terminate()).and_then(|terminate| {
        signal(SignalKind::interrupt()).map(|interrupt| (terminate, interrupt))
    });
    let (mut terminate, mut interrupt) = signals.map_err(|error| {
        failed(DaemonStart::Signals);
        error.to_string()
    })?;
    let notifications = notify::decisions(driver.clone());
    driver.start();
    driver.check_for_updates();
    let newest = tokio::spawn(update::follow_newest(driver.paths().clone()));
    let server = tokio::spawn(ipc::serve(listener, driver.clone()));
    let notifications = tokio::spawn(notifications);
    let (stop_telemetry, telemetry_stopped) = tokio::sync::oneshot::channel();
    let telemetry = tokio::spawn(telemetry::run_daemon(telemetry_stopped, driver.clone()));
    let watch = driver.watch();
    let folders = driver.status().folders;
    tracing::info!(
        socket = %socket.display(),
        version = env!("CARGO_PKG_VERSION"),
        folders = folders.len(),
        "the daemon is running"
    );
    emit(Lifecycle::Started {
        version: env!("CARGO_PKG_VERSION"),
        socket: socket.display().to_string(),
        folders: folders.len(),
    });
    let states = json.then(|| tokio::spawn(folder_states(driver.clone(), watch, folders)));
    let (signal, name) = tokio::select! {
        _ = terminate.recv() => (report::Signal::Term, "SIGTERM"),
        _ = interrupt.recv() => (report::Signal::Int, "SIGINT"),
    };
    tracing::info!(signal = name, "stopping the daemon");
    if let Some(states) = states {
        states.abort();
        let _ = states.await;
    }
    emit(Lifecycle::Stopping { signal });
    server.abort();
    notifications.abort();
    newest.abort();
    let _ = std::fs::remove_file(&socket);
    driver.stop().await;
    let _ = stop_telemetry.send(());
    let _ = telemetry.await;
    tracing::info!("the daemon stopped");
    emit(Lifecycle::Stopped);
    Ok(Done::printed())
}

pub(crate) async fn daemon_driver(paths: Paths) -> Result<Driver, DriverError> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match Driver::open_with_telemetry(paths.clone(), telemetry::recorder()) {
            Err(DriverError::Busy(_))
                if tokio::time::Instant::now() < deadline
                    && tokio::net::UnixStream::connect(paths.socket())
                        .await
                        .is_err() =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            opened => return opened,
        }
    }
}

/// Prints a `folder_state` line whenever a folder's state differs from the one last printed for it. `watch` was
/// subscribed before `folders` was read, so a change in between still wakes the loop.
async fn folder_states(
    driver: Driver,
    mut watch: tokio::sync::watch::Receiver<()>,
    folders: Vec<FolderStatus>,
) {
    let mut shown: std::collections::HashMap<String, FolderState> = (folders.into_iter())
        .map(|folder| (folder.path, folder.state))
        .collect();
    while watch.changed().await.is_ok() {
        for folder in driver.status().folders {
            if shown.get(&folder.path) == Some(&folder.state) {
                continue;
            }
            shown.insert(folder.path.clone(), folder.state);
            let data = FolderData::of(&folder);
            let event = Lifecycle::FolderState {
                folder: data.path,
                space_id: data.space_id,
                state: data.state,
            };
            println!("{}", event.line());
        }
    }
}
