// SPDX-License-Identifier: Apache-2.0

use std::path::Path;
use std::time::{Duration, Instant};

use koidrive_cloudflare::{Jurisdiction, Script, Secret};
use koidrive_driver::Paths;
use koidrive_ipc::Code;
use koidrive_proto::v1::ServerInfo;
use koidrive_telemetry::schema::SetupStep;
use url::Url;

use super::claim::Claim;
use super::embedded_bundle;
use super::remote::{server_info, wait_for_setup_attempts, wait_until_serving};
use super::session::{confirm, open_browser};
use super::state::{SetupState, StateFile, Step};
use super::{Bound, Cloud, Found, Session, TAG};
use crate::error::CliError;

/// Cloudflare's error code for an account without an R2 subscription (section 4.1).
const R2_DISABLED: u64 = 10042;
const R2_WAIT: Duration = Duration::from_secs(30 * 60);

/// Why a new deploy or claim must not replace `state`, the state file of a run in `account` or another one that has
/// not finished: the file is that run's only record of what it created.
pub(super) fn unfinished(state: &SetupState, account: &str, file: &Path) -> CliError {
    let resource = if state.step == Step::Uninstalling && state.script.is_empty() {
        format!(
            "the bucket {} in the {} jurisdiction",
            state.bucket,
            state.jurisdiction.as_str()
        )
    } else {
        state.script.clone()
    };
    let run = if state.account != account {
        format!(
            "a setup of {} in another Cloudflare account ({}) has not finished; run koi setup and log in to that account to finish it",
            resource, state.account
        )
    } else if state.step == Step::Uninstalling {
        format!(
            "the delete of {} has not finished; run koi server delete to finish it",
            resource
        )
    } else {
        format!(
            "the deploy of {} has not finished; run koi setup to finish it",
            resource
        )
    };
    let human = if state.account != account {
        format!(
            "Setup of {} in another Cloudflare account has not finished. Run koi setup and sign in to the account that created that server to finish it",
            resource
        )
    } else {
        run.clone()
    };
    CliError::from(format!("{run}, or delete {} to forget it", file.display())).human(format!(
        "{human}. Keep the saved setup progress until the operation is complete."
    ))
}

impl Session {
    async fn require_subdomain(&mut self) -> Result<(), CliError> {
        let account = self.account().to_owned();
        let base = workers_subdomain_name(&self.account.name);
        let mut candidate = base.clone();
        for _ in 0..5 {
            match self.fresh().await?.workers_subdomain(&account).await {
                Ok(subdomain) => {
                    self.subdomain = Some(subdomain);
                    return Ok(());
                }
                Err(error) if error.has_code(10007) => {}
                Err(error) => {
                    return Err(CliError::from(error)
                        .context("cannot read the account's workers.dev subdomain"));
                }
            }
            match self
                .fresh()
                .await?
                .create_workers_subdomain(&account, &candidate)
                .await
            {
                Ok(subdomain) => {
                    self.terminal
                        .message(&format!("Using {subdomain}.workers.dev"));
                    self.subdomain = Some(subdomain);
                    return Ok(());
                }
                Err(error) if error.has_code(10031) => {
                    let mut bytes = [0u8; 4];
                    getrandom::fill(&mut bytes)
                        .expect("the operating system provides random bytes");
                    let suffix: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
                    let prefix = base[..base.len().min(54)].trim_end_matches('-');
                    candidate = format!("{prefix}-{suffix}");
                }
                Err(error) => {
                    return Err(CliError::from(error)
                        .context("cannot create the account's workers.dev subdomain"));
                }
            }
        }
        Err("could not find an available workers.dev subdomain; run setup again".into())
    }

    /// Waits until R2 is enabled on the account. R2 cannot be enabled through the API, so the user completes the
    /// dashboard's checkout (section 4.1).
    async fn require_r2(&mut self) -> Result<(), CliError> {
        let started = Instant::now();
        let mut told = false;
        let terminal = self.terminal.clone();
        let mut progress = terminal
            .setup_guided()
            .then(|| terminal.continue_progress("Checking storage..."));
        loop {
            match self
                .fresh()
                .await?
                .buckets(self.account(), Jurisdiction::Default)
                .await
            {
                Ok(_) => {
                    if told && let Some(progress) = progress.take() {
                        progress.finish("R2 is ready");
                    }
                    return Ok(());
                }
                Err(error) if error.has_code(R2_DISABLED) => {
                    if !told {
                        let page = Url::parse(&format!(
                            "https://dash.cloudflare.com/{}/r2/overview",
                            self.account()
                        ))
                        .expect("the R2 page is a URL");
                        if self.terminal.setup_guided() {
                            drop(progress.take());
                            terminal.separator();
                            self.terminal
                                .message("Complete the R2 checkout in your browser to continue.");
                            self.terminal.message(
                                "R2 needs a payment method on your Cloudflare account. Usage within the free tier costs nothing.",
                            );
                            let opening = terminal.progress("Opening your browser...");
                            let opened = open_browser(&page).await;
                            drop(opening);
                            if !opened {
                                self.terminal
                                    .message("Couldn't open your browser. Open this page:");
                                self.terminal.message(page.as_str());
                            }
                            progress = Some(terminal.progress("Waiting for R2..."));
                        } else {
                            println!(
                                "R2 is not enabled on this account. Koidrive keeps your files in R2."
                            );
                            println!(
                                "R2 needs a payment method on your Cloudflare account. Usage within the free tier costs nothing.\nEnable it on this page:\n{page}"
                            );
                            open_browser(&page).await;
                            println!("Waiting for R2...");
                        }
                        told = true;
                    }
                    if started.elapsed() > R2_WAIT {
                        return Err("R2 was not enabled within 30 minutes; run the command again once it is".into());
                    }
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
                Err(error) => {
                    return Err(CliError::caused(
                        format!("cannot list the account's R2 buckets: {error}"),
                        error,
                    ));
                }
            }
        }
    }

    /// The state file of an unfinished deploy in this account, if there is one. Any other run that has not finished
    /// stops the command here, before it calls the account: its file is that run's only record of what it created.
    fn resumable(&self, paths: &Paths) -> Result<Option<SetupState>, CliError> {
        let file = StateFile::new(paths);
        match file.load()? {
            Some(state) if self.continues(&state) => Ok(Some(state)),
            Some(other) => Err(unfinished(&other, self.account(), file.path())),
            None => Ok(None),
        }
    }

    /// Whether `state` is an unfinished deploy in this session's account, which setup goes on with.
    fn continues(&self, state: &SetupState) -> bool {
        state.account == self.account() && state.step != Step::Uninstalling
    }

    /// `koi server deploy` after the login: finish an interrupted deploy, or deploy a new server into an account
    /// that has none.
    pub(super) async fn server_deploy(
        &mut self,
        paths: &Paths,
        device_name: &str,
    ) -> Result<(), CliError> {
        if let Some(state) = self.resumable(paths)? {
            return self.finish_deploy(paths, state, device_name).await;
        }
        crate::telemetry::step(SetupStep::R2Checkout, self.require_r2()).await?;
        let found = self.find_servers().await?;
        if let Some(server) = found.first() {
            return Err(format!(
                "the account already has a Koidrive server, {} at {}; run koi server add-device",
                server.script.id, server.url
            )
            .into());
        }
        self.deploy(paths, device_name).await
    }

    /// `koi server add-device` after the login: add this device to the named server, or to the only one.
    pub(super) async fn server_add_device(
        &mut self,
        paths: &Paths,
        server: Option<&str>,
        device_name: &str,
    ) -> Result<(), CliError> {
        if let (Some(state), Some(name)) = (self.resumable(paths)?, server)
            && state.script != name
        {
            return Err(unfinished(
                &state,
                self.account(),
                StateFile::new(paths).path(),
            ));
        }
        let found = self.pick(server).await?;
        self.add_device(paths, found, device_name, false).await
    }

    /// `koi setup` after the login: finish an interrupted deploy, deploy a new server, or add this device.
    pub(super) async fn setup(&mut self, paths: &Paths, device_name: &str) -> Result<(), CliError> {
        if let Some(state) = self.resumable(paths)? {
            self.terminal.message("Continuing your server setup.");
            return self.finish_deploy(paths, state, device_name).await;
        }
        crate::telemetry::step(SetupStep::R2Checkout, self.require_r2()).await?;
        let terminal = self.terminal.clone();
        let progress = terminal.progress("Looking for your server...");
        let mut found = self.find_servers().await?;
        let server = match found.len() {
            0 => {
                drop(progress);
                return self.deploy(paths, device_name).await;
            }
            1 => {
                progress.finish("Found your server");
                found.remove(0)
            }
            _ => {
                drop(progress);
                let choices = found
                    .iter()
                    .map(|server| {
                        server
                            .url
                            .host_str()
                            .unwrap_or(server.script.id.as_str())
                            .to_owned()
                    })
                    .collect();
                let index = self
                    .terminal
                    .select("Which server should this device use?", choices)
                    .await?;
                found.remove(index)
            }
        };
        self.add_device(paths, server, device_name, true).await
    }

    /// Picks a free Worker name and a new bucket name, then deploys and claims a new server (sections 4.3 and 4.4).
    async fn deploy(&mut self, paths: &Paths, device_name: &str) -> Result<(), CliError> {
        let terminal = self.terminal.clone();
        let progress = terminal
            .setup_guided()
            .then(|| terminal.continue_progress("Preparing storage..."));
        let bundle = embedded_bundle()?;
        let api = self.fresh().await?;
        let scripts: Vec<String> = api
            .scripts(self.account())
            .await
            .map_err(|error| CliError::from(error).context("cannot list the account's Workers"))?
            .into_iter()
            .map(|script| script.id)
            .collect();
        let base = &bundle.manifest.default_bucket;
        let state = SetupState {
            account: self.account().to_owned(),
            script: free_name(base, &scripts),
            bucket: new_bucket_name(base),
            ..SetupState::default()
        };
        StateFile::new(paths).save(&state)?;
        drop(progress);
        self.finish_deploy(paths, state, device_name).await
    }

    /// Runs the deploy steps after `state.step` and claims the server. Every step is safe to repeat.
    async fn finish_deploy(
        &mut self,
        paths: &Paths,
        mut state: SetupState,
        device_name: &str,
    ) -> Result<(), CliError> {
        let file = StateFile::new(paths);
        let bundle = embedded_bundle()?;
        self.require_subdomain().await?;
        let mut key = None;
        let terminal = self.terminal.clone();
        let progress = terminal
            .setup_guided()
            .then(|| terminal.progress("Preparing storage..."));
        if state.step < Step::BucketCreated {
            if !terminal.setup_guided() {
                println!("Creating the R2 bucket {}...", state.bucket);
            }
            crate::telemetry::step(SetupStep::Bucket, create_bucket(self, &file, &mut state))
                .await?;
        }
        if state.step < Step::ScriptUploaded {
            keep_script_name(
                self,
                &mut state,
                &bundle.manifest.r2_binding,
                &bundle.manifest.default_bucket,
            )
            .await?;
            if let Some(progress) = &progress {
                progress.update("Deploying your server...");
            } else {
                println!(
                    "Uploading Koidrive {} as the Worker {}...",
                    bundle.manifest.version, state.script
                );
            }
            let secret = Secret::random(32);
            state.secret_set = true;
            file.save(&state)?;
            let bucket = Bound {
                name: state.bucket.clone(),
                jurisdiction: state.jurisdiction,
            };
            let upload = self.upload(&bundle, &state.script, &bucket, None, Some(&secret));
            crate::telemetry::step(SetupStep::Script, upload).await?;
            state.step = Step::ScriptUploaded;
            file.save(&state)?;
            key = Some(secret);
        }
        let url = self.worker_url(&state.script).await?;
        if let Some(progress) = &progress {
            progress.update("Waiting for your server...");
        }
        if state.step < Step::Serving {
            crate::telemetry::step(SetupStep::Serving, self.serve(&state.script, &url)).await?;
            state.step = Step::Serving;
            file.save(&state)?;
        }
        let server = Found {
            script: Script {
                id: state.script.clone(),
                created_on: None,
                tags: vec![TAG.to_owned()],
                migration_tag: None,
                has_assets: true,
            },
            bucket: Some(Bound {
                name: state.bucket.clone(),
                jurisdiction: state.jurisdiction,
            }),
            info: server_info(&url).await,
            url,
        };
        if let Some(progress) = progress {
            progress.finish("Server created");
        }
        let _progress = terminal
            .setup_guided()
            .then(|| terminal.progress("Connecting this device..."));
        let claim =
            Claim::new(self, paths, &server, device_name, state, key).run(Duration::from_secs(60));
        crate::telemetry::step(SetupStep::Claim, claim).await
    }

    /// Adds this device to a server that already exists (section 4.5). `interactive` lets it finish a deploy that
    /// stopped before the server answered, and offer an upgrade when the server's claim code does not handle setup
    /// attempts; otherwise it stops and names the command that does.
    pub(super) async fn add_device(
        &mut self,
        paths: &Paths,
        mut server: Found,
        device_name: &str,
        interactive: bool,
    ) -> Result<(), CliError> {
        let file = StateFile::new(paths);
        let saved = match file.load()? {
            Some(state) if self.continues(&state) && state.script == server.script.id => {
                Some(state)
            }
            Some(other) => return Err(unfinished(&other, self.account(), file.path())),
            None => None,
        };
        let info = match server.info.clone() {
            Some(info) => info,
            None if interactive => {
                let terminal = self.terminal.clone();
                let progress = if terminal.setup_guided() {
                    Some(terminal.progress("Finishing your server setup..."))
                } else {
                    println!(
                        "{} stopped halfway through its deploy. Finishing it...",
                        server.script.id
                    );
                    None
                };
                let info = self.serve(&server.script.id, &server.url).await?;
                if let Some(progress) = progress {
                    progress.finish("Server is ready");
                }
                server.info = Some(info.clone());
                info
            }
            None => {
                return Err(format!(
                    "{} stopped halfway through its deploy; run koi setup to finish it",
                    server.script.id
                )
                .into());
            }
        };
        let mut window = Duration::from_secs(60);
        if !info.setup_attempts {
            let reason = if self.terminal.setup_guided() {
                format!(
                    "Your server needs Koidrive {} before it can connect this device.",
                    embedded_bundle()?.manifest.version
                )
            } else {
                format!(
                    "{} runs Koidrive {}, which cannot make a retried claim end with one device. It needs an upgrade to {} first.",
                    server.script.id,
                    info.server_version,
                    embedded_bundle()?.manifest.version
                )
            };
            if !interactive {
                return Err(
                    format!("{reason} Run koi server upgrade, then this command again.").into(),
                );
            }
            if let Err(error) = confirm(
                &self.terminal,
                &format!("{reason} Upgrade it now?"),
                false,
                "--yes",
            )
            .await
            {
                if error.code == Code::Interrupted {
                    return Err(error);
                }
                return Err(
                    format!("{reason} Run koi server upgrade, then koi setup again.").into(),
                );
            }
            self.upgrade(&embedded_bundle()?, &server, false, false)
                .await?;
            let terminal = self.terminal.clone();
            let _progress = if terminal.setup_guided() {
                Some(terminal.progress("Waiting for your server..."))
            } else {
                println!(
                    "Waiting for the server's storage to move to the new version (up to 330 seconds)..."
                );
                None
            };
            wait_for_setup_attempts(&server.url).await?;
            window = Duration::from_secs(330);
        }
        if !info.claimed {
            window = Duration::from_secs(330);
        }
        let state = match saved {
            Some(state) => state,
            None => SetupState {
                account: self.account().to_owned(),
                script: server.script.id.clone(),
                bucket: server
                    .bucket
                    .as_ref()
                    .map(|bucket| bucket.name.clone())
                    .unwrap_or_default(),
                jurisdiction: server
                    .bucket
                    .as_ref()
                    .map(|bucket| bucket.jurisdiction)
                    .unwrap_or_default(),
                step: Step::Serving,
                ..SetupState::default()
            },
        };
        let terminal = self.terminal.clone();
        let _progress = terminal
            .setup_guided()
            .then(|| terminal.progress("Connecting this device..."));
        let claim = Claim::new(self, paths, &server, device_name, state, None).run(window);
        crate::telemetry::step(SetupStep::Claim, claim).await
    }

    /// Turns on the Worker's workers.dev route and waits until the server answers there.
    async fn serve(&mut self, script: &str, url: &Url) -> Result<ServerInfo, CliError> {
        self.fresh()
            .await?
            .enable_workers_dev(self.account(), script)
            .await
            .map_err(|error| {
                CliError::from(error).context(format!("cannot turn on workers.dev for {script}"))
            })?;
        wait_until_serving(url, None).await
    }
}

pub(super) fn workers_subdomain_name(account: &str) -> String {
    let mut name = String::new();
    for c in account.chars().map(|c| c.to_ascii_lowercase()) {
        if c.is_ascii_alphanumeric() || c == '-' {
            name.push(c);
        } else if !name.ends_with('-') {
            name.push('-');
        }
    }
    let name = name.trim_start_matches('-');
    let name = name[..name.len().min(63)].trim_end_matches('-');
    if name.is_empty() {
        "koidrive".to_owned()
    } else {
        name.to_owned()
    }
}

/// Creates the bucket `state.bucket` and records the step. `deploy` saved that name, which only this deploy produces, before
/// the first try, so a 409 for it means an earlier try of this deploy created the bucket and its answer was lost.
pub(super) async fn create_bucket(
    cloud: &mut impl Cloud,
    file: &StateFile,
    state: &mut SetupState,
) -> Result<(), CliError> {
    let account = cloud.account().to_owned();
    match cloud
        .fresh()
        .await?
        .create_bucket(&account, &state.bucket, state.jurisdiction)
        .await
    {
        Ok(()) => {}
        Err(error) if error.status() == Some(409) => {}
        Err(error) => {
            return Err(CliError::caused(
                format!("cannot create the bucket {}: {error}", state.bucket),
                error,
            ));
        }
    }
    state.step = Step::BucketCreated;
    file.save(state)
}

/// A bucket name only this deploy produces: `base`, a hyphen and 16 random hex digits (64 bits). Another
/// application cannot hold it unless it guesses it, so the deploy never takes over someone else's bucket.
pub(super) fn new_bucket_name(base: &str) -> String {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).expect("the operating system provides random bytes");
    let suffix: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{base}-{suffix}")
}

/// Keeps `state.script` while no other application holds the name. A Worker under it that has the tag and binds
/// `state.bucket` is this deploy's own upload from before the state file recorded it, so the upload goes over it. Any
/// other Worker under the name belongs to someone else, so the deploy moves to the first free name after `base`.
pub(super) async fn keep_script_name(
    cloud: &mut impl Cloud,
    state: &mut SetupState,
    binding: &str,
    base: &str,
) -> Result<(), CliError> {
    let api = cloud.fresh().await?;
    let account = cloud.account().to_owned();
    let scripts = api
        .scripts(&account)
        .await
        .map_err(|error| CliError::from(error).context("cannot list the account's Workers"))?;
    let Some(current) = scripts.iter().find(|script| script.id == state.script) else {
        return Ok(());
    };
    if current.tags.iter().any(|tag| tag == TAG) {
        let settings = api
            .script_settings(&account, &state.script)
            .await
            .map_err(|error| {
                CliError::caused(
                    format!("cannot read the settings of {}: {error}", state.script),
                    error,
                )
            })?;
        if settings.r2_bucket(binding).is_some_and(|bound| {
            bound.bucket_name.as_deref() == Some(state.bucket.as_str())
                && bound.jurisdiction.unwrap_or_default() == state.jurisdiction
        }) {
            return Ok(());
        }
    }
    let taken: Vec<String> = scripts.into_iter().map(|script| script.id).collect();
    let free = free_name(base, &taken);
    println!(
        "Another application now has a Worker named {}, so the server becomes the Worker {free}.",
        state.script
    );
    state.script = free;
    Ok(())
}

/// `base`, or the first of `base-2`, `base-3`, ... that is not taken.
pub(super) fn free_name(base: &str, taken: &[String]) -> String {
    std::iter::once(base.to_owned())
        .chain((2..).map(|n| format!("{base}-{n}")))
        .find(|name| !taken.contains(name))
        .expect("some name is free")
}
