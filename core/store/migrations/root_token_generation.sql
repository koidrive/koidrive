-- requires: root_token
ALTER TABLE link ADD COLUMN root_token_generation INTEGER NOT NULL DEFAULT 0;
