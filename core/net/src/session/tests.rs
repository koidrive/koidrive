// SPDX-License-Identifier: Apache-2.0

use std::time::{Duration, UNIX_EPOCH};

use koidrive_proto::v1::{
    CaughtUp, Changes, CommitRecord, Error as ProtoError, Goodbye, Notice, Resume, ServerMessage,
    TreePage, client_message, server_message,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::WebSocketStream;

use super::*;
use crate::api::PROTOBUF;
use crate::{Api, Credentials};
use koidrive_proto::v1::error::Code;

type Server = WebSocketStream<TcpStream>;

pub(crate) async fn listen() -> (TcpListener, Arc<Auth>) {
    listen_on("127.0.0.1:0", Duration::from_secs(3600), None).await
}

async fn listen_on(
    address: &str,
    lifetime: Duration,
    refresh_margin: Option<Duration>,
) -> (TcpListener, Arc<Auth>) {
    let listener = TcpListener::bind(address).await.unwrap();
    let base: Url = format!("http://{}", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let credentials = Credentials {
        user_id: "u".into(),
        device_id: "d".into(),
        refresh_credential: "r".into(),
    };
    let expires = SystemTime::now() + lifetime;
    let token = AccessToken {
        token: "token".into(),
        expires_at_ms: expires.duration_since(UNIX_EPOCH).unwrap().as_millis() as i64,
        ..Default::default()
    };
    let mut auth = Auth::new(Api::local_development(base).unwrap(), credentials).with_token(token);
    if let Some(margin) = refresh_margin {
        auth = auth.with_refresh_margin(margin);
    }
    (listener, Arc::new(auth))
}

pub(crate) fn config() -> SessionConfig {
    let mut config = SessionConfig::new("0190a0b0-0000-7000-8000-000000000001", "test", "0", 1);
    config.backoff = Backoff {
        initial: Duration::from_millis(10),
        max: Duration::from_millis(50),
    };
    config
}

pub(crate) async fn accept(listener: &TcpListener, welcome: Welcome) -> Server {
    let (tcp, _) = listener.accept().await.unwrap();
    accept_upgrade(tcp, welcome).await
}

async fn accept_upgrade(tcp: TcpStream, welcome: Welcome) -> Server {
    let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
    assert!(matches!(recv(&mut ws).await, client_message::Msg::Hello(_)));
    push(&mut ws, server_message::Msg::Welcome(welcome)).await;
    ws
}

async fn recv(ws: &mut Server) -> client_message::Msg {
    loop {
        match ws.next().await.expect("the client hung up").unwrap() {
            WsMessage::Binary(bytes) => return ClientMessage::decode(bytes).unwrap().msg.unwrap(),
            _ => continue,
        }
    }
}

pub(crate) async fn recv_rpc(ws: &mut Server) -> RpcRequest {
    match recv(ws).await {
        client_message::Msg::Rpc(request) => request,
        other => panic!("expected an RpcRequest, got {other:?}"),
    }
}

async fn push(ws: &mut Server, msg: server_message::Msg) {
    ws.send(WsMessage::Binary(
        ServerMessage { msg: Some(msg) }.encode_to_vec().into(),
    ))
    .await
    .unwrap();
}

pub(crate) async fn reply(ws: &mut Server, req_id: u64, result: rpc_response::Result) {
    push(
        ws,
        server_message::Msg::Rpc(RpcResponse {
            req_id,
            result: Some(result),
        }),
    )
    .await;
}

pub(crate) fn welcome(history_id: &str, device_seq: u64) -> Welcome {
    Welcome {
        proto: 1,
        history_id: history_id.into(),
        device_seq,
        ..Default::default()
    }
}

fn commit(device_seq: u64) -> Commit {
    Commit {
        commit_id: "c1".into(),
        base_seq: 3,
        history_id: "h".into(),
        device_seq,
        ..Default::default()
    }
}

async fn next(events: &mut Events) -> Event {
    tokio::time::timeout(Duration::from_secs(5), events.next())
        .await
        .expect("no event within 5 s")
        .expect("events ended")
}

fn notice_of_size(size: usize) -> Notice {
    let mut notice = Notice {
        message: String::new(),
        ..Default::default()
    };
    let encoded = |n: &Notice| {
        ServerMessage {
            msg: Some(server_message::Msg::Notice(n.clone())),
        }
        .encoded_len()
    };
    notice.message = "x".repeat(size - 16);
    while encoded(&notice) < size {
        notice.message.push('x');
    }
    while encoded(&notice) > size {
        notice.message.pop();
    }
    assert_eq!(encoded(&notice), size);
    notice
}

include!("tests/stream.rs");
include!("tests/refusal.rs");
include!("tests/connection.rs");

mod refusal_reader {
    use proptest::prelude::*;

    use super::*;

    type Framing = Option<(&'static str, String)>;

    fn head(framing: Framing) -> tungstenite::http::response::Parts {
        let mut response = tungstenite::http::Response::builder()
            .status(503)
            .header(CONTENT_TYPE, PROTOBUF);
        if let Some((name, value)) = framing {
            response = response.header(name, value);
        }
        response.body(()).unwrap().into_parts().0
    }

    fn chunked() -> Framing {
        Some(("transfer-encoding", "chunked".into()))
    }

    fn length(length: usize) -> Framing {
        Some(("content-length", length.to_string()))
    }

    fn read(framing: Framing, answer: &[u8], split: usize) -> Option<Bytes> {
        let (tail, rest) = answer.split_at(split.min(answer.len()));
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(refusal_body(&head(framing), tail.to_vec(), rest))
    }

    fn encode_chunked(body: &[u8], sizes: &[usize]) -> Vec<u8> {
        let mut encoded = Vec::new();
        let mut rest = body;
        for size in sizes.iter().cycle() {
            if rest.is_empty() {
                break;
            }
            let (chunk, after) = rest.split_at((*size).min(rest.len()));
            encoded.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
            encoded.extend_from_slice(chunk);
            encoded.extend_from_slice(b"\r\n");
            rest = after;
        }
        encoded.extend_from_slice(b"0\r\n\r\n");
        encoded
    }

    #[test]
    fn a_body_past_the_bound_is_not_read() {
        let body = vec![0; MAX_REFUSAL + 1];
        assert_eq!(read(length(body.len()), &body, 0), None);
        assert_eq!(read(chunked(), &encode_chunked(&body, &[4096]), 0), None);
        assert_eq!(read(None, &body, 0), None);
    }

    proptest! {
        #![proptest_config(koidrive_test_support::battery_config(
            concat!(env!("CARGO_MANIFEST_DIR"), "/proptest-regressions/session/tests.txt"),
            ProptestConfig::default().cases,
        ))]
        #[test]
        fn arbitrary_answers_are_bounded_and_never_panic(
            framing in prop_oneof![Just(None), Just(chunked()), (0usize..100_000).prop_map(length)],
            size_line in "([0-9a-fA-F]{1,20}(;[a-z]{0,3})?\r\n)?",
            bytes in proptest::collection::vec(any::<u8>(), 0..512),
            split in any::<usize>(),
        ) {
            let answer = [size_line.as_bytes(), &bytes].concat();
            if let Some(body) = read(framing, &answer, split) {
                prop_assert!(body.len() <= MAX_REFUSAL);
            }
        }

        #[test]
        fn complete_bodies_are_read_whole_and_cut_ones_are_not(
            body in proptest::collection::vec(any::<u8>(), 0..2048),
            sizes in proptest::collection::vec(1usize..600, 1..8),
            is_chunked in any::<bool>(),
            split in any::<usize>(),
            cut in any::<prop::sample::Index>(),
        ) {
            let (framing, encoded) = if is_chunked {
                (chunked(), encode_chunked(&body, &sizes))
            } else {
                (length(body.len()), body.clone())
            };
            let read_whole = read(framing.clone(), &encoded, split);
            prop_assert_eq!(read_whole.as_deref(), Some(body.as_slice()));
            if !encoded.is_empty() {
                let cut = cut.index(encoded.len());
                prop_assert_eq!(read(framing, &encoded[..cut], split), None);
            }
        }
    }
}
