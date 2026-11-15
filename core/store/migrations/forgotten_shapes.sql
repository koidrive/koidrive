CREATE TABLE forgotten_shapes (
    node_id TEXT PRIMARY KEY,
    parent_id TEXT,
    name TEXT NOT NULL,
    kind INTEGER NOT NULL,
    content BLOB
) STRICT, WITHOUT ROWID;
