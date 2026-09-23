CREATE TABLE model_discovery_version (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    version TEXT NOT NULL,
    synced_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
