-- A single snapshot for both rolling windows and the weekly estimate.
-- Only the two partial boundary hours read numeric entries; all interior
-- hours read incrementally maintained totals. The lower bound is exclusive.
WITH windows(label, starts_at, ends_at, main_only) AS (
    VALUES ('last_5h', $2::timestamptz - interval '5 hours', $2, false),
           ('last_7d', $2::timestamptz - interval '168 hours', $2, false),
           ('weekly', $3::timestamptz, $4::timestamptz, true)
), bounds AS (
    SELECT *, date_trunc('hour', starts_at, 'UTC') AS start_hour,
              date_trunc('hour', ends_at, 'UTC') AS end_hour
    FROM windows
)
SELECT label, totals.*
FROM bounds b
CROSS JOIN LATERAL (
    SELECT COALESCE(sum(cny), 0)::text AS cny,
           COALESCE(sum(requests), 0)::bigint AS requests,
           COALESCE(sum(unpriced), 0)::bigint AS unpriced
    FROM (
        SELECT CASE WHEN b.main_only THEN h.codex_cny ELSE h.cny END AS cny,
               CASE WHEN b.main_only THEN h.codex_requests ELSE h.requests END AS requests,
               CASE WHEN b.main_only THEN h.codex_unpriced ELSE h.unpriced END AS unpriced
        FROM account_spending_hourly h
        WHERE h.account_id = $1 AND h.hour > b.start_hour AND h.hour < b.end_hour
        UNION ALL
        SELECT e.cny, 1, (e.cny IS NULL)::int
        FROM account_spending_entries e
        WHERE e.account_id = $1
          AND e.finished_at > b.starts_at
          AND e.finished_at < b.start_hour + interval '1 hour'
          AND e.finished_at <= b.ends_at
          AND (NOT b.main_only OR e.codex_main)
        UNION ALL
        SELECT e.cny, 1, (e.cny IS NULL)::int
        FROM account_spending_entries e
        WHERE e.account_id = $1 AND b.end_hour > b.start_hour
          AND e.finished_at >= b.end_hour AND e.finished_at <= b.ends_at
          AND (NOT b.main_only OR e.codex_main)
    ) parts
) totals
