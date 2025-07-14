-- Add down migration script here
ALTER TABLE map_curr_heights DROP COLUMN afk_update_count;

DROP TRIGGER IF EXISTS map_curr_heights_afk_update_trigger
ON map_curr_heights;
