// SPDX-License-Identifier: Apache-2.0

fn auth_at(base: &str, token: bool) -> Arc<Auth> {
    let credentials = Credentials {
        user_id: "u".into(),
        device_id: "d".into(),
        refresh_credential: "r".into(),
    };
    let lifetime = if token {
        Duration::from_secs(3600)
    } else {
        Duration::ZERO
    };
    let expires = SystemTime::now() + lifetime;
    let token = AccessToken {
        token: "token".into(),
        expires_at_ms: expires.duration_since(UNIX_EPOCH).unwrap().as_millis() as i64,
        ..Default::default()
    };
    let api = Api::local_development(base.parse().unwrap()).unwrap();
    let auth = Auth::new(api, credentials)
        .with_token(token)
        .with_refresh_margin(Duration::ZERO);
    Arc::new(auth)
}

async fn first_disconnect(auth: Arc<Auth>) -> DisconnectReason {
    let (_session, mut events) = Session::start(auth, config()).unwrap();
    match next(&mut events).await {
        Event::Disconnected { reason, .. } => reason,
        other => panic!("expected a disconnect, got {other:?}"),
    }
}

async fn serve_once(answer: &'static [u8]) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut tcp, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let _ = tcp.read(&mut request).await;
        let _ = tcp.write_all(answer).await;
    });
    address
}

#[tokio::test]
async fn each_way_a_connection_fails_names_its_step_and_cause() {
    let reset = koidrive_test_support::reset_tcp_address();
    let plain_http = serve_once(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n\r\n").await;
    let hung_up = serve_once(b"").await;
    let token_refused = serve_once(b"HTTP/1.1 503 Unavailable\r\ncontent-length: 0\r\n\r\n").await;
    let port_zero = "http://127.0.0.1:0".to_owned();
    let port_zero_cause = if cfg!(target_os = "macos") {
        "unreachable"
    } else {
        "refused"
    };
    let cases = [
        (
            port_zero.clone(),
            true,
            format!("connect: {port_zero_cause}"),
        ),
        (format!("http://{reset}"), true, "upgrade: reset".to_owned()),
        (
            "https://koidrive-test.invalid".to_owned(),
            true,
            "connect: dns".to_owned(),
        ),
        (port_zero, false, format!("token: {port_zero_cause}")),
        (format!("http://{reset}"), false, "token: reset".to_owned()),
        (
            format!("http://{token_refused}"),
            false,
            "token: http 503".to_owned(),
        ),
        (format!("https://{plain_http}"), true, "tls: tls".to_owned()),
        (
            format!("http://{hung_up}"),
            true,
            "upgrade: reset".to_owned(),
        ),
    ];
    for (base, token, kind) in cases {
        let reason = first_disconnect(auth_at(&base, token)).await;
        assert_eq!(reason.kind(), kind, "{base}: {reason:?}");
        assert!(
            reason.unreachable() != kind.ends_with("http 503"),
            "{reason:?}"
        );
        assert!(!reason.detail().is_empty(), "{reason:?}");
    }
}

#[test]
fn a_disconnect_reads_in_plain_words() {
    let transport = |cause, detail: &str| {
        DisconnectReason::Transport(Transport {
            step: Step::Connect,
            cause,
            detail: detail.into(),
        })
    };
    let shown = [
        (
            transport(Cause::Dns, "x"),
            "the server's address could not be found",
        ),
        (
            transport(Cause::Certificate, "x"),
            "the server's certificate is not trusted",
        ),
        (
            transport(Cause::Other, "the stored credential is unreadable"),
            "the stored credential is unreadable",
        ),
        (
            DisconnectReason::Refused { status: 502 },
            "the server answered HTTP 502",
        ),
        (
            DisconnectReason::Closed {
                code: Some(1011),
                reason: "overloaded".into(),
            },
            "the server closed the connection (code 1011)",
        ),
        (
            DisconnectReason::Timeout,
            "the server did not answer in time",
        ),
    ];
    for (reason, words) in shown {
        assert_eq!(reason.to_string(), words);
    }
    let certificate = io::Error::new(
        io::ErrorKind::InvalidData,
        rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer),
    );
    let transport = Transport::of(Step::Tls, &certificate);
    assert_eq!(transport.cause, Cause::Certificate);
    assert!(transport.detail.contains("UnknownIssuer"), "{transport:?}");
    let refused = io::Error::from(io::ErrorKind::ConnectionRefused);
    let transport = Transport::of(Step::Connect, &refused);
    assert_eq!(transport.cause, Cause::Refused);
}

#[tokio::test]
async fn the_first_retry_after_a_working_connection_is_quick_and_the_next_backs_off() {
    let (listener, auth) = listen().await;
    let mut config = config();
    config.ping_interval = Duration::from_millis(50);
    config.backoff = Backoff {
        initial: Duration::from_millis(400),
        max: Duration::from_secs(60),
    };
    let (session, mut events) = Session::start(auth, config).unwrap();
    let silent = accept(&listener, welcome("h", 0)).await;
    assert!(matches!(next(&mut events).await, Event::Connected(_)));
    let Event::Disconnected {
        reason: DisconnectReason::Timeout,
        retry_in,
    } = next(&mut events).await
    else {
        panic!("the unanswered pings time out");
    };
    assert!(retry_in <= Duration::from_millis(400), "{retry_in:?}");

    drop(silent);
    let (event, _) = tokio::join!(next(&mut events), async {
        let (mut tcp, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        assert!(tcp.read(&mut request).await.unwrap() > 0);
    });
    let Event::Disconnected { reason, retry_in } = event else {
        panic!("the next attempt fails");
    };
    assert_eq!(reason.kind(), "upgrade: reset");
    assert!(retry_in >= Duration::from_millis(400), "{retry_in:?}");
    session.close().await;
}

#[test]
fn a_refusal_or_confirmation_from_an_earlier_connection_leaves_the_current_one_alone() {
    let bundles = Bundles::default();
    bundles.welcome(true);
    let first = bundles.connection();
    bundles.welcome(true);
    let second = bundles.connection();
    assert_ne!(first, second);
    bundles.refuse(first);
    bundles.confirm(first);
    assert!(bundles.offered());
    assert!(!bundles.confirmed());
    bundles.confirm(second);
    assert!(bundles.confirmed());
    bundles.refuse(second);
    assert!(!bundles.offered());
    bundles.welcome(true);
    assert!(bundles.offered() && !bundles.confirmed());
    bundles.welcome(false);
    assert!(!bundles.offered());
}
