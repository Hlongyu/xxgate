-- Existing installations retain their accounts and bindings on node "default".
ALTER TABLE accounts ADD COLUMN node_id TEXT NOT NULL DEFAULT 'default';
CREATE INDEX accounts_node ON accounts(node_id) WHERE deleted_at IS NULL;
ALTER TABLE requests ADD COLUMN node_id TEXT NOT NULL DEFAULT 'default';
CREATE INDEX requests_node_created ON requests(node_id, created_at DESC);
CREATE INDEX requests_node_unfinished ON requests(node_id) WHERE finished_at IS NULL;

ALTER TABLE request_hourly ADD COLUMN node_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE request_hourly DROP CONSTRAINT request_hourly_pkey;
ALTER TABLE request_hourly ADD PRIMARY KEY(hour, account_id, key_id, provider, model, service_tier, node_id);
CREATE INDEX request_hourly_node ON request_hourly(node_id,hour DESC);

CREATE TABLE cluster_nodes (
    id TEXT PRIMARY KEY,
    role TEXT NOT NULL CHECK (role IN ('worker', 'router')),
    url TEXT NOT NULL,
    boot_id UUID NOT NULL,
    heartbeat_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    accepting BOOLEAN NOT NULL DEFAULT false,
    runtime JSONB NOT NULL DEFAULT '{}'
);
CREATE TABLE session_nodes (
    key_id UUID NOT NULL,
    client_session_id TEXT NOT NULL,
    node_id TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY(key_id, client_session_id)
);
INSERT INTO session_nodes(key_id,client_session_id,node_id)
SELECT s.key_id,s.client_session_id,a.node_id FROM sessions s
JOIN bindings b ON b.id=s.binding_id JOIN accounts a ON a.id=b.account_id;
CREATE TABLE cluster_operations (
    id UUID PRIMARY KEY,
    node_id TEXT NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL DEFAULT now()+interval '1 hour'
);
CREATE TABLE cluster_revision (singleton BOOLEAN PRIMARY KEY DEFAULT true CHECK(singleton), version BIGINT NOT NULL);
INSERT INTO cluster_revision VALUES(true,1);
CREATE FUNCTION xxgate_cluster_changed() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE cluster_revision SET version=version+1;
    RETURN NULL;
END $$;
CREATE TRIGGER cluster_keys_changed AFTER INSERT OR DELETE OR UPDATE OF enabled,group_id,name,deleted_at ON api_keys
FOR EACH STATEMENT EXECUTE FUNCTION xxgate_cluster_changed();
CREATE TRIGGER cluster_settings_changed AFTER UPDATE ON settings
FOR EACH STATEMENT EXECUTE FUNCTION xxgate_cluster_changed();
