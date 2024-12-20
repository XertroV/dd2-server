-- Add up migration script here
CREATE INDEX vehicle_states_ts_idx ON vehicle_states(ts);
CREATE INDEX contexts_ts_idx ON contexts(created_ts);
