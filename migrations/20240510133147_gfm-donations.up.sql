-- Add up migration script here
CREATE TABLE gfm_donations (
    id SERIAL PRIMARY KEY,
    ts TIMESTAMP WITHOUT TIME ZONE NOT NULL DEFAULT NOW(),
    amount DOUBLE PRECISION NOT NULL
);
CREATE INDEX gfm_donations_ts ON gfm_donations(ts);
