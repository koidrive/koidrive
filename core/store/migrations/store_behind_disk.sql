-- requires: recovery_cause, waiting_update_released
ALTER TABLE commit_state ADD COLUMN store_behind_disk INTEGER
    CHECK (store_behind_disk IS NULL OR (store_behind_disk = 1 AND halt IS 'recovery'));
UPDATE commit_state SET store_behind_disk = 1 WHERE halt = 'recovery';
ALTER TABLE waiting_updates ADD COLUMN unproven INTEGER NOT NULL DEFAULT 0
    CHECK (unproven IN (0, 1));
UPDATE waiting_updates SET unproven = 1 WHERE name IS NULL AND slot IS NULL AND released = 0;
