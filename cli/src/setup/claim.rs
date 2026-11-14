// SPDX-License-Identifier: Apache-2.0

//! Claiming a server with a one-time `KOIDRIVE_RECOVERY_KEY` (docs/design/cli-setup.md sections 4.4 to 4.6): set the
//! secret, claim with the setup attempt id, save the credential, then delete the secret. Each step is recorded in the
//! state file first, so a rerun after a crash or a lost response converges on one device.

use std::time::{Duration, Instant};

use koidrive_cloudflare::Secret;
use koidrive_driver::{Credential, Paths};
use koidrive_ipc::{Request, Response};
use koidrive_net::ApiError;
use koidrive_proto::v1::ClaimRequest;
use koidrive_proto::v1::error::Code;

use super::state::{SetupState, StateFile};
use super::{Cloud, Found, Session, report_paired, server_api};
use crate::error::CliError;
use crate::ipc;
use url::Url;

pub const KEY_NAME: &str = "KOIDRIVE_RECOVERY_KEY";
/// A new key at most this often per run; more spent keys mean something else is wrong.
const MAX_KEYS: usize = 5;

/// The server's refusals while it cannot see the key yet: a new secret still propagating, or an Account DO that still
/// runs code without the key claim. Both pass on a later try.
const NOT_YET: [&str; 2] = [
    "no valid claim code; open /setup to issue one",
    "this deployment already has an owner",
];
/// The server's refusals that spend the key: one a claim already used, and a claim whose device a later claim of the
/// same attempt revoked before it answered. Retrying the key never succeeds; a new one does.
const SPENT: [&str; 2] = [
    "this recovery key was already used; set a new one",
    "a later claim of this setup attempt replaced this one; set a new recovery key and claim again",
];

pub struct Claim<'a> {
    session: &'a mut Session,
    paths: &'a Paths,
    server: &'a Found,
    device_name: &'a str,
    state: SetupState,
    /// The key the deploy uploaded with the Worker, if this run set one.
    key: Option<Secret>,
}

enum Outcome {
    Claimed(Credential),
    Spent,
}

impl<'a> Claim<'a> {
    pub fn new(
        session: &'a mut Session,
        paths: &'a Paths,
        server: &'a Found,
        device_name: &'a str,
        state: SetupState,
        key: Option<Secret>,
    ) -> Self {
        Claim {
            session,
            paths,
            server,
            device_name,
            state,
            key,
        }
    }

    /// Claims within `window` per key: 60 seconds for a server that already runs the key claim, up to 330 when an
    /// Account DO may still be moving to new code.
    pub async fn run(mut self, window: Duration) -> Result<(), CliError> {
        let file = StateFile::new(self.paths);
        if self.state.setup_attempt.is_none() {
            self.state.setup_attempt = Some(Secret::random(16).expose().to_owned());
        }
        file.save(&self.state)?;
        if saved_device(self.paths, &self.server.url).await? {
            if !self.session.terminal.setup_guided() {
                println!(
                    "This device is already set up with {}, so it was not claimed again.",
                    self.server.script.id
                );
            }
            if self.state.secret_set {
                self.delete_key().await?;
            }
            return Ok(file.remove()?);
        }
        let mut key = match self.key.take() {
            Some(key) => key,
            None => self.set_key(&file, true).await?,
        };
        let mut credential = None;
        for _ in 0..MAX_KEYS {
            match self.claim(&key, window).await? {
                Outcome::Claimed(issued) => {
                    credential = Some(issued);
                    break;
                }
                Outcome::Spent => key = self.set_key(&file, false).await?,
            }
        }
        let credential = credential.ok_or_else(|| {
            format!("the server refused {MAX_KEYS} keys as already used; run the command again")
        })?;
        let saved = ipc::run(self.paths, Request::SaveCredential { credential }).await?;
        report_paired(self.paths, saved, &self.session.terminal)?;
        self.state.setup_attempt = None;
        file.save(&self.state)?;
        self.delete_key().await?;
        Ok(file.remove()?)
    }

    /// Sets a new one-time key on the Worker. Before the first one it checks that the owner has not set a key of
    /// their own for recovery, which this run must not replace.
    async fn set_key(&mut self, file: &StateFile, first: bool) -> Result<Secret, CliError> {
        let account = self.session.account().to_owned();
        let script = self.state.script.clone();
        let api = self.session.fresh().await?;
        if first && !self.state.secret_set {
            let secrets = api.secrets(&account, &script).await.map_err(|error| {
                CliError::from(error).context(format!("cannot list the secrets of {script}"))
            })?;
            if secrets.iter().any(|name| name == KEY_NAME) {
                return Err(format!(
                    "{script} already has a {KEY_NAME} secret, which its owner set for recovery. Use that key, or delete the secret in the dashboard, then run this command again."
                ).into());
            }
        }
        self.state.secret_set = true;
        file.save(&self.state)?;
        let key = Secret::random(32);
        api.put_secret(&account, &script, KEY_NAME, &key)
            .await
            .map_err(|error| {
                CliError::from(error).context(format!("cannot set the one-time key on {script}"))
            })?;
        Ok(key)
    }

    async fn claim(&self, key: &Secret, window: Duration) -> Result<Outcome, CliError> {
        let api = server_api(&self.server.url)?;
        let request = ClaimRequest {
            code: key.expose().to_owned(),
            user_name: user_name(),
            device_name: self.device_name.to_owned(),
            platform: std::env::consts::OS.to_owned(),
            setup_attempt: self.state.setup_attempt.clone().unwrap_or_default(),
            request_id: String::new(),
        };
        let started = Instant::now();
        if !self.session.terminal.setup_guided() {
            println!("Claiming {} with this device...", self.server.script.id);
        }
        loop {
            let wait = match api.claim(&request).await {
                Ok(issued) => {
                    return Ok(Outcome::Claimed(Credential {
                        server: api.base().to_string(),
                        user_id: issued.user_id,
                        user_name: Some(issued.user_name).filter(|name| !name.is_empty()),
                        device_id: issued.device_id,
                        device_name: self.device_name.to_owned(),
                        refresh_credential: issued.refresh_credential,
                    }));
                }
                Err(ApiError::Server { status: 403, error })
                    if SPENT.contains(&error.message()) =>
                {
                    return Ok(Outcome::Spent);
                }
                Err(ApiError::Server { status: 403, error })
                    if NOT_YET.contains(&error.message()) =>
                {
                    Duration::from_secs(3)
                }
                Err(ApiError::Server { error, .. }) if error.code() == Code::RetryLater => {
                    error.retry_after().unwrap_or(Duration::from_secs(10))
                }
                Err(
                    ApiError::Transport(_)
                    | ApiError::TooLarge { .. }
                    | ApiError::Unreadable { .. },
                ) => Duration::from_secs(3),
                Err(ApiError::Status { status }) if status >= 500 => Duration::from_secs(3),
                Err(error) => {
                    return Err(CliError::from(error).context("the server refused the claim"));
                }
            };
            if started.elapsed() + wait > window {
                return Err(CliError::new(
                    koidrive_ipc::Code::Unavailable,
                    format!(
                        "the server did not accept the claim within {} seconds; run the command again to continue",
                        window.as_secs()
                    ),
                ));
            }
            tokio::time::sleep(wait).await;
        }
    }

    /// Deletes the one-time key and checks that it is gone. Other secrets stay.
    async fn delete_key(&mut self) -> Result<(), CliError> {
        let account = self.session.account().to_owned();
        let script = self.state.script.clone();
        let api = self.session.fresh().await?;
        api.delete_secret(&account, &script, KEY_NAME)
            .await
            .map_err(|error| {
                CliError::caused(format!(
                    "cannot delete the one-time key from {script}: {error}; run the command again"
                ), error)
            })?;
        let secrets = api.secrets(&account, &script).await.map_err(|error| {
            CliError::from(error).context(format!("cannot list the secrets of {script}"))
        })?;
        if secrets.iter().any(|name| name == KEY_NAME) {
            return Err(
                format!("{KEY_NAME} is still set on {script}; run the command again").into(),
            );
        }
        self.state.secret_set = false;
        Ok(())
    }
}

/// Whether this profile already holds a working credential for `server`, which a run that stopped after saving its
/// claim's credential leaves behind (section 4.6). The server decides with `POST /v1/token`: a credential it refuses
/// as unknown or revoked is removed, so the claim can go on. Any other answer stops the run and changes nothing.
pub async fn saved_device(paths: &Paths, server: &Url) -> Result<bool, CliError> {
    let Some(credential) =
        Credential::load(&paths.credential()).map_err(|error| error.to_string())?
    else {
        return Ok(false);
    };
    let api = credential.api().map_err(|error| error.to_string())?;
    if api.base().as_str().trim_end_matches('/') != server.as_str().trim_end_matches('/') {
        return Err(format!(
            "this device is already set up with {}; its credential is {}",
            credential.server,
            paths.credential().display()
        )
        .into());
    }
    match crate::reading(paths, &credential)?.access_token().await {
        Ok(_) => Ok(true),
        Err(error) if error.is_unauthenticated() => {
            let request = Request::SignOut {
                device_id: credential.device_id,
            };
            match ipc::run(paths, request).await? {
                Response::SignedOut { not_removed } => {
                    if !not_removed.is_empty() {
                        let failures = not_removed
                            .iter()
                            .map(|failure| {
                                format!("{:?}: {}", failure.path, crate::printable(&failure.error))
                            })
                            .collect::<Vec<_>>()
                            .join("; ");
                        return Err(CliError::new(
                            koidrive_ipc::Code::Failed,
                            format!(
                                "the server no longer accepts this device's saved credential, but cleanup is incomplete: {failures}. Run koi device leave to finish cleanup before setting up this device again"
                            ),
                        )
                        .then(&["koi", "device", "leave"]));
                    }
                    println!(
                        "The server no longer accepts this device's saved credential, so it was removed."
                    );
                    Ok(false)
                }
                other => Err(format!("the daemon answered {other:?}").into()),
            }
        }
        Err(error) => Err(CliError::caused(
            format!(
                "cannot check this device's saved credential with {}: {error}. Nothing was changed; run the command again",
                credential.server
            ),
            error,
        )),
    }
}

/// The owner's name for a new server: the login name, which the apps show and the owner can change later.
fn user_name() -> String {
    std::env::var("USER")
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Owner".into())
}

#[cfg(test)]
mod tests {
    use koidrive_proto::v1::{AccessToken, Error};
    use prost::Message;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    use super::*;
    use crate::setup::fake::{ACCOUNT, Fake, Reply, proto, session, tokens};
    use crate::setup::state::Step;

    fn token(status: u16, body: Vec<u8>) -> Reply {
        Reply {
            method: "POST",
            path: "/v1/token".into(),
            status,
            content_type: "application/x-protobuf",
            body,
        }
    }

    fn refusal(code: Code) -> Vec<u8> {
        Error {
            code: code as i32,
            message: "unknown device or refresh credential".into(),
            ..Error::default()
        }
        .encode_to_vec()
    }

    /// A profile whose credential names `server`.
    fn profile(server: &Url) -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        Credential {
            server: server.to_string(),
            user_id: "user".into(),
            user_name: Some("owner".into()),
            device_id: "0194f8a2-0000-7000-8000-000000000001".into(),
            device_name: "laptop".into(),
            refresh_credential: "refresh".into(),
        }
        .save_new(&paths.credential())
        .unwrap();
        (dir, paths)
    }

    #[tokio::test]
    async fn a_refused_credential_save_keeps_the_setup_attempt() {
        let issued = koidrive_proto::v1::DeviceCredentials {
            user_id: "user".into(),
            device_id: "0194f8a2-0000-7000-8000-000000000002".into(),
            refresh_credential: "device".into(),
            ..Default::default()
        };
        let fake = Fake::start(vec![tokens(), proto("POST", "/v1/claim", issued)]).await;
        let mut session = session(&fake).await;
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        std::fs::create_dir_all(&paths.runtime_dir).unwrap();
        let daemon = tokio::net::UnixListener::bind(paths.socket()).unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = daemon.accept().await.unwrap();
            let (reader, mut writer) = stream.split();
            let mut request = String::new();
            BufReader::new(reader)
                .read_line(&mut request)
                .await
                .unwrap();
            let refusal = Response::Error {
                message: "the disk is full".into(),
                code: koidrive_ipc::Code::Failed,
                reason: None,
                next: Vec::new(),
            };
            let line = format!("{}\n", serde_json::to_string(&refusal).unwrap());
            writer.write_all(line.as_bytes()).await.unwrap();
        });
        let server = Found {
            script: koidrive_cloudflare::Script {
                id: "koidrive".into(),
                created_on: None,
                tags: Vec::new(),
                migration_tag: None,
                has_assets: true,
            },
            bucket: None,
            url: fake.base.clone(),
            info: None,
        };
        let state = SetupState {
            account: ACCOUNT.into(),
            script: "koidrive".into(),
            step: Step::Serving,
            secret_set: true,
            setup_attempt: Some("attempt".into()),
            ..SetupState::default()
        };

        let error = Claim::new(
            &mut session,
            &paths,
            &server,
            "laptop",
            state,
            Some(Secret::random(32)),
        )
        .run(Duration::from_secs(1))
        .await
        .unwrap_err()
        .message;

        assert_eq!(error, "the disk is full");
        let saved = StateFile::new(&paths).load().unwrap().unwrap();
        assert_eq!(saved.setup_attempt.as_deref(), Some("attempt"));
        assert!(!fake.sent(&["DELETE"]));
    }

    #[tokio::test]
    async fn without_a_saved_credential_the_claim_goes_on() {
        let fake = Fake::start(vec![]).await;
        let dir = tempfile::tempdir().unwrap();
        assert!(
            !saved_device(&Paths::under(dir.path()), &fake.base)
                .await
                .unwrap()
        );
        assert!(fake.seen().is_empty());
    }

    #[tokio::test]
    async fn a_credential_the_server_accepts_skips_the_claim() {
        let answer = AccessToken {
            token: "access".into(),
            expires_at_ms: i64::MAX,
            ..Default::default()
        };
        let fake = Fake::start(vec![token(200, answer.encode_to_vec())]).await;
        let (_dir, paths) = profile(&fake.base);

        assert!(saved_device(&paths, &fake.base).await.unwrap());
        assert!(paths.credential().exists());
    }

    #[tokio::test]
    async fn a_credential_the_server_no_longer_knows_is_removed() {
        let fake = Fake::start(vec![token(401, refusal(Code::Unauthenticated))]).await;
        let (_dir, paths) = profile(&fake.base);

        assert!(!saved_device(&paths, &fake.base).await.unwrap());
        assert!(!paths.credential().exists());
    }

    #[tokio::test]
    async fn incomplete_cleanup_stops_setup_and_reports_every_remaining_path() {
        let fake = Fake::start(vec![tokens(), token(401, refusal(Code::Unauthenticated))]).await;
        let mut session = session(&fake).await;
        let (_dir, paths) = profile(&fake.base);
        let failures = vec![
            koidrive_driver::CleanupFailure {
                path: paths.credential(),
                error: "permission denied".into(),
            },
            koidrive_driver::CleanupFailure {
                path: paths.state_dir.join("cleanup\n\u{1b}[2J"),
                error: "disk\n\u{1b}[31m error".into(),
            },
        ];
        let listener = koidrive_ipc::bind(&paths.socket()).unwrap();
        let reply = failures.clone();
        let served = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let line = koidrive_ipc::read_line(&mut stream).await.unwrap();
            assert!(matches!(
                serde_json::from_str::<Request>(&line).unwrap(),
                Request::SignOut { .. }
            ));
            koidrive_ipc::write_line(&mut stream, &Response::SignedOut { not_removed: reply })
                .await
                .unwrap();
        });

        let server = Found {
            script: koidrive_cloudflare::Script {
                id: "koidrive".into(),
                created_on: None,
                tags: Vec::new(),
                migration_tag: None,
                has_assets: true,
            },
            bucket: None,
            url: fake.base.clone(),
            info: None,
        };
        let state = SetupState {
            setup_attempt: Some("attempt".into()),
            ..SetupState::default()
        };
        let error = Claim::new(&mut session, &paths, &server, "laptop", state, None)
            .run(Duration::from_secs(1))
            .await
            .unwrap_err();
        served.await.unwrap();

        assert_eq!(error.code, koidrive_ipc::Code::Failed);
        for failure in failures {
            assert!(error.message.contains(&format!("{:?}", failure.path)));
            assert!(error.message.contains(&crate::printable(&failure.error)));
        }
        assert!(!error.message.contains(['\n', '\u{1b}']));
        assert!(error.message.contains("cleanup\\n\\u{1b}[2J\""));
        assert!(error.message.contains("disk\\u{a}\\u{1b}[31m error"));
        assert!(!error.message.contains("so it was removed"));
        assert_eq!(
            error.next,
            [koidrive_ipc::Next::of(&["koi", "device", "leave"])]
        );
        assert!(paths.credential().exists());
        let saved = StateFile::new(&paths).load().unwrap().unwrap();
        assert_eq!(saved.setup_attempt.as_deref(), Some("attempt"));
        assert_eq!(
            fake.seen()
                .iter()
                .map(|request| request.path.as_str())
                .collect::<Vec<_>>(),
            ["/oauth2/token", "/v1/token"]
        );
    }

    #[tokio::test]
    async fn any_other_answer_stops_the_run_and_keeps_the_credential() {
        let answers = [
            token(503, b"unavailable".to_vec()),
            token(429, refusal(Code::RetryLater)),
            token(401, refusal(Code::RetryLater)),
        ];
        for answer in answers {
            let fake = Fake::start(vec![answer]).await;
            let (_dir, paths) = profile(&fake.base);

            let error = saved_device(&paths, &fake.base).await.unwrap_err().message;

            assert!(error.contains("Nothing was changed"), "{error}");
            assert!(paths.credential().exists());
        }
    }

    #[tokio::test]
    async fn a_credential_for_another_server_stops_the_run() {
        let fake = Fake::start(vec![]).await;
        let (_dir, paths) = profile(&Url::parse("https://files.example.com/").unwrap());

        let error = saved_device(&paths, &fake.base).await.unwrap_err().message;

        assert!(
            error.contains("already set up with https://files.example.com/"),
            "{error}"
        );
        assert!(fake.seen().is_empty());
        assert!(paths.credential().exists());
    }
}
