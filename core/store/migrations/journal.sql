-- requires: commit_slot_given_up, commit_slot_tokens, content_provenance, cycle_reverted, discard_members, discarded_shapes, dropped_edits, effects_in_flight, file_handles, first_listing, first_listing_dropped, forgotten_shapes, found_marker, given_up, kept_causes, kept_folders, kept_originals, local_changes, local_nodes_ctime, local_nodes_placement_and_origin, lost_positions, lost_shapes, missing_nodes, move_bases, moved_here, names_left_out, owed_deletes, past_positions_trash_group, pending_marks, question_frozen, recorded_found, recovery_cause, recovery_policy, remote_nodes_edit_rev, remote_nodes_meta_edit_rev, removed_from, removed_here, restore_asked, restore_asked_kind, root_identity, root_minted, root_token, split_trash, synced_positions, unobserved, unread_folders, unseen_changes, unsent_creates, unsent_moves, volume_mounts, waiting_update_released
CREATE TABLE journal (
    node_id TEXT NOT NULL,
    field TEXT NOT NULL CHECK (field IN ('place', 'content', 'status')),
    intent BLOB NOT NULL,
    author TEXT,
    base BLOB,
    value TEXT NOT NULL,
    wire_base TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('local', 'sent', 'held')),
    flight TEXT,
    held_as BLOB,
    held_version_id TEXT,
    time_ms INTEGER NOT NULL,
    replay INTEGER NOT NULL,
    PRIMARY KEY (node_id, field)
) STRICT, WITHOUT ROWID;

CREATE TABLE delete_views (
    node_id TEXT NOT NULL,
    ord INTEGER NOT NULL,
    view_node_id TEXT NOT NULL,
    field TEXT NOT NULL CHECK (field IN ('place', 'content', 'status')),
    intent BLOB NOT NULL,
    value TEXT,
    PRIMARY KEY (node_id, ord)
) STRICT, WITHOUT ROWID;

CREATE TABLE intent_history (
    node_id TEXT NOT NULL,
    field TEXT NOT NULL CHECK (field IN ('place', 'content', 'status')),
    ord INTEGER NOT NULL,
    intent BLOB NOT NULL,
    applied_at_ms INTEGER NOT NULL,
    in_local INTEGER NOT NULL,
    PRIMARY KEY (node_id, field, ord)
) STRICT, WITHOUT ROWID;

CREATE TABLE journal_absent (
    node_id TEXT PRIMARY KEY NOT NULL,
    since_ms INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

CREATE TABLE journal_state (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    listed INTEGER NOT NULL,
    backfill_owed INTEGER NOT NULL
) STRICT;

ALTER TABLE remote_nodes ADD COLUMN intents BLOB;
ALTER TABLE remote_nodes ADD COLUMN trash_held BLOB;
ALTER TABLE synced_nodes ADD COLUMN intents BLOB;
