// SPDX-License-Identifier: Apache-2.0

use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use koidrive_cloudflare::{Account, CLIENT_ID, Cloudflare, Endpoints, Login, Loopback};
use koidrive_ipc::Code;
use koidrive_telemetry::schema::SetupStep;
use tokio::signal::unix::{SignalKind, signal};
use url::Url;

#[cfg(feature = "test-setup")]
use super::testing;
use super::{Cloud, LoginArgs, SESSION, Session};
use crate::error::CliError;
use crate::printable;
use crate::terminal::{Terminal, read_line};

const API_BASE: &str = "https://api.cloudflare.com/client/v4/";

const LOGIN_WAIT: Duration = Duration::from_secs(10 * 60);
/// Refresh the access token when it has less than this left, before a step that may take long.
const TOKEN_MARGIN: Duration = Duration::from_secs(5 * 60);

/// Logs in, runs `work`, and revokes the grant however `work` ends: success, failure, Ctrl-C or SIGTERM. After a
/// signal it exits at once with 130 for Ctrl-C or 143 for SIGTERM, as a shell does for a command the signal ended.
pub(super) async fn with_session(
    args: &LoginArgs,
    terminal: Arc<Terminal>,
    verbose: bool,
    work: impl AsyncFnOnce(&mut Session) -> Result<(), CliError>,
) -> Result<(), CliError> {
    let mut terminate = signal(SignalKind::terminate()).map_err(|error| error.to_string())?;
    let mut interrupt = signal(SignalKind::interrupt()).map_err(|error| error.to_string())?;
    let mut status = None;
    let stopped = async || {
        let (how, code) = tokio::select! {
            _ = terminate.recv() => ("stopped by SIGTERM", 143),
            _ = interrupt.recv() => ("stopped", 130),
        };
        status = Some(code);
        how
    };
    SESSION.store(true, Ordering::Relaxed);
    let login = crate::telemetry::step(SetupStep::Login, Session::login(args, terminal.clone()));
    let outcome = guarded(login, stopped, work).await;
    terminal.cancel().await;
    SESSION.store(false, Ordering::Relaxed);
    if let (Err(error), Some(code)) = (&outcome, status) {
        crate::telemetry::flush();
        let message = if !verbose && crate::output::concise("setup") {
            error.human.as_deref().unwrap_or(&error.message)
        } else {
            &error.message
        };
        crate::output::print(
            "setup",
            crate::output::Presentation::Text,
            crate::output::Tone::Attention,
            "",
            crate::output::Note::Error(&format!("{}\n", printable(message))),
            None,
        );
        std::process::exit(code);
    }
    outcome
}

/// Runs `work` on the session `login` returns, then revokes its grant. The grant exists once `login` returns, so
/// everything after that, the account lookup included, ends with the revocation, also when `stopped` fires.
pub(super) async fn guarded(
    login: impl Future<Output = Result<Session, CliError>>,
    mut stopped: impl AsyncFnMut() -> &'static str,
    work: impl AsyncFnOnce(&mut Session) -> Result<(), CliError>,
) -> Result<(), CliError> {
    let mut session = tokio::select! {
        session = login => session?,
        how = stopped() => return Err(CliError::from(format!("{how} before the login finished"))
            .human("Stopped before sign-in finished. Run this command again when you are ready.")),
    };
    let outcome = tokio::select! {
        outcome = async {
            session.pick_account().await?;
            work(&mut session).await
        } => outcome,
        how = stopped() => Err(CliError::from(format!("{how}; run the same command again to continue"))
            .human("Stopped. Run the same command again to continue.")),
    };
    let terminal = session.terminal.clone();
    let _progress = terminal
        .setup_guided()
        .then(|| terminal.continue_progress("Connecting this device..."));
    session.end().await;
    outcome
}

pub(crate) async fn read_code(code: Option<String>, code_stdin: bool) -> Result<String, CliError> {
    if !code_stdin {
        return Ok(code.expect("clap requires a code or --code-stdin"));
    }
    let code = read_answer(|| read_line(std::io::stdin().lock())).await?;
    if code.is_empty() {
        return Err(CliError::usage("stdin contains no code"));
    }
    Ok(code)
}

pub(super) async fn read_answer(
    read: impl FnOnce() -> std::io::Result<String> + Send + 'static,
) -> Result<String, CliError> {
    let (sender, answer) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = sender.send(read());
    });
    let line = answer
        .await
        .map_err(|_| "the answer could not be read".to_owned())?
        .map_err(|error| error.to_string())?;
    Ok(line.trim().to_owned())
}

pub(super) async fn confirm(
    terminal: &Terminal,
    question: &str,
    yes: bool,
    flag: &str,
) -> Result<(), CliError> {
    if yes {
        return Ok(());
    }
    if !terminal.guided() {
        return Err(
            format!("{question} Run again with {flag} to go on without a terminal.").into(),
        );
    }
    match terminal.confirm(question, false).await? {
        true => Ok(()),
        false => Err("stopped; nothing more was changed".into()),
    }
}

/// Opens `url` in the default browser, without a shell. A failure only means the user opens it by hand.
pub(super) async fn open_browser(url: &Url) -> bool {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let opened = tokio::process::Command::new(opener)
        .arg(url.as_str())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .status();
    tokio::time::timeout(Duration::from_secs(5), opened)
        .await
        .is_ok_and(|result| result.is_ok_and(|status| status.success()))
}

impl Cloud for Session {
    fn account(&self) -> &str {
        &self.account.id
    }

    /// Refreshes the access token when it is about to expire. A refresh that fails ends the command, because a retry
    /// with the old refresh token would revoke the grant (section 2).
    async fn fresh(&mut self) -> Result<Cloudflare, CliError> {
        if self.grant.expires_within(TOKEN_MARGIN) {
            self.grant.refresh(&self.http).await.map_err(|error| {
                CliError::caused(
                    format!("{error}; run the command again to log in again"),
                    error,
                )
            })?;
        }
        Ok(self.api())
    }
}

impl Session {
    /// Logs in with Cloudflare and returns once the code is exchanged for a grant, before any other call.
    async fn login(args: &LoginArgs, terminal: Arc<Terminal>) -> Result<Session, CliError> {
        #[cfg(feature = "test-setup")]
        let testing = testing::config()?;
        let http = koidrive_cloudflare::http_client().map_err(|error| error.to_string())?;
        #[cfg(feature = "test-setup")]
        let http = if testing.is_some() {
            reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(10))
                .build()
                .map_err(|error| error.to_string())?
        } else {
            http
        };
        let loopback = if args.no_browser {
            None
        } else {
            Some(Loopback::bind(args.port).await?)
        };
        let port = match &loopback {
            Some(loopback) => loopback.port(),
            None => args.port.unwrap_or(8976),
        };
        let endpoints = Endpoints::cloudflare();
        #[cfg(feature = "test-setup")]
        let endpoints = match &testing {
            Some(testing) => Endpoints {
                authorize: testing.base.join("oauth2/auth").unwrap(),
                token: testing.base.join("oauth2/token").unwrap(),
                revoke: testing.base.join("oauth2/revoke").unwrap(),
            },
            None => endpoints,
        };
        let login = Login::new(endpoints, CLIENT_ID, port);
        let page = login.authorize_url();
        let code = match &loopback {
            Some(loopback) => {
                if terminal.setup_guided() {
                    let progress = terminal.progress("Opening your browser...");
                    let opened = open_browser(&page).await;
                    drop(progress);
                    if !opened {
                        terminal.separator();
                        terminal.message("Couldn't open your browser. Open this page:");
                        terminal.message(page.as_str());
                    }
                } else {
                    terminal.message("Log in with Cloudflare in your browser. If it does not open, open this page:");
                    terminal.message(page.as_str());
                    open_browser(&page).await;
                }
                let _progress = terminal
                    .setup_guided()
                    .then(|| terminal.progress("Waiting for Cloudflare login..."));
                tokio::time::timeout(LOGIN_WAIT, loopback.callback(&login))
                    .await
                    .map_err(|_| {
                        CliError::new(
                            Code::AuthRequired,
                            "the login did not finish within 10 minutes",
                        )
                    })??
            }
            None => {
                if terminal.setup_guided() {
                    terminal.separator();
                }
                terminal.message(if terminal.setup_guided() {
                    "Open this page on any device:"
                } else {
                    "Open this page on any device, log in with Cloudflare and select Authorize:"
                });
                terminal.message(page.as_str());
                terminal.message(if terminal.setup_guided() {
                    "After you authorize, copy the address of the page that fails to load."
                } else {
                    "The browser then fails to load a page on 127.0.0.1. Copy that page's address."
                });
                let pasted = terminal
                    .input(
                        if terminal.setup_guided() {
                            "Callback address"
                        } else {
                            "Address"
                        },
                        true,
                    )
                    .await?;
                let url = Url::parse(&pasted)
                    .map_err(|error| CliError::usage(format!("that is not an address: {error}")))?;
                login.code_from(&url)?
            }
        };
        let _progress = terminal.setup_guided().then(|| {
            if args.no_browser {
                terminal.progress("Waiting for Cloudflare login...")
            } else {
                terminal.continue_progress("Waiting for Cloudflare login...")
            }
        });
        let grant = login.exchange(&http, code).await?;
        drop(_progress);
        let api_base = Url::parse(API_BASE).expect("the API base is a URL");
        #[cfg(feature = "test-setup")]
        let api_base = testing
            .map(|testing| testing.base.join("client/v4/").unwrap())
            .unwrap_or(api_base);
        Ok(Session {
            terminal,
            http,
            grant,
            api_base,
            account: Account {
                id: String::new(),
                name: String::new(),
            },
            subdomain: None,
        })
    }

    /// Reads the one account the user picked on the consent page.
    async fn pick_account(&mut self) -> Result<(), CliError> {
        let work = async {
            self.api()
                .accounts()
                .await
                .map_err(|error| CliError::from(error).context("cannot list the account"))
        };
        let accounts = if self.terminal.setup_guided() {
            let _progress = self
                .terminal
                .continue_progress("Waiting for Cloudflare login...");
            work.await?
        } else {
            self.terminal
                .wait("Checking your Cloudflare account...", work)
                .await?
        };
        match <[Account; 1]>::try_from(accounts) {
            Ok([account]) => self.account = account,
            Err(accounts) => {
                return Err(format!(
                    "the login gave access to {} accounts; pick exactly one on the consent page",
                    accounts.len()
                )
                .into());
            }
        }
        if self.terminal.setup_guided() {
            self.terminal
                .complete(&format!("Signed in to {}", self.account.name));
        } else {
            self.terminal.message(&format!(
                "Logged in to the Cloudflare account {}.",
                printable(&self.account.name)
            ));
        }
        Ok(())
    }

    /// Revokes the grant, retrying three times. A failure leaves the grant active until its tokens expire, so the user
    /// is told where to revoke it.
    async fn end(self) {
        self.terminal.cancel().await;
        for attempt in 0..4 {
            match self.grant.revoke(&self.http).await {
                Ok(()) => return,
                Err(error) if attempt == 3 => {
                    eprintln!(
                        "koi: could not revoke the Cloudflare login ({error}). It stays active; its access token expires within an hour. Revoke it now under Manage OAuth authorizations in the Cloudflare dashboard."
                    );
                }
                Err(_) => tokio::time::sleep(Duration::from_secs(2)).await,
            }
        }
    }

    pub(super) fn api(&self) -> Cloudflare {
        Cloudflare::new(
            self.http.clone(),
            self.api_base.clone(),
            self.grant.access_token().clone(),
        )
    }
}
