ALTER TABLE local_nodes
    ADD COLUMN left_out_when_placed INTEGER NOT NULL DEFAULT 0
    CHECK (left_out_when_placed IN (0, 1));

ALTER TABLE local_nodes
    ADD COLUMN content_from_server INTEGER NOT NULL DEFAULT 0
    CHECK (content_from_server IN (0, 1));
