-- these are public
CREATE TABLE profile_settings (
    id SERIAL PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(web_services_user_id) ON DELETE CASCADE,
    s_value json NOT NULL,
    created_at TIMESTAMP WITHOUT TIME ZONE NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMP WITHOUT TIME ZONE NOT NULL DEFAULT NOW()
);
CREATE UNIQUE INDEX profile_settings_user_idx ON profile_settings(user_id);

-- these are private
CREATE TABLE user_preferences (
    id SERIAL PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(web_services_user_id) ON DELETE CASCADE,
    s_value json NOT NULL,
    created_at TIMESTAMP WITHOUT TIME ZONE NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMP WITHOUT TIME ZONE NOT NULL DEFAULT NOW()
);
CREATE UNIQUE INDEX user_preferences_user_idx ON user_preferences(user_id);

CREATE TABLE map_events (
    id SERIAL PRIMARY KEY,
    map_uid VARCHAR(27) NOT NULL,
    event_type VARCHAR(64) NOT NULL,
    happened_at TIMESTAMP WITHOUT TIME ZONE,
    has_happened BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMP WITHOUT TIME ZONE NOT NULL DEFAULT NOW()
);
INSERT INTO map_events (map_uid, event_type) VALUES ('DeepDip2__The_Storm_Is_Here', '1finish');
INSERT INTO map_events (map_uid, event_type) VALUES ('DeepDip2__The_Storm_Is_Here', '2finish');
INSERT INTO map_events (map_uid, event_type) VALUES ('DeepDip2__The_Storm_Is_Here', '3finish');
