// SPDX-License-Identifier: Apache-2.0

use koidrive_telemetry::schema::Event;
use koidrive_telemetry::{Store, install_panic_hook};

#[inline(never)]
fn crash_site(path: &str) {
    panic!("could not open {path}");
}

#[test]
fn a_real_panic_is_queued_with_koidrive_frames_and_nothing_of_its_message() {
    unsafe {
        std::env::remove_var("DO_NOT_TRACK");
        std::env::remove_var("KOIDRIVE_TELEMETRY");
    }
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    store.set(true).unwrap();
    install_panic_hook(store.clone());
    let secret = "/home/alice/Private Notes.docx";
    let crashed = std::thread::spawn(move || crash_site(secret)).join();
    assert!(crashed.is_err());

    let queued = store.peek().unwrap();
    let [Event::Panic { file, frames, .. }] = queued.events.as_slice() else {
        panic!("no single panic queued: {queued:?}");
    };
    assert_eq!(
        String::from(file.clone()),
        "core/telemetry/tests/koidrive_crash.rs"
    );
    let frames: Vec<&str> = frames.iter().map(|frame| frame.as_str()).collect();
    assert!(frames.contains(&"koidrive_crash::crash_site"), "{frames:?}");
    assert!(
        frames
            .iter()
            .all(|frame| frame.starts_with("koidrive_crash::")),
        "{frames:?}"
    );
    let written = std::fs::read_to_string(dir.path().join("telemetry/queue-v2.json")).unwrap();
    for leaked in ["alice", "Private", "/home", "could not open", "std::"] {
        assert!(!written.contains(leaked), "{leaked} in {written}");
    }
}
