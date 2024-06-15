-- Add up migration script here
ALTER TABLE shadow_bans DROP CONSTRAINT shadow_bans_user_id_fkey;
ALTER TABLE shadow_bans ADD CONSTRAINT shadow_bans_user_id_fkey FOREIGN KEY (user_id) REFERENCES users (web_services_user_id) ON DELETE CASCADE;

ALTER TABLE twitch_usernames DROP CONSTRAINT twitch_usernames_user_id_fkey;
ALTER TABLE twitch_usernames ADD CONSTRAINT twitch_usernames_user_id_fkey FOREIGN KEY (user_id) REFERENCES users (web_services_user_id) ON DELETE CASCADE;
