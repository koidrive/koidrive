// SPDX-License-Identifier: Apache-2.0

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use koidrive_proto::v1::error::Code;
use koidrive_proto::v1::token_request::Telemetry;
use koidrive_proto::v1::{AccessToken, TokenRequest};
use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

use super::*;

enum Plan {
    Answer,
    LoseAnswer,
    Drop,
    Hold(oneshot::Receiver<()>),
    Delay(oneshot::Receiver<()>),
}

struct Server {
    current: String,
    clock_ms: i64,
    rotation_clock_ms: i64,
    supports_rotation: bool,
    plans: VecDeque<Plan>,
    seen: Vec<TokenRequest>,
    held: usize,
}

impl Server {
    fn apply(&mut self, request: &TokenRequest) -> Result<AccessToken, ()> {
        if request.refresh_credential != self.current {
            return Err(());
        }
        let mut token = AccessToken {
            token: format!("token-{}", self.seen.len()),
            expires_at_ms: now_ms() + 15 * 60_000,
            ..Default::default()
        };
        if self.supports_rotation && request.rotation {
            let time = self.clock_ms.max(self.rotation_clock_ms);
            self.rotation_clock_ms = time;
            token.server_time_ms = time;
            if !request.next_refresh_credential.is_empty() && time <= request.rotate_before_ms {
                self.current = request.next_refresh_credential.clone();
                token.rotated = true;
            }
        }
        Ok(token)
    }
}

#[derive(Clone)]
struct Stub(Arc<Mutex<Server>>);

impl Stub {
    async fn start(current: &str) -> (Stub, Api) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let stub = Stub(Arc::new(Mutex::new(Server {
            current: current.to_owned(),
            clock_ms: now_ms(),
            rotation_clock_ms: 0,
            supports_rotation: true,
            plans: VecDeque::new(),
            seen: Vec::new(),
            held: 0,
        })));
        let serving = stub.clone();
        tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                tokio::spawn(serving.clone().serve(socket));
            }
        });
        (stub, Api::local_development(base).unwrap())
    }

    fn server(&self) -> std::sync::MutexGuard<'_, Server> {
        self.0.lock().unwrap()
    }

    fn plan(&self, plan: Plan) {
        self.server().plans.push_back(plan);
    }

    async fn serve(self, mut socket: TcpStream) {
        let request = read_token_request(&mut socket).await;
        let plan = {
            let mut server = self.server();
            server.seen.push(request.clone());
            server.plans.pop_front().unwrap_or(Plan::Answer)
        };
        let answer = match plan {
            Plan::Answer => self.server().apply(&request),
            Plan::LoseAnswer => {
                let _ = self.server().apply(&request);
                return;
            }
            Plan::Drop => return,
            Plan::Hold(release) => {
                self.server().held += 1;
                let _ = release.await;
                self.server().apply(&request)
            }
            Plan::Delay(release) => {
                drop(socket);
                let _ = release.await;
                let _ = self.server().apply(&request);
                return;
            }
        };
        let (status, body) = match answer {
            Ok(token) => (200, token.encode_to_vec()),
            Err(()) => (
                401,
                koidrive_proto::v1::Error {
                    code: Code::Unauthenticated as i32,
                    message: "unknown device or refresh credential".into(),
                    ..Default::default()
                }
                .encode_to_vec(),
            ),
        };
        let head = format!(
            "HTTP/1.1 {status} X\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        let _ = socket.write_all(&[head.as_bytes(), &body].concat()).await;
    }
}

async fn read_token_request(socket: &mut TcpStream) -> TokenRequest {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = socket.read(&mut chunk).await.unwrap();
        buffer.extend_from_slice(&chunk[..n]);
        let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&buffer[..end]).to_ascii_lowercase();
        let length: usize = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .map_or(0, |value| value.trim().parse().unwrap());
        if buffer.len() >= end + 4 + length {
            return TokenRequest::decode(&buffer[end + 4..end + 4 + length]).unwrap();
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[derive(Default)]
struct Slots {
    staged: Option<Staged>,
    promoted: Vec<String>,
    fail_stage: bool,
    failing_promotions: usize,
}

#[derive(Default)]
struct MemoryStore(Mutex<Slots>);

impl CredentialStore for MemoryStore {
    fn stage(&self, staged: &Staged) -> io::Result<()> {
        let mut slots = self.0.lock().unwrap();
        if slots.fail_stage {
            return Err(io::Error::other("injected stage failure"));
        }
        slots.staged = Some(staged.clone());
        Ok(())
    }

    fn promote(&self, staged: &Staged) -> io::Result<()> {
        let mut slots = self.0.lock().unwrap();
        if slots.failing_promotions > 0 {
            slots.failing_promotions -= 1;
            return Err(io::Error::other("injected promotion failure"));
        }
        slots.promoted.push(staged.refresh_credential.clone());
        slots.staged = None;
        Ok(())
    }

    fn clear_staged(&self) -> io::Result<()> {
        self.0.lock().unwrap().staged = None;
        Ok(())
    }
}

impl MemoryStore {
    fn staged(&self) -> Option<Staged> {
        self.0.lock().unwrap().staged.clone()
    }
}

fn rotating(api: Api, store: &Arc<MemoryStore>) -> Auth {
    let stored = Stored {
        current: Credentials {
            user_id: "u".into(),
            device_id: "d".into(),
            refresh_credential: "old".into(),
        },
        staged: None,
        changed_at: UNIX_EPOCH,
    };
    Auth::rotating(api, stored, store.clone(), ROTATION_PERIOD)
        .with_refresh_margin(Duration::from_secs(3600))
}

fn settled(auth: &Auth, stub: &Stub, store: &MemoryStore) {
    assert_eq!(auth.credentials().refresh_credential, stub.server().current);
    assert_eq!(store.staged(), None);
}

fn proposals(stub: &Stub) -> Vec<String> {
    stub.server()
        .seen
        .iter()
        .map(|request| request.next_refresh_credential.clone())
        .filter(|proposal| !proposal.is_empty())
        .collect()
}

#[tokio::test]
async fn a_failed_promotion_is_stored_before_the_next_rotation_and_survives_a_lost_proposal_and_a_restart()
 {
    struct FailingStore {
        stored: Mutex<Stored>,
        fail_once: std::sync::atomic::AtomicBool,
    }
    impl CredentialStore for FailingStore {
        fn stage(&self, staged: &Staged) -> io::Result<()> {
            self.stored.lock().unwrap().staged = Some(staged.clone());
            Ok(())
        }
        fn promote(&self, staged: &Staged) -> io::Result<()> {
            if self
                .fail_once
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                return Err(io::Error::other("injected current-file write failure"));
            }
            let mut stored = self.stored.lock().unwrap();
            stored.current.refresh_credential = staged.refresh_credential.clone();
            stored.staged = None;
            stored.changed_at = SystemTime::now();
            Ok(())
        }
        fn clear_staged(&self) -> io::Result<()> {
            self.stored.lock().unwrap().staged = None;
            Ok(())
        }
    }
    let (stub, api) = Stub::start("old").await;
    let initial = Stored {
        current: Credentials {
            user_id: "u".into(),
            device_id: "d".into(),
            refresh_credential: "old".into(),
        },
        staged: None,
        changed_at: UNIX_EPOCH,
    };
    let store = Arc::new(FailingStore {
        stored: Mutex::new(initial.clone()),
        fail_once: std::sync::atomic::AtomicBool::new(true),
    });
    let auth = Auth::rotating(api.clone(), initial, store.clone(), ROTATION_PERIOD)
        .with_refresh_margin(Duration::from_secs(3600));
    auth.access_token().await.unwrap();
    assert!(store.stored.lock().unwrap().staged.is_some());
    auth.state.lock().await.stored.changed_at = UNIX_EPOCH;
    stub.plan(Plan::Answer);
    stub.plan(Plan::Drop);
    auth.access_token().await.unwrap();
    drop(auth);
    let disk = store.stored.lock().unwrap().clone();
    let restarted = Auth::rotating(api, disk, store, ROTATION_PERIOD)
        .with_refresh_margin(Duration::from_secs(3600));
    assert!(
        restarted.access_token().await.is_ok(),
        "the durable slots must retain the credential the server accepts"
    );
}

#[tokio::test]
async fn no_credential_is_staged_while_the_promotion_of_the_last_one_keeps_failing() {
    let (stub, api) = Stub::start("old").await;
    let store = Arc::new(MemoryStore::default());
    store.0.lock().unwrap().failing_promotions = 2;
    let auth = rotating(api, &store);
    auth.access_token().await.unwrap();
    let first = stub.server().current.clone();
    assert_ne!(first, "old");
    auth.state.lock().await.stored.changed_at = UNIX_EPOCH;
    auth.access_token().await.unwrap();
    assert_eq!(proposals(&stub), std::slice::from_ref(&first));
    assert_eq!(
        store.staged().map(|staged| staged.refresh_credential),
        Some(first.clone()),
        "the credential the server holds stays in the staged slot"
    );
    auth.access_token().await.unwrap();
    let second = stub.server().current.clone();
    assert_ne!(second, first);
    assert_eq!(store.0.lock().unwrap().promoted, [first, second]);
    settled(&auth, &stub, &store);
}

#[tokio::test]
async fn a_credential_older_than_the_period_is_replaced_and_the_store_promotes_it() {
    let (stub, api) = Stub::start("old").await;
    let store = Arc::new(MemoryStore::default());
    let auth = rotating(api, &store);
    auth.access_token().await.unwrap();
    let proposed = proposals(&stub);
    assert_eq!(proposed.len(), 1);
    assert_eq!(proposed[0].len(), 43);
    assert_eq!(stub.server().current, proposed[0]);
    assert_eq!(store.0.lock().unwrap().promoted, proposed);
    assert!(stub.server().seen.iter().all(|request| request.rotation));
    settled(&auth, &stub, &store);

    auth.access_token().await.unwrap();
    assert_eq!(
        proposals(&stub).len(),
        1,
        "a new credential is not rotated again"
    );
}

#[tokio::test]
async fn a_failed_stage_write_keeps_the_credential_and_rotates_at_a_later_refresh() {
    let (stub, api) = Stub::start("old").await;
    let store = Arc::new(MemoryStore::default());
    store.0.lock().unwrap().fail_stage = true;
    let auth = rotating(api, &store);
    auth.access_token().await.unwrap();
    assert!(proposals(&stub).is_empty());
    assert_eq!(auth.credentials().refresh_credential, "old");

    store.0.lock().unwrap().fail_stage = false;
    auth.access_token().await.unwrap();
    assert_eq!(proposals(&stub).len(), 1);
    settled(&auth, &stub, &store);
}

#[tokio::test]
async fn a_lost_answer_keeps_both_slots_and_the_next_refresh_moves_to_the_staged_credential() {
    let (stub, api) = Stub::start("old").await;
    let store = Arc::new(MemoryStore::default());
    let auth = rotating(api, &store);
    stub.plan(Plan::Answer);
    stub.plan(Plan::LoseAnswer);
    auth.access_token().await.unwrap();
    let staged = store
        .staged()
        .expect("the outcome is unknown, so the staged credential stays");
    assert_eq!(auth.credentials().refresh_credential, "old");
    assert_eq!(stub.server().current, staged.refresh_credential);

    auth.access_token().await.unwrap();
    settled(&auth, &stub, &store);
    let last = stub.server().seen.last().unwrap().clone();
    assert_eq!(last.refresh_credential, staged.refresh_credential);
    assert!(last.next_refresh_credential.is_empty());
}

#[tokio::test]
async fn a_proposal_dropped_before_the_server_is_proposed_again_and_stored_after_the_clock_moves_back()
 {
    let (stub, api) = Stub::start("old").await;
    let store = Arc::new(MemoryStore::default());
    let auth = rotating(api, &store);
    stub.plan(Plan::Answer);
    stub.plan(Plan::Drop);
    auth.access_token().await.unwrap();
    let staged = store.staged().unwrap();
    assert_eq!(stub.server().current, "old");

    stub.server().clock_ms -= 10 * 60_000;
    auth.access_token().await.unwrap();
    assert_eq!(
        proposals(&stub),
        [
            staged.refresh_credential.clone(),
            staged.refresh_credential.clone()
        ]
    );
    assert_eq!(
        stub.server().seen.last().unwrap().rotate_before_ms,
        staged.rotate_before_ms
    );
    settled(&auth, &stub, &store);
}

#[tokio::test]
async fn a_delayed_proposal_and_its_retry_store_the_credential_whichever_is_handled_first() {
    for retry_first in [true, false] {
        let (stub, api) = Stub::start("old").await;
        let store = Arc::new(MemoryStore::default());
        let auth = rotating(api, &store);
        let (release, delayed) = oneshot::channel();
        stub.plan(Plan::Answer);
        stub.plan(Plan::Delay(delayed));
        auth.access_token().await.unwrap();
        let staged = store.staged().unwrap();

        if retry_first {
            auth.access_token().await.unwrap();
            release.send(()).unwrap();
        } else {
            release.send(()).unwrap();
            while stub.server().current == "old" {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            auth.access_token().await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            stub.server().current,
            staged.refresh_credential,
            "retry first: {retry_first}"
        );
        settled(&auth, &stub, &store);
    }
}

#[tokio::test]
async fn a_proposal_held_past_its_deadline_is_answered_without_rotated_and_the_staged_slot_cleared()
{
    let (stub, api) = Stub::start("old").await;
    let store = Arc::new(MemoryStore::default());
    let auth = Arc::new(rotating(api, &store));
    let (release, held) = oneshot::channel();
    stub.plan(Plan::Answer);
    stub.plan(Plan::Hold(held));
    let refreshing = tokio::spawn({
        let auth = auth.clone();
        async move { auth.access_token().await }
    });
    while stub.server().held == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let staged = store.staged().unwrap();
    stub.server().clock_ms = staged.rotate_before_ms + 1;
    release.send(()).unwrap();
    let token = refreshing.await.unwrap().unwrap();
    assert!(!token.rotated);
    assert_eq!(stub.server().current, "old");
    settled(&auth, &stub, &store);
}

#[tokio::test]
async fn a_server_without_rotation_is_never_sent_a_proposal() {
    let (stub, api) = Stub::start("old").await;
    stub.server().supports_rotation = false;
    let store = Arc::new(MemoryStore::default());
    let auth = rotating(api, &store);
    auth.access_token().await.unwrap();
    auth.access_token().await.unwrap();
    assert!(proposals(&stub).is_empty());
    settled(&auth, &stub, &store);
}

#[tokio::test]
async fn a_reader_that_is_refused_presents_the_reloaded_current_and_then_staged_credential() {
    let (stub, api) = Stub::start("rotated").await;
    let reloads = Arc::new(Mutex::new(0));
    let reload: Reload = {
        let reloads = reloads.clone();
        Arc::new(move || {
            *reloads.lock().unwrap() += 1;
            Ok(Stored {
                current: Credentials {
                    user_id: "u".into(),
                    device_id: "d".into(),
                    refresh_credential: "current".into(),
                },
                staged: Some(Staged {
                    refresh_credential: "rotated".into(),
                    rotate_before_ms: 1,
                }),
                changed_at: UNIX_EPOCH,
            })
        })
    };
    let credentials = Credentials {
        user_id: "u".into(),
        device_id: "d".into(),
        refresh_credential: "held".into(),
    };
    let auth =
        Auth::reloading(api, credentials, reload).with_refresh_margin(Duration::from_secs(3600));
    auth.access_token().await.unwrap();
    let presented: Vec<_> = stub
        .server()
        .seen
        .iter()
        .map(|request| request.refresh_credential.clone())
        .collect();
    assert_eq!(presented, ["held", "current", "rotated"]);
    assert!(
        stub.server()
            .seen
            .iter()
            .all(|request| !request.rotation && request.next_refresh_credential.is_empty())
    );
    assert_eq!(auth.credentials().refresh_credential, "rotated");
    assert_eq!(*reloads.lock().unwrap(), 1);

    stub.server().current = "elsewhere".into();
    let refused = auth.access_token().await;
    assert!(
        refused.as_ref().is_err_and(ApiError::is_unauthenticated),
        "{refused:?}"
    );
}

#[tokio::test]
async fn each_refresh_carries_the_usage_report_choice_set_before_it() {
    let (stub, api) = Stub::start("old").await;
    let credentials = Credentials {
        user_id: "u".into(),
        device_id: "d".into(),
        refresh_credential: "old".into(),
    };
    let auth = Auth::new(api, credentials).with_refresh_margin(Duration::from_secs(20 * 60));
    auth.access_token().await.unwrap();
    crate::set_telemetry(Telemetry::On);
    auth.access_token().await.unwrap();
    crate::set_telemetry(Telemetry::Off);
    auth.access_token().await.unwrap();
    let sent: Vec<Telemetry> = (stub.server().seen.iter())
        .map(|request| request.telemetry())
        .collect();
    assert_eq!(
        sent,
        [Telemetry::Unspecified, Telemetry::On, Telemetry::Off]
    );
}
