-- Add up migration script here
CREATE INDEX vs_is_official_ts_idx ON vehicle_states(is_official, ts, session_token);

CREATE VIEW vs_official_most_recent_height_a AS (
    SELECT
    u.display_name,
    s.user_id,
    vs.session_token,
    vs.context_id,
    vs.pos[1] AS height,
    vs.is_official,
    vs.ts
    FROM vehicle_states AS vs
    LEFT JOIN sessions AS s ON vs.session_token = s.session_token
    JOIN (
        SELECT
            s.user_id,
            MAX(vs.ts) AS max_ts
        FROM vehicle_states AS vs
        JOIN sessions AS s ON vs.session_token = s.session_token
        WHERE vs.is_official = true
        GROUP BY s.user_id
    ) AS latest ON vs.ts = latest.max_ts AND s.user_id = latest.user_id
    LEFT JOIN users AS u ON s.user_id = u.web_services_user_id
    ORDER BY vs.ts DESC
);

CREATE VIEW vs_official_most_recent_height_b AS (
    SELECT
    display_name,
    user_id,
    session_token,
    context_id,
    height,
    is_official,
    ts
    FROM (
        SELECT
            u.display_name,
            s.user_id,
            vs.session_token,
            vs.context_id,
            vs.pos[1] AS height,
            vs.is_official,
            vs.ts,
            ROW_NUMBER() OVER (PARTITION BY s.user_id ORDER BY vs.ts DESC) as rn
        FROM vehicle_states AS vs
        LEFT JOIN sessions AS s ON vs.session_token = s.session_token
        LEFT JOIN users AS u ON s.user_id = u.web_services_user_id
        WHERE vs.is_official = true
    ) as subquery
    WHERE rn = 1 ORDER BY ts DESC
)
