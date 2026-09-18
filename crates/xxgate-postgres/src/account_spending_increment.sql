WITH entry AS (
    INSERT INTO account_spending_entries(request_id, account_id, finished_at, cny, codex_main)
    VALUES ($1, $2, $3, $4, $5)
    ON CONFLICT (request_id) DO NOTHING
    RETURNING *
)
INSERT INTO account_spending_hourly
    (account_id, hour, cny, requests, unpriced, codex_cny, codex_requests, codex_unpriced)
SELECT account_id, date_trunc('hour', finished_at, 'UTC'), COALESCE(cny, 0), 1,
       (cny IS NULL)::int, CASE WHEN codex_main THEN COALESCE(cny, 0) ELSE 0 END,
       codex_main::int, (codex_main AND cny IS NULL)::int
FROM entry
ON CONFLICT (account_id, hour) DO UPDATE SET
    cny = account_spending_hourly.cny + EXCLUDED.cny,
    requests = account_spending_hourly.requests + EXCLUDED.requests,
    unpriced = account_spending_hourly.unpriced + EXCLUDED.unpriced,
    codex_cny = account_spending_hourly.codex_cny + EXCLUDED.codex_cny,
    codex_requests = account_spending_hourly.codex_requests + EXCLUDED.codex_requests,
    codex_unpriced = account_spending_hourly.codex_unpriced + EXCLUDED.codex_unpriced
