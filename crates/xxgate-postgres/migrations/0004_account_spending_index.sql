CREATE INDEX requests_account_finished ON requests(account_id, finished_at DESC)
WHERE finished_at IS NOT NULL;
