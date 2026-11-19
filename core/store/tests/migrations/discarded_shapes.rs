// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn a_store_from_before_discarded_shapes_were_recorded_takes_them_from_the_waiting_update_or_s() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut before = engine_state();
    let in_s = engine_shape("root", "d.txt");
    let waiting = engine_shape("root", "w.txt");
    before.trees.discarded = BTreeMap::from([
        ("d".into(), in_s.clone()),
        ("w".into(), waiting.clone()),
        ("l".into(), engine_shape("root", "l.txt")),
        ("gone".into(), engine_shape("root", "gone.txt")),
        ("lost".into(), engine_shape("root", "lost.txt")),
    ]);
    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            tx.put_synced_node(&discarded_in_s())?;
            tx.put_synced_node(&SyncedNode {
                node_id: "w".into(),
                shape: file_shape("root", "w-before.txt"),
                pending_for: String::new(),
            })?;
            for (node_id, name) in [("d", "d-edited.txt"), ("l", "l.txt")] {
                tx.put_local_node(&LocalNode {
                    left_out_when_placed: false,
                    content_from_server: false,
                    node_id: node_id.into(),
                    shape: file_shape("root", name),
                    identity: FileId::stored(1, Some(0), None),
                    parked: false,
                })?;
            }
            tx.set_engine_state(&before)
        })
        .unwrap();
    store_before(&path, "discarded_shapes");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    let mut after = store.transaction(|tx| tx.engine_state()).unwrap();
    let asked = after.commits.questions.pop().unwrap();
    assert_eq!(
        (asked.kind, asked.answer),
        (
            QuestionKind::Recovery {
                group: RecoveryGroup::CreatedOnThisDevice,
                sent_in: None,
                cause: None,
            },
            None
        )
    );
    assert_eq!(
        asked.held,
        BTreeMap::from([(
            "l".into(),
            Held {
                members: BTreeMap::from([("l".into(), None)]),
                files: 1,
            }
        )])
    );
    assert_eq!(
        asked.shown,
        BTreeMap::from([("l".into(), engine_shape("root", "l.txt"))])
    );
    before.trees.discarded = BTreeMap::from([("d".into(), in_s), ("w".into(), waiting)]);
    assert_eq!(after, before);
}

#[test]
fn a_file_discarded_before_shapes_were_recorded_that_s_no_longer_holds_is_asked_about_again() {
    for edited in [false, true] {
        a_discarded_file_that_s_no_longer_holds_is_asked_about_again(edited);
    }
}

fn a_discarded_file_that_s_no_longer_holds_is_asked_about_again(edited: bool) {
    use koidrive_engine::classify::Trees;
    use koidrive_engine::effects::Effects;
    use koidrive_engine::ids::IdGen;
    use koidrive_engine::tree::{LocalNode as Entry, RemoteNode};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut trees = Trees::new("root");
    let identity = FileIdentity {
        ino: 1,
        birth_ns: Some(0),
        handle: None,
    };
    trees.set_local(
        "secret".into(),
        Entry {
            left_out_when_placed: false,
            content_from_server: false,
            shape: engine_shape("root", "secret.txt"),
            identity,
            parked: false,
        },
    );
    trees.set_remote(
        "other".into(),
        RemoteNode {
            shape: engine_shape("root", "other.txt"),
            trashed: false,
            meta_rev: 1,
            meta_edit_rev: 1,
            content_rev: 1,
            trash_group: String::new(),
            version_id: String::new(),
            made_over: Vec::new(),
            edit_rev: 1,
            pending_for: None,
            pending_by: None,
        },
    );
    trees.settle(&Default::default());
    let mut commits = Commits::default();
    let mut ids = IdGen::new(333);
    commits.first_link(&mut trees, &mut ids, 1, PriorSync::Shown);
    let question = commits.questions()[0].id.clone();
    assert!(commits.decide(&mut trees, &question, Answer::Discard));
    assert!(
        commits
            .build(&mut trees, &mut ids, 1, "history", "me", 1)
            .is_none()
    );
    trees.forget_lost(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(),
        &Default::default(),
        0,
        0,
    );
    trees.settle(commits.aliases());
    assert!(trees.synced().get(&"secret".into()).is_none());
    assert!(!trees.waiting().contains_key(&"secret".into()));
    let mut now = file_shape("root", "secret.txt");
    if edited {
        now.content = Some(content(6));
        let mut entry = trees.local().get(&"secret".into()).unwrap().clone();
        entry.shape.content = Some(content(6));
        trees.set_local("secret".into(), entry);
    }
    let before = EngineState {
        trees: trees.state(),
        commits: commits.state(),
        effects: EffectsState::default(),
    };
    Store::open(&path)
        .unwrap()
        .transaction(|tx| {
            tx.put_local_node(&LocalNode {
                left_out_when_placed: false,
                content_from_server: false,
                node_id: "secret".into(),
                shape: now,
                identity: FileId::stored(1, Some(0), None),
                parked: false,
            })?;
            tx.set_engine_state(&before)
        })
        .unwrap();
    store_before(&path, "discarded_shapes");

    let after = (Store::open(&path).unwrap())
        .transaction(|tx| tx.engine_state())
        .unwrap();
    trees.restore(after.trees).unwrap();
    trees.rules_scanned();
    let mut commits = Commits::from_state(after.commits).unwrap();
    commits.hold(&mut trees);
    let built = commits
        .build(&mut trees, &mut ids, 1, "history", "me", 1)
        .cloned();
    assert!(built.is_none(), "a discarded file was uploaded: {built:?}");
    let mut effects = Effects::default();
    while let Some(record) = effects
        .plan(&trees, commits.in_flight(), &mut ids, 1, BatchLimit::ONE)
        .first()
        .cloned()
    {
        assert_ne!(record.node, "secret", "edited: {edited}, {record:?}");
        let made = FileIdentity { ino: 3, ..identity };
        effects
            .done(
                &mut trees,
                commits.in_flight(),
                &record.effect_id,
                Some(made),
            )
            .unwrap();
        trees.settle(commits.aliases());
    }
    assert!(trees.held(&"secret".into()));
    let asked = commits.questions().last().unwrap();
    assert_eq!(
        asked.shown[&"secret".into()].content,
        trees.local().get(&"secret".into()).unwrap().shape.content
    );
    let answer = if edited {
        Answer::Publish
    } else {
        Answer::Discard
    };
    assert!(commits.decide(&mut trees, &asked.id.clone(), answer));
    let built = commits
        .build(&mut trees, &mut ids, 1, "history", "me", 1)
        .cloned();
    let created = built.is_some_and(|commit| {
        (commit.ops.iter()).any(|operation| {
            matches!(&operation.op, Some(op::Op::CreateFile(create)) if create.node_id == "secret")
        })
    });
    assert_eq!(created, edited);
    if !edited {
        let removed = effects
            .plan(&trees, commits.in_flight(), &mut ids, 1, BatchLimit::ONE)
            .first()
            .cloned()
            .unwrap();
        assert_eq!(removed.node, "secret");
        assert!(matches!(removed.effect, Effect::RemoveFile { .. }));
    }
}

#[test]
fn a_discarded_folder_is_not_sent_while_a_question_asks_again_about_what_it_holds() {
    use koidrive_engine::classify::Trees;
    use koidrive_engine::effects::Effects;
    use koidrive_engine::ids::IdGen;
    use koidrive_engine::tree::{LocalNode as Entry, RemoteNode};

    for answer in [Answer::Discard, Answer::Publish] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let mut trees = Trees::new("root");
        let folder = EngineShape {
            parent: "root".into(),
            name: "dir".into(),
            kind: Kind::Dir,
            content: None,
        };
        for (ino, (id, shape)) in [("dir", folder), ("c", engine_shape("dir", "c.txt"))]
            .into_iter()
            .enumerate()
        {
            let identity = FileIdentity {
                ino: ino as u128 + 1,
                birth_ns: Some(0),
                handle: None,
            };
            let entry = Entry {
                left_out_when_placed: false,
                content_from_server: false,
                shape,
                identity,
                parked: false,
            };
            trees.set_local(id.into(), entry);
        }
        trees.set_remote(
            "other".into(),
            RemoteNode {
                shape: engine_shape("root", "other.txt"),
                trashed: false,
                meta_rev: 1,
                meta_edit_rev: 1,
                content_rev: 1,
                trash_group: String::new(),
                version_id: String::new(),
                made_over: Vec::new(),
                edit_rev: 1,
                pending_for: None,
                pending_by: None,
            },
        );
        trees.settle(&Default::default());
        let mut commits = Commits::default();
        let mut ids = IdGen::new(335);
        commits.first_link(&mut trees, &mut ids, 1, PriorSync::Shown);
        let question = commits.questions()[0].id.clone();
        assert!(commits.decide(&mut trees, &question, Answer::Discard));
        assert!(
            commits
                .build(&mut trees, &mut ids, 1, "history", "me", 1)
                .is_none()
        );
        trees.set_synced("c".into(), None);
        trees.settle(commits.aliases());
        assert!(trees.synced().get(&"dir".into()).is_some());
        assert!(trees.synced().get(&"c".into()).is_none());
        let before = EngineState {
            trees: trees.state(),
            commits: commits.state(),
            effects: EffectsState::default(),
        };
        Store::open(&path)
            .unwrap()
            .transaction(|tx| {
                tx.put_synced_node(&SyncedNode {
                    node_id: "dir".into(),
                    shape: dir_shape("root", "dir"),
                    pending_for: String::new(),
                })?;
                for (ino, (id, shape)) in [
                    ("dir", dir_shape("root", "dir")),
                    ("c", file_shape("dir", "c.txt")),
                ]
                .into_iter()
                .enumerate()
                {
                    tx.put_local_node(&LocalNode {
                        left_out_when_placed: false,
                        content_from_server: false,
                        node_id: id.into(),
                        shape,
                        identity: FileId::stored(ino as u128 + 1, Some(0), None),
                        parked: false,
                    })?;
                }
                tx.set_engine_state(&before)
            })
            .unwrap();
        store_before(&path, "discarded_shapes");

        let after = (Store::open(&path).unwrap())
            .transaction(|tx| tx.engine_state())
            .unwrap();
        trees.restore(after.trees).unwrap();
        trees.rules_scanned();
        let mut commits = Commits::from_state(after.commits).unwrap();
        commits.hold(&mut trees);
        assert!(trees.held(&"c".into()) && !trees.held(&"dir".into()));
        let built = commits
            .build(&mut trees, &mut ids, 1, "history", "me", 1)
            .cloned();
        assert!(built.is_none(), "sent before the answer: {built:?}");
        let mut effects = Effects::default();
        if let Some(record) = effects
            .plan(&trees, commits.in_flight(), &mut ids, 1, BatchLimit::ONE)
            .first()
        {
            assert!(record.node != "dir" && record.node != "c", "{record:?}");
        }

        let asked = commits.questions().last().unwrap().id.clone();
        assert!(commits.decide(&mut trees, &asked, answer));
        let sent: Vec<String> = commits
            .build(&mut trees, &mut ids, 1, "history", "me", 1)
            .into_iter()
            .flat_map(|commit| &commit.ops)
            .filter_map(|op| match &op.op {
                Some(op::Op::CreateDir(create)) => Some(create.node_id.clone()),
                Some(op::Op::CreateFile(create)) => Some(create.node_id.clone()),
                _ => None,
            })
            .collect();
        let expected: &[&str] = match answer {
            Answer::Publish => &["dir", "c"],
            _ => &[],
        };
        assert_eq!(sent, expected, "{answer:?}");
    }
}
