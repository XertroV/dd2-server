-- Add down migration script here
DROP VIEW ranked_stats;

ALTER TABLE stats_archive DROP COLUMN extra;
ALTER TABLE stats DROP COLUMN extra;
DROP TABLE twitch_usernames;

CREATE VIEW ranked_stats AS (
    SELECT *, rank() OVER (ORDER BY pb_height DESC) AS rank
    FROM stats
);
