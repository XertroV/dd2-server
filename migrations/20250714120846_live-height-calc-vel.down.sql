-- Add down migration script here
DROP TRIGGER IF EXISTS map_curr_heights_update_trigger ON map_curr_heights;
DROP FUNCTION IF EXISTS update_afk_and_velocity();

CREATE OR REPLACE FUNCTION increment_afk_update_count()
RETURNS TRIGGER AS $$
BEGIN
    IF
        sqrt(
            pow(NEW.pos[1] - OLD.pos[1], 2) +
            pow(NEW.pos[2] - OLD.pos[2], 2) +
            pow(NEW.pos[3] - OLD.pos[3], 2)
        ) <= 1.0
    THEN
        NEW.afk_update_count := OLD.afk_update_count + 1;
    ELSE
        NEW.afk_update_count := 0;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE TRIGGER map_curr_heights_afk_update_trigger
BEFORE UPDATE ON map_curr_heights
FOR EACH ROW
EXECUTE FUNCTION increment_afk_update_count();

ALTER TABLE map_curr_heights DROP COLUMN IF EXISTS velocity;
