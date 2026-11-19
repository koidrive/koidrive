CREATE TABLE synced_positions (
    node_id TEXT NOT NULL,
    position INTEGER NOT NULL,
    parent_id TEXT,
    name TEXT NOT NULL,
    PRIMARY KEY (node_id, position)
) STRICT, WITHOUT ROWID;
