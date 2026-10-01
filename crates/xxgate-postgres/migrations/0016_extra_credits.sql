CREATE TABLE account_extra_credits (
    account_id uuid PRIMARY KEY REFERENCES accounts(id),
    observed_at timestamptz NOT NULL,
    data jsonb NOT NULL
);
