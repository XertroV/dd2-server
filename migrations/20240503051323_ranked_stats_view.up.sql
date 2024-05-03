-- Add up migration script here
CREATE VIEW ranked_stats AS (
    SELECT *, rank() OVER (ORDER BY pb_height DESC) AS rank
    FROM stats
);
