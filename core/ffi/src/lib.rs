// SPDX-License-Identifier: Apache-2.0

//! UniFFI bridge between the Rust core and the native desktop apps.
//!
//! The app holds a [`SyncSession`], which forwards to a [`Core`]. The surface follows the sync
//! engine's inputs and outputs (docs/design/sync-engine.md §1): session lifecycle, server and
//! device sign-in, linking folders to spaces, a stream of [`SessionEvent`]s for status,
//! progress and recent changes, the engine's questions with typed [`Answer`]s, its notices and
//! its problems. [`SyncSession::open`] works through the `koi` command and its daemon on the CLI's profile;
//! [`SyncSession::fake`] runs [`FakeCore`], which plays back scripted states for previews and tests.

mod core;
mod daemon;
mod fake;
mod helper;
mod profile;
mod session;
mod state;
mod types;

pub use crate::core::{Core, Events, SessionListener};
pub use crate::fake::{FakeActivity, FakeCore, FakeScenario, GivenAnswer};
pub use crate::session::SyncSession;
pub use crate::types::*;

uniffi::setup_scaffolding!();

/// The [`AppCrashStamp`] of the profile [`SyncSession::open`] opens through the `koi` at `helper`, read without opening
/// it; reports off when no profile is found.
#[uniffi::export]
pub fn app_crash_stamp(helper: String) -> AppCrashStamp {
    profile::Profile::discover(std::path::Path::new(&helper), |name| std::env::var_os(name))
        .map(|profile| daemon::app_crash_stamp(&profile.paths))
        .unwrap_or_default()
}

/// Returns the version of the Rust core.
#[uniffi::export]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    struct Recorder(Arc<Mutex<Vec<SessionEvent>>>);

    impl SessionListener for Recorder {
        fn on_event(&self, event: SessionEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    #[test]
    fn session_forwards_to_its_core_and_streams_events() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let session = SyncSession::fake(Box::new(Recorder(seen.clone())));
        assert_eq!(session.state().status, SyncStatus::Stopped);
        session.start();
        session
            .fake_core()
            .unwrap()
            .load_scenario(FakeScenario::MassDelete)
            .unwrap();
        let question = session.state().questions[0].clone();
        assert_eq!(
            session.answer(question.id.clone(), Answer::Keep),
            Err(SessionError::AnswerNotAllowed)
        );
        session.answer(question.id, Answer::Restore).unwrap();
        assert!(session.state().questions.is_empty());

        let seen = seen.lock().unwrap();
        assert_eq!(
            seen.first(),
            Some(&SessionEvent::Questions { questions: vec![] })
        );
        assert_eq!(
            seen.last(),
            Some(&SessionEvent::Folders {
                folders: session.state().folders
            })
        );
    }

    struct DropFlag(Arc<AtomicBool>);

    impl SessionListener for DropFlag {
        fn on_event(&self, _event: SessionEvent) {}
    }

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn dropping_the_session_drops_the_listener_even_while_the_fake_lives() {
        let dropped = Arc::new(AtomicBool::new(false));
        let session = SyncSession::fake(Box::new(DropFlag(dropped.clone())));
        let fake = session.fake_core().unwrap();
        session.start();
        assert!(!dropped.load(Ordering::SeqCst));

        drop(session);
        assert!(dropped.load(Ordering::SeqCst));
        fake.load_scenario(FakeScenario::UpToDate).unwrap();
    }
}
