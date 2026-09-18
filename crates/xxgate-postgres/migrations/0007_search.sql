CREATE TABLE search_prices (
    version bigserial PRIMARY KEY,
    data jsonb NOT NULL
);

ALTER TABLE request_hourly ADD COLUMN search_calls bigint NOT NULL DEFAULT 0;
ALTER TABLE request_hourly ADD COLUMN search_cny numeric NOT NULL DEFAULT 0;
CREATE INDEX requests_kind_created ON requests ((COALESCE(data->>'kind','responses')), created_at DESC);
