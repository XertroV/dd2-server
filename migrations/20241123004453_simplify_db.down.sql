-- Add down migration script here

CREATE TABLE game_infos (
    id SERIAL PRIMARY KEY,
    info TEXT UNIQUE NOT NULL
);
CREATE TABLE gamer_infos (
    id SERIAL PRIMARY KEY,
    info TEXT NOT NULL
);
CREATE TABLE plugin_infos (
    id SERIAL PRIMARY KEY,
    info TEXT UNIQUE NOT NULL
);
CREATE TABLE game_cam_nods (
    id SERIAL PRIMARY KEY,
    session_token UUID REFERENCES sessions(session_token) NOT NULL,
    context_id UUID REFERENCES contexts(context_id) NOT NULL,
    raw BYTEA NOT NULL,
    init_byte SMALLINT NOT NULL,
    is_race_nod_null BOOLEAN NOT NULL,
    is_editor_cam_null BOOLEAN NOT NULL,
    is_race_88_null BOOLEAN NOT NULL,
    is_cam_1a8_16 BOOLEAN NOT NULL,
    ts TIMESTAMP WITHOUT TIME ZONE NOT NULL DEFAULT NOW()
);
CREATE VIEW user_game_cam_nods AS (
    SELECT s.user_id, g.*
    FROM game_cam_nods AS g
    LEFT JOIN sessions AS s ON s.session_token = g.session_token
);

DROP VIEW most_recent_session;
DROP VIEW recent_sessions;

-- note: not perfect because we can't require NOT NULL
ALTER TABLE sessions ADD COLUMN game_info_id INTEGER REFERENCES game_infos(id);
ALTER TABLE sessions ADD COLUMN gamer_info_id INTEGER REFERENCES gamer_infos(id);
ALTER TABLE sessions ADD COLUMN plugin_info_id INTEGER REFERENCES plugin_infos(id);


CREATE VIEW recent_sessions AS (
    SELECT u.display_name, s.*, ROW_NUMBER() OVER (PARTITION BY s.user_id ORDER BY s.created_ts DESC) as rn
    FROM sessions as s
    INNER JOIN users as u ON s.user_id = u.web_services_user_id
    ORDER BY s.created_ts DESC
);

CREATE VIEW most_recent_session AS (
    select * from recent_sessions where rn = 1
);
