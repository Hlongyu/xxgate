CREATE TABLE routing_groups (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL CHECK (length(btrim(name)) BETWEEN 1 AND 128),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX routing_groups_name ON routing_groups(lower(name));
INSERT INTO routing_groups(id, name) VALUES ('00000000-0000-0000-0000-000000000001', '默认分组');

ALTER TABLE api_keys ADD COLUMN group_id UUID NOT NULL
    DEFAULT '00000000-0000-0000-0000-000000000001' REFERENCES routing_groups(id);
CREATE INDEX api_keys_group ON api_keys(group_id);

CREATE TABLE account_groups (
    account_id UUID NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    group_id UUID NOT NULL REFERENCES routing_groups(id),
    PRIMARY KEY (account_id, group_id)
);
CREATE INDEX account_groups_group ON account_groups(group_id, account_id);
INSERT INTO account_groups(account_id, group_id)
    SELECT id, '00000000-0000-0000-0000-000000000001' FROM accounts;
UPDATE accounts SET data = data || '{"group_ids":["00000000-0000-0000-0000-000000000001"]}'::jsonb;

-- Old bindings belonged to the only routing scope, now represented by the default group.
UPDATE bindings SET data = data || '{"group_id":"00000000-0000-0000-0000-000000000001"}'::jsonb;
