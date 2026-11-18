// SPDX-License-Identifier: Apache-2.0

async fn read_http(tcp: &mut TcpStream) -> (String, Vec<u8>) {
    let mut request = Vec::new();
    let mut chunk = [0; 4096];
    let end = loop {
        let n = tcp.read(&mut chunk).await.unwrap();
        assert!(n > 0, "the client hung up");
        request.extend_from_slice(&chunk[..n]);
        if let Some(at) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
    };
    let head = String::from_utf8_lossy(&request[..end]).into_owned();
    let length: usize = (head.lines())
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().unwrap())
        })
        .unwrap_or(0);
    while request.len() < end + length {
        let n = tcp.read(&mut chunk).await.unwrap();
        assert!(n > 0, "the client hung up");
        request.extend_from_slice(&chunk[..n]);
    }
    (head, request[end..end + length].to_vec())
}

async fn write_http(tcp: &mut TcpStream, status: u16, body: Vec<u8>) {
    let head = format!(
        "HTTP/1.1 {status} X\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    tcp.write_all(&[head.as_bytes(), &body].concat())
        .await
        .unwrap();
}

#[tokio::test]
async fn a_dropped_commit_resent_over_http_returns_the_quota_refusal_and_its_wait() {
    let (listener, auth) = listen().await;
    let mut config = config();
    config.retry_later_attempts = 0;
    let (session, _events) = Session::start(auth, config).unwrap();
    let server = tokio::spawn(async move {
        let mut ws = accept(&listener, welcome("h", 1)).await;
        recv_rpc(&mut ws).await;
        drop(ws);
        let ws = accept(&listener, welcome("h", 1)).await;
        let (mut http, _) = listener.accept().await.unwrap();
        read_http(&mut http).await;
        let quota = refusal(Code::QuotaExceeded as i32, 60_000);
        write_http(&mut http, 429, quota.encode_to_vec()).await;
        ws
    });
    let refused = tokio::time::timeout(Duration::from_secs(5), session.commit(commit(2)))
        .await
        .expect("the resent commit is answered");
    assert!(
        matches!(&refused, Err(CallError::Server(error))
            if error.code() == Code::QuotaExceeded && error.retry_after() == Some(Duration::from_secs(60))),
        "{refused:?}"
    );
    drop(server.await.unwrap());
}

#[tokio::test]
async fn a_dropped_commit_goes_on_the_socket_when_its_http_resend_gets_no_answer() {
    let (listener, auth) = listen().await;
    let (session, _events) = Session::start(auth, config()).unwrap();
    let server = tokio::spawn(async move {
        let mut ws = accept(&listener, welcome("h", 1)).await;
        let sent = recv_rpc(&mut ws).await;
        drop(ws);
        let mut ws = accept(&listener, welcome("h", 1)).await;
        let (mut http, _) = listener.accept().await.unwrap();
        read_http(&mut http).await;
        drop(http);
        let resent = recv_rpc(&mut ws).await;
        assert_eq!(sent, resent, "the same request, req_id included");
        let result = CommitResult {
            seq: 4,
            ..Default::default()
        };
        reply(&mut ws, resent.req_id, rpc_response::Result::Commit(result)).await;
        ws
    });
    let result = tokio::time::timeout(Duration::from_secs(5), session.commit(commit(2)))
        .await
        .expect("the resent commit is answered")
        .unwrap();
    assert_eq!(result.seq, 4);
    drop(server.await.unwrap());
}

#[tokio::test]
async fn a_commit_out_over_http_is_not_sent_on_the_socket_after_a_welcome_of_another_history() {
    let (listener, auth) = listen().await;
    let (session, _events) = Session::start(auth, config()).unwrap();
    let server = tokio::spawn(async move {
        let mut first = accept(&listener, welcome("h", 1)).await;
        recv_rpc(&mut first).await;
        drop(first);
        let second = accept(&listener, welcome("h", 1)).await;
        let (mut http, _) = listener.accept().await.unwrap();
        read_http(&mut http).await;
        drop(second);
        let mut third = accept(&listener, welcome("another", 1)).await;
        drop(http);
        let sent = tokio::time::timeout(Duration::from_millis(300), recv_rpc(&mut third)).await;
        assert!(
            sent.is_err(),
            "the commit went out after the new Welcome: {sent:?}"
        );
        third
    });
    let result = tokio::time::timeout(Duration::from_secs(5), session.commit(commit(2)))
        .await
        .expect("the commit is answered");
    assert_eq!(
        result,
        Err(CallError::NotResent(NotResent::HistoryChanged {
            commit: "h".into(),
            welcome: "another".into(),
        }))
    );
    drop(server.await.unwrap());
}

async fn answer_with_error(tcp: &mut TcpStream, status: u16, error: ProtoError) {
    let mut request = [0; 4096];
    assert!(tcp.read(&mut request).await.unwrap() > 0);
    let body = error.encode_to_vec();
    let head = format!(
        "HTTP/1.1 {status} X\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    tcp.write_all(&[head.as_bytes(), &body].concat())
        .await
        .unwrap();
}

fn refusal(code: i32, retry_after_ms: u64) -> ProtoError {
    ProtoError {
        code,
        retry_after_ms,
        message: "refused".into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn only_retry_later_and_quota_with_a_wait_are_resent() {
    let (listener, auth) = listen().await;
    let (session, _events) = Session::start(auth, config()).unwrap();
    let server = tokio::spawn(async move {
        let mut ws = accept(&listener, welcome("h", 0)).await;
        for code in [999, Code::Unspecified as i32, Code::QuotaExceeded as i32] {
            let request = recv_rpc(&mut ws).await;
            assert!(
                matches!(request.call, Some(Call::ListTree(_))),
                "a new call, not a resend"
            );
            reply(
                &mut ws,
                request.req_id,
                rpc_response::Result::Error(refusal(code, 0)),
            )
            .await;
        }
        let request = recv_rpc(&mut ws).await;
        assert!(
            matches!(request.call, Some(Call::ListTrash(_))),
            "the refused calls were not resent"
        );
        let quota = refusal(Code::QuotaExceeded as i32, 200);
        reply(&mut ws, request.req_id, rpc_response::Result::Error(quota)).await;
        let asked = Instant::now();
        let resent = recv_rpc(&mut ws).await;
        assert!(
            asked.elapsed() >= Duration::from_millis(200),
            "{:?}",
            asked.elapsed()
        );
        assert_eq!(
            request.call, resent.call,
            "a quota with a wait is resent after it"
        );
        reply(
            &mut ws,
            resent.req_id,
            rpc_response::Result::Trash(Default::default()),
        )
        .await;
        ws
    });
    for code in [999, Code::Unspecified as i32, Code::QuotaExceeded as i32] {
        let refused = session.list_tree("", 1).await;
        assert!(
            matches!(&refused, Err(CallError::Server(error)) if error.0.code == code),
            "{refused:?}"
        );
    }
    session.list_trash("", 1).await.unwrap();
    drop(server.await.unwrap());
}

#[tokio::test]
async fn a_token_refused_with_an_unknown_code_stops_the_session_without_asking_again() {
    let (listener, auth) = listen_on("127.0.0.1:0", Duration::ZERO, Some(Duration::ZERO)).await;
    let (_session, mut events) = Session::start(auth, config()).unwrap();
    let (mut token_request, _) = listener.accept().await.unwrap();
    answer_with_error(&mut token_request, 500, refusal(999, 0)).await;
    assert!(matches!(
        next(&mut events).await,
        Event::Stopped(StopReason::Refused(why)) if why.ends_with(": refused")
    ));
    let again = tokio::time::timeout(Duration::from_millis(300), listener.accept()).await;
    assert!(again.is_err(), "no second token request");
}

#[tokio::test]
async fn an_upgrade_refused_for_a_history_a_rewind_replaced_waits_the_longest_backoff_and_connects_once_served()
 {
    let (listener, auth) = listen().await;
    let (_session, mut events) = Session::start(auth, config()).unwrap();
    let (mut upgrade, _) = listener.accept().await.unwrap();
    answer_with_error(&mut upgrade, 409, refusal(Code::HistoryMismatch as i32, 0)).await;
    let event = next(&mut events).await;
    let Event::Disconnected { reason, retry_in } = &event else {
        panic!("{event:?}");
    };
    assert_eq!(
        reason.to_string(),
        "the server refused the connection: CODE_HISTORY_MISMATCH: refused"
    );
    assert!(*retry_in >= config().backoff.max, "{retry_in:?}");
    let _ws = accept(&listener, welcome("h", 0)).await;
    let event = next(&mut events).await;
    assert!(matches!(event, Event::Connected(_)), "{event:?}");
}

#[tokio::test]
async fn an_upgrade_refused_while_the_server_cannot_check_the_device_retries_at_the_normal_backoff_and_connects_once_served()
 {
    let (listener, auth) = listen().await;
    let (_session, mut events) = Session::start(auth, config()).unwrap();
    let (mut upgrade, _) = listener.accept().await.unwrap();
    answer_in_two_parts(
        &mut upgrade,
        503,
        b"could not check the device; retry later".to_vec(),
    )
    .await;
    let event = next(&mut events).await;
    let Event::Disconnected {
        reason: DisconnectReason::Refused { status: 503 },
        retry_in,
    } = &event
    else {
        panic!("{event:?}");
    };
    assert!(*retry_in < config().backoff.max, "{retry_in:?}");
    let _ws = accept(&listener, welcome("h", 0)).await;
    let event = next(&mut events).await;
    assert!(matches!(event, Event::Connected(_)), "{event:?}");
}

#[tokio::test]
async fn an_upgrade_refused_with_an_unknown_code_stops_the_session_and_a_known_one_reconnects() {
    let (listener, auth) = listen().await;
    let (_session, mut events) = Session::start(auth.clone(), config()).unwrap();
    let (mut upgrade, _) = listener.accept().await.unwrap();
    answer_with_error(&mut upgrade, 500, refusal(999, 0)).await;
    assert!(matches!(
        next(&mut events).await,
        Event::Stopped(StopReason::Refused(why)) if why.ends_with(": refused")
    ));

    let (_session, mut events) = Session::start(auth, config()).unwrap();
    let (mut upgrade, _) = listener.accept().await.unwrap();
    answer_with_error(&mut upgrade, 500, refusal(Code::Internal as i32, 0)).await;
    let event = next(&mut events).await;
    assert!(
        matches!(
            event,
            Event::Disconnected {
                reason: DisconnectReason::Refused { status: 500 },
                ..
            }
        ),
        "{event:?}"
    );
}

#[tokio::test]
async fn a_refresh_refused_with_an_unknown_code_stops_the_session() {
    let (listener, auth) = listen_on(
        "127.0.0.1:0",
        Duration::from_millis(300),
        Some(Duration::ZERO),
    )
    .await;
    let (_session, mut events) = Session::start(auth, config()).unwrap();
    let _ws = accept(&listener, welcome("h", 0)).await;
    assert!(matches!(next(&mut events).await, Event::Connected(_)));
    let (mut refresh, _) = listener.accept().await.unwrap();
    answer_with_error(&mut refresh, 500, refusal(999, 0)).await;
    assert!(matches!(
        next(&mut events).await,
        Event::Stopped(StopReason::Refused(why)) if why.ends_with(": refused")
    ));
}

async fn answer_in_two_parts(tcp: &mut TcpStream, status: u16, body: Vec<u8>) {
    let mut request = [0; 4096];
    assert!(tcp.read(&mut request).await.unwrap() > 0);
    let head = format!(
        "HTTP/1.1 {status} X\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\n\r\n",
        body.len()
    );
    tcp.write_all(head.as_bytes()).await.unwrap();
    tcp.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    tcp.write_all(&body).await.unwrap();
}

#[tokio::test]
async fn an_upgrade_refusal_whose_body_arrives_after_its_head_is_read_whole() {
    let (listener, auth) = listen().await;
    let (_session, mut events) = Session::start(auth.clone(), config()).unwrap();
    let (mut upgrade, _) = listener.accept().await.unwrap();
    answer_in_two_parts(&mut upgrade, 500, refusal(999, 0).encode_to_vec()).await;
    assert!(matches!(
        next(&mut events).await,
        Event::Stopped(StopReason::Refused(why)) if why.ends_with(": refused")
    ));

    let (_session, mut events) = Session::start(auth, config()).unwrap();
    let (mut upgrade, _) = listener.accept().await.unwrap();
    let quota = refusal(Code::QuotaExceeded as i32, 300);
    answer_in_two_parts(&mut upgrade, 429, quota.encode_to_vec()).await;
    match next(&mut events).await {
        Event::Disconnected {
            reason: DisconnectReason::Refused { status: 429 },
            retry_in,
        } => assert!(retry_in >= Duration::from_millis(300), "{retry_in:?}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn an_empty_upgrade_refusal_is_left_to_its_status() {
    let (listener, auth) = listen().await;
    let (_session, mut events) = Session::start(auth, config()).unwrap();
    let (mut upgrade, _) = listener.accept().await.unwrap();
    answer_in_two_parts(&mut upgrade, 503, Vec::new()).await;
    let event = next(&mut events).await;
    assert!(
        matches!(
            event,
            Event::Disconnected {
                reason: DisconnectReason::Refused { status: 503 },
                ..
            }
        ),
        "{event:?}"
    );
}

#[tokio::test]
async fn a_401_with_an_unknown_code_stops_the_session_with_its_message() {
    let (listener, auth) = listen_on("127.0.0.1:0", Duration::ZERO, Some(Duration::ZERO)).await;
    let (_session, mut events) = Session::start(auth, config()).unwrap();
    let (mut token_request, _) = listener.accept().await.unwrap();
    answer_with_error(&mut token_request, 401, refusal(999, 0)).await;
    let event = next(&mut events).await;
    assert!(
        matches!(&event, Event::Stopped(StopReason::Refused(why)) if why.ends_with(": refused")),
        "not CredentialsRefused: {event:?}"
    );

    let (listener, auth) = listen_on(
        "127.0.0.1:0",
        Duration::from_millis(300),
        Some(Duration::ZERO),
    )
    .await;
    let (_session, mut events) = Session::start(auth, config()).unwrap();
    let _ws = accept(&listener, welcome("h", 0)).await;
    assert!(matches!(next(&mut events).await, Event::Connected(_)));
    let (mut refresh, _) = listener.accept().await.unwrap();
    answer_with_error(&mut refresh, 401, refusal(999, 0)).await;
    let event = next(&mut events).await;
    assert!(
        matches!(&event, Event::Stopped(StopReason::Refused(why)) if why.ends_with(": refused")),
        "{event:?}"
    );
}

async fn answer_in_parts(tcp: &mut TcpStream, parts: Vec<Vec<u8>>) -> String {
    let mut request = [0; 4096];
    let n = tcp.read(&mut request).await.unwrap();
    for part in parts {
        tcp.write_all(&part).await.unwrap();
        tcp.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    String::from_utf8_lossy(&request[..n]).into_owned()
}

#[tokio::test]
async fn chunked_and_close_delimited_upgrade_refusals_are_read_whole() {
    let (listener, auth) = listen().await;
    let body = refusal(999, 0).encode_to_vec();
    let (first, rest) = body.split_at(3);
    let chunked = vec![
        b"HTTP/1.1 500 X\r\ncontent-type: application/x-protobuf\r\ntransfer-encoding: chunked\r\n\r\n".to_vec(),
        [format!("{:x}\r\n", first.len()).as_bytes(), first, b"\r\n"].concat(),
        [format!("{:x}\r\n", rest.len()).as_bytes(), rest, b"\r\n0\r\n\r\n"].concat(),
    ];
    let (_session, mut events) = Session::start(auth.clone(), config()).unwrap();
    let (mut upgrade, _) = listener.accept().await.unwrap();
    answer_in_parts(&mut upgrade, chunked).await;
    let event = next(&mut events).await;
    assert!(
        matches!(&event, Event::Stopped(StopReason::Refused(why)) if why.ends_with(": refused")),
        "chunked: {event:?}"
    );

    let (_session, mut events) = Session::start(auth, config()).unwrap();
    let (mut upgrade, _) = listener.accept().await.unwrap();
    let head =
        b"HTTP/1.1 500 X\r\ncontent-type: application/x-protobuf\r\nconnection: close\r\n\r\n"
            .to_vec();
    answer_in_parts(&mut upgrade, vec![head, first.to_vec(), rest.to_vec()]).await;
    drop(upgrade);
    let event = next(&mut events).await;
    assert!(
        matches!(&event, Event::Stopped(StopReason::Refused(why)) if why.ends_with(": refused")),
        "close-delimited: {event:?}"
    );
}

#[tokio::test]
async fn a_refusal_whose_error_cannot_be_read_is_retried_a_bounded_number_of_times_in_a_row() {
    let (listener, auth) = listen().await;
    let (_session, mut events) = Session::start(auth, config()).unwrap();
    let server = tokio::spawn(async move {
        let mut attempts = 0;
        while let Ok(Ok((mut upgrade, _))) =
            tokio::time::timeout(Duration::from_secs(2), listener.accept()).await
        {
            attempts += 1;
            let answer = if attempts == MAX_UNREADABLE_REFUSALS {
                vec![b"HTTP/1.1 500 X\r\ncontent-length: 0\r\n\r\n".to_vec()]
            } else if attempts % 2 == 0 {
                vec![
                    b"HTTP/1.1 500 X\r\ncontent-type: application/x-protobuf\r\ncontent-length: 3\r\n\r\n".to_vec(),
                    vec![0xff; 3],
                ]
            } else {
                vec![
                    b"HTTP/1.1 500 X\r\ncontent-type: application/x-protobuf\r\ncontent-length: 1000\r\n\r\n".to_vec(),
                    vec![0x08; 10],
                ]
            };
            answer_in_parts(&mut upgrade, answer).await;
        }
        attempts
    });
    let stopped = loop {
        match next(&mut events).await {
            Event::Disconnected {
                reason: DisconnectReason::Refused { status: 500 },
                ..
            } => {}
            other => break other,
        }
    };
    assert!(
        matches!(&stopped, Event::Stopped(StopReason::Refused(why)) if why.contains("could not be read")),
        "{stopped:?}"
    );
    assert_eq!(server.await.unwrap(), 2 * MAX_UNREADABLE_REFUSALS);
}

#[tokio::test]
async fn a_101_without_upgrade_and_connection_headers_is_not_a_websocket() {
    let (listener, auth) = listen().await;
    let (_session, mut events) = Session::start(auth, config()).unwrap();
    let (mut upgrade, _) = listener.accept().await.unwrap();
    let mut request = [0; 4096];
    let n = upgrade.read(&mut request).await.unwrap();
    let request = String::from_utf8_lossy(&request[..n]).into_owned();
    let key = request
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("sec-websocket-key")
                .then(|| value.trim().to_owned())
        })
        .unwrap();
    let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes());
    let answer =
        format!("HTTP/1.1 101 Switching Protocols\r\nsec-websocket-accept: {accept}\r\n\r\n");
    upgrade.write_all(answer.as_bytes()).await.unwrap();
    let event = next(&mut events).await;
    assert!(
        matches!(
            event,
            Event::Disconnected {
                reason: DisconnectReason::Transport(_),
                ..
            }
        ),
        "{event:?}"
    );
}

#[tokio::test]
async fn malformed_chunked_refusals_are_left_to_their_status() {
    let (listener, auth) = listen().await;
    let body = refusal(999, 0).encode_to_vec();
    let head = b"HTTP/1.1 503 X\r\ncontent-type: application/x-protobuf\r\ntransfer-encoding: chunked\r\n\r\n";
    let oversized = [b"ffffffffffffffff\r\n".as_slice(), &body].concat();
    let unterminated = [
        format!("{:x}\r\n", body.len()).as_bytes(),
        &body,
        b"\r\n0\r\n",
    ]
    .concat();
    for (case, rest) in [("oversized", oversized), ("unterminated", unterminated)] {
        let (_session, mut events) = Session::start(auth.clone(), config()).unwrap();
        let (mut upgrade, _) = listener.accept().await.unwrap();
        answer_in_parts(&mut upgrade, vec![head.to_vec(), rest]).await;
        drop(upgrade);
        let event = next(&mut events).await;
        assert!(
            matches!(
                event,
                Event::Disconnected {
                    reason: DisconnectReason::Refused { status: 503 },
                    ..
                }
            ),
            "{case}: {event:?}"
        );
    }
}

async fn answer_endlessly(mut tcp: TcpStream, status: u16) {
    let mut request = [0; 4096];
    assert!(tcp.read(&mut request).await.unwrap() > 0);
    let head = format!(
        "HTTP/1.1 {status} X\r\ncontent-type: application/x-protobuf\r\ncontent-length: 1000000000000\r\n\r\n"
    );
    tcp.write_all(head.as_bytes()).await.unwrap();
    let part = [0x0a; 64 << 10];
    while tcp.write_all(&part).await.is_ok() {}
}

#[tokio::test]
async fn an_oversized_token_answer_is_a_transport_failure_and_is_asked_again() {
    let (listener, auth) = listen_on("127.0.0.1:0", Duration::ZERO, Some(Duration::ZERO)).await;
    let (_session, mut events) = Session::start(auth, config()).unwrap();
    let (token_request, _) = listener.accept().await.unwrap();
    tokio::spawn(answer_endlessly(token_request, 200));
    let event = next(&mut events).await;
    assert!(
        matches!(&event, Event::Disconnected { reason: DisconnectReason::Transport(why), .. } if why.step == Step::Token && why.detail.contains("an answer of more than")),
        "{event:?}"
    );
    let again = tokio::time::timeout(Duration::from_secs(2), listener.accept()).await;
    assert!(again.is_ok(), "the token is asked for again");
}

async fn answer_not_an_error(mut tcp: TcpStream) {
    let mut request = [0; 4096];
    assert!(tcp.read(&mut request).await.unwrap() > 0);
    let head =
        b"HTTP/1.1 500 X\r\ncontent-type: application/x-protobuf\r\ncontent-length: 3\r\n\r\n";
    tcp.write_all(&[head.as_slice(), &[0xff; 3]].concat())
        .await
        .unwrap();
}

async fn answer_cut_short(mut tcp: TcpStream) {
    let mut request = [0; 4096];
    assert!(tcp.read(&mut request).await.unwrap() > 0);
    let head =
        b"HTTP/1.1 500 X\r\ncontent-type: application/x-protobuf\r\ncontent-length: 1000\r\n\r\n";
    tcp.write_all(&[head.as_slice(), &[0x08; 10]].concat())
        .await
        .unwrap();
}

async fn settled(events: &mut Events) -> Event {
    let settle = async {
        loop {
            match next(events).await {
                Event::Disconnected { .. } | Event::Connected(_) => {}
                other => return other,
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(10), settle)
        .await
        .expect("the session kept reconnecting for 10 s")
}

#[tokio::test]
async fn unreadable_token_errors_stop_the_session_in_a_row() {
    let unreadable_until_stopped = |listener: TcpListener| async move {
        let mut attempts = 0;
        while let Ok(Ok((tcp, _))) =
            tokio::time::timeout(Duration::from_secs(2), listener.accept()).await
        {
            attempts += 1;
            match attempts % 3 {
                0 => tokio::spawn(answer_endlessly(tcp, 500)),
                1 => tokio::spawn(answer_not_an_error(tcp)),
                _ => tokio::spawn(answer_cut_short(tcp)),
            };
        }
        attempts
    };

    let (listener, auth) = listen_on("127.0.0.1:0", Duration::ZERO, Some(Duration::ZERO)).await;
    let (_session, mut events) = Session::start(auth, config()).unwrap();
    let server = tokio::spawn(unreadable_until_stopped(listener));
    let event = settled(&mut events).await;
    assert!(
        matches!(&event, Event::Stopped(StopReason::Refused(why)) if why.contains("could not be read")),
        "token: {event:?}"
    );
    assert_eq!(server.await.unwrap(), MAX_UNREADABLE_REFUSALS);

    let (listener, auth) = listen_on(
        "127.0.0.1:0",
        Duration::from_millis(300),
        Some(Duration::ZERO),
    )
    .await;
    let (_session, mut events) = Session::start(auth, config()).unwrap();
    let _ws = accept(&listener, welcome("h", 0)).await;
    assert!(matches!(next(&mut events).await, Event::Connected(_)));
    let server = tokio::spawn(unreadable_until_stopped(listener));
    let event = settled(&mut events).await;
    assert!(
        matches!(&event, Event::Stopped(StopReason::Refused(why)) if why.contains("could not be read")),
        "refresh: {event:?}"
    );
    assert_eq!(server.await.unwrap(), MAX_UNREADABLE_REFUSALS);
}
