// SPDX-License-Identifier: Apache-2.0

use super::*;
use koidrive_fs::Mount;
use std::path::PathBuf;

fn stamp(store: &mut Store) -> u64 {
    store.read(|tx| tx.write_stamp()).unwrap()
}

fn write(store: &mut Store, at: &str) -> Result<(), Error> {
    let mounts = BTreeMap::from([(PathBuf::from(at), Mount::Unidentified)]);
    store.transaction(|tx| tx.set_volume_mounts(&mounts))
}

fn mounts(path: &Path) -> Vec<PathBuf> {
    let mut store = Store::open(path).unwrap();
    store
        .read(|tx| tx.volume_mounts())
        .unwrap()
        .into_keys()
        .collect()
}

fn written_back(path: &Path) {
    Connection::open(path)
        .unwrap()
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap();
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
fn only_writes_of_a_connection_that_stamps_them_replace_the_stamp_each_with_a_new_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut stamped = Store::open(&path).unwrap();
    let mut other = Store::open(&path).unwrap();
    write(&mut other, "1").unwrap();
    assert_eq!(stamp(&mut other), 0);
    let first = stamp(&mut stamped);
    stamped.stamp_writes(first);
    write(&mut stamped, "2").unwrap();
    let second = stamp(&mut other);
    write(&mut stamped, "3").unwrap();
    let third = stamp(&mut other);
    stamped.transaction(|tx| tx.cursor()).unwrap();
    write(&mut other, "4").unwrap();
    assert_eq!(stamp(&mut other), third);
    assert!(
        second != 0 && third != 0 && second != third,
        "{second} {third}"
    );
    stamped.check_stamp().unwrap();
}

#[test]
fn a_connection_that_stamps_its_writes_writes_nothing_to_a_store_put_back_under_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut store = Store::open(&path).unwrap();
    let first = stamp(&mut store);
    store.stamp_writes(first);
    write(&mut store, "1").unwrap();
    Connection::open(&path)
        .unwrap()
        .execute("DELETE FROM write_stamp", [])
        .unwrap();

    assert!(matches!(store.check_stamp(), Err(Error::Replaced)));
    let refused = store.transaction(|tx| tx.cursor());
    assert!(matches!(refused, Err(Error::Replaced)), "{refused:?}");
    assert!(matches!(write(&mut store, "2"), Err(Error::Replaced)));
    let mut reopened = Store::open(&path).unwrap();
    assert_eq!(stamp(&mut reopened), 0);
    assert_eq!(mounts(&path), [PathBuf::from("1")]);
}

#[test]
fn a_second_store_put_back_at_the_stamp_a_reloaded_connection_reached_is_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let take = |to: &str| {
        written_back(&path);
        let to = dir.path().join(to);
        std::fs::copy(&path, &to).unwrap();
        to
    };
    let put_back = |from: &Path| {
        written_back(&path);
        std::fs::copy(from, &path).unwrap();
    };
    let mut store = Store::open(&path).unwrap();
    let first = stamp(&mut store);
    store.stamp_writes(first);
    write(&mut store, "1").unwrap();
    let x = take("x.db");
    write(&mut store, "2").unwrap();
    let y = take("y.db");
    put_back(&x);
    assert!(matches!(write(&mut store, "lost"), Err(Error::Replaced)));
    drop(store);

    let mut store = Store::open(&path).unwrap();
    let reloaded = stamp(&mut store);
    store.stamp_writes(reloaded);
    write(&mut store, "3").unwrap();
    put_back(&y);
    let checked = store.check_stamp();
    assert!(matches!(checked, Err(Error::Replaced)), "{checked:?}");
    let refused = write(&mut store, "4");
    assert!(matches!(refused, Err(Error::Replaced)), "{refused:?}");
    drop(store);
    assert_eq!(mounts(&path), [PathBuf::from("2")]);
}

#[test]
fn a_store_from_before_write_stamps_put_back_under_a_stamped_connection_is_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let before = dir.path().join("before.db");
    drop(Store::open(&before).unwrap());
    store_before(&before, "write_stamp");
    written_back(&before);
    let mut store = Store::open(&path).unwrap();
    let first = stamp(&mut store);
    store.stamp_writes(first);
    write(&mut store, "1").unwrap();
    written_back(&path);
    std::fs::copy(&before, &path).unwrap();

    let checked = store.check_stamp();
    assert!(matches!(checked, Err(Error::Replaced)), "{checked:?}");
    let refused = write(&mut store, "2");
    assert!(matches!(refused, Err(Error::Replaced)), "{refused:?}");
}

#[test]
fn a_write_stamp_unreadable_for_another_reason_is_a_store_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let mut store = Store::open(&path).unwrap();
    let first = stamp(&mut store);
    store.stamp_writes(first);
    write(&mut store, "1").unwrap();
    Connection::open(&path)
        .unwrap()
        .execute_batch("ALTER TABLE write_stamp RENAME COLUMN stamp TO renamed")
        .unwrap();

    let checked = store.check_stamp();
    assert!(matches!(checked, Err(Error::Sqlite(_))), "{checked:?}");
    let refused = write(&mut store, "2");
    assert!(matches!(refused, Err(Error::Sqlite(_))), "{refused:?}");
}
