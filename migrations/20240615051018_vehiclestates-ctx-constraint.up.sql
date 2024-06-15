-- Add up migration script here
ALTER TABLE vehicle_states DROP CONSTRAINT vehicle_states_context_id_fkey;
ALTER TABLE vehicle_states ADD CONSTRAINT vehicle_states_context_id_fkey FOREIGN KEY (context_id) REFERENCES contexts (context_id) ON DELETE CASCADE;
