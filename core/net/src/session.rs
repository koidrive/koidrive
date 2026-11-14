// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Collected, Empty, Limited};
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use koidrive_proto::v1::{
    AccessToken, ChunkRef, ClientMessage, Commit, CommitResult, DownloadTargets,
    GetDownloadTargets, GetVersionChunks, Hello, ListTrash, ListTree, ListVersions, PrepareUpload,
    PrepareUploadResult, Reauth, Resume, RpcRequest, RpcResponse, ServerMessage, TrashPage,
    TreePage, VersionChunks, VersionList, Welcome, client_message, rpc_request::Call, rpc_response,
    server_message::Msg,
};
use prost::Message as _;
use rustls::pki_types::ServerName;
use rustls_platform_verifier::BuilderVerifierExt;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpStream, lookup_host};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::error::Elapsed;
use tokio::time::{Instant, sleep, sleep_until, timeout, timeout_at};
use tokio_rustls::TlsConnector;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::{AUTHORIZATION, CONTENT_TYPE};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{self, Message as WsMessage};
use url::Url;

use crate::api::PROTOBUF;
use crate::auth::Auth;
use crate::backoff::Backoff;
use crate::error::{ApiError, CallError, NotResent, Resend, ServerError, StopReason};
use crate::event::{Committed, DisconnectReason, Event};
use crate::stream::{Fed, Stream};
use crate::transport::{Cause, Step, Transport};

/// The protocol minor versions under `/v1/` this client speaks (spec v0 section 9.1).
pub const PROTO_MIN: u32 = 1;
pub const PROTO_MAX: u32 = 1;
pub const MAX_SERVER_MESSAGE: usize = 1 << 20;
pub const MAX_CLIENT_MESSAGE: usize = 8 << 20;

const CLOSE_TOKEN_EXPIRED: u16 = 4001;
const CLOSE_UNSUPPORTED_VERSION: u16 = 4002;
const CLOSE_DEVICE_REVOKED: u16 = 4003;
const MAX_REAUTH_WAIT: Duration = Duration::from_secs(3600);
const MAX_REFUSAL: usize = 64 << 10;
const REFUSAL_READ_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_UNREADABLE_REFUSALS: u32 = 5;
const MIN_REAUTH_WAIT: Duration = Duration::from_secs(1);

#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub space_id: String,
    pub hello: Hello,
    pub backoff: Backoff,
    pub ping_interval: Duration,
    pub connect_timeout: Duration,
    pub welcome_timeout: Duration,
    pub write_timeout: Duration,
    /// How many times a call refused with `CODE_RETRY_LATER`, or with `CODE_QUOTA_EXCEEDED` and a wait, is sent again
    /// before the error is returned. Any other refusal is returned at once.
    pub retry_later_attempts: u32,
}

impl SessionConfig {
    pub fn new(
        space_id: impl Into<String>,
        client_name: impl Into<String>,
        client_version: impl Into<String>,
        max_format: u32,
    ) -> Self {
        SessionConfig {
            space_id: space_id.into(),
            hello: Hello {
                client_name: client_name.into(),
                client_version: client_version.into(),
                proto_min: PROTO_MIN,
                proto_max: PROTO_MAX,
                max_format,
                features: Vec::new(),
            },
            backoff: Backoff::default(),
            ping_interval: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(30),
            welcome_timeout: Duration::from_secs(30),
            write_timeout: Duration::from_secs(60),
            retry_later_attempts: 5,
        }
    }
}

/// The events of a session, in order. Ends after `Event::Stopped`.
#[derive(Debug)]
pub struct Events(mpsc::UnboundedReceiver<Event>);

impl Events {
    pub async fn next(&mut self) -> Option<Event> {
        self.0.recv().await
    }

    /// The next event if one is queued already, without waiting.
    pub fn try_next(&mut self) -> Option<Event> {
        self.0.try_recv().ok()
    }
}

/// A session with one space. It keeps reconnecting until it is closed or stopped. Dropping it closes it.
#[derive(Debug)]
pub struct Session {
    commands: mpsc::UnboundedSender<Command>,
    closing: watch::Sender<bool>,
    stopped: Arc<OnceLock<StopReason>>,
    commit_lock: Mutex<()>,
    backoff: Backoff,
    retry_later_attempts: u32,
    task: JoinHandle<()>,
}

impl Session {
    /// Starts the session task on the current Tokio runtime.
    pub fn start(auth: Arc<Auth>, config: SessionConfig) -> Result<(Session, Events), StopReason> {
        let url = websocket_url(auth.api().base(), &config.space_id)?;
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (events, event_rx) = mpsc::unbounded_channel();
        let stopped = Arc::new(OnceLock::new());
        let (closing, closing_rx) = watch::channel(false);
        let session = Session {
            commands,
            closing,
            stopped: stopped.clone(),
            commit_lock: Mutex::new(()),
            backoff: config.backoff,
            retry_later_attempts: config.retry_later_attempts,
            task: tokio::spawn(
                Task {
                    auth,
                    config,
                    url,
                    tls: None,
                    commands: command_rx,
                    closing: closing_rx,
                    events,
                    stopped,
                    next_req: 1,
                    pending: BTreeMap::new(),
                    over_http: BTreeMap::new(),
                    http: JoinSet::new(),
                    stream: Stream::default(),
                    attempt: 0,
                    token: None,
                    expired: None,
                    asked_wait: None,
                    unreadable_refusals: 0,
                }
                .run(),
            ),
        };
        Ok((session, Events(event_rx)))
    }

    /// Streams the log after `since.since_seq` of `since.history_id`: the last seq the driver applied completely, or
    /// the first `TreePage.head_seq` of a bootstrap (spec v0 section 7.5), with the commit token there (RFC 0013). The
    /// session resumes from the last seq it passed on, and its token, after every reconnect, until a
    /// `ResyncRequired` stops the stream.
    pub fn subscribe(&self, since: Resume) {
        let _ = self.commands.send(Command::Subscribe(since));
    }

    /// Sends a commit and waits for its result. One commit is in flight at a time (spec v0 section 8). If the
    /// connection drops, the identical commit is resent only when the next `Welcome` accounts for its slot
    /// (RFC 0002 section 2); otherwise the call fails with `CallError::NotResent`. The result also arrives as
    /// `Event::Committed` in seq order. The session emits the events of a connection, its `Event::Connected` first,
    /// before it answers a call on it, so every event emitted before the answer is in `Events` once this returns.
    pub async fn commit(&self, commit: Commit) -> Result<CommitResult, CallError> {
        let _one_at_a_time = self.commit_lock.lock().await;
        let slot = CommitSlot {
            commit_id: commit.commit_id.clone(),
            history_id: commit.history_id.clone(),
            device_seq: commit.device_seq,
        };
        match self.call(Call::Commit(commit), Some(slot)).await? {
            rpc_response::Result::Commit(result) => Ok(result),
            _ => Err(CallError::UnexpectedResult),
        }
    }

    /// At most 1,000 chunks per call (spec v0 section 13).
    pub async fn prepare_upload(
        &self,
        chunks: Vec<ChunkRef>,
    ) -> Result<PrepareUploadResult, CallError> {
        match self
            .call(Call::PrepareUpload(PrepareUpload { chunks }), None)
            .await?
        {
            rpc_response::Result::PrepareUpload(result) => Ok(result),
            _ => Err(CallError::UnexpectedResult),
        }
    }

    pub async fn get_download_targets(
        &self,
        hashes: Vec<Vec<u8>>,
    ) -> Result<DownloadTargets, CallError> {
        match self
            .call(
                Call::GetDownloadTargets(GetDownloadTargets { hashes }),
                None,
            )
            .await?
        {
            rpc_response::Result::DownloadTargets(result) => Ok(result),
            _ => Err(CallError::UnexpectedResult),
        }
    }

    pub async fn get_version_chunks(
        &self,
        version_id: impl Into<String>,
        page: u32,
    ) -> Result<VersionChunks, CallError> {
        let request = GetVersionChunks {
            version_id: version_id.into(),
            page,
        };
        match self.call(Call::GetVersionChunks(request), None).await? {
            rpc_response::Result::VersionChunks(result) => Ok(result),
            _ => Err(CallError::UnexpectedResult),
        }
    }

    pub async fn list_versions(&self, request: ListVersions) -> Result<VersionList, CallError> {
        match self.call(Call::ListVersions(request), None).await? {
            rpc_response::Result::Versions(result) => Ok(result),
            _ => Err(CallError::UnexpectedResult),
        }
    }

    pub async fn list_tree(
        &self,
        cursor: impl Into<String>,
        limit: u32,
    ) -> Result<TreePage, CallError> {
        match self
            .call(
                Call::ListTree(ListTree {
                    cursor: cursor.into(),
                    limit,
                }),
                None,
            )
            .await?
        {
            rpc_response::Result::Tree(result) => Ok(result),
            _ => Err(CallError::UnexpectedResult),
        }
    }

    pub async fn list_trash(
        &self,
        cursor: impl Into<String>,
        limit: u32,
    ) -> Result<TrashPage, CallError> {
        match self
            .call(
                Call::ListTrash(ListTrash {
                    cursor: cursor.into(),
                    limit,
                }),
                None,
            )
            .await?
        {
            rpc_response::Result::Trash(result) => Ok(result),
            _ => Err(CallError::UnexpectedResult),
        }
    }

    /// Closes the connection and waits for the session task to end. Pending calls fail with `StopReason::Closed`.
    pub async fn close(self) {
        let _ = self.commands.send(Command::Close);
        let _ = self.closing.send(true);
        let _ = self.task.await;
    }

    async fn call(
        &self,
        call: Call,
        commit: Option<CommitSlot>,
    ) -> Result<rpc_response::Result, CallError> {
        let bytes = ClientMessage {
            msg: Some(client_message::Msg::Rpc(RpcRequest {
                req_id: u64::MAX,
                call: Some(call.clone()),
            })),
        }
        .encoded_len();
        if bytes > MAX_CLIENT_MESSAGE {
            return Err(CallError::TooLarge { bytes });
        }
        let mut attempt = 0;
        loop {
            let (reply, answer) = oneshot::channel();
            let command = Command::Call {
                call: call.clone(),
                commit: commit.clone(),
                reply,
            };
            self.commands.send(command).map_err(|_| self.stopped())?;
            match answer.await.map_err(|_| self.stopped())? {
                Err(CallError::Server(error))
                    if error.resend() == Resend::Allowed && attempt < self.retry_later_attempts =>
                {
                    let wait = error
                        .retry_after()
                        .unwrap_or_else(|| self.backoff.delay(attempt));
                    sleep(wait).await;
                    attempt += 1;
                }
                outcome => return outcome,
            }
        }
    }

    fn stopped(&self) -> CallError {
        CallError::Stopped(self.stopped.get().cloned().unwrap_or(StopReason::Closed))
    }
}

fn websocket_url(base: &Url, space_id: &str) -> Result<Url, StopReason> {
    if space_id.is_empty() || !space_id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return Err(StopReason::BadUrl(format!(
            "{space_id:?} is not a space id"
        )));
    }
    let mut url = base
        .join(&format!("v1/spaces/{space_id}/ws"))
        .map_err(|e| StopReason::BadUrl(e.to_string()))?;
    let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    url.set_scheme(scheme)
        .map_err(|()| StopReason::BadUrl(base.to_string()))?;
    Ok(url)
}

#[derive(Clone, Debug)]
struct CommitSlot {
    commit_id: String,
    history_id: String,
    device_seq: u64,
}

/// RFC 0002 section 2.
fn may_resend(slot: &CommitSlot, welcome: &Welcome) -> Result<(), NotResent> {
    if welcome.history_id != slot.history_id {
        return Err(NotResent::HistoryChanged {
            commit: slot.history_id.clone(),
            welcome: welcome.history_id.clone(),
        });
    }
    if welcome.device_seq == slot.device_seq
        || Some(welcome.device_seq) == slot.device_seq.checked_sub(1)
    {
        return Ok(());
    }
    Err(NotResent::DeviceSeqMismatch {
        slot: slot.device_seq,
        welcome: welcome.device_seq,
    })
}

type Reply = oneshot::Sender<Result<rpc_response::Result, CallError>>;

enum Command {
    Call {
        call: Call,
        commit: Option<CommitSlot>,
        reply: Reply,
    },
    Subscribe(Resume),
    Close,
}

struct PendingCall {
    call: Call,
    commit: Option<CommitSlot>,
    reply: Reply,
    sent: bool,
}

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
type Socket = WebSocketStream<Shared>;

enum Exit {
    Reconnect { reason: DisconnectReason, now: bool },
    Stop(StopReason),
    Closed,
}

impl Exit {
    fn timeout() -> Exit {
        Exit::Reconnect {
            reason: DisconnectReason::Timeout,
            now: false,
        }
    }

    fn transport(transport: Transport) -> Exit {
        Exit::Reconnect {
            reason: DisconnectReason::Transport(transport),
            now: false,
        }
    }

    fn violation(what: &str) -> Exit {
        Exit::Reconnect {
            reason: DisconnectReason::ProtocolViolation(what.to_owned()),
            now: false,
        }
    }
}

struct Task {
    auth: Arc<Auth>,
    config: SessionConfig,
    url: Url,
    tls: Option<Arc<rustls::ClientConfig>>,
    commands: mpsc::UnboundedReceiver<Command>,
    closing: watch::Receiver<bool>,
    events: mpsc::UnboundedSender<Event>,
    stopped: Arc<OnceLock<StopReason>>,
    next_req: u64,
    pending: BTreeMap<u64, PendingCall>,
    over_http: BTreeMap<u64, PendingCall>,
    http: JoinSet<(u64, Result<RpcResponse, ApiError>)>,
    stream: Stream,
    attempt: u32,
    token: Option<String>,
    expired: Option<String>,
    asked_wait: Option<Duration>,
    unreadable_refusals: u32,
}

impl Task {
    async fn run(mut self) {
        let reason = loop {
            let mut closing = self.closing.clone();
            let exit = tokio::select! {
                biased;
                exit = self.connection() => exit,
                () = closed(&mut closing) => Exit::Closed,
            };
            let (reason, now) = match exit {
                Exit::Closed => break StopReason::Closed,
                Exit::Stop(reason) => break reason,
                Exit::Reconnect { reason, now } => (reason, now),
            };
            let retry_in = if now && self.attempt == 0 {
                Duration::ZERO
            } else {
                self.config.backoff.delay(self.attempt)
            }
            .max(self.asked_wait.take().unwrap_or_default());
            self.attempt = self.attempt.saturating_add(1);
            tracing::debug!(?reason, ?retry_in, "koidrive session disconnected");
            self.emit(Event::Disconnected { reason, retry_in });
            if !self.idle(retry_in).await {
                break StopReason::Closed;
            }
        };
        tracing::debug!(%reason, "koidrive session stopped");
        let _ = self.stopped.set(reason.clone());
        let pending = std::mem::take(&mut self.pending);
        for (_, call) in pending
            .into_iter()
            .chain(std::mem::take(&mut self.over_http))
        {
            let _ = call.reply.send(Err(CallError::Stopped(reason.clone())));
        }
        self.emit(Event::Stopped(reason));
    }

    async fn idle(&mut self, delay: Duration) -> bool {
        let deadline = sleep(delay);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                () = &mut deadline => return true,
                command = self.commands.recv() => match command {
                    None | Some(Command::Close) => return false,
                    Some(Command::Call { call, commit, reply }) => {
                        let req_id = self.req_id();
                        self.pending.insert(req_id, PendingCall { call, commit, reply, sent: false });
                    }
                    Some(Command::Subscribe(since)) => {
                        self.stream.subscribe(since);
                    }
                },
            }
        }
    }

    async fn connection(&mut self) -> Exit {
        let (mut socket, token, welcome) = match self.establish().await {
            Ok(established) => established,
            Err(exit) => return exit,
        };
        self.attempt = 0;
        self.unreadable_refusals = 0;
        self.emit(Event::Connected(welcome.clone()));
        if let Some(resume) = self.stream.reconnected()
            && let Err(exit) = self
                .send(&mut socket, client_message::Msg::Resume(resume))
                .await
        {
            return exit;
        }
        if let Err(exit) = self.resend(&welcome, &mut socket).await {
            return exit;
        }
        self.serve(socket, token).await
    }

    async fn establish(&mut self) -> Result<(Socket, AccessToken, Welcome), Exit> {
        let (mut socket, token) = timeout(self.config.connect_timeout, self.open())
            .await
            .map_err(|_| Exit::timeout())??;
        let welcome = self.handshake(&mut socket).await?;
        Ok((socket, token, welcome))
    }

    async fn open(&mut self) -> Result<(Socket, AccessToken), Exit> {
        if let Some(expired) = self.expired.clone() {
            self.auth.reject(&expired).await;
            self.expired = None;
        }
        let token = match self.auth.access_token().await {
            Ok(token) => token,
            Err(error) if error.is_unauthenticated() => {
                return Err(Exit::Stop(StopReason::CredentialsRefused));
            }
            Err(error) => {
                if let Some(status) = error.status() {
                    self.count_refusal(status, !matches!(error, ApiError::Unreadable { .. }))?;
                }
                if let Some(server) = error.server()
                    && let Some(exit) = self.refused_with(server)
                {
                    return Err(exit);
                }
                let mut transport = Transport::of(Step::Token, &error);
                if let Some(status) = error.status() {
                    transport.cause = Cause::Status(status);
                }
                return Err(Exit::transport(transport));
            }
        };
        self.token = Some(token.token.clone());
        let host = self
            .url
            .host_str()
            .unwrap_or_default()
            .trim_matches(['[', ']'])
            .to_owned();
        let port = self.url.port_or_known_default().unwrap_or(80);
        let unresolved = |error: &io::Error| {
            Exit::transport(Transport {
                cause: Cause::Dns,
                ..Transport::of(Step::Connect, error)
            })
        };
        let addresses: Vec<SocketAddr> = lookup_host((host.as_str(), port))
            .await
            .map_err(|error| unresolved(&error))?
            .collect();
        if addresses.is_empty() {
            let error = io::Error::new(io::ErrorKind::NotFound, "the name has no address");
            return Err(unresolved(&error));
        }
        let tcp = TcpStream::connect(&addresses[..])
            .await
            .map_err(|error| Exit::transport(Transport::of(Step::Connect, &error)))?;
        let _ = tcp.set_nodelay(true);
        let io: Box<dyn Io> = if self.url.scheme() == "wss" {
            let name = ServerName::try_from(host.clone())
                .map_err(|e| Exit::Stop(StopReason::BadUrl(e.to_string())))?;
            let tls = TlsConnector::from(self.tls_config()?);
            let tls = tls.connect(name, tcp).await;
            Box::new(tls.map_err(|error| Exit::transport(Transport::of(Step::Tls, &error)))?)
        } else {
            Box::new(tcp)
        };
        let socket = self.upgrade(io, &token).await?;
        Ok((socket, token))
    }

    async fn upgrade(&mut self, io: Box<dyn Io>, token: &AccessToken) -> Result<Socket, Exit> {
        let mut request = self
            .url
            .as_str()
            .into_client_request()
            .map_err(|e| Exit::Stop(StopReason::BadUrl(e.to_string())))?;
        let bearer = HeaderValue::from_str(&format!("Bearer {}", token.token))
            .map_err(|_| Exit::violation("an access token that is not a header value"))?;
        request.headers_mut().insert(AUTHORIZATION, bearer);
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_SERVER_MESSAGE))
            .max_frame_size(Some(MAX_SERVER_MESSAGE));
        let kept = Shared(Arc::new(std::sync::Mutex::new(io)));
        let response =
            match tokio_tungstenite::client_async_with_config(request, kept.clone(), Some(config))
                .await
            {
                Ok((socket, _)) => return Ok(socket),
                Err(tungstenite::Error::Http(response)) => response,
                Err(error) => return Err(Exit::transport(Transport::of(Step::Upgrade, &error))),
            };
        let status = response.status().as_u16();
        let protobuf = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with(PROTOBUF));
        if protobuf {
            let (head, tail) = response.into_parts();
            let error = refusal_body(&head, tail.unwrap_or_default(), kept)
                .await
                .and_then(|body| match ServerError::decode(&body) {
                    Some(server) => Some(Some(server)),
                    None if body.is_empty() => Some(None),
                    None => None,
                });
            self.count_refusal(status, error.is_some())?;
            if let Some(Some(server)) = error
                && let Some(exit) = self.refused_with(&server)
            {
                return Err(exit);
            }
        } else {
            self.count_refusal(status, true)?;
        }
        Err(self.refused(status, &token.token).await)
    }

    fn count_refusal(&mut self, status: u16, readable: bool) -> Result<(), Exit> {
        if readable {
            self.unreadable_refusals = 0;
            return Ok(());
        }
        self.unreadable_refusals += 1;
        if self.unreadable_refusals < MAX_UNREADABLE_REFUSALS {
            return Ok(());
        }
        Err(Exit::Stop(StopReason::Refused(format!(
            "HTTP {status}, {} times in a row, with an error that could not be read",
            self.unreadable_refusals
        ))))
    }

    fn refused_with(&mut self, server: &ServerError) -> Option<Exit> {
        match server.resend() {
            Resend::Never => Some(Exit::Stop(StopReason::Refused(server.to_string()))),
            Resend::Allowed => {
                self.asked_wait = server.retry_after();
                None
            }
            Resend::ByCode => None,
        }
    }

    async fn refused(&mut self, status: u16, token: &str) -> Exit {
        match status {
            401 => {
                self.auth.reject(token).await;
                Exit::Reconnect {
                    reason: DisconnectReason::Refused { status },
                    now: true,
                }
            }
            403 => Exit::Stop(StopReason::Forbidden),
            404 => Exit::Stop(StopReason::NotFound),
            _ => Exit::Reconnect {
                reason: DisconnectReason::Refused { status },
                now: false,
            },
        }
    }

    fn tls_config(&mut self) -> Result<Arc<rustls::ClientConfig>, Exit> {
        if let Some(config) = &self.tls {
            return Ok(config.clone());
        }
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let mut config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .and_then(|builder| builder.with_platform_verifier())
            .map_err(|e| Exit::Stop(StopReason::Tls(e.to_string())))?
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let config = Arc::new(config);
        self.tls = Some(config.clone());
        Ok(config)
    }

    async fn handshake(&mut self, socket: &mut Socket) -> Result<Welcome, Exit> {
        self.send(
            socket,
            client_message::Msg::Hello(self.config.hello.clone()),
        )
        .await?;
        let deadline = Instant::now() + self.config.welcome_timeout;
        loop {
            let item = timeout_at(deadline, socket.next())
                .await
                .map_err(|_| Exit::Reconnect {
                    reason: DisconnectReason::Timeout,
                    now: false,
                })?;
            match self.frame(item).await? {
                Some(Msg::Welcome(welcome)) => return self.negotiated(welcome),
                Some(Msg::Goodbye(goodbye)) => {
                    return Err(self.closed(Some(goodbye.code as u16), goodbye.reason));
                }
                Some(Msg::Notice(notice)) => self.emit(Event::Notice(notice)),
                Some(_) => return Err(Exit::violation("a message before Welcome")),
                None => {}
            }
        }
    }

    /// A commit that went out on a dropped connection goes over HTTP RPC: when a commit hits a Cloudflare Durable Objects
    /// storage limit, the socket gets no answer and closes, but HTTP RPC answers `QUOTA_EXCEEDED` with a wait.
    async fn resend(&mut self, welcome: &Welcome, socket: &mut Socket) -> Result<(), Exit> {
        for calls in [&mut self.pending, &mut self.over_http] {
            let refused: Vec<(u64, NotResent)> = (calls.iter())
                .filter_map(|(req_id, call)| {
                    Some((*req_id, may_resend(call.commit.as_ref()?, welcome).err()?))
                })
                .collect();
            for (req_id, reason) in refused {
                if let Some(call) = calls.remove(&req_id) {
                    tracing::debug!(%reason, "koidrive session did not resend a commit");
                    let _ = call.reply.send(Err(CallError::NotResent(reason)));
                }
            }
        }
        let unanswered: Vec<u64> = (self.pending.iter())
            .filter(|(_, call)| call.sent && call.commit.is_some())
            .map(|(req_id, _)| *req_id)
            .collect();
        for req_id in unanswered {
            let call = self.pending.remove(&req_id).expect("listed above");
            let (auth, space_id, request) = (
                self.auth.clone(),
                self.config.space_id.clone(),
                call.call.clone(),
            );
            self.http
                .spawn(async move { (req_id, auth.api().rpc(&auth, &space_id, request).await) });
            self.over_http.insert(req_id, call);
        }
        let calls: Vec<RpcRequest> = (self.pending.iter_mut())
            .map(|(req_id, call)| {
                call.sent = true;
                RpcRequest {
                    req_id: *req_id,
                    call: Some(call.call.clone()),
                }
            })
            .collect();
        for request in calls {
            self.send(socket, client_message::Msg::Rpc(request)).await?;
        }
        Ok(())
    }

    async fn answered_over_http(
        &mut self,
        req_id: u64,
        answer: Result<RpcResponse, ApiError>,
        socket: &mut Socket,
    ) -> Result<(), Exit> {
        let Some(mut call) = self.over_http.remove(&req_id) else {
            return Ok(());
        };
        let result = match answer {
            Ok(response) => response.result,
            Err(error) => match error.server() {
                Some(refusal) => Some(rpc_response::Result::Error(refusal.0.clone())),
                None => {
                    tracing::debug!(%error, "koidrive session could not resend a commit over HTTP; sending it on the socket");
                    let request = RpcRequest {
                        req_id,
                        call: Some(call.call.clone()),
                    };
                    call.sent = true;
                    self.pending.insert(req_id, call);
                    return self.send(socket, client_message::Msg::Rpc(request)).await;
                }
            },
        };
        self.pending.insert(req_id, call);
        self.answer(RpcResponse { req_id, result }, socket).await
    }

    async fn serve(&mut self, mut socket: Socket, mut token: AccessToken) -> Exit {
        let interval = self.config.ping_interval;
        let mut ping = tokio::time::interval_at(Instant::now() + interval, interval);
        let mut last_heard = Instant::now();
        let reauth = sleep_until(self.reauth_at(&token));
        tokio::pin!(reauth);
        let mut refreshing: Option<Refresh> = None;
        loop {
            tokio::select! {
                item = socket.next() => {
                    last_heard = Instant::now();
                    let msg = match self.frame(item).await {
                        Ok(Some(msg)) => msg,
                        Ok(None) => continue,
                        Err(exit) => return exit,
                    };
                    if let Err(exit) = self.dispatch(msg, &mut socket).await {
                        return exit;
                    }
                }
                command = self.commands.recv() => {
                    let sent = match command {
                        None | Some(Command::Close) => {
                            let _ = socket.close(None).await;
                            return Exit::Closed;
                        }
                        Some(Command::Subscribe(since)) => {
                            let resume = self.stream.subscribe(since);
                            self.send(&mut socket, client_message::Msg::Resume(resume)).await
                        }
                        Some(Command::Call { call, commit, reply }) => {
                            let req_id = self.req_id();
                            let request = RpcRequest { req_id, call: Some(call.clone()) };
                            self.pending.insert(req_id, PendingCall { call, commit, reply, sent: true });
                            self.send(&mut socket, client_message::Msg::Rpc(request)).await
                        }
                    };
                    if let Err(exit) = sent {
                        return exit;
                    }
                }
                Some(Ok((req_id, answer))) = self.http.join_next(), if !self.http.is_empty() => {
                    if let Err(exit) = self.answered_over_http(req_id, answer, &mut socket).await {
                        return exit;
                    }
                }
                _ = ping.tick() => {
                    if last_heard.elapsed() > 2 * interval {
                        return Exit::Reconnect { reason: DisconnectReason::Timeout, now: false };
                    }
                    if let Err(exit) = self.write(&mut socket, WsMessage::Ping(Default::default())).await {
                        return exit;
                    }
                }
                () = &mut reauth, if refreshing.is_none() => {
                    let auth = self.auth.clone();
                    let limit = self.config.connect_timeout;
                    refreshing = Some(Box::pin(async move { timeout(limit, auth.access_token()).await }));
                }
                refreshed = refreshed(&mut refreshing) => {
                    refreshing = None;
                    let Ok(refreshed) = refreshed else {
                        tracing::debug!("koidrive session timed out refreshing its access token");
                        reauth.as_mut().reset(Instant::now() + self.config.backoff.delay(0));
                        continue;
                    };
                    if refreshed.is_ok() {
                        self.unreadable_refusals = 0;
                    }
                    match refreshed {
                        Ok(fresh) if fresh.token != token.token => {
                            let message = client_message::Msg::Reauth(Reauth { access_token: fresh.token.clone() });
                            if let Err(exit) = self.send(&mut socket, message).await {
                                return exit;
                            }
                            self.token = Some(fresh.token.clone());
                            token = fresh;
                            reauth.as_mut().reset(self.reauth_at(&token));
                        }
                        Ok(_) => reauth.as_mut().reset(Instant::now() + MIN_REAUTH_WAIT),
                        Err(error) if error.is_unauthenticated() => return Exit::Stop(StopReason::CredentialsRefused),
                        Err(error) => {
                            if let Some(status) = error.status()
                                && let Err(exit) = self.count_refusal(status, !matches!(error, ApiError::Unreadable { .. }))
                            {
                                return exit;
                            }
                            let server = error.server();
                            if let Some(server) = server && server.resend() == Resend::Never {
                                return Exit::Stop(StopReason::Refused(server.to_string()));
                            }
                            tracing::debug!(%error, "koidrive session could not refresh its access token");
                            let wait = server.and_then(ServerError::retry_after);
                            reauth.as_mut().reset(Instant::now() + wait.unwrap_or(self.config.backoff.delay(0)));
                        }
                    }
                }
            }
        }
    }

    fn negotiated(&self, welcome: Welcome) -> Result<Welcome, Exit> {
        let hello = &self.config.hello;
        if (hello.proto_min..=hello.proto_max).contains(&welcome.proto) {
            return Ok(welcome);
        }
        Err(Exit::Stop(StopReason::UnsupportedVersion(format!(
            "the server chose protocol 1.{}, but this client offered 1.{} to 1.{}",
            welcome.proto, hello.proto_min, hello.proto_max
        ))))
    }

    fn reauth_at(&self, token: &AccessToken) -> Instant {
        let wait = self
            .auth
            .refresh_at(token)
            .duration_since(SystemTime::now())
            .unwrap_or_default();
        Instant::now() + wait.clamp(MIN_REAUTH_WAIT, MAX_REAUTH_WAIT)
    }

    async fn frame(
        &mut self,
        item: Option<Result<WsMessage, tungstenite::Error>>,
    ) -> Result<Option<Msg>, Exit> {
        match item {
            None => Err(Exit::Reconnect {
                reason: DisconnectReason::Closed {
                    code: None,
                    reason: String::new(),
                },
                now: false,
            }),
            Some(Err(tungstenite::Error::Capacity(_))) => {
                Err(Exit::violation("a message larger than 1 MiB"))
            }
            Some(Err(error)) => Err(Exit::transport(Transport::of(Step::Stream, &error))),
            Some(Ok(WsMessage::Binary(bytes))) => match ServerMessage::decode(bytes) {
                Ok(message) => Ok(message.msg),
                Err(_) => Err(Exit::violation("a malformed ServerMessage")),
            },
            Some(Ok(WsMessage::Close(frame))) => {
                let (code, reason) = frame
                    .map(|f| (Some(u16::from(f.code)), f.reason.to_string()))
                    .unwrap_or_default();
                Err(self.closed(code, reason))
            }
            Some(Ok(_)) => Ok(None),
        }
    }

    fn closed(&mut self, code: Option<u16>, reason: String) -> Exit {
        match code {
            Some(CLOSE_UNSUPPORTED_VERSION) => Exit::Stop(StopReason::UnsupportedVersion(reason)),
            Some(CLOSE_DEVICE_REVOKED) => Exit::Stop(StopReason::DeviceRevoked),
            Some(CLOSE_TOKEN_EXPIRED) => {
                self.expired = self.token.clone();
                Exit::Reconnect {
                    reason: DisconnectReason::Closed { code, reason },
                    now: true,
                }
            }
            _ => Exit::Reconnect {
                reason: DisconnectReason::Closed { code, reason },
                now: false,
            },
        }
    }

    async fn dispatch(&mut self, msg: Msg, socket: &mut Socket) -> Result<(), Exit> {
        match msg {
            Msg::Changes(changes) => {
                let fed = self.stream.changes(changes.commits);
                self.feed(fed, socket).await
            }
            Msg::CaughtUp(caught_up) => {
                let fed = self.stream.caught_up(caught_up);
                self.feed(fed, socket).await
            }
            Msg::ResyncRequired(resync) => {
                let fed = self.stream.resync(resync);
                self.feed(fed, socket).await
            }
            Msg::Rpc(response) => self.answer(response, socket).await,
            Msg::Notice(notice) => {
                self.emit(Event::Notice(notice));
                Ok(())
            }
            Msg::Goodbye(goodbye) => Err(self.closed(Some(goodbye.code as u16), goodbye.reason)),
            Msg::Welcome(_) => Err(Exit::violation("a second Welcome")),
        }
    }

    async fn answer(&mut self, response: RpcResponse, socket: &mut Socket) -> Result<(), Exit> {
        let Some(call) = self.pending.remove(&response.req_id) else {
            tracing::debug!(
                req_id = response.req_id,
                "koidrive session got a response to no pending call"
            );
            return Ok(());
        };
        let outcome = match response.result {
            None => Err(CallError::UnexpectedResult),
            Some(rpc_response::Result::Error(error)) => Err(CallError::Server(ServerError(error))),
            Some(rpc_response::Result::Commit(result)) if let Some(slot) = &call.commit => {
                let fed = self.stream.committed(Committed {
                    commit_id: slot.commit_id.clone(),
                    result: result.clone(),
                });
                self.feed(fed, socket).await?;
                Ok(rpc_response::Result::Commit(result))
            }
            Some(result) => Ok(result),
        };
        let _ = call.reply.send(outcome);
        Ok(())
    }

    async fn feed(&mut self, fed: Fed, socket: &mut Socket) -> Result<(), Exit> {
        for event in fed.events {
            self.emit(event);
        }
        match fed.resume {
            Some(resume) => self.send(socket, client_message::Msg::Resume(resume)).await,
            None => Ok(()),
        }
    }

    async fn send(&self, socket: &mut Socket, msg: client_message::Msg) -> Result<(), Exit> {
        let bytes = ClientMessage { msg: Some(msg) }.encode_to_vec();
        self.write(socket, WsMessage::Binary(bytes.into())).await
    }

    async fn write(&self, socket: &mut Socket, message: WsMessage) -> Result<(), Exit> {
        match timeout(self.config.write_timeout, socket.send(message)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(Exit::transport(Transport::of(Step::Stream, &error))),
            Err(_) => Err(Exit::timeout()),
        }
    }

    fn req_id(&mut self) -> u64 {
        let req_id = self.next_req;
        self.next_req += 1;
        req_id
    }

    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }
}

type Refresh = Pin<Box<dyn Future<Output = Result<Result<AccessToken, ApiError>, Elapsed>> + Send>>;

async fn refreshed(
    refreshing: &mut Option<Refresh>,
) -> Result<Result<AccessToken, ApiError>, Elapsed> {
    match refreshing {
        Some(refresh) => refresh.await,
        None => std::future::pending().await,
    }
}

/// The connection under a WebSocket. It is shared so that the body of a refused upgrade can still be read after
/// tungstenite's handshake drops its handle.
#[derive(Clone)]
struct Shared(Arc<std::sync::Mutex<Box<dyn Io>>>);

impl Shared {
    fn io(&self) -> std::sync::MutexGuard<'_, Box<dyn Io>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl AsyncRead for Shared {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut **self.io()).poll_read(cx, buf)
    }
}

impl AsyncWrite for Shared {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut **self.io()).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut **self.io()).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut **self.io()).poll_shutdown(cx)
    }
}

async fn refusal_body(
    head: &tungstenite::http::response::Parts,
    tail: Vec<u8>,
    rest: impl AsyncRead + Unpin,
) -> Option<Bytes> {
    let mut answer = format!("{:?} {} \r\n", head.version, head.status.as_u16()).into_bytes();
    for (name, value) in &head.headers {
        answer.extend_from_slice(name.as_str().as_bytes());
        answer.extend_from_slice(b": ");
        answer.extend_from_slice(value.as_bytes());
        answer.extend_from_slice(b"\r\n");
    }
    answer.extend_from_slice(b"\r\n");
    answer.extend_from_slice(&tail);
    let replay = Replay {
        answer: Bytes::from(answer),
        rest,
        asked: false,
        waiting: None,
    };
    let read = async {
        let (mut sender, connection) = http1::handshake(TokioIo::new(replay)).await.ok()?;
        let response = sender.send_request(hyper::Request::new(Empty::<Bytes>::new()));
        drop(sender);
        let body = async {
            let body = Limited::new(response.await.ok()?.into_body(), MAX_REFUSAL);
            body.collect().await.ok().map(Collected::to_bytes)
        };
        let mut body = std::pin::pin!(body);
        tokio::select! {
            body = &mut body => body,
            _ = connection => body.await,
        }
    };
    timeout(REFUSAL_READ_TIMEOUT, read).await.ok().flatten()
}

struct Replay<R> {
    answer: Bytes,
    rest: R,
    /// Hyper reads an answer only once it has asked.
    asked: bool,
    waiting: Option<Waker>,
}

impl<R: AsyncRead + Unpin> AsyncRead for Replay<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if !this.asked {
            this.waiting = Some(cx.waker().clone());
            return Poll::Pending;
        }
        if this.answer.is_empty() {
            return Pin::new(&mut this.rest).poll_read(cx, buf);
        }
        let n = this.answer.len().min(buf.remaining());
        buf.put_slice(&this.answer.split_to(n));
        Poll::Ready(Ok(()))
    }
}

impl<R: Unpin> AsyncWrite for Replay<R> {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        this.asked = true;
        if let Some(waker) = this.waiting.take() {
            waker.wake();
        }
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

async fn closed(closing: &mut watch::Receiver<bool>) {
    let _ = closing.wait_for(|closed| *closed).await;
}

#[cfg(test)]
pub(crate) mod tests;
