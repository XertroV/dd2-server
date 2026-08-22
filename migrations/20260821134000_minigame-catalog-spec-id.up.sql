-- Bind catalog rows to custom_map_aux_specs.id (injected as specId on GET).
ALTER TABLE minigame_catalog
    ADD COLUMN spec_id BIGINT REFERENCES custom_map_aux_specs(id) ON DELETE CASCADE;

UPDATE minigame_catalog c
SET spec_id = s.id
FROM custom_map_aux_specs s
WHERE s.user_id = c.user_id AND s.name_id = c.name_id;

CREATE INDEX minigame_catalog_spec_id_idx ON minigame_catalog (spec_id);
CREATE INDEX minigame_catalog_spec_id_slug_idx ON minigame_catalog (spec_id, slug);
