// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::io;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use hyper::body::{Body, Frame, SizeHint};
use koidrive_proto::v1::{ChunkRef, Target, UploadReceipt, chunk_plan::Action};
use prost::Message;
use reqwest::header::CONTENT_TYPE;
use reqwest::{Method, StatusCode};
use sha2::{Digest, Sha256};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

use crate::api::Api;
use crate::backoff::Backoff;
use crate::error::{CallError, Resend, ServerError};
use crate::session::Session;
use crate::transport::Cause;

pub const MAX_CHUNKS_PER_CALL: usize = 1000;
/// How many chunks a [`Transfer`] uploads at once unless it is given another [`UploadGate`].
pub const DEFAULT_UPLOAD_CONCURRENCY: usize = 4;
/// How many chunks a [`Transfer`] downloads at once unless it is given another number.
pub const DEFAULT_DOWNLOAD_CONCURRENCY: NonZeroUsize = NonZeroUsize::new(8).unwrap();
const TARGET_MARGIN: Duration = Duration::from_secs(60);
const MAX_PREALLOCATION: usize = 4 << 20;
const SMALL_ANSWER: usize = 64 << 10;
const SLICE: usize = 64 << 10;

/// True when `bytes` are the chunk: their SHA-256 is its hash and their length its size.
pub fn matches(chunk: &ChunkRef, bytes: &[u8]) -> bool {
    bytes.len() as u64 == u64::from(chunk.size)
        && Sha256::digest(bytes).as_slice() == chunk.hash.as_slice()
}

fn hex(hash: &[u8]) -> String {
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[derive(Debug, thiserror::Error)]
pub enum ChunkError {
    #[error(transparent)]
    Call(#[from] CallError),
    /// The bytes read for an upload are not the chunk, so the file changed after it was chunked. Nothing was sent;
    /// the caller plans the commit again (RFC 0002 section 11).
    #[error("the bytes read for chunk {} do not match its hash and size", hex(.hash))]
    SourceChanged { hash: Vec<u8> },
    /// The bytes downloaded for a chunk did not match its hash and size on every attempt. None reached the caller.
    #[error("the bytes received for chunk {} do not match its hash and size", hex(.hash))]
    Corrupt { hash: Vec<u8> },
    #[error("the transfer of chunk {} was refused with HTTP {status}", hex(.hash))]
    Refused {
        hash: Vec<u8>,
        status: u16,
        error: Option<ServerError>,
    },
    /// `cause` is what kind of failure it was, when the request reached no answer.
    #[error("the transfer of chunk {} kept failing: {message}", hex(.hash))]
    Unavailable {
        hash: Vec<u8>,
        message: String,
        cause: Option<Cause>,
    },
    #[error("the server gave no usable plan or target for chunk {}", hex(.hash))]
    NoTarget { hash: Vec<u8> },
    #[error("reading or writing chunk {}", hex(.hash))]
    Io {
        hash: Vec<u8>,
        #[source]
        source: io::Error,
    },
}

enum Attempt<T> {
    Done(T),
    Expired,
    Retry(ChunkError),
    Fail(ChunkError),
}

/// Limits how many chunk uploads are in flight at once. Clones share the limit, so every [`Transfer`] given a clone of
/// one gate counts against the same number. A chunk takes its place before it is read and gives it back when its
/// upload ends, so the limit also bounds the chunk bytes held in memory.
#[derive(Clone, Debug)]
pub struct UploadGate {
    permits: Arc<Semaphore>,
    concurrency: usize,
}

impl UploadGate {
    /// A gate that lets `concurrency` chunks be in flight at once.
    ///
    /// # Panics
    ///
    /// If `concurrency` is 0, since no upload could ever start.
    pub fn new(concurrency: usize) -> Self {
        assert!(concurrency > 0, "an upload gate needs at least one place");
        UploadGate {
            permits: Arc::new(Semaphore::new(concurrency)),
            concurrency,
        }
    }

    pub fn concurrency(&self) -> usize {
        self.concurrency
    }

    async fn enter(&self) -> OwnedSemaphorePermit {
        self.permits
            .clone()
            .acquire_owned()
            .await
            .expect("the semaphore is never closed")
    }
}

/// Moves chunk bytes between the client and the targets the server hands out.
#[derive(Clone, Debug)]
pub struct Transfer {
    http: reqwest::Client,
    uploads: reqwest::Client,
    pub backoff: Backoff,
    /// Bounds the chunks this transfer uploads at once, together with every other transfer that shares the gate.
    pub gate: UploadGate,
    /// How many chunks one download fetches at once.
    pub download_concurrency: NonZeroUsize,
    /// Attempts per chunk, and rounds of asking the server again for chunks it deferred or whose target expired. Only a
    /// round in which no chunk was uploaded or found present counts.
    pub attempts: u32,
    pub attempt_timeout: Duration,
    pub stall_timeout: Duration,
}

impl Transfer {
    pub fn new(api: &Api) -> Self {
        Transfer {
            http: api.http().clone(),
            uploads: api.uploads().clone(),
            backoff: Backoff::default(),
            gate: UploadGate::new(DEFAULT_UPLOAD_CONCURRENCY),
            download_concurrency: DEFAULT_DOWNLOAD_CONCURRENCY,
            attempts: 5,
            attempt_timeout: Duration::from_secs(120),
            stall_timeout: Duration::from_secs(60),
        }
    }

    /// Uploads the chunks the server does not hold yet and returns their receipts for the commit, in the order the
    /// uploads finished. `read` gives a chunk's bytes; it is called only for chunks the server asks for, each distinct
    /// hash once. Chunks start in order, as many at once as the gate lets in, and a chunk is read only once it has its
    /// place. The first chunk that fails for good ends the upload, and the chunks still in flight are dropped.
    /// Presigned targets give no receipt; the server checks those chunks itself at commit time.
    ///
    /// The future `read` returns borrows neither the chunk nor `read` itself. That keeps this transfer `Send`, so a
    /// spawned task can run it.
    pub async fn upload<R: Future<Output = io::Result<Bytes>>>(
        &self,
        session: &Session,
        chunks: &[ChunkRef],
        read: impl FnMut(&ChunkRef) -> R,
    ) -> Result<Vec<UploadReceipt>, ChunkError> {
        self.upload_observed(session, chunks, read, |_| {}, |_| {})
            .await
    }

    /// [`Transfer::upload`], calling `stored` with each chunk once the server or its storage took it and `present`
    /// with each chunk the server already held, also when a later chunk ends the upload.
    pub async fn upload_observed<R: Future<Output = io::Result<Bytes>>>(
        &self,
        session: &Session,
        chunks: &[ChunkRef],
        mut read: impl FnMut(&ChunkRef) -> R,
        mut stored: impl FnMut(&ChunkRef),
        mut present: impl FnMut(&ChunkRef),
    ) -> Result<Vec<UploadReceipt>, ChunkError> {
        let mut todo = distinct(chunks);
        let mut receipts = Vec::new();
        let mut round = 0;
        while !todo.is_empty() {
            let mut deferred = Vec::new();
            let mut wait = Duration::ZERO;
            for (first, batch) in (0..)
                .step_by(MAX_CHUNKS_PER_CALL)
                .zip(todo.chunks(MAX_CHUNKS_PER_CALL))
            {
                let result = session.prepare_upload(batch.to_vec()).await?;
                let prepared = SystemTime::now();
                let plans: HashMap<_, _> = result
                    .chunks
                    .iter()
                    .map(|plan| (plan.hash.as_slice(), plan))
                    .collect();
                let mut uploads = Vec::new();
                for (index, chunk) in (first..).zip(batch) {
                    let no_target = || ChunkError::NoTarget {
                        hash: chunk.hash.clone(),
                    };
                    let plan = plans.get(chunk.hash.as_slice()).ok_or_else(no_target)?;
                    match plan.action() {
                        Action::Present => present(chunk),
                        Action::RetryLater => {
                            wait = wait.max(Duration::from_millis(plan.retry_after_ms));
                            deferred.push(index);
                        }
                        Action::Upload => uploads.push((
                            index,
                            chunk,
                            plan.target.as_ref().ok_or_else(no_target)?,
                        )),
                        Action::Unspecified => return Err(no_target()),
                    }
                }
                let mut started = 0;
                let mut running = FuturesUnordered::new();
                loop {
                    tokio::select! {
                        biased;
                        Some(sent) = running.next() => match sent? {
                            (index, Some(receipt)) => {
                                stored(&todo[index]);
                                receipts.extend(receipt);
                            }
                            (index, None) => deferred.push(index),
                        },
                        permit = self.gate.enter(), if started < uploads.len() => {
                            let (index, chunk, target) = uploads[started];
                            started += 1;
                            if expiring(target) && !expiring_at(target, prepared) {
                                deferred.push(index);
                            } else {
                                let bytes = read(chunk);
                                running.push(async move {
                                    let _permit = permit;
                                    let bytes = bytes.await.map_err(|source| ChunkError::Io {
                                        hash: chunk.hash.clone(),
                                        source,
                                    })?;
                                    if !matches(chunk, &bytes) {
                                        return Err(ChunkError::SourceChanged {
                                            hash: chunk.hash.clone(),
                                        });
                                    }
                                    let sent = self
                                        .retrying(|| self.put(target, chunk, bytes.clone()))
                                        .await?;
                                    Ok((index, sent))
                                });
                            }
                        }
                        else => break,
                    }
                }
            }
            if deferred.is_empty() {
                break;
            }
            let progressed = deferred.len() < todo.len();
            deferred.sort_unstable();
            if !progressed && round >= self.attempts {
                let message =
                    "the server kept deferring the upload or refusing its ticket".to_owned();
                return Err(ChunkError::Unavailable {
                    hash: todo[deferred[0]].hash.clone(),
                    message,
                    cause: None,
                });
            }
            tokio::time::sleep(if wait.is_zero() {
                self.backoff.delay(round)
            } else {
                wait
            })
            .await;
            if !progressed {
                round += 1;
            }
            todo = deferred
                .into_iter()
                .map(|index| todo[index].clone())
                .collect();
        }
        Ok(receipts)
    }

    /// Downloads each distinct chunk once, checks it against its hash and size, and hands it to `write` once, in the
    /// order the chunks arrive, so the caller writes each at its offsets. Up to `download_concurrency` chunks are
    /// fetched at once, with targets asked for at most [`MAX_CHUNKS_PER_CALL`] at a time. Bytes that do not match are
    /// fetched again and never passed on. The first chunk that fails for good ends the download, and the chunks still
    /// in flight are dropped.
    ///
    /// As with [`Transfer::upload`], the future `write` returns borrows neither the chunk nor `write` itself.
    pub async fn download<W: Future<Output = io::Result<()>>>(
        &self,
        session: &Session,
        chunks: &[ChunkRef],
        mut write: impl FnMut(&ChunkRef, Bytes) -> W,
    ) -> Result<(), ChunkError> {
        for batch in distinct(chunks).chunks(MAX_CHUNKS_PER_CALL) {
            let hashes = batch.iter().map(|chunk| chunk.hash.clone()).collect();
            let mut targets: HashMap<Vec<u8>, Target> = session
                .get_download_targets(hashes)
                .await?
                .targets
                .into_iter()
                .filter_map(|t| Some((t.hash, t.target?)))
                .collect();
            let mut waiting = batch.iter();
            let mut running = FuturesUnordered::new();
            loop {
                while running.len() < self.download_concurrency.get()
                    && let Some(chunk) = waiting.next()
                {
                    running.push(self.fetch(session, chunk, targets.remove(&chunk.hash)));
                }
                let Some(fetched) = running.next().await else {
                    break;
                };
                let (chunk, bytes) = fetched?;
                write(chunk, bytes).await.map_err(|source| ChunkError::Io {
                    hash: chunk.hash.clone(),
                    source,
                })?;
            }
        }
        Ok(())
    }

    async fn fetch<'c>(
        &self,
        session: &Session,
        chunk: &'c ChunkRef,
        mut target: Option<Target>,
    ) -> Result<(&'c ChunkRef, Bytes), ChunkError> {
        let mut renewed = 0;
        loop {
            let current = match target.take() {
                Some(target) if !expiring(&target) => target,
                _ => self.download_target(session, chunk).await?,
            };
            match self.retrying(|| self.get(&current, chunk)).await? {
                Some(bytes) => return Ok((chunk, bytes)),
                None if renewed < self.attempts => renewed += 1,
                None => {
                    let message = "the server kept refusing the download ticket".to_owned();
                    return Err(ChunkError::Unavailable {
                        hash: chunk.hash.clone(),
                        message,
                        cause: None,
                    });
                }
            }
        }
    }

    async fn download_target(
        &self,
        session: &Session,
        chunk: &ChunkRef,
    ) -> Result<Target, ChunkError> {
        let targets = session
            .get_download_targets(vec![chunk.hash.clone()])
            .await?;
        let found = targets
            .targets
            .into_iter()
            .find(|t| t.hash == chunk.hash)
            .and_then(|t| t.target);
        found.ok_or_else(|| ChunkError::NoTarget {
            hash: chunk.hash.clone(),
        })
    }

    async fn retrying<T, F: Future<Output = Attempt<T>>>(
        &self,
        mut attempt: impl FnMut() -> F,
    ) -> Result<Option<T>, ChunkError> {
        let mut tries = 0;
        loop {
            match attempt().await {
                Attempt::Done(value) => return Ok(Some(value)),
                Attempt::Expired => return Ok(None),
                Attempt::Fail(error) => return Err(error),
                Attempt::Retry(error) => {
                    tries += 1;
                    if tries >= self.attempts {
                        return Err(error);
                    }
                    tracing::debug!(%error, "koidrive chunk transfer will be retried");
                    let asked = match &error {
                        ChunkError::Refused {
                            error: Some(server),
                            ..
                        } => server.retry_after(),
                        _ => None,
                    };
                    tokio::time::sleep(asked.unwrap_or_else(|| self.backoff.delay(tries - 1)))
                        .await;
                }
            }
        }
    }

    /// One upload. The connection takes slices into its send buffers, so the answer wait also covers sending what
    /// those buffers still hold.
    async fn put(
        &self,
        target: &Target,
        chunk: &ChunkRef,
        bytes: Bytes,
    ) -> Attempt<Option<UploadReceipt>> {
        let progress = Arc::new(Mutex::new(Progress::Body(Instant::now())));
        let body = reqwest::Body::wrap(Sending {
            rest: bytes,
            progress: progress.clone(),
        });
        let attempt = async {
            let request = self.request(&self.uploads, target, "PUT").body(body);
            let response = match request.send().await {
                Ok(response) => response,
                Err(error) => return Attempt::Retry(unavailable(chunk, error)),
            };
            let status = response.status();
            if status.is_success() && !protobuf(&response) {
                return Attempt::Done(None);
            }
            let body = if protobuf(&response) {
                match small_answer(response).await {
                    Ok(body) => body,
                    Err(error) => return Attempt::Retry(unavailable(chunk, error)),
                }
            } else {
                None
            };
            if status.is_success() {
                return match body.and_then(|body| UploadReceipt::decode(body.as_slice()).ok()) {
                    Some(receipt) if receipt.hash == chunk.hash && receipt.size == chunk.size => {
                        Attempt::Done(Some(receipt))
                    }
                    _ => Attempt::Fail(ChunkError::Refused {
                        hash: chunk.hash.clone(),
                        status: status.as_u16(),
                        error: None,
                    }),
                };
            }
            let refused = ChunkError::Refused {
                hash: chunk.hash.clone(),
                status: status.as_u16(),
                error: body.and_then(|body| ServerError::decode(&body)),
            };
            let refused = match by_code(refused) {
                Ok(attempt) => return attempt,
                Err(refused) => refused,
            };
            match status {
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Attempt::Expired,
                StatusCode::BAD_REQUEST => Attempt::Retry(refused),
                status if retryable(status) => Attempt::Retry(refused),
                _ => Attempt::Fail(refused),
            }
        };
        tokio::select! {
            attempt = attempt => attempt,
            message = self.stalled(&progress) => Attempt::Retry(ChunkError::Unavailable {
                hash: chunk.hash.clone(),
                message,
                cause: Some(Cause::TimedOut),
            }),
        }
    }

    async fn stalled(&self, progress: &Mutex<Progress>) -> String {
        loop {
            let now = *progress.lock().unwrap();
            let (deadline, wake) = match now {
                Progress::Body(at) => {
                    let deadline = at + self.stall_timeout;
                    (
                        deadline,
                        deadline.min(Instant::now() + self.attempt_timeout),
                    )
                }
                Progress::Answer(at) => (at + self.attempt_timeout, at + self.attempt_timeout),
            };
            if Instant::now() < deadline {
                tokio::time::sleep_until(wake).await;
                continue;
            }
            return match now {
                Progress::Body(_) => format!("the body did not move for {:?}", self.stall_timeout),
                Progress::Answer(_) => {
                    format!("no complete answer within {:?}", self.attempt_timeout)
                }
            };
        }
    }

    async fn get(&self, target: &Target, chunk: &ChunkRef) -> Attempt<Bytes> {
        let attempt = async {
            let mut response = match self.request(&self.http, target, "GET").send().await {
                Ok(response) => response,
                Err(error) => return Attempt::Retry(unavailable(chunk, error)),
            };
            let status = response.status();
            if !status.is_success() {
                let error = if protobuf(&response) {
                    small_answer(response)
                        .await
                        .ok()
                        .flatten()
                        .and_then(|body| ServerError::decode(&body))
                } else {
                    None
                };
                let refused = ChunkError::Refused {
                    hash: chunk.hash.clone(),
                    status: status.as_u16(),
                    error,
                };
                let refused = match by_code(refused) {
                    Ok(attempt) => return attempt,
                    Err(refused) => refused,
                };
                return match status {
                    StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Attempt::Expired,
                    status if retryable(status) => Attempt::Retry(refused),
                    _ => Attempt::Fail(refused),
                };
            }
            let corrupt = || {
                Attempt::Retry(ChunkError::Corrupt {
                    hash: chunk.hash.clone(),
                })
            };
            let limit = chunk.size as usize;
            let mut body = Vec::with_capacity(limit.min(MAX_PREALLOCATION));
            loop {
                match response.chunk().await {
                    Ok(Some(part)) if body.len() + part.len() <= limit => {
                        body.extend_from_slice(&part)
                    }
                    Ok(Some(_)) => return corrupt(),
                    Ok(None) => break,
                    Err(error) => return Attempt::Retry(unavailable(chunk, error)),
                }
            }
            if !matches(chunk, &body) {
                return corrupt();
            }
            Attempt::Done(Bytes::from(body))
        };
        self.bounded(chunk, attempt).await
    }

    async fn bounded<T>(
        &self,
        chunk: &ChunkRef,
        attempt: impl Future<Output = Attempt<T>>,
    ) -> Attempt<T> {
        match tokio::time::timeout(self.attempt_timeout, attempt).await {
            Ok(attempt) => attempt,
            Err(_) => Attempt::Retry(ChunkError::Unavailable {
                hash: chunk.hash.clone(),
                message: format!("no complete answer within {:?}", self.attempt_timeout),
                cause: Some(Cause::TimedOut),
            }),
        }
    }

    fn request(
        &self,
        client: &reqwest::Client,
        target: &Target,
        default_method: &str,
    ) -> reqwest::RequestBuilder {
        let method = if target.method.is_empty() {
            default_method
        } else {
            &target.method
        };
        let method = Method::from_bytes(method.as_bytes()).unwrap_or(Method::GET);
        let mut request = client.request(method, &target.url);
        for (name, value) in &target.headers {
            request = request.header(name, value);
        }
        request
    }
}

#[derive(Clone, Copy)]
enum Progress {
    Body(Instant),
    Answer(Instant),
}

struct Sending {
    rest: Bytes,
    progress: Arc<Mutex<Progress>>,
}

impl Body for Sending {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        if self.rest.is_empty() {
            return Poll::Ready(None);
        }
        let take = self.rest.len().min(SLICE);
        let slice = self.rest.split_to(take);
        let now = Instant::now();
        *self.progress.lock().unwrap() = if self.rest.is_empty() {
            Progress::Answer(now)
        } else {
            Progress::Body(now)
        };
        Poll::Ready(Some(Ok(Frame::data(slice))))
    }

    fn is_end_stream(&self) -> bool {
        self.rest.is_empty()
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.rest.len() as u64)
    }
}

fn distinct(chunks: &[ChunkRef]) -> Vec<ChunkRef> {
    let mut seen = HashSet::new();
    chunks
        .iter()
        .filter(|chunk| seen.insert(chunk.hash.clone()))
        .cloned()
        .collect()
}

fn expiring(target: &Target) -> bool {
    expiring_at(target, SystemTime::now())
}

fn expiring_at(target: &Target, now: SystemTime) -> bool {
    if target.expires_at_ms <= 0 {
        return false;
    }
    let now = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    (now + TARGET_MARGIN).as_millis() >= target.expires_at_ms as u128
}

fn by_code<T>(refused: ChunkError) -> Result<Attempt<T>, ChunkError> {
    let ChunkError::Refused {
        error: Some(error), ..
    } = &refused
    else {
        return Err(refused);
    };
    match error.resend() {
        Resend::Never => Ok(Attempt::Fail(refused)),
        Resend::Allowed => Ok(Attempt::Retry(refused)),
        Resend::ByCode => Err(refused),
    }
}

fn retryable(status: StatusCode) -> bool {
    status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

async fn small_answer(mut response: reqwest::Response) -> Result<Option<Vec<u8>>, reqwest::Error> {
    let mut body = Vec::new();
    while let Some(part) = response.chunk().await? {
        if body.len() + part.len() > SMALL_ANSWER {
            return Ok(None);
        }
        body.extend_from_slice(&part);
    }
    Ok(Some(body))
}

fn protobuf(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/x-protobuf"))
}

fn unavailable(chunk: &ChunkRef, error: reqwest::Error) -> ChunkError {
    let (cause, message) = Cause::of(&error.without_url());
    ChunkError::Unavailable {
        hash: chunk.hash.clone(),
        message,
        cause: Some(cause),
    }
}

#[cfg(test)]
mod tests;
