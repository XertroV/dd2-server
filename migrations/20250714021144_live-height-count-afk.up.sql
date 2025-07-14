-- definition of table:
-- CREATE TABLE map_curr_heights (
--     id SERIAL PRIMARY KEY,
--     map_uid VARCHAR(27) NOT NULL,
--     user_id UUID NOT NULL REFERENCES users(web_services_user_id) ON DELETE CASCADE,
--     height DOUBLE PRECISION NOT NULL,
--     pos DOUBLE PRECISION[3] NOT NULL,
--     race_time INTEGER NOT NULL,
--     updated_at TIMESTAMP WITHOUT TIME ZONE NOT NULL DEFAULT CURRENT_TIMESTAMP,
--     update_count INTEGER NOT NULL DEFAULT 0
-- );
-- CREATE UNIQUE INDEX map_curr_heights_user_map_idx ON map_curr_heights(user_id, map_uid);
-- CREATE INDEX map_curr_heights_users_heights_idx ON map_curr_heights(user_id, updated_at DESC, height DESC);
-- CREATE INDEX map_curr_heights_map_height_idx ON map_curr_heights(map_uid, updated_at DESC, height DESC);
-- Add up migration script here
ALTER TABLE map_curr_heights
ADD COLUMN afk_update_count INTEGER NOT NULL DEFAULT 0;

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
