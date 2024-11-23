-- Add up migration script here

-- Update recent_sessions to not include those columns
DROP VIEW most_recent_session;
DROP VIEW recent_sessions;

ALTER TABLE sessions DROP COLUMN game_info_id;
ALTER TABLE sessions DROP COLUMN gamer_info_id;
ALTER TABLE sessions DROP COLUMN plugin_info_id;

CREATE VIEW recent_sessions AS (
    SELECT u.display_name, s.*, ROW_NUMBER() OVER (PARTITION BY s.user_id ORDER BY s.created_ts DESC) as rn
    FROM sessions as s
    INNER JOIN users as u ON s.user_id = u.web_services_user_id
    ORDER BY s.created_ts DESC
);

CREATE VIEW most_recent_session AS (
    select * from recent_sessions where rn = 1
);

DROP TABLE game_infos;
DROP TABLE gamer_infos;
DROP TABLE plugin_infos;

DROP VIEW user_game_cam_nods;
DROP TABLE game_cam_nods;
