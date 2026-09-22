-- Thirty-day quota cycles need more than the previous eight-day ledger.
-- Preserve frozen valuations and fill only retained requests absent from it.
-- Old request details may already be gone; record the conservative boundary
-- before which completeness cannot be guaranteed for existing accounts.
CREATE TABLE account_spending_coverage (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    complete_since TIMESTAMPTZ NOT NULL
);
INSERT INTO account_spending_coverage(singleton, complete_since)
VALUES (TRUE, date_trunc('hour', now() - interval '8 days', 'UTC'));

INSERT INTO account_spending_entries
    (request_id, account_id, finished_at, cny, codex_main, input_tokens, output_tokens)
SELECT id, account_id, finished_at, (data->'valuation'->>'cny')::numeric,
       COALESCE((data->>'upstream_attempts')::bigint, 0) > 0
           AND COALESCE(data->>'upstream_model', model) <> 'gpt-5.3-codex-spark',
       (data->'usage'->>'input_tokens')::numeric,
       (data->'usage'->>'output_tokens')::numeric
FROM requests
WHERE account_id IS NOT NULL
  AND finished_at >= date_trunc('hour', now() - interval '31 days', 'UTC')
ON CONFLICT (request_id) DO NOTHING;

INSERT INTO account_spending_hourly
    (account_id, hour, cny, requests, unpriced, codex_cny, codex_requests,
     codex_unpriced, input_tokens, output_tokens, missing_tokens)
SELECT account_id, date_trunc('hour', finished_at, 'UTC'),
       COALESCE(sum(cny), 0), count(*), count(*) FILTER (WHERE cny IS NULL),
       COALESCE(sum(cny) FILTER (WHERE codex_main), 0),
       count(*) FILTER (WHERE codex_main),
       count(*) FILTER (WHERE codex_main AND cny IS NULL),
       sum(input_tokens), sum(output_tokens),
       count(*) FILTER (WHERE input_tokens IS NULL OR output_tokens IS NULL)
FROM account_spending_entries
GROUP BY account_id, date_trunc('hour', finished_at, 'UTC')
ON CONFLICT (account_id, hour) DO UPDATE SET
    cny=EXCLUDED.cny, requests=EXCLUDED.requests, unpriced=EXCLUDED.unpriced,
    codex_cny=EXCLUDED.codex_cny, codex_requests=EXCLUDED.codex_requests,
    codex_unpriced=EXCLUDED.codex_unpriced, input_tokens=EXCLUDED.input_tokens,
    output_tokens=EXCLUDED.output_tokens, missing_tokens=EXCLUDED.missing_tokens;
