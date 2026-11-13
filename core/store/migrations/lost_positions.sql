-- requires: moved_here
CREATE TABLE lost_positions (
    node_id TEXT PRIMARY KEY,
    parent_id TEXT NOT NULL,
    name TEXT NOT NULL
) STRICT, WITHOUT ROWID;

CREATE TABLE lost_creates (
    node_id TEXT PRIMARY KEY
) STRICT, WITHOUT ROWID;

UPDATE moved_here SET upgraded = 1
 WHERE NOT EXISTS (
    SELECT 1 FROM past_positions
     WHERE past_positions.node_id = moved_here.node_id
       AND past_positions.rev < moved_here.seq
 );
