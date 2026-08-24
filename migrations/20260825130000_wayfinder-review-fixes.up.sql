-- Canonical minigame/collectible contracts and race-free map listing.

-- Upgrade the original best-effort listing hint into the single immutable
-- map -> spec authority used by every Wayfinder report. Existing rows remain
-- deliberately unverified until the owning mapper proves them through Nadeo.
DELETE FROM map_listing_bind b
WHERE b.spec_id IS NULL
   OR NOT EXISTS (SELECT 1 FROM custom_map_aux_specs s WHERE s.id = b.spec_id);
ALTER TABLE map_listing_bind
    DROP CONSTRAINT map_listing_bind_spec_id_fkey,
    ALTER COLUMN spec_id SET NOT NULL,
    ADD COLUMN verified_author_id UUID,
    ADD COLUMN author_verified_at TIMESTAMPTZ,
    ADD CONSTRAINT map_listing_bind_spec_id_fkey
        FOREIGN KEY (spec_id) REFERENCES custom_map_aux_specs(id) ON DELETE RESTRICT,
    ADD CONSTRAINT map_listing_bind_verification_pair_check
        CHECK ((verified_author_id IS NULL) = (author_verified_at IS NULL));

UPDATE minigame_catalog
SET kind = CASE kind
    WHEN 'TimeTrial' THEN 'Sprint'
    WHEN 'KingOfTheHill' THEN 'Survival'
    ELSE kind
END;
UPDATE minigame_catalog SET lower_is_better = (kind = 'Sprint');
UPDATE minigame_on_map
SET kind = CASE kind
    WHEN 'TimeTrial' THEN 'Sprint'
    WHEN 'KingOfTheHill' THEN 'Survival'
    ELSE kind
END;

ALTER TABLE minigame_catalog
    ADD CONSTRAINT minigame_catalog_canonical_kind_check
    CHECK (kind IN ('Sprint', 'Survival', 'Darts', 'HighJump', 'LongJump', 'MaxAvgSpeed')),
    ADD CONSTRAINT minigame_catalog_sort_direction_check
    CHECK (lower_is_better = (kind = 'Sprint'));
ALTER TABLE minigame_on_map
    ADD CONSTRAINT minigame_on_map_canonical_kind_check
    CHECK (kind IN ('Sprint', 'Survival', 'Darts', 'HighJump', 'LongJump', 'MaxAvgSpeed'));

ALTER TABLE collectible_catalog
    ADD COLUMN collect_on_unlock BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN has_zone BOOLEAN NOT NULL DEFAULT FALSE;
UPDATE collectible_catalog
SET collect_on_unlock = (pickup = 'grant'),
    has_zone = (pickup = 'zone');
ALTER TABLE collectible_catalog
    ADD CONSTRAINT collectible_catalog_collect_path_check
    CHECK (collect_on_unlock OR has_zone);

ALTER TABLE collectible_on_map
    ADD COLUMN collect_on_unlock BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN has_zone BOOLEAN NOT NULL DEFAULT FALSE;
UPDATE collectible_on_map
SET collect_on_unlock = (pickup = 'grant'),
    has_zone = (pickup = 'zone');
ALTER TABLE collectible_on_map
    ADD CONSTRAINT collectible_on_map_collect_path_check
    CHECK (collect_on_unlock OR has_zone);

-- Old nullable grant-via-82 audit rows remain readable, but every new audit row
-- must carry the position from report 83.
ALTER TABLE collectible_collect_log
    ADD CONSTRAINT collectible_collect_log_position_required
    CHECK (pos_x IS NOT NULL AND pos_y IS NOT NULL AND pos_z IS NOT NULL) NOT VALID;

-- Serialize threshold checks per map. Under READ COMMITTED, the COUNT statement
-- after this lock sees any competing insert that committed while we waited.
CREATE OR REPLACE FUNCTION map_leaderboard_maybe_list()
RETURNS TRIGGER AS $$
DECLARE
    thresh INTEGER;
    public_count INTEGER;
BEGIN
    IF NEW.map_uid = 'DeepDip2__The_Storm_Is_Here' THEN
        RETURN NEW;
    END IF;

    PERFORM pg_advisory_xact_lock(hashtextextended(NEW.map_uid, 0));

    IF EXISTS (SELECT 1 FROM listed_maps WHERE map_uid = NEW.map_uid AND listed) THEN
        RETURN NEW;
    END IF;

    SELECT s.listing_threshold
    INTO thresh
    FROM map_listing_bind b
    LEFT JOIN custom_map_aux_specs s ON s.id = b.spec_id
    WHERE b.map_uid = NEW.map_uid
      AND b.author_verified_at IS NOT NULL;

    thresh := LEAST(100, GREATEST(5, COALESCE(thresh, 25)));

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
