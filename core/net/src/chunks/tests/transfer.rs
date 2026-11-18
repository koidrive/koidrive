// SPDX-License-Identifier: Apache-2.0

#[tokio::test]
async fn upload_gate_reports_configured_concurrency_while_busy() {
    let gate = UploadGate::new(2);
    let permit = gate.enter(1).await;
    assert_eq!(gate.concurrency(), 2);
    assert_eq!(gate.clone().concurrency(), 2);
    drop(permit);
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
        .retrying(0, async || transfer.get(&target, &wanted).await)
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
        .retrying(0, async || {
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
