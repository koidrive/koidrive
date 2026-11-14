// SPDX-License-Identifier: Apache-2.0

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use koidrive_proto::v1::Error as ProtoError;
use koidrive_proto::v1::error::Code;
use koidrive_proto::v1::{
    ChunkPlan, ChunkTarget, DownloadTargets, PrepareUploadResult, rpc_request, rpc_response,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Instant;

use super::*;
use crate::session::Events;
use crate::session::tests::{accept, config, listen, recv_rpc, reply, welcome};

#[tokio::test]
async fn upload_gate_reports_configured_concurrency_while_busy() {
    let gate = UploadGate::new(2);
    let permit = gate.enter().await;
    assert_eq!(gate.concurrency(), 2);
    assert_eq!(gate.clone().concurrency(), 2);
    drop(permit);
}

fn chunk(bytes: &[u8]) -> ChunkRef {
    ChunkRef {
        hash: Sha256::digest(bytes).to_vec(),
        size: bytes.len() as u32,
        encoding: 0,
    }
}

fn transfer() -> Transfer {
    let api = Api::local_development("http://127.0.0.1:9".parse().unwrap()).unwrap();
    Transfer {
        backoff: Backoff {
            initial: Duration::from_millis(1),
            max: Duration::from_millis(5),
        },
        ..Transfer::new(&api)
    }
}

async fn serve(
    responses: Vec<(u16, &'static str, Vec<u8>)>,
) -> (Target, tokio::task::JoinHandle<Vec<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/v1/spaces/s/chunks/x?ticket=secret",
        listener.local_addr().unwrap()
    );
    let server = tokio::spawn(async move {
        let mut bodies = Vec::new();
        for (status, content_type, body) in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            bodies.push(read_request(&mut socket).await.1);
            let head = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(&body).await.unwrap();
        }
        bodies
    });
    (
        Target {
            url,
            method: String::new(),
            headers: Default::default(),
            expires_at_ms: 0,
        },
        server,
    )
}

async fn read_request(socket: &mut TcpStream) -> (String, Vec<u8>) {
    let mut request = Vec::new();
    let mut buffer = [0u8; 4096];
    let (head_end, length) = loop {
        let n = socket.read(&mut buffer).await.unwrap();
        assert!(n > 0, "the client left before the end of its request");
        request.extend_from_slice(&buffer[..n]);
        if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&request[..end]).to_lowercase();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .map_or(0, |value| value.trim().parse::<usize>().unwrap());
            break (end + 4, length);
        }
    };
    while request.len() < head_end + length {
        let n = socket.read(&mut buffer).await.unwrap();
        assert!(n > 0, "the client left before the end of its request");
        request.extend_from_slice(&buffer[..n]);
    }
    (
        String::from_utf8_lossy(&request[..head_end]).into_owned(),
        request[head_end..].to_vec(),
    )
}

async fn endless(
    status: u16,
    content_type: &'static str,
    part: Vec<u8>,
    every: Duration,
) -> (Target, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/chunk", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        let head = format!(
            "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\ntransfer-encoding: chunked\r\n\r\n"
        );
        let mut framed = format!("{:x}\r\n", part.len()).into_bytes();
        framed.extend_from_slice(&part);
        framed.extend_from_slice(b"\r\n");
        let _ = socket.write_all(head.as_bytes()).await;
        while socket.write_all(&framed).await.is_ok() {
            tokio::time::sleep(every).await;
        }
    });
    let target = Target {
        url,
        ..Default::default()
    };
    (target, server)
}

#[test]
fn matches_checks_hash_and_size() {
    let good = chunk(b"hello");
    assert!(matches(&good, b"hello"));
    assert!(!matches(&good, b"hellp"));
    assert!(!matches(&ChunkRef { size: 6, ..good }, b"hello"));
}

#[test]
fn distinct_keeps_the_first_of_each_hash_in_order() {
    let (a, b) = (chunk(b"a"), chunk(b"b"));
    assert_eq!(distinct(&[a.clone(), b.clone(), a.clone()]), vec![a, b]);
}

#[test]
fn a_target_near_its_expiry_is_renewed() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let target = |expires_at_ms| Target {
        expires_at_ms,
        ..Default::default()
    };
    assert!(expiring(&target(now + 30_000)));
    assert!(!expiring(&target(now + 600_000)));
    assert!(!expiring(&target(0)), "no expiry given");
}

#[tokio::test]
async fn a_download_that_does_not_match_its_hash_is_retried_and_then_refused() {
    let wanted = chunk(b"the right bytes");
    let (target, server) = serve(vec![
        (200, "application/octet-stream", b"the wrong bytes".to_vec()),
        (
            200,
            "application/octet-stream",
            b"the right bytes!".to_vec(),
        ),
        (200, "application/octet-stream", b"the right bytes".to_vec()),
    ])
    .await;
    let transfer = transfer();
    for _ in 0..2 {
        assert!(matches!(
            transfer.get(&target, &wanted).await,
            Attempt::Retry(ChunkError::Corrupt { .. })
        ));
    }
    assert!(
        matches!(transfer.get(&target, &wanted).await, Attempt::Done(bytes) if bytes == b"the right bytes"[..])
    );
    server.await.unwrap();

    let (target, server) =
        serve(vec![(200, "application/octet-stream", b"nope".to_vec()); 3]).await;
    let transfer = Transfer {
        attempts: 3,
        ..transfer
    };
    let outcome = transfer
        .retrying(async || transfer.get(&target, &wanted).await)
        .await;
    assert!(
        matches!(outcome, Err(ChunkError::Corrupt { .. })),
        "{outcome:?}"
    );
    server.await.unwrap();
}

#[tokio::test]
async fn an_upload_returns_the_relay_receipt_and_sends_the_bytes_unchanged() {
    let sent = chunk(b"payload");
    let receipt = UploadReceipt {
        hash: sent.hash.clone(),
        size: sent.size,
        token: "v1.1.mac".into(),
    };
    let (target, server) = serve(vec![(
        200,
        "application/x-protobuf",
        receipt.encode_to_vec(),
    )])
    .await;
    let outcome = transfer()
        .put(&target, &sent, Bytes::from_static(b"payload"))
        .await;
    assert!(matches!(outcome, Attempt::Done(Some(r)) if r == receipt));
    assert_eq!(server.await.unwrap(), [b"payload".to_vec()]);
}

#[tokio::test]
async fn upload_responses_are_classified() {
    let sent = chunk(b"payload");
    let mismatch = ProtoError {
        code: 3,
        message: "the body does not match the chunk hash".into(),
        ..Default::default()
    };
    let other = UploadReceipt {
        hash: vec![0; 32],
        size: 7,
        token: "t".into(),
    };
    let (target, server) = serve(vec![
        (
            403,
            "application/x-protobuf",
            ProtoError {
                code: 2,
                ..Default::default()
            }
            .encode_to_vec(),
        ),
        (400, "application/x-protobuf", mismatch.encode_to_vec()),
        (503, "text/plain", Vec::new()),
        (
            413,
            "application/x-protobuf",
            ProtoError {
                code: 5,
                ..Default::default()
            }
            .encode_to_vec(),
        ),
        (200, "application/x-protobuf", other.encode_to_vec()),
        (200, "application/xml", Vec::new()),
    ])
    .await;
    let transfer = transfer();
    let put = async || {
        transfer
            .put(&target, &sent, Bytes::from_static(b"payload"))
            .await
    };
    assert!(
        matches!(put().await, Attempt::Expired),
        "a refused ticket is renewed"
    );
    match put().await {
        Attempt::Retry(ChunkError::Refused {
            status: 400,
            error: Some(error),
            ..
        }) => assert_eq!(error.message(), mismatch.message),
        _ => panic!("a hash mismatch on the way is retried"),
    }
    assert!(matches!(
        put().await,
        Attempt::Retry(ChunkError::Refused { status: 503, .. })
    ));
    assert!(matches!(
        put().await,
        Attempt::Fail(ChunkError::Refused { status: 413, .. })
    ));
    assert!(
        matches!(put().await, Attempt::Fail(_)),
        "a receipt for another chunk is refused"
    );
    assert!(
        matches!(put().await, Attempt::Done(None)),
        "a presigned upload has no receipt"
    );
    server.await.unwrap();
}

#[test]
fn errors_do_not_show_tickets() {
    let error = ChunkError::Unavailable {
        hash: vec![0xab; 2],
        message: "connection refused".into(),
        cause: Some(Cause::Refused),
    };
    assert_eq!(
        error.to_string(),
        "the transfer of chunk abab kept failing: connection refused"
    );
}

#[tokio::test]
async fn an_endless_receipt_or_error_is_not_read_past_a_small_limit() {
    let sent = chunk(b"payload");
    let transfer = transfer();
    let quickly = Duration::from_secs(5);

    let (target, server) = endless(
        200,
        "application/x-protobuf",
        vec![0; 16 << 10],
        Duration::ZERO,
    )
    .await;
    let put = tokio::time::timeout(
        quickly,
        transfer.put(&target, &sent, Bytes::from_static(b"payload")),
    )
    .await;
    assert!(
        matches!(
            put,
            Ok(Attempt::Fail(ChunkError::Refused {
                status: 200,
                error: None,
                ..
            }))
        ),
        "an oversized receipt is refused without reading it all"
    );
    server.abort();

    let (target, server) = endless(
        500,
        "application/x-protobuf",
        vec![0; 16 << 10],
        Duration::ZERO,
    )
    .await;
    let get = tokio::time::timeout(quickly, transfer.get(&target, &sent)).await;
    assert!(
        matches!(
            get,
            Ok(Attempt::Retry(ChunkError::Refused {
                status: 500,
                error: None,
                ..
            }))
        ),
        "an oversized error is dropped without reading it all"
    );
    server.abort();

    let (target, server) = endless(200, "text/plain", vec![0; 16 << 10], Duration::ZERO).await;
    let put = tokio::time::timeout(
        quickly,
        transfer.put(&target, &sent, Bytes::from_static(b"payload")),
    )
    .await;
    assert!(
        matches!(put, Ok(Attempt::Done(None))),
        "the body of a presigned answer is not read"
    );
    server.abort();
}

#[tokio::test]
async fn an_answer_that_trickles_is_cut_off_by_the_attempt_timeout() {
    let wanted = chunk(&[7; 1000]);
    let transfer = Transfer {
        attempt_timeout: Duration::from_millis(300),
        ..transfer()
    };
    let every = Duration::from_millis(50);

    let (target, server) = endless(200, "application/octet-stream", vec![7], every).await;
    let started = Instant::now();
    let get = transfer.get(&target, &wanted).await;
    assert!(matches!(
        get,
        Attempt::Retry(ChunkError::Unavailable { .. })
    ));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    server.abort();

    let (target, server) = endless(200, "application/x-protobuf", vec![0], every).await;
    let started = Instant::now();
    let put = transfer
        .put(&target, &wanted, Bytes::from(vec![7; 1000]))
        .await;
    assert!(matches!(
        put,
        Attempt::Retry(ChunkError::Unavailable { .. })
    ));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    server.abort();
}

#[tokio::test]
async fn a_429_is_retried_after_the_wait_the_server_asks() {
    let sent = chunk(b"payload");
    let busy = ProtoError {
        code: Code::RetryLater as i32,
        retry_after_ms: 300,
        ..Default::default()
    };
    let (target, server) = serve(vec![
        (429, "application/x-protobuf", busy.encode_to_vec()),
        (200, "application/xml", Vec::new()),
    ])
    .await;
    let transfer = transfer();
    assert!(transfer.backoff.max < Duration::from_millis(300));
    let started = Instant::now();
    let outcome = transfer
        .retrying(async || {
            transfer
                .put(&target, &sent, Bytes::from_static(b"payload"))
                .await
        })
        .await;
    assert!(matches!(outcome, Ok(Some(None))), "{outcome:?}");
    assert!(
        started.elapsed() >= Duration::from_millis(300),
        "{:?}",
        started.elapsed()
    );
    server.await.unwrap();
}

#[tokio::test]
async fn a_target_inside_the_expiry_margin_is_still_tried() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let wanted = chunk(b"hello");
    let (target, server) = serve(vec![
        (200, "application/octet-stream", b"hello".to_vec()),
        (200, "application/xml", Vec::new()),
    ])
    .await;
    let target = Target {
        expires_at_ms: now + 30_000,
        ..target
    };
    assert!(expiring(&target));
    let transfer = transfer();
    assert!(
        matches!(transfer.get(&target, &wanted).await, Attempt::Done(bytes) if bytes == b"hello"[..])
    );
    assert!(matches!(
        transfer
            .put(&target, &wanted, Bytes::from_static(b"hello"))
            .await,
        Attempt::Done(None)
    ));
    server.await.unwrap();
}

fn upload_plan(relay: SocketAddr, chunk: &ChunkRef, ticket: &str, lifetime: Duration) -> ChunkPlan {
    let expires = SystemTime::now() + lifetime;
    ChunkPlan {
        hash: chunk.hash.clone(),
        action: Action::Upload as i32,
        target: Some(Target {
            url: format!("http://{relay}/v1/spaces/s/chunks/x?ticket={ticket}"),
            expires_at_ms: expires.duration_since(UNIX_EPOCH).unwrap().as_millis() as i64,
            ..Default::default()
        }),
        retry_after_ms: 0,
    }
}

async fn relay() -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let tickets = Arc::new(Mutex::new(Vec::new()));
    let seen = tickets.clone();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (head, body) = read_request(&mut socket).await;
            let (_, query) = head.split_once("ticket=").unwrap();
            let ticket = query.split(' ').next().unwrap().to_owned();
            let (status, answer) = if ticket == "stale" {
                (403, Vec::new())
            } else {
                let receipt = UploadReceipt {
                    hash: Sha256::digest(&body).to_vec(),
                    size: body.len() as u32,
                    token: ticket.clone(),
                };
                (200, receipt.encode_to_vec())
            };
            seen.lock().unwrap().push(ticket);
            let head = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                answer.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(&answer).await.unwrap();
        }
    });
    (address, tickets)
}

async fn scripted_session(
    mut plans: impl FnMut(usize) -> Vec<ChunkPlan> + Send + 'static,
) -> (Session, Events, Arc<Mutex<Vec<Vec<ChunkRef>>>>) {
    let (listener, auth) = listen().await;
    let (session, events) = Session::start(auth, config()).unwrap();
    let asked = Arc::new(Mutex::new(Vec::new()));
    let calls = asked.clone();
    tokio::spawn(async move {
        let mut ws = accept(&listener, welcome("h", 0)).await;
        loop {
            let request = recv_rpc(&mut ws).await;
            let Some(rpc_request::Call::PrepareUpload(prepare)) = request.call else {
                panic!("expected PrepareUpload, got {:?}", request.call);
            };
            let round = {
                let mut calls = calls.lock().unwrap();
                calls.push(prepare.chunks);
                calls.len() - 1
            };
            let result = rpc_response::Result::PrepareUpload(PrepareUploadResult {
                chunks: plans(round),
            });
            reply(&mut ws, request.req_id, result).await;
        }
    });
    (session, events, asked)
}

#[tokio::test]
async fn an_upload_target_that_expires_during_its_batch_is_prepared_again_before_the_chunk_is_read()
{
    let a = chunk(b"chunk a");
    let b = chunk(b"chunk b");
    let (relay, tickets) = relay().await;
    let (session, _events, asked) = scripted_session({
        let (a, b) = (a.clone(), b.clone());
        move |round| match round {
            0 => vec![
                upload_plan(relay, &b, "b", Duration::from_secs(3600)),
                upload_plan(relay, &a, "stale", Duration::from_millis(61_000)),
            ],
            _ => vec![upload_plan(relay, &a, "fresh", Duration::from_secs(3600))],
        }
    })
    .await;

    let mut reads = Vec::new();
    let receipts = tokio::time::timeout(
        Duration::from_secs(30),
        Transfer {
            gate: UploadGate::new(1),
            ..transfer()
        }
        .upload(&session, &[b.clone(), a.clone()], |chunk| {
            reads.push(chunk.hash.clone());
            let (bytes, delay): (&'static [u8], _) = if chunk.hash == b.hash {
                (b"chunk b", Duration::from_millis(1500))
            } else {
                (b"chunk a", Duration::ZERO)
            };
            async move {
                tokio::time::sleep(delay).await;
                Ok(Bytes::from_static(bytes))
            }
        }),
    )
    .await
    .expect("a chunk deferred for expiry gives its place in the gate back")
    .unwrap();

    assert_eq!(*tickets.lock().unwrap(), ["b", "fresh"]);
    assert_eq!(reads, [b.hash.clone(), a.hash.clone()]);
    assert_eq!(
        *asked.lock().unwrap(),
        [vec![b.clone(), a.clone()], vec![a.clone()]]
    );
    let receipts: Vec<_> = receipts
        .into_iter()
        .map(|receipt| (receipt.hash, receipt.token))
        .collect();
    assert_eq!(
        receipts,
        [(b.hash, "b".to_owned()), (a.hash, "fresh".to_owned())]
    );
}

#[tokio::test]
async fn an_upload_target_already_inside_the_expiry_margin_when_prepared_is_sent_as_given() {
    let a = chunk(b"chunk a");
    let (relay, tickets) = relay().await;
    let (session, _events, asked) = scripted_session({
        let a = a.clone();
        move |_| vec![upload_plan(relay, &a, "skewed", Duration::from_secs(30))]
    })
    .await;

    let mut reads = 0;
    let receipts = transfer()
        .upload(&session, std::slice::from_ref(&a), |_| {
            reads += 1;
            async { Ok(Bytes::from_static(b"chunk a")) }
        })
        .await
        .unwrap();

    assert_eq!(*asked.lock().unwrap(), [vec![a.clone()]]);
    assert_eq!(*tickets.lock().unwrap(), ["skewed"]);
    assert_eq!(reads, 1);
    let receipts: Vec<_> = receipts
        .into_iter()
        .map(|receipt| (receipt.hash, receipt.token))
        .collect();
    assert_eq!(receipts, [(a.hash, "skewed".to_owned())]);
}

fn retry_later(chunk: &ChunkRef) -> ChunkPlan {
    ChunkPlan {
        hash: chunk.hash.clone(),
        action: Action::RetryLater as i32,
        retry_after_ms: 1,
        ..Default::default()
    }
}

#[tokio::test]
async fn upload_rounds_that_make_progress_do_not_count_towards_the_round_limit() {
    let [a, b, c] = [b"chunk a", b"chunk b", b"chunk c"].map(|bytes| chunk(bytes));
    let (relay, tickets) = relay().await;
    let (session, _events, asked) = scripted_session({
        let (a, b, c) = (a.clone(), b.clone(), c.clone());
        let hour = Duration::from_secs(3600);
        move |round| match round {
            0 => vec![
                upload_plan(relay, &a, "a", hour),
                retry_later(&b),
                retry_later(&c),
            ],
            1 => vec![upload_plan(relay, &b, "b", hour), retry_later(&c)],
            _ => vec![upload_plan(relay, &c, "c", hour)],
        }
    })
    .await;

    let receipts = Transfer {
        attempts: 1,
        ..transfer()
    }
    .upload(&session, &[a.clone(), b.clone(), c.clone()], |chunk| {
        let bytes: &'static [u8] = if chunk.hash == a.hash {
            b"chunk a"
        } else if chunk.hash == b.hash {
            b"chunk b"
        } else {
            b"chunk c"
        };
        async move { Ok(Bytes::from_static(bytes)) }
    })
    .await
    .unwrap();

    assert_eq!(asked.lock().unwrap().len(), 3);
    assert_eq!(*tickets.lock().unwrap(), ["a", "b", "c"]);
    let receipts: Vec<_> = receipts.into_iter().map(|receipt| receipt.hash).collect();
    assert_eq!(receipts, [a.hash, b.hash, c.hash]);
}

#[tokio::test]
async fn upload_rounds_without_progress_count_towards_the_round_limit() {
    let a = chunk(b"chunk a");
    let b = chunk(b"chunk b");
    let (relay, _tickets) = relay().await;
    let (session, _events, asked) = scripted_session({
        let (a, b) = (a.clone(), b.clone());
        move |round| match round {
            0 => vec![
                upload_plan(relay, &a, "a", Duration::from_secs(3600)),
                retry_later(&b),
            ],
            _ => vec![retry_later(&b)],
        }
    })
    .await;

    let outcome = Transfer {
        attempts: 1,
        ..transfer()
    }
    .upload(&session, &[a.clone(), b.clone()], |_| async {
        Ok(Bytes::from_static(b"chunk a"))
    })
    .await;

    assert!(matches!(outcome, Err(ChunkError::Unavailable { hash, .. }) if hash == b.hash));
    assert_eq!(asked.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn refusals_follow_the_server_error_code_before_the_http_status() {
    let sent = chunk(b"payload");
    let error = |code: i32, retry_after_ms: u64| {
        ProtoError {
            code,
            retry_after_ms,
            ..Default::default()
        }
        .encode_to_vec()
    };
    let protobuf = "application/x-protobuf";
    let (target, server) = serve(vec![
        (503, protobuf, error(999, 0)),
        (429, protobuf, error(Code::QuotaExceeded as i32, 0)),
        (429, protobuf, error(Code::QuotaExceeded as i32, 100)),
        (503, protobuf, error(Code::Internal as i32, 0)),
        (500, "text/plain", Vec::new()),
        (
            500,
            protobuf,
            ProtoError {
                message: "no code".into(),
                ..Default::default()
            }
            .encode_to_vec(),
        ),
        (500, protobuf, Vec::new()),
    ])
    .await;
    let transfer = transfer();
    let put = async || {
        transfer
            .put(&target, &sent, Bytes::from_static(b"payload"))
            .await
    };
    assert!(
        matches!(put().await, Attempt::Fail(_)),
        "an unknown code is a refusal"
    );
    assert!(
        matches!(put().await, Attempt::Fail(_)),
        "a quota without a wait is a refusal"
    );
    assert!(
        matches!(put().await, Attempt::Retry(_)),
        "a quota with a wait is retried"
    );
    assert!(
        matches!(put().await, Attempt::Retry(_)),
        "a known code keeps its status mapping"
    );
    assert!(
        matches!(put().await, Attempt::Retry(_)),
        "no error: the status decides"
    );
    assert!(
        matches!(transfer.get(&target, &sent).await, Attempt::Fail(_)),
        "UNSPECIFIED is a refusal"
    );
    assert!(
        matches!(
            transfer.get(&target, &sent).await,
            Attempt::Retry(ChunkError::Refused {
                status: 500,
                error: None,
                ..
            })
        ),
        "an empty body is no refusal: the status decides"
    );
    server.await.unwrap();
}

async fn accept_upload() -> (Target, tokio::task::JoinHandle<(TcpStream, usize, usize)>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/chunk", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            let n = socket.read(&mut buffer).await.unwrap();
            head.extend_from_slice(&buffer[..n]);
            if let Some(end) = head.windows(4).position(|w| w == b"\r\n\r\n") {
                let text = String::from_utf8_lossy(&head[..end]).to_lowercase();
                let length = text
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .map_or(0, |value| value.trim().parse::<usize>().unwrap());
                return (socket, head.len() - end - 4, length);
            }
        }
    });
    let target = Target {
        url,
        ..Default::default()
    };
    (target, server)
}

fn outcome<T>(attempt: &Attempt<T>) -> String {
    match attempt {
        Attempt::Done(_) => "done".into(),
        Attempt::Expired => "expired".into(),
        Attempt::Retry(error) => format!("retry: {error}"),
        Attempt::Fail(error) => format!("fail: {error}"),
    }
}

#[tokio::test]
async fn an_upload_that_takes_longer_than_the_read_timeout_completes_while_it_moves() {
    let body = vec![7; 3_500_000];
    let sent = chunk(&body);
    let (target, server) = accept_upload().await;
    let server = tokio::spawn(async move {
        let (mut socket, mut received, length) = server.await.unwrap();
        let mut buffer = [0u8; 10_000];
        while received < length {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let n = socket.read(&mut buffer).await.unwrap();
            assert!(n > 0, "the client left after {received} bytes");
            received += n;
        }
        socket
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
            .await
            .unwrap();
        received
    });
    let started = Instant::now();
    let put = transfer().put(&target, &sent, Bytes::from(body)).await;
    assert!(matches!(put, Attempt::Done(None)), "{}", outcome(&put));
    assert_eq!(server.await.unwrap(), 3_500_000);
    assert!(
        started.elapsed() > Duration::from_secs(30),
        "the body is read at 0.8 Mbit/s, so it takes longer than the client's 30 s read timeout: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn an_upload_whose_body_stops_moving_is_retried_after_the_stall_timeout() {
    let body = vec![7; 32 << 20];
    let sent = chunk(&body);
    let transfer = Transfer {
        stall_timeout: Duration::from_millis(300),
        ..transfer()
    };
    let (target, server) = accept_upload().await;
    let started = Instant::now();
    let put = transfer.put(&target, &sent, Bytes::from(body)).await;
    assert!(
        matches!(&put, Attempt::Retry(ChunkError::Unavailable { message, .. }) if message.contains("did not move")),
        "{}",
        outcome(&put)
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    server.abort();
}

const HOUR: Duration = Duration::from_secs(3600);

#[derive(Clone, Copy)]
struct Reply {
    after: Duration,
    status: u16,
}

fn ok_after(after: Duration) -> Reply {
    Reply { after, status: 200 }
}

#[derive(Default)]
struct Seen {
    tickets: Vec<String>,
    open: usize,
    peak: usize,
    left: Vec<String>,
}

async fn busy_relay(
    reply: impl Fn(&str, usize) -> Reply + Send + Sync + 'static,
) -> (SocketAddr, Arc<Mutex<Seen>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let (log, reply) = (seen.clone(), Arc::new(reply));
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (seen, reply) = (log.clone(), reply.clone());
            tokio::spawn(async move {
                let (head, body) = read_request(&mut socket).await;
                let (_, query) = head.split_once("ticket=").unwrap();
                let ticket = query.split(' ').next().unwrap().to_owned();
                let answer = {
                    let mut seen = seen.lock().unwrap();
                    let earlier = seen.tickets.iter().filter(|t| **t == ticket).count();
                    seen.tickets.push(ticket.clone());
                    seen.open += 1;
                    seen.peak = seen.peak.max(seen.open);
                    reply(&ticket, earlier)
                };
                let mut probe = [0u8; 1];
                let left = tokio::select! {
                    _ = tokio::time::sleep(answer.after) => false,
                    _ = socket.read(&mut probe) => true,
                };
                {
                    let mut seen = seen.lock().unwrap();
                    seen.open -= 1;
                    if left {
                        seen.left.push(ticket);
                        return;
                    }
                }
                let (content_type, body) = if answer.status == 200 {
                    let receipt = UploadReceipt {
                        hash: Sha256::digest(&body).to_vec(),
                        size: body.len() as u32,
                        token: ticket,
                    };
                    ("application/x-protobuf", receipt.encode_to_vec())
                } else {
                    ("text/plain", Vec::new())
                };
                let head = format!(
                    "HTTP/1.1 {} X\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    answer.status,
                    body.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(&body).await;
            });
        }
    });
    (address, seen)
}

fn named(count: usize) -> Vec<(String, ChunkRef, Bytes)> {
    (0..count)
        .map(|i| {
            let bytes = Bytes::from(format!("chunk {i}"));
            (format!("c{i}"), chunk(&bytes), bytes)
        })
        .collect()
}

fn refs(parts: &[(String, ChunkRef, Bytes)]) -> Vec<ChunkRef> {
    parts.iter().map(|(_, chunk, _)| chunk.clone()).collect()
}

fn names(parts: &[(String, ChunkRef, Bytes)]) -> Vec<String> {
    parts.iter().map(|(name, _, _)| name.clone()).collect()
}

async fn uploads_through(
    relay: SocketAddr,
    parts: &[(String, ChunkRef, Bytes)],
) -> (Session, Events, Arc<Mutex<Vec<Vec<ChunkRef>>>>) {
    let plans: Vec<_> = parts
        .iter()
        .map(|(name, chunk, _)| upload_plan(relay, chunk, name, HOUR))
        .collect();
    scripted_session(move |_| plans.clone()).await
}

fn source(
    parts: &[(String, ChunkRef, Bytes)],
) -> impl FnMut(&ChunkRef) -> std::future::Ready<io::Result<Bytes>> + use<> {
    let bytes: HashMap<_, _> = parts
        .iter()
        .map(|(_, chunk, bytes)| (chunk.hash.clone(), bytes.clone()))
        .collect();
    move |chunk| std::future::ready(Ok(bytes[&chunk.hash].clone()))
}

fn tokens(receipts: Vec<UploadReceipt>) -> Vec<String> {
    receipts.into_iter().map(|receipt| receipt.token).collect()
}

fn sorted(mut names: Vec<String>) -> Vec<String> {
    names.sort();
    names
}

#[tokio::test]
async fn uploads_keep_as_many_chunks_in_flight_as_the_gate_allows_and_no_more() {
    let parts = named(12);
    let (relay, seen) = busy_relay(|_, _| ok_after(Duration::from_millis(100))).await;
    let (session, _events, _) = uploads_through(relay, &parts).await;

    let receipts = transfer()
        .upload(&session, &refs(&parts), source(&parts))
        .await
        .unwrap();

    assert_eq!(sorted(tokens(receipts)), sorted(names(&parts)));
    assert_eq!(seen.lock().unwrap().peak, DEFAULT_UPLOAD_CONCURRENCY);
}

#[tokio::test]
async fn uploads_that_finish_out_of_order_read_and_send_each_distinct_chunk_once() {
    let parts = named(10);
    let (sent, present) = parts.split_at(8);
    let (relay, seen) = busy_relay(|ticket, _| {
        let index: u64 = ticket[1..].parse().unwrap();
        ok_after(Duration::from_millis((8 - index) * 20))
    })
    .await;
    let (session, _events, _) = scripted_session({
        let mut plans: Vec<_> = sent
            .iter()
            .map(|(name, chunk, _)| upload_plan(relay, chunk, name, HOUR))
            .collect();
        plans.extend(present.iter().map(|(_, chunk, _)| ChunkPlan {
            hash: chunk.hash.clone(),
            action: Action::Present as i32,
            ..Default::default()
        }));
        move |_| plans.clone()
    })
    .await;
    let mut chunks = refs(&parts);
    chunks.extend(refs(&parts[..4]));

    let mut reads = Vec::new();
    let mut read = source(&parts);
    let receipts = transfer()
        .upload(&session, &chunks, |chunk| {
            reads.push(chunk.hash.clone());
            read(chunk)
        })
        .await
        .unwrap();

    let receipts = tokens(receipts);
    assert_ne!(receipts, names(sent), "later uploads finished first");
    assert_eq!(sorted(receipts), names(sent));
    reads.sort();
    let mut expected: Vec<_> = refs(sent).into_iter().map(|chunk| chunk.hash).collect();
    expected.sort();
    assert_eq!(
        reads, expected,
        "each chunk to send read once, present ones never"
    );
    assert_eq!(sorted(seen.lock().unwrap().tickets.clone()), names(sent));
}

#[tokio::test]
async fn a_chunk_that_is_retried_does_not_hold_up_the_others() {
    let parts = named(6);
    let (relay, seen) = busy_relay(|ticket, earlier| match (ticket, earlier) {
        ("c0", 0 | 1) => Reply {
            after: Duration::from_millis(100),
            status: 503,
        },
        ("c0", _) => ok_after(Duration::ZERO),
        _ => ok_after(Duration::from_millis(10)),
    })
    .await;
    let (session, _events, _) = uploads_through(relay, &parts).await;

    let receipts = transfer()
        .upload(&session, &refs(&parts), source(&parts))
        .await
        .unwrap();

    let receipts = tokens(receipts);
    assert_eq!(receipts.last().unwrap(), "c0", "{receipts:?}");
    assert_eq!(sorted(receipts), names(&parts));
    let seen = seen.lock().unwrap();
    assert_eq!(seen.tickets.iter().filter(|t| *t == "c0").count(), 3);
}

#[tokio::test]
async fn a_chunk_that_changed_ends_the_upload_while_others_are_in_flight() {
    let parts = named(6);
    let changed = parts[2].1.clone();
    let (relay, seen) = busy_relay(|_, _| ok_after(Duration::from_secs(30))).await;
    let (session, _events, _) = uploads_through(relay, &parts).await;

    let (transfer, chunks) = (transfer(), refs(&parts));
    let mut reads = Vec::new();
    let mut read = source(&parts);
    let upload = transfer.upload(&session, &chunks, |chunk| {
        reads.push(chunk.hash.clone());
        let bytes = read(chunk);
        let changed = chunk.hash == changed.hash;
        async move {
            if changed {
                tokio::time::sleep(Duration::from_millis(50)).await;
                return Ok(Bytes::from_static(b"other bytes"));
            }
            bytes.await
        }
    });
    let outcome = tokio::time::timeout(Duration::from_secs(5), upload)
        .await
        .expect("the upload ends without waiting for the chunks in flight");

    assert!(
        matches!(&outcome, Err(ChunkError::SourceChanged { hash }) if *hash == changed.hash),
        "{outcome:?}"
    );
    let first_four: Vec<_> = refs(&parts[..4]).into_iter().map(|c| c.hash).collect();
    assert_eq!(reads, first_four, "no chunk is read after the error");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let seen = seen.lock().unwrap();
    assert!(
        seen.tickets
            .iter()
            .all(|ticket| ["c0", "c1", "c3"].contains(&ticket.as_str())),
        "nothing of the changed chunk is sent and nothing starts after it: {:?}",
        seen.tickets
    );
}

#[tokio::test]
async fn a_chunk_refused_for_good_ends_the_upload_without_waiting_for_slower_ones() {
    let parts = named(4);
    let (relay, seen) = busy_relay(|ticket, _| match ticket {
        "c3" => Reply {
            after: Duration::ZERO,
            status: 404,
        },
        _ => ok_after(Duration::from_secs(30)),
    })
    .await;
    let (session, _events, _) = uploads_through(relay, &parts).await;

    let (transfer, chunks) = (transfer(), refs(&parts));
    let upload = transfer.upload(&session, &chunks, source(&parts));
    let outcome = tokio::time::timeout(Duration::from_secs(5), upload)
        .await
        .expect("the upload ends without waiting for the slower chunks");

    assert!(
        matches!(outcome, Err(ChunkError::Refused { status: 404, .. })),
        "{outcome:?}"
    );
    let abandoned = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            {
                let seen = seen.lock().unwrap();
                let others: Vec<_> = seen
                    .tickets
                    .iter()
                    .filter(|t| *t != "c3")
                    .cloned()
                    .collect();
                if !others.is_empty() && sorted(seen.left.clone()) == sorted(others) {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    let seen = seen.lock().unwrap();
    assert!(
        abandoned.is_ok(),
        "the client closes every slower upload: {:?} sent, {:?} closed",
        seen.tickets,
        seen.left
    );
}

#[tokio::test]
async fn transfers_that_share_a_gate_share_its_limit() {
    let parts = named(12);
    let (relay, seen) = busy_relay(|_, _| ok_after(Duration::from_millis(50))).await;
    let gate = UploadGate::new(2);
    let (first, second) = parts.split_at(6);
    let (one, _one_events, _) = uploads_through(relay, first).await;
    let (two, _two_events, _) = uploads_through(relay, second).await;
    let sharing = || Transfer {
        gate: gate.clone(),
        ..transfer()
    };
    let (sharing_one, sharing_two) = (sharing(), sharing());
    let (first_chunks, second_chunks) = (refs(first), refs(second));

    let (a, b) = tokio::join!(
        sharing_one.upload(&one, &first_chunks, source(first)),
        sharing_two.upload(&two, &second_chunks, source(second)),
    );

    assert_eq!(a.unwrap().len() + b.unwrap().len(), 12);
    assert_eq!(seen.lock().unwrap().peak, 2);
}

#[tokio::test]
async fn one_chunk_at_a_time_reads_and_sends_in_the_order_of_the_chunks() {
    let [a, b, c, d, e] = [b"chunk a", b"chunk b", b"chunk c", b"chunk d", b"chunk e"]
        .map(|bytes| (chunk(bytes), Bytes::from_static(bytes)));
    let (relay, seen) = busy_relay(|ticket, _| match ticket {
        "stale" => Reply {
            after: Duration::ZERO,
            status: 403,
        },
        _ => ok_after(Duration::ZERO),
    })
    .await;
    let (session, _events, asked) = scripted_session({
        let [a, b, c, d, e] = [&a, &b, &c, &d, &e].map(|(chunk, _)| chunk.clone());
        move |round| match round {
            0 => vec![
                upload_plan(relay, &a, "a", HOUR),
                retry_later(&b),
                upload_plan(relay, &c, "stale", HOUR),
                ChunkPlan {
                    hash: d.hash.clone(),
                    action: Action::Present as i32,
                    ..Default::default()
                },
                upload_plan(relay, &e, "e", HOUR),
            ],
            _ => vec![
                upload_plan(relay, &b, "b", HOUR),
                upload_plan(relay, &c, "c", HOUR),
            ],
        }
    })
    .await;
    let all = [&a, &b, &c, &d, &e];
    let chunks: Vec<_> = all.iter().map(|(chunk, _)| chunk.clone()).collect();
    let name = |hash: &[u8]| {
        ["a", "b", "c", "d", "e"][all.iter().position(|(c, _)| c.hash == hash).unwrap()]
    };

    let mut reads = Vec::new();
    let receipts = Transfer {
        gate: UploadGate::new(1),
        ..transfer()
    }
    .upload(&session, &chunks, |chunk| {
        reads.push(name(&chunk.hash));
        let bytes = all
            .iter()
            .find(|(c, _)| c.hash == chunk.hash)
            .unwrap()
            .1
            .clone();
        std::future::ready(Ok(bytes))
    })
    .await
    .unwrap();

    assert_eq!(reads, ["a", "c", "e", "b", "c"]);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.tickets, ["a", "stale", "e", "b", "c"]);
    assert_eq!(seen.peak, 1);
    assert_eq!(tokens(receipts), ["a", "e", "b", "c"]);
    let asked: Vec<Vec<_>> = asked
        .lock()
        .unwrap()
        .iter()
        .map(|round| round.iter().map(|chunk| name(&chunk.hash)).collect())
        .collect();
    assert_eq!(asked, [vec!["a", "b", "c", "d", "e"], vec!["b", "c"]]);
}

#[tokio::test]
async fn four_chunks_at_once_take_at_most_a_third_of_the_time_of_one_at_a_time() {
    let parts = named(100);
    let (relay, _) = busy_relay(|_, _| ok_after(Duration::from_millis(50))).await;
    let mut took = Vec::new();
    for concurrency in [1, 4] {
        let (session, _events, _) = uploads_through(relay, &parts).await;
        let transfer = Transfer {
            gate: UploadGate::new(concurrency),
            ..transfer()
        };
        let started = Instant::now();
        let receipts = transfer
            .upload(&session, &refs(&parts), source(&parts))
            .await
            .unwrap();
        took.push(started.elapsed());
        assert_eq!(receipts.len(), 100);
    }
    assert!(
        took[1] * 3 <= took[0],
        "one at a time {:?}, four at once {:?}",
        took[0],
        took[1]
    );
}

#[derive(Default)]
struct Stored {
    gets: Vec<(String, String)>,
    open: usize,
    peak: usize,
}

async fn store(
    parts: &[(String, ChunkRef, Bytes)],
    hold: Duration,
    corrupt_first: Option<&str>,
) -> (SocketAddr, Arc<Mutex<Stored>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let stored = Arc::new(Mutex::new(Stored::default()));
    let bytes: Arc<HashMap<String, Bytes>> = Arc::new(
        parts
            .iter()
            .map(|(name, _, bytes)| (name.clone(), bytes.clone()))
            .collect(),
    );
    let corrupt_first = corrupt_first.map(str::to_owned);
    let log = stored.clone();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (log, bytes, corrupt_first) = (log.clone(), bytes.clone(), corrupt_first.clone());
            tokio::spawn(async move {
                let (head, _) = read_request(&mut socket).await;
                let (_, path) = head.split_once("/c/").unwrap();
                let (name, query) = path.split_once("?ticket=").unwrap();
                let ticket = query.split(' ').next().unwrap();
                let (status, body) = {
                    let mut stored = log.lock().unwrap();
                    let earlier = stored.gets.iter().filter(|(n, _)| n == name).count();
                    stored.gets.push((name.to_owned(), ticket.to_owned()));
                    stored.open += 1;
                    stored.peak = stored.peak.max(stored.open);
                    match ticket {
                        "stale" => (403, Vec::new()),
                        "gone" => (404, Vec::new()),
                        _ if earlier == 0 && corrupt_first.as_deref() == Some(name) => {
                            let mut wrong = bytes[name].to_vec();
                            wrong[0] ^= 1;
                            (200, wrong)
                        }
                        _ => (200, bytes[name].to_vec()),
                    }
                };
                tokio::time::sleep(hold).await;
                log.lock().unwrap().open -= 1;
                let head = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/octet-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(&body).await;
            });
        }
    });
    (address, stored)
}

fn download_target(store: SocketAddr, name: &str, chunk: &ChunkRef, ticket: &str) -> ChunkTarget {
    ChunkTarget {
        hash: chunk.hash.clone(),
        target: Some(Target {
            url: format!("http://{store}/c/{name}?ticket={ticket}"),
            ..Default::default()
        }),
    }
}

async fn targets_session(
    mut answer: impl FnMut(usize, &[Vec<u8>]) -> Vec<ChunkTarget> + Send + 'static,
) -> (Session, Events, Arc<Mutex<Vec<Vec<Vec<u8>>>>>) {
    let (listener, auth) = listen().await;
    let (session, events) = Session::start(auth, config()).unwrap();
    let asked = Arc::new(Mutex::new(Vec::new()));
    let calls = asked.clone();
    tokio::spawn(async move {
        let mut ws = accept(&listener, welcome("h", 0)).await;
        loop {
            let request = recv_rpc(&mut ws).await;
            let Some(rpc_request::Call::GetDownloadTargets(get)) = request.call else {
                panic!("expected GetDownloadTargets, got {:?}", request.call);
            };
            let round = {
                let mut calls = calls.lock().unwrap();
                calls.push(get.hashes.clone());
                calls.len() - 1
            };
            let result = rpc_response::Result::DownloadTargets(DownloadTargets {
                targets: answer(round, &get.hashes),
            });
            reply(&mut ws, request.req_id, result).await;
        }
    });
    (session, events, asked)
}

async fn downloads_from(
    store: SocketAddr,
    parts: &[(String, ChunkRef, Bytes)],
    ticket: impl Fn(usize, &str) -> &'static str + Send + 'static,
) -> (Session, Events, Arc<Mutex<Vec<Vec<Vec<u8>>>>>) {
    let by_hash: HashMap<Vec<u8>, (String, ChunkRef)> = parts
        .iter()
        .map(|(name, chunk, _)| (chunk.hash.clone(), (name.clone(), chunk.clone())))
        .collect();
    targets_session(move |round, hashes| {
        hashes
            .iter()
            .map(|hash| {
                let (name, chunk) = &by_hash[hash];
                download_target(store, name, chunk, ticket(round, name))
            })
            .collect()
    })
    .await
}

async fn download_all(
    transfer: &Transfer,
    session: &Session,
    chunks: &[ChunkRef],
) -> Result<Vec<(Vec<u8>, Bytes)>, ChunkError> {
    let mut written = Vec::new();
    transfer
        .download(session, chunks, |chunk, bytes| {
            written.push((chunk.hash.clone(), bytes));
            std::future::ready(Ok(()))
        })
        .await?;
    written.sort();
    Ok(written)
}

fn expected(parts: &[(String, ChunkRef, Bytes)]) -> Vec<(Vec<u8>, Bytes)> {
    let mut expected: Vec<_> = parts
        .iter()
        .map(|(_, chunk, bytes)| (chunk.hash.clone(), bytes.clone()))
        .collect();
    expected.sort();
    expected
}

#[tokio::test]
async fn downloads_keep_as_many_chunks_in_flight_as_the_limit_allows_and_no_more() {
    let parts = named(40);
    let (store, stored) = store(&parts, Duration::from_millis(200), None).await;
    let (session, _events, _) = downloads_from(store, &parts, |_, _| "good").await;

    let written = download_all(&transfer(), &session, &refs(&parts))
        .await
        .unwrap();

    assert_eq!(written, expected(&parts));
    let peak = stored.lock().unwrap().peak;
    assert_eq!(peak, DEFAULT_DOWNLOAD_CONCURRENCY.get());
    assert!(peak > 1);
}

#[tokio::test]
async fn downloads_ask_for_targets_a_thousand_distinct_hashes_at_a_time_and_write_each_chunk_once()
{
    let parts = named(2 * MAX_CHUNKS_PER_CALL + 100);
    let (store, stored) = store(&parts, Duration::ZERO, None).await;
    let (session, _events, asked) = downloads_from(store, &parts, |_, _| "good").await;
    let mut chunks = refs(&parts);
    chunks.extend(refs(&parts[..300]));

    let written = download_all(&transfer(), &session, &chunks).await.unwrap();

    assert_eq!(written, expected(&parts));
    let sizes: Vec<usize> = asked.lock().unwrap().iter().map(Vec::len).collect();
    assert_eq!(sizes, [MAX_CHUNKS_PER_CALL, MAX_CHUNKS_PER_CALL, 100]);
    assert_eq!(stored.lock().unwrap().gets.len(), parts.len());
}

#[tokio::test]
async fn a_refused_download_ticket_is_renewed_for_its_chunk_alone() {
    let parts = named(3);
    let (store, stored) = store(&parts, Duration::ZERO, None).await;
    let (session, _events, asked) = downloads_from(store, &parts, |round, name| {
        if round == 0 && name == "c1" {
            "stale"
        } else {
            "good"
        }
    })
    .await;

    let written = download_all(&transfer(), &session, &refs(&parts))
        .await
        .unwrap();

    assert_eq!(written, expected(&parts));
    assert_eq!(
        *asked.lock().unwrap(),
        [
            refs(&parts)
                .into_iter()
                .map(|chunk| chunk.hash)
                .collect::<Vec<_>>(),
            vec![parts[1].1.hash.clone()]
        ]
    );
    let mut gets = stored.lock().unwrap().gets.clone();
    gets.sort();
    let get = |name: &str, ticket: &str| (name.to_owned(), ticket.to_owned());
    assert_eq!(
        gets,
        [
            get("c0", "good"),
            get("c1", "good"),
            get("c1", "stale"),
            get("c2", "good")
        ]
    );
}

#[tokio::test]
async fn a_corrupt_chunk_is_fetched_again_and_only_its_right_bytes_are_written() {
    let parts = named(3);
    let (store, stored) = store(&parts, Duration::ZERO, Some("c2")).await;
    let (session, _events, _) = downloads_from(store, &parts, |_, _| "good").await;

    let written = download_all(&transfer(), &session, &refs(&parts))
        .await
        .unwrap();

    assert_eq!(written, expected(&parts));
    let gets = stored.lock().unwrap().gets.clone();
    let of = |name: &str| gets.iter().filter(|(n, _)| n == name).count();
    assert_eq!((of("c0"), of("c1"), of("c2")), (1, 1, 2));
}

#[tokio::test]
async fn a_chunk_refused_for_good_ends_the_download_and_names_its_hash() {
    let parts = named(20);
    let (store, _) = store(&parts, Duration::from_millis(20), None).await;
    let (session, _events, _) =
        downloads_from(
            store,
            &parts,
            |_, name| if name == "c5" { "gone" } else { "good" },
        )
        .await;

    let mut written = Vec::new();
    let outcome = transfer()
        .download(&session, &refs(&parts), |chunk, _| {
            written.push(chunk.hash.clone());
            std::future::ready(Ok(()))
        })
        .await;

    match outcome {
        Err(ChunkError::Refused { hash, status, .. }) => {
            assert_eq!((hash, status), (parts[5].1.hash.clone(), 404));
        }
        other => panic!("{other:?}"),
    }
    assert!(!written.contains(&parts[5].1.hash));
    assert!(written.len() < parts.len() - 1, "{} written", written.len());
}
