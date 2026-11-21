-- requires: file_handles, missing_nodes, recorded_found
ALTER TABLE local_nodes ADD COLUMN prior_displaced_parent TEXT;
ALTER TABLE local_nodes ADD COLUMN copy_volume BLOB
    CHECK (copy_volume IS NULL OR (birth_ns IS NOT NULL AND length(copy_volume) IN (8, 16)));

INSERT INTO local_nodes (node_id, parent_id, name, kind, content, ino, ino_high, birth_ns, parked, handle)
SELECT recorded.node_id,
       CASE WHEN synced.node_id IS NULL THEN remote.parent_id ELSE synced.parent_id END,
       CASE WHEN synced.node_id IS NULL THEN remote.name ELSE synced.name END,
       CASE WHEN synced.node_id IS NULL THEN remote.kind ELSE synced.kind END,
       matching_current_content(synced.content, remote.current),
       recorded.ino,
       recorded.ino_high,
       recorded.birth_ns,
       0,
       recorded.handle
  FROM recorded_nodes AS recorded
  LEFT JOIN synced_nodes AS synced ON synced.node_id = recorded.node_id
  LEFT JOIN remote_nodes AS remote ON remote.node_id = recorded.node_id
 WHERE recorded.displaced = 1
   AND (synced.node_id IS NOT NULL OR remote.node_id IS NOT NULL)
   AND NOT EXISTS (SELECT 1 FROM local_nodes WHERE local_nodes.node_id = recorded.node_id);

CREATE TEMP TABLE displaced_pending_create AS
SELECT value ->> 0 AS node_id,
       value ->> 1 AS parent_id,
       value ->> 2 AS name,
       value ->> 3 AS kind,
       unhex(value ->> 4) AS content
  FROM commit_in_flight, json_each(in_flight_creates(commit_message))
 WHERE NOT EXISTS (SELECT 1 FROM remote_nodes WHERE remote_nodes.node_id = value ->> 0);

INSERT INTO local_nodes (node_id, parent_id, name, kind, content, ino, ino_high, birth_ns, parked, handle)
SELECT recorded.node_id, pending.parent_id, pending.name, pending.kind, pending.content,
       recorded.ino, recorded.ino_high, recorded.birth_ns, 0, recorded.handle
  FROM recorded_nodes AS recorded
  JOIN displaced_pending_create AS pending USING (node_id)
 WHERE recorded.displaced = 1
   AND NOT EXISTS (SELECT 1 FROM local_nodes WHERE local_nodes.node_id = recorded.node_id);

DELETE FROM recorded_nodes
 WHERE displaced = 1
   AND NOT EXISTS (SELECT 1 FROM local_nodes WHERE local_nodes.node_id = recorded_nodes.node_id);

DELETE FROM missing_nodes
 WHERE NOT EXISTS (
       SELECT 1 FROM recorded_nodes
        WHERE recorded_nodes.node_id = missing_nodes.node_id AND recorded_nodes.displaced = 1);

CREATE TABLE recorded_nodes_by_kind (
    node_id TEXT NOT NULL,
    displaced INTEGER NOT NULL CHECK (displaced IN (0, 1)),
    ino INTEGER,
    birth_ns INTEGER,
    ino_high INTEGER NOT NULL DEFAULT 0 CHECK (ino IS NOT NULL OR ino_high = 0),
    lost INTEGER NOT NULL DEFAULT 0 CHECK (lost IN (0, 1)),
    handle INTEGER CHECK (ino IS NOT NULL OR handle IS NULL),
    found BLOB CHECK (displaced = 0 OR found IS NULL),
    awaiting_create INTEGER NOT NULL DEFAULT 0 CHECK (awaiting_create IN (0, displaced)),
    PRIMARY KEY (node_id, displaced),
    CHECK ((displaced = 1) = (ino IS NOT NULL)),
    CHECK (ino IS NOT NULL OR birth_ns IS NULL)
) STRICT, WITHOUT ROWID;
INSERT INTO recorded_nodes_by_kind (node_id, displaced, ino, birth_ns, ino_high, lost, handle, found, awaiting_create)
SELECT node_id, displaced, ino, birth_ns, ino_high, lost, handle, found,
       displaced = 1 AND EXISTS (
           SELECT 1 FROM displaced_pending_create AS pending WHERE pending.node_id = recorded_nodes.node_id)
  FROM recorded_nodes;
DROP TABLE recorded_nodes;
ALTER TABLE recorded_nodes_by_kind RENAME TO recorded_nodes;
DROP TABLE displaced_pending_create;
