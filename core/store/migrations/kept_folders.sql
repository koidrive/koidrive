CREATE TABLE kept_folders (node_id TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE kept_entries (
    node_id TEXT NOT NULL,
    name TEXT NOT NULL,
    taken INTEGER NOT NULL,
    PRIMARY KEY (node_id, name)
) STRICT, WITHOUT ROWID;
