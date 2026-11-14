ALTER TABLE remote_nodes ADD COLUMN pending_for TEXT;
ALTER TABLE remote_nodes ADD COLUMN pending_by TEXT;
ALTER TABLE synced_nodes ADD COLUMN pending_for TEXT;
CREATE TABLE unsent_marks (
    node_id TEXT PRIMARY KEY,
    pending_for TEXT NOT NULL
) STRICT, WITHOUT ROWID;
CREATE TABLE pending_finishes (
    node_id TEXT PRIMARY KEY,
    parent_id TEXT NOT NULL,
    pending_name TEXT NOT NULL,
    name TEXT NOT NULL,
    meta_rev INTEGER,
    disk_first INTEGER NOT NULL DEFAULT 0 CHECK (disk_first IN (0, 1)),
    base INTEGER,
    sending INTEGER NOT NULL CHECK (sending IN (0, 1))
) STRICT, WITHOUT ROWID;
INSERT INTO pending_finishes (node_id, parent_id, pending_name, name, sending)
SELECT p.node_id, r.parent_id, p.sent, p.name, 0
FROM pending_names p
JOIN remote_nodes r ON r.node_id = p.node_id
JOIN synced_nodes s ON s.node_id = p.node_id
WHERE p.landed = 1
    AND r.status = 1
    AND r.parent_id IS NOT NULL
    AND r.name = p.sent
    AND s.name = p.sent
    AND s.parent_id IS r.parent_id;
INSERT INTO pending_finishes (node_id, parent_id, pending_name, name, sending)
SELECT p.node_id, l.parent_id, p.sent, p.name, 0
FROM pending_names p
JOIN local_nodes l ON l.node_id = p.node_id
WHERE p.landed = 0
    AND l.parent_id IS NOT NULL
    AND l.name = p.sent
    AND EXISTS (SELECT 1 FROM commit_in_flight);
DROP TABLE pending_names;
