-- Keep cycle tokens after request details are cleaned up. Missing old data
-- stays NULL rather than inventing zero usage.
ALTER TABLE account_spending_entries ADD COLUMN input_tokens NUMERIC;
ALTER TABLE account_spending_entries ADD COLUMN output_tokens NUMERIC;
UPDATE account_spending_entries e SET
    input_tokens = (r.data->'usage'->>'input_tokens')::numeric,
    output_tokens = (r.data->'usage'->>'output_tokens')::numeric
FROM requests r WHERE r.id=e.request_id;
ALTER TABLE account_spending_hourly ADD COLUMN input_tokens NUMERIC;
ALTER TABLE account_spending_hourly ADD COLUMN output_tokens NUMERIC;
ALTER TABLE account_spending_hourly ADD COLUMN missing_tokens BIGINT NOT NULL DEFAULT 0;
UPDATE account_spending_hourly h SET input_tokens=t.input_tokens,
    output_tokens=t.output_tokens, missing_tokens=t.missing_tokens
FROM (SELECT account_id, date_trunc('hour',finished_at,'UTC') AS hour,
    sum(input_tokens) AS input_tokens, sum(output_tokens) AS output_tokens,
    count(*) FILTER (WHERE input_tokens IS NULL OR output_tokens IS NULL) AS missing_tokens
    FROM account_spending_entries GROUP BY account_id,date_trunc('hour',finished_at,'UTC')) t
WHERE h.account_id=t.account_id AND h.hour=t.hour;
