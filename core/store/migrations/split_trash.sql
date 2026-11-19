-- requires: root_minted
CREATE TABLE trash_groups (
    node_id TEXT PRIMARY KEY,
    trash_group TEXT NOT NULL
) STRICT, WITHOUT ROWID;
CREATE TABLE split_restores (
    restore_id TEXT PRIMARY KEY,
    trash_group TEXT NOT NULL,
    parent_id TEXT NOT NULL,
    name TEXT NOT NULL,
    folders TEXT NOT NULL,
    landed_parent_id TEXT,
    landed_name TEXT
) STRICT, WITHOUT ROWID;
CREATE TABLE commit_in_flight_restore_owners (
    op INTEGER PRIMARY KEY,
    restore_id TEXT NOT NULL
) STRICT, WITHOUT ROWID;
