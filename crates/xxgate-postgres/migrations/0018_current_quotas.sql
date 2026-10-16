-- Keep the history for reports, but read current quotas without scanning it.
-- Block inserts until backfill and trigger installation commit together.
LOCK TABLE quota_snapshots IN SHARE ROW EXCLUSIVE MODE;

CREATE TABLE account_current_quotas (
    account_id UUID NOT NULL,
    pool TEXT NOT NULL,
    window_minutes BIGINT NOT NULL,
    observed_at TIMESTAMPTZ NOT NULL,
    snapshot_id BIGINT NOT NULL,
    data JSONB NOT NULL,
    PRIMARY KEY (account_id, pool, window_minutes)
);

INSERT INTO account_current_quotas
SELECT DISTINCT ON (account_id, pool, window_minutes)
    account_id, pool, window_minutes, observed_at, id, data
FROM quota_snapshots
ORDER BY account_id, pool, window_minutes, observed_at DESC, id DESC;

-- Updating in the inserting transaction also supports older application
-- versions after rollback. Late observations cannot replace newer quotas.
CREATE FUNCTION update_current_quota() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO account_current_quotas
        (account_id, pool, window_minutes, observed_at, snapshot_id, data)
    VALUES (NEW.account_id, NEW.pool, NEW.window_minutes, NEW.observed_at, NEW.id, NEW.data)
    ON CONFLICT (account_id, pool, window_minutes) DO UPDATE
    SET observed_at = EXCLUDED.observed_at,
        snapshot_id = EXCLUDED.snapshot_id,
        data = EXCLUDED.data
    WHERE (account_current_quotas.observed_at, account_current_quotas.snapshot_id)
        < (EXCLUDED.observed_at, EXCLUDED.snapshot_id);
    RETURN NEW;
END;
$$;

CREATE TRIGGER quota_snapshot_current
AFTER INSERT ON quota_snapshots
FOR EACH ROW EXECUTE FUNCTION update_current_quota();
