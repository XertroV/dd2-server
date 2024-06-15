-- Add down migration script here
ALTER TABLE display_names DROP CONSTRAINT display_names_user_id_fkey;
ALTER TABLE display_names ADD CONSTRAINT display_names_user_id_fkey FOREIGN KEY (user_id) REFERENCES users(web_services_user_id);

ALTER TABLE falls DROP CONSTRAINT falls_user_id_fkey;
ALTER TABLE falls ADD CONSTRAINT falls_user_id_fkey FOREIGN KEY (user_id) REFERENCES users(web_services_user_id);

ALTER TABLE leaderboard_archive DROP CONSTRAINT leaderboard_archive_user_id_fkey;
ALTER TABLE leaderboard_archive ADD CONSTRAINT leaderboard_archive_user_id_fkey FOREIGN KEY (user_id) REFERENCES users(web_services_user_id) ON DELETE CASCADE;

ALTER TABLE leaderboard DROP CONSTRAINT leaderboard_user_id_fkey;
ALTER TABLE leaderboard ADD CONSTRAINT leaderboard_user_id_fkey FOREIGN KEY (user_id) REFERENCES users(web_services_user_id) ON DELETE CASCADE;

ALTER TABLE stats DROP CONSTRAINT stats_user_id_fkey;
ALTER TABLE stats ADD CONSTRAINT stats_user_id_fkey FOREIGN KEY (user_id) REFERENCES users(web_services_user_id) ON DELETE CASCADE;
