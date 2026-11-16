CREATE TABLE given_up (
    node_id TEXT PRIMARY KEY,
    content_rev INTEGER NOT NULL,
    meta_rev INTEGER NOT NULL,
    held_here INTEGER NOT NULL CHECK (held_here IN (0, 1))
) STRICT, WITHOUT ROWID;
