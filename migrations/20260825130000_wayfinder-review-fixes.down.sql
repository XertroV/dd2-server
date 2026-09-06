ALTER TABLE map_listing_bind
    DROP CONSTRAINT IF EXISTS map_listing_bind_materialization_pair_check,
    DROP CONSTRAINT IF EXISTS map_listing_bind_verification_pair_check,
    DROP CONSTRAINT IF EXISTS map_listing_bind_spec_id_fkey,
    DROP COLUMN IF EXISTS materialized_spec_updated_at,
    DROP COLUMN IF EXISTS materialized_spec_hash,
    DROP COLUMN IF EXISTS verified_author_id,
    DROP COLUMN IF EXISTS author_verified_at,
    ALTER COLUMN spec_id DROP NOT NULL,
    ADD CONSTRAINT map_listing_bind_spec_id_fkey
        FOREIGN KEY (spec_id) REFERENCES custom_map_aux_specs(id) ON DELETE SET NULL;

CREATE OR REPLACE FUNCTION map_leaderboard_maybe_list()
RETURNS TRIGGER AS $$
DECLARE
    thresh INTEGER;
    public_count INTEGER;
BEGIN
    IF NEW.map_uid = 'DeepDip2__The_Storm_Is_Here' THEN
        RETURN NEW;
    END IF;
    IF EXISTS (SELECT 1 FROM listed_maps WHERE map_uid = NEW.map_uid AND listed) THEN
        RETURN NEW;
    END IF;

    SELECT s.listing_threshold
    INTO thresh
    FROM map_listing_bind b
    LEFT JOIN custom_map_aux_specs s ON s.id = b.spec_id
    WHERE b.map_uid = NEW.map_uid;
    thresh := COALESCE(thresh, 25);

    SELECT COUNT(*)::int
    INTO public_count
    FROM map_leaderboard m
    LEFT JOIN shadow_bans sb ON m.user_id = sb.user_id
    LEFT JOIN map_bans mb ON m.user_id = mb.user_id AND mb.map_uid = m.map_uid
        AND mb.lifted_at IS NULL
        AND (mb.expires_at IS NULL OR mb.expires_at > NOW())
    WHERE sb.user_id IS NULL AND mb.user_id IS NULL AND m.map_uid = NEW.map_uid;

    IF public_count >= thresh THEN
        INSERT INTO listed_maps (map_uid, listed, became_listed)
        VALUES (NEW.map_uid, TRUE, NOW())
        ON CONFLICT (map_uid) DO NOTHING;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

ALTER TABLE collectible_collect_log
    DROP CONSTRAINT IF EXISTS collectible_collect_log_position_required;
ALTER TABLE collectible_on_map
    DROP CONSTRAINT IF EXISTS collectible_on_map_collect_path_check,
    DROP COLUMN IF EXISTS collect_on_unlock,
    DROP COLUMN IF EXISTS has_zone;
ALTER TABLE collectible_catalog
    DROP CONSTRAINT IF EXISTS collectible_catalog_collect_path_check,
    DROP COLUMN IF EXISTS collect_on_unlock,
    DROP COLUMN IF EXISTS has_zone;
ALTER TABLE minigame_on_map
    DROP CONSTRAINT IF EXISTS minigame_on_map_canonical_kind_check;
ALTER TABLE minigame_attempt
    DROP CONSTRAINT IF EXISTS minigame_attempt_finish_score_nonnegative_check;
ALTER TABLE minigame_pb
    DROP CONSTRAINT IF EXISTS minigame_pb_score_nonnegative_check;
ALTER TABLE minigame_catalog
    DROP CONSTRAINT IF EXISTS minigame_catalog_sort_direction_check,
    DROP CONSTRAINT IF EXISTS minigame_catalog_canonical_kind_check;

-- Restore the legacy kind names and sort rule expected by the prior server
-- binary instead of leaving canonical values that it cannot interpret.
UPDATE minigame_catalog
SET kind = CASE kind
        WHEN 'Sprint' THEN 'TimeTrial'
        WHEN 'Survival' THEN 'KingOfTheHill'
        ELSE kind
    END,
    lower_is_better = (kind = 'Sprint');
UPDATE minigame_on_map
SET kind = CASE kind
    WHEN 'Sprint' THEN 'TimeTrial'
    WHEN 'Survival' THEN 'KingOfTheHill'
    ELSE kind
END;
