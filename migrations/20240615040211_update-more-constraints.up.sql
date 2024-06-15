-- Add up migration script here
ALTER TABLE sessions DROP CONSTRAINT sessions_user_id_fkey;
ALTER TABLE sessions ADD CONSTRAINT sessions_user_id_fkey FOREIGN KEY (user_id) REFERENCES users (web_services_user_id) ON DELETE CASCADE;

ALTER TABLE contexts DROP CONSTRAINT contexts_session_token_fkey;
ALTER TABLE contexts ADD CONSTRAINT contexts_session_token_fkey FOREIGN KEY (session_token) REFERENCES sessions (session_token) ON DELETE CASCADE;
