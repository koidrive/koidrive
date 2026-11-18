ALTER TABLE local_nodes ADD COLUMN ctime_ns INTEGER;
UPDATE local_nodes SET ctime_ns = last_write_ns();
