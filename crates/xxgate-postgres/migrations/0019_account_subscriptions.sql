CREATE TABLE account_subscriptions (
    account_id uuid PRIMARY KEY REFERENCES accounts(id),
    checked_at timestamptz NOT NULL,
    data jsonb NOT NULL
);
