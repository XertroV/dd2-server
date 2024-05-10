-- Add down migration script here
DROP VIEW ranked_lb_view;
CREATE VIEW ranked_lb_view AS (
    SELECT l.user_id, dn.display_name, rank() OVER (ORDER BY l.height DESC) AS rank, l.height, l.ts, l.update_count
    FROM leaderboard AS l
    LEFT JOIN display_names AS dn ON l.user_id = dn.user_id
);
