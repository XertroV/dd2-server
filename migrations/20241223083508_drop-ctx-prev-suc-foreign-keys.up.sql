-- Add up migration script here
-- constraints to drop contexts_successor_fkey, contexts_predecessor_fkey
ALTER TABLE contexts DROP CONSTRAINT IF EXISTS contexts_successor_fkey;
ALTER TABLE contexts DROP CONSTRAINT IF EXISTS contexts_predecessor_fkey;
ALTER TABLE vehicle_states DROP CONSTRAINT IF EXISTS vehicle_states_context_id_fkey;
ALTER TABLE game_cam_nods DROP CONSTRAINT IF EXISTS game_cam_nods_context_id_fkey;
ALTER TABLE position_reports DROP CONSTRAINT IF EXISTS position_reports_context_id_fkey;
