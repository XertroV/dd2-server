-- Add up migration script here
CREATE INDEX ses_ts_idx ON sessions(created_ts);
CREATE INDEX ctx_raw_flags_ix ON contexts(flags_raw);

CREATE VIEW recent_sessions AS (
    SELECT u.display_name, s.*, ROW_NUMBER() OVER (PARTITION BY s.user_id ORDER BY s.created_ts DESC) as rn
    FROM sessions as s
    INNER JOIN users as u ON s.user_id = u.web_services_user_id
    ORDER BY s.created_ts DESC
);

CREATE VIEW most_recent_session AS (
    select * from recent_sessions where rn = 1
);

-- context updating from 2024-05-07 03:48:16.941229
CREATE VIEW adm__contexts_in_editor AS (
    SELECT u.display_name, u.web_services_user_id, c.*
    FROM contexts AS c
    INNER JOIN sessions as s ON c.session_token = s.session_token
    INNER JOIN users AS u ON s.user_id = u.web_services_user_id
    INNER JOIN maps AS m ON c.map_id = m.map_id
    WHERE c.created_ts > '2024-05-07 03:48:16.941229'
        AND c.flags_raw > 0
        AND (m.uid = 'DeepDip2__The_Storm_Is_Here'
            OR c.has_vl_item
            OR ABS(c.block_count + c.item_count - 58891) < 1000)
            -- array starts at 1; normally we'd use 5,7,9,11
        AND (c.flags[6] OR c.flags[8] OR c.flags[10] OR c.flags[12])
    ORDER BY c.created_ts DESC
);


CREATE VIEW adm__users_with_same_ip AS (
    SELECT s.ip_address, STRING_AGG(DISTINCT s.user_id::text, ', ') AS users, STRING_AGG(DISTINCT u.display_name, ', ') AS names
    FROM sessions s
    INNER JOIN users u ON s.user_id = u.web_services_user_id
    GROUP BY s.ip_address
    HAVING COUNT(DISTINCT s.user_id) > 1
);

CREATE TABLE server_connected_stats (
    id SERIAL PRIMARY KEY,
    ts TIMESTAMP WITHOUT TIME ZONE NOT NULL DEFAULT NOW(),
    nb_players INTEGER NOT NULL
);
CREATE INDEX server_connected_stats_ts_idx ON server_connected_stats(ts);

CREATE TABLE active_players_stats (
    id SERIAL PRIMARY KEY,
    ts TIMESTAMP WITHOUT TIME ZONE NOT NULL DEFAULT NOW(),
    nb_players INTEGER NOT NULL
);
CREATE INDEX active_players_stats_ts_idx ON active_players_stats(ts);
