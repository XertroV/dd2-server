-- Add up migration script here
-- game_cam_nods -> session_token
CREATE INDEX game_cam_nods_session_token_idx ON game_cam_nods (session_token);
-- contexts -> session_token
CREATE INDEX contexts_session_token_idx ON contexts (session_token);
