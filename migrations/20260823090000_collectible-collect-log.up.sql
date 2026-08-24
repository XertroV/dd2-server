-- Append-only first-collect events. Sticky hide/This Map stays on collectible_progress.
-- pos_* nullable only for grant-via-82 (socket 82 has no position yet).
CREATE TABLE collectible_collect_log (
    id BIGSERIAL PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(web_services_user_id) ON DELETE CASCADE,
    map_uid VARCHAR(27) NOT NULL,
    slug TEXT NOT NULL,
    collectible_key CHAR(16) NOT NULL,
    collected_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    pos_x DOUBLE PRECISION,
    pos_y DOUBLE PRECISION,
    pos_z DOUBLE PRECISION,
    source TEXT NOT NULL CHECK (source IN ('zone', 'grant')),
    UNIQUE (user_id, collectible_key)
);
CREATE INDEX collectible_collect_log_uid_idx ON collectible_collect_log (map_uid, user_id);
CREATE INDEX collectible_collect_log_key_idx ON collectible_collect_log (collectible_key);
