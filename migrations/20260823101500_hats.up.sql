-- Hat catalog, map bindings, sticky unlocks, per-map equipped.
-- Unlocks are server-authoritative. Bans do not participate (no ban FKs / filters).
CREATE TABLE hat_catalog (
    id BIGSERIAL PRIMARY KEY,
    spec_id BIGINT REFERENCES custom_map_aux_specs(id) ON DELETE CASCADE,
    user_id UUID NOT NULL REFERENCES users(web_services_user_id) ON DELETE CASCADE,
    name_id VARCHAR(32) NOT NULL,
    slug TEXT NOT NULL,
    name TEXT NOT NULL,
    finish_this_map BOOLEAN NOT NULL DEFAULT false,
    record_uids JSONB NOT NULL DEFAULT '[]'::jsonb,
    UNIQUE (spec_id, slug)
);
CREATE INDEX hat_catalog_spec_id_idx ON hat_catalog (spec_id);
CREATE INDEX hat_catalog_spec_id_slug_idx ON hat_catalog (spec_id, slug);

CREATE TABLE hat_on_map (
    id BIGSERIAL PRIMARY KEY,
    map_uid VARCHAR(27) NOT NULL,
    slug TEXT NOT NULL,
    hat_key CHAR(16) NOT NULL,
    name TEXT NOT NULL,
    finish_this_map BOOLEAN NOT NULL,
    record_uids JSONB NOT NULL DEFAULT '[]'::jsonb,
    spec_user UUID NOT NULL REFERENCES users(web_services_user_id) ON DELETE CASCADE,
    name_id VARCHAR(32) NOT NULL,
    UNIQUE (map_uid, slug)
);
CREATE INDEX hat_on_map_uid_idx ON hat_on_map (map_uid);
CREATE INDEX hat_on_map_key_idx ON hat_on_map (hat_key);

CREATE TABLE hat_unlock (
    id BIGSERIAL PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(web_services_user_id) ON DELETE CASCADE,
    hat_key CHAR(16) NOT NULL,
    map_uid VARCHAR(27) NOT NULL,
    slug TEXT NOT NULL,
    unlocked_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (user_id, hat_key)
);
CREATE INDEX hat_unlock_uid_idx ON hat_unlock (map_uid, user_id);
CREATE INDEX hat_unlock_user_idx ON hat_unlock (user_id);

CREATE TABLE hat_equipped (
    user_id UUID NOT NULL REFERENCES users(web_services_user_id) ON DELETE CASCADE,
    map_uid VARCHAR(27) NOT NULL,
    slug TEXT NOT NULL,
    hat_key CHAR(16) NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, map_uid)
);
CREATE INDEX hat_equipped_map_idx ON hat_equipped (map_uid);
