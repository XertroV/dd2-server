-- Add up migration script here

-- Users table
CREATE TABLE users (
    web_services_user_id UUID PRIMARY KEY NOT NULL,
    display_name VARCHAR(75) NOT NULL,
    created_ts TIMESTAMP WITHOUT TIME ZONE NOT NULL DEFAULT NOW(),
    last_login_ts TIMESTAMP WITHOUT TIME ZONE NOT NULL DEFAULT NOW()
);

-- Display names table
CREATE TABLE display_names (
    user_id UUID REFERENCES users(web_services_user_id) NOT NULL,
    display_name VARCHAR(75) NOT NULL,
    created_ts TIMESTAMP WITHOUT TIME ZONE NOT NULL DEFAULT NOW(),
    PRIMARY KEY (user_id, created_ts)
);

-- Plugin infos table
CREATE TABLE plugin_infos (
    id SERIAL PRIMARY KEY,
    info TEXT UNIQUE NOT NULL
);
CREATE INDEX pinfo_idx ON plugin_infos(info);

-- Game infos table
CREATE TABLE game_infos (
    id SERIAL PRIMARY KEY,
    info TEXT UNIQUE NOT NULL
);
CREATE INDEX ginfo_idx ON game_infos(info);

CREATE TABLE gamer_infos (
    id SERIAL PRIMARY KEY,
    info TEXT NOT NULL
);
CREATE INDEX grinfo_idx ON gamer_infos(info);

-- Sessions table
CREATE TABLE sessions (
    session_token UUID PRIMARY KEY,
    user_id UUID REFERENCES users(web_services_user_id),
    created_ts TIMESTAMP WITHOUT TIME ZONE NOT NULL DEFAULT NOW(),
    plugin_info_id INTEGER REFERENCES plugin_infos(id) NOT NULL,
    game_info_id INTEGER REFERENCES game_infos(id) NOT NULL,
    gamer_info_id INTEGER REFERENCES gamer_infos(id) NOT NULL,
    replaced BOOLEAN NOT NULL DEFAULT false,
    ended_ts TIMESTAMP WITHOUT TIME ZONE
);
CREATE INDEX user_id_idx ON sessions(user_id, replaced);


-- Maps table
CREATE TABLE maps (
    map_id SERIAL PRIMARY KEY,
    uid VARCHAR(30) UNIQUE NOT NULL,
    name VARCHAR(255) NOT NULL,
    hash VARCHAR(32) UNIQUE NOT NULL,
    load_count INTEGER NOT NULL DEFAULT 1,
    created_ts TIMESTAMP WITHOUT TIME ZONE NOT NULL DEFAULT NOW()
);
CREATE INDEX uid_idx ON maps(uid);

-- Contexts table
CREATE TABLE contexts (
    context_id UUID PRIMARY KEY NOT NULL,
    session_token UUID REFERENCES sessions(session_token) NOT NULL UNIQUE,
    is_editor BOOLEAN NOT NULL,
    is_mt_editor BOOLEAN NOT NULL,
    is_playground BOOLEAN NOT NULL,
    is_solo BOOLEAN NOT NULL,
    is_server BOOLEAN NOT NULL,
    has_vl_item BOOLEAN NOT NULL,
    map_id INTEGER REFERENCES maps(map_id),
    managers BIGINT NOT NULL,
    created_ts TIMESTAMP WITHOUT TIME ZONE NOT NULL DEFAULT NOW(),
    terminated BOOLEAN NOT NULL,
    predecessor UUID REFERENCES contexts(context_id),
    successor UUID REFERENCES contexts(context_id),
    ended_ts TIMESTAMP WITHOUT TIME ZONE
);
CREATE INDEX session_token_idx ON contexts(session_token);
CREATE INDEX map_id_idx ON contexts(map_id, created_ts);


-- Game cam nods table
CREATE TABLE game_cam_nods (
    id SERIAL PRIMARY KEY,
    context_id UUID REFERENCES contexts(context_id),
    raw BYTEA NOT NULL,
    init_byte SMALLINT NOT NULL,
    is_race_nod_null BOOLEAN NOT NULL,
    is_editor_cam_null BOOLEAN NOT NULL,
    is_race_88_null BOOLEAN NOT NULL,
    is_cam_1a8_16 BOOLEAN NOT NULL,
    ts TIMESTAMP WITHOUT TIME ZONE NOT NULL DEFAULT NOW()
);
CREATE INDEX context_id_idx ON game_cam_nods(context_id);
CREATE INDEX flags_idx ON game_cam_nods(init_byte, is_race_nod_null, is_editor_cam_null, is_race_88_null, is_cam_1a8_16);


-- Position reports table
CREATE TABLE position_reports (
    id SERIAL PRIMARY KEY,
    session_token UUID REFERENCES sessions(session_token),
    user_id UUID REFERENCES users(web_services_user_id),
    context_id UUID REFERENCES contexts(context_id),
    is_official BOOLEAN NOT NULL,
    x DOUBLE PRECISION NOT NULL,
    y DOUBLE PRECISION NOT NULL,
    z DOUBLE PRECISION NOT NULL,
    ts TIMESTAMP WITHOUT TIME ZONE NOT NULL DEFAULT NOW()
);
CREATE INDEX pr_session_token_idx ON position_reports(session_token, ts);
CREATE INDEX pr_user_id_ts_idx ON position_reports(user_id, ts);
CREATE INDEX pr_y_idx ON position_reports(y);

-- Falls table
CREATE TABLE falls (
    id SERIAL PRIMARY KEY,
    session_token UUID REFERENCES sessions(session_token) NOT NULL,
    user_id UUID REFERENCES users(web_services_user_id) NOT NULL,
    start_floor INTEGER NOT NULL,
    start_pos_x DOUBLE PRECISION NOT NULL,
    start_pos_y DOUBLE PRECISION NOT NULL,
    start_pos_z DOUBLE PRECISION NOT NULL,
    start_speed DOUBLE PRECISION NOT NULL,
    start_time INTEGER NOT NULL,
    end_floor INTEGER,
    end_pos_x DOUBLE PRECISION,
    end_pos_y DOUBLE PRECISION,
    end_pos_z DOUBLE PRECISION,
    end_time INTEGER,
    ts TIMESTAMP WITHOUT TIME ZONE NOT NULL DEFAULT NOW()
);
CREATE INDEX falls_session_token_ts_idx on falls(session_token, ts);
CREATE INDEX falls_user_id_ts_idx on falls(user_id, ts);

-- Stats table
CREATE TABLE stats (
    user_id UUID REFERENCES users(web_services_user_id) NOT NULL,
    nb_jumps INTEGER NOT NULL,
    nb_falls INTEGER NOT NULL,
    nb_floors_fallen INTEGER NOT NULL,
    last_pb_set_ts TIMESTAMP WITHOUT TIME ZONE,
    total_dist_fallen DOUBLE PRECISION NOT NULL,
    pb_height DOUBLE PRECISION NOT NULL,
    pb_floor INTEGER NOT NULL,
    nb_resets INTEGER NOT NULL,
    ggs_triggered INTEGER NOT NULL,
    title_gags_triggered INTEGER NOT NULL,
    title_gags_special_triggered INTEGER NOT NULL,
    bye_byes_triggered INTEGER NOT NULL,
    -- array of 19 integers
    monument_triggers JSON NOT NULL,
    reached_floor_count JSON NOT NULL,
    floor_voice_lines_played JSON NOT NULL,
    -- auto incrementing integer on update
    update_count INTEGER NOT NULL DEFAULT 0,
    ts TIMESTAMP WITHOUT TIME ZONE NOT NULL DEFAULT NOW(),
    PRIMARY KEY (user_id)
);

CREATE TABLE stats_archive (
    id SERIAL PRIMARY KEY,
    user_id UUID REFERENCES users(web_services_user_id) NOT NULL,
    nb_jumps INTEGER NOT NULL,
    nb_falls INTEGER NOT NULL,
    nb_floors_fallen INTEGER NOT NULL,
    last_pb_set_ts TIMESTAMP WITHOUT TIME ZONE NOT NULL DEFAULT NOW(),
    total_dist_fallen DOUBLE PRECISION NOT NULL,
    pb_height DOUBLE PRECISION NOT NULL,
    pb_floor INTEGER NOT NULL,
    nb_resets INTEGER NOT NULL,
    ggs_triggered INTEGER NOT NULL,
    title_gags_triggered INTEGER NOT NULL,
    title_gags_special_triggered INTEGER NOT NULL,
    bye_byes_triggered INTEGER NOT NULL,
    -- array of 19 integers
    monument_triggers JSON  NOT NULL,
    reached_floor_count JSON  NOT NULL,
    floor_voice_lines_played JSON  NOT NULL,
    rank_at_time INTEGER NOT NULL,
    update_count INTEGER NOT NULL,
    ts TIMESTAMP WITHOUT TIME ZONE NOT NULL DEFAULT NOW()
);
CREATE INDEX sa_user_id_ts_idx ON stats_archive(user_id, ts);

-- Leaderboard table
CREATE TABLE leaderboard (
    user_id UUID REFERENCES users(web_services_user_id) NOT NULL,
    height DOUBLE PRECISION NOT NULL,
    update_count INTEGER NOT NULL DEFAULT 0,
    ts TIMESTAMP WITHOUT TIME ZONE NOT NULL,
    PRIMARY KEY (user_id)
);
CREATE INDEX lb_height_idx ON leaderboard(height, ts);

-- Leaderboard archive table
CREATE TABLE leaderboard_archive (
    user_id UUID REFERENCES users(web_services_user_id) NOT NULL,
    height DOUBLE PRECISION NOT NULL,
    rank_at_time INTEGER NOT NULL,
    ts TIMESTAMP WITHOUT TIME ZONE NOT NULL DEFAULT NOW(),
    PRIMARY KEY (user_id, ts)
);

-- Friends table
CREATE TABLE friends (
    id SERIAL PRIMARY KEY,
    user_id UUID REFERENCES users(web_services_user_id) NOT NULL,
    friend_id UUID REFERENCES users(web_services_user_id) NOT NULL,
    ts TIMESTAMP WITHOUT TIME ZONE NOT NULL DEFAULT NOW()
);
CREATE INDEX friends_user_id_idx ON friends(user_id, ts);
CREATE INDEX friends_friend_id_idx ON friends(friend_id, ts);
