CREATE TABLE given_up (
    node_id TEXT PRIMARY KEY,
    content_rev INTEGER NOT NULL,
    meta_rev INTEGER NOT NULL
) STRICT, WITHOUT ROWID;
