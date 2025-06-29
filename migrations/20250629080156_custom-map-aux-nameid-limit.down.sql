-- Drop the new unique constraint
ALTER TABLE custom_map_aux_specs DROP CONSTRAINT IF EXISTS custom_map_aux_specs_user_id_name_id_key;

-- Revert the column type
ALTER TABLE custom_map_aux_specs
    ALTER COLUMN name_id TYPE TEXT;

-- Re-add the previous unique index (if it existed)
CREATE UNIQUE INDEX idx_custom_map_aux_specs_user_id_name_id ON custom_map_aux_specs (user_id, name_id);
