-- Stored aux-spec hash + cheaper first-PB listing check.
--
-- 1. `custom_map_aux_specs.spec_hash` replaces the `md5(spec::text)` that every
--    hat read and every gameplay report recomputed. The aux spec JSONB is
--    routinely tens to hundreds of KB and `resolve_bound_spec` /
--    `lock_materialized_spec` / `get_hats_mine` / `get_hats_on_map` /
--    `equipped_push_entry` all hashed it on the hot path. A STORED generated
--    column keeps the value in lockstep with `spec` without touching any
--    write path, so `map_listing_bind.materialized_spec_hash` stays comparable
--    to exactly the same string it stored before.
--
-- 2. `map_leaderboard_maybe_list` now checks `listed` before taking the
--    per-map advisory lock. The lock is transaction scoped, so the previous
--    ordering serialized *every* new `map_leaderboard` row on every map
--    forever, including maps listed months ago. The post-lock check is kept as
--    the race double-check, so behaviour is unchanged.
--
-- Hazard worth knowing (no schema change, documented here because this is the
-- migration that made hat unlocks durable): hat slugs default positionally to
-- `Hat-N` when a spec entry omits `slug`, and `hat_key = MD5(mapUid || ':h:' ||
-- slug)`. Inserting a hat mid-list therefore renumbers every later hat and
-- orphans the `hat_unlock` rows keyed off the old slug. Mappers should give
-- every hat an explicit stable `slug`; the server warn-logs on upload when one
-- is missing.

ALTER TABLE custom_map_aux_specs
    ADD COLUMN spec_hash TEXT NOT NULL GENERATED ALWAYS AS (md5(spec::text)) STORED;

CREATE OR REPLACE FUNCTION map_leaderboard_maybe_list()
RETURNS TRIGGER AS $$
DECLARE
    thresh INTEGER;
    public_count INTEGER;
BEGIN
    IF NEW.map_uid = 'DeepDip2__The_Storm_Is_Here' THEN
        RETURN NEW;
    END IF;

    -- Cheap unlocked pre-check: an already listed map never needs the lock.
    IF EXISTS (SELECT 1 FROM listed_maps WHERE map_uid = NEW.map_uid AND listed) THEN
        RETURN NEW;
    END IF;

    -- Serialize threshold checks per map. Under READ COMMITTED, the COUNT
    -- statement after this lock sees any competing insert that committed while
    -- we waited.
    PERFORM pg_advisory_xact_lock(hashtextextended(NEW.map_uid, 0));

    -- Race double-check: another contender may have listed the map while we
    -- were waiting for the lock.
    IF EXISTS (SELECT 1 FROM listed_maps WHERE map_uid = NEW.map_uid AND listed) THEN
        RETURN NEW;
    END IF;

    SELECT s.listing_threshold
    INTO thresh
    FROM map_listing_bind b
    LEFT JOIN custom_map_aux_specs s ON s.id = b.spec_id
    WHERE b.map_uid = NEW.map_uid
      AND b.author_verified_at IS NOT NULL
      AND b.verified_author_id = s.user_id;

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
