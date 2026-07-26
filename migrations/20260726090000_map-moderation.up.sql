-- Map metadata cache (Nadeo author + display fields)
CREATE TABLE map_meta (
    map_uid VARCHAR(27) PRIMARY KEY NOT NULL,
    author_id UUID NOT NULL,
    map_name TEXT,
    thumbnail_url TEXT,
    fetched_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX map_meta_author_id_idx ON map_meta (author_id);

-- Platform Super-mods
CREATE TABLE super_mods (
    user_id UUID PRIMARY KEY NOT NULL REFERENCES users (web_services_user_id) ON DELETE CASCADE,
    granted_by UUID REFERENCES users (web_services_user_id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Ensure bootstrap Super-mod user exists (XertroV)
INSERT INTO users (web_services_user_id, display_name)
VALUES ('0a2d1bc0-4aaa-4374-b2db-3d561bdab1c9', 'XertroV')
ON CONFLICT (web_services_user_id) DO NOTHING;

INSERT INTO super_mods (user_id, granted_by)
VALUES ('0a2d1bc0-4aaa-4374-b2db-3d561bdab1c9', NULL)
ON CONFLICT (user_id) DO NOTHING;

-- Map-local Delegates
CREATE TABLE map_delegates (
    map_uid VARCHAR(27) NOT NULL,
    user_id UUID NOT NULL REFERENCES users (web_services_user_id) ON DELETE CASCADE,
    granted_by UUID NOT NULL REFERENCES users (web_services_user_id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (map_uid, user_id)
);
CREATE INDEX map_delegates_user_id_idx ON map_delegates (user_id);

-- Map Bans (soft-hide from public LB/live; rows retained)
CREATE TABLE map_bans (
    map_uid VARCHAR(27) NOT NULL,
    user_id UUID NOT NULL REFERENCES users (web_services_user_id) ON DELETE CASCADE,
    private_reason TEXT NOT NULL,
    public_message TEXT NOT NULL,
    created_by UUID NOT NULL REFERENCES users (web_services_user_id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ,
    lifted_at TIMESTAMPTZ,
    lifted_by UUID REFERENCES users (web_services_user_id) ON DELETE SET NULL,
    PRIMARY KEY (map_uid, user_id)
);
CREATE INDEX map_bans_active_idx ON map_bans (map_uid)
    WHERE lifted_at IS NULL;

-- Unofficial standing mark per (user, map)
CREATE TABLE map_unofficial (
    map_uid VARCHAR(27) NOT NULL,
    user_id UUID NOT NULL REFERENCES users (web_services_user_id) ON DELETE CASCADE,
    source TEXT NOT NULL DEFAULT 'ban',
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_by UUID REFERENCES users (web_services_user_id) ON DELETE SET NULL,
    PRIMARY KEY (map_uid, user_id)
);

-- Web admin sessions (TM OAuth)
CREATE TABLE web_sessions (
    session_id UUID PRIMARY KEY NOT NULL,
    user_id UUID NOT NULL REFERENCES users (web_services_user_id) ON DELETE CASCADE,
    display_name TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ NOT NULL,
    oauth_access_token TEXT,
    oauth_refresh_token TEXT
);
CREATE INDEX web_sessions_user_id_idx ON web_sessions (user_id);
CREATE INDEX web_sessions_expires_at_idx ON web_sessions (expires_at);

-- OAuth CSRF state
CREATE TABLE oauth_states (
    state TEXT PRIMARY KEY NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ NOT NULL
);
