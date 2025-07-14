-- Add velocity column
ALTER TABLE map_curr_heights
ADD COLUMN velocity DOUBLE PRECISION[3] NOT NULL DEFAULT ARRAY[0.0, 0.0, 0.0];

-- Replace afk_update_count trigger with new trigger that also updates velocity
DROP TRIGGER IF EXISTS map_curr_heights_afk_update_trigger ON map_curr_heights;
DROP FUNCTION IF EXISTS increment_afk_update_count();

CREATE OR REPLACE FUNCTION update_afk_and_velocity()
RETURNS TRIGGER AS $$
DECLARE
    dt DOUBLE PRECISION;
    vel DOUBLE PRECISION[3];
BEGIN
    -- Calculate delta t in seconds
    dt := EXTRACT(EPOCH FROM (NOW() - OLD.updated_at));
    IF dt <= 0 THEN
        vel := ARRAY[0.0, 0.0, 0.0];
    ELSE
        vel := ARRAY[
            (NEW.pos[1] - OLD.pos[1]) / dt,
            (NEW.pos[2] - OLD.pos[2]) / dt,
            (NEW.pos[3] - OLD.pos[3]) / dt
        ];
    END IF;
    NEW.velocity := vel;

    -- AFK logic (same as before, but with <= 1.0 threshold)
    IF sqrt(
        pow(NEW.pos[1] - OLD.pos[1], 2) +
        pow(NEW.pos[2] - OLD.pos[2], 2) +
        pow(NEW.pos[3] - OLD.pos[3], 2)
    ) <= 1.0 THEN
        NEW.afk_update_count := OLD.afk_update_count + 1;
    ELSE
        NEW.afk_update_count := 0;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE TRIGGER map_curr_heights_update_trigger
BEFORE UPDATE ON map_curr_heights
FOR EACH ROW
EXECUTE FUNCTION update_afk_and_velocity();
