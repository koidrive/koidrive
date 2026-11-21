CREATE TABLE write_stamp (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    stamp INTEGER NOT NULL CHECK (stamp > 0)
) STRICT;
