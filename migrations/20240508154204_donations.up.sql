-- Add up migration script here
CREATE TABLE donations (
    id INTEGER PRIMARY KEY,
    auth_provider VARCHAR(32) NOT NULL,
    user_name VARCHAR(128) NOT NULL,
    display_name VARCHAR(128) NOT NULL,
    comment TEXT NOT NULL,
    amount DECIMAL(10, 2) NOT NULL,
    avatar TEXT,
    donated_at TIMESTAMP NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT NOW()
);
CREATE INDEX donations_display_name_idx ON donations(display_name);
CREATE INDEX donations_user_name_idx ON donations(user_name);
CREATE INDEX donations_amount_idx ON donations(amount);
CREATE INDEX donations_donated_at_idx ON donations(donated_at);

CREATE TABLE user_dono_totals (
    id SERIAL PRIMARY KEY,
    user_name VARCHAR(128) NOT NULL,
    total DECIMAL(10, 2) NOT NULL
);
CREATE UNIQUE INDEX dono_totals_user_name_unique_idx ON user_dono_totals(user_name);
CREATE INDEX dono_totals_total_idx ON user_dono_totals(total);
