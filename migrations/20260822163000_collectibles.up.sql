-- Collectible catalog, map bindings, sticky unlock/collect progress.
CREATE TABLE collectible_catalog (
    id BIGSERIAL PRIMARY KEY,
    spec_id BIGINT REFERENCES custom_map_aux_specs(id) ON DELETE CASCADE,
    user_id UUID NOT NULL REFERENCES users(web_services_user_id) ON DELETE CASCADE,
    name_id VARCHAR(32) NOT NULL,
    slug TEXT NOT NULL,
    name TEXT NOT NULL,
    pickup TEXT NOT NULL CHECK (pickup IN ('zone', 'grant')),
    gated_by TEXT,
    unlock_score BIGINT,
    UNIQUE (spec_id, slug)
);
CREATE INDEX collectible_catalog_spec_id_idx ON collectible_catalog (spec_id);
CREATE INDEX collectible_catalog_spec_id_slug_idx ON collectible_catalog (spec_id, slug);

CREATE TABLE collectible_on_map (
    id BIGSERIAL PRIMARY KEY,
    map_uid VARCHAR(27) NOT NULL,
    slug TEXT NOT NULL,
    collectible_key CHAR(16) NOT NULL,
    name TEXT NOT NULL,
    pickup TEXT NOT NULL,
    gated_by TEXT,
    unlock_score BIGINT,
    spec_user UUID NOT NULL REFERENCES users(web_services_user_id) ON DELETE CASCADE,
    name_id VARCHAR(32) NOT NULL,
    UNIQUE (map_uid, slug)
);
CREATE INDEX collectible_on_map_uid_idx ON collectible_on_map (map_uid);
CREATE INDEX collectible_on_map_key_idx ON collectible_on_map (collectible_key);

CREATE TABLE collectible_progress (
    id BIGSERIAL PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(web_services_user_id) ON DELETE CASCADE,
    collectible_key CHAR(16) NOT NULL,
    map_uid VARCHAR(27) NOT NULL,
    slug TEXT NOT NULL,
    unlocked_at TIMESTAMPTZ,
    collected_at TIMESTAMPTZ,
    UNIQUE (user_id, collectible_key)
);
CREATE INDEX collectible_progress_uid_idx ON collectible_progress (map_uid, user_id);
