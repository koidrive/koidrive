-- requires: recovery_cause
ALTER TABLE commit_state ADD COLUMN store_behind_disk INTEGER
    CHECK (store_behind_disk IS NULL OR (store_behind_disk = 1 AND halt IS 'recovery'));
UPDATE commit_state SET store_behind_disk = 1 WHERE halt = 'recovery';
ALTER TABLE waiting_updates ADD COLUMN store_behind_disk INTEGER NOT NULL DEFAULT 0
    CHECK (store_behind_disk IN (0, 1));
