-- Add down migration script here
DROP INDEX IF EXISTS idx_custom_map_stats_map_uid;
DROP TABLE IF EXISTS custom_map_stats CASCADE;
