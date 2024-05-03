-- Add up migration script here

CREATE TABLE cached_json (
    id SERIAL PRIMARY KEY,
    key VARCHAR(64) UNIQUE NOT NULL,
    value JSONB NOT NULL
);
CREATE INDEX cached_json_key_idx ON cached_json(key);
