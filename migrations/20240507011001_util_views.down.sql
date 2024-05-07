-- Add down migration script here
DROP VIEW ranked_lb_view;
CREATE VIEW ranked_lb_view AS (
    SELECT l.user_id, dn.display_name, rank() OVER (ORDER BY l.height DESC) AS rank, l.height, l.ts
    FROM leaderboard AS l
    LEFT JOIN display_names AS dn ON l.user_id = dn.user_id
);

ALTER TABLE contexts DROP COLUMN flags_raw;

DROP INDEX falls_start_pos_y_ts_idx;
DROP INDEX falls_end_pos_y_ts_idx;

DROP VIEW user_to_respawn;
DROP VIEW user_game_cam_nods;
DROP VIEW falls_only_jumps;
DROP VIEW falls_no_jumps;
DROP VIEW falls_minor;
