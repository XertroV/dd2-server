-- Add up migration script here
DROP VIEW ranked_lb_view;
CREATE VIEW ranked_lb_view AS (
    SELECT l.user_id,
        u.display_name,
        rank() OVER (ORDER BY l.height DESC) AS rank,
        l.height,
        l.ts,
        l.update_count
    FROM leaderboard l
    LEFT JOIN users u ON l.user_id = u.web_services_user_id
    LEFT JOIN shadow_bans AS sb ON l.user_id = sb.user_id
    WHERE sb.user_id IS NULL
);
