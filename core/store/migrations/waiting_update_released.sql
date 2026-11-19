ALTER TABLE waiting_updates ADD COLUMN released INTEGER NOT NULL DEFAULT 0 CHECK (released IN (0, 1));
