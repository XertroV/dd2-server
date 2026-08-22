-- Time Trial slice 3: minigame catalog, map bindings, PBs, private attempts.
CREATE TABLE minigame_catalog (
    id BIGSERIAL PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(web_services_user_id) ON DELETE CASCADE,
    name_id VARCHAR(32) NOT NULL,
    slug TEXT NOT NULL,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    lower_is_better BOOL NOT NULL,
    UNIQUE (user_id, name_id, slug)
);
CREATE INDEX minigame_catalog_user_name_idx ON minigame_catalog (user_id, name_id);

CREATE TABLE minigame_on_map (
    id BIGSERIAL PRIMARY KEY,
    map_uid VARCHAR(27) NOT NULL,
    slug TEXT NOT NULL,
    minigame_key CHAR(16) NOT NULL,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    spec_user UUID NOT NULL REFERENCES users(web_services_user_id) ON DELETE CASCADE,
    name_id VARCHAR(32) NOT NULL,
    UNIQUE (map_uid, slug)
);
CREATE INDEX minigame_on_map_uid_idx ON minigame_on_map (map_uid);
CREATE INDEX minigame_on_map_key_idx ON minigame_on_map (minigame_key);

CREATE TABLE minigame_pb (
    id BIGSERIAL PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(web_services_user_id) ON DELETE CASCADE,
    minigame_key CHAR(16) NOT NULL,
    map_uid VARCHAR(27) NOT NULL,
    slug TEXT NOT NULL,
    score BIGINT NOT NULL DEFAULT 0,
    finish_count INT NOT NULL DEFAULT 0,
    attempt_count INT NOT NULL DEFAULT 0,
    last_played TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (user_id, minigame_key)
);
CREATE INDEX minigame_pb_key_score_idx ON minigame_pb (minigame_key, score);

CREATE TABLE minigame_attempt (
    id BIGSERIAL PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(web_services_user_id) ON DELETE CASCADE,
    minigame_key CHAR(16) NOT NULL,
    map_uid VARCHAR(27) NOT NULL,
    slug TEXT NOT NULL,
    elapsed_ms BIGINT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('finish', 'cancel')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX minigame_attempt_user_key_idx ON minigame_attempt (user_id, minigame_key);
