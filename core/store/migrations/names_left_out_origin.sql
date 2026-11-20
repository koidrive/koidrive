-- requires: names_left_out
ALTER TABLE names_left_out ADD COLUMN origin TEXT NOT NULL DEFAULT 'disk_refusal'
    CHECK (origin = 'disk_refusal' OR (origin = 'frozen_holder' AND sent_parent_id IS NOT NULL));
