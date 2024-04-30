use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use log::warn;
use sqlx::{prelude::FromRow, query, query_as, types::Uuid, Pool, Postgres};

use crate::router::Stats;

pub async fn update_users_stats(pool: &Pool<Postgres>, user_id: &Uuid, stats: &Stats) -> Result<(), sqlx::Error> {
    let r = query!("UPDATE stats SET nb_jumps = $1, nb_falls = $2, nb_floors_fallen = $3, last_pb_set_ts = $4, total_dist_fallen = $5, pb_height = $6, pb_floor = $7, nb_resets = $8, ggs_triggered = $9, title_gags_triggered = $10, title_gags_special_triggered = $11, bye_byes_triggered = $12, monument_triggers = $13, reached_floor_count = $14, floor_voice_lines_played = $15, update_count = update_count + 1, ts = NOW() WHERE user_id = $16 RETURNING update_count;",
        stats.nb_jumps as i32,
        stats.nb_falls as i32,
        stats.nb_floors_fallen as i32,
        DateTime::from_timestamp(stats.last_pb_set_ts as i64, 0).unwrap().naive_utc(),
        stats.total_dist_fallen as f64,
        stats.pb_height as f64,
        stats.pb_floor as i32,
        stats.nb_resets as i32,
        stats.ggs_triggered as i32,
        stats.title_gags_triggered as i32,
        stats.title_gags_special_triggered as i32,
        stats.bye_byes_triggered as i32,
        stats.monument_triggers,
        stats.reached_floor_count,
        stats.floor_voice_lines_played,
        user_id
    ).fetch_one(pool).await;
    match r {
        Ok(r) => {
            if (r.update_count + 28) % 30 == 0 {
                // insert into stats_archive
                query!("INSERT INTO stats_archive (user_id, nb_jumps, nb_falls, nb_floors_fallen, last_pb_set_ts, total_dist_fallen, pb_height, pb_floor, nb_resets, ggs_triggered, title_gags_triggered, title_gags_special_triggered, bye_byes_triggered, monument_triggers, reached_floor_count, floor_voice_lines_played, update_count)
                    SELECT user_id, nb_jumps, nb_falls, nb_floors_fallen, last_pb_set_ts, total_dist_fallen, pb_height, pb_floor, nb_resets, ggs_triggered, title_gags_triggered, title_gags_special_triggered, bye_byes_triggered, monument_triggers, reached_floor_count, floor_voice_lines_played, update_count
                    FROM stats WHERE user_id = $1;", user_id).execute(pool).await?;
            }
            Ok(())
        }
        Err(sqlx::Error::RowNotFound) => {
            query!("INSERT INTO stats (user_id, nb_jumps, nb_falls, nb_floors_fallen, last_pb_set_ts, total_dist_fallen, pb_height, pb_floor, nb_resets, ggs_triggered, title_gags_triggered, title_gags_special_triggered, bye_byes_triggered, monument_triggers, reached_floor_count, floor_voice_lines_played) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16);",
                user_id,
                stats.nb_jumps as i32,
                stats.nb_falls as i32,
                stats.nb_floors_fallen as i32,
                DateTime::from_timestamp(stats.last_pb_set_ts as i64, 0).unwrap().naive_utc(),
                stats.total_dist_fallen as f64,
                stats.pb_height as f64,
                stats.pb_floor as i32,
                stats.nb_resets as i32,
                stats.ggs_triggered as i32,
                stats.title_gags_triggered as i32,
                stats.title_gags_special_triggered as i32,
                stats.bye_byes_triggered as i32,
                stats.monument_triggers,
                stats.reached_floor_count,
                stats.floor_voice_lines_played
            ).execute(pool).await?;
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/*
-- Falls table
CREATE TABLE falls (
    id SERIAL PRIMARY KEY,
    session_token UUID REFERENCES sessions(session_token) NOT NULL,
    user_id UUID REFERENCES users(web_services_user_id) NOT NULL,
    start_floor INTEGER NOT NULL,
    start_pos_x DOUBLE PRECISION NOT NULL,
    start_pos_y DOUBLE PRECISION NOT NULL,
    start_pos_z DOUBLE PRECISION NOT NULL,
    start_speed DOUBLE PRECISION NOT NULL,
    start_time TIMESTAMP WITHOUT TIME ZONE NOT NULL,
    end_floor INTEGER NOT NULL,
    end_pos_x DOUBLE PRECISION NOT NULL,
    end_pos_y DOUBLE PRECISION NOT NULL,
    end_pos_z DOUBLE PRECISION NOT NULL,
    end_time TIMESTAMP WITHOUT TIME ZONE NOT NULL,
    ts TIMESTAMP WITHOUT TIME ZONE NOT NULL DEFAULT NOW()
);
*/

pub async fn update_fall_with_end(
    pool: &Pool<Postgres>,
    user_id: &Uuid,
    floor: i32,
    end_pos: (f32, f32, f32),
    end_time: i32,
) -> Result<(), sqlx::Error> {
    let most_recent_fall = query!(
        "SELECT id FROM falls WHERE user_id = $1 AND end_time IS NULL ORDER BY ts DESC LIMIT 1;",
        user_id
    )
    .fetch_one(pool)
    .await;
    let most_recent_fall = match most_recent_fall {
        Ok(r) => r.id,
        Err(e) => {
            warn!("Player {:?} has no most recent fall: {:?}", user_id, e);
            return Err(e);
        }
    };
    query!(
        "UPDATE falls SET end_floor = $1, end_pos_x = $2, end_pos_y = $3, end_pos_z = $4, end_time = $5 WHERE id = $6;",
        floor,
        end_pos.0 as f64,
        end_pos.1 as f64,
        end_pos.2 as f64,
        end_time,
        most_recent_fall
    )
    .execute(pool)
    .await?;

    Ok(())
}
