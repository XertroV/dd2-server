--sql
CREATE TABLE custom_map_aux_specs (
    id BIGSERIAL PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(web_services_user_id) ON DELETE CASCADE,
    name_id TEXT NOT NULL,
    spec JSONB NOT NULL,
    hit_counter BIGINT NOT NULL DEFAULT 0, -- how many times this aux spec has been accessed.
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(user_id, name_id)
);
CREATE INDEX idx_custom_map_aux_specs_user_id_name_id ON custom_map_aux_specs (user_id, name_id);
