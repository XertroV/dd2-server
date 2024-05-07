-- Add up migration script here
CREATE VIEW user_to_respawn AS (
    SELECT s.user_id, r.*
    FROM respawns AS r
    LEFT JOIN sessions AS s ON s.session_token = r.session_token
);

CREATE VIEW user_game_cam_nods AS (
    SELECT s.user_id, g.*
    FROM game_cam_nods AS g
    LEFT JOIN sessions AS s ON s.session_token = g.session_token
);

CREATE INDEX falls_start_pos_y_ts_idx ON falls(start_pos_y, ts);
CREATE INDEX falls_end_pos_y_ts_idx ON falls(end_pos_y, ts);

CREATE VIEW falls_only_jumps AS (
    select * from falls WHERE start_pos_y < end_pos_y
);

CREATE VIEW falls_no_jumps AS (
    select * from falls where (start_pos_y - end_pos_y) > 31
);

CREATE VIEW falls_minor AS (
    select * from falls where start_pos_y > end_pos_y AND (start_pos_y - end_pos_y) <= 31
);

ALTER TABLE contexts ADD COLUMN flags_raw bigint DEFAULT 0;

DROP VIEW ranked_lb_view;
CREATE VIEW ranked_lb_view AS (
    SELECT l.user_id, dn.display_name, rank() OVER (ORDER BY l.height DESC) AS rank, l.height, l.ts, l.update_count
    FROM leaderboard AS l
    LEFT JOIN display_names AS dn ON l.user_id = dn.user_id
);
