-- Add up migration script here
CREATE TABLE colors (
    user_id UUID PRIMARY KEY REFERENCES users(web_services_user_id),
    color DOUBLE PRECISION ARRAY[3] NOT NULL
);
CREATE INDEX colors_user_id_idx ON colors(user_id);
