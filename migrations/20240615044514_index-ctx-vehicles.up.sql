-- Add up migration script here
CREATE INDEX ctx_vehicles_context_id_idx ON vehicle_states (context_id);
