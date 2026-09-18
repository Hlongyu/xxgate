-- Match the complete predecessor lookup, including its stable UUID tie break.
-- Only dispatched Responses requests can be a cache baseline.
CREATE INDEX requests_cache_predecessor ON requests
    (key_id, client_session_id, (data->>'client_thread_id'), created_at DESC, id DESC)
    WHERE COALESCE(data->>'kind','responses')='responses'
      AND (data->>'upstream_attempts')::int>0;

CREATE INDEX requests_created_desc ON requests (created_at DESC, id DESC);
