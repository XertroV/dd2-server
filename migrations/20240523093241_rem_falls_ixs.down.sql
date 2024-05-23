-- Add down migration script here

CREATE INDEX falls_start_pos_y_ts_idx ON falls(start_pos_y, ts);
CREATE INDEX falls_end_pos_y_ts_idx ON falls(end_pos_y, ts);
