-- Add up migration script here
ALTER TABLE respawns DROP CONSTRAINT respawns_session_token_fkey;
ALTER TABLE respawns ADD CONSTRAINT respawns_session_token_fkey FOREIGN KEY (session_token) REFERENCES sessions (session_token) ON DELETE CASCADE;

ALTER TABLE game_cam_nods DROP CONSTRAINT game_cam_nods_context_id_fkey;
ALTER TABLE game_cam_nods ADD CONSTRAINT game_cam_nods_context_id_fkey FOREIGN KEY (context_id) REFERENCES contexts (context_id) ON DELETE CASCADE;
