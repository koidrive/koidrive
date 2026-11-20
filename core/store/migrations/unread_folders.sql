-- requires: past_positions_trash_group, question_frozen, recorded_found
CREATE TABLE unread_folders (
    path BLOB PRIMARY KEY,
    ctime_ns INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

CREATE TABLE unread_folders_recorded (
    only INTEGER PRIMARY KEY CHECK (only = 0)
) STRICT;
