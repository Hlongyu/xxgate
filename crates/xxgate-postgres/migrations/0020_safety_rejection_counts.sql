-- Count actual upstream safety rejections, never gateway-blocked retries.
ALTER TABLE safety_rejections ADD COLUMN rejection_count INTEGER NOT NULL DEFAULT 1
    CHECK (rejection_count BETWEEN 1 AND 3);
-- Retention may have removed older requests; the durable row proves at least one.
UPDATE safety_rejections s SET rejection_count = GREATEST(1, LEAST(3, (
    SELECT count(*) FROM requests r
    WHERE r.key_id=s.key_id AND r.client_session_id=s.session_id
      AND r.data->>'error_code'='upstream_content_policy_violation'
)));
