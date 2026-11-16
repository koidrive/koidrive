ALTER TABLE link ADD COLUMN root_volume BLOB CHECK (root_volume IS NULL OR length(root_volume) IN (8, 16));

ALTER TABLE local_nodes DROP COLUMN dev;

CREATE TABLE recorded_nodes_without_dev (
    node_id TEXT PRIMARY KEY,
    displaced INTEGER NOT NULL CHECK (displaced IN (0, 1)),
    ino INTEGER,
    birth_ns INTEGER,
    ino_high INTEGER NOT NULL DEFAULT 0 CHECK (ino IS NOT NULL OR ino_high = 0),
    lost INTEGER NOT NULL DEFAULT 0 CHECK (lost IN (0, 1)),
    CHECK ((displaced = 1) = (ino IS NOT NULL)),
    CHECK (ino IS NOT NULL OR birth_ns IS NULL)
) STRICT, WITHOUT ROWID;
INSERT INTO recorded_nodes_without_dev (node_id, displaced, ino, birth_ns, ino_high, lost)
SELECT node_id, displaced, ino, birth_ns, ino_high, lost FROM recorded_nodes;
DROP TABLE recorded_nodes;
ALTER TABLE recorded_nodes_without_dev RENAME TO recorded_nodes;
