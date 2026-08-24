DROP TRIGGER IF EXISTS map_leaderboard_maybe_list ON map_leaderboard;
DROP FUNCTION IF EXISTS map_leaderboard_maybe_list();
DROP TABLE IF EXISTS listed_maps;
DROP TABLE IF EXISTS map_listing_bind;
ALTER TABLE custom_map_aux_specs
    DROP CONSTRAINT IF EXISTS custom_map_aux_specs_listing_threshold_check;
ALTER TABLE custom_map_aux_specs
    DROP COLUMN IF EXISTS listing_threshold;
