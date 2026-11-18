// SPDX-License-Identifier: Apache-2.0

#[tokio::test]
async fn an_upload_target_that_expires_during_its_batch_is_prepared_again_before_the_chunk_is_read()
{
    let (a_bytes, b_bytes) = (full("chunk a"), full("chunk b"));
    let (a, b) = (chunk(&a_bytes), chunk(&b_bytes));
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
            let (bytes, delay) = if chunk.hash == b.hash {
                (b_bytes.clone(), Duration::from_millis(1500))
            } else {
                (a_bytes.clone(), Duration::ZERO)
            };
            async move {
                tokio::time::sleep(delay).await;
                Ok(bytes)
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
        (
            429,
            "Application/X-Protobuf; charset=binary",
            error(Code::QuotaExceeded as i32, 100),
        ),
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
        matches!(
            put().await,
            Attempt::Retry(ChunkError::Refused {
                error: Some(ref server),
                ..
            }) if server.0.retry_after_ms == 100
        ),
        "media types match without case and with parameters"
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

fn full(label: &str) -> Bytes {
    let mut bytes = label.as_bytes().to_vec();
    bytes.resize(FULL_CHUNK_BYTES as usize, 0);
    Bytes::from(bytes)
}

fn full_named(count: usize) -> Vec<(String, ChunkRef, Bytes)> {
    (0..count)
        .map(|i| {
            let bytes = full(&format!("chunk {i}"));
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
async fn uploads_keep_as_many_full_chunks_in_flight_as_the_gate_allows_and_no_more() {
    let parts = full_named(12);
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
async fn small_chunks_share_the_bytes_of_one_full_chunk_in_flight() {
    let parts = named(40);
    let (relay, seen) = busy_relay(|_, _| ok_after(Duration::from_millis(100))).await;
    let (session, _events, _) = uploads_through(relay, &parts).await;

    let receipts = Transfer {
        gate: UploadGate::new(1),
        ..transfer()
    }
    .upload(&session, &refs(&parts), source(&parts))
    .await
    .unwrap();

    assert_eq!(receipts.len(), 40);
    assert_eq!(seen.lock().unwrap().peak, 16);
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
    let parts = full_named(6);
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
    let parts = full_named(12);
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
    let [a, b, c, d, e] = ["chunk a", "chunk b", "chunk c", "chunk d", "chunk e"].map(|label| {
        let bytes = full(label);
        (chunk(&bytes), bytes)
    });
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
async fn the_first_wave_of_full_chunks_is_as_wide_as_the_concurrency() {
    let parts = full_named(8);
    for concurrency in [1, 4] {
        let arrived = AtomicUsize::new(0);
        let (relay, seen) = busy_relay(move |_, _| {
            let first_wave = arrived.fetch_add(1, Ordering::SeqCst) < concurrency;
            ok_after(Duration::from_secs(if first_wave { 2 } else { 0 }))
        })
        .await;
        let (session, _events, _) = uploads_through(relay, &parts).await;

        let receipts = Transfer {
            gate: UploadGate::new(concurrency),
            ..transfer()
        }
        .upload(&session, &refs(&parts), source(&parts))
        .await
        .unwrap();

        assert_eq!(receipts.len(), 8);
        assert_eq!(seen.lock().unwrap().peak, concurrency);
    }
}
