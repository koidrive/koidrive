// SPDX-License-Identifier: Apache-2.0

//! [`Folder`] with a test in place of the shell: a real folder, store and observer, `koidrive-sim`'s space as the
//! server, and chunks held in memory. A test runs the work a step returns and feeds the result back, so it chooses
//! the order results arrive in. After every step the store is loaded again and compared with the engine in memory.

include!("tests/common.rs");
include!("tests/conflict_questions.rs");
include!("tests/device_counter_and_recovery.rs");
include!("tests/first_link_and_held_files.rs");
include!("tests/ignored_entries_and_moves.rs");
include!("tests/large_deletion_questions.rs");
include!("tests/moves_and_name_collisions.rs");
include!("tests/progress.rs");
include!("tests/reconnect_and_commit_refusal.rs");
include!("tests/restore.rs");
include!("tests/rules_and_blocked_removals.rs");
include!("tests/rules_and_ignored_links.rs");
include!("tests/server_loss_recovery.rs");
include!("tests/status_and_failure_reporting.rs");
include!("tests/swapped_folders.rs");
include!("tests/unreadable_files.rs");
include!("tests/sync_transfers_and_retries.rs");

#[path = "../tests/folder_telemetry.rs"]
mod telemetry_tests;
