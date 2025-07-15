-- Add up migration script here
CREATE TABLE custom_map_stats (
    id SERIAL PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(web_services_user_id) ON DELETE CASCADE,
    map_uid VARCHAR(27) NOT NULL,
    stats JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(user_id, map_uid)
);
CREATE INDEX idx_custom_map_stats_map_uid ON custom_map_stats (map_uid);
