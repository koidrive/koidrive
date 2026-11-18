// SPDX-License-Identifier: Apache-2.0

#[test]
fn a_commit_is_resent_only_when_welcome_accounts_for_its_slot() {
    let slot = CommitSlot {
        commit_id: "c".into(),
        history_id: "h".into(),
        device_seq: 5,
    };
    assert_eq!(may_resend(&slot, &welcome("h", 4)), Ok(()), "not accepted");
    assert_eq!(
        may_resend(&slot, &welcome("h", 5)),
        Ok(()),
        "accepted, response lost"
    );
    assert_eq!(
        may_resend(&slot, &welcome("h", 3)),
        Err(NotResent::DeviceSeqMismatch {
            slot: 5,
            welcome: 3
        })
    );
    assert_eq!(
        may_resend(&slot, &welcome("h", 6)),
        Err(NotResent::DeviceSeqMismatch {
            slot: 5,
            welcome: 6
        })
    );
    assert_eq!(
        may_resend(&slot, &welcome("other", 4)),
        Err(NotResent::HistoryChanged {
            commit: "h".into(),
            welcome: "other".into()
        })
    );
    let first = CommitSlot {
        device_seq: 1,
        ..slot
    };
    assert_eq!(may_resend(&first, &welcome("h", 0)), Ok(()));
}

#[test]
fn the_websocket_url_follows_the_base_scheme_and_path() {
    let url = |base: &str| {
        websocket_url(
            &Api::local_development(base.parse().unwrap())
                .unwrap()
                .base()
                .clone(),
            "0190-ab",
        )
        .unwrap()
        .to_string()
    };
    assert_eq!(
        url("http://127.0.0.1:8787"),
        "ws://127.0.0.1:8787/v1/spaces/0190-ab/ws"
    );
    assert_eq!(
        url("https://files.example.com/fb"),
        "wss://files.example.com/fb/v1/spaces/0190-ab/ws"
    );
    let base = Api::local_development("http://localhost".parse().unwrap())
        .unwrap()
        .base()
        .clone();
    assert!(websocket_url(&base, "../x").is_err());
}

#[tokio::test]
async fn responses_are_matched_to_calls_by_req_id() {
    let (listener, auth) = listen().await;
    let (session, _events) = Session::start(auth, config()).unwrap();
    let server = tokio::spawn(async move {
        let mut ws = accept(&listener, welcome("h", 0)).await;
        let first = recv_rpc(&mut ws).await;
        let second = recv_rpc(&mut ws).await;
        assert_ne!(first.req_id, second.req_id);
        let page = |cursor: &str| {
            rpc_response::Result::Tree(TreePage {
                next_cursor: cursor.into(),
                ..Default::default()
            })
        };
        reply(&mut ws, second.req_id, page("second")).await;
        reply(&mut ws, first.req_id, page("first")).await;
        ws
    });
    let (a, b) = tokio::join!(session.list_tree("a", 1), async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        session.list_tree("b", 1).await
    });
    assert_eq!(a.unwrap().next_cursor, "first");
    assert_eq!(b.unwrap().next_cursor, "second");
    drop(server.await.unwrap());
}

#[tokio::test]
async fn a_message_of_1_mib_is_read_and_a_larger_one_drops_the_connection() {
    let (listener, auth) = listen().await;
    let (_session, mut events) = Session::start(auth, config()).unwrap();
    let largest = notice_of_size(MAX_SERVER_MESSAGE);
    let too_large = notice_of_size(MAX_SERVER_MESSAGE + 1);
    let server = tokio::spawn({
        let largest = largest.clone();
        async move {
            let mut ws = accept(&listener, welcome("h", 0)).await;
            push(&mut ws, server_message::Msg::Notice(largest)).await;
            let frame = ServerMessage {
                msg: Some(server_message::Msg::Notice(too_large)),
            }
            .encode_to_vec();
            let _ = ws.send(WsMessage::Binary(frame.into())).await;
            (listener, ws)
        }
    });
    assert!(matches!(next(&mut events).await, Event::Connected(_)));
    assert_eq!(next(&mut events).await, Event::Notice(largest));
    match next(&mut events).await {
        Event::Disconnected {
            reason: DisconnectReason::ProtocolViolation(what),
            ..
        } => assert!(what.contains("1 MiB"), "{what}"),
        other => panic!("expected a protocol violation, got {other:?}"),
    }
    drop(server.await.unwrap());
}

#[tokio::test]
async fn an_oversized_request_is_refused_before_it_is_sent() {
    let (_listener, auth) = listen().await;
    let (session, _events) = Session::start(auth, config()).unwrap();
    let chunks = vec![
        ChunkRef {
            hash: vec![7; 32],
            size: 1,
            encoding: 0
        };
        MAX_CLIENT_MESSAGE / 30
    ];
    match session.prepare_upload(chunks).await {
        Err(CallError::TooLarge { bytes }) => assert!(bytes > MAX_CLIENT_MESSAGE),
        other => panic!("expected TooLarge, got {other:?}"),
    }
}

#[tokio::test]
async fn a_revoked_device_stops_the_session_and_fails_pending_calls() {
    let (listener, auth) = listen().await;
    let (session, mut events) = Session::start(auth, config()).unwrap();
    let server = tokio::spawn(async move {
        let mut ws = accept(&listener, welcome("h", 0)).await;
        recv_rpc(&mut ws).await;
        push(
            &mut ws,
            server_message::Msg::Goodbye(Goodbye {
                code: 4003,
                reason: "device revoked".into(),
            }),
        )
        .await;
        let _ = ws.close(None).await;
    });
    assert_eq!(
        session.list_trash("", 10).await,
        Err(CallError::Stopped(StopReason::DeviceRevoked))
    );
    assert!(matches!(next(&mut events).await, Event::Connected(_)));
    assert_eq!(
        next(&mut events).await,
        Event::Stopped(StopReason::DeviceRevoked)
    );
    assert_eq!(
        session.list_trash("", 10).await,
        Err(CallError::Stopped(StopReason::DeviceRevoked))
    );
    server.await.unwrap();
}

#[tokio::test]
async fn retry_later_is_sent_again_after_the_requested_wait() {
    let (listener, auth) = listen().await;
    let (session, _events) = Session::start(auth, config()).unwrap();
    let server = tokio::spawn(async move {
        let mut ws = accept(&listener, welcome("h", 0)).await;
        let first = recv_rpc(&mut ws).await;
        let busy = ProtoError {
            code: Code::RetryLater as i32,
            retry_after_ms: 20,
            ..Default::default()
        };
        reply(&mut ws, first.req_id, rpc_response::Result::Error(busy)).await;
        let second = recv_rpc(&mut ws).await;
        assert_eq!(first.call, second.call);
        reply(
            &mut ws,
            second.req_id,
            rpc_response::Result::Trash(Default::default()),
        )
        .await;
        ws
    });
    session.list_trash("", 10).await.unwrap();
    drop(server.await.unwrap());
}

#[tokio::test]
async fn a_dropped_commit_is_resent_unchanged_when_welcome_shows_it_was_accepted() {
    let (listener, auth) = listen().await;
    let (session, mut events) = Session::start(auth, config()).unwrap();
    let server = tokio::spawn(async move {
        let mut ws = accept(&listener, welcome("h", 1)).await;
        let sent = recv_rpc(&mut ws).await;
        drop(ws);
        let ws = accept(&listener, welcome("h", 2)).await;
        let (mut http, _) = listener.accept().await.unwrap();
        let (head, body) = read_http(&mut http).await;
        assert!(
            head.starts_with("POST /v1/spaces/0190a0b0-0000-7000-8000-000000000001/rpc "),
            "{head}"
        );
        let resent = RpcRequest::decode(&body[..]).unwrap();
        assert_eq!(sent.call, resent.call, "the same call");
        let result = CommitResult {
            seq: 4,
            ..Default::default()
        };
        let response = RpcResponse {
            req_id: resent.req_id,
            result: Some(rpc_response::Result::Commit(result)),
        };
        write_http(&mut http, 200, response.encode_to_vec()).await;
        ws
    });
    let result = session.commit(commit(2)).await.unwrap();
    assert_eq!(result.seq, 4);
    let mut seen = Vec::new();
    while !matches!(seen.last(), Some(Event::Committed(_))) {
        seen.push(next(&mut events).await);
    }
    assert!(
        matches!(
            seen[..],
            [
                Event::Connected(_),
                Event::Disconnected { .. },
                Event::Connected(_),
                Event::Committed(_)
            ]
        ),
        "{seen:?}"
    );
    drop(server.await.unwrap());
}

#[tokio::test]
async fn a_dropped_commit_is_not_resent_when_welcome_cannot_account_for_it() {
    for (later, expected) in [
        (
            welcome("h", 7),
            NotResent::DeviceSeqMismatch {
                slot: 2,
                welcome: 7,
            },
        ),
        (
            welcome("new", 1),
            NotResent::HistoryChanged {
                commit: "h".into(),
                welcome: "new".into(),
            },
        ),
    ] {
        let (listener, auth) = listen().await;
        let (session, _events) = Session::start(auth, config()).unwrap();
        let server = tokio::spawn(async move {
            let mut ws = accept(&listener, welcome("h", 1)).await;
            recv_rpc(&mut ws).await;
            drop(ws);
            let mut ws = accept(&listener, later).await;
            let after = recv_rpc(&mut ws).await;
            assert!(
                matches!(after.call, Some(Call::ListTree(_))),
                "the commit was not resent: {after:?}"
            );
            reply(
                &mut ws,
                after.req_id,
                rpc_response::Result::Tree(Default::default()),
            )
            .await;
            ws
        });
        assert_eq!(
            session.commit(commit(2)).await,
            Err(CallError::NotResent(expected))
        );
        session.list_tree("", 1).await.unwrap();
        drop(server.await.unwrap());
    }
}

#[tokio::test]
async fn the_stream_resumes_from_the_last_seq_after_a_reconnect() {
    let (listener, auth) = listen().await;
    let (session, mut events) = Session::start(auth, config()).unwrap();
    session.subscribe(Resume {
        history_id: "h".into(),
        since_seq: 2,
        ..Default::default()
    });
    let record = |seq: u64| CommitRecord {
        seq,
        commit_id: format!("c{seq}"),
        committed_at_ms: seq as i64 * 10,
        ..Default::default()
    };
    let server = tokio::spawn(async move {
        let mut ws = accept(&listener, welcome("h", 0)).await;
        assert_eq!(
            recv(&mut ws).await,
            client_message::Msg::Resume(Resume {
                history_id: "h".into(),
                since_seq: 2,
                ..Default::default()
            })
        );
        push(
            &mut ws,
            server_message::Msg::Changes(Changes {
                commits: vec![record(3), record(4)],
            }),
        )
        .await;
        push(
            &mut ws,
            server_message::Msg::CaughtUp(CaughtUp {
                head_seq: 4,
                ..Default::default()
            }),
        )
        .await;
        drop(ws);
        let mut ws = accept(&listener, welcome("h", 0)).await;
        assert_eq!(
            recv(&mut ws).await,
            client_message::Msg::Resume(Resume {
                history_id: "h".into(),
                since_seq: 4,
                since_commit_id: "c4".into(),
                since_committed_at_ms: 40,
            })
        );
        push(
            &mut ws,
            server_message::Msg::Changes(Changes {
                commits: vec![record(4), record(5)],
            }),
        )
        .await;
        ws
    });
    let mut changes = Vec::new();
    while changes.last() != Some(&5) {
        if let Event::Changes(records) = next(&mut events).await {
            changes.extend(records.iter().map(|r| r.seq));
        }
    }
    assert_eq!(changes, [3, 4, 5]);
    drop(server.await.unwrap());
}

#[tokio::test]
async fn a_welcome_with_a_protocol_not_offered_stops_before_anything_is_sent() {
    let (listener, auth) = listen().await;
    let (session, mut events) = Session::start(auth, config()).unwrap();
    session.subscribe(Resume {
        history_id: "h".into(),
        ..Default::default()
    });
    let server = tokio::spawn(async move {
        let mut ws = accept(
            &listener,
            Welcome {
                proto: 999,
                ..welcome("h", 0)
            },
        )
        .await;
        let after = tokio::time::timeout(Duration::from_secs(1), ws.next()).await;
        assert!(
            !matches!(after, Ok(Some(Ok(WsMessage::Binary(_))))),
            "no Resume or RPC after an unsupported Welcome: {after:?}"
        );
    });
    let call = session.list_tree("", 1).await;
    let Err(CallError::Stopped(StopReason::UnsupportedVersion(why))) = call else {
        panic!("expected UnsupportedVersion, got {call:?}");
    };
    assert!(why.contains("1.999"), "{why}");
    match next(&mut events).await {
        Event::Stopped(StopReason::UnsupportedVersion(_)) => {}
        other => panic!("no Connected for an unsupported Welcome, got {other:?}"),
    }
    server.await.unwrap();
}

#[tokio::test]
async fn a_withheld_upgrade_times_out_and_close_does_not_wait_for_it() {
    let (listener, auth) = listen().await;
    let mut config = config();
    config.connect_timeout = Duration::from_millis(30);
    config.backoff = Backoff {
        initial: Duration::from_secs(5),
        max: Duration::from_secs(5),
    };
    let (session, mut events) = Session::start(auth.clone(), config).unwrap();
    let (mut tcp, _) = listener.accept().await.unwrap();
    let mut buffer = [0; 4096];
    assert!(
        tcp.read(&mut buffer).await.unwrap() > 0,
        "the upgrade request arrived"
    );
    let event = tokio::time::timeout(Duration::from_millis(500), events.next()).await;
    assert!(
        matches!(
            event,
            Ok(Some(Event::Disconnected {
                reason: DisconnectReason::Timeout,
                ..
            }))
        ),
        "{event:?}"
    );
    let closed = tokio::time::timeout(Duration::from_millis(150), session.close()).await;
    assert!(
        closed.is_ok(),
        "close interrupts the wait before the next attempt"
    );

    let mut config = self::config();
    config.connect_timeout = Duration::from_secs(60);
    let (session, _events) = Session::start(auth, config).unwrap();
    let (mut tcp, _) = listener.accept().await.unwrap();
    assert!(tcp.read(&mut buffer).await.unwrap() > 0);
    let closed = tokio::time::timeout(Duration::from_millis(150), session.close()).await;
    assert!(closed.is_ok(), "close interrupts a withheld upgrade");
}

#[tokio::test]
async fn responses_to_unknown_or_answered_req_ids_are_ignored() {
    let (listener, auth) = listen().await;
    let (session, _events) = Session::start(auth, config()).unwrap();
    let server = tokio::spawn(async move {
        let mut ws = accept(&listener, welcome("h", 0)).await;
        let page = |cursor: &str| {
            rpc_response::Result::Tree(TreePage {
                next_cursor: cursor.into(),
                ..Default::default()
            })
        };
        let first = recv_rpc(&mut ws).await;
        reply(&mut ws, first.req_id + 999, page("unknown")).await;
        reply(&mut ws, first.req_id, page("first")).await;
        reply(&mut ws, first.req_id, page("duplicate")).await;
        let second = recv_rpc(&mut ws).await;
        reply(&mut ws, second.req_id, page("second")).await;
        ws
    });
    assert_eq!(session.list_tree("", 1).await.unwrap().next_cursor, "first");
    assert_eq!(
        session.list_tree("", 1).await.unwrap().next_cursor,
        "second"
    );
    drop(server.await.unwrap());
}

#[tokio::test]
async fn a_malformed_server_message_drops_the_connection_as_a_protocol_violation() {
    let (listener, auth) = listen().await;
    let (_session, mut events) = Session::start(auth, config()).unwrap();
    let server = tokio::spawn(async move {
        let mut ws = accept(&listener, welcome("h", 0)).await;
        ws.send(WsMessage::Binary(vec![0xff].into())).await.unwrap();
        (listener, ws)
    });
    assert!(matches!(next(&mut events).await, Event::Connected(_)));
    assert!(matches!(
        next(&mut events).await,
        Event::Disconnected {
            reason: DisconnectReason::ProtocolViolation(_),
            ..
        }
    ));
    drop(server.await.unwrap());
}

async fn block_a_write(session: &Arc<Session>) {
    let calls = tokio::spawn({
        let session = session.clone();
        async move {
            let chunks = vec![
                ChunkRef {
                    hash: vec![7; 32],
                    size: 1,
                    ..Default::default()
                };
                1000
            ];
            futures_util::future::join_all((0..250).map(|_| session.prepare_upload(chunks.clone())))
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!calls.is_finished(), "the server answers nothing");
    calls.abort();
    let _ = calls.await;
}

#[tokio::test]
async fn close_interrupts_a_write_the_server_does_not_read() {
    let (listener, auth) = listen().await;
    let (session, mut events) = Session::start(auth, config()).unwrap();
    let _server = accept(&listener, welcome("h", 0)).await;
    assert!(matches!(next(&mut events).await, Event::Connected(_)));
    let session = Arc::new(session);
    block_a_write(&session).await;
    let session = Arc::try_unwrap(session).unwrap();
    let closed = tokio::time::timeout(Duration::from_millis(500), session.close()).await;
    assert!(closed.is_ok(), "close does not wait for the blocked write");
    assert!(matches!(
        next(&mut events).await,
        Event::Stopped(StopReason::Closed)
    ));
}

#[tokio::test]
async fn a_write_the_server_does_not_read_times_out() {
    let (listener, auth) = listen().await;
    let mut config = config();
    config.write_timeout = Duration::from_millis(200);
    let (session, mut events) = Session::start(auth, config).unwrap();
    let _server = accept(&listener, welcome("h", 0)).await;
    assert!(matches!(next(&mut events).await, Event::Connected(_)));
    let session = Arc::new(session);
    let disconnected =
        tokio::spawn(
            async move { tokio::time::timeout(Duration::from_secs(2), events.next()).await },
        );
    block_a_write(&session).await;
    let event = disconnected.await.unwrap();
    assert!(
        matches!(
            event,
            Ok(Some(Event::Disconnected {
                reason: DisconnectReason::Timeout,
                ..
            }))
        ),
        "{event:?}"
    );
}

#[tokio::test]
async fn an_ipv6_loopback_base_connects() {
    let (listener, auth) = listen_on("[::1]:0", Duration::from_secs(3600), None).await;
    let (_session, mut events) = Session::start(auth, config()).unwrap();
    let _server = accept(&listener, welcome("h", 0)).await;
    assert!(matches!(next(&mut events).await, Event::Connected(_)));
}

#[tokio::test]
async fn retry_later_waits_as_long_as_the_server_asks_beyond_the_backoff_limit() {
    let (listener, auth) = listen().await;
    let config = config();
    assert!(config.backoff.max < Duration::from_millis(300));
    let (session, _events) = Session::start(auth, config).unwrap();
    let server = tokio::spawn(async move {
        let mut ws = accept(&listener, welcome("h", 0)).await;
        let first = recv_rpc(&mut ws).await;
        let busy = ProtoError {
            code: Code::RetryLater as i32,
            retry_after_ms: 300,
            ..Default::default()
        };
        reply(&mut ws, first.req_id, rpc_response::Result::Error(busy)).await;
        let asked = Instant::now();
        let second = recv_rpc(&mut ws).await;
        assert!(
            asked.elapsed() >= Duration::from_millis(300),
            "{:?}",
            asked.elapsed()
        );
        reply(
            &mut ws,
            second.req_id,
            rpc_response::Result::Trash(Default::default()),
        )
        .await;
        ws
    });
    session.list_trash("", 10).await.unwrap();
    drop(server.await.unwrap());
}

#[tokio::test]
async fn the_connection_keeps_serving_while_a_token_refresh_is_withheld() {
    let (listener, auth) = listen_on(
        "127.0.0.1:0",
        Duration::from_millis(300),
        Some(Duration::ZERO),
    )
    .await;
    let mut config = config();
    config.connect_timeout = Duration::from_secs(60);
    let (session, mut events) = Session::start(auth, config).unwrap();
    let mut ws = accept(&listener, welcome("h", 0)).await;
    assert!(matches!(next(&mut events).await, Event::Connected(_)));
    let (mut token_request, _) = listener.accept().await.unwrap();
    assert!(
        token_request.read(&mut [0; 4096]).await.unwrap() > 0,
        "the refresh is under way"
    );
    let server = tokio::spawn(async move {
        let request = recv_rpc(&mut ws).await;
        reply(
            &mut ws,
            request.req_id,
            rpc_response::Result::Tree(Default::default()),
        )
        .await;
        ws
    });
    let listed = tokio::time::timeout(Duration::from_millis(500), session.list_tree("", 1)).await;
    assert!(
        matches!(listed, Ok(Ok(_))),
        "an RPC is served while the refresh waits: {listed:?}"
    );
    let _ws = server.await.unwrap();
    let closed = tokio::time::timeout(Duration::from_millis(500), session.close()).await;
    assert!(closed.is_ok(), "close does not wait for the refresh");
}

#[tokio::test]
async fn tokens_that_are_due_at_once_are_not_refreshed_in_a_loop() {
    let (listener, auth) = listen_on(
        "127.0.0.1:0",
        Duration::from_secs(3600),
        Some(Duration::from_secs(7200)),
    )
    .await;
    let issued = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let server = tokio::spawn({
        let issued = issued.clone();
        async move {
            let mut sockets = Vec::new();
            loop {
                let (mut tcp, _) = listener.accept().await.unwrap();
                let mut start = [0; 4];
                tcp.peek(&mut start).await.unwrap();
                if &start != b"POST" {
                    sockets.push(accept_upgrade(tcp, welcome("h", 0)).await);
                    continue;
                }
                let n = issued.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut request = [0; 4096];
                let _ = tcp.read(&mut request).await.unwrap();
                let expires = SystemTime::now() + Duration::from_secs(3600);
                let body = AccessToken {
                    token: format!("token-{n}"),
                    expires_at_ms: expires.duration_since(UNIX_EPOCH).unwrap().as_millis() as i64,
                    ..Default::default()
                }
                .encode_to_vec();
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                tcp.write_all(head.as_bytes()).await.unwrap();
                tcp.write_all(&body).await.unwrap();
            }
        }
    });
    let (session, mut events) = Session::start(auth, config()).unwrap();
    assert!(matches!(next(&mut events).await, Event::Connected(_)));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let issued = issued.load(std::sync::atomic::Ordering::SeqCst);
    assert!(issued <= 3, "{issued} tokens in 1.5 s");
    session.close().await;
    server.abort();
}

#[tokio::test]
async fn a_token_expiry_close_during_a_refresh_reconnects_with_a_new_token() {
    let (listener, auth) = listen_on(
        "127.0.0.1:0",
        Duration::from_millis(300),
        Some(Duration::ZERO),
    )
    .await;
    let mut config = config();
    config.connect_timeout = Duration::from_secs(60);
    let (session, mut events) = Session::start(auth, config).unwrap();
    let mut ws = accept(&listener, welcome("h", 0)).await;
    assert!(matches!(next(&mut events).await, Event::Connected(_)));
    let (mut refresh, _) = listener.accept().await.unwrap();
    assert!(
        refresh.read(&mut [0; 4096]).await.unwrap() > 0,
        "a refresh is in flight and gets no answer"
    );
    let goodbye = Goodbye {
        code: u32::from(CLOSE_TOKEN_EXPIRED),
        reason: "expired".into(),
    };
    push(&mut ws, server_message::Msg::Goodbye(goodbye)).await;
    let event = tokio::time::timeout(Duration::from_millis(500), events.next()).await;
    assert!(
        matches!(
            event,
            Ok(Some(Event::Disconnected {
                reason: DisconnectReason::Closed {
                    code: Some(CLOSE_TOKEN_EXPIRED),
                    ..
                },
                ..
            }))
        ),
        "the expiry is handled while the refresh waits: {event:?}"
    );
    let (mut token_request, _) =
        tokio::time::timeout(Duration::from_millis(500), listener.accept())
            .await
            .expect("the session asks for a new token to reconnect")
            .unwrap();
    assert!(token_request.read(&mut [0; 4096]).await.unwrap() > 0);
    let closed = tokio::time::timeout(Duration::from_millis(500), session.close()).await;
    assert!(closed.is_ok(), "close does not wait for the token request");
    drop(refresh);
}
