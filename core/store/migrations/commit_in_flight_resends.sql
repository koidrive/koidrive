CREATE TABLE commit_in_flight_resends (
    node_id TEXT PRIMARY KEY
) STRICT, WITHOUT ROWID;

INSERT INTO commit_in_flight_resends (node_id)
SELECT node_id
  FROM resends
 WHERE EXISTS (SELECT 1 FROM commit_in_flight);
