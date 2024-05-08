-- Add down migration script here
DROP INDEX active_players_stats_ts_idx;
DROP TABLE active_players_stats;
DROP INDEX server_connected_stats_ts_idx;
DROP TABLE server_connected_stats;
DROP VIEW adm__users_with_same_ip;
DROP VIEW adm__contexts_in_editor;
DROP VIEW most_recent_session;
DROP VIEW recent_sessions;
DROP INDEX ses_ts_idx;
DROP INDEX ctx_raw_flags_ix;
