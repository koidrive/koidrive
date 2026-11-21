CREATE TABLE born_as_temp (
    node_id TEXT PRIMARY KEY,
    upgraded INTEGER NOT NULL CHECK (upgraded IN (0, 1))
) STRICT, WITHOUT ROWID;

INSERT INTO born_as_temp (node_id, upgraded)
SELECT node_id, 1
  FROM local_nodes
 WHERE kind = 2
   AND temp_name(name);
