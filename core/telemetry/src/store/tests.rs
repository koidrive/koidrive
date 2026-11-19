// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;

use serde_json::Value;

use super::*;
use crate::Recorder;
use crate::recorder::Pending;
use crate::schema::{Command, Counter, Event, Failure, SourceFile, Version};

fn counted(command: Command) -> Pending {
    let recorder = Recorder::on();
    recorder.count(Counter::Command(command), 1);
    recorder.take()
}

fn held_files(dir: &Path) -> usize {
    fs::read_dir(dir.join("telemetry/held")).map_or(0, |entries| entries.count())
}

fn commands(batch: &Batch) -> BTreeMap<String, u64> {
    (batch.hours.iter())
        .flat_map(|report| report.counters.get("command").cloned().unwrap_or_default())
        .collect()
}

#[test]
fn a_lock_that_is_not_a_file_fails_without_waiting_on_it() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("telemetry")).unwrap();
    koidrive_test_support::make_fifo(&dir.path().join("telemetry/lock"));
    let state = dir.path().to_path_buf();
    let set = koidrive_test_support::without_waiting(move || Store::new(&state).set(true).err());
    assert_eq!(
        set.map(|error| error.kind()),
        Some(io::ErrorKind::InvalidInput)
    );
}

#[test]
fn a_yes_queues_what_was_held_under_the_id_the_preview_showed() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let hold = store.hold().unwrap().unwrap();
    store.keep(&hold, counted(Command::Setup)).unwrap();
    assert!(store.peek().unwrap().is_empty());
    let held = store.held().unwrap();
    let [(shown, batch)] = held.as_slice() else {
        panic!("{held:?}")
    };
    assert_eq!(shown, hold.id());
    assert_eq!(commands(batch), BTreeMap::from([("setup".to_owned(), 1)]));
    let settings = store.answer(hold, true).unwrap();
    assert_eq!(settings.enabled, Some(true));
    assert_eq!(settings.install_id.as_ref(), Some(shown));
    assert_eq!(
        commands(&store.peek().unwrap()),
        BTreeMap::from([("setup".to_owned(), 1)])
    );
    assert!(store.held().unwrap().is_empty());
    assert_eq!(held_files(dir.path()), 0);
}

#[test]
fn a_no_or_no_answer_deletes_what_was_held() {
    for answer in [Some(false), None] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let hold = store.hold().unwrap().unwrap();
        store.keep(&hold, counted(Command::Setup)).unwrap();
        match answer {
            Some(on) => drop(store.answer(hold, on).unwrap()),
            None => store.forget(hold).unwrap(),
        }
        assert!(store.held().unwrap().is_empty());
        assert!(store.peek().unwrap().is_empty());
        assert_eq!(held_files(dir.path()), 0);
        assert_eq!(store.settings().unwrap().enabled, answer);
    }
}

#[test]
fn runs_asking_at_once_each_keep_and_send_their_own() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let first = store.hold().unwrap().unwrap();
    let second = store.hold().unwrap().unwrap();
    store.keep(&first, counted(Command::Setup)).unwrap();
    store.keep(&second, counted(Command::Join)).unwrap();
    assert_eq!(store.held().unwrap().len(), 2);
    store.answer(first, true).unwrap();
    assert_eq!(
        commands(&store.peek().unwrap()),
        BTreeMap::from([("setup".to_owned(), 1)])
    );
    store.answer(second, true).unwrap();
    assert_eq!(
        commands(&store.peek().unwrap()),
        BTreeMap::from([("join".to_owned(), 1), ("setup".to_owned(), 1)])
    );
    assert_eq!(held_files(dir.path()), 0);
}

#[test]
fn what_a_dropped_hold_kept_is_deleted_while_a_child_process_starts() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let hold = store.hold().unwrap().unwrap();
    store.keep(&hold, counted(Command::Setup)).unwrap();
    let child = koidrive_test_support::Forked::start();
    drop(hold);
    let held = store.held().unwrap();
    drop(child);
    assert!(held.is_empty());
    assert_eq!(held_files(dir.path()), 0);
}

#[test]
fn what_a_run_that_died_held_is_deleted_and_never_queued() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let hold = store.hold().unwrap().unwrap();
    store.keep(&hold, counted(Command::Setup)).unwrap();
    drop(hold);
    assert!(store.held().unwrap().is_empty());
    assert_eq!(held_files(dir.path()), 0);
    let dead = store.hold().unwrap().unwrap();
    store.keep(&dead, counted(Command::Setup)).unwrap();
    drop(dead);
    let next = store.hold().unwrap().unwrap();
    assert_eq!(held_files(dir.path()), 2);
    store.answer(next, true).unwrap();
    assert!(store.peek().unwrap().is_empty());
}

#[test]
fn a_staged_write_a_dead_run_left_is_deleted_too() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let hold = store.hold().unwrap().unwrap();
    store.keep(&hold, counted(Command::Setup)).unwrap();
    let kept = store.held_path(&hold.run);
    fs::rename(&kept, kept.with_extension("json.tmp")).unwrap();
    drop(hold);
    assert!(store.held().unwrap().is_empty());
    assert_eq!(held_files(dir.path()), 0);
}

#[test]
fn crashed_legacy_and_atomic_held_writes_are_removed_before_listing() {
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    for mode in ["legacy", "atomic"] {
        let dir = tempfile::tempdir().unwrap();
        let mut child = Child(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "store::tests::crashed_held_write_worker",
                    "--nocapture",
                ])
                .env("KOI_HELD_CRASH_DIR", dir.path())
                .env("KOI_HELD_CRASH_MODE", mode)
                .stdin(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                assert_eq!(status.code(), Some(73));
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "crash worker timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let store = Store::new(dir.path());
        assert_eq!(held_files(dir.path()), 3, "{mode} did not leave a stage");
        assert!(store.held().unwrap().is_empty());
        assert_eq!(held_files(dir.path()), 0, "{mode} left unconsented bytes");
        store.set(true).unwrap();
        assert!(store.peek().unwrap().is_empty());
    }
}

#[test]
fn crashed_held_write_worker() {
    use std::io::Write;
    let Some(dir) = std::env::var_os("KOI_HELD_CRASH_DIR") else {
        return;
    };
    let store = Store::new(Path::new(&dir));
    let hold = store.hold().unwrap().unwrap();
    store.keep(&hold, counted(Command::Setup)).unwrap();
    let path = store.held_path(&hold.run);
    let bytes = fs::read(&path).unwrap();
    if std::env::var("KOI_HELD_CRASH_MODE").unwrap() == "legacy" {
        fs::write(path.with_extension("json.tmp"), bytes).unwrap();
    } else {
        let stage = koidrive_state_file::AtomicFile::prepare(&path).unwrap();
        stage.as_file().write_all(&bytes).unwrap();
        std::mem::forget(stage);
    }
    std::process::exit(73);
}

#[test]
fn a_choice_stored_before_the_hold_starts_none() {
    for on in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        store.set(on).unwrap();
        assert!(store.hold().unwrap().is_none());
        assert_eq!(held_files(dir.path()), 0);
    }
}

#[test]
fn turning_reports_off_meanwhile_drops_what_is_held() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let hold = store.hold().unwrap().unwrap();
    store.keep(&hold, counted(Command::Setup)).unwrap();
    store.set(false).unwrap();
    store.keep(&hold, counted(Command::Join)).unwrap();
    assert!(store.held().unwrap().is_empty());
    store.answer(hold, true).unwrap();
    assert!(store.peek().unwrap().is_empty());
}

#[test]
fn the_internal_mark_survives_turning_reports_off_and_on() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    assert!(!store.internal(|_| None).unwrap());
    store.mark_internal(true).unwrap();
    store.set(false).unwrap();
    store.set(true).unwrap();
    store.reset().unwrap();
    assert!(store.internal(|_| None).unwrap());
    assert_eq!(store.settings().unwrap().enabled, Some(true));
    store.mark_internal(false).unwrap();
    assert!(!store.internal(|_| None).unwrap());
}

#[test]
fn an_earlier_release_queue_stays_when_it_cannot_be_read_after_its_batch_went() {
    use std::os::unix::fs::PermissionsExt;

    let (_dir, store) = store();
    store.set(true).unwrap();
    let old = store.dir.join(OLD_QUEUE);
    fs::write(&old, br#"{"days":{"1":{"day":1}},"events":[{"type":"panic","day":2,"file":"cli/src/main.rs","line":1}]}"#).unwrap();
    let sending = store.take_old().unwrap().unwrap();
    fs::set_permissions(&old, fs::Permissions::from_mode(0o000)).unwrap();
    assert!(store.sent_old(sending).is_err());
    fs::set_permissions(&old, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(store.peek_old().unwrap().events.len(), 1);
}

#[test]
fn an_earlier_release_queue_goes_when_reports_turn_off_and_is_dropped_when_unreadable() {
    let (_dir, store) = store();
    store.set(true).unwrap();
    let old = store.dir.join(OLD_QUEUE);
    fs::write(
        &old,
        br#"{"days":{"1":{"day":1,"counters":{"command":{"status":1}}}}}"#,
    )
    .unwrap();
    store.set(false).unwrap();
    assert!(!old.exists());
    store.set(true).unwrap();
    fs::write(&old, b"{").unwrap();
    assert!(store.take_old().unwrap().is_none());
    assert!(!old.exists());
}

#[test]
fn unanswered_prompts_persist_without_consent_or_an_identity() {
    for on in [false, true] {
        let (dir, store) = store();
        assert!(!store.prompt_pending().unwrap());
        assert!(store.begin_prompt().unwrap());
        assert!(store.prompt_pending().unwrap());
        assert_eq!(store.settings().unwrap(), Settings::default());
        assert!(matches!(
            store.status(|_| None).unwrap().mode,
            crate::Mode::Off
        ));
        assert!(!dir.path().join("telemetry/settings.json").exists());
        assert!(!dir.path().join("telemetry/queue-v2.json").exists());
        store.set(on).unwrap();
        assert!(!store.prompt_pending().unwrap());
        assert!(!store.begin_prompt().unwrap());
        assert_eq!(store.settings().unwrap().enabled, Some(on));
    }
}

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    (dir, store)
}

fn panic_on(hour: u32) -> Event {
    Event::Panic {
        at: u64::from(hour) * 3600,
        file: SourceFile::from_location("cli/src/main.rs"),
        line: 7,
        frames: Vec::new(),
    }
}

fn pending(hour: u32, status: u64) -> Pending {
    let recorder = Recorder::on();
    recorder.count_on(hour, Counter::Command(Command::Status), status);
    recorder.event(panic_on(hour));
    recorder.take()
}

#[test]
fn a_new_profile_was_never_asked() {
    let (_dir, store) = store();
    assert_eq!(store.settings().unwrap(), Settings::default());
}

#[test]
fn turning_it_on_draws_an_id_once_and_off_deletes_the_id_and_the_queue() {
    let (_dir, store) = store();
    let on = store.set(true).unwrap();
    let id = on.install_id.clone().unwrap();
    assert_eq!(store.set(true).unwrap().install_id, Some(id.clone()));
    store.absorb_at(pending(100, 1), 100, true).unwrap();
    assert!(!store.peek().unwrap().is_empty());

    let off = store.set(false).unwrap();
    assert_eq!(
        off,
        Settings {
            enabled: Some(false),
            install_id: None,
            generation: off.generation.clone(),
            internal: false,
        }
    );
    assert!(store.peek().unwrap().is_empty());
    assert_ne!(store.set(true).unwrap().install_id, Some(id));
}

#[test]
fn a_broken_settings_file_does_not_stop_a_choice() {
    let (_dir, store) = store();
    store.set(true).unwrap();
    store.absorb_at(pending(100, 1), 100, true).unwrap();
    fs::write(store.dir.join("settings.json"), b"{").unwrap();
    assert!(store.settings().is_err());
    assert_eq!(store.set(false).unwrap().enabled, Some(false));
    assert!(store.peek().unwrap().is_empty());
    assert_eq!(store.settings().unwrap().install_id, None);
}

#[test]
fn turning_it_on_over_a_broken_settings_file_drops_the_old_queue() {
    let (_dir, store) = store();
    store.set(true).unwrap();
    store.absorb_at(pending(99, 1), 100, true).unwrap();
    drop(store.take(100).unwrap());
    store.absorb_at(pending(100, 1), 100, true).unwrap();
    fs::write(store.dir.join("settings.json"), b"{").unwrap();
    assert!(store.set(true).unwrap().install_id.is_some());
    assert!(store.peek().unwrap().is_empty());
    assert!(store.take(101).unwrap().unwrap().batch.is_empty());
}

#[test]
fn a_new_id_over_a_deleted_settings_file_drops_the_old_queue() {
    for draw in [
        |store: &Store| drop(store.set(true).unwrap()),
        |store: &Store| drop(store.install_id().unwrap()),
    ] {
        let (_dir, store) = store();
        store.set(true).unwrap();
        store.absorb_at(pending(99, 1), 100, true).unwrap();
        drop(store.take(100).unwrap());
        store.absorb_at(pending(100, 1), 100, true).unwrap();
        fs::remove_file(store.dir.join("settings.json")).unwrap();
        draw(&store);
        assert!(store.peek().unwrap().is_empty());
    }
}

#[test]
fn reset_draws_a_new_id_and_empties_the_queue() {
    let (_dir, store) = store();
    assert_eq!(store.reset().unwrap(), None);
    let id = store.set(true).unwrap().install_id.unwrap();
    store.absorb_at(pending(100, 1), 100, true).unwrap();
    let new = store.reset().unwrap().unwrap();
    assert_ne!(new, id);
    assert_eq!(store.settings().unwrap().install_id, Some(new));
    assert!(store.peek().unwrap().is_empty());
}

#[test]
fn a_new_id_is_drawn_only_once_the_old_queue_is_gone() {
    for (on, draw) in [
        (
            true,
            Store::reset as fn(&Store) -> io::Result<Option<InstallId>>,
        ),
        (false, |store: &Store| store.set(true).map(|s| s.install_id)),
        (false, |store: &Store| store.install_id().map(Some)),
    ] {
        let (_dir, store) = store();
        store.set(on).unwrap();
        let before = store.settings().unwrap().install_id;
        fs::create_dir(store.dir.join(QUEUE)).unwrap();
        assert!(draw(&store).is_err());
        assert_eq!(store.settings().unwrap().install_id, before);
    }
}

#[test]
fn sums_from_several_processes_add_up() {
    let (_dir, store) = store();
    store.set(true).unwrap();
    store.absorb_at(pending(100, 1), 100, true).unwrap();
    store.absorb_at(pending(100, 2), 100, true).unwrap();
    let queued = store.peek().unwrap();
    assert_eq!(queued.hours.len(), 1);
    assert_eq!(queued.hours[0].counters["command"]["status"], 3);
    assert_eq!(queued.events.len(), 2);
}

fn hours(queue: &Queue) -> Vec<u32> {
    queue.hours.keys().copied().collect()
}

#[test]
fn take_leaves_this_hour_and_give_back_restores_the_rest() {
    let (_dir, store) = store();
    store.set(true).unwrap();
    store.absorb_at(pending(99, 1), 100, true).unwrap();
    store.absorb_at(pending(100, 5), 100, true).unwrap();
    let sending = store.take(100).unwrap().unwrap();
    assert_eq!(
        (sending.batch.hours.iter())
            .map(|report| report.hour)
            .collect::<Vec<_>>(),
        [99]
    );
    assert_eq!(sending.batch.events.len(), 2);
    let left = store.queue().unwrap();
    assert_eq!(hours(&left), [100]);
    assert!(left.events.is_empty());
    assert_eq!(left.out.unwrap().batch, sending.batch);
    assert_eq!(store.peek().unwrap().hours.len(), 2);

    let (batch, uuids) = (sending.batch.clone(), sending.uuids.clone());
    assert_eq!(uuids.len(), 3);
    store.give_back(sending).unwrap();
    store.absorb_at(pending(99, 4), 100, true).unwrap();
    let again = store.take(101).unwrap().unwrap();
    assert_eq!((again.batch, again.uuids), (batch, uuids));
}

#[test]
fn take_drops_what_expired() {
    let (_dir, store) = store();
    store.set(true).unwrap();
    store.absorb_at(pending(99, 1), 100, true).unwrap();
    assert!(
        store
            .take(99 + KEEP_HOURS + 1)
            .unwrap()
            .unwrap()
            .batch
            .is_empty()
    );
    store.absorb_at(pending(99, 1), 100, true).unwrap();
    let sending = store.take(100).unwrap().unwrap();
    store.give_back(sending).unwrap();
    assert!(
        store
            .take(99 + KEEP_HOURS + 1)
            .unwrap()
            .unwrap()
            .batch
            .is_empty()
    );
    assert!(store.peek().unwrap().is_empty());
}

#[test]
fn a_take_with_nothing_due_still_drops_what_expired() {
    let (_dir, store) = store();
    store.set(true).unwrap();
    store.absorb_at(pending(99, 1), 100, true).unwrap();
    let late = 99 + KEEP_HOURS + 1;
    drop(store.take(late).unwrap());
    assert!(store.queue().unwrap().hours.is_empty());
    assert!(store.queue().unwrap().events.is_empty());
}

#[test]
fn an_expired_batch_out_makes_no_room_for_itself() {
    let (_dir, store) = store();
    store.set(true).unwrap();
    store.absorb_at(pending(99, 1), 100, true).unwrap();
    let sending = store.take(100).unwrap().unwrap();
    store.give_back(sending).unwrap();
    let late = 99 + KEEP_HOURS + 1;
    store.absorb_at(pending(late, 1), late, true).unwrap();
    let queue = store.queue().unwrap();
    assert!(queue.out.is_none());
    assert_eq!(queue.events.len(), 1);
}

#[test]
fn events_out_count_toward_the_cap() {
    let (_dir, store) = store();
    store.set(true).unwrap();
    let panics = |n| Pending {
        hours: BTreeMap::new(),
        events: vec![panic_on(99); n],
    };
    store
        .absorb_at(panics(Event::PER_KIND_PER_DAY), 100, true)
        .unwrap();
    let sending = store.take(100).unwrap().unwrap();
    store.give_back(sending).unwrap();
    store.absorb_at(panics(5), 100, true).unwrap();
    assert!(store.queue().unwrap().events.is_empty());
}

#[test]
fn the_daily_cap_holds_across_sends() {
    let (_dir, store) = store();
    store.set(true).unwrap();
    let problem = Failure::all().next().unwrap();
    let mut sent = 0;
    for minute in 0..100 {
        let pending = Pending {
            hours: BTreeMap::new(),
            events: vec![Event::Problem {
                at: 100 * 3600 + minute,
                problem,
                count: 1,
            }],
        };
        store.absorb_at(pending, 100, true).unwrap();
        let sending = store.take(101).unwrap().unwrap();
        sent += sending.batch.events.len();
        store.sent(sending).unwrap();
    }
    assert_eq!(sent, Event::PER_KIND_PER_DAY);
}

#[test]
fn a_full_queue_drops_other_events_before_a_panic() {
    let (_dir, store) = store();
    store.set(true).unwrap();
    let now = 1_000;
    let oldest = now - 13 * 24;
    let panic = Pending {
        hours: BTreeMap::new(),
        events: vec![panic_on(oldest)],
    };
    store.absorb_at(panic, now, true).unwrap();
    for day in 0..13 {
        let events = (Failure::all().take(3))
            .flat_map(|problem| {
                (0..Event::PER_KIND_PER_DAY).map(move |n| Event::Problem {
                    at: u64::from(oldest + day * 24) * 3600 + n as u64,
                    problem,
                    count: 1,
                })
            })
            .collect();
        let pending = Pending {
            hours: BTreeMap::new(),
            events,
        };
        store.absorb_at(pending, now, true).unwrap();
    }
    let queued = store.peek().unwrap();
    assert_eq!(queued.events.len(), MAX_EVENTS);
    assert!((queued.events.iter()).any(|queued| matches!(queued.event, Event::Panic { .. })));
}

fn panics(at_hour: u32, n: usize) -> Pending {
    Pending {
        hours: BTreeMap::new(),
        events: vec![panic_on(at_hour); n],
    }
}

#[test]
fn a_batch_given_back_still_shows_in_full() {
    let (_dir, store) = store();
    store.set(true).unwrap();
    store
        .absorb_at(panics(99, Event::PER_KIND_PER_DAY), 100, true)
        .unwrap();
    drop(store.take(100).unwrap().unwrap());
    assert_eq!(store.peek().unwrap().events.len(), Event::PER_KIND_PER_DAY);
}

#[test]
fn a_dropped_sending_frees_the_next_take_while_a_child_process_starts() {
    let (_dir, store) = store();
    store.set(true).unwrap();
    store.absorb_at(panics(99, 1), 100, true).unwrap();
    let sending = store.take(100).unwrap().unwrap();
    let child = koidrive_test_support::Forked::start();
    drop(sending);
    let again = store.take(100).unwrap();
    drop(child);
    assert!(again.is_some());
}

#[test]
fn an_unknown_event_in_a_batch_out_leaves_the_rest_with_their_uuids() {
    let (_dir, store) = store();
    store.set(true).unwrap();
    let mut pending = panics(99, 2);
    if let Event::Panic { line, .. } = &mut pending.events[1] {
        *line = 8;
    }
    store.absorb_at(pending, 100, true).unwrap();
    let sending = store.take(100).unwrap().unwrap();
    let uuids = sending.uuids.clone();
    drop(sending);
    let path = store.dir.join(QUEUE);
    let mut file: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    file["out"]["batch"]["events"][0] =
        serde_json::json!({ "type": "from_a_later_release", "at": 1 });
    fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
    let again = store.take(100).unwrap().unwrap();
    assert_eq!(again.uuids, uuids[1..]);
    assert!(matches!(
        again.batch.events[..],
        [Queued {
            event: Event::Panic { line: 8, .. },
            ..
        }]
    ));
}

#[test]
fn a_full_batch_out_makes_room_for_a_new_panic() {
    let (_dir, store) = store();
    store.set(true).unwrap();
    let now = 1_000;
    for day in 0..13 {
        let events = (Failure::all().take(3))
            .flat_map(|problem| {
                (0..Event::PER_KIND_PER_DAY).map(move |n| Event::Problem {
                    at: u64::from(now - 13 * 24 + day * 24) * 3600 + n as u64,
                    problem,
                    count: 1,
                })
            })
            .collect();
        let pending = Pending {
            hours: BTreeMap::new(),
            events,
        };
        store.absorb_at(pending, now, true).unwrap();
    }
    drop(store.take(now).unwrap().unwrap());
    store.absorb_at(panics(now, 1), now, true).unwrap();
    let queued = store.peek().unwrap();
    assert_eq!(queued.events.len(), MAX_EVENTS);
    assert!((queued.events.iter()).any(|queued| matches!(queued.event, Event::Panic { .. })));
    let sending = store.take(now).unwrap().unwrap();
    assert_eq!(
        sending.uuids.len(),
        sending.batch.hours.len() + sending.batch.events.len()
    );
}

#[test]
fn an_event_this_build_does_not_know_drops_only_itself() {
    let (_dir, store) = store();
    store.set(true).unwrap();
    fs::write(
        store.dir.join(QUEUE),
        br#"{"events":[{"type":"panic","at":3600,"file":"cli/src/main.rs","line":7},{"type":"from_a_later_release","at":3600}],"later_field":1}"#,
    )
    .unwrap();
    let queued = store.peek().unwrap();
    assert_eq!(queued.events.len(), 1);
    assert!(matches!(
        queued.events[0].event,
        Event::Panic { line: 7, .. }
    ));
}

#[test]
fn a_batch_partly_expired_while_out_still_settles() {
    let (_dir, store) = store();
    store.set(true).unwrap();
    store.absorb_at(pending(98, 1), 100, true).unwrap();
    store.absorb_at(pending(99, 1), 100, true).unwrap();
    let sending = store.take(100).unwrap().unwrap();
    assert_eq!(sending.batch.hours.len(), 2);
    store
        .absorb_at(pending(100, 1), 98 + KEEP_HOURS + 1, true)
        .unwrap();
    assert_eq!(store.queue().unwrap().out.unwrap().batch.hours.len(), 1);
    store.sent(sending).unwrap();
    assert!(store.queue().unwrap().out.is_none());
}

#[test]
fn a_sending_of_another_profile_settles_nothing() {
    let (_dir, store) = store();
    let (_other_dir, other) = self::store();
    for store in [&store, &other] {
        store.set(true).unwrap();
        store.absorb_at(pending(99, 1), 100, true).unwrap();
    }
    let theirs = other.take(100).unwrap().unwrap();
    let _ours = store.take(100).unwrap().unwrap();
    store.sent(theirs).unwrap();
    assert!(store.queue().unwrap().out.is_some());
}

#[test]
fn a_sent_batch_leaves_the_queue() {
    let (_dir, store) = store();
    store.set(true).unwrap();
    store.absorb_at(pending(99, 1), 100, true).unwrap();
    let sending = store.take(100).unwrap().unwrap();
    store.sent(sending).unwrap();
    assert!(store.peek().unwrap().is_empty());
}

#[test]
fn one_send_at_a_time_and_a_dropped_one_loses_nothing() {
    let (_dir, store) = store();
    store.set(true).unwrap();
    store.absorb_at(pending(99, 1), 100, true).unwrap();
    let sending = store.take(100).unwrap().unwrap();
    assert!(store.take(100).unwrap().is_none());
    let (batch, uuids) = (sending.batch.clone(), sending.uuids.clone());
    drop(sending);

    store.absorb_at(pending(99, 2), 100, true).unwrap();
    let again = store.take(100).unwrap().unwrap();
    assert_eq!((&again.batch, &again.uuids), (&batch, &uuids));
    store.sent(again).unwrap();
    let next = store.take(100).unwrap().unwrap();
    assert_eq!(next.batch.hours[0].counters["command"]["status"], 2);
    assert_ne!(next.uuids, uuids);
}

#[test]
fn a_batch_out_while_telemetry_is_reset_or_turned_off_is_not_given_back() {
    let (_dir, store) = store();
    store.set(true).unwrap();
    store.absorb_at(pending(99, 1), 100, true).unwrap();
    let sending = store.take(100).unwrap().unwrap();
    store.reset().unwrap();
    store.give_back(sending).unwrap();
    assert!(store.peek().unwrap().is_empty());

    store.absorb_at(pending(99, 1), 100, true).unwrap();
    let sending = store.take(100).unwrap().unwrap();
    store.set(false).unwrap();
    store.give_back(sending).unwrap();
    assert!(store.peek().unwrap().is_empty());
    assert!(store.take(100).unwrap().is_none());
    assert_eq!(store.settings().unwrap().install_id, None);

    let id = store.set(true).unwrap().install_id.unwrap();
    let new = store.reset().unwrap().unwrap();
    store
        .absorb_for(pending(99, 1), 100, true, Some(&id))
        .unwrap();
    assert!(store.peek().unwrap().is_empty());
    store
        .absorb_for(pending(99, 1), 100, true, Some(&new))
        .unwrap();
    assert!(!store.peek().unwrap().is_empty());
}

#[test]
fn nothing_is_kept_without_an_install_id() {
    let (_dir, store) = store();
    store.absorb_at(pending(100, 1), 100, true).unwrap();
    assert!(store.peek().unwrap().is_empty());
    store.set(true).unwrap();
    store.set(false).unwrap();
    store.absorb_at(pending(100, 1), 100, true).unwrap();
    assert!(store.peek().unwrap().is_empty());
}

#[test]
fn an_unreadable_queue_is_dropped_instead_of_stopping_later_writes() {
    let (_dir, store) = store();
    store.set(true).unwrap();
    fs::write(store.dir.join(QUEUE), b"").unwrap();
    store.absorb_at(pending(100, 1), 100, true).unwrap();
    assert_eq!(store.peek().unwrap().hours.len(), 1);
}

#[test]
fn a_queue_that_cannot_be_read_is_left_alone() {
    let (_dir, store) = store();
    store.set(true).unwrap();
    fs::create_dir(store.dir.join(QUEUE)).unwrap();
    assert!(store.absorb_at(pending(100, 1), 100, true).is_err());
    assert!(store.dir.join(QUEUE).is_dir());
}

#[test]
fn the_directory_is_the_owner_s_alone() {
    use std::os::unix::fs::PermissionsExt;
    let (_dir, store) = store();
    store.set(true).unwrap();
    let mode = fs::metadata(&store.dir).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700);
}

#[test]
fn old_hours_and_events_are_dropped() {
    let (_dir, store) = store();
    store.set(true).unwrap();
    store
        .absorb_at(pending(400 - KEEP_HOURS - 1, 1), 400 - KEEP_HOURS - 1, true)
        .unwrap();
    store.absorb_at(pending(400, 1), 400, true).unwrap();
    let queued = store.peek().unwrap();
    assert_eq!(
        queued
            .hours
            .iter()
            .map(|report| report.hour)
            .collect::<Vec<_>>(),
        [400]
    );
    assert!(
        queued
            .events
            .iter()
            .all(|queued| queued.event.hour() == 400)
    );
}

#[test]
fn the_queue_holds_at_most_20_events_of_a_kind_a_day() {
    let (_dir, store) = store();
    store.set(true).unwrap();
    for _ in 0..30 {
        store
            .absorb_at(
                Pending {
                    hours: BTreeMap::new(),
                    events: vec![panic_on(100)],
                },
                100,
                true,
            )
            .unwrap();
    }
    assert_eq!(store.peek().unwrap().events.len(), Event::PER_KIND_PER_DAY);
}

#[test]
fn a_busy_queue_is_skipped_instead_of_waited_for() {
    let (_dir, store) = store();
    let _held = store.lock(true).unwrap();
    let error = store.absorb_at(pending(100, 1), 100, false).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
}

fn none(_: &str) -> Option<std::ffi::OsString> {
    None
}

fn crash_report(
    store: &Store,
    id: &str,
    var: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Value {
    let stamp = store.app_crash_stamp(var).unwrap();
    serde_json::json!({
        "report": { "id": id, "timestamp": crate::now() * 1_000_000 },
        "crash": { "error": { "type": "signal", "signal": { "signal": 11 } } },
        "user": { "reports_on": stamp.on, "reports_generation": stamp.generation },
    })
}

fn app_crashes(store: &Store) -> usize {
    (store.peek().unwrap().events.iter())
        .filter(|queued| matches!(queued.event, Event::AppCrash { .. }))
        .count()
}

#[test]
fn an_app_crash_report_handed_over_again_is_queued_once_even_after_its_send() {
    let (_dir, store) = store();
    store.set(true).unwrap();
    let first = crash_report(&store, "4D2B0160-69B6-4DC1-BFED-08A0554A67C9", none);
    store.absorb_app_crash(&first, none).unwrap();
    store.absorb_app_crash(&first, none).unwrap();
    assert_eq!(app_crashes(&store), 1);

    let sending = store.take(crate::this_hour()).unwrap().unwrap();
    store.sent(sending).unwrap();
    store.absorb_app_crash(&first, none).unwrap();
    assert_eq!(app_crashes(&store), 0);

    let second = crash_report(&store, "0F3C2E7A-1B4D-4E5F-8A9B-0C1D2E3F4A5B", none);
    store.absorb_app_crash(&second, none).unwrap();
    assert_eq!(app_crashes(&store), 1);
    let queue = fs::read_to_string(store.dir.join(QUEUE)).unwrap();
    assert_eq!(queue.matches("4D2B0160").count(), 1);
}

#[test]
fn an_app_crash_goes_under_the_version_that_crashed() {
    let (_dir, store) = store();
    let store = store.recorded_by(Origin {
        koi_version: Version::parse("0.1.1").unwrap(),
        client: crate::schema::Client::MacosApp,
    });
    store.set(true).unwrap();
    let mut report = crash_report(&store, "A", none);
    report["user"]["koidrive_version"] = "0.1.0-rc.1".into();
    report["user"]["name"] = "alice".into();
    store.absorb_app_crash(&report, none).unwrap();
    store
        .absorb_app_crash(&crash_report(&store, "B", none), none)
        .unwrap();
    let versions: Vec<_> = (store.peek().unwrap().events.into_iter())
        .map(|queued| String::from(queued.origin.unwrap().koi_version))
        .collect();
    assert_eq!(versions, ["0.1.0-rc.1", "0.1.1"]);
}

#[test]
fn an_app_crash_that_cannot_be_written_is_an_error_and_is_queued_once_the_queue_is_back() {
    let (_dir, store) = store();
    store.set(true).unwrap();
    fs::create_dir(store.dir.join(QUEUE)).unwrap();
    let report = crash_report(&store, "A", none);
    assert!(store.absorb_app_crash(&report, none).is_err());
    fs::remove_dir(store.dir.join(QUEUE)).unwrap();
    store.absorb_app_crash(&report, none).unwrap();
    assert_eq!(app_crashes(&store), 1);
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Env {
    Unset,
    On,
    Off,
}

impl Env {
    fn var(self) -> impl Fn(&str) -> Option<std::ffi::OsString> {
        move |name| match (self, name) {
            (Env::On, consent::VARIABLE) => Some("1".into()),
            (Env::Off, "DO_NOT_TRACK") => Some("1".into()),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Step {
    TurnOn,
    TurnOff,
    Reset,
    AppReads(Env),
}

const STEPS: [Step; 6] = [
    Step::TurnOn,
    Step::TurnOff,
    Step::Reset,
    Step::AppReads(Env::Unset),
    Step::AppReads(Env::On),
    Step::AppReads(Env::Off),
];

fn sequences(longest: usize, steps: &[Step]) -> Vec<Vec<Step>> {
    let mut all = vec![Vec::new()];
    let mut last = vec![Vec::new()];
    for _ in 0..longest {
        last = (last.iter())
            .flat_map(|done: &Vec<Step>| {
                (steps.iter()).map(move |step| [done.as_slice(), &[*step]].concat())
            })
            .collect();
        all.extend(last.iter().cloned());
    }
    all
}

struct Model {
    stored: Option<bool>,
    read_under: Env,
    read_on: bool,
    purged_since_read: bool,
    on_at_crash: bool,
    revoked: bool,
}

#[test]
fn an_app_crash_is_sent_only_if_reports_were_on_as_it_crashed_and_no_off_or_reset_came_after() {
    let dir = tempfile::tempdir().unwrap();
    let mut cases = 0;
    for before in sequences(3, &STEPS) {
        for after in sequences(2, &STEPS[..3]) {
            for import in [Env::Unset, Env::On, Env::Off] {
                cases += 1;
                let store = Store::new(&dir.path().join(cases.to_string()));
                let mut model = Model {
                    stored: None,
                    read_under: Env::Unset,
                    read_on: false,
                    purged_since_read: false,
                    on_at_crash: false,
                    revoked: false,
                };
                let mut stamp = store.app_crash_stamp(Env::Unset.var()).unwrap();
                let mut report = None;
                let steps = before
                    .iter()
                    .map(Some)
                    .chain([None])
                    .chain(after.iter().map(Some));
                for step in steps {
                    let crashed = report.is_some();
                    match step {
                        None => {
                            model.on_at_crash =
                                consent::resolve(model.read_under.var(), model.stored).mode
                                    != consent::Mode::Off;
                            report = Some(serde_json::json!({
                                "report": { "id": "A", "timestamp": crate::now() * 1_000_000 },
                                "crash": { "error": { "type": "signal" } },
                                "user": { "reports_on": stamp.on, "reports_generation": stamp.generation },
                            }));
                        }
                        Some(Step::TurnOn) => {
                            store.set(true).unwrap();
                            model.stored = Some(true);
                        }
                        Some(Step::TurnOff) => {
                            store.set(false).unwrap();
                            model.stored = Some(false);
                            model.purged_since_read = true;
                            model.revoked |= crashed;
                        }
                        Some(Step::Reset) => {
                            store.reset().unwrap();
                            model.purged_since_read = true;
                            model.revoked |= crashed;
                        }
                        Some(Step::AppReads(env)) if !crashed => {
                            stamp = store.app_crash_stamp(env.var()).unwrap();
                            model.read_under = *env;
                            model.read_on = consent::resolve(env.var(), model.stored).mode
                                != consent::Mode::Off;
                            model.purged_since_read = false;
                        }
                        Some(Step::AppReads(_)) => {}
                    }
                }
                let on_at_import =
                    consent::resolve(import.var(), model.stored).mode != consent::Mode::Off;
                store
                    .absorb_app_crash(report.as_ref().unwrap(), import.var())
                    .unwrap();
                let sent = app_crashes(&store) == 1;
                let case = format!("{before:?}, crash, {after:?}, import under {import:?}");
                if sent {
                    assert!(
                        model.on_at_crash && !model.revoked && on_at_import,
                        "sent against consent: {case}"
                    );
                }
                let expected = model.read_on && !model.purged_since_read && on_at_import;
                assert_eq!(sent, expected, "{case}");
            }
        }
    }
    assert_eq!(cases, 259 * 13 * 3);
}
