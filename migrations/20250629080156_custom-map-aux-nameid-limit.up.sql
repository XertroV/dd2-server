-- Add up migration script here
-- Drop the existing index
DROP INDEX IF EXISTS idx_custom_map_aux_specs_user_id_name_id;

-- Drop the existing unique constraint
ALTER TABLE custom_map_aux_specs DROP CONSTRAINT IF EXISTS custom_map_aux_specs_user_id_name_id_key;

-- Alter the column type
ALTER TABLE custom_map_aux_specs
    ALTER COLUMN name_id TYPE VARCHAR(32);

-- Re-add the unique constraint (which automatically creates a unique index)
ALTER TABLE custom_map_aux_specs
    ADD CONSTRAINT custom_map_aux_specs_user_id_name_id_key UNIQUE (user_id, name_id);
