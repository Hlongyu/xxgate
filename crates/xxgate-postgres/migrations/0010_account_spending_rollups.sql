-- Account cards use completion time, while request_hourly uses creation time.
-- Keep a small numeric ledger for exact (from, to] boundaries, and roll up
-- complete UTC hours. Neither table needs the large request JSON on reads.
CREATE TABLE account_spending_entries (
    request_id UUID PRIMARY KEY,
    account_id UUID NOT NULL,
    finished_at TIMESTAMPTZ NOT NULL,
    cny NUMERIC,
    codex_main BOOLEAN NOT NULL
);
CREATE INDEX account_spending_entries_window
    ON account_spending_entries(account_id, finished_at);
CREATE INDEX account_spending_entries_retention ON account_spending_entries(finished_at);

CREATE TABLE account_spending_hourly (
    account_id UUID NOT NULL,
    hour TIMESTAMPTZ NOT NULL,
    cny NUMERIC NOT NULL,
    requests BIGINT NOT NULL,
    unpriced BIGINT NOT NULL,
    codex_cny NUMERIC NOT NULL,
    codex_requests BIGINT NOT NULL,
    codex_unpriced BIGINT NOT NULL,
    PRIMARY KEY (account_id, hour)
);
CREATE INDEX account_spending_hourly_retention ON account_spending_hourly(hour);

-- Backfill persisted historical valuations once; never reprice old requests.
-- Eight days cover both rolling windows and all current weekly quota samples.
INSERT INTO account_spending_entries(request_id, account_id, finished_at, cny, codex_main)
SELECT id, account_id, finished_at, (data->'valuation'->>'cny')::numeric,
       COALESCE((data->>'upstream_attempts')::bigint, 0) > 0
           AND COALESCE(data->>'upstream_model', model) <> 'gpt-5.3-codex-spark'
FROM requests
WHERE account_id IS NOT NULL
  AND finished_at >= date_trunc('hour', now() - interval '8 days', 'UTC');

INSERT INTO account_spending_hourly
    (account_id, hour, cny, requests, unpriced, codex_cny, codex_requests, codex_unpriced)
SELECT account_id, date_trunc('hour', finished_at, 'UTC'),
       COALESCE(sum(cny), 0), count(*), count(*) FILTER (WHERE cny IS NULL),
       COALESCE(sum(cny) FILTER (WHERE codex_main), 0),
       count(*) FILTER (WHERE codex_main),
       count(*) FILTER (WHERE codex_main AND cny IS NULL)
FROM account_spending_entries
GROUP BY account_id, date_trunc('hour', finished_at, 'UTC');
