// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use koidrive_cloudflare::{
    AuthorizationCode, CLIENT_ID, Endpoints, Login, Loopback, OauthError, SCOPES, http_client,
};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use url::Url;

const PORT: u16 = 49152;

#[derive(Clone, Debug)]
struct Received {
    path: String,
    form: Vec<(String, String)>,
}

impl Received {
    fn field(&self, name: &str) -> Option<&str> {
        self.form
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn names(&self) -> Vec<&str> {
        self.form.iter().map(|(key, _)| key.as_str()).collect()
    }
}

struct FakeServer {
    endpoints: Endpoints,
    received: Arc<Mutex<Vec<Received>>>,
}

impl FakeServer {
    async fn start(responses: Vec<(u16, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let received = Arc::new(Mutex::new(Vec::new()));
        let log = received.clone();
        let mut responses = VecDeque::from(responses);
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let received = read_form(&mut stream).await;
                log.lock().unwrap().push(received);
                let (status, body) = responses.pop_front().unwrap_or((500, String::new()));
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            }
        });
        FakeServer {
            endpoints: Endpoints {
                authorize: base.join("/oauth2/auth").unwrap(),
                token: base.join("/oauth2/token").unwrap(),
                revoke: base.join("/oauth2/revoke").unwrap(),
            },
            received,
        }
    }

    fn received(&self) -> Vec<Received> {
        self.received.lock().unwrap().clone()
    }
}

async fn read_form(stream: &mut TcpStream) -> Received {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
        let read = stream.read(&mut chunk).await.unwrap();
        assert!(
            read > 0,
            "the client closed the connection before its request head ended"
        );
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8(buffer[..head_end].to_vec()).unwrap();
    let path = head.split(' ').nth(1).unwrap().to_owned();
    let length = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map_or(0, |(_, value)| value.trim().parse::<usize>().unwrap());
    while buffer.len() < head_end + length {
        let read = stream.read(&mut chunk).await.unwrap();
        assert!(
            read > 0,
            "the client closed the connection before its body ended"
        );
        buffer.extend_from_slice(&chunk[..read]);
    }
    let form = url::form_urlencoded::parse(&buffer[head_end..head_end + length])
        .into_owned()
        .collect();
    Received { path, form }
}

fn tokens(access: &str, refresh: &str, scope: &str) -> (u16, String) {
    let body = serde_json::json!({
        "access_token": access,
        "refresh_token": refresh,
        "token_type": "bearer",
        "expires_in": 3600,
        "scope": scope,
    });
    (200, body.to_string())
}

fn all_scopes() -> String {
    SCOPES.join(" ")
}

fn ok() -> (u16, String) {
    (200, String::new())
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

fn query(url: &Url) -> HashMap<String, String> {
    url.query_pairs().into_owned().collect()
}

fn state_of(login: &Login) -> String {
    query(&login.authorize_url())["state"].clone()
}

fn callback(port: u16, query: &str) -> Url {
    Url::parse(&format!("http://127.0.0.1:{port}/oauth/callback?{query}")).unwrap()
}

async fn code_sent(server: &FakeServer, login: Login, code: AuthorizationCode) -> String {
    login.exchange(&client(), code).await.unwrap();
    server
        .received()
        .last()
        .unwrap()
        .field("code")
        .unwrap()
        .to_owned()
}

async fn get(port: u16, target: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let request =
        format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: text/html\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let status = response.split(' ').nth(1).unwrap().parse().unwrap();
    let body = response.split_once("\r\n\r\n").unwrap().1.to_owned();
    (status, body)
}

#[test]
fn the_cloudflare_endpoints_are_the_documented_ones() {
    let endpoints = Endpoints::cloudflare();
    assert_eq!(
        endpoints.authorize.as_str(),
        "https://dash.cloudflare.com/oauth2/auth"
    );
    assert_eq!(
        endpoints.token.as_str(),
        "https://dash.cloudflare.com/oauth2/token"
    );
    assert_eq!(
        endpoints.revoke.as_str(),
        "https://dash.cloudflare.com/oauth2/revoke"
    );
}

#[test]
fn authorize_url_carries_every_parameter() {
    let login = Login::new(Endpoints::cloudflare(), CLIENT_ID, PORT);
    let url = login.authorize_url();
    assert_eq!(
        url.origin().ascii_serialization(),
        "https://dash.cloudflare.com"
    );
    assert_eq!(url.path(), "/oauth2/auth");
    let query = query(&url);
    assert_eq!(query.len(), 7, "{:?}", query.keys());
    assert_eq!(query["response_type"], "code");
    assert_eq!(query["client_id"], CLIENT_ID);
    assert_eq!(
        query["redirect_uri"],
        format!("http://127.0.0.1:{PORT}/oauth/callback")
    );
    assert_eq!(
        query["scope"],
        "account-settings.read workers-scripts.admin workers-r2.write offline_access"
    );
    assert_eq!(query["code_challenge_method"], "S256");
    assert_eq!(query["state"].len(), 22, "16 random bytes in base64url");
    assert_eq!(
        query["code_challenge"].len(),
        43,
        "a SHA-256 digest in base64url"
    );

    let other = query_of_new_login();
    assert_ne!(other["state"], query["state"]);
    assert_ne!(other["code_challenge"], query["code_challenge"]);
}

fn query_of_new_login() -> HashMap<String, String> {
    query(&Login::new(Endpoints::cloudflare(), CLIENT_ID, PORT).authorize_url())
}

#[tokio::test]
async fn exchange_posts_the_code_and_the_verifier_of_the_s256_challenge() {
    let server = FakeServer::start(vec![tokens("access-1", "refresh-1", &all_scopes())]).await;
    let login = Login::new(server.endpoints.clone(), CLIENT_ID, PORT);
    let authorize = query(&login.authorize_url());
    let pasted = callback(
        PORT,
        &format!(
            "code=the-code&scope={}&state={}",
            all_scopes(),
            authorize["state"]
        ),
    );
    let code = login.code_from(&pasted).unwrap();

    let grant = login.exchange(&client(), code).await.unwrap();

    assert_eq!(grant.access_token().expose(), "access-1");
    assert!(!grant.expires_within(Duration::from_secs(3500)));
    assert!(grant.expires_within(Duration::from_secs(3600)));
    let received = server.received();
    assert_eq!(received.len(), 1);
    let exchange = &received[0];
    assert_eq!(exchange.path, "/oauth2/token");
    assert_eq!(
        exchange.names(),
        [
            "grant_type",
            "code",
            "redirect_uri",
            "client_id",
            "code_verifier"
        ]
    );
    assert_eq!(exchange.field("grant_type"), Some("authorization_code"));
    assert_eq!(exchange.field("code"), Some("the-code"));
    assert_eq!(
        exchange.field("redirect_uri"),
        Some(authorize["redirect_uri"].as_str())
    );
    assert_eq!(exchange.field("client_id"), Some(CLIENT_ID));
    let verifier = exchange.field("code_verifier").unwrap();
    assert_eq!(verifier.len(), 43, "32 random bytes in base64url");
    assert_eq!(
        URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
        authorize["code_challenge"]
    );
}

#[test]
fn code_from_refuses_a_wrong_or_missing_state() {
    let login = Login::new(Endpoints::cloudflare(), CLIENT_ID, PORT);
    for query in [
        "code=c&state=wrong",
        "code=c",
        "error=access_denied&state=wrong",
        "error=access_denied",
    ] {
        let refused = login.code_from(&callback(PORT, query));
        assert!(
            matches!(refused, Err(OauthError::StateMismatch)),
            "{query}: {refused:?}"
        );
    }
}

#[test]
fn code_from_refuses_a_path_other_than_the_callback() {
    let login = Login::new(Endpoints::cloudflare(), CLIENT_ID, PORT);
    let state = state_of(&login);
    for path in ["/favicon.ico", "/", "/oauth/callback/", "/oauth"] {
        let url = Url::parse(&format!(
            "http://127.0.0.1:{PORT}{path}?code=c&state={state}"
        ))
        .unwrap();
        let refused = login.code_from(&url);
        assert!(
            matches!(refused, Err(OauthError::NotCallback)),
            "{path}: {refused:?}"
        );
    }
}

#[test]
fn code_from_turns_an_error_redirect_into_an_error() {
    let login = Login::new(Endpoints::cloudflare(), CLIENT_ID, PORT);
    let state = state_of(&login);
    let denied = login
        .code_from(&callback(
            PORT,
            &format!("error=access_denied&error_description=The+user+denied+access&state={state}"),
        ))
        .unwrap_err();
    let OauthError::Denied { error, description } = &denied else {
        panic!("expected Denied, got {denied:?}");
    };
    assert_eq!(error, "access_denied");
    assert_eq!(description.as_deref(), Some("The user denied access"));
    assert_eq!(
        denied.to_string(),
        "Cloudflare did not authorize the login: access_denied (The user denied access)"
    );

    let empty = login.code_from(&callback(PORT, &format!("code=&state={state}")));
    assert!(matches!(empty, Err(OauthError::MissingCode)), "{empty:?}");
}

#[tokio::test]
async fn loopback_round_trip_returns_the_callback_code() {
    let server = FakeServer::start(vec![tokens("access-1", "refresh-1", &all_scopes())]).await;
    let loopback = Loopback::bind(None).await.unwrap();
    let port = loopback.port();
    let login = Login::new(server.endpoints.clone(), CLIENT_ID, port);
    let state = state_of(&login);

    let (code, (favicon, page)) = tokio::join!(loopback.callback(&login), async {
        let favicon = get(port, "/favicon.ico").await;
        (
            favicon,
            get(
                port,
                &format!("/oauth/callback?code=the-code&scope=x&state={state}"),
            )
            .await,
        )
    });

    assert_eq!(favicon.0, 404);
    assert_eq!(page.0, 200);
    assert!(
        page.1
            .contains("You can close this tab and return to the terminal."),
        "{}",
        page.1
    );
    assert_eq!(code_sent(&server, login, code.unwrap()).await, "the-code");
}

#[tokio::test]
async fn loopback_answers_a_wrong_state_with_400_and_keeps_waiting() {
    let server = FakeServer::start(vec![tokens("access-1", "refresh-1", &all_scopes())]).await;
    let loopback = Loopback::bind(None).await.unwrap();
    let port = loopback.port();
    let login = Login::new(server.endpoints.clone(), CLIENT_ID, port);
    let state = state_of(&login);

    let (code, answers) = tokio::join!(loopback.callback(&login), async {
        vec![
            get(port, "/oauth/callback?code=forged&state=wrong").await.0,
            get(port, "/oauth/callback?error=access_denied").await.0,
            get(
                port,
                &format!("/oauth/callback?code=the-code&state={state}"),
            )
            .await
            .0,
        ]
    });

    assert_eq!(answers, [400, 400, 200]);
    assert_eq!(code_sent(&server, login, code.unwrap()).await, "the-code");
}

#[tokio::test]
async fn loopback_ends_the_wait_on_a_denied_login() {
    let loopback = Loopback::bind(None).await.unwrap();
    let port = loopback.port();
    let login = Login::new(Endpoints::cloudflare(), CLIENT_ID, port);
    let state = state_of(&login);

    let target = format!("/oauth/callback?error=access_denied&state={state}");
    let (code, page) = tokio::join!(loopback.callback(&login), get(port, &target));

    assert_eq!(page.0, 200);
    assert!(
        matches!(code, Err(OauthError::Denied { .. })),
        "{:?}",
        code.err()
    );
}

#[tokio::test]
async fn loopback_binds_the_given_port_on_127_0_0_1_only() {
    let mut bound = None;
    for _ in 0..32 {
        let candidate = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let free = candidate.local_addr().unwrap().port();
        let ipv6 = tokio::net::TcpSocket::new_v6().unwrap();
        match ipv6.bind(std::net::SocketAddr::from((
            std::net::Ipv6Addr::LOCALHOST,
            free,
        ))) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(error) => panic!("{error}"),
        }
        drop(candidate);
        match Loopback::bind(Some(free)).await {
            Ok(loopback) => {
                assert_eq!(loopback.port(), free);
                bound = Some((loopback, ipv6));
                break;
            }
            Err(OauthError::Listen { source, .. })
                if source.kind() == std::io::ErrorKind::AddrInUse => {}
            Err(error) => panic!("{error}"),
        }
    }
    let (loopback, _ipv6) =
        bound.expect("an explicit loopback port could not be bound after 32 collisions");
    let free = loopback.port();
    assert!(TcpStream::connect(("127.0.0.1", free)).await.is_ok());
    assert!(
        !matches!(
            tokio::time::timeout(Duration::from_secs(1), TcpStream::connect(("::1", free))).await,
            Ok(Ok(_))
        ),
        "the listener must not answer on ::1"
    );

    let taken = Loopback::bind(Some(free)).await;
    assert!(
        matches!(taken, Err(OauthError::Listen { port, .. }) if port == free),
        "{:?}",
        taken.err()
    );
}

#[tokio::test]
async fn exchange_fails_on_a_missing_scope_and_revokes_the_grant() {
    let granted = "account-settings.read workers-scripts.admin offline_access";
    let server =
        FakeServer::start(vec![tokens("access-1", "refresh-1", granted), ok(), ok()]).await;
    let login = Login::new(server.endpoints.clone(), CLIENT_ID, PORT);
    let code = login
        .code_from(&callback(
            PORT,
            &format!("code=c&state={}", state_of(&login)),
        ))
        .unwrap();

    let refused = login.exchange(&client(), code).await.err().unwrap();

    assert!(
        matches!(&refused, OauthError::MissingScopes { missing } if missing == "workers-r2.write"),
        "{refused:?}"
    );
    assert!(
        refused.to_string().contains("without workers-r2.write;"),
        "{refused}"
    );
    let paths: Vec<_> = server
        .received()
        .into_iter()
        .map(|received| received.path)
        .collect();
    assert_eq!(paths, ["/oauth2/token", "/oauth2/revoke", "/oauth2/revoke"]);
}

#[tokio::test]
async fn token_errors_show_only_error_and_error_description() {
    let refusal = serde_json::json!({
        "error": "invalid_grant",
        "error_description": "The authorization code was already used",
        "access_token": "leaked-access-token",
    });
    let malformed = serde_json::json!({ "access_token": "leaked-access-token" });
    let server = FakeServer::start(vec![
        (400, refusal.to_string()),
        (200, malformed.to_string()),
    ])
    .await;

    for expected in [
        "invalid_grant (The authorization code was already used)",
        "could not be read",
    ] {
        let login = Login::new(server.endpoints.clone(), CLIENT_ID, PORT);
        let code = login
            .code_from(&callback(
                PORT,
                &format!("code=c&state={}", state_of(&login)),
            ))
            .unwrap();
        let message = login
            .exchange(&client(), code)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(message.contains(expected), "{message}");
        assert!(!message.contains("leaked"), "{message}");
    }
}

async fn grant_from(server: &FakeServer) -> koidrive_cloudflare::Grant {
    let login = Login::new(server.endpoints.clone(), CLIENT_ID, PORT);
    let code = login
        .code_from(&callback(
            PORT,
            &format!("code=c&state={}", state_of(&login)),
        ))
        .unwrap();
    login.exchange(&client(), code).await.unwrap()
}

#[tokio::test]
async fn refresh_replaces_both_tokens() {
    let server = FakeServer::start(vec![
        tokens("access-1", "refresh-1", &all_scopes()),
        tokens("access-2", "refresh-2", &all_scopes()),
        ok(),
        ok(),
    ])
    .await;
    let mut grant = grant_from(&server).await;

    grant.refresh(&client()).await.unwrap();
    assert_eq!(grant.access_token().expose(), "access-2");
    assert!(!grant.expires_within(Duration::from_secs(3500)));
    grant.revoke(&client()).await.unwrap();

    let received = server.received();
    let refresh = &received[1];
    assert_eq!(refresh.path, "/oauth2/token");
    assert_eq!(
        refresh.names(),
        ["grant_type", "refresh_token", "client_id"]
    );
    assert_eq!(refresh.field("grant_type"), Some("refresh_token"));
    assert_eq!(refresh.field("refresh_token"), Some("refresh-1"));
    assert_eq!(refresh.field("client_id"), Some(CLIENT_ID));
    let revoked: Vec<_> = received[2..]
        .iter()
        .map(|received| received.field("token").unwrap())
        .collect();
    assert_eq!(revoked, ["refresh-2", "access-2"]);
}

#[tokio::test]
async fn a_refresh_without_a_required_scope_revokes_the_new_tokens() {
    let server = FakeServer::start(vec![
        tokens("access-1", "refresh-1", &all_scopes()),
        tokens("access-2", "refresh-2", "account-settings.read"),
        ok(),
        ok(),
    ])
    .await;
    let mut grant = grant_from(&server).await;

    let refused = grant.refresh(&client()).await.err().unwrap();

    assert!(
        matches!(&refused, OauthError::MissingScopes { missing } if missing.contains("workers-scripts.admin")),
        "{refused:?}"
    );
    assert!(grant.expires_within(Duration::ZERO));
    let again = grant.refresh(&client()).await;
    assert!(matches!(again, Err(OauthError::Spent)), "{:?}", again.err());
    let received = server.received();
    let revoked: Vec<_> = received[2..]
        .iter()
        .map(|received| received.field("token").unwrap())
        .collect();
    assert_eq!(revoked, ["refresh-2", "access-2"]);
}

#[tokio::test]
async fn a_later_revoke_retries_the_new_refresh_token_of_a_refused_scope() {
    let server = FakeServer::start(vec![
        tokens("access-1", "refresh-1", &all_scopes()),
        tokens("access-2", "refresh-2", "account-settings.read"),
        (503, String::new()),
        ok(),
        ok(),
        ok(),
    ])
    .await;
    let mut grant = grant_from(&server).await;

    let refused = grant.refresh(&client()).await.err().unwrap();
    assert!(
        matches!(refused, OauthError::MissingScopes { .. }),
        "{refused:?}"
    );
    grant.revoke(&client()).await.unwrap();

    let revoked: Vec<_> = server.received()[2..]
        .iter()
        .map(|received| received.field("token").unwrap().to_owned())
        .collect();
    assert_eq!(revoked, ["refresh-2", "access-2", "refresh-2", "access-2"]);
}

fn endless_tokens(access: &str, refresh: &str) -> (u16, String) {
    let body = serde_json::json!({
        "access_token": access,
        "refresh_token": refresh,
        "token_type": "bearer",
        "expires_in": u64::MAX,
        "scope": all_scopes(),
    });
    (200, body.to_string())
}

#[tokio::test]
async fn tokens_with_an_unusable_expiry_are_revoked_on_exchange_and_refresh() {
    let server = FakeServer::start(vec![
        endless_tokens("access-0", "refresh-0"),
        ok(),
        ok(),
        tokens("access-1", "refresh-1", &all_scopes()),
        endless_tokens("access-2", "refresh-2"),
        ok(),
        ok(),
    ])
    .await;
    let login = Login::new(server.endpoints.clone(), CLIENT_ID, PORT);
    let code = login
        .code_from(&callback(
            PORT,
            &format!("code=c&state={}", state_of(&login)),
        ))
        .unwrap();
    let refused = login.exchange(&client(), code).await.err().unwrap();
    assert!(
        matches!(refused, OauthError::MalformedTokenResponse),
        "{refused:?}"
    );

    let mut grant = grant_from(&server).await;
    let refused = grant.refresh(&client()).await.err().unwrap();
    assert!(
        matches!(refused, OauthError::MalformedTokenResponse),
        "{refused:?}"
    );
    assert!(grant.expires_within(Duration::ZERO));
    let again = grant.refresh(&client()).await;
    assert!(matches!(again, Err(OauthError::Spent)), "{:?}", again.err());

    let revoked: Vec<_> = server
        .received()
        .iter()
        .filter(|received| received.path == "/oauth2/revoke")
        .map(|received| received.field("token").unwrap().to_owned())
        .collect();
    assert_eq!(revoked, ["refresh-0", "access-0", "refresh-2", "access-2"]);
    assert_eq!(
        server.received().len(),
        7,
        "a spent grant sends no second refresh"
    );
}

#[tokio::test]
async fn a_failed_refresh_leaves_the_grant_unusable() {
    let refusal = serde_json::json!({ "error": "invalid_grant" }).to_string();
    let server = FakeServer::start(vec![
        tokens("access-1", "refresh-1", &all_scopes()),
        (400, refusal),
        ok(),
        ok(),
    ])
    .await;
    let mut grant = grant_from(&server).await;

    let failed = grant.refresh(&client()).await;
    assert!(
        matches!(failed, Err(OauthError::TokenRefused { status: 400, .. })),
        "{:?}",
        failed.err()
    );
    assert!(grant.expires_within(Duration::ZERO));
    let again = grant.refresh(&client()).await;
    assert!(matches!(again, Err(OauthError::Spent)), "{:?}", again.err());
    assert_eq!(
        server.received().len(),
        2,
        "a spent grant sends no second refresh"
    );

    grant.revoke(&client()).await.unwrap();
    let revoked: Vec<_> = server.received()[2..]
        .iter()
        .map(|received| received.field("token").unwrap().to_owned())
        .collect();
    assert_eq!(revoked, ["refresh-1", "access-1"]);
}

#[tokio::test]
async fn revoke_sends_the_refresh_token_then_the_access_token() {
    let server = FakeServer::start(vec![
        tokens("access-1", "refresh-1", &all_scopes()),
        (503, String::new()),
        ok(),
    ])
    .await;
    let grant = grant_from(&server).await;

    let failed = grant.revoke(&client()).await;

    assert!(
        matches!(
            failed,
            Err(OauthError::RevokeRefused {
                token: "refresh token",
                status: 503
            })
        ),
        "{:?}",
        failed.err()
    );
    let received = server.received();
    assert_eq!(
        received.len(),
        3,
        "the access token is revoked even when the refresh token was not"
    );
    for (revoke, (token, hint)) in received[1..]
        .iter()
        .zip([("refresh-1", "refresh_token"), ("access-1", "access_token")])
    {
        assert_eq!(revoke.path, "/oauth2/revoke");
        assert_eq!(revoke.field("token"), Some(token));
        assert_eq!(revoke.field("token_type_hint"), Some(hint));
        assert_eq!(revoke.field("client_id"), Some(CLIENT_ID));
    }
}

#[tokio::test]
async fn http_client_refuses_plain_http() {
    let server = FakeServer::start(Vec::new()).await;
    let refused = http_client()
        .unwrap()
        .post(server.endpoints.token.clone())
        .send()
        .await;
    assert!(refused.is_err());
    assert!(server.received().is_empty());
}
