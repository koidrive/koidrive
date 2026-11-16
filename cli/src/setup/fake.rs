// SPDX-License-Identifier: Apache-2.0

use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use url::Url;

use koidrive_cloudflare::{Account, CLIENT_ID, Endpoints, Login};

use super::Session;

pub struct Reply {
    pub method: &'static str,
    pub path: String,
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub jurisdiction: Option<String>,
    pub body: Vec<u8>,
}

pub struct Fake {
    pub base: Url,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Fake {
    pub async fn start(replies: Vec<Reply>) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let replies = Arc::new(Mutex::new(replies));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = seen.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (replies, seen) = (replies.clone(), recorder.clone());
                let service =
                    service_fn(move |request| answer(request, replies.clone(), seen.clone()));
                tokio::spawn(http1::Builder::new().serve_connection(TokioIo::new(stream), service));
            }
        });
        Fake { base, seen }
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    pub fn sent(&self, methods: &[&str]) -> bool {
        self.seen()
            .iter()
            .any(|seen| methods.contains(&seen.method.as_str()))
    }
}

pub const ACCOUNT: &str = "0123456789abcdef";

pub fn cf(method: &'static str, path: &str, result: serde_json::Value) -> Reply {
    Reply {
        method,
        path: format!("/client/v4/accounts/{ACCOUNT}{path}"),
        status: 200,
        content_type: "application/json",
        body: serde_json::json!({ "success": true, "errors": [], "result": result })
            .to_string()
            .into_bytes(),
    }
}

pub fn proto(method: &'static str, path: &str, message: impl prost::Message) -> Reply {
    Reply {
        method,
        path: path.into(),
        status: 200,
        content_type: "application/x-protobuf",
        body: message.encode_to_vec(),
    }
}

pub fn tokens() -> Reply {
    Reply {
        method: "POST",
        path: "/oauth2/token".into(),
        status: 200,
        content_type: "application/json",
        body: serde_json::json!({
            "access_token": "access",
            "refresh_token": "refresh",
            "expires_in": 3600,
            "scope": koidrive_cloudflare::SCOPES.join(" "),
        })
        .to_string()
        .into_bytes(),
    }
}

pub fn revoked() -> Reply {
    Reply {
        method: "POST",
        path: "/oauth2/revoke".into(),
        status: 200,
        content_type: "application/json",
        body: b"{}".to_vec(),
    }
}

pub(super) async fn login(oauth: &Fake, api: Url) -> Session {
    let endpoint = |path: &str| oauth.base.join(path).unwrap();
    let endpoints = Endpoints {
        authorize: endpoint("oauth2/auth"),
        token: endpoint("oauth2/token"),
        revoke: endpoint("oauth2/revoke"),
    };
    let login = Login::new(endpoints, CLIENT_ID, 8976);
    let page = login.authorize_url();
    let (_, state) = page.query_pairs().find(|(key, _)| key == "state").unwrap();
    let mut redirect = Url::parse("http://127.0.0.1:8976/oauth/callback").unwrap();
    redirect
        .query_pairs_mut()
        .append_pair("state", &state)
        .append_pair("code", "code");
    let code = login.code_from(&redirect).unwrap();
    let http = reqwest::Client::new();
    let grant = login.exchange(&http, code).await.unwrap();
    Session {
        terminal: std::sync::Arc::new(crate::terminal::Terminal::new(true, false)),
        http,
        grant,
        api_base: api,
        account: Account {
            id: String::new(),
            name: String::new(),
        },
        subdomain: None,
        requested_subdomain: None,
    }
}

pub(super) async fn session(fake: &Fake) -> Session {
    let mut session = login(fake, fake.base.join("client/v4/").unwrap()).await;
    session.account = Account {
        id: ACCOUNT.into(),
        name: "test".into(),
    };
    session
}

async fn answer(
    request: Request<Incoming>,
    replies: Arc<Mutex<Vec<Reply>>>,
    seen: Arc<Mutex<Vec<Seen>>>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let method = request.method().to_string();
    let path = request
        .uri()
        .path_and_query()
        .map(|path| path.as_str().to_owned())
        .unwrap_or_default();
    let jurisdiction = request
        .headers()
        .get("cf-r2-jurisdiction")
        .map(|value| value.to_str().unwrap().to_owned());
    let body = request
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    seen.lock().unwrap().push(Seen {
        method: method.clone(),
        path: path.clone(),
        jurisdiction,
        body,
    });
    let mut replies = replies.lock().unwrap();
    let reply = match replies
        .iter()
        .position(|reply| reply.method == method && reply.path == path)
    {
        Some(index) => replies.remove(index),
        None => Reply {
            method: "",
            path,
            status: 500,
            content_type: "text/plain",
            body: format!("the fake has no reply for {method}").into_bytes(),
        },
    };
    Ok(Response::builder()
        .status(reply.status)
        .header("content-type", reply.content_type)
        .body(Full::new(Bytes::from(reply.body)))
        .unwrap())
}
