ALTER TABLE open_questions ADD COLUMN frozen INTEGER NOT NULL DEFAULT 0;
CREATE TABLE open_question_frozen (
    question_id TEXT NOT NULL REFERENCES open_questions (question_id),
    node_id TEXT NOT NULL,
    parent_id TEXT NOT NULL,
    name TEXT NOT NULL,
    taken INTEGER NOT NULL,
    PRIMARY KEY (question_id, node_id)
) STRICT, WITHOUT ROWID;
