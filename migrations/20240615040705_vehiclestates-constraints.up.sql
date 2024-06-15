-- Add up migration script here
ALTER TABLE vehicle_states DROP CONSTRAINT vehicle_states_session_token_fkey;
ALTER TABLE vehicle_states ADD CONSTRAINT vehicle_states_session_token_fkey FOREIGN KEY (session_token) REFERENCES sessions (session_token) ON DELETE CASCADE;
