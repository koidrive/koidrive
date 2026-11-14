CREATE TABLE restore_asked (
    restore_id TEXT PRIMARY KEY,
    path TEXT,
    version_saved_at_ms INTEGER
) STRICT, WITHOUT ROWID;
