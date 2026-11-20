-- requires: recovery_policy
CREATE TABLE names_left_out (
    node_id TEXT PRIMARY KEY,
    sent_parent_id TEXT,
    sent_name TEXT,
    CHECK ((sent_parent_id IS NULL) = (sent_name IS NULL))
) STRICT, WITHOUT ROWID;
