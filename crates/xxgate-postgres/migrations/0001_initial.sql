CREATE TABLE IF NOT EXISTS administrators (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    password_hash TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE admin_sessions (
    secret_hash TEXT PRIMARY KEY,
    id UUID NOT NULL UNIQUE,
    expires_at TIMESTAMPTZ NOT NULL
);
CREATE TABLE api_keys (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL,
    prefix TEXT NOT NULL,
    secret_hash TEXT NOT NULL UNIQUE,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_used_at TIMESTAMPTZ
);
CREATE TABLE accounts (
    id UUID PRIMARY KEY,
    version BIGINT NOT NULL,
    credential_version BIGINT NOT NULL,
    enabled BOOLEAN NOT NULL,
    data JSONB NOT NULL,
    credentials BYTEA NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE settings (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    version BIGINT NOT NULL,
    data JSONB NOT NULL
);
CREATE TABLE model_catalog (
    id TEXT PRIMARY KEY,
    version BIGINT NOT NULL,
    data JSONB NOT NULL
);
CREATE TABLE prices (
    version BIGSERIAL PRIMARY KEY,
    provider TEXT NOT NULL,
    access_kind TEXT NOT NULL,
    model TEXT NOT NULL,
    data JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX prices_model_version ON prices(provider, access_kind, model, version DESC);
CREATE TABLE sessions (
    key_id UUID NOT NULL,
    client_session_id TEXT NOT NULL,
    generation BIGINT NOT NULL DEFAULT 0,
    binding_id UUID,
    PRIMARY KEY (key_id, client_session_id)
);
CREATE TABLE bindings (
    id UUID PRIMARY KEY,
    key_id UUID NOT NULL,
    client_session_id TEXT NOT NULL,
    generation BIGINT NOT NULL,
    account_id UUID NOT NULL REFERENCES accounts(id),
    data JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE(key_id, client_session_id, generation)
);
CREATE TABLE identity_mappings (
    binding_id UUID NOT NULL REFERENCES bindings(id),
    kind TEXT NOT NULL,
    client_id TEXT NOT NULL,
    upstream_id TEXT NOT NULL,
    PRIMARY KEY (binding_id, kind, client_id),
    UNIQUE (binding_id, kind, upstream_id)
);
CREATE TABLE requests (
    id UUID PRIMARY KEY,
    key_id UUID NOT NULL,
    account_id UUID,
    client_session_id TEXT NOT NULL,
    model TEXT NOT NULL,
    state TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    finished_at TIMESTAMPTZ,
    data JSONB NOT NULL
);
CREATE INDEX requests_created ON requests(created_at DESC, id);
CREATE INDEX requests_account_created ON requests(account_id, created_at DESC);
CREATE INDEX requests_session ON requests(key_id, client_session_id, created_at DESC);
CREATE INDEX requests_state ON requests(state, created_at DESC);
CREATE TABLE audit_events (
    id UUID PRIMARY KEY,
    request_id UUID,
    account_id UUID,
    kind TEXT NOT NULL,
    actor TEXT NOT NULL,
    at TIMESTAMPTZ NOT NULL,
    data JSONB NOT NULL
);
CREATE INDEX audit_events_request ON audit_events(request_id, at);
CREATE INDEX audit_events_time ON audit_events(at DESC);
CREATE INDEX audit_events_admin ON audit_events(at DESC) WHERE request_id IS NULL;
CREATE TABLE quota_snapshots (
    id BIGSERIAL PRIMARY KEY,
    account_id UUID NOT NULL REFERENCES accounts(id),
    pool TEXT NOT NULL,
    window_minutes BIGINT NOT NULL DEFAULT -1,
    observed_at TIMESTAMPTZ NOT NULL,
    data JSONB NOT NULL
);
CREATE INDEX quota_account_window ON quota_snapshots(account_id, pool, window_minutes, observed_at DESC);
CREATE TABLE request_hourly (
    hour TIMESTAMPTZ NOT NULL,
    account_id UUID NOT NULL,
    key_id UUID NOT NULL,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    service_tier TEXT NOT NULL,
    requests BIGINT NOT NULL DEFAULT 0,
    completed BIGINT NOT NULL DEFAULT 0,
    failed BIGINT NOT NULL DEFAULT 0,
    cancelled BIGINT NOT NULL DEFAULT 0,
    input_tokens NUMERIC(30,0) NOT NULL DEFAULT 0,
    output_tokens NUMERIC(30,0) NOT NULL DEFAULT 0,
    cached_tokens NUMERIC(30,0) NOT NULL DEFAULT 0,
    image_count BIGINT NOT NULL DEFAULT 0,
    cny NUMERIC(38,8) NOT NULL DEFAULT 0,
    unpriced BIGINT NOT NULL DEFAULT 0,
    incomplete_usage BIGINT NOT NULL DEFAULT 0,
    duration_ms NUMERIC(30,0) NOT NULL DEFAULT 0,
    PRIMARY KEY(hour, account_id, key_id, provider, model, service_tier)
);
CREATE INDEX request_hourly_account ON request_hourly(account_id, hour DESC);

