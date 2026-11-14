// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;
use std::time::Duration;

use crate::support::proxy::{Fault, Proxy};
use crate::support::*;
use koidrive_net::{
    Api, ApiError, Auth, CallError, Credentials, Event, Events, NotResent, StopReason,
};
use koidrive_proto::v1::{
    ClaimRequest, CommitResult, CreateDir, Move, Op, Resume, ResyncRequired, Welcome, error::Code,
    op, op_result, resync_required::Reason, server_info::DataPath,
};

fn mkdir(parent_id: &str, name: &str) -> (String, Op) {
    let node_id = uuidv7();
    let op = Op {
        op: Some(op::Op::CreateDir(CreateDir {
            node_id: node_id.clone(),
            parent_id: parent_id.into(),
            name: name.into(),
            pending_for: String::new(),
        })),
    };
    (node_id, op)
}

fn applied(result: &CommitResult) -> bool {
    result
        .results
        .iter()
        .all(|r| r.code == op_result::Code::Applied as i32)
}

fn queued_welcome(events: &mut Events) -> Welcome {
    while let Some(event) = events.try_next() {
        if let Event::Connected(welcome) = event {
            return welcome;
        }
    }
    panic!("the Welcome was not queued before the answer");
}

async fn tree_names(session: &koidrive_net::Session) -> Vec<String> {
    let mut names = Vec::new();
    let mut cursor = String::new();
    loop {
        let page = session.list_tree(cursor, 2).await.unwrap();
        names.extend(
            page.nodes
                .into_iter()
                .map(|node| node.name)
                .filter(|name| !name.is_empty()),
        );
        if page.next_cursor.is_empty() {
            return names;
        }
        cursor = page.next_cursor;
    }
}

#[tokio::test]
#[ignore = "needs a local Worker; run core/net/scripts/integration-test.sh"]
async fn auth_claims_pairs_refreshes_and_refuses_bad_credentials() {
    let owner = owner().await;
    let info = api().server_info().await.unwrap();
    assert!(info.claimed);
    assert_eq!(info.data_path(), DataPath::Relay);
    assert!(info.proto_min <= koidrive_net::PROTO_MAX && koidrive_net::PROTO_MIN <= info.proto_max);

    let again = ClaimRequest {
        code: "AAAA-AAAA-AAAA".into(),
        user_name: "x".into(),
        device_name: "x".into(),
        platform: "x".into(),
        setup_attempt: String::new(),
        request_id: String::new(),
    };
    match api().claim(&again).await {
        Err(ApiError::Server { status: 403, error }) => assert_eq!(error.code(), Code::Forbidden),
        other => panic!("a second claim must be refused, got {other:?}"),
    }

    let fresh = auth(owner.clone());
    assert!(
        !fresh.access_token().await.unwrap().token.is_empty(),
        "a token from POST /v1/token"
    );
    fresh.api().list_spaces(&fresh).await.unwrap();

    let laptop = auth(new_device("laptop").await);
    assert_eq!(laptop.credentials().user_id, owner.user_id);
    assert_ne!(laptop.credentials().device_id, owner.device_id);
    let space = new_space().await;
    let spaces = laptop.api().list_spaces(&laptop).await.unwrap();
    assert!(
        spaces.spaces.iter().any(|s| s.space_id == space.space_id),
        "a paired device sees the owner's spaces"
    );

    let wrong = auth(Credentials {
        refresh_credential: "not-the-credential".into(),
        ..owner.clone()
    });
    let error = wrong.api().list_spaces(&wrong).await.unwrap_err();
    assert!(error.is_unauthenticated(), "{error:?}");
    let (_session, mut events) = start(wrong, &space);
    assert_eq!(
        until(&mut events, |e| matches!(e, Event::Stopped(_))
            .then(|| e.clone()))
        .await,
        Event::Stopped(StopReason::CredentialsRefused)
    );

    let stale = koidrive_proto::v1::AccessToken {
        token: "v1.unknown.e30.AAAA".into(),
        expires_at_ms: i64::MAX,
        ..Default::default()
    };
    let recovering = Arc::new(Auth::new(api(), owner).with_token(stale));
    let (session, mut events) = start(recovering, &space);
    let refused = until(&mut events, |e| match e {
        Event::Disconnected { reason, .. } => Some(reason.clone()),
        Event::Connected(_) => panic!("a forged token must not connect"),
        _ => None,
    })
    .await;
    assert_eq!(
        refused,
        koidrive_net::DisconnectReason::Refused { status: 401 }
    );
    connected(&mut events).await;
    session.list_tree("", 1).await.unwrap();
}

#[tokio::test]
#[ignore = "needs a local Worker; run core/net/scripts/integration-test.sh"]
async fn a_commit_round_trips_to_another_device() {
    let space = new_space().await;
    let (a, mut a_events) = start(auth(owner().await), &space);
    let (b, mut b_events) = start(auth(new_device("b").await), &space);
    let welcome = connected(&mut a_events).await;
    assert_eq!(
        (welcome.head_seq, welcome.device_seq, welcome.read_only),
        (0, 0, false)
    );
    connected(&mut b_events).await;
    let history = welcome.history_id.clone();
    a.subscribe(Resume {
        history_id: history.clone(),
        since_seq: 0,
        ..Default::default()
    });
    b.subscribe(Resume {
        history_id: history.clone(),
        since_seq: 0,
        ..Default::default()
    });
    until(&mut a_events, |e| {
        matches!(e, Event::CaughtUp(c) if c.head_seq == 0).then_some(())
    })
    .await;
    until(&mut b_events, |e| {
        matches!(e, Event::CaughtUp(c) if c.head_seq == 0).then_some(())
    })
    .await;

    let (docs, op) = mkdir(&space.space_id, "Docs");
    let sent = commit(&history, 0, 1, vec![op]);
    let result = a.commit(sent.clone()).await.unwrap();
    assert_eq!(result.seq, 1);
    assert!(applied(&result), "{result:?}");
    let own = until(&mut a_events, |e| match e {
        Event::Committed(c) => Some(c.clone()),
        _ => None,
    })
    .await;
    assert_eq!(
        (own.commit_id.as_str(), own.result.seq),
        (sent.commit_id.as_str(), 1)
    );
    let pushed = until(&mut b_events, |e| match e {
        Event::Changes(records) => Some(records.clone()),
        _ => None,
    })
    .await;
    assert_eq!(pushed.len(), 1);
    assert_eq!(
        (pushed[0].seq, pushed[0].commit_id.as_str()),
        (1, sent.commit_id.as_str())
    );
    assert_eq!(pushed[0].device_id, owner().await.device_id);

    let rename = Op {
        op: Some(op::Op::Move(Move {
            node_id: docs,
            new_parent_id: space.space_id.clone(),
            new_name: "Papers".into(),
            from: None,
            pending_for: String::new(),
        })),
    };
    let result = b
        .commit(commit(&history, 1, 1, vec![rename]))
        .await
        .unwrap();
    assert_eq!(result.seq, 2);
    let pushed = until(&mut a_events, |e| match e {
        Event::Changes(records) => Some(records.clone()),
        _ => None,
    })
    .await;
    assert_eq!(pushed[0].seq, 2);
    assert_eq!(tree_names(&a).await, ["Papers"]);

    let (_, op) = mkdir(&space.space_id, "Elsewhere");
    let refused = a
        .commit(commit(
            "0190a0b0-0000-7000-8000-000000000000",
            2,
            2,
            vec![op.clone()],
        ))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Some(Code::HistoryMismatch));
    let refused = a
        .commit(commit(&history, 2, 1, vec![op.clone()]))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Some(Code::StaleDeviceState));
    let refused = a
        .commit(commit(&history, 9, 2, vec![op]))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Some(Code::AheadOfServer));
}

#[tokio::test]
#[ignore = "needs a local Worker; run core/net/scripts/integration-test.sh"]
async fn a_connection_dropped_mid_rpc_resends_what_rfc_0002_allows() {
    let space = new_space().await;
    let proxy = Proxy::start(&server_url()).await;
    let through_proxy = Arc::new(Auth::new(
        Api::local_development(proxy.url.clone()).unwrap(),
        owner().await,
    ));
    let (session, mut events) = start(through_proxy, &space);
    let history = connected(&mut events).await.history_id;
    session.subscribe(Resume {
        history_id: history.clone(),
        since_seq: 0,
        ..Default::default()
    });
    until(&mut events, |e| {
        matches!(e, Event::CaughtUp(_)).then_some(())
    })
    .await;

    proxy.arm(Fault::DropOnServerData);
    let (_, op) = mkdir(&space.space_id, "answered but lost");
    let result = session
        .commit(commit(&history, 0, 1, vec![op]))
        .await
        .unwrap();
    assert!(proxy.fired(), "the response was dropped");
    assert_eq!(result.seq, 1);
    let welcome = queued_welcome(&mut events);
    assert_eq!(
        welcome.device_seq, 1,
        "the server had accepted the commit before the drop"
    );

    proxy.arm(Fault::DropOnClientData);
    let (_, op) = mkdir(&space.space_id, "never arrived");
    let result = session
        .commit(commit(&history, 1, 2, vec![op]))
        .await
        .unwrap();
    assert!(proxy.fired(), "the request was dropped");
    assert_eq!(result.seq, 2);
    assert_eq!(
        queued_welcome(&mut events).device_seq,
        1,
        "the first attempt never reached the server"
    );

    proxy.arm(Fault::DropOnServerData);
    let names = tree_names(&session).await;
    assert!(proxy.fired());
    let mut names = names;
    names.sort();
    assert_eq!(
        names,
        ["answered but lost", "never arrived"],
        "each commit was applied once"
    );

    until(&mut events, |e| {
        matches!(e, Event::CaughtUp(c) if c.head_seq == 2).then_some(())
    })
    .await;

    let other = auth(owner().await);
    let (observer, mut observer_events) = start(other, &space);
    assert_eq!(
        connected(&mut observer_events).await.head_seq,
        2,
        "two commits, no duplicate"
    );
    observer.close().await;
    session.close().await;
}

#[tokio::test]
#[ignore = "needs a local Worker; run core/net/scripts/integration-test.sh"]
async fn a_commit_is_not_resent_when_welcome_cannot_account_for_it() {
    let space = new_space().await;
    let proxy = Proxy::start(&server_url()).await;
    let through_proxy = Arc::new(Auth::new(
        Api::local_development(proxy.url.clone()).unwrap(),
        new_device("stale").await,
    ));
    let (session, mut events) = start(through_proxy, &space);
    let history = connected(&mut events).await.history_id;
    proxy.arm(Fault::DropOnClientData);
    let (_, op) = mkdir(&space.space_id, "x");
    let mut stale = commit(&history, 0, 5, vec![op]);
    let outcome = session.commit(stale.clone()).await;
    assert_eq!(
        outcome,
        Err(CallError::NotResent(NotResent::DeviceSeqMismatch {
            slot: 5,
            welcome: 0
        }))
    );
    stale.device_seq = 1;
    stale.history_id = "0190a0b0-0000-7000-8000-000000000000".into();
    proxy.arm(Fault::DropOnClientData);
    let outcome = session.commit(stale).await;
    assert!(
        matches!(
            outcome,
            Err(CallError::NotResent(NotResent::HistoryChanged { .. }))
        ),
        "{outcome:?}"
    );
    assert!(
        tree_names(&session).await.is_empty(),
        "nothing was committed"
    );
}

#[tokio::test]
#[ignore = "needs a local Worker; run core/net/scripts/integration-test.sh"]
async fn resync_required_then_a_bootstrap_resumes_the_stream() {
    let space = new_space().await;
    let (writer, mut writer_events) = start(auth(owner().await), &space);
    let history = connected(&mut writer_events).await.history_id;
    for (i, name) in ["a", "b", "c"].into_iter().enumerate() {
        let (_, op) = mkdir(&space.space_id, name);
        writer
            .commit(commit(&history, i as u64, i as u64 + 1, vec![op]))
            .await
            .unwrap();
    }

    let (session, mut events) = start(auth(new_device("reader").await), &space);
    connected(&mut events).await;
    let resync = |e: &Event| match e {
        Event::ResyncRequired(r) => Some(*r),
        _ => None,
    };
    session.subscribe(Resume {
        history_id: history.clone(),
        since_seq: 9,
        ..Default::default()
    });
    let ahead = until(&mut events, resync).await;
    assert_eq!(ahead.reason(), Reason::AheadOfServer);
    session.subscribe(Resume {
        history_id: "0190a0b0-0000-7000-8000-000000000000".to_string(),
        since_seq: 1,
        ..Default::default()
    });
    let changed = until(&mut events, resync).await;
    assert_eq!(
        changed,
        ResyncRequired {
            reason: Reason::HistoryChanged as i32,
            restore_seq: 0,
            restore_point_ms: 0
        }
    );

    let first = session.list_tree("", 2).await.unwrap();
    let start_seq = first.head_seq;
    assert_eq!(start_seq, 3);
    let mut cursor = first.next_cursor;
    let mut count = first.nodes.len();
    while !cursor.is_empty() {
        let page = session.list_tree(cursor, 2).await.unwrap();
        count += page.nodes.len();
        cursor = page.next_cursor;
    }
    assert_eq!(count, 4, "the root and three folders");

    let (_, op) = mkdir(&space.space_id, "d");
    writer
        .commit(commit(&history, 3, 4, vec![op]))
        .await
        .unwrap();
    session.subscribe(Resume {
        history_id: history.clone(),
        since_seq: start_seq,
        since_commit_id: first.head_commit_id.clone(),
        since_committed_at_ms: first.head_committed_at_ms,
    });
    let replayed = until(&mut events, |e| match e {
        Event::Changes(records) => Some(records.iter().map(|r| r.seq).collect::<Vec<_>>()),
        _ => None,
    })
    .await;
    assert_eq!(replayed, [4], "the log after the first page's head_seq");
    until(&mut events, |e| {
        matches!(e, Event::CaughtUp(c) if c.head_seq == 4).then_some(())
    })
    .await;
}

#[tokio::test]
#[ignore = "needs a local Worker; run core/net/scripts/integration-test.sh"]
async fn reauth_replaces_the_token_without_dropping_the_connection() {
    let space = new_space().await;
    let proxy = Proxy::start(&server_url()).await;
    let margin = Duration::from_secs(15 * 60) - Duration::from_secs(2);
    let refreshing = Arc::new(
        Auth::new(
            Api::local_development(proxy.url.clone()).unwrap(),
            owner().await,
        )
        .with_refresh_margin(margin),
    );
    let (session, mut events) = start(refreshing, &space);
    connected(&mut events).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let before = proxy.websocket_client_bytes();
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(
        proxy.websocket_client_bytes() > before,
        "Reauth frames went out; nothing else was sent"
    );
    session.list_tree("", 1).await.unwrap();
    session.close().await;
    while let Some(event) = events.next().await {
        assert!(
            !matches!(event, Event::Disconnected { .. }),
            "the server accepted every Reauth: {event:?}"
        );
    }
}
