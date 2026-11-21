CREATE TABLE answered_rules (rules_file_id TEXT PRIMARY KEY, folder_id TEXT NOT NULL, gitignore INTEGER NOT NULL, rules BLOB NOT NULL) STRICT, WITHOUT ROWID;
