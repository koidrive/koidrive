// SPDX-License-Identifier: Apache-2.0

fn server_id(n: u128) -> String {
    koidrive_engine::ids::uuid_v7(1, n)
}

fn from_trash(node: u128) -> RestoreRequest {
    RestoreRequest::FromTrash {
        node_id: server_id(node),
        parent_id: None,
        name: None,
    }
}

#[test]
fn a_restore_request_survives_a_reopen_and_reads_as_pending() {
    let (_dir, paths, _root) = profile(&[]);
    let driver = Driver::open(paths.clone()).unwrap();
    let mut watch = driver.watch();
    watch.mark_unchanged();
    let placed = RestoreRequest::FromTrash {
        node_id: server_id(1),
        parent_id: Some(server_id(2)),
        name: Some("back.txt".into()),
    };
    let version = RestoreRequest::Version {
        node_id: server_id(3),
        version_id: server_id(4),
        saved_at_ms: Some(1_794_000_000_000),
    };
    assert_eq!(driver.restores(SPACE).unwrap(), []);
    let first = driver
        .restore(SPACE, placed.clone(), Seen::default())
        .unwrap();
    assert!(watch.has_changed().unwrap());
    std::thread::sleep(Duration::from_millis(2));
    let second = driver
        .restore(SPACE, version.clone(), Seen::default())
        .unwrap();
    assert!(first < second, "the IDs sort in request order");
    drop(driver);

    let driver = Driver::open(paths).unwrap();
    let pending = |id: &str, request: &RestoreRequest| RestoreStatus {
        id: id.to_owned(),
        path: None,
        asked_at_ms: koidrive_engine::ids::unix_ms(id).unwrap() as i64,
        request: request.clone(),
        state: RestoreState::Pending { waits_for: None },
    };
    assert_eq!(
        driver.restores(SPACE).unwrap(),
        [pending(&first, &placed), pending(&second, &version)]
    );
}

#[test]
fn restore_refuses_a_malformed_id_and_an_unlinked_space() {
    let (_dir, paths, _root) = profile(&[]);
    let driver = Driver::open(paths).unwrap();
    let other_space = "0190f0a0-0000-7000-8000-000000000002";
    assert!(matches!(
        driver.restore(other_space, from_trash(1), Seen::default()),
        Err(DriverError::SpaceNotLinked(space)) if space == other_space
    ));
    assert!(matches!(
        driver.restores(other_space),
        Err(DriverError::SpaceNotLinked(_))
    ));
    let version = |node_id: &str, version_id: &str| RestoreRequest::Version {
        node_id: node_id.into(),
        version_id: version_id.into(),
        saved_at_ms: None,
    };
    let placed = |parent_id: &str| RestoreRequest::FromTrash {
        node_id: server_id(1),
        parent_id: Some(parent_id.into()),
        name: None,
    };
    let cases = [
        (version("report.txt", &server_id(2)), "node_id"),
        (version(&server_id(1), ""), "version_id"),
        (version(&server_id(1), "v2"), "version_id"),
        (
            placed("0190F3A0-1234-7FFF-BFFF-FFFFFFFFFFFF"),
            "target_parent_id",
        ),
    ];
    for (request, field) in cases {
        assert!(matches!(
            driver.restore(SPACE, request, Seen::default()),
            Err(DriverError::InvalidRestore(found)) if found == field
        ));
    }
    assert_eq!(
        driver.restores(SPACE).unwrap(),
        [],
        "a commit with a malformed ID would halt the space, so none is recorded"
    );
}

#[test]
fn restores_lists_what_ended_and_then_what_waits() {
    use koidrive_engine::restore::{Outcome, Rejection, Resolved, RestoreIntent};

    let (_dir, paths, _root) = profile(&[]);
    let driver = Driver::open(paths.clone()).unwrap();
    let in_inbox = driver
        .restore(SPACE, from_trash(9), Seen::default())
        .unwrap();
    let intent = |node: u128| RestoreIntent::from_trash(server_id(node), None, None).unwrap();
    let outcomes = [
        (Outcome::Restored, RestoreState::Restored),
        (
            Outcome::Renamed { name: "b".into() },
            RestoreState::Renamed { name: "b".into() },
        ),
        (
            Outcome::Copied {
                node: server_id(50),
                name: "c".into(),
            },
            RestoreState::Copied {
                node_id: server_id(50),
                name: "c".into(),
            },
        ),
        (
            Outcome::Rejected(Rejection::NotFound),
            RestoreState::Rejected {
                reason: RestoreRejection::NotFound,
            },
        ),
        (
            Outcome::Rejected(Rejection::ReadOnly),
            RestoreState::Rejected {
                reason: RestoreRejection::ReadOnly,
            },
        ),
        (
            Outcome::Rejected(Rejection::Other("LATER".into())),
            RestoreState::Rejected {
                reason: RestoreRejection::Other("LATER".into()),
            },
        ),
        (Outcome::DoesNotFit, RestoreState::DoesNotFit),
    ];
    let mut state = koidrive_store::EngineState::default();
    state.commits.restores.insert("queued".into(), intent(8));
    for (n, (outcome, _)) in outcomes.iter().enumerate() {
        state.commits.resolved.push(Resolved {
            id: format!("ended-{}", 9 - n),
            intent: intent(n as u128),
            outcome: outcome.clone(),
        });
    }
    let path = paths.store(SPACE).unwrap();
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&state))
        .unwrap();

    let listed = driver.restores(SPACE).unwrap();
    let ids: Vec<&str> = listed.iter().map(|restore| restore.id.as_str()).collect();
    let ended = (0..outcomes.len()).map(|n| format!("ended-{}", 9 - n));
    let expected: Vec<String> = ended.chain([in_inbox, "queued".into()]).collect();
    assert_eq!(ids, expected);
    for (restore, (_, state)) in listed.iter().zip(&outcomes) {
        assert_eq!(&restore.state, state);
    }
    assert_eq!(listed[1].request, from_trash(1));
    for waiting in &listed[outcomes.len()..] {
        assert_eq!(waiting.state, RestoreState::Pending { waits_for: None });
    }
}

#[test]
fn restores_name_the_path_of_their_node_and_when_they_were_asked_for() {
    use koidrive_proto::v1::{Content, NodeKind, NodeState, NodeStatus, VersionInfo};

    let (_dir, paths, _root) = profile(&[]);
    let node = |id: u128, parent: &str, name: &str, kind: NodeKind, status: NodeStatus| NodeState {
        node_id: server_id(id),
        parent_id: parent.to_owned(),
        name: name.to_owned(),
        kind: kind as i32,
        status: status as i32,
        trash_group: match status {
            NodeStatus::Trashed => "group".into(),
            _ => String::new(),
        },
        current: Some(VersionInfo {
            version_id: server_id(100 + id),
            content: Some(Content::default()),
            ..VersionInfo::default()
        }),
        ..NodeState::default()
    };
    let docs = server_id(1);
    let nodes = [
        node(1, SPACE, "Docs", NodeKind::Dir, NodeStatus::Live),
        node(2, &docs, "report.docx", NodeKind::File, NodeStatus::Live),
        node(3, &docs, "old.pptx", NodeKind::File, NodeStatus::Trashed),
        node(
            4,
            &server_id(5),
            "orphan.txt",
            NodeKind::File,
            NodeStatus::Trashed,
        ),
    ];
    Store::open(paths.store(SPACE).unwrap())
        .unwrap()
        .transaction(|tx| nodes.iter().try_for_each(|node| tx.put_remote_node(node)))
        .unwrap();
    let driver = Driver::open(paths).unwrap();
    let before = now_ms();
    let version = RestoreRequest::Version {
        node_id: server_id(2),
        version_id: server_id(200),
        saved_at_ms: None,
    };
    let asked: Vec<String> = [
        (version, Some("Earlier/report.docx")),
        (from_trash(3), None),
        (from_trash(4), Some("Before the first sync/orphan.txt")),
        (from_trash(9), None),
    ]
    .into_iter()
    .map(|(request, path)| {
        driver
            .restore(
                SPACE,
                request,
                Seen {
                    path: path.map(str::to_owned),
                    ..Seen::default()
                },
            )
            .unwrap()
    })
    .collect();
    let after = now_ms();

    let listed = driver.restores(SPACE).unwrap();
    let found: Vec<Option<&str>> = (asked.iter())
        .map(|id| {
            let restore = listed.iter().find(|restore| &restore.id == id).unwrap();
            restore.path.as_ref().map(koidrive_fs::ItemPath::unquoted)
        })
        .collect();
    assert_eq!(
        found,
        [
            Some("Docs/report.docx"),
            Some("Docs/old.pptx"),
            Some("Before the first sync/orphan.txt"),
            None
        ]
    );
    for restore in &listed {
        assert!(
            (before..=after).contains(&restore.asked_at_ms),
            "{restore:?} was asked for between {before} and {after}"
        );
    }
}

#[tokio::test]
async fn a_restore_asked_for_while_the_folder_runs_reaches_its_engine() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &base.join("Koidrive"), "d1");
    let driver = Driver::open(paths.clone()).unwrap();
    driver.start();
    eventually("the folder's task runs its loop", || {
        driver.status().folders[0].state == FolderState::Offline
    })
    .await;

    let id = driver
        .restore(SPACE, from_trash(1), Seen::default())
        .unwrap();
    let path = paths.store(SPACE).unwrap();
    eventually("the engine holds the request", || {
        let (inbox, (queued, _)) = Store::open(&path)
            .unwrap()
            .transaction(|tx| {
                Ok::<_, koidrive_store::Error>((tx.restore_requests()?, tx.restores()?))
            })
            .unwrap();
        inbox.is_empty() && queued.contains_key(&id)
    })
    .await;
    let listed = driver.restores(SPACE).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].state, RestoreState::Pending { waits_for: None });
    driver.stop().await;
}

#[tokio::test]
async fn a_paused_folder_no_longer_says_it_cannot_reach_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let paths = Paths::under(&base.join("profile"));
    linked(&paths, &base.join("Koidrive"), "d1");
    let driver = Driver::open(paths.clone()).unwrap();
    driver.start();
    eventually("the folder shows that it cannot reach the server", || {
        let folder = &driver.status().folders[0];
        folder.unreachable().is_some()
    })
    .await;
    assert!(driver.pause().await.unwrap());
    let folder = driver.status().folders[0].clone();
    assert_eq!(folder.state, FolderState::Stopped);
    assert_eq!(folder.wait, None, "paused: {folder:?}");
    driver.stop().await;
}

#[test]
fn a_restore_held_by_a_problem_node_reads_as_pending_with_the_reason() {
    use koidrive_engine::restore::RestoreIntent;

    let (_dir, paths, _root) = profile(&[]);
    let driver = Driver::open(paths.clone()).unwrap();
    let mut state = koidrive_store::EngineState::default();
    state.commits.problems.insert(server_id(1));
    let held = RestoreIntent::version(server_id(1), server_id(40)).unwrap();
    let free = RestoreIntent::from_trash(server_id(2), None, None).unwrap();
    state.commits.restores.insert("a-held".into(), held);
    state.commits.restores.insert("b-free".into(), free);
    let path = paths.store(SPACE).unwrap();
    Store::open(&path)
        .unwrap()
        .transaction(|tx| tx.set_engine_state(&state))
        .unwrap();
    let asked = driver
        .restore(SPACE, from_trash(1), Seen::default())
        .unwrap();

    let listed = driver.restores(SPACE).unwrap();
    let states: Vec<_> = listed.iter().map(|r| (r.id.as_str(), &r.state)).collect();
    let problem = RestoreState::Pending {
        waits_for: Some(RestoreWait::NodeProblem),
    };
    let free = RestoreState::Pending { waits_for: None };
    assert_eq!(
        states,
        [
            (asked.as_str(), &problem),
            ("a-held", &problem),
            ("b-free", &free)
        ]
    );
    assert_eq!(
        serde_json::to_value(&problem).unwrap(),
        serde_json::json!({ "type": "pending", "waits_for": "node_problem" })
    );
}

#[test]
fn a_question_stored_without_its_limits_names_them() {
    let decoded = |deletions, affected_files, space_files| {
        let question = Question {
            recovery_cause: None,
            id: "q1".into(),
            space_id: SPACE.into(),
            asked_at_ms: 1,
            kind: QuestionKind::MassDelete {
                deletions,
                affected_files,
                space_files,
                exceeded: Vec::new(),
            },
            items: Vec::new(),
            lineage: "q1".into(),
        };
        let body = serde_json::to_string(&question).unwrap();
        let stored = StoredQuestion {
            question_id: "q1".into(),
            asked_at_ms: 1,
            body: body.replace(r#","exceeded":[]"#, "").into_bytes(),
            answer: None,
        };
        match decode_question(SPACE, &stored).unwrap().kind {
            QuestionKind::MassDelete { exceeded, .. } => exceeded,
            kind => panic!("a mass-delete question: {kind:?}"),
        }
    };
    assert_eq!(
        decoded(1_200, 1_500, 2_000),
        [Limit::Deletions, Limit::Share]
    );
    assert_eq!(decoded(3, 30, 40), [Limit::Share]);
    assert_eq!(decoded(3, 3, 40), [Limit::Deletions]);
}
