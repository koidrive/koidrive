ALTER TABLE commit_state ADD COLUMN recovery_policy_code INTEGER
    CHECK (recovery_policy_code IS NULL OR halt IS 'recovery');
