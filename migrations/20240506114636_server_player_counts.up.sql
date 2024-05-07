-- Add up migration script here
CREATE TABLE server_player_counts (
    server_id VARCHAR(32) NOT NULL PRIMARY KEY,
    player_count INT NOT NULL,
    updated_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP
);
