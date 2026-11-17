// SPDX-License-Identifier: Apache-2.0

//! Building commits from local changes and acknowledging their results (docs/design/sync-engine.md
//! sections 2.4, 2.6, 2.7, 2.8 and 4.2, and RFC 0002), on hand-built trees and results.

include!("commit/agreements_after_loss.rs");
include!("commit/commit_moves_and_conflicts.rs");
include!("commit/commit_results_and_conflicts.rs");
include!("commit/commit_size_limits.rs");
include!("commit/common.rs");
include!("commit/edit_ancestry.rs");
include!("commit/late_content_acknowledgements.rs");
include!("commit/late_create_and_move_acknowledgements.rs");
include!("commit/late_position_acknowledgements.rs");
include!("commit/lost_changes_and_reverted_nodes.rs");
include!("commit/lost_download_versions.rs");
include!("commit/lost_waiting_changes.rs");
include!("commit/own_records_after_loss.rs");
include!("commit/parked_edits_and_pending_names.rs");
include!("commit/refusal_and_reconnect.rs");
include!("commit/refused_creates.rs");
include!("commit/revived_trash_groups.rs");
include!("commit/waiting_updates_and_removals.rs");
