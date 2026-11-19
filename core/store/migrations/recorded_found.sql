-- requires: root_identity
ALTER TABLE recorded_nodes ADD COLUMN found BLOB CHECK (displaced = 0 OR found IS NULL);
