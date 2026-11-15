ALTER TABLE link ADD COLUMN found_marker INTEGER NOT NULL DEFAULT 0 CHECK (found_marker IN (0, 1));
