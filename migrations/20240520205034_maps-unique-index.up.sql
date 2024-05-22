-- Add up migration script here
CREATE UNIQUE INDEX maps_uniq_uid_hash_idx ON maps(uid, hash);
