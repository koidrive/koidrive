-- requires: root_identity
CREATE TABLE volume_mounts (
    path BLOB PRIMARY KEY,
    volume BLOB CHECK (volume IS NULL OR length(volume) IN (8, 16))
) STRICT, WITHOUT ROWID;
