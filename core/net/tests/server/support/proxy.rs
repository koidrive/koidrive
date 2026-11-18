// SPDX-License-Identifier: Apache-2.0

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use koidrive_proto::v1::{ClientMessage, client_message};
use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use url::Url;

use super::WAIT;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Fault {
    #[default]
    None,
    DropOnClientData,
    DropOnServerData,
}

pub struct Proxy {
    pub url: Url,
    fault: Arc<Mutex<Fault>>,
    reauths: watch::Receiver<usize>,
    corrupt: Arc<Mutex<Corrupt>>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Corrupt {
    pub uploads: u32,
    pub downloads: u32,
}

#[derive(Clone)]
struct Shared {
    upstream: SocketAddr,
    fault: Arc<Mutex<Fault>>,
    reauths: Arc<watch::Sender<usize>>,
    corrupt: Arc<Mutex<Corrupt>>,
}

impl Proxy {
    pub async fn start(upstream: &Url) -> Proxy {
        let upstream: SocketAddr = upstream.socket_addrs(|| None).unwrap()[0];
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let (reauths, seen) = watch::channel(0);
        let shared = Shared {
            upstream,
            fault: Arc::default(),
            reauths: Arc::new(reauths),
            corrupt: Arc::default(),
        };
        let proxy = Proxy {
            url,
            fault: shared.fault.clone(),
            reauths: seen,
            corrupt: shared.corrupt.clone(),
        };
        tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                tokio::spawn(relay(client, shared.clone()));
            }
        });
        proxy
    }

    pub fn arm(&self, fault: Fault) {
        *self.fault.lock().unwrap() = fault;
    }

    pub fn fired(&self) -> bool {
        *self.fault.lock().unwrap() == Fault::None
    }

    pub fn corrupt(&self, corrupt: Corrupt) {
        *self.corrupt.lock().unwrap() = corrupt;
    }

    pub fn corrupt_left(&self) -> Corrupt {
        *self.corrupt.lock().unwrap()
    }

    pub async fn reauthed(&self) {
        let mut reauths = self.reauths.clone();
        tokio::time::timeout(WAIT, reauths.wait_for(|sent| *sent > 0))
            .await
            .expect("no Reauth frame went out")
            .unwrap();
    }
}

async fn relay(mut client: TcpStream, shared: Shared) {
    let Shared {
        upstream,
        fault,
        reauths,
        corrupt,
    } = shared;
    let Ok(mut server) = TcpStream::connect(upstream).await else {
        return;
    };
    let mut stream = ClientStream::Head(Vec::new());
    let (mut damage_upload, mut damage_download) = (false, false);
    let (mut from_client, mut from_server) = ([0u8; 64 * 1024], [0u8; 64 * 1024]);
    let trips = |trigger: Fault| {
        let mut armed = fault.lock().unwrap();
        let fire = *armed == trigger;
        if fire {
            *armed = Fault::None;
        }
        fire
    };
    loop {
        tokio::select! {
            read = client.read(&mut from_client) => match read {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    let sent = stream.reauths_in(&from_client[..n]);
                    if sent > 0 {
                        reauths.send_modify(|count| *count += sent);
                    }
                    let data = &mut from_client[..n];
                    if data.starts_with(b"PUT ") && contains(data, b"/chunks/") {
                        damage_upload = take(&corrupt, |c| &mut c.uploads);
                    }
                    damage_upload = damage_upload && !flip_body(data);
                    if trips(Fault::DropOnClientData) || server.write_all(data).await.is_err() {
                        return;
                    }
                }
            },
            read = server.read(&mut from_server) => match read {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    let data = &mut from_server[..n];
                    if data.starts_with(b"HTTP/1.1 200") && contains(data, b"octet-stream") {
                        damage_download = take(&corrupt, |c| &mut c.downloads);
                    }
                    damage_download = damage_download && !flip_body(data);
                    if trips(Fault::DropOnServerData) || client.write_all(data).await.is_err() {
                        return;
                    }
                }
            },
        }
    }
}

enum ClientStream {
    Head(Vec<u8>),
    WebSocket(Vec<u8>),
    Http,
}

impl ClientStream {
    fn reauths_in(&mut self, data: &[u8]) -> usize {
        match self {
            ClientStream::Head(head) => {
                head.extend_from_slice(data);
                let Some(end) = head.windows(4).position(|w| w == b"\r\n\r\n") else {
                    return 0;
                };
                let frames = head.split_off(end + 4);
                *self = if contains(head, b"/ws HTTP/1.1\r") {
                    ClientStream::WebSocket(frames)
                } else {
                    ClientStream::Http
                };
                self.reauths_in(&[])
            }
            ClientStream::WebSocket(frames) => {
                frames.extend_from_slice(data);
                drain_reauths(frames)
            }
            ClientStream::Http => 0,
        }
    }
}

fn drain_reauths(frames: &mut Vec<u8>) -> usize {
    let mut reauths = 0;
    while let Some((opcode, payload, end)) = client_frame(frames) {
        let message = (opcode == 2).then(|| ClientMessage::decode(payload.as_slice()));
        if let Some(Ok(ClientMessage {
            msg: Some(client_message::Msg::Reauth(_)),
        })) = message
        {
            reauths += 1;
        }
        frames.drain(..end);
    }
    reauths
}

fn client_frame(frames: &[u8]) -> Option<(u8, Vec<u8>, usize)> {
    let [first, second, ..] = *frames else {
        return None;
    };
    let (len, mask_at) = match second & 0x7f {
        126 => (
            usize::from(u16::from_be_bytes(frames.get(2..4)?.try_into().ok()?)),
            4,
        ),
        127 => (
            u64::from_be_bytes(frames.get(2..10)?.try_into().ok()?) as usize,
            10,
        ),
        len => (usize::from(len), 2),
    };
    let mask: [u8; 4] = frames.get(mask_at..mask_at + 4)?.try_into().ok()?;
    let end = mask_at + 4 + len;
    let payload = (frames.get(mask_at + 4..end)?.iter())
        .zip(mask.iter().cycle())
        .map(|(byte, key)| byte ^ key)
        .collect();
    Some((first & 0x0f, payload, end))
}

fn contains(data: &[u8], needle: &[u8]) -> bool {
    data.windows(needle.len()).any(|w| w == needle)
}

fn take(corrupt: &Mutex<Corrupt>, kind: impl FnOnce(&mut Corrupt) -> &mut u32) -> bool {
    let mut corrupt = corrupt.lock().unwrap();
    let left = kind(&mut corrupt);
    let armed = *left > 0;
    *left = left.saturating_sub(1);
    armed
}

fn flip_body(data: &mut [u8]) -> bool {
    let start = match data.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(end) => end + 4,
        None if data.starts_with(b"PUT ") || data.starts_with(b"HTTP/") => return false,
        None => 0,
    };
    match data.get_mut(start) {
        Some(byte) => {
            *byte ^= 0xff;
            true
        }
        None => false,
    }
}

#[test]
fn reauths_are_counted_however_the_client_stream_is_split() {
    let request =
        b"GET /ws HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n";
    let reauth = ClientMessage {
        msg: Some(client_message::Msg::Reauth(koidrive_proto::v1::Reauth {
            access_token: "token".into(),
        })),
    }
    .encode_to_vec();
    let mask = [7, 1, 9, 4];
    let mut stream = request.to_vec();
    stream.extend([0x82, 0x80 | u8::try_from(reauth.len()).unwrap()]);
    stream.extend(mask);
    stream.extend(
        reauth
            .iter()
            .zip(mask.iter().cycle())
            .map(|(byte, key)| byte ^ key),
    );
    for split in 1..stream.len() {
        let mut client = ClientStream::Head(Vec::new());
        let (head, rest) = stream.split_at(split);
        assert_eq!(
            client.reauths_in(head) + client.reauths_in(rest),
            1,
            "split at {split}"
        );
    }
    let mut http = ClientStream::Head(Vec::new());
    assert_eq!(
        http.reauths_in(
            &[
                b"GET /v1/tree HTTP/1.1\r\n\r\n".as_slice(),
                &stream[request.len()..]
            ]
            .concat()
        ),
        0
    );
}
