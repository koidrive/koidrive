// SPDX-License-Identifier: Apache-2.0

fn offering() -> Welcome {
    Welcome {
        features: vec![CHUNK_BUNDLES.into()],
        ..welcome("h", 0)
    }
}

fn bundle_plan(relay: SocketAddr, chunk: &ChunkRef, ticket: &str) -> ChunkPlan {
    let mut plan = upload_plan(relay, chunk, ticket, HOUR);
    plan.target.as_mut().unwrap().url = format!(
        "http://{relay}/v1/spaces/s/chunks/{}?inc=0.1&ticket={ticket}",
        hex(&chunk.hash)
    );
    plan
}

#[derive(Clone, Debug, PartialEq)]
enum Wire {
    Put(String),
    Bundle(Vec<String>),
}

fn ticket_of(url: &str) -> String {
    url.split_once("ticket=")
        .unwrap()
        .1
        .split([' ', '&'])
        .next()
        .unwrap()
        .to_owned()
}

async fn bundle_relay(
    answer: Option<fn(&str, usize) -> u16>,
) -> (SocketAddr, Arc<Mutex<Vec<Wire>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let log = Arc::new(Mutex::new(Vec::<Wire>::new()));
    let seen = log.clone();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (head, body) = read_request(&mut socket).await;
            let earlier = |seen: &[Wire], ticket: &str| {
                seen.iter()
                    .map(|sent| match sent {
                        Wire::Put(t) => usize::from(t == ticket),
                        Wire::Bundle(ts) => ts.iter().filter(|t| *t == ticket).count(),
                    })
                    .sum::<usize>()
            };
            let (status, answer) = if head.starts_with("PUT ") {
                let ticket = ticket_of(&head);
                let receipt = UploadReceipt {
                    hash: Sha256::digest(&body).to_vec(),
                    size: body.len() as u32,
                    token: ticket.clone(),
                };
                seen.lock().unwrap().push(Wire::Put(ticket));
                (200, receipt.encode_to_vec())
            } else if let Some(answer) = answer {
                let bundle = ChunkBundle::decode(body.as_slice()).unwrap();
                let tickets: Vec<_> = bundle.chunks.iter().map(|c| ticket_of(&c.target)).collect();
                let results = {
                    let mut seen = seen.lock().unwrap();
                    let results = bundle
                        .chunks
                        .iter()
                        .zip(&tickets)
                        .map(|(chunk, ticket)| {
                            let status = answer(ticket, earlier(&seen, ticket));
                            let hash = Sha256::digest(&chunk.data).to_vec();
                            BundledChunkResult {
                                receipt: (status == 200).then(|| UploadReceipt {
                                    hash: hash.clone(),
                                    size: chunk.data.len() as u32,
                                    token: ticket.clone(),
                                }),
                                error: (status != 200).then(|| ProtoError {
                                    code: if status == 403 {
                                        Code::Forbidden
                                    } else {
                                        Code::RetryLater
                                    } as i32,
                                    ..Default::default()
                                }),
                                hash,
                                status: status.into(),
                            }
                        })
                        .collect();
                    seen.push(Wire::Bundle(tickets));
                    results
                };
                (200, ChunkBundleResult { results }.encode_to_vec())
            } else {
                (404, Vec::new())
            };
            let head = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                answer.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(&answer).await.unwrap();
        }
    });
    (address, log)
}

fn stored_all(_: &str, _: usize) -> u16 {
    200
}

async fn bundled_upload(
    welcome: Welcome,
    answer: Option<fn(&str, usize) -> u16>,
    parts: &[(String, ChunkRef, Bytes)],
) -> (
    Result<Vec<UploadReceipt>, ChunkError>,
    Vec<Wire>,
    Vec<Vec<ChunkRef>>,
    Session,
) {
    let (relay, sent) = bundle_relay(answer).await;
    let plans: Vec<_> = parts
        .iter()
        .map(|(name, chunk, _)| bundle_plan(relay, chunk, name))
        .collect();
    let (session, _events, asked) = scripted_session_with(welcome, move |_| plans.clone()).await;
    let receipts = transfer()
        .upload(&session, &refs(parts), source(parts))
        .await;
    let sent = sent.lock().unwrap().clone();
    let asked = asked.lock().unwrap().clone();
    (receipts, sent, asked, session)
}

#[tokio::test]
async fn small_chunks_go_in_bundles_of_at_most_32_when_the_server_offers_them() {
    let parts = named(40);
    let (receipts, sent, _, _) = bundled_upload(offering(), Some(stored_all), &parts).await;
    assert_eq!(sorted(tokens(receipts.unwrap())), sorted(names(&parts)));
    let sizes: Vec<_> = sent
        .iter()
        .map(|sent| match sent {
            Wire::Bundle(tickets) => tickets.len(),
            Wire::Put(_) => 1,
        })
        .collect();
    assert_eq!(sizes, [32, 8]);
    assert!(sent.iter().all(|sent| matches!(sent, Wire::Bundle(_))));
}

fn unordered(sent: &[Wire]) -> Vec<String> {
    sorted(sent.iter().map(|sent| format!("{sent:?}")).collect())
}

#[tokio::test]
async fn a_server_that_does_not_offer_bundles_gets_one_put_per_chunk() {
    let parts = named(5);
    let (receipts, sent, _, _) = bundled_upload(welcome("h", 0), Some(stored_all), &parts).await;
    assert_eq!(sorted(tokens(receipts.unwrap())), sorted(names(&parts)));
    assert_eq!(
        unordered(&sent),
        unordered(&names(&parts).into_iter().map(Wire::Put).collect::<Vec<_>>())
    );
}

#[tokio::test]
async fn a_server_without_the_bundle_route_gets_the_chunks_as_puts_and_no_more_bundles() {
    let parts = named(5);
    let (receipts, sent, _, session) = bundled_upload(offering(), None, &parts).await;
    assert_eq!(sorted(tokens(receipts.unwrap())), sorted(names(&parts)));
    assert_eq!(
        unordered(&sent),
        unordered(&names(&parts).into_iter().map(Wire::Put).collect::<Vec<_>>())
    );
    assert!(!session.bundles_offered());
}

#[tokio::test]
async fn chunks_of_a_full_permit_or_more_go_alone_while_smaller_ones_share_a_bundle() {
    let mut parts = named(3);
    let big = Bytes::from(vec![9u8; PERMIT_BYTES as usize]);
    parts.insert(1, ("big".into(), chunk(&big), big));
    let (receipts, sent, _, _) = bundled_upload(offering(), Some(stored_all), &parts).await;
    assert_eq!(receipts.unwrap().len(), 4);
    assert_eq!(
        unordered(&sent),
        unordered(&[
            Wire::Bundle(vec!["c0".into(), "c1".into(), "c2".into()]),
            Wire::Put("big".into())
        ])
    );
}

#[tokio::test]
async fn only_the_chunks_a_bundle_could_not_store_are_sent_again() {
    fn answer(ticket: &str, earlier: usize) -> u16 {
        match (ticket, earlier) {
            ("c1", 0) => 503,
            ("c2", 0) => 403,
            _ => 200,
        }
    }
    let parts = named(4);
    let (receipts, sent, asked, _) = bundled_upload(offering(), Some(answer), &parts).await;
    assert_eq!(sorted(tokens(receipts.unwrap())), sorted(names(&parts)));
    let tickets = |names: &[&str]| Wire::Bundle(names.iter().map(|n| (*n).into()).collect());
    assert_eq!(
        sent,
        [
            tickets(&["c0", "c1", "c2", "c3"]),
            tickets(&["c1"]),
            Wire::Put("c2".into())
        ]
    );
    assert_eq!(
        asked.len(),
        2,
        "the chunk whose ticket was refused is prepared again"
    );
    assert_eq!(asked[1], vec![parts[2].1.clone()]);
}

#[tokio::test]
async fn each_chunk_of_a_bundle_gets_the_outcome_a_put_with_the_same_answer_gets() {
    let sent = chunk(b"payload");
    let other = chunk(b"other");
    let error = |code: Code, retry_after_ms: u64| ProtoError {
        code: code as i32,
        message: "refused".into(),
        retry_after_ms,
        ..Default::default()
    };
    let receipt = |of: &ChunkRef| UploadReceipt {
        hash: of.hash.clone(),
        size: of.size,
        token: "t".into(),
    };
    let cases: Vec<(u16, Option<UploadReceipt>, Option<ProtoError>)> = vec![
        (200, Some(receipt(&sent)), None),
        (200, Some(receipt(&other)), None),
        (403, None, Some(error(Code::Forbidden, 0))),
        (401, None, Some(error(Code::Unauthenticated, 0))),
        (400, None, Some(error(Code::InvalidArgument, 0))),
        (413, None, Some(error(Code::LimitExceeded, 0))),
        (429, None, Some(error(Code::RetryLater, 1000))),
        (429, None, Some(error(Code::QuotaExceeded, 0))),
        (503, None, Some(error(Code::RetryLater, 0))),
        (503, None, None),
        (500, None, Some(error(Code::Internal, 0))),
        (404, None, Some(error(Code::NotFound, 0))),
    ];
    let mut alone = Vec::new();
    for (status, receipt, error) in &cases {
        let body = match (receipt, error) {
            (Some(receipt), _) => receipt.encode_to_vec(),
            (_, Some(error)) => error.encode_to_vec(),
            _ => Vec::new(),
        };
        let (target, server) = serve(vec![(*status, "application/x-protobuf", body)]).await;
        alone.push(outcome(
            &transfer()
                .put(&target, &sent, Bytes::from_static(b"payload"))
                .await,
        ));
        server.await.unwrap();
    }
    let in_bundle: Vec<_> = cases
        .into_iter()
        .map(|(status, receipt, error)| {
            outcome(&put_outcome(
                &sent,
                StatusCode::from_u16(status).unwrap(),
                receipt,
                error.map(ServerError),
                None,
            ))
        })
        .collect();
    assert_eq!(in_bundle, alone);
    assert_eq!(&alone[..3], ["done", &alone[1], "expired"]);
}

#[test]
fn only_relay_targets_of_the_chunk_itself_have_a_bundle_route() {
    let c = chunk(b"c");
    let target = |url: String, method: &str| Target {
        url,
        method: method.into(),
        ..Default::default()
    };
    let relay = format!(
        "https://k.test/base/v1/spaces/s1/chunks/{}?inc=0.1&ticket=t",
        hex(&c.hash)
    );
    assert_eq!(
        bundle_url(&target(relay.clone(), ""), &c).as_deref(),
        Some("https://k.test/base/v1/spaces/s1/chunks")
    );
    assert_eq!(
        bundle_url(&target(relay.clone(), "PUT"), &c).as_deref(),
        Some("https://k.test/base/v1/spaces/s1/chunks")
    );
    assert_eq!(bundle_url(&target(relay.clone(), "POST"), &c), None);
    assert_eq!(
        bundle_url(&target(relay.replace(&hex(&c.hash), "ab"), ""), &c),
        None
    );
    assert_eq!(
        bundle_url(
            &target("https://r2.test/bucket/key?X-Amz-Signature=s".into(), "PUT"),
            &c
        ),
        None
    );
}

type Post = fn(&ChunkBundle, usize) -> (u16, Vec<u8>);

struct Posted {
    sent: Vec<Wire>,
    most_at_once: usize,
}

async fn posting_relay(
    post: Post,
    put: fn(&str) -> u16,
    hold: Duration,
) -> (SocketAddr, Arc<Mutex<Posted>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let log = Arc::new(Mutex::new(Posted {
        sent: Vec::new(),
        most_at_once: 0,
    }));
    let seen = log.clone();
    let posts = Arc::new(AtomicUsize::new(0));
    let at_once = Arc::new(AtomicUsize::new(0));
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (seen, posts, at_once) = (seen.clone(), posts.clone(), at_once.clone());
            tokio::spawn(async move {
                let (head, body) = read_request(&mut socket).await;
                let (status, answer) = if head.starts_with("PUT ") {
                    let ticket = ticket_of(&head);
                    seen.lock().unwrap().sent.push(Wire::Put(ticket.clone()));
                    match put(&ticket) {
                        200 => {
                            let receipt = UploadReceipt {
                                hash: Sha256::digest(&body).to_vec(),
                                size: body.len() as u32,
                                token: ticket,
                            };
                            (200, receipt.encode_to_vec())
                        }
                        status => {
                            let error = ProtoError {
                                code: Code::RetryLater as i32,
                                ..Default::default()
                            };
                            (status, error.encode_to_vec())
                        }
                    }
                } else {
                    let bundle = ChunkBundle::decode(body.as_slice()).unwrap();
                    let now = at_once.fetch_add(1, Ordering::SeqCst) + 1;
                    {
                        let mut seen = seen.lock().unwrap();
                        seen.most_at_once = seen.most_at_once.max(now);
                        seen.sent.push(Wire::Bundle(
                            bundle.chunks.iter().map(|c| ticket_of(&c.target)).collect(),
                        ));
                    }
                    tokio::time::sleep(hold).await;
                    at_once.fetch_sub(1, Ordering::SeqCst);
                    post(&bundle, posts.fetch_add(1, Ordering::SeqCst))
                };
                if status == 0 {
                    return;
                }
                let head = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    answer.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(&answer).await;
            });
        }
    });
    (address, log)
}

fn answered(bundle: &ChunkBundle, status: impl Fn(&str) -> (u16, Code)) -> Vec<u8> {
    let results = bundle
        .chunks
        .iter()
        .map(|chunk| {
            let ticket = ticket_of(&chunk.target);
            let (status, code) = status(&ticket);
            let hash = Sha256::digest(&chunk.data).to_vec();
            BundledChunkResult {
                receipt: (status == 200).then(|| UploadReceipt {
                    hash: hash.clone(),
                    size: chunk.data.len() as u32,
                    token: ticket.clone(),
                }),
                error: (status != 200).then(|| ProtoError {
                    code: code as i32,
                    ..Default::default()
                }),
                hash,
                status: status.into(),
            }
        })
        .collect();
    ChunkBundleResult { results }.encode_to_vec()
}

fn all_stored(bundle: &ChunkBundle, _: usize) -> (u16, Vec<u8>) {
    (200, answered(bundle, |_| (200, Code::Unspecified)))
}

fn sized(count: usize, size: impl Fn(usize) -> usize) -> Vec<(String, ChunkRef, Bytes)> {
    (0..count)
        .map(|i| {
            let mut bytes = format!("chunk {i}").into_bytes();
            bytes.resize(size(i), i as u8);
            let bytes = Bytes::from(bytes);
            (format!("c{i}"), chunk(&bytes), bytes)
        })
        .collect()
}

struct Posting {
    result: Result<Vec<UploadReceipt>, ChunkError>,
    sent: Vec<Wire>,
    most_at_once: usize,
    stored: usize,
    session: Session,
}

impl Posting {
    fn posts(&self) -> Vec<usize> {
        (self.sent.iter())
            .filter_map(|sent| match sent {
                Wire::Bundle(tickets) => Some(tickets.len()),
                Wire::Put(_) => None,
            })
            .collect()
    }

    fn puts(&self) -> usize {
        self.sent.len() - self.posts().len()
    }
}

async fn posting(
    post: Post,
    parts: &[(String, ChunkRef, Bytes)],
    transfer: Transfer,
    hold: Duration,
) -> Posting {
    posting_with_puts(post, |_| 200, parts, transfer, hold).await
}

async fn posting_with_puts(
    post: Post,
    put: fn(&str) -> u16,
    parts: &[(String, ChunkRef, Bytes)],
    transfer: Transfer,
    hold: Duration,
) -> Posting {
    let (relay, log) = posting_relay(post, put, hold).await;
    let plans: Vec<_> = parts
        .iter()
        .map(|(name, chunk, _)| bundle_plan(relay, chunk, name))
        .collect();
    let (session, _events, _asked) =
        scripted_session_with(offering(), move |_| plans.clone()).await;
    let stored = Arc::new(AtomicUsize::new(0));
    let counted = stored.clone();
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        transfer.upload_observed(
            &session,
            &refs(parts),
            source(parts),
            move |_| {
                counted.fetch_add(1, Ordering::SeqCst);
            },
            |_| {},
        ),
    )
    .await
    .expect("the upload hung");
    let log = log.lock().unwrap();
    Posting {
        result,
        sent: log.sent.clone(),
        most_at_once: log.most_at_once,
        stored: stored.load(Ordering::SeqCst),
        session,
    }
}

#[tokio::test]
async fn after_a_404_from_the_bundle_route_the_rest_of_the_round_goes_as_puts_at_once() {
    fn missing(_: &ChunkBundle, _: usize) -> (u16, Vec<u8>) {
        (404, Vec::new())
    }
    let parts = named(100);
    let run = posting(missing, &parts, transfer(), Duration::from_millis(50)).await;
    assert_eq!(run.result.as_ref().unwrap().clone().len(), 100);
    assert_eq!(run.posts(), [32]);
    assert_eq!(run.puts(), 100);
    assert!(!run.session.bundles_offered());

    let parts = sized(40, |_| 250 << 10);
    let narrow = Transfer {
        gate: UploadGate::new(1),
        ..transfer()
    };
    let run = posting(missing, &parts, narrow, Duration::from_millis(50)).await;
    assert_eq!(run.result.as_ref().unwrap().clone().len(), 40);
    assert_eq!(run.posts().len(), 1);
    assert_eq!(run.puts(), 40);
}

#[tokio::test]
async fn a_whole_request_refusal_other_than_404_or_405_sends_the_bundle_as_puts() {
    fn invalid(_: &ChunkBundle, _: usize) -> (u16, Vec<u8>) {
        let error = ProtoError {
            code: Code::InvalidArgument as i32,
            message: "a bundle lists one chunk twice".into(),
            ..Default::default()
        };
        (400, error.encode_to_vec())
    }
    fn too_large(_: &ChunkBundle, _: usize) -> (u16, Vec<u8>) {
        let error = ProtoError {
            code: Code::LimitExceeded as i32,
            ..Default::default()
        };
        (413, error.encode_to_vec())
    }
    for post in [invalid as Post, too_large] {
        let parts = named(5);
        let run = posting(post, &parts, transfer(), Duration::ZERO).await;
        assert_eq!(
            sorted(tokens(run.result.as_ref().unwrap().clone())),
            sorted(names(&parts))
        );
        assert_eq!(run.posts(), [5]);
        assert_eq!(run.puts(), 5);
        assert_eq!(run.stored, 5);
        assert!(run.session.bundles_offered());
    }
}

#[tokio::test]
async fn a_bundle_cut_off_in_transit_is_sent_again_whole() {
    fn cut_once(bundle: &ChunkBundle, n: usize) -> (u16, Vec<u8>) {
        if n == 0 {
            (0, Vec::new())
        } else {
            all_stored(bundle, n)
        }
    }
    let parts = named(4);
    let run = posting(cut_once, &parts, transfer(), Duration::ZERO).await;
    assert_eq!(run.result.as_ref().unwrap().clone().len(), 4);
    assert_eq!(run.posts(), [4, 4]);
    assert_eq!(run.puts(), 0);
    assert_eq!(run.stored, 4);
}

#[tokio::test]
async fn a_chunk_a_bundle_never_stores_ends_the_upload_after_the_attempts_a_put_gets() {
    fn never_c1(bundle: &ChunkBundle, _: usize) -> (u16, Vec<u8>) {
        (
            200,
            answered(bundle, |ticket| match ticket {
                "c1" => (503, Code::RetryLater),
                _ => (200, Code::Unspecified),
            }),
        )
    }
    let parts = named(2);
    let run = posting(never_c1, &parts, transfer(), Duration::ZERO).await;
    assert!(run.result.is_err());
    assert_eq!(run.posts(), [2, 1, 1, 1, 1]);
    assert_eq!(run.posts().len(), transfer().attempts as usize);
    assert_eq!(run.stored, 1);
}

#[tokio::test]
async fn chunks_an_answer_receipted_count_as_stored_even_when_another_fails_for_good() {
    fn c0_too_large(bundle: &ChunkBundle, _: usize) -> (u16, Vec<u8>) {
        (
            200,
            answered(bundle, |ticket| match ticket {
                "c0" => (413, Code::LimitExceeded),
                _ => (200, Code::Unspecified),
            }),
        )
    }
    let parts = named(4);
    let run = posting(c0_too_large, &parts, transfer(), Duration::ZERO).await;
    assert!(run.result.is_err());
    assert_eq!(run.posts(), [4]);
    assert_eq!(run.stored, 3);
}

#[tokio::test]
async fn an_answer_with_the_wrong_count_or_hashes_sends_the_bundle_as_puts() {
    fn one_short(bundle: &ChunkBundle, _: usize) -> (u16, Vec<u8>) {
        let mut answer =
            ChunkBundleResult::decode(answered(bundle, |_| (200, Code::Unspecified)).as_slice())
                .unwrap();
        answer.results.pop();
        (200, answer.encode_to_vec())
    }
    fn wrong_hash(bundle: &ChunkBundle, _: usize) -> (u16, Vec<u8>) {
        let mut answer =
            ChunkBundleResult::decode(answered(bundle, |_| (200, Code::Unspecified)).as_slice())
                .unwrap();
        answer.results[1].hash = vec![0; 32];
        (200, answer.encode_to_vec())
    }
    for post in [one_short as Post, wrong_hash] {
        let parts = named(4);
        let run = posting(post, &parts, transfer(), Duration::ZERO).await;
        assert_eq!(
            sorted(tokens(run.result.as_ref().unwrap().clone())),
            sorted(names(&parts))
        );
        assert_eq!(run.posts(), [4]);
        assert_eq!(run.puts(), 4);
        assert_eq!(run.stored, 4);
    }
}

#[tokio::test]
async fn a_bundle_holds_at_most_1_mib_and_takes_gate_permits_for_all_its_bytes() {
    let parts = sized(35, |i| if i % 5 == 0 { 4 << 10 } else { 255 << 10 });
    let narrow = Transfer {
        gate: UploadGate::new(1),
        ..transfer()
    };
    let run = posting(all_stored, &parts, narrow, Duration::from_millis(150)).await;
    assert_eq!(run.result.as_ref().unwrap().clone().len(), 35);
    assert_eq!(run.posts(), [5; 7]);
    assert_eq!(run.puts(), 0);
    assert_eq!(
        run.most_at_once, 4,
        "a 1 MiB bundle takes 4 of the 16 permits"
    );
}

#[tokio::test]
async fn a_chunk_falling_back_to_puts_keeps_the_attempts_its_bundle_already_used() {
    fn busy_then_invalid(_: &ChunkBundle, n: usize) -> (u16, Vec<u8>) {
        let code = if n < 4 {
            Code::RetryLater
        } else {
            Code::InvalidArgument
        };
        let error = ProtoError {
            code: code as i32,
            ..Default::default()
        };
        (if n < 4 { 503 } else { 400 }, error.encode_to_vec())
    }
    let parts = named(2);
    let run = posting_with_puts(
        busy_then_invalid,
        |_| 503,
        &parts,
        transfer(),
        Duration::ZERO,
    )
    .await;
    assert!(run.result.is_err());
    assert_eq!(run.posts(), [2; 5]);
    assert_eq!(
        run.puts(),
        1,
        "4 bundle attempts leave one PUT attempt of 5"
    );
}

#[tokio::test]
async fn a_bundle_waiting_to_be_resent_goes_as_puts_once_another_bundle_gets_a_404() {
    fn busy_then_missing(bundle: &ChunkBundle, n: usize) -> (u16, Vec<u8>) {
        match n {
            1 => {
                let error = ProtoError {
                    code: Code::RetryLater as i32,
                    retry_after_ms: 300,
                    ..Default::default()
                };
                (503, error.encode_to_vec())
            }
            2 => (404, Vec::new()),
            _ => all_stored(bundle, n),
        }
    }
    let parts = named(130);
    let run = posting(
        busy_then_missing,
        &parts,
        transfer(),
        Duration::from_millis(30),
    )
    .await;
    assert_eq!(run.result.as_ref().unwrap().len(), 130);
    let bundles: Vec<_> = (run.sent.iter())
        .filter_map(|sent| match sent {
            Wire::Bundle(tickets) => Some(tickets.clone()),
            Wire::Put(_) => None,
        })
        .collect();
    let mut seen = std::collections::HashSet::new();
    assert!(
        bundles.iter().all(|tickets| seen.insert(tickets.clone())),
        "a bundle was sent again after another bundle got a 404: {:?}",
        run.posts()
    );
    assert!(!run.session.bundles_offered());
}

#[tokio::test]
async fn a_refusal_keeps_the_server_cause_and_the_code_of_an_edge_error_page() {
    let sent = chunk(b"payload");
    let overloaded = ProtoError {
        code: Code::RetryLater as i32,
        cause: koidrive_proto::v1::error::Cause::DoOverloaded as i32,
        ..Default::default()
    }
    .encode_to_vec();
    let (target, server) = serve(vec![
        (503, "application/x-protobuf", overloaded),
        (
            503,
            "text/plain; charset=UTF-8",
            b"error code: 1102".to_vec(),
        ),
        (
            500,
            "text/plain; charset=UTF-8",
            b"error code: 1101".to_vec(),
        ),
        (
            502,
            "text/html",
            b"<html><body>Bad gateway</body></html>".to_vec(),
        ),
    ])
    .await;
    let transfer = transfer();
    let Attempt::Retry(ChunkError::Refused {
        error: Some(error),
        cloudflare_error_code: None,
        ..
    }) = transfer
        .put(&target, &sent, Bytes::from_static(b"payload"))
        .await
    else {
        panic!("a RETRY_LATER refusal is retried with its error");
    };
    assert_eq!(
        error.0.cause,
        koidrive_proto::v1::error::Cause::DoOverloaded as i32
    );
    assert!(matches!(
        transfer
            .put(&target, &sent, Bytes::from_static(b"payload"))
            .await,
        Attempt::Retry(ChunkError::Refused {
            status: 503,
            error: None,
            cloudflare_error_code: Some(1102),
            ..
        })
    ));
    assert!(matches!(
        transfer.get(&target, &sent).await,
        Attempt::Retry(ChunkError::Refused {
            status: 500,
            error: None,
            cloudflare_error_code: Some(1101),
            ..
        })
    ));
    assert!(matches!(
        transfer.get(&target, &sent).await,
        Attempt::Retry(ChunkError::Refused {
            status: 502,
            error: None,
            cloudflare_error_code: None,
            ..
        })
    ));
    server.await.unwrap();
}

#[tokio::test]
async fn a_transfer_with_bundles_off_sends_one_put_per_chunk_to_a_server_that_offers_them() {
    let parts = named(40);
    let off = Transfer {
        bundles: false,
        ..transfer()
    };
    let run = posting(all_stored, &parts, off, Duration::ZERO).await;
    assert_eq!(run.result.as_ref().unwrap().len(), 40);
    assert_eq!(run.posts(), Vec::<usize>::new());
    assert_eq!(run.puts(), 40);
    assert!(run.session.bundles_offered());
}
