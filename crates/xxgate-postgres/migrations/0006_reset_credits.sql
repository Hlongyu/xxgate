CREATE TABLE account_reset_credits (
    account_id uuid PRIMARY KEY REFERENCES accounts(id),
    data jsonb NOT NULL
);

CREATE TABLE account_reset_operations (
    id uuid PRIMARY KEY,
    account_id uuid NOT NULL REFERENCES accounts(id),
    completed boolean NOT NULL DEFAULT false,
    data jsonb NOT NULL
);
CREATE UNIQUE INDEX one_pending_reset_per_account
    ON account_reset_operations(account_id) WHERE NOT completed;
