// SPDX-License-Identifier: Apache-2.0

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use koidrive_net::{Api, Auth, Credentials, Events, Session, SessionConfig};
use koidrive_proto::v1::{
    AccessToken, ChunkPlan, ClientMessage, PrepareUploadResult, RpcResponse, ServerMessage, Target,
    UploadReceipt, Welcome, chunk_plan, client_message, rpc_request, rpc_response, server_message,
};
use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message as WsMessage;

const HOLD_LIMIT: Duration = Duration::from_secs(5);
const SETTLE: Duration = Duration::from_millis(100);

#[derive(Default)]
pub(crate) struct Seen {
    pub peak: usize,
    pub hashes: Vec<Vec<u8>>,
    pub refuse: Option<Vec<u8>>,
    pub stored: Vec<Vec<u8>>,
}

pub(crate) async fn relay(expected: usize) -> (SocketAddr, Arc<Mutex<Seen>>) {
    relay_refusing(expected, 0).await
}

pub(crate) async fn relay_refusing(
    expected: usize,
    refused: usize,
) -> (SocketAddr, Arc<Mutex<Seen>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let log = seen.clone();
    let open = Arc::new(watch::Sender::new(0usize));
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (seen, open) = (log.clone(), open.clone());
            tokio::spawn(async move {
                let Some((ticket, size)) = read_upload(&mut socket).await else {
                    return;
                };
                let hash = unhex(&ticket);
                let (mut arrived, mut refuse) = (0, false);
                open.send_modify(|open| {
                    *open += 1;
                    let mut seen = seen.lock().unwrap();
                    seen.hashes.push(hash.clone());
                    arrived = seen.hashes.len();
                    refuse = seen.refuse.as_ref() == Some(&hash);
                    seen.peak = seen.peak.max(*open);
                });
                if arrived <= refused || refuse {
                    open.send_modify(|open| *open -= 1);
                    let head =
                        "HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
                    let _ = socket.write_all(head.as_bytes()).await;
                    return;
                }
                let mut enough = open.subscribe();
                let _ = tokio::time::timeout(HOLD_LIMIT, enough.wait_for(|open| *open >= expected))
                    .await;
                tokio::time::sleep(SETTLE).await;
                open.send_modify(|open| *open -= 1);
                seen.lock().unwrap().stored.push(hash.clone());
                let receipt = UploadReceipt {
                    hash,
                    size: size as u32,
                    token: ticket,
                }
                .encode_to_vec();
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    receipt.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(&receipt).await;
            });
        }
    });
    (address, seen)
}

async fn read_upload(socket: &mut TcpStream) -> Option<(String, usize)> {
    let mut request = Vec::new();
    let mut buffer = [0u8; 4096];
    let head_end = loop {
        let n = socket.read(&mut buffer).await.ok().filter(|n| *n > 0)?;
        request.extend_from_slice(&buffer[..n]);
        if let Some(at) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
    };
    let head = String::from_utf8_lossy(&request[..head_end]).into_owned();
    let length: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().unwrap())
        })
        .unwrap();
    while request.len() < head_end + length {
        let n = socket.read(&mut buffer).await.ok().filter(|n| *n > 0)?;
        request.extend_from_slice(&buffer[..n]);
    }
    let (_, query) = head.split_once("ticket=").unwrap();
    Some((query.split(' ').next().unwrap().to_owned(), length))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
        .collect()
}

pub(crate) async fn session(relay: SocketAddr) -> (Session, Events, Api) {
    session_holding(relay, Vec::new()).await
}

pub(crate) async fn session_holding(
    relay: SocketAddr,
    held: Vec<Vec<u8>>,
) -> (Session, Events, Api) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let api = Api::local_development(base.parse().unwrap()).unwrap();
    let in_an_hour = || {
        (SystemTime::now() + Duration::from_secs(3600))
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    };
    let credentials = Credentials {
        user_id: "u".into(),
        device_id: "d".into(),
        refresh_credential: "r".into(),
    };
    let auth = Auth::new(api.clone(), credentials).with_token(AccessToken {
        token: "token".into(),
        expires_at_ms: in_an_hour(),
        ..Default::default()
    });
    let config = SessionConfig::new("0190a0b0-0000-7000-8000-000000000001", "test", "0", 1);
    let (session, events) = Session::start(Arc::new(auth), config).unwrap();
    tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let send = |msg| WsMessage::Binary(ServerMessage { msg: Some(msg) }.encode_to_vec().into());
        let welcome = Welcome {
            proto: 1,
            history_id: "h".into(),
            ..Default::default()
        };
        let mut welcomed = false;
        while let Some(Ok(message)) = ws.next().await {
            let WsMessage::Binary(bytes) = message else {
                continue;
            };
            let reply = match ClientMessage::decode(bytes).unwrap().msg {
                Some(client_message::Msg::Hello(_)) if !welcomed => {
                    welcomed = true;
                    server_message::Msg::Welcome(welcome.clone())
                }
                Some(client_message::Msg::Rpc(request)) => {
                    let Some(rpc_request::Call::PrepareUpload(prepare)) = request.call else {
                        panic!("expected PrepareUpload, got {:?}", request.call);
                    };
                    let chunks = (prepare.chunks.iter())
                        .map(|chunk| match held.contains(&chunk.hash) {
                            true => ChunkPlan {
                                hash: chunk.hash.clone(),
                                action: chunk_plan::Action::Present as i32,
                                ..Default::default()
                            },
                            false => ChunkPlan {
                                hash: chunk.hash.clone(),
                                action: chunk_plan::Action::Upload as i32,
                                target: Some(Target {
                                    url: format!(
                                        "http://{relay}/v1/spaces/s/chunks/x?ticket={}",
                                        hex(&chunk.hash)
                                    ),
                                    expires_at_ms: in_an_hour(),
                                    ..Default::default()
                                }),
                                retry_after_ms: 0,
                            },
                        })
                        .collect();
                    server_message::Msg::Rpc(RpcResponse {
                        req_id: request.req_id,
                        result: Some(rpc_response::Result::PrepareUpload(PrepareUploadResult {
                            chunks,
                        })),
                    })
                }
                other => panic!("unexpected message {other:?}"),
            };
            ws.send(send(reply)).await.unwrap();
        }
    });
    (session, events, api)
}
