-- Add down migration script here
DROP INDEX dono_totals_user_name_unique_idx;
DROP INDEX dono_totals_total_idx;
DROP TABLE user_dono_totals;

DROP INDEX donations_display_name_idx;
DROP INDEX donations_user_name_idx;
DROP INDEX donations_amount_idx;
DROP INDEX donations_donated_at_idx;
DROP TABLE donations;
