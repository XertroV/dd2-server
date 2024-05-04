-- Add up migration script here
CREATE TABLE shadow_bans (
    user_id UUID REFERENCES users(web_services_user_id) PRIMARY KEY NOT NULL,
    reason TEXT NOT NULL,
    created_ts TIMESTAMP WITHOUT TIME ZONE NOT NULL DEFAULT NOW()
);

DROP VIEW ranked_lb_view;

CREATE VIEW ranked_lb_view AS (
    SELECT l.user_id, dn.display_name, rank() OVER (ORDER BY l.height DESC) AS rank, l.height, l.ts
    FROM leaderboard AS l
    LEFT JOIN display_names AS dn ON l.user_id = dn.user_id
    LEFT JOIN shadow_bans AS sb ON l.user_id = sb.user_id
    WHERE sb.user_id IS NULL
);

DROP VIEW ranked_stats;

CREATE VIEW ranked_stats AS (
    SELECT s.*,
        CASE
            WHEN sb.user_id IS NOT NULL THEN 99999
            ELSE rank() OVER (ORDER BY pb_height DESC)
        END AS rank
    FROM stats as s
    LEFT JOIN shadow_bans AS sb ON s.user_id = sb.user_id
);
