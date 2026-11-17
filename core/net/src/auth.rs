// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use koidrive_proto::v1::token_request::Telemetry;
use koidrive_proto::v1::{AccessToken, DeviceCredentials, TokenRequest};
use tokio::sync::Mutex;

use crate::api::Api;
use crate::error::ApiError;

pub const DEFAULT_REFRESH_MARGIN: Duration = Duration::from_secs(120);

/// How old a refresh credential gets before the process that owns it replaces it (RFC 0011 section 2).
pub const ROTATION_PERIOD: Duration = Duration::from_secs(24 * 3600);

const ROTATION_DEADLINE_MS: i64 = 60_000;

static TELEMETRY: AtomicI32 = AtomicI32::new(Telemetry::Unspecified as i32);

/// Sets this device's usage-report choice, which every later `POST /v1/token` of this process carries.
pub fn set_telemetry(choice: Telemetry) {
    TELEMETRY.store(choice as i32, Ordering::Relaxed);
}

/// A device's long-lived credentials from `POST /v1/claim` or `POST /v1/pair/complete`. The caller stores them.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub user_id: String,
    pub device_id: String,
    pub refresh_credential: String,
}

impl From<&DeviceCredentials> for Credentials {
    fn from(credentials: &DeviceCredentials) -> Self {
        Credentials {
            user_id: credentials.user_id.clone(),
            device_id: credentials.device_id.clone(),
            refresh_credential: credentials.refresh_credential.clone(),
        }
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("user_id", &self.user_id)
            .field("device_id", &self.device_id)
            .finish_non_exhaustive()
    }
}

/// A refresh credential the device proposed, kept with its deadline until the server's answer settles whether it was
/// stored (RFC 0011 section 2).
#[derive(Clone, PartialEq, Eq)]
pub struct Staged {
    pub refresh_credential: String,
    pub rotate_before_ms: i64,
}

impl fmt::Debug for Staged {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Staged")
            .field("rotate_before_ms", &self.rotate_before_ms)
            .finish_non_exhaustive()
    }
}

/// The stored credential as one state: the current credential and, while a rotation's outcome is unknown, the staged
/// one. `changed_at` is when the current credential was stored, which gives its age.
#[derive(Clone, Debug)]
pub struct Stored {
    pub current: Credentials,
    pub staged: Option<Staged>,
    pub changed_at: SystemTime,
}

/// Where the process that owns the credential keeps its two slots. Each operation returns once its write is durable.
pub trait CredentialStore: Send + Sync {
    /// Stores `staged` in the staged slot.
    fn stage(&self, staged: &Staged) -> io::Result<()>;
    /// Makes `staged` the current credential and clears the staged slot.
    fn promote(&self, staged: &Staged) -> io::Result<()>;
    /// Clears the staged slot.
    fn clear_staged(&self) -> io::Result<()>;
}

/// Reads the stored credential again, for a process that does not own it.
pub type Reload = Arc<dyn Fn() -> io::Result<Stored> + Send + Sync>;

enum Rotation {
    Off,
    Owner {
        store: Arc<dyn CredentialStore>,
        period: Duration,
    },
    Reader(Reload),
}

struct State {
    token: Option<AccessToken>,
    stored: Stored,
    unstored_promotion: Option<Staged>,
}

/// Hands out access tokens for one device, refreshing them through `POST /v1/token` before they expire. One refresh
/// runs at a time, and it alone reads or changes the credential slots.
pub struct Auth {
    api: Api,
    state: Mutex<State>,
    credentials: std::sync::Mutex<Credentials>,
    rotation: Rotation,
    margin: Duration,
}

impl Auth {
    pub fn new(api: Api, credentials: Credentials) -> Self {
        Auth {
            api,
            credentials: std::sync::Mutex::new(credentials.clone()),
            state: Mutex::new(State {
                token: None,
                unstored_promotion: None,
                stored: Stored {
                    current: credentials,
                    staged: None,
                    changed_at: SystemTime::now(),
                },
            }),
            rotation: Rotation::Off,
            margin: DEFAULT_REFRESH_MARGIN,
        }
    }

    /// An `Auth` for the process that owns the stored credential: it rotates the credential once it is older than
    /// `period` and keeps both slots in `store`.
    pub fn rotating(
        api: Api,
        stored: Stored,
        store: Arc<dyn CredentialStore>,
        period: Duration,
    ) -> Self {
        let auth = Auth::new(api, stored.current.clone());
        auth.state
            .try_lock()
            .expect("a new Auth is not shared")
            .stored = stored;
        Auth {
            rotation: Rotation::Owner { store, period },
            ..auth
        }
    }

    /// An `Auth` for a process that only reads the stored credential: when the server refuses it, it reads the
    /// credential again with `reload`, once, and presents what it holds.
    pub fn reloading(api: Api, credentials: Credentials, reload: Reload) -> Self {
        Auth {
            rotation: Rotation::Reader(reload),
            ..Auth::new(api, credentials)
        }
    }

    /// Starts with a token the caller already holds, such as the one in `DeviceCredentials`.
    pub fn with_token(self, token: AccessToken) -> Self {
        self.state
            .try_lock()
            .expect("a new Auth is not shared")
            .token = Some(token);
        self
    }

    pub fn with_refresh_margin(self, margin: Duration) -> Self {
        Auth { margin, ..self }
    }

    /// The credentials the last refresh presented successfully, or the ones it started with.
    pub fn credentials(&self) -> Credentials {
        self.credentials.lock().unwrap().clone()
    }

    pub fn api(&self) -> &Api {
        &self.api
    }

    /// A token valid for at least the refresh margin, fetching a new one when needed.
    pub async fn access_token(&self) -> Result<AccessToken, ApiError> {
        let mut state = self.state.lock().await;
        if let Some(token) = state.token.as_ref()
            && !self.due(token)
        {
            return Ok(token.clone());
        }
        let token = match &self.rotation {
            Rotation::Off => {
                let current = state.stored.current.refresh_credential.clone();
                self.request(&current, None, false).await?
            }
            Rotation::Owner { store, period } => {
                self.refresh_rotating(&mut state, store.as_ref(), *period)
                    .await?
            }
            Rotation::Reader(reload) => self.refresh_reading(&mut state, reload).await?,
        };
        state.token = Some(token.clone());
        Ok(token)
    }

    /// RFC 0011 section 2, steps 1 to 6.
    async fn refresh_rotating(
        &self,
        state: &mut State,
        store: &dyn CredentialStore,
        period: Duration,
    ) -> Result<AccessToken, ApiError> {
        if let Some(promotion) = state.unstored_promotion.clone() {
            if let Err(error) = store.promote(&promotion) {
                tracing::warn!(%error, "the rotated refresh credential is still not stored as current; not rotating again until it is");
                let current = state.stored.current.refresh_credential.clone();
                return self.request(&current, None, true).await;
            }
            state.unstored_promotion = None;
        }
        if let Some(staged) = state.stored.staged.clone() {
            return self.settle(state, store, &staged).await;
        }
        let current = state.stored.current.refresh_credential.clone();
        let token = self.request(&current, None, true).await?;
        let age = state.stored.changed_at.elapsed().unwrap_or_default();
        if token.server_time_ms == 0 || age < period {
            return Ok(token);
        }
        let staged = Staged {
            refresh_credential: new_refresh_credential(),
            rotate_before_ms: token.server_time_ms + ROTATION_DEADLINE_MS,
        };
        if let Err(error) = store.stage(&staged) {
            tracing::warn!(%error, "could not store a new refresh credential; rotating at a later refresh");
            return Ok(token);
        }
        state.stored.staged = Some(staged.clone());
        match self.settle(state, store, &staged).await {
            Err(error) if !error.is_unauthenticated() => {
                tracing::info!(%error, "the rotation's outcome is unknown; keeping both credentials");
                Ok(token)
            }
            settled => settled,
        }
    }

    async fn settle(
        &self,
        state: &mut State,
        store: &dyn CredentialStore,
        staged: &Staged,
    ) -> Result<AccessToken, ApiError> {
        let current = state.stored.current.refresh_credential.clone();
        match self.request(&current, Some(staged), true).await {
            Ok(token) if token.rotated => {
                self.promote(state, store, staged);
                Ok(token)
            }
            Ok(token) => {
                if token.server_time_ms > staged.rotate_before_ms {
                    if let Err(error) = store.clear_staged() {
                        tracing::warn!(%error, "could not clear the staged refresh credential");
                    }
                    state.stored.staged = None;
                }
                Ok(token)
            }
            Err(error) if error.is_unauthenticated() => {
                let token = self.request(&staged.refresh_credential, None, true).await?;
                self.promote(state, store, staged);
                Ok(token)
            }
            Err(error) => Err(error),
        }
    }

    fn promote(&self, state: &mut State, store: &dyn CredentialStore, staged: &Staged) {
        state.unstored_promotion = match store.promote(staged) {
            Ok(()) => None,
            Err(error) => {
                tracing::warn!(%error, "could not store the rotated refresh credential as current");
                Some(staged.clone())
            }
        };
        state.stored.current.refresh_credential = staged.refresh_credential.clone();
        state.stored.staged = None;
        state.stored.changed_at = SystemTime::now();
        *self.credentials.lock().unwrap() = state.stored.current.clone();
        tracing::info!("rotated the refresh credential");
    }

    /// RFC 0011 section 2, processes that only read.
    async fn refresh_reading(
        &self,
        state: &mut State,
        reload: &Reload,
    ) -> Result<AccessToken, ApiError> {
        let presented = state.stored.current.refresh_credential.clone();
        let refused = match self.request(&presented, None, false).await {
            Err(error) if error.is_unauthenticated() => error,
            answer => return answer,
        };
        let stored = reload().map_err(ApiError::Credential)?;
        if (&stored.current.user_id, &stored.current.device_id)
            != (
                &state.stored.current.user_id,
                &state.stored.current.device_id,
            )
        {
            return Err(ApiError::Credential(io::Error::other(
                "the profile now holds another device's credential",
            )));
        }
        let candidates = [
            Some(stored.current.refresh_credential.clone()),
            stored.staged.map(|staged| staged.refresh_credential),
        ];
        let mut last = refused;
        for candidate in candidates.into_iter().flatten() {
            if candidate == presented {
                continue;
            }
            match self.request(&candidate, None, false).await {
                Ok(token) => {
                    state.stored.current.refresh_credential = candidate;
                    *self.credentials.lock().unwrap() = state.stored.current.clone();
                    return Ok(token);
                }
                Err(error) if error.is_unauthenticated() => last = error,
                Err(error) => return Err(error),
            }
        }
        Err(last)
    }

    async fn request(
        &self,
        refresh_credential: &str,
        proposal: Option<&Staged>,
        rotation: bool,
    ) -> Result<AccessToken, ApiError> {
        let request = TokenRequest {
            device_id: self.credentials.lock().unwrap().device_id.clone(),
            refresh_credential: refresh_credential.to_owned(),
            rotation,
            next_refresh_credential: proposal
                .map(|staged| staged.refresh_credential.clone())
                .unwrap_or_default(),
            rotate_before_ms: proposal.map_or(0, |staged| staged.rotate_before_ms),
            telemetry: TELEMETRY.load(Ordering::Relaxed),
        };
        self.api.token(&request).await
    }

    pub(crate) fn refresh_at(&self, token: &AccessToken) -> SystemTime {
        UNIX_EPOCH
            .checked_add(Duration::from_millis(token.expires_at_ms.max(0) as u64))
            .and_then(|expires| expires.checked_sub(self.margin))
            .unwrap_or(UNIX_EPOCH)
    }

    fn due(&self, token: &AccessToken) -> bool {
        SystemTime::now() >= self.refresh_at(token)
    }

    pub(crate) async fn reject(&self, token: &str) {
        let mut state = self.state.lock().await;
        if state
            .token
            .as_ref()
            .is_some_and(|current| current.token == token)
        {
            state.token = None;
        }
    }
}

fn new_refresh_credential() -> String {
    let mut bytes = [0u8; 32];
    rustls::crypto::aws_lc_rs::default_provider()
        .secure_random
        .fill(&mut bytes)
        .expect("the system random source works");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Auth")
            .field("credentials", &self.credentials())
            .field("margin", &self.margin)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod rotation_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn auth(margin: Duration) -> Auth {
        let api = Api::local_development(
            format!("http://{}", koidrive_test_support::reset_tcp_address())
                .parse()
                .unwrap(),
        )
        .unwrap();
        let credentials = Credentials {
            user_id: "u".into(),
            device_id: "d".into(),
            refresh_credential: "secret".into(),
        };
        Auth::new(api, credentials).with_refresh_margin(margin)
    }

    fn token_expiring_in(from_now: Duration) -> AccessToken {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        AccessToken {
            token: "t".into(),
            expires_at_ms: (now + from_now).as_millis() as i64,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_token_outside_the_margin_is_reused_without_a_request() {
        let auth =
            auth(Duration::from_secs(120)).with_token(token_expiring_in(Duration::from_secs(600)));
        assert_eq!(auth.access_token().await.unwrap().token, "t");
    }

    #[tokio::test]
    async fn a_token_inside_the_margin_is_refreshed() {
        let auth =
            auth(Duration::from_secs(120)).with_token(token_expiring_in(Duration::from_secs(60)));
        assert!(
            matches!(auth.access_token().await, Err(ApiError::Transport(_))),
            "the owned server resets the connection, so a refresh was attempted"
        );
    }

    #[tokio::test]
    async fn reject_drops_only_the_refused_token() {
        let auth = auth(Duration::ZERO).with_token(token_expiring_in(Duration::from_secs(600)));
        auth.reject("other").await;
        assert_eq!(auth.access_token().await.unwrap().token, "t");
        auth.reject("t").await;
        assert!(auth.access_token().await.is_err());
    }

    #[test]
    fn debug_output_leaves_out_the_secrets() {
        let auth = auth(Duration::ZERO);
        let text = format!("{auth:?} {:?}", auth.credentials());
        assert!(!text.contains("secret"), "{text}");
    }
}
