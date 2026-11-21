// SPDX-License-Identifier: Apache-2.0

use super::*;

fn stamp(store: &mut Store) -> u64 {
    store.read(|tx| tx.write_stamp()).unwrap()
}

fn write(store: &mut Store, ids: u64) -> Result<(), Error> {
    store.transaction(|tx| tx.set_id_generator(&ids.to_le_bytes()))
}

#[test]
fn a_store_from_before_write_stamps_opens_at_the_first_stamp() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    drop(Store::open(&path).unwrap());
    store_before(&path, "write_stamp");

    let mut store = Store::open(&path).unwrap();
    assert_eq!(applied(&path), all_migrations());
    assert_eq!(stamp(&mut store), 0);
}

#[test]
fn only_writes_of_a_connection_that_stamps_them_advance_the_stamp() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut stamped = Store::open(&path).unwrap();
    let mut other = Store::open(&path).unwrap();
    write(&mut other, 1).unwrap();
    assert_eq!(stamp(&mut other), 0);
    let first = stamp(&mut stamped);
    stamped.stamp_writes(first);
    write(&mut stamped, 2).unwrap();
    write(&mut stamped, 3).unwrap();
    stamped.transaction(|tx| tx.cursor()).unwrap();
    write(&mut other, 4).unwrap();
    assert_eq!(stamp(&mut other), 2);
    stamped.check_stamp().unwrap();
}

#[test]
fn a_connection_that_stamps_its_writes_writes_nothing_to_a_store_put_back_under_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut store = Store::open(&path).unwrap();
    let first = stamp(&mut store);
    store.stamp_writes(first);
    write(&mut store, 1).unwrap();
    Connection::open(&path)
        .unwrap()
        .execute("DELETE FROM write_stamp", [])
        .unwrap();

    assert!(matches!(store.check_stamp(), Err(Error::Replaced)));
    let refused = store.transaction(|tx| tx.cursor());
    assert!(matches!(refused, Err(Error::Replaced)), "{refused:?}");
    assert!(matches!(write(&mut store, 2), Err(Error::Replaced)));
    let mut reopened = Store::open(&path).unwrap();
    assert_eq!(stamp(&mut reopened), 0);
    assert_eq!(
        reopened.read(|tx| tx.id_generator()).unwrap(),
        Some(1u64.to_le_bytes().to_vec())
    );
}
