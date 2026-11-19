-- requires: root_minted
CREATE TABLE trash_groups (
    node_id TEXT PRIMARY KEY,
    trash_group TEXT NOT NULL
) STRICT, WITHOUT ROWID;
CREATE TABLE restore_pieces (
    node_id TEXT PRIMARY KEY,
    restore_id TEXT NOT NULL,
    trash_group TEXT NOT NULL
) STRICT, WITHOUT ROWID;
