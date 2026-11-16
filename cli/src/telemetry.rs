// SPDX-License-Identifier: Apache-2.0

//! The CLI's side of telemetry (docs/guide/telemetry.md): the question in `koi setup`, `koi telemetry ...`, the
//! command and setup-step events of one run, and the daemon's loop that moves the sums to disk and sends them.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use koidrive_driver::{Driver, Paths, Status, Volume};
use koidrive_telemetry::schema::{
    self, Client, Counter, ErrorCode, Failure, Level, Outcome, SetupFlow, SetupStep, Synced,
    magnitude,
};
use koidrive_telemetry::{
    Environment, Event, Hold, InstallId, Mode, Origin, Recorder, Sent, Source, Store, Uploader,
    install_panic_hook, should_ask,
};
use schemars::JsonSchema;
use serde::Serialize;

use crate::error::CliError;
use crate::report::Done;
use crate::terminal::{PromptFailure, Terminal};

/// How often the daemon moves its sums to disk and sends what is due.
const TICK: Duration = Duration::from_secs(60);
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
    /// Send usage reports.
    On,
    /// Stop sending usage reports, and delete the install id and everything not sent yet.
    Off,
    /// Print the reports that wait to be sent, as the daemon would send them. Sends nothing.
    Preview,
    /// Draw a new install id and delete everything not sent yet.
    Reset,
    /// Mark this machine's reports as internal, for machines of people who build Koidrive.
    Internal {
        #[arg(value_enum)]
        state: Switch,
    },
}

/// The recorder of this run, the install id its sums belong to and the flow its setup steps belong to.
struct Run {
    store: Store,
    recorder: Recorder,
    /// The profile's install id at the last flush. Sums recorded under it are dropped when it changed meanwhile.
    id: Mutex<Option<InstallId>>,
    /// The setup flow this run is in, once it is known: `koi setup` learns it from the person's choice.
    flow: Mutex<Option<SetupFlow>>,
    command: schema::Command,
    hold: Mutex<Option<Hold>>,
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
    asks: bool,
) {
    let client = match command {
        schema::Command::Daemon => Client::CliDaemon,
        _ => Client::CliCommand,
    };
    let environment = Environment::detect(client, env!("CARGO_PKG_VERSION"));
    let store = Store::new(&paths.state_dir).recorded_by(Origin {
        koi_version: environment.koi_version,
        client,
    });
    if let Some(switch) = switch
        && let Err(error) = store.set(switch == Switch::On)
    {
        tracing::warn!(%error, "the telemetry choice could not be stored");
    }
    let on = matches!(
        store.status(var).map(|status| status.mode),
        Ok(Mode::On | Mode::Debug)
    );
    let id = on
        .then(|| {
            (store.install_id())
                .inspect_err(
                    |error| tracing::debug!(%error, "the telemetry install id cannot be stored"),
                )
                .ok()
        })
        .flatten();
    let undecided = asks
        && store
            .settings()
            .is_ok_and(|settings| should_ask(var, settings.enabled));
    let hold = undecided
        .then(|| {
            (store.hold())
                .inspect_err(
                    |error| tracing::debug!(%error, "telemetry cannot be kept until the answer"),
                )
                .ok()
                .flatten()
        })
        .flatten();
    let recorder = Recorder::off();
    recorder.record_for(hold.as_ref().map(Hold::id).or(id.as_ref()));
    recorder.count(Counter::Command(command), 1);
    install_panic_hook(store.clone());
    let _ = RUN.set(Run {
        store,
        recorder,
        id: Mutex::new(id),
        flow: Mutex::new(flow),
        command,
        hold: Mutex::new(hold),
    });
}

pub async fn ask(paths: &Paths, terminal: &Terminal) -> Result<(), CliError> {
    let store = Store::new(&paths.state_dir);
    let (asked, answer) = match store.settings() {
        Ok(settings) if should_ask(var, settings.enabled) => match store.begin_prompt() {
            Ok(true) => (true, question(terminal).await),
            Ok(false) => (
                false,
                Ok(store.settings().ok().and_then(|settings| settings.enabled)),
            ),
            Err(error) => {
                tracing::warn!(%error, "the unanswered telemetry question could not be stored");
                (false, Ok(None))
            }
        },
        Ok(settings) => (false, Ok(settings.enabled)),
        Err(error) => {
            tracing::warn!(%error, "the telemetry settings cannot be read");
            (false, Ok(None))
        }
    };
    let choice = answer.as_ref().ok().copied().flatten();
    let stored = match (choice, take_hold()) {
        (Some(on), Some(hold)) if asked || on => store.answer(hold, on).map(|_| on),
        (Some(on), None) if asked => store.set(on).map(|_| on),
        (_, Some(hold)) => store.forget(hold).map(|()| false),
        (_, None) => Ok(false),
    };
    match stored {
        Ok(true) => {
            let _ = tokio::time::timeout(EXIT_SEND, send(&store, Client::CliCommand)).await;
        }
        Ok(false) => {}
        Err(error) => tracing::warn!(%error, "the telemetry choice could not be stored"),
    }
    answer.map(drop)
}

pub fn forget() {
    let Some(run) = RUN.get() else { return };
    if let Some(hold) = take_hold()
        && let Err(error) = run.store.forget(hold)
    {
        tracing::debug!(%error, "kept telemetry could not be deleted");
    }
}

fn take_hold() -> Option<Hold> {
    RUN.get()
        .and_then(|run| run.hold.lock().ok().and_then(|mut hold| hold.take()))
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
    let question = if terminal.setup_guided() {
        terminal.separator();
        terminal.message("Reports include setup steps, usage counts, crash locations, hourly sync totals, OS, and Koidrive version.");
        terminal
            .message("No file names, paths, contents, error messages, server address, or account.");
        terminal.message("Reports carry your IP address, which estimates your country and city.");
        terminal
            .message("Yes also sends the reports of this setup. Preview: koi telemetry preview.");
        "Send usage reports?"
    } else {
        terminal.message("Send usage reports to the Koidrive project?");
        terminal.message("Reports hold setup steps, command and error counts, crash locations, hourly sync totals, and your OS and Koidrive version.");
        terminal.message("They never hold file names, paths, contents, error messages, your server address or your account.");
        terminal.message("They carry your IP address, which estimates your country and city.");
        terminal
            .message("Yes also sends the reports of this command. Preview: koi telemetry preview.");
        "Send reports?"
    };
    match terminal.confirm(question, true).await {
        Ok(answer) => Ok(Some(answer)),
        Err(error @ (PromptFailure::Cancelled | PromptFailure::CancelledPresented)) => {
            Err(error.into())
        }
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
                at: koidrive_telemetry::now(),
                flow,
                step: self.step,
                outcome: self.outcome,
                cloudflare_code: None,
                seconds_magnitude: magnitude(self.started.elapsed().as_secs()),
            });
        }
    }
}

pub async fn finish() {
    let Some(run) = RUN.get() else { return };
    let events_written = run.flush(run.mode());
    let waiting = run.command != schema::Command::Telemetry
        && (run.store.peek()).is_ok_and(|queued| {
            (queued.events.iter())
                .any(|queued| matches!(queued.event, Event::Panic { .. } | Event::AppCrash { .. }))
        });
    if events_written > 0 || waiting {
        let _ = tokio::time::timeout(EXIT_SEND, send(&run.store, Client::CliCommand)).await;
    }
}

pub fn flush() {
    if let Some(run) = RUN.get() {
        run.flush(run.mode());
    }
}

impl Run {
    fn mode(&self) -> Mode {
        (self.store.status(var)).map_or(Mode::Off, |status| status.mode)
    }

    fn flush(&self, mode: Mode) -> usize {
        let pending = self.recorder.take();
        if let Ok(hold) = self.hold.lock()
            && let Some(kept) = hold.as_ref()
        {
            if let Err(error) = self.store.keep(kept, pending) {
                tracing::debug!(%error, "telemetry could not be kept until the answer");
            }
            return 0;
        }
        let on = matches!(mode, Mode::On | Mode::Debug);
        let Ok(mut id) = self.id.lock() else {
            return 0;
        };
        let mut events_written = 0;
        if on && let Some(recorded) = id.as_ref() {
            let events = pending.events.len();
            match self.store.absorb_as(pending, recorded) {
                Ok(()) => events_written = events,
                Err(error) => tracing::debug!(%error, "telemetry could not be written"),
            }
        }
        if let Ok(settings) = self.store.settings() {
            *id = settings.install_id;
        }
        self.recorder.record_for(id.as_ref().filter(|_| on));
        events_written
    }
}

async fn send(store: &Store, client: Client) -> Option<Sent> {
    let mode = store.status(var).ok()?.mode;
    let uploader = Uploader::from_env(var)
        .inspect_err(|error| tracing::debug!(%error, "telemetry cannot be sent"))
        .ok()?;
    let environment = environment(store, client);
    koidrive_telemetry::send(
        store,
        uploader.as_ref(),
        &environment,
        mode,
        koidrive_telemetry::this_hour(),
    )
    .await
    .inspect_err(|error| tracing::debug!(%error, "telemetry could not be read or written"))
    .ok()
}

fn environment(store: &Store, client: Client) -> Environment {
    Environment {
        internal: store.internal(var).unwrap_or_default(),
        ..Environment::detect(client, env!("CARGO_PKG_VERSION"))
    }
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
    let _ = tokio::time::timeout(EXIT_SEND, send(&run.store, Client::CliDaemon)).await;
}

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
            recorder.event(Event::Problem {
                at: koidrive_telemetry::now(),
                problem: failure,
                count: added,
            });
        }
    }
    *last = now;
}

pub fn failed(failure: Failure) {
    if let Some(run) = RUN.get() {
        run.recorder.count(Counter::Failure(failure), 1);
        run.recorder.event(Event::Problem {
            at: koidrive_telemetry::now(),
            problem: failure,
            count: 1,
        });
    }
}

pub fn command_failed(code: koidrive_ipc::Code) {
    use koidrive_ipc::Code;
    let code = match code {
        Code::Failed => ErrorCode::Failed,
        Code::Usage => ErrorCode::Usage,
        Code::DecisionRequired => ErrorCode::DecisionRequired,
        Code::AuthRequired => ErrorCode::AuthRequired,
        Code::Halted => ErrorCode::Halted,
        Code::Unavailable => ErrorCode::Unavailable,
        Code::StaleDecision => ErrorCode::StaleDecision,
        Code::WrongCode => ErrorCode::WrongCode,
        Code::Interrupted => ErrorCode::Interrupted,
    };
    if let Some(run) = RUN.get() {
        run.recorder.event(Event::CommandFailed {
            at: koidrive_telemetry::now(),
            command: run.command,
            code,
        });
    }
}

async fn every<W: Future>(
    period: Duration,
    mut stop: tokio::sync::oneshot::Receiver<()>,
    mut tick: impl FnMut(),
    mut work: impl FnMut() -> W,
) {
    let mut ticks = tokio::time::interval(period);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = ticks.tick() => {}
            _ = &mut stop => break,
        }
        tick();
        tokio::select! {
            _ = work() => {}
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
    waiting_hours: usize,
    waiting_days: usize,
    waiting_events: usize,
    /// Whether this build can send: it has a project token.
    sends: bool,
    /// Whether reports are marked internal.
    internal: bool,
}

#[derive(Serialize, JsonSchema)]
pub(crate) struct InternalData {
    internal: bool,
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
        let human = if error.kind() == std::io::ErrorKind::InvalidData {
            Some("Koidrive could not read saved usage-report data. Run koi diagnose to collect details before changing reporting settings.".to_owned())
        } else {
            crate::error::local_io_human(&error).map(|cause| format!("Koidrive could not read or update usage-report data. {cause} Run koi diagnose to collect details."))
        };
        CliError {
            human,
            ..CliError::from(format!(
                "the telemetry files in {}: {error}",
                paths.state_dir.display()
            ))
        }
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
            let old = store.peek_old().map_err(failed)?;
            let sends = Uploader::from_env(var).ok().flatten().is_some();
            let internal = store.internal(var).map_err(failed)?;
            let mut text = format!("Usage reports: {shown} ({why})\n");
            let mut summary = text.clone();
            if let Some(id) = &settings.install_id {
                text.push_str(&format!("Install id: {id}\n"));
            }
            if internal {
                text.push_str("Reports are marked internal.\n");
            }
            let events = queued.events.len() + old.events.len();
            let waiting = match old.days.len() {
                0 => format!(
                    "Waiting to send: {} hours, {events} events\n",
                    queued.hours.len()
                ),
                days => format!(
                    "Waiting to send: {} hours, {days} days from an earlier version, {events} events\n",
                    queued.hours.len()
                ),
            };
            text.push_str(&waiting);
            summary.push_str(&waiting);
            if status.mode == Mode::On && !sends {
                text.push_str(
                    "This build has no project token, so reports wait on this machine and are not sent.\n",
                );
                summary.push_str(
                    "This build cannot send usage reports. Unsent reports stay on this device.\n",
                );
            }
            let action = if on {
                "Turn off: koi telemetry off\n"
            } else {
                "Turn on: koi telemetry on\n"
            };
            text.push_str(action);
            summary.push_str(action);
            let data = TelemetryStatusData {
                mode,
                source,
                install_id: settings.install_id,
                waiting_hours: queued.hours.len(),
                waiting_days: old.days.len(),
                waiting_events: events,
                sends,
                internal,
            };
            Ok(Done::ok(data, text).human(summary, ""))
        }
        TelemetryAction::On => {
            let settings = store.set(true).map_err(failed)?;
            let data = ChoiceData {
                enabled: true,
                install_id: settings.install_id,
            };
            let override_note = overridden(true)?;
            let text = format!(
                "Usage reports are on. See what is sent with koi telemetry preview.\n{}",
                override_note
            );
            let summary = if override_note.is_empty() {
                "Usage reports are on. Preview them with koi telemetry preview.\n".to_owned()
            } else {
                format!("Saved your choice to send usage reports.\n{override_note}")
            };
            Ok(Done::ok(data, text)
                .human(summary, "")
                .then(&["koi", "telemetry", "preview"]))
        }
        TelemetryAction::Off => {
            store.set(false).map_err(failed)?;
            let data = ChoiceData {
                enabled: false,
                install_id: None,
            };
            let override_note = overridden(false)?;
            let text = format!(
                "Usage reports are off. The install id and the reports not sent yet are deleted.\n{}",
                override_note
            );
            let summary = if override_note.is_empty() {
                "Usage reports are off. Unsent reports were deleted.\n".to_owned()
            } else {
                format!(
                    "Saved your choice not to send usage reports. Unsent reports were deleted.\n{override_note}"
                )
            };
            Ok(Done::ok(data, text).human(summary, ""))
        }
        TelemetryAction::Preview => {
            let queued = store.peek().map_err(failed)?;
            let old = store.peek_old().map_err(failed)?;
            let id = store.settings().map_err(failed)?.install_id;
            let held = store.held().map_err(failed)?;
            let token = Uploader::from_env(var)
                .ok()
                .flatten()
                .map(|uploader| uploader.token().to_owned())
                .unwrap_or_default();
            let environment = environment(&store, Client::CliDaemon);
            let mut bodies = Vec::new();
            if let Some(id) = &id {
                bodies.push(koidrive_telemetry::old_batch(
                    &token,
                    id,
                    &environment,
                    &old,
                    &[],
                ));
                bodies.push(koidrive_telemetry::batch(
                    &token,
                    id,
                    &environment,
                    &queued,
                    &[],
                ));
            }
            for (id, held) in &held {
                bodies.push(koidrive_telemetry::batch(
                    &token,
                    id,
                    &environment,
                    held,
                    &[],
                ));
            }
            let events: Vec<serde_json::Value> = (bodies.iter())
                .filter_map(|body| body["batch"].as_array())
                .flatten()
                .cloned()
                .collect();
            if events.is_empty() {
                return Ok(Done::ok(
                    serde_json::Value::Null,
                    "Nothing waits to be sent.\n",
                ));
            }
            let body = serde_json::json!({ "api_key": token, "historical_migration": false, "batch": events });
            let text = format!(
                "{}\n",
                serde_json::to_string_pretty(&body).map_err(|error| error.to_string())?
            );
            Ok(Done {
                presentation: crate::output::Presentation::Literal,
                ..Done::ok(body, text)
            })
        }
        TelemetryAction::Internal { state } => {
            let internal = store
                .mark_internal(state == Switch::On)
                .map_err(failed)?
                .internal;
            let text = match internal {
                true => "Reports from this machine are marked internal.\n",
                false => "Reports from this machine are not marked internal.\n",
            };
            Ok(Done::ok(InternalData { internal }, text))
        }
        TelemetryAction::Reset => match store.reset().map_err(failed)? {
            Some(id) => {
                let text = format!("New install id: {id}. The reports not sent yet are deleted.\n");
                let data = ChoiceData {
                    enabled: true,
                    install_id: Some(id),
                };
                Ok(Done::ok(data, text)
                    .human("Usage report ID reset. Unsent reports were deleted.\n", ""))
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
    fn the_daemon_records_the_failures_seen_since_its_last_tick_as_sums_and_problem_events() {
        use koidrive_telemetry::schema::{Asked, Halted};

        let recorder = Recorder::on();
        let halted = Failure::Halted(Halted::StoreBehind);
        let asked = Failure::Asked(Asked::Conflict);
        let mut last = BTreeMap::from([(halted, 2)]);
        let now = BTreeMap::from([(halted, 3), (asked, 4)]);
        record_failures(&recorder, &mut last, now.clone());
        record_failures(&recorder, &mut last, now.clone());
        assert_eq!(last, now);
        let taken = recorder.take();
        let hour = taken.hours.into_values().next().unwrap();
        let report = serde_json::to_value(hour).unwrap();
        assert_eq!(
            report["counters"],
            serde_json::json!({
                "asked": { "conflict": 4 },
                "halted": { "store_behind": 1 },
            })
        );
        let problems: Vec<_> = (taken.events.iter())
            .map(|event| {
                let value = serde_json::to_value(event).unwrap();
                (value["problem"].clone(), value["count"].clone())
            })
            .collect();
        assert_eq!(
            problems,
            [
                (
                    serde_json::json!("halted.store_behind"),
                    serde_json::json!(1)
                ),
                (serde_json::json!("asked.conflict"), serde_json::json!(4)),
            ]
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
            progress: None,
            held_disks: Vec::new(),
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
        let day = recorder.take().hours.into_values().next().unwrap();
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
            command: schema::Command::Daemon,
            hold: Mutex::new(None),
        };
        run.recorder
            .count(Counter::Command(schema::Command::Daemon), 1);
        store.set(true).unwrap();
        run.flush(Mode::On);
        run.recorder.count(Counter::Sync(Synced::CommitsSent), 2);
        run.flush(Mode::On);
        let days = serde_json::to_value(store.peek().unwrap().hours).unwrap();
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

    #[test]
    fn a_new_install_id_between_ticks_starts_a_new_collector_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let id = store.set(true).unwrap().install_id;
        let run = Run {
            store: store.clone(),
            recorder: Recorder::off(),
            id: Mutex::new(id),
            flow: Mutex::new(None),
            command: schema::Command::Daemon,
            hold: Mutex::new(None),
        };
        run.flush(Mode::On);
        let changes: [fn(&Store); 2] = [
            |store: &Store| {
                store.set(false).unwrap();
                store.set(true).unwrap();
            },
            |store: &Store| drop(store.reset().unwrap()),
        ];
        for change in changes {
            let before = run.recorder.session();
            run.recorder.count(Counter::Sync(Synced::CommitsSent), 1);
            change(&store);
            run.flush(Mode::On);
            assert!(before.is_some());
            assert_ne!(run.recorder.session(), before);
            assert_eq!(
                run.recorder.install_id(),
                store.settings().unwrap().install_id
            );
        }
        assert!(store.peek().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn events_go_one_tick_after_they_happen_and_reports_one_tick_after_their_hour() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        store.set(true).unwrap();
        let environment = Environment::detect(Client::CliDaemon, "0.1.0");
        let hour = koidrive_telemetry::this_hour();
        let now = Cell::new(hour);
        let queue = |hour: u32| {
            let recorder = Recorder::on();
            recorder.count_on(hour, Counter::Command(schema::Command::Status), 1);
            store.absorb(recorder.take()).unwrap();
        };
        let happen = || {
            let event = Event::Panic {
                at: koidrive_telemetry::now(),
                file: schema::SourceFile::from_location("cli/src/main.rs"),
                line: 1,
                frames: Vec::new(),
            };
            let pending = koidrive_telemetry::Pending {
                events: vec![event],
                ..Default::default()
            };
            store.absorb(pending).unwrap();
        };
        let waiting = || {
            let queued = store.peek().unwrap();
            let hours = (queued.hours.iter())
                .map(|report| report.hour)
                .collect::<Vec<_>>();
            (hours, queued.events.len())
        };
        let daemon = |stop| {
            every(
                TICK,
                stop,
                || {},
                || koidrive_telemetry::send(&store, None, &environment, Mode::Debug, now.get()),
            )
        };
        let settle = || async {
            for _ in 0..20 {
                tokio::task::yield_now().await;
            }
        };

        queue(hour - 1);
        queue(hour);
        let (stop_first, stop) = tokio::sync::oneshot::channel();
        tokio::join!(daemon(stop), async {
            settle().await;
            assert_eq!(waiting(), (vec![hour], 0), "a past hour waited for a tick");
            happen();
            tokio::time::advance(TICK - Duration::from_secs(1)).await;
            settle().await;
            assert_eq!(waiting(), (vec![hour], 1));
            now.set(hour + 1);
            tokio::time::advance(Duration::from_secs(1)).await;
            settle().await;
            assert_eq!(
                waiting(),
                (vec![], 0),
                "the event or the hour waited past a tick"
            );
            stop_first.send(()).unwrap();
        });

        queue(hour + 1);
        now.set(hour + 2);
        let (stop_second, stop) = tokio::sync::oneshot::channel();
        tokio::join!(daemon(stop), async {
            settle().await;
            assert_eq!(waiting(), (vec![], 0), "a restart waited for a tick");
            stop_second.send(()).unwrap();
        });
    }

    #[tokio::test(start_paused = true)]
    async fn ticks_start_a_tick_apart_however_long_a_send_takes() {
        let (stop_daemon, stop) = tokio::sync::oneshot::channel();
        let ticks = Cell::new(0);
        let looping = every(
            TICK,
            stop,
            || ticks.set(ticks.get() + 1),
            || tokio::time::sleep(Duration::from_secs(10)),
        );
        let settle = || async {
            for _ in 0..20 {
                tokio::task::yield_now().await;
            }
        };
        let observer = async {
            settle().await;
            tokio::time::advance(TICK).await;
            settle().await;
            assert_eq!(ticks.get(), 2, "the second tick waited for the send");
            stop_daemon.send(()).unwrap();
        };
        tokio::join!(looping, observer);
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
