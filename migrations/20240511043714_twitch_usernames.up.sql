-- Add up migration script here
CREATE TABLE twitch_usernames (
    user_id Uuid PRIMARY KEY REFERENCES users(web_services_user_id),
    twitch_name VARCHAR(64) NOT NULL
);
CREATE INDEX twitch_usernames_name_idx ON twitch_usernames(twitch_name);

ALTER TABLE stats ADD COLUMN extra JSON NOT NULL DEFAULT '{}'::json;
ALTER TABLE stats_archive ADD COLUMN extra JSON NOT NULL DEFAULT '{}'::json;

DROP VIEW ranked_stats;
CREATE VIEW ranked_stats AS (
    SELECT *, rank() OVER (ORDER BY pb_height DESC) AS rank
    FROM stats
);
