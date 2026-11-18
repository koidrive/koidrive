// SPDX-License-Identifier: Apache-2.0

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
