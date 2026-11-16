// SPDX-License-Identifier: Apache-2.0

use std::io;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use url::Url;

use crate::secret::Secret;

pub const CLIENT_ID: &str = "6f1270857b9b3da60246032d40329971";

/// The scopes `koi setup` needs (cli-setup.md section 3).
pub const SCOPES: [&str; 4] = [
    "account-settings.read",
    "workers-scripts.admin",
    "workers-r2.write",
    "offline_access",
];

const CALLBACK_PATH: &str = "/oauth/callback";

const MAX_REQUEST_HEAD: usize = 64 * 1024;

const CONNECTION_TIMEOUT: Duration = Duration::from_secs(10);

const CALLBACK_PAGE: &str = "<!doctype html>\n<meta charset=\"utf-8\">\n<title>Koidrive</title>\n\
<p>You can close this tab and return to the terminal.</p>\n";

/// The HTTP client for the OAuth endpoints and the Cloudflare API: HTTPS only, no redirects, a 15 s connect timeout
/// and a 60 s limit per request. It fails only when the TLS backend cannot load the platform's certificate verifier.
pub fn http_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .user_agent(concat!("koidrive-cloudflare/", env!("CARGO_PKG_VERSION")))
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60))
        .build()
}

#[derive(Clone, Debug)]
pub struct Endpoints {
    pub authorize: Url,
    pub token: Url,
    pub revoke: Url,
}

impl Endpoints {
    /// The endpoints Cloudflare documents for OAuth clients.
    pub fn cloudflare() -> Self {
        let endpoint = |path: &str| {
            Url::parse("https://dash.cloudflare.com")
                .and_then(|base| base.join(path))
                .expect("the dashboard's OAuth endpoints are valid URLs")
        };
        Endpoints {
            authorize: endpoint("/oauth2/auth"),
            token: endpoint("/oauth2/token"),
            revoke: endpoint("/oauth2/revoke"),
        }
    }
}

/// Why a login, a refresh or a revocation failed.
#[derive(Debug, thiserror::Error)]
pub enum OauthError {
    #[error("could not listen for the login callback on 127.0.0.1 port {port}")]
    Listen {
        port: u16,
        #[source]
        source: io::Error,
    },
    #[error("the login callback listener stopped")]
    Accept(#[source] io::Error),
    #[error(
        "the address is not the login callback; it must be the http://127.0.0.1 page the browser opened after Authorize"
    )]
    NotCallback,
    /// The callback's `state` is missing or belongs to another login.
    #[error("the login callback belongs to another login attempt; start the login again")]
    StateMismatch,
    #[error("Cloudflare did not authorize the login: {}", oauth_message(.error, .description))]
    Denied {
        error: String,
        description: Option<String>,
    },
    #[error("the login callback carries no authorization code; start the login again")]
    MissingCode,
    #[error("could not reach Cloudflare's OAuth endpoint")]
    Transport(#[source] reqwest::Error),
    /// The token endpoint refused. Only the body's `error` and `error_description` are kept, so no other part of a
    /// response reaches an error message.
    #[error("Cloudflare refused the token request with HTTP {status}: {}", oauth_message(.error, .description))]
    TokenRefused {
        status: u16,
        error: String,
        description: Option<String>,
    },
    #[error("Cloudflare's token response could not be read")]
    MalformedTokenResponse,
    #[error(
        "Cloudflare granted the login without {missing}; log in again and allow every permission on the consent page"
    )]
    MissingScopes { missing: String },
    #[error("the grant can no longer be refreshed after a failed refresh; log in again")]
    Spent,
    #[error(
        "Cloudflare did not revoke the {token} (HTTP {status}); revoke Koidrive under Manage OAuth authorizations in the dashboard"
    )]
    RevokeRefused { token: &'static str, status: u16 },
}

fn oauth_message(error: &str, description: &Option<String>) -> String {
    match description {
        Some(description) => format!("{error} ({description})"),
        None => error.to_owned(),
    }
}

/// One login: a PKCE verifier and a state value for exactly one code exchange.
pub struct Login {
    endpoints: Endpoints,
    client_id: String,
    redirect_uri: String,
    verifier: Secret,
    challenge: String,
    state: Secret,
}

impl Login {
    /// A login whose redirect is `http://127.0.0.1:<port>/oauth/callback`.
    pub fn new(endpoints: Endpoints, client_id: &str, port: u16) -> Self {
        let verifier = Secret::random(32);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.expose().as_bytes()));
        Login {
            endpoints,
            client_id: client_id.to_owned(),
            redirect_uri: format!("http://127.0.0.1:{port}{CALLBACK_PATH}"),
            verifier,
            challenge,
            state: Secret::random(16),
        }
    }

    /// The page the user opens to sign in and authorize Koidrive.
    pub fn authorize_url(&self) -> Url {
        let mut url = self.endpoints.authorize.clone();
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.client_id)
            .append_pair("redirect_uri", &self.redirect_uri)
            .append_pair("scope", &SCOPES.join(" "))
            .append_pair("state", self.state.expose())
            .append_pair("code_challenge", &self.challenge)
            .append_pair("code_challenge_method", "S256");
        url
    }

    /// The code in a redirect URL, from the loopback listener or pasted by the user, after checking its state. The
    /// state is checked before an `error` is read, so only the real redirect can end a login.
    pub fn code_from(&self, redirect: &Url) -> Result<AuthorizationCode, OauthError> {
        if redirect.path() != CALLBACK_PATH {
            return Err(OauthError::NotCallback);
        }
        let (mut code, mut state, mut error, mut description) = (None, None, None, None);
        for (key, value) in redirect.query_pairs() {
            match key.as_ref() {
                "code" => code = Some(value.into_owned()),
                "state" => state = Some(value.into_owned()),
                "error" => error = Some(value.into_owned()),
                "error_description" => description = Some(value.into_owned()),
                _ => {}
            }
        }
        if !state
            .is_some_and(|state| constant_time_eq(state.as_bytes(), self.state.expose().as_bytes()))
        {
            return Err(OauthError::StateMismatch);
        }
        if let Some(error) = error {
            return Err(OauthError::Denied { error, description });
        }
        match code {
            Some(code) if !code.is_empty() => Ok(AuthorizationCode(Secret::new(code))),
            _ => Err(OauthError::MissingCode),
        }
    }

    /// Exchanges the code at the token endpoint. It consumes the login, so a code is never sent twice: a replayed code
    /// revokes the whole grant (section 2). If Cloudflare granted fewer scopes than [`SCOPES`], it revokes the new
    /// grant before it returns [`OauthError::MissingScopes`]; a failure of that revocation is not reported.
    pub async fn exchange(
        self,
        http: &reqwest::Client,
        code: AuthorizationCode,
    ) -> Result<Grant, OauthError> {
        let sent = Instant::now();
        let tokens = token_request(
            http,
            &self.endpoints.token,
            &[
                ("grant_type", "authorization_code"),
                ("code", code.0.expose()),
                ("redirect_uri", &self.redirect_uri),
                ("client_id", &self.client_id),
                ("code_verifier", self.verifier.expose()),
            ],
        )
        .await?;
        let usable = usable(sent, &tokens);
        let mut grant = Grant {
            expires_at: sent,
            access: Secret::new(tokens.access_token),
            refresh: Some(Secret::new(tokens.refresh_token)),
            stale: None,
            endpoints: self.endpoints,
            client_id: self.client_id,
        };
        match usable {
            Ok(expires_at) => {
                grant.expires_at = expires_at;
                Ok(grant)
            }
            Err(error) => {
                let _ = grant.revoke(http).await;
                Err(error)
            }
        }
    }
}

/// An authorization code from the login callback, to be exchanged once.
#[derive(Debug)]
pub struct AuthorizationCode(Secret);

/// Listens on 127.0.0.1 for the browser's redirect.
pub struct Loopback {
    listener: TcpListener,
    port: u16,
}

impl Loopback {
    /// Binds 127.0.0.1 on `port`, or on a free port. It never listens on another address.
    pub async fn bind(port: Option<u16>) -> Result<Self, OauthError> {
        let requested = port.unwrap_or(0);
        let listen_error = |source| OauthError::Listen {
            port: requested,
            source,
        };
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, requested))
            .await
            .map_err(listen_error)?;
        let port = listener.local_addr().map_err(listen_error)?.port();
        Ok(Loopback { listener, port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Answers requests until one reaches the callback path with this login's state, and returns its code. Other
    /// paths get 404 and a wrong state gets 400, and neither ends the wait, so another process on the machine cannot
    /// end the login. Connections are read concurrently, so one that never sends a request delays nobody.
    pub async fn callback(&self, login: &Login) -> Result<AuthorizationCode, OauthError> {
        let mut reading = JoinSet::new();
        loop {
            tokio::select! {
                accepted = self.listener.accept() => {
                    let (stream, _) = accepted.map_err(OauthError::Accept)?;
                    reading.spawn(read_request_line(stream));
                }
                Some(Ok((mut stream, line))) = reading.join_next() => {
                    let url = line.and_then(|line| self.request_url(&line));
                    let Some(url) = url else {
                        respond(&mut stream, "400 Bad Request", "").await;
                        continue;
                    };
                    match login.code_from(&url) {
                        Err(OauthError::NotCallback) => respond(&mut stream, "404 Not Found", "").await,
                        Err(OauthError::StateMismatch) => respond(&mut stream, "400 Bad Request", "").await,
                        result => {
                            respond(&mut stream, "200 OK", CALLBACK_PAGE).await;
                            return result;
                        }
                    }
                }
            }
        }
    }

    fn request_url(&self, line: &str) -> Option<Url> {
        let mut parts = line.split(' ');
        let (_method, target, version) = (parts.next()?, parts.next()?, parts.next()?);
        if !target.starts_with('/') || !version.starts_with("HTTP/") || parts.next().is_some() {
            return None;
        }
        Url::parse(&format!("http://127.0.0.1:{}{target}", self.port)).ok()
    }
}

/// Reads the whole head, because closing a socket with unread bytes resets the connection, and the browser would
/// then show an error instead of the page.
async fn read_request_line(mut stream: TcpStream) -> (TcpStream, Option<String>) {
    let mut buffer = Vec::new();
    let read = tokio::time::timeout(CONNECTION_TIMEOUT, async {
        let mut chunk = [0u8; 4096];
        loop {
            if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                let line = buffer
                    .split(|&byte| byte == b'\r')
                    .next()
                    .unwrap_or_default();
                return String::from_utf8(line.to_vec()).ok();
            }
            if buffer.len() >= MAX_REQUEST_HEAD {
                return None;
            }
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return None,
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            }
        }
    })
    .await;
    (stream, read.ok().flatten())
}

async fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = tokio::time::timeout(CONNECTION_TIMEOUT, async {
        stream.write_all(response.as_bytes()).await?;
        stream.shutdown().await
    })
    .await;
}

/// The tokens of one grant, in memory only.
pub struct Grant {
    endpoints: Endpoints,
    client_id: String,
    access: Secret,
    refresh: Option<Secret>,
    stale: Option<Secret>,
    expires_at: Instant,
}

impl Grant {
    pub fn access_token(&self) -> &Secret {
        &self.access
    }

    /// Whether the access token expires within `margin`, counted from when the token request was sent. It is always
    /// true after a failed refresh.
    pub fn expires_within(&self, margin: Duration) -> bool {
        self.expires_at.saturating_duration_since(Instant::now()) <= margin
    }

    /// Refreshes once and forgets the old refresh token. After a lost response the caller starts a new login instead
    /// of retrying, because a late reuse of a rotated refresh token revokes the grant (section 2). After any failure
    /// the grant only keeps its access token for [`Grant::revoke`]. A refresh that grants fewer scopes than [`SCOPES`]
    /// revokes the new tokens and returns [`OauthError::MissingScopes`], leaving the grant unusable.
    pub async fn refresh(&mut self, http: &reqwest::Client) -> Result<(), OauthError> {
        let refresh = self.refresh.take().ok_or(OauthError::Spent)?;
        self.stale = Some(refresh.clone());
        let sent = Instant::now();
        self.expires_at = sent;
        let tokens = token_request(
            http,
            &self.endpoints.token,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh.expose()),
                ("client_id", &self.client_id),
            ],
        )
        .await?;
        let usable = usable(sent, &tokens);
        self.stale = None;
        self.access = Secret::new(tokens.access_token);
        self.refresh = Some(Secret::new(tokens.refresh_token));
        match usable {
            Ok(expires_at) => {
                self.expires_at = expires_at;
                Ok(())
            }
            Err(error) => {
                let _ = self.revoke(http).await;
                self.stale = self.refresh.take();
                Err(error)
            }
        }
    }

    /// Revokes the refresh token, then the refresh token of a failed refresh, then the access token. It sends each
    /// even if an earlier one fails, and reports the first failure. Revoking a token again is harmless (RFC 7009 section 2.2), so the caller may retry.
    pub async fn revoke(&self, http: &reqwest::Client) -> Result<(), OauthError> {
        let tokens = [
            self.refresh
                .as_ref()
                .map(|token| (token, "refresh_token", "refresh token")),
            self.stale
                .as_ref()
                .map(|token| (token, "refresh_token", "refresh token")),
            Some((&self.access, "access_token", "access token")),
        ];
        let mut first_failure = None;
        for (token, hint, name) in tokens.into_iter().flatten() {
            let answer = http
                .post(self.endpoints.revoke.clone())
                .form(&[
                    ("token", token.expose()),
                    ("token_type_hint", hint),
                    ("client_id", self.client_id.as_str()),
                ])
                .send()
                .await;
            let failure = match answer {
                Ok(response) if response.status().is_success() => continue,
                Ok(response) => OauthError::RevokeRefused {
                    token: name,
                    status: response.status().as_u16(),
                },
                Err(error) => OauthError::Transport(error),
            };
            first_failure.get_or_insert(failure);
        }
        first_failure.map_or(Ok(()), Err)
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
    scope: Option<String>,
}

#[derive(Deserialize)]
struct TokenErrorResponse {
    error: String,
    error_description: Option<String>,
}

async fn token_request(
    http: &reqwest::Client,
    url: &Url,
    form: &[(&str, &str)],
) -> Result<TokenResponse, OauthError> {
    let response = http
        .post(url.clone())
        .form(form)
        .send()
        .await
        .map_err(OauthError::Transport)?;
    let status = response.status();
    let body = response.bytes().await.map_err(OauthError::Transport)?;
    if !status.is_success() {
        let refusal = serde_json::from_slice::<TokenErrorResponse>(&body).ok();
        return Err(OauthError::TokenRefused {
            status: status.as_u16(),
            error: refusal
                .as_ref()
                .map_or_else(|| "no error message".to_owned(), |r| r.error.clone()),
            description: refusal.and_then(|r| r.error_description),
        });
    }
    serde_json::from_slice(&body).map_err(|_| OauthError::MalformedTokenResponse)
}

/// A response without `scope` was granted as requested (RFC 6749 section 5.1).
fn missing_scopes(granted: Option<&str>) -> Vec<&'static str> {
    let Some(granted) = granted else {
        return Vec::new();
    };
    SCOPES
        .into_iter()
        .filter(|scope| !granted.split(' ').any(|granted| granted == *scope))
        .collect()
}

fn usable(sent: Instant, tokens: &TokenResponse) -> Result<Instant, OauthError> {
    let missing = missing_scopes(tokens.scope.as_deref());
    if !missing.is_empty() {
        return Err(OauthError::MissingScopes {
            missing: missing.join(", "),
        });
    }
    sent.checked_add(Duration::from_secs(tokens.expires_in))
        .ok_or(OauthError::MalformedTokenResponse)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .fold(0u8, |difference, (x, y)| difference | (x ^ y))
            == 0
}
