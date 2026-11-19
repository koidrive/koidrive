// SPDX-License-Identifier: Apache-2.0

const SPACE: &str = "0190f0a0-0000-7000-8000-000000000001";

fn credential(device_id: &str) -> Credential {
    Credential {
        server: "https://files.example.com/".into(),
        user_id: "u1".into(),
        user_name: None,
        device_id: device_id.into(),
        device_name: "vm".into(),
        refresh_credential: "secret".into(),
    }
}

fn release_driver(paths: Paths) -> Driver {
    let mut driver = Driver::open(paths).unwrap();
    let inner = Arc::get_mut(&mut driver.inner).unwrap();
    let config = Config::load(&inner.paths.config_file).unwrap_or_default();
    let variable = std::env::var(update::VARIABLE).ok();
    inner.updates = Arc::new(Updates::for_version(
        update::switched_on(&config.updates, variable.as_deref()),
        inner.paths.update_check(),
        semver::Version::parse("0.1.0").unwrap(),
    ));
    driver
}

fn profile(questions: &[Question]) -> (tempfile::TempDir, Paths, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    let root = base.join("Koidrive");
    fs::create_dir_all(&root).unwrap();
    let store_path = paths.store(SPACE).unwrap();
    fs::create_dir_all(store_path.parent().unwrap()).unwrap();
    let mut store = Store::open(&store_path).unwrap();
    store
        .transaction(|tx| {
            tx.set_link(&Link {
                space_id: SPACE.into(),
                space_name: "Projects".into(),
                root: root.clone(),
                root_dev: 1,
                root_volume: None,
                server_url: "https://files.example.com/".into(),
                device_id: "d1".into(),
                linked_at_ms: 1,
                found_marker: false,
            })?;
            for question in questions {
                tx.put_question(&StoredQuestion {
                    question_id: question.id.clone(),
                    asked_at_ms: question.asked_at_ms,
                    body: serde_json::to_vec(question).unwrap(),
                    answer: None,
                })?;
            }
            Ok::<_, koidrive_store::Error>(())
        })
        .unwrap();
    (dir, paths, root)
}

fn mass_delete(id: &str) -> Question {
    Question {
        recovery_cause: None,
        id: id.into(),
        lineage: id.into(),
        space_id: SPACE.into(),
        asked_at_ms: 10,
        kind: QuestionKind::MassDelete {
            deletions: 1_001,
            affected_files: 1_001,
            space_files: 5_000,
            exceeded: vec![crate::question::Limit::Deletions],
        },
        items: vec![HeldItem {
            node_id: "n1".into(),
            path: "old".into(),
            kind: None,
            files: 1_001,
            device_id: None,
            device_name: None,
            size: None,
            modified_at_ms: None,
            restore: None,
        }],
    }
}

#[test]
fn one_process_at_a_time_opens_a_profile() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let driver = Driver::open(paths.clone()).unwrap();
    assert!(matches!(
        Driver::open(paths.clone()),
        Err(DriverError::Busy(_))
    ));
    drop(driver);
    Driver::open(paths).unwrap();
}

fn unix_ms_now() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn nowhere() -> &'static str {
    static URL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    URL.get_or_init(|| format!("http://{}/", koidrive_test_support::reset_tcp_address()))
}

fn paired(paths: &Paths, device_id: &str) {
    Credential {
        server: nowhere().into(),
        ..credential(device_id)
    }
    .save_new(&paths.credential())
    .unwrap();
}

fn linked(paths: &Paths, root: &Path, device_id: &str) {
    paired(paths, device_id);
    linked_to(paths, root, device_id, SPACE);
}

fn linked_to(paths: &Paths, root: &Path, device_id: &str, space_id: &str) {
    fs::create_dir_all(root).unwrap();
    write_marker(
        root,
        &Marker {
            space_id: space_id.into(),
            effects: 0,
            token: None,
        },
    )
    .unwrap();
    let store_path = paths.store(space_id).unwrap();
    fs::create_dir_all(store_path.parent().unwrap()).unwrap();
    let link = Link {
        space_id: space_id.into(),
        space_name: "Projects".into(),
        root: root.to_path_buf(),
        root_dev: fs::metadata(root).unwrap().dev(),
        root_volume: None,
        server_url: nowhere().into(),
        device_id: device_id.into(),
        linked_at_ms: 1,
        found_marker: false,
    };
    Store::open(&store_path)
        .unwrap()
        .transaction(|tx| tx.set_link(&link))
        .unwrap();
}

/// Waits until `done`. No deadline fails it, since a loaded machine can take any time: a wait that never ends
/// names `what` each minute, and the CI job's timeout stops it.
async fn eventually(what: &str, done: impl Fn() -> bool) {
    let mut waited = Instant::now();
    while !done() {
        if waited.elapsed() >= Duration::from_secs(60) {
            eprintln!("still waiting: {what}");
            waited = Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn halted(driver: &Driver) -> String {
    eventually("the folder halts", || {
        driver.status().folders[0].state == FolderState::Halted
    })
    .await;
    driver.status().folders[0].problem.clone().unwrap()
}

#[tokio::test]
async fn the_folders_of_a_profile_share_one_limit_on_uploads_in_flight() {
    use koidrive_proto::v1::ChunkRef;

    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    paired(&paths, "d1");
    linked_to(&paths, &base.join("A"), "d1", SPACE);
    linked_to(
        &paths,
        &base.join("B"),
        "d1",
        "0190f0a0-0000-7000-8000-000000000002",
    );
    fs::create_dir_all(paths.config_file.parent().unwrap()).unwrap();
    fs::write(&paths.config_file, "[upload]\nconcurrency = 3\n").unwrap();
    let driver = Driver::open(paths).unwrap();
    let credential = driver.credential().unwrap();
    let tasks: Vec<_> = (driver.inner.folders.lock().unwrap().values())
        .map(|folder| driver.task(folder, &credential, Vec::new()).unwrap())
        .collect();
    assert_eq!(tasks.len(), 2);

    let (relay, seen) = crate::scripted::relay(3).await;
    let uploads = tasks.iter().enumerate().map(|(folder, task)| async move {
        let (session, _events, _) = crate::scripted::session(relay).await;
        let chunks: Vec<_> = (0..6)
            .map(|i| {
                let mut bytes = format!("chunk {i} of folder {folder}").into_bytes();
                bytes.resize(koidrive_net::FULL_CHUNK_BYTES as usize - 64, 0);
                let chunk = ChunkRef {
                    hash: koidrive_chunk::chunk_hash(&bytes).to_vec(),
                    size: bytes.len() as u32,
                    ..Default::default()
                };
                (chunk, bytes::Bytes::from(bytes))
            })
            .collect();
        let refs: Vec<_> = chunks.iter().map(|(chunk, _)| chunk.clone()).collect();
        let read = |wanted: &ChunkRef| {
            let found = chunks.iter().find(|(chunk, _)| chunk == wanted).unwrap();
            std::future::ready(Ok(found.1.clone()))
        };
        task.transfer().upload(&session, &refs, read).await.unwrap()
    });
    let receipts = futures_util::future::join_all(uploads).await;

    assert_eq!(receipts.iter().map(Vec::len).sum::<usize>(), 12);
    assert_eq!(seen.lock().unwrap().peak, 3);
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> (String, Vec<u8>) {
    use tokio::io::AsyncReadExt;

    let mut request = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = socket.read(&mut chunk).await.unwrap();
        request.extend_from_slice(&chunk[..n]);
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
        .unwrap_or(0);
    while request.len() < head_end + length {
        let n = socket.read(&mut chunk).await.unwrap();
        request.extend_from_slice(&chunk[..n]);
    }
    let path = head.split_whitespace().nth(1).unwrap().to_owned();
    (path, request[head_end..head_end + length].to_vec())
}

async fn write_reply(socket: &mut tokio::net::TcpStream, reply: &[u8]) {
    use tokio::io::AsyncWriteExt;

    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        reply.len()
    );
    socket.write_all(response.as_bytes()).await.unwrap();
    socket.write_all(reply).await.unwrap();
}

async fn serve_once(reply: Vec<u8>) -> (String, tokio::task::JoinHandle<(String, Vec<u8>)>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = format!("http://{}/", listener.local_addr().unwrap());
    let served = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        write_reply(&mut socket, &reply).await;
        request
    });
    (server, served)
}

struct Held {
    arrived: tokio::sync::oneshot::Receiver<()>,
    release: tokio::sync::oneshot::Sender<()>,
    served: tokio::task::JoinHandle<Vec<String>>,
}

async fn serve_held(replies: Vec<Vec<u8>>) -> (String, Held) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = format!("http://{}/", listener.local_addr().unwrap());
    let (arrived_tx, arrived) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let served = tokio::spawn(async move {
        let mut paths = Vec::new();
        let mut arrived_tx = Some(arrived_tx);
        let mut released = Some(released);
        let last = replies.len() - 1;
        for (i, reply) in replies.into_iter().enumerate() {
            let (mut socket, _) = listener.accept().await.unwrap();
            paths.push(read_request(&mut socket).await.0);
            if i == last {
                arrived_tx.take().unwrap().send(()).unwrap();
                released.take().unwrap().await.unwrap();
            }
            write_reply(&mut socket, &reply).await;
        }
        paths
    });
    (
        server,
        Held {
            arrived,
            release,
            served,
        },
    )
}

async fn still_waiting<T>(task: &tokio::task::JoinHandle<T>) -> bool {
    tokio::time::sleep(Duration::from_millis(200)).await;
    !task.is_finished()
}

/// Sets the mode of `dir` and tells whether it keeps this process out: it does not for root. The tests that rely
/// on it skip when it does not.
fn close_dir(dir: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(dir, fs::Permissions::from_mode(0o555)).unwrap();
    let probe = dir.join("probe");
    if fs::write(&probe, b"").is_err() {
        return true;
    }
    fs::remove_file(probe).unwrap();
    open_dir(dir);
    assert!(rustix::process::geteuid().is_root());
    eprintln!("SKIPPED: running as root, so permissions do not block writes");
    false
}

fn open_dir(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
}

fn issued(device_id: &str) -> Vec<u8> {
    prost::Message::encode_to_vec(&DeviceCredentials {
        user_id: "u1".into(),
        device_id: device_id.into(),
        refresh_credential: "secret".into(),
        ..Default::default()
    })
}

#[tokio::test]
async fn claiming_creates_the_first_device_with_its_user_name() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let driver = Driver::open(paths.clone()).unwrap();
    assert!(matches!(
        driver
            .claim("http://127.0.0.1:9/", "code", " ", "laptop")
            .await,
        Err(DriverError::EmptyUserName)
    ));

    let (server, served) = serve_once(issued("d1")).await;
    let (account, _) = driver
        .claim(&server, "claim-code", "Ada", "laptop")
        .await
        .unwrap();
    assert_eq!(account.user_name.as_deref(), Some("Ada"));
    assert_eq!(account.device_id, "d1");
    let (path, body) = served.await.unwrap();
    assert_eq!(path, "/v1/claim");
    let request: ClaimRequest = prost::Message::decode(body.as_slice()).unwrap();
    assert_eq!(
        (
            request.code.as_str(),
            request.user_name.as_str(),
            request.device_name.as_str()
        ),
        ("claim-code", "Ada", "laptop")
    );
    assert!(matches!(
        driver.claim(&server, "claim-code", "Ada", "laptop").await,
        Err(DriverError::AlreadyPaired { .. })
    ));
    drop(driver);
    let reopened = Driver::open(paths).unwrap();
    assert_eq!(reopened.status().account, Some(account));
}

async fn serve_each(
    replies: Vec<(u16, Vec<u8>)>,
    credential: PathBuf,
) -> (String, tokio::task::JoinHandle<Vec<(String, bool)>>) {
    use tokio::io::AsyncWriteExt;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = format!("http://{}/", listener.local_addr().unwrap());
    let served = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (status, body) in replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (path, _) = read_request(&mut socket).await;
            requests.push((path, credential.exists()));
            let head = format!(
                "HTTP/1.1 {status} Reply\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(&body).await.unwrap();
        }
        requests
    });
    (server, served)
}

fn access_token() -> (u16, Vec<u8>) {
    let token = koidrive_proto::v1::AccessToken {
        token: "token".into(),
        expires_at_ms: i64::MAX,
        ..Default::default()
    };
    (200, prost::Message::encode_to_vec(&token))
}

fn last_device_refused() -> (u16, Vec<u8>) {
    let refusal = koidrive_proto::v1::Error {
        code: koidrive_proto::v1::error::Code::ConfirmationRequired.into(),
        message: "this is the user's last active device; confirm to revoke it".into(),
        ..Default::default()
    };
    (409, prost::Message::encode_to_vec(&refusal))
}

fn linked_with_server(base: &Path, server: String) -> Paths {
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &base.join("Koidrive"), "d1");
    fs::remove_file(paths.credential()).unwrap();
    Credential {
        server,
        ..credential("d1")
    }
    .save_new(&paths.credential())
    .unwrap();
    paths
}

#[tokio::test]
async fn a_refused_last_device_keeps_its_credential_and_folders_until_it_confirms() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let credential_path = Paths::under(&base.join("profile")).credential();
    let (server, served) = serve_each(
        vec![access_token(), last_device_refused(), (204, Vec::new())],
        credential_path,
    )
    .await;
    let paths = linked_with_server(&base, server);
    let driver = Driver::open(paths.clone()).unwrap();
    driver.start();

    assert!(matches!(
        driver.sign_out(false).await,
        Err(DriverError::LastDevice)
    ));
    let status = driver.status();
    assert_eq!(status.account.unwrap().device_id, "d1");
    assert_eq!(status.folders.len(), 1);
    assert!(status.running);
    assert!(paths.credential().exists());

    assert_eq!(driver.sign_out(true).await.unwrap().unconfirmed, None);
    let status = driver.status();
    assert_eq!(status.account, None);
    assert!(status.folders.is_empty());
    assert!(!paths.credential().exists());
    assert_eq!(
        served.await.unwrap(),
        [
            ("/v1/token".to_owned(), true),
            ("/v1/devices/d1".to_owned(), true),
            ("/v1/devices/d1?confirm=last-device".to_owned(), true),
        ]
    );
}

async fn serve_calling(
    replies: Vec<(u16, Vec<u8>)>,
    at: usize,
    call: impl FnOnce() + Send + 'static,
) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    use tokio::io::AsyncWriteExt;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = format!("http://{}/", listener.local_addr().unwrap());
    let served = tokio::spawn(async move {
        let mut call = Some(call);
        let mut requests = Vec::new();
        for (i, (status, body)) in replies.into_iter().enumerate() {
            let (mut socket, _) = listener.accept().await.unwrap();
            requests.push(read_request(&mut socket).await.0);
            if i == at {
                call.take().unwrap()();
            }
            let head = format!(
                "HTTP/1.1 {status} Reply\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(&body).await.unwrap();
        }
        requests
    });
    (server, served)
}

async fn paused_driver(paths: &Paths, root: &Path) -> (Driver, Gate) {
    linked(paths, root, "d1");
    fs::write(root.join("file"), b"mine").unwrap();
    let mut driver = Driver::open(paths.clone()).unwrap();
    let (entered, observing) = tokio::sync::oneshot::channel();
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    Arc::get_mut(&mut driver.inner)
        .unwrap()
        .rules
        .get_mut()
        .unwrap()
        .matcher = Arc::new(Paused {
        entered: Mutex::new(Some(entered)),
        gate: gate.clone(),
    });
    driver.start();
    observing.await.unwrap();
    (driver, Gate(gate))
}

fn lockable(root: &Path) -> bool {
    claim::lock(root).unwrap().is_some()
}

fn signed_out(driver: &Driver, paths: &Paths, root: &Path) {
    let status = driver.status();
    assert_eq!(status.account, None);
    assert!(status.folders.is_empty());
    assert!(!status.running);
    assert!(!paths.credential().exists());
    assert!(!paths.store(SPACE).unwrap().parent().unwrap().exists());
    assert_eq!(claim::owner(root).unwrap(), None);
    assert!(lockable(root));
    assert_eq!(fs::read(root.join("file")).unwrap(), b"mine");
}

#[tokio::test]
async fn signing_out_waits_for_a_stop_already_in_progress() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let root = base.join("Koidrive");
    let paths = Paths::under(&base.join("profile"));
    let (driver, gate) = paused_driver(&paths, &root).await;
    let stop = tokio::spawn({
        let driver = driver.clone();
        async move { driver.stop().await }
    });
    eventually("the stop begins", || !driver.status().running).await;
    let sign_out = tokio::spawn({
        let driver = driver.clone();
        async move { driver.sign_out(true).await }
    });
    assert!(still_waiting(&sign_out).await);
    assert!(!lockable(&root));

    gate.open();
    stop.await.unwrap();
    sign_out.await.unwrap().unwrap();
    signed_out(&driver, &paths, &root);
}

fn uploading(driver: &Driver, size: u64) -> Arc<crate::lifecycle::Lifecycle> {
    let folders = driver.inner.folders.lock().unwrap();
    let folder = folders.values().next().unwrap();
    let item = crate::progress::Item {
        direction: crate::progress::Direction::Upload,
        node: "a".into(),
        digest: b"a".to_vec(),
    };
    (folder.progress.lock().unwrap()).track(BTreeMap::from([(item, size)]), |_| false);
    folder.status.clone()
}
