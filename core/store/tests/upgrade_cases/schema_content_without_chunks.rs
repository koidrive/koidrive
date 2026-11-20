// SPDX-License-Identifier: Apache-2.0

upgrade_case!(schema_content_without_chunks, |store| {
    use std::collections::BTreeMap;

    use koidrive_engine::classify::{Origin, Waiting};
    use koidrive_engine::question::{Held, Question, QuestionKind};
    use koidrive_engine::tree::{ContentKey, Kind, NodeId, Shape};
    use koidrive_proto::v1::NodeKind;
    use prost::Message;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/alpha6.db"),
        &path,
    )
    .unwrap();
    let mut copy = Store::open(&path).unwrap();
    let (report, mut state) = copy
        .transaction(|tx| {
            let report = (tx.local_nodes()?.into_iter())
                .find(|node| node.shape.name == "report.txt")
                .unwrap();
            Ok::<_, koidrive_store::Error>((report, tx.engine_state()?))
        })
        .unwrap();
    assert_eq!(state, store.transaction(|tx| tx.engine_state()).unwrap());
    let content = report.shape.content.unwrap();
    assert!(!content.chunks.is_empty());
    let shape = Shape {
        parent: NodeId::from(&report.shape.parent_id),
        name: "f".into(),
        kind: Kind::File,
        content: Some(ContentKey::try_from(&content).unwrap()),
    };
    let trees = &mut state.trees;
    let waiting = Waiting {
        shape: Some(shape.clone()),
        origin: Origin::Other,
    };
    trees.waiting.insert("waiting-new".into(), waiting);
    trees.discarded.insert("discarded-new".into(), shape.clone());
    trees.forgotten_shapes.insert("forgotten-new".into(), shape.clone());
    trees.lost_shapes.insert("lost-new".into(), shape.clone());
    state.commits.questions.push(Question {
        id: "q".into(),
        asked_at_ms: 1,
        kind: QuestionKind::MassDelete {
            deletions: 2,
            affected_files: 2,
            space_files: 2,
            exceeded: Vec::new(),
        },
        held: BTreeMap::from([(
            "shown-new".into(),
            Held {
                members: BTreeMap::from([("shown-new".into(), None), ("shown-old".into(), None)]),
                files: 2,
            },
        )]),
        answer: None,
        shown: BTreeMap::from([("shown-new".into(), shape.clone())]),
        frozen: None,
    });
    copy.transaction(|tx| tx.set_engine_state(&state)).unwrap();

    let conn = rusqlite::Connection::open(&path).unwrap();
    koidrive_store::migrations::register_functions(&conn).unwrap();
    let row = rusqlite::params![
        report.shape.parent_id,
        "f",
        NodeKind::File as i32,
        content.encode_to_vec()
    ];
    conn.execute(
        "INSERT INTO waiting_updates (node_id, parent_id, name, kind, content, slot)
         VALUES ('waiting-old', ?1, ?2, ?3, ?4, NULL)",
        row,
    )
    .unwrap();
    for (table, node) in [
        ("discarded_nodes", "discarded"),
        ("forgotten_shapes", "forgotten"),
        ("lost_shapes", "lost"),
    ] {
        conn.execute(
            &format!(
                "INSERT INTO {table} (node_id, parent_id, name, kind, content)
                 VALUES ('{node}-old', ?1, ?2, ?3, ?4)"
            ),
            row,
        )
        .unwrap();
    }
    conn.execute(
        "INSERT INTO open_question_shapes (question_id, node_id, parent_id, name, kind, content)
         VALUES ('q', 'shown-old', ?1, ?2, ?3, ?4)",
        row,
    )
    .unwrap();
    for (table, node) in [
        ("waiting_updates", "waiting"),
        ("discarded_nodes", "discarded"),
        ("forgotten_shapes", "forgotten"),
        ("lost_shapes", "lost"),
        ("open_question_shapes", "shown"),
    ] {
        let (shorter, same): (bool, bool) = conn
            .query_row(
                &format!(
                    "SELECT length(new.content) < length(old.content), same_content(new.content, old.content)
                     FROM {table} new, {table} old
                     WHERE new.node_id = '{node}-new' AND old.node_id = '{node}-old'"
                ),
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(shorter && same, "{table}");
    }
    drop(conn);

    let loaded = copy.transaction(|tx| tx.engine_state()).unwrap();
    let trees = &loaded.trees;
    let pair = |name: &str| (NodeId::from(format!("{name}-old")), NodeId::from(format!("{name}-new")));
    let (old, new) = pair("waiting");
    assert_eq!(trees.waiting[&old], trees.waiting[&new]);
    for (name, map) in [
        ("discarded", &trees.discarded),
        ("forgotten", &trees.forgotten_shapes),
        ("lost", &trees.lost_shapes),
    ] {
        let (old, new) = pair(name);
        assert_eq!(map[&old], map[&new], "{name}");
    }
    let question = (loaded.commits.questions.iter())
        .find(|question| question.id == "q")
        .unwrap();
    let (old, new) = pair("shown");
    assert_eq!(question.shown[&old], question.shown[&new]);
    assert_eq!(question.shown[&new], shape);

    copy.transaction(|tx| tx.set_engine_state(&loaded)).unwrap();
    assert_eq!(copy.transaction(|tx| tx.engine_state()).unwrap(), loaded);
});
