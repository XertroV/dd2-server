DROP INDEX IF EXISTS minigame_catalog_spec_id_slug_idx;
DROP INDEX IF EXISTS minigame_catalog_spec_id_idx;
ALTER TABLE minigame_catalog DROP COLUMN IF EXISTS spec_id;
