-- Add up migration script here
CREATE VIEW context_to_user_and_session AS (
    SELECT u.web_services_user_id as user_id, c.*
    FROM contexts AS c
    LEFT JOIN sessions AS s ON c.session_token = s.session_token
    LEFT JOIN users AS u ON s.user_id = u.web_services_user_id
);
