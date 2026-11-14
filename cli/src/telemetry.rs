// SPDX-License-Identifier: Apache-2.0

//! The CLI's side of telemetry (docs/guide/telemetry.md): the question in `koi setup`, `koi telemetry ...`, the
//! command and setup-step events of one run, and the daemon's loop that moves the sums to disk and sends them.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use koidrive_driver::{Driver, Paths, Status, Volume};
use koidrive_telemetry::schema::{
    self, Client, Counter, Failure, Level, Outcome, SetupFlow, SetupStep, Synced, magnitude,
};
use koidrive_telemetry::{
    Environment, Event, InstallId, Mode, Recorder, Sent, Source, Store, Uploader,
    install_panic_hook, should_ask,
};
use schemars::JsonSchema;
use serde::Serialize;

use crate::error::CliError;
use crate::report::Done;
use crate::terminal::{PromptFailure, Terminal};

/// How often the daemon moves its sums to disk and sends what is due.
const TICK: Duration = Duration::from_secs(10 * 60);
/// How long a setup command waits at exit to send its events; the daemon sends them later otherwise.
const EXIT_SEND: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Switch {
    On,
    Off,
}

#[derive(clap::Subcommand)]
pub enum TelemetryAction {
    /// Show whether usage reports are sent, why, and what waits to be sent.
    Status,
    /// Send anonymous usage reports.
    On,
    /// Stop sending usage reports, and delete the install id and everything not sent yet.
    Off,
    /// Print the reports that wait to be sent, as the daemon would send them. Sends nothing.
    Preview,
    /// Draw a new install id and delete everything not sent yet.
    Reset,
}

/// The recorder of this run, the install id its sums belong to and the flow its setup steps belong to.
struct Run {
    store: Store,
    recorder: Recorder,
    /// The profile's install id at the last flush. Sums recorded under it are dropped when it changed meanwhile.
    id: Mutex<Option<InstallId>>,
    /// The setup flow this run is in, once it is known: `koi setup` learns it from the person's choice.
    flow: Mutex<Option<SetupFlow>>,
}

static RUN: OnceLock<Run> = OnceLock::new();

pub fn recorder() -> Recorder {
    RUN.get()
        .map_or_else(Recorder::off, |run| run.recorder.clone())
}

fn var(name: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(name)
}

pub async fn start(
    paths: &Paths,
    switch: Option<Switch>,
    command: schema::Command,
    flow: Option<SetupFlow>,
) {
    let store = Store::new(&paths.state_dir);
    if let Some(switch) = switch
        && let Err(error) = store.set(switch == Switch::On)
    {
        tracing::warn!(%error, "the telemetry choice could not be stored");
    }
    let on = matches!(
        store.status(var).map(|status| status.mode),
        Ok(Mode::On | Mode::Debug)
    );
    let recorder = if on { Recorder::on() } else { Recorder::off() };
    recorder.count(Counter::Command(command), 1);
    let id = on
        .then(|| {
            (store.install_id())
                .inspect_err(
                    |error| tracing::debug!(%error, "the telemetry install id cannot be stored"),
                )
                .ok()
        })
        .flatten();
    install_panic_hook(store.clone());
    let _ = RUN.set(Run {
        store,
        recorder,
        id: Mutex::new(id),
        flow: Mutex::new(flow),
    });
}

pub async fn ask(paths: &Paths, terminal: &Terminal) -> Result<(), CliError> {
    let store = Store::new(&paths.state_dir);
    match store.settings() {
        Ok(settings) if should_ask(var, settings.enabled) => {
            match store.begin_prompt() {
                Ok(true) => {}
                Ok(false) => return Ok(()),
                Err(error) => {
                    tracing::warn!(%error, "the unanswered telemetry question could not be stored");
                    return Ok(());
                }
            }
            if let Some(on) = question(terminal).await?
                && let Err(error) = store.set(on)
            {
                tracing::warn!(%error, "the telemetry choice could not be stored");
            }
        }
        Ok(_) => {}
        Err(error) => tracing::warn!(%error, "the telemetry settings cannot be read"),
    }
    Ok(())
}

pub fn prompt_pending(paths: &Paths) -> bool {
    match Store::new(&paths.state_dir).prompt_pending() {
        Ok(pending) => pending,
        Err(error) => {
            tracing::warn!(%error, "the unanswered telemetry question cannot be read");
            false
        }
    }
}

/// Sets the flow this run's setup steps belong to, for `koi setup` once the person chose one.
pub fn set_flow(flow: SetupFlow) {
    if let Some(run) = RUN.get()
        && let Ok(mut current) = run.flow.lock()
    {
        *current = Some(flow);
    }
}

fn flow() -> Option<SetupFlow> {
    RUN.get()
        .and_then(|run| run.flow.lock().ok().and_then(|flow| *flow))
}

async fn question(terminal: &Terminal) -> Result<Option<bool>, CliError> {
    terminal.message("Send anonymous usage reports to the Koidrive project?");
    terminal.message("Reports hold command and error counts, crash locations, daily sync totals, and your OS and Koidrive version.");
    terminal.message("They never hold file names, paths, contents, error messages, your server address or your account.");
    terminal.message("Preview: koi telemetry preview. Turn off: koi telemetry off.");
    match terminal.confirm("Send reports?", true).await {
        Ok(answer) => Ok(Some(answer)),
        Err(PromptFailure::Cancelled) => Err(PromptFailure::Cancelled.into()),
        Err(_) => Ok(None),
    }
}

/// Runs one setup step and records how it ended and how long it took. A step dropped before it ends, by Ctrl-C or
/// a signal, is recorded as aborted.
pub async fn step<T, E>(step: SetupStep, work: impl Future<Output = Result<T, E>>) -> Result<T, E> {
    let mut running = Running {
        step,
        started: Instant::now(),
        outcome: Outcome::Aborted,
    };
    let result = work.await;
    running.outcome = if result.is_ok() {
        Outcome::Ok
    } else {
        Outcome::Failed
    };
    result
}

/// A step in progress, which records itself when it is dropped.
struct Running {
    step: SetupStep,
    started: Instant,
    outcome: Outcome,
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(run) = RUN.get()
            && let Some(flow) = flow()
        {
            run.recorder.event(Event::SetupStep {
                day: koidrive_telemetry::today(),
                flow,
                step: self.step,
                outcome: self.outcome,
                cloudflare_code: None,
                seconds_magnitude: magnitude(self.started.elapsed().as_secs()),
            });
        }
    }
}

/// Moves what this run recorded to disk. A setup command also tries to send it, for at most two seconds.
pub async fn finish() {
    let Some(run) = RUN.get() else { return };
    flush();
    if flow().is_some() && run.recorder.is_on() {
        let _ = tokio::time::timeout(EXIT_SEND, send(&run.store, Client::CliCommand)).await;
    }
}

pub fn flush() {
    let Some(run) = RUN.get() else { return };
    run.flush(
        run.store
            .status(var)
            .map_or(Mode::Off, |status| status.mode),
    );
}

impl Run {
    fn flush(&self, mode: Mode) {
        let on = matches!(mode, Mode::On | Mode::Debug);
        let pending = self.recorder.take();
        self.recorder.switch(on);
        let Ok(mut id) = self.id.lock() else { return };
        if on
            && let Some(recorded) = id.as_ref()
            && let Err(error) = self.store.absorb_as(pending, recorded)
        {
            tracing::debug!(%error, "telemetry could not be written");
        }
        if let Ok(settings) = self.store.settings() {
            *id = settings.install_id;
        }
    }
}

async fn send(store: &Store, client: Client) -> Option<Sent> {
    let mode = store.status(var).ok()?.mode;
    let uploader = Uploader::from_env(var)
        .inspect_err(|error| tracing::debug!(%error, "telemetry cannot be sent"))
        .ok()?;
    let environment = Environment::detect(client, env!("CARGO_PKG_VERSION"));
    koidrive_telemetry::send(
        store,
        uploader.as_ref(),
        &environment,
        mode,
        koidrive_telemetry::today(),
    )
    .await
    .inspect_err(|error| tracing::debug!(%error, "telemetry could not be read or written"))
    .ok()
}

pub async fn run_daemon(stop: tokio::sync::oneshot::Receiver<()>, driver: Driver) {
    let Some(run) = RUN.get() else { return };
    let mut last = Volume::default();
    let mut failed = BTreeMap::new();
    let mut cpu = koidrive_telemetry::Cpu::default();
    cpu.sample(&run.recorder);
    let tick = || {
        cpu.sample(&run.recorder);
        record_sync(
            &run.recorder,
            &mut last,
            koidrive_driver::volume(),
            &driver.status(),
        );
        record_failures(&run.recorder, &mut failed, koidrive_driver::failures());
        flush();
    };
    every(TICK, stop, tick, || send(&run.store, Client::CliDaemon)).await;
}

/// Adds what was synced between `last` and `now`, and keeps the linked spaces and their nodes as readings of the day.
fn record_sync(recorder: &Recorder, last: &mut Volume, now: Volume, status: &Status) {
    let totals = [
        (Synced::CommitsSent, now.commits_sent, last.commits_sent),
        (
            Synced::CommitsReceived,
            now.commits_received,
            last.commits_received,
        ),
        (
            Synced::BytesUploaded,
            now.bytes_uploaded,
            last.bytes_uploaded,
        ),
        (
            Synced::BytesDownloaded,
            now.bytes_downloaded,
            last.bytes_downloaded,
        ),
    ];
    for (synced, now, before) in totals {
        let added = now.saturating_sub(before);
        if added > 0 {
            recorder.count(Counter::Sync(synced), added);
        }
    }
    *last = now;
    if status.folders.is_empty() {
        return;
    }
    recorder.level(Level::Spaces, status.folders.len() as u64);
    let nodes = status
        .folders
        .iter()
        .map(|folder| folder.remote_nodes)
        .sum();
    recorder.level(Level::Nodes, nodes);
}

fn record_failures(
    recorder: &Recorder,
    last: &mut BTreeMap<Failure, u64>,
    now: BTreeMap<Failure, u64>,
) {
    for (&failure, &count) in &now {
        let added = count.saturating_sub(last.get(&failure).copied().unwrap_or(0));
        if added > 0 {
            recorder.count(Counter::Failure(failure), added);
        }
    }
    *last = now;
}

pub fn failed(failure: Failure) {
    if let Some(run) = RUN.get() {
        run.recorder.count(Counter::Failure(failure), 1);
    }
}

async fn every<W: Future>(
    period: Duration,
    mut stop: tokio::sync::oneshot::Receiver<()>,
    mut tick: impl FnMut(),
    mut work: impl FnMut() -> W,
) {
    loop {
        tick();
        tokio::select! {
            _ = work() => {}
            _ = &mut stop => break,
        }
        tokio::select! {
            _ = tokio::time::sleep(period) => {}
            _ = &mut stop => break,
        }
    }
    tick();
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum TelemetryMode {
    On,
    Off,
    Debug,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum TelemetrySource {
    DoNotTrack,
    Variable,
    Choice,
    Unset,
}

/// `koi telemetry ...`'s result as JSON data.
#[derive(Serialize, JsonSchema)]
pub(crate) struct TelemetryStatusData {
    mode: TelemetryMode,
    /// What decided the mode.
    source: TelemetrySource,
    #[schemars(with = "Option<String>")]
    install_id: Option<InstallId>,
    waiting_days: usize,
    waiting_events: usize,
    /// Whether this build can send: it has a project token.
    sends: bool,
}

#[derive(Serialize, JsonSchema)]
pub(crate) struct ChoiceData {
    enabled: bool,
    #[schemars(with = "Option<String>")]
    install_id: Option<InstallId>,
}

/// `koi telemetry ...`.
pub fn command(paths: &Paths, action: TelemetryAction) -> Result<Done, CliError> {
    let store = Store::new(&paths.state_dir);
    let failed = |error: std::io::Error| {
        CliError::from(format!(
            "the telemetry files in {}: {error}",
            paths.state_dir.display()
        ))
    };
    let overridden = |choice: bool| -> Result<&'static str, CliError> {
        let status = store.status(var).map_err(failed)?;
        Ok(match (status.mode != Mode::Off) == choice {
            true => "",
            false if status.source == Source::DoNotTrack => {
                "DO_NOT_TRACK keeps reports off until it is unset.\n"
            }
            false => "KOIDRIVE_TELEMETRY overrides this choice until it is unset.\n",
        })
    };
    match action {
        TelemetryAction::Status => {
            let settings = store.settings().map_err(failed)?;
            let status = store.status(var).map_err(failed)?;
            let on = status.mode != Mode::Off;
            let (source, why) = match status.source {
                Source::DoNotTrack => (TelemetrySource::DoNotTrack, "DO_NOT_TRACK is set"),
                Source::Variable => (TelemetrySource::Variable, "KOIDRIVE_TELEMETRY is set"),
                Source::Stored => (TelemetrySource::Choice, "your choice"),
                Source::Unset => (TelemetrySource::Unset, "not asked yet"),
            };
            let (mode, shown) = match status.mode {
                Mode::Off => (TelemetryMode::Off, "off"),
                Mode::On => (TelemetryMode::On, "on"),
                Mode::Debug => (
                    TelemetryMode::Debug,
                    "debug (printed to stderr, never sent)",
                ),
            };
            let queued = store.peek().map_err(failed)?;
            let sends = Uploader::from_env(var).ok().flatten().is_some();
            let mut text = format!("Usage reports: {shown} ({why})\n");
            if let Some(id) = &settings.install_id {
                text.push_str(&format!("Install id: {id}\n"));
            }
            text.push_str(&format!(
                "Waiting to send: {} days, {} events\n",
                queued.days.len(),
                queued.events.len()
            ));
            if status.mode == Mode::On && !sends {
                text.push_str(
                    "This build has no project token, so reports wait on this machine and are not sent.\n",
                );
            }
            text.push_str(if on {
                "Turn off: koi telemetry off\n"
            } else {
                "Turn on: koi telemetry on\n"
            });
            let data = TelemetryStatusData {
                mode,
                source,
                install_id: settings.install_id,
                waiting_days: queued.days.len(),
                waiting_events: queued.events.len(),
                sends,
            };
            Ok(Done::ok(data, text))
        }
        TelemetryAction::On => {
            let settings = store.set(true).map_err(failed)?;
            let data = ChoiceData {
                enabled: true,
                install_id: settings.install_id,
            };
            let text = format!(
                "Usage reports are on. See what is sent with koi telemetry preview.\n{}",
                overridden(true)?
            );
            Ok(Done::ok(data, text).then(&["koi", "telemetry", "preview"]))
        }
        TelemetryAction::Off => {
            store.set(false).map_err(failed)?;
            let data = ChoiceData {
                enabled: false,
                install_id: None,
            };
            let text = format!(
                "Usage reports are off. The install id and the reports not sent yet are deleted.\n{}",
                overridden(false)?
            );
            Ok(Done::ok(data, text))
        }
        TelemetryAction::Preview => {
            let queued = store.peek().map_err(failed)?;
            let id = store.settings().map_err(failed)?.install_id;
            let (Some(id), false) = (id, queued.is_empty()) else {
                return Ok(Done::ok(
                    serde_json::Value::Null,
                    "Nothing waits to be sent.\n",
                ));
            };
            let token = Uploader::from_env(var)
                .ok()
                .flatten()
                .map(|uploader| uploader.token().to_owned())
                .unwrap_or_default();
            let environment = Environment::detect(Client::CliDaemon, env!("CARGO_PKG_VERSION"));
            let body = koidrive_telemetry::batch(&token, &id, &environment, &queued, &[]);
            let text = format!(
                "{}\n",
                serde_json::to_string_pretty(&body).map_err(|error| error.to_string())?
            );
            Ok(Done::ok(body, text))
        }
        TelemetryAction::Reset => match store.reset().map_err(failed)? {
            Some(id) => {
                let text = format!("New install id: {id}. The reports not sent yet are deleted.\n");
                let data = ChoiceData {
                    enabled: true,
                    install_id: Some(id),
                };
                Ok(Done::ok(data, text))
            }
            None => {
                let data = ChoiceData {
                    enabled: false,
                    install_id: None,
                };
                let text = "Usage reports are off, so there is no install id to reset.\n";
                Ok(Done::ok(data, text))
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn the_daemon_records_the_failures_counted_since_its_last_tick() {
        use koidrive_telemetry::schema::{Asked, Halted};

        let recorder = Recorder::on();
        let halted = Failure::Halted(Halted::StoreBehind);
        let asked = Failure::Asked(Asked::Conflict);
        let mut last = BTreeMap::from([(halted, 2)]);
        let now = BTreeMap::from([(halted, 3), (asked, 4)]);
        record_failures(&recorder, &mut last, now.clone());
        record_failures(&recorder, &mut last, now.clone());
        assert_eq!(last, now);
        let day = recorder.take().days.into_values().next().unwrap();
        let report = serde_json::to_value(day).unwrap();
        assert_eq!(
            report["counters"],
            serde_json::json!({
                "asked": { "conflict": 4 },
                "halted": { "store_behind": 1 },
            })
        );
    }

    #[test]
    fn the_daemon_records_what_was_synced_since_its_last_tick() {
        let recorder = Recorder::on();
        let folder = |remote_nodes| koidrive_driver::FolderStatus {
            recovery: None,
            space_id: "s".into(),
            space_name: "Alice Notes".into(),
            path: "/home/alice/Notes".into(),
            state: koidrive_driver::FolderState::UpToDate,
            history_id: None,
            last_seq: None,
            head_seq: None,
            remote_nodes,
            local_entries: None,
            questions: 0,
            problem: None,
            wait: None,
        };
        let status = Status {
            account: None,
            running: true,
            folders: vec![folder(1_000), folder(500)],
            ..Status::default()
        };
        let mut last = Volume {
            bytes_uploaded: 100,
            ..Volume::default()
        };
        let now = Volume {
            commits_sent: 3,
            bytes_uploaded: 4_196,
            ..Volume::default()
        };
        record_sync(&recorder, &mut last, now, &status);
        record_sync(
            &recorder,
            &mut last,
            now,
            &Status {
                folders: vec![],
                ..status
            },
        );
        assert_eq!(last, now);
        let day = recorder.take().days.into_values().next().unwrap();
        let report = serde_json::to_value(day).unwrap();
        assert_eq!(
            report["counters"]["sync"],
            serde_json::json!({ "commits_sent": 3, "bytes_uploaded": 4_096 })
        );
        assert_eq!(
            report["levels"]["sync"],
            serde_json::json!({ "spaces": 2, "nodes": 1_500 })
        );
        let text = report.to_string();
        for private in ["Alice", "/home"] {
            assert!(!text.contains(private), "{private} in {text}");
        }
    }

    #[test]
    fn a_daemon_started_before_reports_were_on_records_from_its_next_tick() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let run = Run {
            store: store.clone(),
            recorder: Recorder::off(),
            id: Mutex::new(None),
            flow: Mutex::new(None),
        };
        run.recorder
            .count(Counter::Command(schema::Command::Daemon), 1);
        store.set(true).unwrap();
        run.flush(Mode::On);
        run.recorder.count(Counter::Sync(Synced::CommitsSent), 2);
        run.flush(Mode::On);
        let days = serde_json::to_value(store.peek().unwrap().days).unwrap();
        assert_eq!(
            days[0]["counters"],
            serde_json::json!({ "sync": { "commits_sent": 2 } })
        );

        store.set(false).unwrap();
        run.flush(Mode::Off);
        run.recorder.count(Counter::Sync(Synced::CommitsSent), 1);
        store.set(true).unwrap();
        run.flush(Mode::On);
        run.flush(Mode::On);
        assert!(store.peek().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn reports_go_as_the_daemon_starts_and_one_tick_after_their_day_ends() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        store.set(true).unwrap();
        let environment = Environment::detect(Client::CliDaemon, "0.1.0");
        let day = koidrive_telemetry::today();
        let today = Cell::new(day);
        let queue = |day: u32| {
            let recorder = Recorder::on();
            recorder.count_on(day, Counter::Command(schema::Command::Status), 1);
            store.absorb(recorder.take()).unwrap();
        };
        let waiting = || {
            (store.peek().unwrap().days.iter())
                .map(|report| report.day)
                .collect::<Vec<_>>()
        };
        let daemon = |stop| {
            every(
                TICK,
                stop,
                || {},
                || koidrive_telemetry::send(&store, None, &environment, Mode::Debug, today.get()),
            )
        };
        let settle = || async {
            for _ in 0..20 {
                tokio::task::yield_now().await;
            }
        };

        queue(day - 1);
        queue(day);
        let (stop_first, stop) = tokio::sync::oneshot::channel();
        tokio::join!(daemon(stop), async {
            settle().await;
            assert_eq!(waiting(), [day], "a past day waited for a tick");
            today.set(day + 1);
            tokio::time::advance(TICK - Duration::from_secs(1)).await;
            settle().await;
            assert_eq!(waiting(), [day]);
            tokio::time::advance(Duration::from_secs(1)).await;
            settle().await;
            assert_eq!(waiting(), Vec::<u32>::new(), "the day ended a tick ago");
            stop_first.send(()).unwrap();
        });

        queue(day);
        let (stop_second, stop) = tokio::sync::oneshot::channel();
        tokio::join!(daemon(stop), async {
            settle().await;
            assert_eq!(waiting(), Vec::<u32>::new(), "a restart waited for a tick");
            stop_second.send(()).unwrap();
        });
    }

    #[tokio::test]
    async fn stopping_drops_a_send_in_progress_and_flushes_once_more() {
        let (stop_daemon, stop) = tokio::sync::oneshot::channel();
        let (sending, sent) = tokio::sync::oneshot::channel();
        let mut sending = Some(sending);
        let ticks = Cell::new(0);
        let looping = every(
            Duration::from_millis(1),
            stop,
            || ticks.set(ticks.get() + 1),
            || {
                if let Some(sending) = sending.take() {
                    let _ = sending.send(());
                }
                std::future::pending::<()>()
            },
        );
        let stopper = async {
            sent.await.unwrap();
            stop_daemon.send(()).unwrap();
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(looping, stopper)
        })
        .await
        .expect("stopping waited for the send");
        assert_eq!(ticks.get(), 2);
    }
}
