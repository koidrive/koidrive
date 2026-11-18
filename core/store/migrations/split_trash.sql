-- requires: root_minted
CREATE TABLE trash_groups (
    node_id TEXT PRIMARY KEY,
    trash_group TEXT NOT NULL
) STRICT, WITHOUT ROWID;
