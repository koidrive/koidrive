-- requires: question_frozen
CREATE TABLE discard_stays (
    node_id TEXT NOT NULL,
    folder_id TEXT NOT NULL,
    PRIMARY KEY (node_id, folder_id)
) STRICT, WITHOUT ROWID;

CREATE TABLE discard_answered (
    folder_id TEXT PRIMARY KEY
) STRICT, WITHOUT ROWID;

CREATE TABLE first_link_held (
    node_id TEXT PRIMARY KEY,
    parent_id TEXT NOT NULL
) STRICT, WITHOUT ROWID;

CREATE TABLE first_link_open (
    node_id TEXT PRIMARY KEY,
    asked INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

ALTER TABLE open_questions ADD COLUMN frozen_whole INTEGER NOT NULL DEFAULT 0;

ALTER TABLE open_question_frozen ADD COLUMN member INTEGER NOT NULL DEFAULT 0;

ALTER TABLE open_question_frozen ADD COLUMN member_of TEXT;
