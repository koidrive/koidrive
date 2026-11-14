ALTER TABLE commit_state ADD COLUMN policy_since_ms INTEGER;
UPDATE commit_state
   SET policy_since_ms = CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER)
 WHERE halt = 'policy';
