use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use log::{info, warn};
use sqlx::{prelude::FromRow, query, query_as, types::Uuid, Pool, Postgres};

use crate::router::Stats;

pub async fn log_ml_ping(pool: &Pool<Postgres>, ip_v4: &str, ip_v6: &str, is_intro: bool, user_agent: &str) -> Result<(), sqlx::Error> {
    query!(
        "INSERT INTO ml_pings (ip_v4, ip_v6, is_intro, user_agent) VALUES ($1, $2, $3, $4);",
        ip_v4,
        ip_v6,
        is_intro,
        user_agent
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn update_users_stats(pool: &Pool<Postgres>, user_id: &Uuid, stats: &Stats) -> Result<(), sqlx::Error> {
    info!("Updating stats for user {:?}", user_id);
    let r = query!("UPDATE stats SET nb_jumps = $1, nb_falls = $2, nb_floors_fallen = $3, last_pb_set_ts = $4, total_dist_fallen = $5, pb_height = $6, pb_floor = $7, nb_resets = $8, ggs_triggered = $9, title_gags_triggered = $10, title_gags_special_triggered = $11, bye_byes_triggered = $12, monument_triggers = $13, reached_floor_count = $14, floor_voice_lines_played = $15, seconds_spent_in_map = $16, update_count = update_count + 1, ts = NOW() WHERE user_id = $17 RETURNING update_count;",
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
        stats.seconds_spent_in_map,
        user_id
    ).fetch_one(pool).await;
    match r {
        Ok(r) => {
            if (r.update_count + 8) % 10 == 0 {
                // insert into stats_archive
                query!("INSERT INTO stats_archive (user_id, seconds_spent_in_map, nb_jumps, nb_falls, nb_floors_fallen, last_pb_set_ts, total_dist_fallen, pb_height, pb_floor, nb_resets, ggs_triggered, title_gags_triggered, title_gags_special_triggered, bye_byes_triggered, monument_triggers, reached_floor_count, floor_voice_lines_played, update_count, rank_at_time)
                    SELECT user_id, seconds_spent_in_map, nb_jumps, nb_falls, nb_floors_fallen, last_pb_set_ts, total_dist_fallen, pb_height, pb_floor, nb_resets, ggs_triggered, title_gags_triggered, title_gags_special_triggered, bye_byes_triggered, monument_triggers, reached_floor_count, floor_voice_lines_played, update_count, rank() over (ORDER BY pb_height DESC) as rank_at_time
                    FROM stats WHERE user_id = $1;", user_id).execute(pool).await?;
            }
            Ok(())
        }
        Err(sqlx::Error::RowNotFound) => {
            query!("INSERT INTO stats (user_id, seconds_spent_in_map, nb_jumps, nb_falls, nb_floors_fallen, last_pb_set_ts, total_dist_fallen, pb_height, pb_floor, nb_resets, ggs_triggered, title_gags_triggered, title_gags_special_triggered, bye_byes_triggered, monument_triggers, reached_floor_count, floor_voice_lines_played) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17);",
                user_id,
                stats.seconds_spent_in_map,
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

pub async fn insert_start_fall(
    pool: &Pool<Postgres>,
    user_id: &Uuid,
    session_id: &Uuid,
    start_floor: i32,
    start_pos: (f32, f32, f32),
    start_speed: f32,
    start_time: i32,
) -> Result<(), sqlx::Error> {
    query!(
        "INSERT INTO falls (session_token, user_id, start_floor, start_pos_x, start_pos_y, start_pos_z, start_speed, start_time) VALUES ($1, $2, $3, $4, $5, $6, $7, $8);",
        session_id,
        user_id,
        start_floor,
        start_pos.0 as f64,
        start_pos.1 as f64,
        start_pos.2 as f64,
        start_speed as f64,
        start_time,
    ).execute(pool).await?;
    Ok(())
}

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

pub async fn insert_respawn(pool: &Pool<Postgres>, session_id: &Uuid, race_time: i32) -> Result<(), sqlx::Error> {
    query!(
        "INSERT INTO respawns (session_token, race_time) VALUES ($1, $2);",
        session_id,
        race_time
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn insert_finish(pool: &Pool<Postgres>, session_id: &Uuid, race_time: i32) -> Result<(), sqlx::Error> {
    query!(
        "INSERT INTO finishes (session_token, race_time) VALUES ($1, $2);",
        session_id,
        race_time
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn insert_vehicle_state(
    pool: &Pool<Postgres>,
    session_id: &Uuid,
    context_id: Option<&Uuid>,
    is_official: bool,
    pos: [f32; 3],
    rotq: [f32; 4],
    vel: [f32; 3],
) -> Result<(), sqlx::Error> {
    query!(
        "INSERT INTO vehicle_states (session_token, context_id, is_official, pos, rotq, vel) VALUES ($1, $2, $3, $4, $5, $6);",
        session_id,
        context_id,
        is_official,
        &pos.map(|f| f as f64),
        &rotq.map(|f| f as f64),
        &vel.map(|f| f as f64),
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn update_user_pb_height(pool: &Pool<Postgres>, user_id: &Uuid, height: f64) -> Result<(), sqlx::Error> {
    return match query!("SELECT height FROM leaderboard WHERE user_id = $1;", user_id)
        .fetch_one(pool)
        .await
    {
        Ok(r) => {
            if r.height < height {
                let uc = query!(
                    "UPDATE leaderboard SET height = $1, ts = NOW(), update_count = update_count + 1 WHERE user_id = $2 RETURNING update_count;",
                    height,
                    user_id
                )
                .fetch_one(pool)
                .await?;
                if uc.update_count + 2 % 10 == 0 {
                    query!("INSERT INTO leaderboard_archive (user_id, height, rank_at_time) SELECT user_id, height, rank() over (ORDER BY height DESC) as rank_at_time FROM leaderboard WHERE user_id = $1;", user_id).execute(pool).await?;
                }
            }
            Ok(())
        }
        Err(sqlx::Error::RowNotFound) => {
            query!(
                "INSERT INTO leaderboard (user_id, height, ts) VALUES ($1, $2, NOW());",
                user_id,
                height
            )
            .execute(pool)
            .await?;
            Ok(())
        }
        Err(e) => Err(e),
    };
}
