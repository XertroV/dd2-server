-- Add up migration script here
ALTER TABLE falls DROP CONSTRAINT falls_session_token_fkey;
ALTER TABLE falls ADD CONSTRAINT falls_session_token_fkey FOREIGN KEY (session_token) REFERENCES sessions (session_token) ON DELETE CASCADE;
