// SPDX-License-Identifier: Apache-2.0

use std::io::IsTerminal;
use std::sync::Arc;

use koidrive_cloudflare::{Bundle, Secret, Storage, UploadPlan};
use koidrive_driver::Paths;
use koidrive_ipc::{Code, Next};
use koidrive_proto::v1::ServerInfo;
use koidrive_telemetry::schema::SetupStep;
use url::Url;

use super::bundle::bundled;
use super::embedded_bundle;
use super::onboarding::not_set_up;
use super::remote::{server_info, wait_until_serving};
use super::remove::finish_uninstall_service;
use super::session::{confirm, with_session};
use super::state::{StateFile, Step, UninstallProgress};
#[cfg(feature = "test-setup")]
use super::testing;
use super::{Bound, Cloud, Found, LoginArgs, ServerAction, Session, TAG};
use crate::error::CliError;
use crate::report::Done;
use crate::terminal::Terminal;

/// The Durable Object classes that identify an untagged Koidrive Worker, such as a renamed button deploy.
const SERVER_CLASSES: [&str; 2] = ["AccountDO", "SpaceDO"];

impl ServerAction {
    pub(crate) fn login(&self) -> Option<&LoginArgs> {
        match self {
            ServerAction::Deploy { login, .. }
            | ServerAction::AddDevice { login, .. }
            | ServerAction::Upgrade { login, .. }
            | ServerAction::Delete { login, .. } => Some(login),
            ServerAction::Claim { .. } => None,
        }
    }

    /// The same command with `--interactive`, for a person to run on a terminal. It keeps every argument that picks
    /// what the command acts on, so that it reaches the same Worker, bucket and jurisdiction, and drops `--yes`, so
    /// that the person is asked.
    fn interactive_argv(&self) -> Vec<String> {
        let (name, worker, bucket, jurisdiction, device) = match self {
            ServerAction::Deploy { device, .. } => ("deploy", None, None, None, Some(device)),
            ServerAction::AddDevice { worker, device, .. } => {
                ("add-device", worker.as_deref(), None, None, Some(device))
            }
            ServerAction::Upgrade { worker, .. } => {
                ("upgrade", worker.as_deref(), None, None, None)
            }
            ServerAction::Delete {
                worker,
                bucket,
                jurisdiction,
                ..
            } => (
                "delete",
                worker.as_deref(),
                bucket.as_deref(),
                Some(jurisdiction.as_str()).filter(|name| *name != "default"),
                None,
            ),
            ServerAction::Claim { .. } => ("claim", None, None, None, None),
        };
        let mut argv = Next::of(&["koi", "server", name]).argv;
        let port = self
            .login()
            .and_then(|login| login.port)
            .map(|port| port.to_string());
        let named = [
            ("--worker", worker),
            ("--bucket", bucket),
            ("--jurisdiction", jurisdiction),
            ("--name", device.and_then(|device| device.name.as_deref())),
        ];
        for (flag, value) in named {
            if let Some(value) = value {
                argv.extend([flag.to_owned(), value.to_owned()]);
            }
        }
        if matches!(
            self,
            ServerAction::Upgrade {
                allow_downgrade: true,
                ..
            }
        ) {
            argv.push("--allow-downgrade".to_owned());
        }
        argv.push("--interactive".to_owned());
        if self.login().is_some_and(|login| login.no_browser) {
            argv.push("--no-browser".to_owned());
        }
        if let Some(port) = port {
            argv.extend(["--port".to_owned(), port]);
        }
        argv
    }
}

pub async fn server(
    paths: &Paths,
    action: ServerAction,
    json: bool,
    verbose: bool,
    terminal: Arc<Terminal>,
) -> Result<Done, CliError> {
    let login = action.login().expect("main sends a claim to the driver");
    if login.interactive && json {
        return Err(CliError::usage("--interactive cannot be used with --json"));
    }
    let attended = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    if !(login.interactive && attended) {
        let argv = action.interactive_argv();
        let words: Vec<&str> = argv.iter().map(String::as_str).collect();
        let message = format!(
            "this command logs in with Cloudflare in a browser; a person runs {} on a terminal",
            argv.join(" ")
        );
        return Err(CliError::new(Code::AuthRequired, message).then(&words));
    }
    match action {
        ServerAction::Deploy { device, login } => {
            not_set_up(paths)?;
            bundled()?;
            let name = device.name();
            with_session(&login, terminal.clone(), verbose, async |session| {
                session.server_deploy(paths, &name).await
            })
            .await?;
        }
        ServerAction::AddDevice {
            worker,
            device,
            login,
        } => {
            not_set_up(paths)?;
            bundled()?;
            let name = device.name();
            with_session(&login, terminal.clone(), verbose, async |session| {
                session
                    .server_add_device(paths, worker.as_deref(), &name)
                    .await
            })
            .await?;
        }
        ServerAction::Claim { .. } => unreachable!("main sends a claim to the driver"),
        ServerAction::Upgrade {
            worker,
            yes,
            allow_downgrade,
            login,
        } => {
            bundled()?;
            with_session(&login, terminal.clone(), verbose, async |session| {
                let found = session.pick(worker.as_deref()).await?;
                let bundle = embedded_bundle()?;
                let upgrade = session.upgrade(&bundle, &found, yes, allow_downgrade);
                crate::telemetry::step(SetupStep::Upgrade, upgrade).await
            })
            .await?;
        }
        ServerAction::Delete {
            worker,
            bucket,
            jurisdiction,
            yes,
            login,
        } => {
            let file = StateFile::new(paths);
            let saved = file.load()?;
            if saved.as_ref().is_some_and(|state| {
                state.step == Step::Uninstalling
                    && (bucket.is_none()
                        || state.script.is_empty()
                            && bucket
                                .as_deref()
                                .is_some_and(|bucket| state.matches_bucket(bucket, jurisdiction)))
                    && matches!(
                        state.uninstall,
                        Some(
                            UninstallProgress::Service { .. } | UninstallProgress::Complete { .. }
                        )
                    )
            }) {
                finish_uninstall_service(paths, &file, saved.unwrap()).await?;
                return Ok(Done::printed());
            }
            let resuming = saved.is_some_and(|state| state.step == Step::Uninstalling);
            if bucket.is_none() && !resuming {
                bundled()?;
            }
            with_session(&login, terminal.clone(), verbose, async |session| {
                let uninstall = session.uninstall(
                    paths,
                    worker.as_deref(),
                    bucket.as_deref(),
                    jurisdiction,
                    yes,
                );
                crate::telemetry::step(SetupStep::Remove, uninstall).await
            })
            .await?;
        }
    }
    Ok(Done::printed())
}

impl Session {
    async fn subdomain(&mut self) -> Result<String, CliError> {
        if let Some(subdomain) = &self.subdomain {
            return Ok(subdomain.clone());
        }
        let subdomain = self
            .api()
            .workers_subdomain(self.account())
            .await
            .map_err(|error| {
                CliError::from(error).context("cannot read the account's workers.dev subdomain")
            })?;
        self.subdomain = Some(subdomain.clone());
        Ok(subdomain)
    }

    pub(super) async fn worker_url(&mut self, script: &str) -> Result<Url, CliError> {
        let subdomain = self.subdomain().await?;
        #[cfg(feature = "test-setup")]
        if let Some(testing) = testing::config()? {
            return Ok(testing.base.join(&format!("workers/{script}/")).unwrap());
        }
        Url::parse(&format!("https://{script}.{subdomain}.workers.dev/")).map_err(|error| {
            CliError::from(format!(
                "{script} does not make a workers.dev address: {error}"
            ))
        })
    }

    /// The Koidrive servers in the account (section 6): scripts with the tag, and untagged scripts that bind the
    /// Koidrive Durable Object classes, each checked with `GET /v1/server`. A tagged script that binds a bucket but does
    /// not answer is a deploy that stopped halfway, and is kept without `info`.
    pub(super) async fn find_servers(&mut self) -> Result<Vec<Found>, CliError> {
        let binding = embedded_bundle()?.manifest.r2_binding;
        let api = self.fresh().await?;
        let scripts = api
            .scripts(self.account())
            .await
            .map_err(|error| CliError::from(error).context("cannot list the account's Workers"))?;
        let mut found = Vec::new();
        for script in scripts {
            let settings = api
                .script_settings(self.account(), &script.id)
                .await
                .map_err(|error| {
                    CliError::caused(
                        format!("cannot read the settings of {}: {error}", script.id),
                        error,
                    )
                })?;
            let tagged = script.tags.iter().any(|tag| tag == TAG);
            let binds = |class: &str| {
                settings.bindings.iter().any(|bound| {
                    bound.kind == "durable_object_namespace"
                        && bound.class_name.as_deref() == Some(class)
                })
            };
            if !tagged && !SERVER_CLASSES.iter().all(|class| binds(class)) {
                continue;
            }
            let url = self.worker_url(&script.id).await?;
            let info = server_info(&url).await;
            let bucket = settings.r2_bucket(&binding).and_then(|bound| {
                Some(Bound {
                    name: bound.bucket_name.clone()?,
                    jurisdiction: bound.jurisdiction.unwrap_or_default(),
                })
            });
            if info.is_some() || (tagged && bucket.is_some()) {
                found.push(Found {
                    script,
                    bucket,
                    url,
                    info,
                });
            }
        }
        Ok(found)
    }

    /// The one server a command acts on: the named one, or the only one.
    pub(super) async fn pick(&mut self, name: Option<&str>) -> Result<Found, CliError> {
        let mut found = self.find_servers().await?;
        if let Some(name) = name {
            found.retain(|server| server.script.id == name);
        }
        match found.len() {
            0 => Err(CliError::from(match name {
                Some(name) => format!("the account has no Koidrive server named {name}"),
                None => "the account has no Koidrive server; run koi server deploy".into(),
            })),
            1 => Ok(found.remove(0)),
            _ => Err(format!(
                "the account has several Koidrive servers; name one with --worker: {}",
                found
                    .iter()
                    .map(|server| server.script.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
            .into()),
        }
    }

    pub(super) async fn upgrade(
        &mut self,
        bundle: &Bundle,
        server: &Found,
        yes: bool,
        allow_downgrade: bool,
    ) -> Result<(), CliError> {
        let Some(info) = &server.info else {
            return Err(format!(
                "{} does not answer; run koi setup to finish its deploy",
                server.script.id
            )
            .into());
        };
        let manifest = &bundle.manifest;
        if !allow_downgrade
            && let Some(downgrade) = downgrade(info, &manifest.version, manifest.storage)
        {
            let (id, version) = (&server.script.id, &info.server_version);
            let reason = match downgrade {
                Downgrade::Storage { server, bundle } => format!(
                    "{id} stores data in a newer format than the Koidrive {} in this koi reads (storage version {}.{}, against {}.{}), and that release would stop serving it; install a newer koi, or",
                    manifest.version, server.account, server.space, bundle.account, bundle.space
                ),
                Downgrade::Release => format!(
                    "{id} runs Koidrive {version}, newer than the {} in this koi; install a newer koi, or",
                    manifest.version
                ),
                Downgrade::Unknown => format!(
                    "{id} runs Koidrive {version}, a build that does not report its storage version, so this koi cannot tell whether its Koidrive {} is older;",
                    manifest.version
                ),
            };
            return Err(format!(
                "{reason} run koi server upgrade --allow-downgrade to upload it anyway"
            )
            .into());
        }
        if !server.script.tags.iter().any(|tag| tag == TAG) {
            confirm(
                &self.terminal,
                &format!(
                    "{} was deployed with the deploy button. A push to the repository the button created deploys that repository's code again and replaces this upgrade. Update that repository too, or disconnect its builds in the dashboard. Upgrade now?",
                    server.script.id
                ),
                yes,
                "--yes",
            ).await?;
        }
        let bucket = server
            .bucket
            .clone()
            .ok_or_else(|| format!("{} binds no R2 bucket", server.script.id))?;
        let terminal = self.terminal.clone();
        let progress = if terminal.setup_guided() {
            Some(terminal.progress("Updating your server..."))
        } else {
            println!(
                "Uploading Koidrive {} over {} {}...",
                bundle.manifest.version, server.script.id, info.server_version
            );
            None
        };
        self.upload(
            bundle,
            &server.script.id,
            &bucket,
            server.script.migration_tag.as_deref(),
            None,
        )
        .await?;
        wait_until_serving(&server.url, Some(&bundle.manifest.version)).await?;
        if let Some(progress) = progress {
            progress.finish("Server updated");
        } else {
            println!(
                "{} now runs Koidrive {}. Open sync sessions move to it within 5 minutes.",
                server.script.id, bundle.manifest.version
            );
        }
        Ok(())
    }

    pub(super) async fn upload(
        &mut self,
        bundle: &Bundle,
        script: &str,
        bucket: &Bound,
        deployed_tag: Option<&str>,
        secret: Option<&Secret>,
    ) -> Result<(), CliError> {
        let api = self.fresh().await?;
        let jwt = api
            .upload_assets(self.account(), script, &bundle.assets)
            .await
            .map_err(|error| CliError::from(error).context("cannot upload the static assets"))?;
        let plan = UploadPlan::new(
            bundle,
            &bucket.name,
            bucket.jurisdiction,
            deployed_tag,
            secret,
            Some(&jwt),
        )
        .map_err(|error| error.to_string())?;
        api.upload_script(self.account(), script, &plan)
            .await
            .map_err(|error| {
                CliError::from(error).context(format!("cannot upload the Worker {script}"))
            })?;
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Downgrade {
    Storage { server: Storage, bundle: Storage },
    Release,
    Unknown,
}

pub(super) fn downgrade(info: &ServerInfo, version: &str, storage: Storage) -> Option<Downgrade> {
    if let Some(reported) = info.storage {
        let server = Storage {
            account: reported.account,
            space: reported.space,
        };
        if server.account > storage.account || server.space > storage.space {
            return Some(Downgrade::Storage {
                server,
                bundle: storage,
            });
        }
    }
    match (release(&info.server_version), release(version)) {
        (Some(server), Some(bundle)) if server.cmp_precedence(&bundle).is_gt() => {
            Some(Downgrade::Release)
        }
        (None, _) if info.storage.is_none() => Some(Downgrade::Unknown),
        _ => None,
    }
}

fn release(version: &str) -> Option<semver::Version> {
    semver::Version::parse(version)
        .ok()
        .filter(|version| (version.major, version.minor, version.patch) != (0, 0, 0))
}
