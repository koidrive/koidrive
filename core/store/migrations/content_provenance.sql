CREATE TABLE content_provenance (
    node_id TEXT PRIMARY KEY,
    provenance BLOB NOT NULL
) STRICT, WITHOUT ROWID;
