-- requires: moved_here
ALTER TABLE remote_nodes ADD COLUMN place_intent BLOB;
ALTER TABLE moved_here ADD COLUMN move_history BLOB;
ALTER TABLE commit_in_flight ADD COLUMN move_bases BLOB;
