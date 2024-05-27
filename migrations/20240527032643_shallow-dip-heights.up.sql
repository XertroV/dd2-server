-- Add up migration script here
-- CREATE TABLE sd_curr_heights (
--   user_id UUID PRIMARY KEY REFERENCES users(web_services_user_id) ON DELETE CASCADE,
--   height DOUBLE PRECISION NOT NULL,
--   updated_at TIMESTAMP WITHOUT TIME ZONE DEFAULT CURRENT_TIMESTAMP,
--   update_count INTEGER NOT NULL DEFAULT 0
-- );
-- CREATE INDEX sd_curr_heights_height_idx ON sd_curr_heights(height DESC);

-- CREATE TABLE sd_pb_leaderboard (
--     user_id UUID PRIMARY KEY REFERENCES users(web_services_user_id) ON DELETE CASCADE,
--     height DOUBLE PRECISION NOT NULL,
--     updated_at TIMESTAMP WITHOUT TIME ZONE DEFAULT CURRENT_TIMESTAMP,
--     update_count INTEGER NOT NULL DEFAULT 0
-- );
-- CREATE INDEX sd_pb_leaderboard_height_idx ON sd_pb_leaderboard(height DESC);

CREATE TABLE map_curr_heights (
    id SERIAL PRIMARY KEY,
    map_uid VARCHAR(27) NOT NULL,
    user_id UUID NOT NULL REFERENCES users(web_services_user_id) ON DELETE CASCADE,
    height DOUBLE PRECISION NOT NULL,
    pos DOUBLE PRECISION[3] NOT NULL,
    race_time INTEGER NOT NULL,
    updated_at TIMESTAMP WITHOUT TIME ZONE NOT NULL DEFAULT CURRENT_TIMESTAMP,
    update_count INTEGER NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX map_curr_heights_user_map_idx ON map_curr_heights(user_id, map_uid);
CREATE INDEX map_curr_heights_users_heights_idx ON map_curr_heights(user_id, updated_at DESC, height DESC);
CREATE INDEX map_curr_heights_map_height_idx ON map_curr_heights(map_uid, updated_at DESC, height DESC);

CREATE TABLE map_leaderboard (
    id SERIAL PRIMARY KEY,
    map_uid VARCHAR(27) NOT NULL,
    user_id UUID NOT NULL REFERENCES users(web_services_user_id) ON DELETE CASCADE,
    height DOUBLE PRECISION NOT NULL,
    pos DOUBLE PRECISION[3] NOT NULL,
    race_time INTEGER NOT NULL,
    updated_at TIMESTAMP WITHOUT TIME ZONE NOT NULL DEFAULT CURRENT_TIMESTAMP,
    update_count INTEGER NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX map_leaderboard_user_map_idx ON map_leaderboard(user_id, map_uid);
CREATE INDEX map_leaderboard_user_height_idx ON map_leaderboard(user_id, height DESC, updated_at DESC);
CREATE INDEX map_leaderboard_map_height_idx ON map_leaderboard(map_uid, height DESC, updated_at DESC);
