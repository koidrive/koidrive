CREATE TABLE first_listing (
    history_id TEXT PRIMARY KEY,
    live INTEGER NOT NULL CHECK (live IN (0, 1))
) STRICT, WITHOUT ROWID;
