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

CREATE TABLE journal_state (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    listed INTEGER NOT NULL,
    backfill_owed INTEGER NOT NULL,
    ids BLOB
) STRICT;

ALTER TABLE remote_nodes ADD COLUMN intents BLOB;
ALTER TABLE remote_nodes ADD COLUMN trash_held BLOB;
ALTER TABLE synced_nodes ADD COLUMN intents BLOB;
