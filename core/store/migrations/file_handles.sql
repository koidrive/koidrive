-- requires: discarded_shapes, local_changes, local_nodes_placement_and_origin, pending_marks, root_identity
ALTER TABLE local_nodes ADD COLUMN handle INTEGER;
ALTER TABLE recorded_nodes ADD COLUMN handle INTEGER CHECK (ino IS NOT NULL OR handle IS NULL);
