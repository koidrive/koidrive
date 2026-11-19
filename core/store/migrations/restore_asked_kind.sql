-- requires: restore_asked
ALTER TABLE restore_asked ADD COLUMN kind TEXT CHECK (kind IN ('dir', 'file'));
ALTER TABLE restore_asked ADD COLUMN files INTEGER;
