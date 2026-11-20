// SPDX-License-Identifier: Apache-2.0

upgrade_case!(schema_effect_staged_identity, |store| {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/alpha6.db"),
        &path,
    )
    .unwrap();
    drop(Store::open(&path).unwrap());
    let hex = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/effect-record-without-staged.hex"
    ))
    .trim();
    let old: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).unwrap())
        .collect();
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "INSERT INTO effects_in_flight (position, record) VALUES (0, ?1)",
        [old],
    )
    .unwrap();
    drop(conn);
    let mut copy = Store::open(&path).unwrap();
    let mut record = copy
        .transaction(|tx| tx.engine_state())
        .unwrap()
        .effects
        .in_flight
        .remove(0);
    assert_eq!(record.staged, None);
    record.staged = Some(koidrive_engine::tree::FileIdentity {
        ino: 9,
        birth_ns: Some(-1),
        handle: None,
    });
    let mut state = store.transaction(|tx| tx.engine_state()).unwrap();
    assert!(state.effects.in_flight.is_empty());
    state.effects.in_flight.push(record);
    store.transaction(|tx| tx.set_engine_state(&state)).unwrap();
    assert_eq!(store.transaction(|tx| tx.engine_state()).unwrap(), state);
});
