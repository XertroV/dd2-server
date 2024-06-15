-- Add up migration script here
ALTER TABLE game_cam_nods DROP CONSTRAINT game_cam_nods_session_token_fkey;
ALTER TABLE game_cam_nods ADD CONSTRAINT game_cam_nods_session_token_fkey FOREIGN KEY (session_token) REFERENCES sessions (session_token) ON DELETE CASCADE;
