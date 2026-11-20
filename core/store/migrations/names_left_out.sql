-- requires: recovery_policy
CREATE TABLE names_left_out (
    node_id TEXT PRIMARY KEY,
    sending INTEGER NOT NULL CHECK (sending IN (0, 1))
) STRICT, WITHOUT ROWID;
