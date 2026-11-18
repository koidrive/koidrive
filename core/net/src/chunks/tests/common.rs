// SPDX-License-Identifier: Apache-2.0

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use koidrive_proto::v1::Error as ProtoError;
use koidrive_proto::v1::error::Code;
use koidrive_proto::v1::{
    BundledChunkResult, ChunkPlan, ChunkTarget, DownloadTargets, PrepareUploadResult, Welcome,
    rpc_request, rpc_response,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Instant;

use super::*;
use crate::session::tests::{accept, config, listen, recv_rpc, reply, welcome};
use crate::session::{CHUNK_BUNDLES, Events};

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
    plans: impl FnMut(usize) -> Vec<ChunkPlan> + Send + 'static,
) -> (Session, Events, Arc<Mutex<Vec<Vec<ChunkRef>>>>) {
    scripted_session_with(welcome("h", 0), plans).await
}

async fn scripted_session_with(
    welcome: Welcome,
    mut plans: impl FnMut(usize) -> Vec<ChunkPlan> + Send + 'static,
) -> (Session, Events, Arc<Mutex<Vec<Vec<ChunkRef>>>>) {
    let (listener, auth) = listen().await;
    let (session, events) = Session::start(auth, config()).unwrap();
    let asked = Arc::new(Mutex::new(Vec::new()));
    let calls = asked.clone();
    tokio::spawn(async move {
        let mut ws = accept(&listener, welcome).await;
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

fn retry_later(chunk: &ChunkRef) -> ChunkPlan {
    ChunkPlan {
        hash: chunk.hash.clone(),
        action: Action::RetryLater as i32,
        retry_after_ms: 1,
        ..Default::default()
    }
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
