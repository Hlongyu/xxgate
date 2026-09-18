CREATE INDEX requests_errors_created_desc ON requests (created_at DESC, id DESC)
WHERE state IN ('failed', 'rejected', 'interrupted', 'cancelled');
