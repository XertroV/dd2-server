-- Listed maps after player threshold.
-- listingThreshold lives on the aux spec row. First spec-on-map bind wins.
-- listed + became_listed is the durable map row. Flip on new unique LB INSERT.

ALTER TABLE custom_map_aux_specs
    ADD COLUMN listing_threshold INTEGER NOT NULL DEFAULT 25;
ALTER TABLE custom_map_aux_specs
    ADD CONSTRAINT custom_map_aux_specs_listing_threshold_check
    CHECK (listing_threshold >= 5 AND listing_threshold <= 100);

UPDATE custom_map_aux_specs
SET listing_threshold = CASE
    WHEN jsonb_typeof(spec -> 'listingThreshold') IS DISTINCT FROM 'number' THEN 25
    WHEN (spec ->> 'listingThreshold')::numeric < 5 THEN 5
    WHEN (spec ->> 'listingThreshold')::numeric > 100 THEN 100
    ELSE TRUNC((spec ->> 'listingThreshold')::numeric)::int
END;

-- First ReportMinigameMap bind for a map. Later specs do not replace it.
CREATE TABLE map_listing_bind (
    map_uid VARCHAR(27) PRIMARY KEY NOT NULL,
    spec_id BIGINT REFERENCES custom_map_aux_specs(id) ON DELETE SET NULL
);

-- Presence of listed=true is permanent. Raising the threshold does not unlist.
CREATE TABLE listed_maps (
    map_uid VARCHAR(27) PRIMARY KEY NOT NULL,
    listed BOOLEAN NOT NULL DEFAULT TRUE,
    became_listed TIMESTAMPTZ NOT NULL
);
CREATE INDEX listed_maps_became_listed_uid_idx
    ON listed_maps (became_listed DESC, map_uid ASC)
    WHERE listed;

-- Same public-count filters as get_map_leaderboard_len / Map Overview.
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

CREATE TRIGGER map_leaderboard_maybe_list
AFTER INSERT ON map_leaderboard
FOR EACH ROW
EXECUTE FUNCTION map_leaderboard_maybe_list();

-- One-shot backfill: already-over-threshold custom maps. became_listed = now.
-- No binds exist yet at ship, so every map uses the default 25.
INSERT INTO listed_maps (map_uid, listed, became_listed)
SELECT ml.map_uid, TRUE, NOW()
FROM (
    SELECT m.map_uid, COUNT(*)::int AS public_count
    FROM map_leaderboard m
    LEFT JOIN shadow_bans sb ON m.user_id = sb.user_id
    LEFT JOIN map_bans mb ON m.user_id = mb.user_id AND mb.map_uid = m.map_uid
        AND mb.lifted_at IS NULL
        AND (mb.expires_at IS NULL OR mb.expires_at > NOW())
    WHERE sb.user_id IS NULL
      AND mb.user_id IS NULL
      AND m.map_uid <> 'DeepDip2__The_Storm_Is_Here'
    GROUP BY m.map_uid
) ml
WHERE ml.public_count >= 25
ORDER BY ml.map_uid
ON CONFLICT (map_uid) DO NOTHING;
