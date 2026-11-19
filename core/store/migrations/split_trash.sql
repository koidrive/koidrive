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
    folders TEXT NOT NULL
) STRICT, WITHOUT ROWID;
