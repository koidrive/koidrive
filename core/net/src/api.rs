// SPDX-License-Identifier: Apache-2.0

use koidrive_proto::v1::{
    AccessToken, ClaimRequest, CreateShareLinkRequest, CreateSpaceRequest, DeviceCredentials,
    DeviceInfo, DeviceList, PairCompleteRequest, PairStartResponse, RewindRequest, RewindResult,
    RpcRequest, RpcResponse, ServerInfo, ShareLink, ShareLinkList, SpaceInfo, SpaceList,
    SpaceUsage, TokenRequest, rpc_request,
};
use std::collections::HashSet;
use std::time::Duration;

use prost::Message;
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use reqwest::{Method, RequestBuilder, redirect};
use url::{Host, Url};

use crate::auth::Auth;
use crate::error::{ApiError, ServerError};

pub(crate) const PROTOBUF: &str = "application/x-protobuf";

pub(crate) fn is_protobuf(content_type: &str) -> bool {
    content_type
        .split(';')
        .next()
        .is_some_and(|media| media.trim().eq_ignore_ascii_case(PROTOBUF))
}
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// RFC 0007 section 3 bounds each answer of fixed shape at 4 KiB in binary protobuf.
const MAX_ANSWER: usize = 4 << 10;
/// The most bytes read of one `SpaceList` page (RFC 0007 section 3).
const MAX_SPACE_PAGE: usize = 32 << 10;
/// The most bytes read of one `DeviceList` page (RFC 0007 section 3).
const MAX_DEVICE_PAGE: usize = 64 << 10;
const MAX_SHARE_LINK_PAGE: usize = 64 << 10;
const MAX_SPACE_LISTING: usize = 128 << 20;
const MAX_ERROR: usize = 1 << 20;

/// The HTTP endpoints of one Koidrive deployment (spec v0 section 15.1). Bodies are binary protobuf.
///
/// Credentials and tokens travel only over HTTPS, and the WebSocket only over WSS. Plain HTTP is allowed only for
/// a loopback address, and only through [`Api::local_development`]. Redirects are never followed, so a request
/// that carries a token or a refresh credential is never resent to another address or downgraded to HTTP.
#[derive(Clone, Debug)]
pub struct Api {
    http: reqwest::Client,
    uploads: reqwest::Client,
    base: Url,
}

impl Api {
    /// `base` is the deployment's HTTPS origin, for example `https://files.example.com`. A path is kept as a prefix.
    pub fn new(base: Url) -> Result<Self, ApiError> {
        Api::build(base, false)
    }

    /// Like [`Api::new`], but also accepts `http://` on a loopback address (`localhost`, `127.0.0.0/8`, `::1`),
    /// for a server under `wrangler dev`.
    pub fn local_development(base: Url) -> Result<Self, ApiError> {
        Api::build(base, true)
    }

    fn build(base: Url, local_development: bool) -> Result<Self, ApiError> {
        if base.host_str().is_none() || !permitted(&base, local_development) {
            let rule = if local_development {
                "an https URL, or an http URL on a loopback address"
            } else {
                "an https URL; plain http is only for local development on a loopback address"
            };
            return Err(ApiError::Url(format!("{base} is not {rule}")));
        }
        let mut base = base;
        base.set_query(None);
        base.set_fragment(None);
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        let client = |read_timeout: Option<Duration>| {
            let builder = reqwest::Client::builder()
                .user_agent(concat!("koidrive-net/", env!("CARGO_PKG_VERSION")))
                .https_only(!local_development)
                .redirect(redirect::Policy::none())
                .connect_timeout(CONNECT_TIMEOUT);
            match read_timeout {
                Some(timeout) => builder.read_timeout(timeout),
                None => builder,
            }
            .build()
            .map_err(ApiError::Transport)
        };
        Ok(Api {
            http: client(Some(READ_TIMEOUT))?,
            uploads: client(None)?,
            base,
        })
    }

    pub fn base(&self) -> &Url {
        &self.base
    }

    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// The client for chunk uploads. It has no read timeout, because reqwest starts that timer when it sends the
    /// request and would end an upload whose body takes longer than that to send.
    pub(crate) fn uploads(&self) -> &reqwest::Client {
        &self.uploads
    }

    /// The URL of the path `segments` below the base. An empty segment, `.`, `..`, or one that holds an ASCII control
    /// character is refused: the `url` crate drops tabs and line breaks, so `.\t.` would be read as `..`.
    pub(crate) fn url(&self, segments: &[&str]) -> Result<Url, ApiError> {
        let another_path = segments.iter().find(|segment| {
            matches!(**segment, "" | "." | "..") || segment.contains(|c: char| c.is_ascii_control())
        });
        if let Some(segment) = another_path {
            return Err(ApiError::PathSegment((*segment).to_owned()));
        }
        let mut url = self.base.clone();
        url.path_segments_mut()
            .expect("an http URL has a path")
            .pop_if_empty()
            .extend(segments);
        Ok(url)
    }

    /// `GET /v1/server`.
    pub async fn server_info(&self) -> Result<ServerInfo, ApiError> {
        self.send(
            self.http.get(self.url(&["v1", "server"])?),
            "ServerInfo",
            MAX_ANSWER,
        )
        .await
    }

    /// `GET /setup`: has a server without an owner write a one-time claim code to its log, unless a valid code is
    /// already there.
    pub async fn request_claim_code(&self) -> Result<(), ApiError> {
        let response = self
            .http
            .get(self.url(&["setup"])?)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(ApiError::Transport)?;
        match response.status() {
            status if status.is_success() => Ok(()),
            status => Err(ApiError::Status {
                status: status.as_u16(),
            }),
        }
    }

    /// `POST /v1/claim`: registers the first owner with the one-time claim code from the Worker log.
    pub async fn claim(&self, request: &ClaimRequest) -> Result<DeviceCredentials, ApiError> {
        self.send(
            self.post(self.url(&["v1", "claim"])?, request),
            "DeviceCredentials",
            MAX_ANSWER,
        )
        .await
    }

    /// `POST /v1/pair/complete`, called by the new device with the code an already paired device received.
    pub async fn pair_complete(
        &self,
        request: &PairCompleteRequest,
    ) -> Result<DeviceCredentials, ApiError> {
        self.send(
            self.post(self.url(&["v1", "pair", "complete"])?, request),
            "DeviceCredentials",
            MAX_ANSWER,
        )
        .await
    }

    /// `POST /v1/token`: exchanges the refresh credential for an access token.
    pub async fn token(&self, request: &TokenRequest) -> Result<AccessToken, ApiError> {
        self.send(
            self.post(self.url(&["v1", "token"])?, request),
            "AccessToken",
            MAX_ANSWER,
        )
        .await
    }

    /// `POST /v1/pair/start`, called by an already paired device.
    pub async fn pair_start(&self, auth: &Auth) -> Result<PairStartResponse, ApiError> {
        self.authed(
            auth,
            Method::POST,
            self.url(&["v1", "pair", "start"])?,
            None,
            "PairStartResponse",
            MAX_ANSWER,
        )
        .await
    }

    /// `GET /v1/spaces`, following `next_cursor` to the last page (RFC 0007 section 2). Each page is read up to
    /// `MAX_SPACE_PAGE`, and to no more than `MAX_SPACE_LISTING` minus the encoded size of the pages kept so far, so the
    /// listing never holds more than `MAX_SPACE_LISTING` bytes at once. A page that repeats any earlier cursor ends the listing as undecodable.
    pub async fn list_spaces(&self, auth: &Auth) -> Result<SpaceList, ApiError> {
        let mut list = SpaceList::default();
        let mut read = 0;
        let mut seen = std::collections::HashSet::new();
        loop {
            let mut url = self.url(&["v1", "spaces"])?;
            if !list.next_cursor.is_empty() {
                url.query_pairs_mut()
                    .append_pair("cursor", &list.next_cursor);
            }
            let page: SpaceList = self
                .authed(
                    auth,
                    Method::GET,
                    url,
                    None,
                    "SpaceList",
                    MAX_SPACE_PAGE.min(MAX_SPACE_LISTING - read),
                )
                .await?;
            read += page.encoded_len();
            if !page.next_cursor.is_empty() && !seen.insert(page.next_cursor.clone()) {
                return Err(ApiError::Decode("SpaceList"));
            }
            list.spaces.extend(page.spaces);
            list.next_cursor = page.next_cursor;
            if list.next_cursor.is_empty() {
                return Ok(list);
            }
        }
    }

    /// `POST /v1/spaces`.
    pub async fn create_space(
        &self,
        auth: &Auth,
        request: &CreateSpaceRequest,
    ) -> Result<SpaceInfo, ApiError> {
        self.authed(
            auth,
            Method::POST,
            self.url(&["v1", "spaces"])?,
            Some(request.encode_to_vec()),
            "SpaceInfo",
            MAX_ANSWER,
        )
        .await
    }

    pub async fn rewind(
        &self,
        auth: &Auth,
        space_id: &str,
        request: &RewindRequest,
    ) -> Result<RewindResult, ApiError> {
        self.authed(
            auth,
            Method::POST,
            self.url(&["v1", "spaces", space_id, "rewind"])?,
            Some(request.encode_to_vec()),
            "RewindResult",
            MAX_ANSWER,
        )
        .await
    }

    /// `POST /v1/spaces/{space_id}/rpc`: one call of the session protocol without a WebSocket. The server answers with the
    /// call's result, or with an `Error` in `RpcResponse.result`, which the caller reads like any other result.
    pub async fn rpc(
        &self,
        auth: &Auth,
        space_id: &str,
        call: rpc_request::Call,
    ) -> Result<RpcResponse, ApiError> {
        let request = RpcRequest {
            req_id: 1,
            call: Some(call),
        };
        self.authed(
            auth,
            Method::POST,
            self.url(&["v1", "spaces", space_id, "rpc"])?,
            Some(request.encode_to_vec()),
            "RpcResponse",
            crate::session::MAX_SERVER_MESSAGE,
        )
        .await
    }

    /// `GET /v1/spaces/{space_id}/usage`, owner only (RFC 0010): the bytes of the chunks the space holds, split by
    /// live files, the trash and old versions, with each chunk counted once.
    pub async fn space_usage(&self, auth: &Auth, space_id: &str) -> Result<SpaceUsage, ApiError> {
        let url = self.url(&["v1", "spaces", space_id, "usage"])?;
        self.authed(auth, Method::GET, url, None, "SpaceUsage", MAX_ANSWER)
            .await
    }

    /// `GET /v1/devices`: one page of the devices of this device's user, revoked ones included (RFC 0006 section 7).
    /// Pass an empty `cursor` for the first page and the previous page's `next_cursor` for the next.
    pub async fn list_devices(&self, auth: &Auth, cursor: &str) -> Result<DeviceList, ApiError> {
        let mut url = self.url(&["v1", "devices"])?;
        if !cursor.is_empty() {
            url.query_pairs_mut().append_pair("cursor", cursor);
        }
        self.authed(auth, Method::GET, url, None, "DeviceList", MAX_DEVICE_PAGE)
            .await
    }

    /// Every device of this device's user, revoked ones included: [`Api::list_devices`] page by page.
    pub async fn all_devices(&self, auth: &Auth) -> Result<Vec<DeviceInfo>, ApiError> {
        let mut devices = Vec::new();
        let mut cursor = String::new();
        let mut seen = HashSet::new();
        loop {
            let page = self.list_devices(auth, &cursor).await?;
            devices.extend(page.devices);
            if page.next_cursor.is_empty() {
                return Ok(devices);
            }
            if !seen.insert(page.next_cursor.clone()) {
                return Err(ApiError::Decode("DeviceList"));
            }
            cursor = page.next_cursor;
        }
    }

    /// `DELETE /v1/devices/{device_id}`: revokes another device of this device's user (RFC 0009). `device_id` is an ID
    /// from [`Api::list_devices`]. To revoke this device, use [`Api::revoke_self`].
    pub async fn revoke_device(&self, auth: &Auth, device_id: &str) -> Result<(), ApiError> {
        let url = self.url(&["v1", "devices", device_id])?;
        self.authed(auth, Method::DELETE, url, None, "empty reply", MAX_ANSWER)
            .await
    }

    /// `POST /v1/spaces/{space_id}/links`, owner only (RFC 0005 section 2). The answer holds the link's secret.
    pub async fn create_share_link(
        &self,
        auth: &Auth,
        space_id: &str,
        request: &CreateShareLinkRequest,
    ) -> Result<ShareLink, ApiError> {
        self.authed(
            auth,
            Method::POST,
            self.url(&["v1", "spaces", space_id, "links"])?,
            Some(request.encode_to_vec()),
            "ShareLink",
            MAX_ANSWER,
        )
        .await
    }

    /// `GET /v1/spaces/{space_id}/links`, owner only: one page of the space's links, ended ones included, newest first
    /// (RFC 0005 section 2). Pass an empty `cursor` for the first page and the previous page's `next_cursor` for the next.
    pub async fn list_share_links(
        &self,
        auth: &Auth,
        space_id: &str,
        cursor: &str,
    ) -> Result<ShareLinkList, ApiError> {
        let mut url = self.url(&["v1", "spaces", space_id, "links"])?;
        if !cursor.is_empty() {
            url.query_pairs_mut().append_pair("cursor", cursor);
        }
        self.authed(
            auth,
            Method::GET,
            url,
            None,
            "ShareLinkList",
            MAX_SHARE_LINK_PAGE,
        )
        .await
        .map_err(|error| match error {
            ApiError::Transport(error) => ApiError::Transport(error.without_url()),
            other => other,
        })
    }

    /// `DELETE /v1/spaces/{space_id}/links/{link_id}`, owner only (RFC 0005 section 2). Revoking a revoked link succeeds.
    /// The request URL holds the link's secret, so a transport error leaves the URL out.
    pub async fn revoke_share_link(
        &self,
        auth: &Auth,
        space_id: &str,
        link_id: &str,
    ) -> Result<(), ApiError> {
        let url = self.url(&["v1", "spaces", space_id, "links", link_id])?;
        self.authed(auth, Method::DELETE, url, None, "empty reply", MAX_ANSWER)
            .await
            .map_err(|error| match error {
                ApiError::Transport(error) => ApiError::Transport(error.without_url()),
                other => other,
            })
    }

    /// `DELETE /v1/devices/{device_id}` with this device's own ID, as a sign-out sends it (RFC 0009 section 5). If no other
    /// device of the user is active, the server refuses with `CONFIRMATION_REQUIRED` unless `confirm_last_device` is set,
    /// which a client sets only after telling the person that this is their last device.
    pub async fn revoke_self(
        &self,
        auth: &Auth,
        confirm_last_device: bool,
    ) -> Result<(), ApiError> {
        let mut url = self.url(&["v1", "devices", &auth.credentials().device_id])?;
        if confirm_last_device {
            url.query_pairs_mut().append_pair("confirm", "last-device");
        }
        self.authed(auth, Method::DELETE, url, None, "empty reply", MAX_ANSWER)
            .await
    }

    fn post(&self, url: Url, body: &impl Message) -> RequestBuilder {
        self.http
            .post(url)
            .header(CONTENT_TYPE, PROTOBUF)
            .body(body.encode_to_vec())
    }

    async fn authed<M: Message + Default>(
        &self,
        auth: &Auth,
        method: Method,
        url: Url,
        body: Option<Vec<u8>>,
        what: &'static str,
        limit: usize,
    ) -> Result<M, ApiError> {
        let mut token = auth.access_token().await?;
        let mut refreshed = false;
        loop {
            let mut request = self
                .http
                .request(method.clone(), url.clone())
                .bearer_auth(&token.token);
            if let Some(body) = &body {
                request = request.header(CONTENT_TYPE, PROTOBUF).body(body.clone());
            }
            match self.send(request, what, limit).await {
                Err(error) if error.is_unauthenticated() && !refreshed => {
                    auth.reject(&token.token).await;
                    token = auth.access_token().await?;
                    refreshed = true;
                }
                outcome => return outcome,
            }
        }
    }

    async fn send<M: Message + Default>(
        &self,
        request: RequestBuilder,
        what: &'static str,
        limit: usize,
    ) -> Result<M, ApiError> {
        let mut response = request
            .header(ACCEPT, PROTOBUF)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(ApiError::Transport)?;
        let status = response.status();
        let is_protobuf = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(is_protobuf);
        let limit = if status.is_success() {
            limit
        } else {
            MAX_ERROR
        };
        let mut body = Vec::new();
        while let Some(part) =
            response
                .chunk()
                .await
                .map_err(|error| match status.is_success() {
                    true => ApiError::Transport(error),
                    false => ApiError::Unreadable {
                        status: status.as_u16(),
                    },
                })?
        {
            if body.len() + part.len() > limit {
                return Err(match status.is_success() {
                    true => ApiError::TooLarge { limit },
                    false => ApiError::Unreadable {
                        status: status.as_u16(),
                    },
                });
            }
            body.extend_from_slice(&part);
        }
        if status.is_success() {
            return M::decode(body.as_slice()).map_err(|_| ApiError::Decode(what));
        }
        let status = status.as_u16();
        if !is_protobuf || body.is_empty() {
            return Err(ApiError::Status { status });
        }
        match ServerError::decode(&body) {
            Some(error) => Err(ApiError::Server { status, error }),
            None => Err(ApiError::Unreadable { status }),
        }
    }
}

fn permitted(url: &Url, local_development: bool) -> bool {
    match url.scheme() {
        "https" => true,
        "http" => local_development && loopback(url),
        _ => false,
    }
}

fn loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const MAX_NAME: usize = 255;
    use tokio::net::TcpListener;

    use super::*;

    #[test]
    fn protobuf_media_type_ignores_case_and_parameters() {
        for yes in [
            "application/x-protobuf",
            "Application/X-Protobuf",
            "application/x-protobuf; charset=binary",
            " application/x-protobuf ;q=1",
        ] {
            assert!(is_protobuf(yes), "{yes}");
        }
        for no in [
            "application/x-protobuffer",
            "text/plain",
            "",
            "application/json",
        ] {
            assert!(!is_protobuf(no), "{no}");
        }
    }

    #[test]
    fn plain_http_is_refused_outside_local_development_on_loopback() {
        let url = |text: &str| text.parse::<Url>().unwrap();
        for remote in [
            "http://203.0.113.1",
            "http://files.example.com",
            "ws://127.0.0.1",
            "ftp://127.0.0.1",
        ] {
            assert!(Api::new(url(remote)).is_err(), "{remote}");
            assert!(Api::local_development(url(remote)).is_err(), "{remote}");
        }
        for local in [
            "http://127.0.0.1:8787",
            "http://127.1.2.3",
            "http://[::1]:8787",
            "http://localhost:8787",
        ] {
            assert!(Api::new(url(local)).is_err(), "{local} needs the opt-in");
            assert!(Api::local_development(url(local)).is_ok(), "{local}");
        }
        assert!(Api::new(url("https://files.example.com/fb")).is_ok());
        assert!(Api::local_development(url("https://files.example.com")).is_ok());
    }

    async fn answer_once(reply: &'static str) -> (Url, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base: Url = format!("http://{}/", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let n = socket.read(&mut request).await.unwrap();
            socket.write_all(reply.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&request[..n]).into_owned()
        });
        (base, server)
    }

    #[tokio::test]
    async fn a_claim_code_is_requested_from_the_setup_page() {
        let (base, server) = answer_once(
            "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 3\r\nconnection: close\r\n\r\nok\n",
        )
        .await;
        Api::local_development(base)
            .unwrap()
            .request_claim_code()
            .await
            .unwrap();
        assert!(server.await.unwrap().starts_with("GET /setup HTTP/1.1"));
    }

    #[tokio::test]
    async fn a_setup_page_that_does_not_answer_ok_is_an_error() {
        let (base, server) =
            answer_once("HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .await;
        let outcome = Api::local_development(base)
            .unwrap()
            .request_claim_code()
            .await;
        assert!(
            matches!(outcome, Err(ApiError::Status { status: 404 })),
            "{outcome:?}"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn redirects_are_not_followed_with_credentials() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base: Url = format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let n = socket.read(&mut request).await.unwrap();
            let location = "http://203.0.113.1/v1/token";
            let reply = format!(
                "HTTP/1.1 307 Temporary Redirect\r\nlocation: {location}\r\ncontent-length: 0\r\n\r\n"
            );
            socket.write_all(reply.as_bytes()).await.unwrap();
            let mut rest = Vec::new();
            let _ = socket.read_to_end(&mut rest).await;
            String::from_utf8_lossy(&request[..n]).into_owned()
        });
        let api = Api::local_development(base).unwrap();
        let request = TokenRequest {
            device_id: "d".into(),
            refresh_credential: "secret".into(),
            ..Default::default()
        };
        let outcome = api.token(&request).await;
        assert!(
            matches!(outcome, Err(ApiError::Status { status: 307 })),
            "{outcome:?}"
        );
        drop(api);
        assert!(
            server.await.unwrap().starts_with("POST /v1/token"),
            "one request, not followed"
        );
    }

    #[tokio::test]
    async fn a_401_with_an_unknown_code_is_returned_without_a_new_token() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base: Url = format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let credentials = crate::auth::Credentials {
            user_id: "u".into(),
            device_id: "d".into(),
            refresh_credential: "r".into(),
        };
        let expires = std::time::SystemTime::now() + Duration::from_secs(3600);
        let token = AccessToken {
            token: "token".into(),
            expires_at_ms: expires
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64,
            ..Default::default()
        };
        let auth = Auth::new(Api::local_development(base).unwrap(), credentials).with_token(token);
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            assert!(socket.read(&mut request).await.unwrap() > 0);
            let body = koidrive_proto::v1::Error {
                code: 999,
                message: "refused".into(),
                ..Default::default()
            }
            .encode_to_vec();
            let head = format!(
                "HTTP/1.1 401 X\r\ncontent-type: {PROTOBUF}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            socket
                .write_all(&[head.as_bytes(), &body].concat())
                .await
                .unwrap();
            let again = tokio::time::timeout(Duration::from_millis(300), listener.accept()).await;
            assert!(again.is_err(), "no token request and no resend");
        });
        let outcome = auth.api().list_spaces(&auth).await;
        assert!(
            matches!(&outcome, Err(ApiError::Server { status: 401, error }) if error.0.code == 999 && error.message() == "refused"),
            "{outcome:?}"
        );
        server.await.unwrap();
    }
    async fn answering(status: u16, body: Option<Vec<u8>>) -> (Url, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            assert!(socket.read(&mut request).await.unwrap() > 0);
            let head = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: {PROTOBUF}\r\ntransfer-encoding: chunked\r\n\r\n"
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            let chunk =
                |part: &[u8]| [format!("{:x}\r\n", part.len()).as_bytes(), part, b"\r\n"].concat();
            match body {
                Some(body) => {
                    for part in body.chunks(16 << 10) {
                        socket.write_all(&chunk(part)).await.unwrap();
                    }
                    socket.write_all(b"0\r\n\r\n").await.unwrap();
                    let mut rest = Vec::new();
                    let _ = socket.read_to_end(&mut rest).await;
                }
                None => {
                    let part = chunk(&[0x0a; 16 << 10]);
                    while socket.write_all(&part).await.is_ok() {}
                }
            }
        });
        (base, server)
    }

    #[tokio::test]
    async fn an_endless_answer_or_error_is_not_read_past_its_limit() {
        for status in [200, 500] {
            let (base, server) = answering(status, None).await;
            let api = Api::local_development(base).unwrap();
            let outcome = tokio::time::timeout(Duration::from_secs(5), api.server_info())
                .await
                .expect("the read stops at the limit");
            assert!(
                match status {
                    200 => matches!(outcome, Err(ApiError::TooLarge { limit: MAX_ANSWER })),
                    _ => matches!(outcome, Err(ApiError::Unreadable { status: 500 })),
                },
                "{status}: {outcome:?}"
            );
            drop(api);
            tokio::time::timeout(Duration::from_secs(5), server)
                .await
                .expect("the client hung up")
                .unwrap();
        }
    }

    #[test]
    fn a_single_answer_cap_covers_the_largest_answer_the_server_can_send() {
        let id = "0190a0b0-0000-7000-8000-000000000001";
        let claims = {
            let name = "\\u0000".repeat(MAX_NAME);
            format!(r#"{{"u":"{id}","d":"{id}","n":"{name}","e":{}}}"#, i64::MAX).len()
        };
        let token = format!(
            "v1.{}.{}.{}",
            "k".repeat(8),
            "p".repeat(claims.div_ceil(3) * 4),
            "m".repeat(43)
        );
        let credentials = DeviceCredentials {
            user_id: id.into(),
            device_id: id.into(),
            refresh_credential: "r".repeat(43),
            access: Some(AccessToken {
                token,
                expires_at_ms: i64::MAX,
                ..Default::default()
            }),
            credential_expires_at_ms: i64::MAX,
            user_name: "\u{1}".repeat(MAX_NAME),
        };
        assert!(
            credentials.encoded_len() <= MAX_ANSWER,
            "{} > {MAX_ANSWER}",
            credentials.encoded_len()
        );
        let space = SpaceInfo {
            space_id: id.into(),
            name: "n".repeat(MAX_NAME),
            format_version: u32::MAX,
            created_at_ms: i64::MAX,
        };
        let page = SpaceList {
            spaces: vec![space.clone(); 100],
            next_cursor: u64::MAX.to_string(),
        };
        assert!(space.encoded_len() <= MAX_ANSWER);
        assert!(
            page.encoded_len() <= MAX_SPACE_PAGE,
            "{}",
            page.encoded_len()
        );
    }

    #[tokio::test]
    async fn an_error_that_is_not_an_error_message_is_unreadable_and_an_empty_one_is_left_to_its_status()
     {
        let (base, _server) = answering(500, Some(vec![0xff; 3])).await;
        let outcome = Api::local_development(base).unwrap().server_info().await;
        assert!(
            matches!(outcome, Err(ApiError::Unreadable { status: 500 })),
            "{outcome:?}"
        );
        let (base, _server) = answering(500, Some(Vec::new())).await;
        let outcome = Api::local_development(base).unwrap().server_info().await;
        assert!(
            matches!(outcome, Err(ApiError::Status { status: 500 })),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_space_list_page_may_be_longer_than_a_fixed_shape_answer_but_not_than_a_page() {
        let page = |length: usize| {
            SpaceList {
                spaces: vec![SpaceInfo {
                    name: "n".repeat(length),
                    ..Default::default()
                }],
                ..Default::default()
            }
            .encode_to_vec()
        };
        let spaces = page(MAX_ANSWER);
        assert!(spaces.len() > MAX_ANSWER && spaces.len() <= MAX_SPACE_PAGE);
        let (base, _server) = answering(200, Some(spaces.clone())).await;
        let auth = signed_in(base);
        let list = auth.api().list_spaces(&auth).await.unwrap();
        assert_eq!(list.spaces[0].name.len(), MAX_ANSWER);

        let (base, _server) = answering(200, Some(spaces)).await;
        let outcome = Api::local_development(base).unwrap().server_info().await;
        assert!(
            matches!(outcome, Err(ApiError::TooLarge { limit: MAX_ANSWER })),
            "{outcome:?}"
        );

        let (base, _server) = answering(200, Some(page(MAX_SPACE_PAGE))).await;
        let auth = signed_in(base);
        let outcome = auth.api().list_spaces(&auth).await;
        assert!(
            matches!(
                outcome,
                Err(ApiError::TooLarge {
                    limit: MAX_SPACE_PAGE
                })
            ),
            "{outcome:?}"
        );

        let devices = DeviceList {
            devices: vec![koidrive_proto::v1::DeviceInfo {
                name: "n".repeat(MAX_DEVICE_PAGE),
                ..Default::default()
            }],
            ..Default::default()
        };
        let (base, _server) = answering(200, Some(devices.encode_to_vec())).await;
        let auth = signed_in(base);
        let outcome = auth.api().list_devices(&auth, "").await;
        assert!(
            matches!(
                outcome,
                Err(ApiError::TooLarge {
                    limit: MAX_DEVICE_PAGE
                })
            ),
            "{outcome:?}"
        );
    }

    fn signed_in(base: Url) -> Auth {
        let credentials = crate::auth::Credentials {
            user_id: "u".into(),
            device_id: "d".into(),
            refresh_credential: "r".into(),
        };
        let expires = std::time::SystemTime::now() + Duration::from_secs(3600);
        let token = AccessToken {
            token: "token".into(),
            expires_at_ms: expires
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64,
            ..Default::default()
        };
        Auth::new(Api::local_development(base).unwrap(), credentials).with_token(token)
    }

    async fn paging(
        pages: Vec<(Vec<&'static str>, &'static str)>,
    ) -> (Url, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (ids, cursor) in pages {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0u8; 4096];
                let read = socket.read(&mut request).await.unwrap();
                let head = String::from_utf8_lossy(&request[..read]);
                requests.push(head.lines().next().unwrap().to_owned());
                let body = SpaceList {
                    spaces: ids
                        .into_iter()
                        .map(|id| SpaceInfo {
                            space_id: id.into(),
                            ..Default::default()
                        })
                        .collect(),
                    next_cursor: cursor.into(),
                }
                .encode_to_vec();
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: {PROTOBUF}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(head.as_bytes()).await.unwrap();
                socket.write_all(&body).await.unwrap();
            }
            requests
        });
        (base, server)
    }

    #[tokio::test]
    async fn list_spaces_follows_the_cursor_to_the_last_page() {
        let (base, server) = paging(vec![(vec!["a", "b&c"], "b&c"), (vec!["d"], "")]).await;
        let auth = signed_in(base);
        let list = auth.api().list_spaces(&auth).await.unwrap();
        let ids: Vec<_> = list
            .spaces
            .iter()
            .map(|space| space.space_id.as_str())
            .collect();
        assert_eq!(ids, ["a", "b&c", "d"]);
        assert_eq!(list.next_cursor, "");
        assert_eq!(
            server.await.unwrap(),
            [
                "GET /v1/spaces HTTP/1.1",
                "GET /v1/spaces?cursor=b%26c HTTP/1.1"
            ]
        );
    }

    #[tokio::test]
    async fn space_usage_reads_the_space_s_usage_route() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let read = socket.read(&mut request).await.unwrap();
            let body = SpaceUsage {
                live_files: 2,
                live_bytes: 300,
                unreferenced_bytes: 7,
                ..Default::default()
            }
            .encode_to_vec();
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: {PROTOBUF}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(&body).await.unwrap();
            String::from_utf8_lossy(&request[..read]).into_owned()
        });
        let auth = signed_in(base);
        let usage = auth.api().space_usage(&auth, "s").await.unwrap();
        assert_eq!(
            (usage.live_files, usage.live_bytes, usage.unreferenced_bytes),
            (2, 300, 7)
        );
        let request = server.await.unwrap();
        assert!(
            request.starts_with("GET /v1/spaces/s/usage HTTP/1.1"),
            "{request}"
        );
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer token"),
            "{request}"
        );
    }

    #[tokio::test]
    async fn an_id_is_sent_as_one_path_segment() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let read = socket.read(&mut request).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 404 X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .await
                .unwrap();
            let head = String::from_utf8_lossy(&request[..read]);
            head.lines().next().unwrap().to_owned()
        });
        let auth = signed_in(base);
        let outcome = (auth.api())
            .space_usage(&auth, "../devices/d?confirm=last-device#")
            .await;
        assert!(
            matches!(outcome, Err(ApiError::Status { status: 404 })),
            "{outcome:?}"
        );
        assert_eq!(
            server.await.unwrap(),
            "GET /v1/spaces/..%2Fdevices%2Fd%3Fconfirm=last-device%23/usage HTTP/1.1"
        );
    }

    #[tokio::test]
    async fn an_id_that_would_name_another_path_is_refused_before_anything_is_sent() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let auth = signed_in(base);
        for id in ["", ".", "..", ".\t.", "..\n", "\r..", "d\n1"] {
            let outcome = auth.api().revoke_device(&auth, id).await;
            assert!(
                matches!(&outcome, Err(ApiError::PathSegment(segment)) if segment == id),
                "{id:?}: {outcome:?}"
            );
        }
        let sent = tokio::time::timeout(Duration::from_millis(100), listener.accept()).await;
        assert!(sent.is_err(), "nothing was sent");
    }

    #[tokio::test]
    async fn a_failed_share_listing_keeps_the_cursor_out_of_its_error() {
        let base: Url = format!("http://{}", koidrive_test_support::reset_tcp_address())
            .parse()
            .unwrap();
        let auth = signed_in(base);
        let link_id = "AAAAAAAAAAAAAAAAAAAAAA";
        let cursor = format!("1000.{link_id}");
        let outcome = auth.api().list_share_links(&auth, "s1", &cursor).await;
        let Err(error @ ApiError::Transport(_)) = outcome else {
            panic!("{outcome:?}");
        };
        let mut shown = format!("{error} {error:?}");
        let mut source = std::error::Error::source(&error);
        while let Some(cause) = source {
            shown.push_str(&format!(" {cause} {cause:?}"));
            source = cause.source();
        }
        assert!(!shown.contains(link_id), "{shown}");
    }

    #[tokio::test]
    async fn a_failed_revoke_keeps_the_link_out_of_its_error() {
        let base: Url = format!("http://{}", koidrive_test_support::reset_tcp_address())
            .parse()
            .unwrap();
        let auth = signed_in(base);
        let link_id = "AAAAAAAAAAAAAAAAAAAAAA";
        let outcome = auth.api().revoke_share_link(&auth, "s1", link_id).await;
        let Err(error @ ApiError::Transport(_)) = outcome else {
            panic!("{outcome:?}");
        };
        let mut shown = format!("{error} {error:?}");
        let mut source = std::error::Error::source(&error);
        while let Some(cause) = source {
            shown.push_str(&format!(" {cause} {cause:?}"));
            source = cause.source();
        }
        assert!(!shown.contains(link_id), "{shown}");
    }

    #[tokio::test]
    async fn list_spaces_stops_at_a_cursor_it_has_seen() {
        let (base, _server) =
            paging(vec![(vec!["a"], "a"), (vec!["b"], "b"), (vec!["c"], "a")]).await;
        let auth = signed_in(base);
        let outcome = auth.api().list_spaces(&auth).await;
        assert!(
            matches!(outcome, Err(ApiError::Decode("SpaceList"))),
            "{outcome:?}"
        );
    }
}
