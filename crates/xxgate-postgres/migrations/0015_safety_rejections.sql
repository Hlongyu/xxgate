-- Session decisions are independent of request retention and survive restarts.
-- No request body, content fingerprint, or raw upstream error text is stored.
CREATE TABLE safety_rejections (
    key_id UUID NOT NULL,
    session_id TEXT NOT NULL,
    data JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (key_id, session_id)
);
