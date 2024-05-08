use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, NaiveDateTime, Utc};
use log::{info, warn};
use serde::{Deserialize, Serialize};
use sqlx::{prelude::FromRow, query, query_as, types::Uuid, Pool, Postgres};

use crate::{
    consts::DD2_MAP_UID,
    router::{LeaderboardEntry, Stats},
};

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
                query!(r#"--sql
                INSERT INTO stats_archive (user_id, seconds_spent_in_map, nb_jumps, nb_falls, nb_floors_fallen, last_pb_set_ts, total_dist_fallen, pb_height, pb_floor, nb_resets, ggs_triggered, title_gags_triggered, title_gags_special_triggered, bye_byes_triggered, monument_triggers, reached_floor_count, floor_voice_lines_played, update_count, rank_at_time)
                    SELECT user_id, seconds_spent_in_map, nb_jumps, nb_falls, nb_floors_fallen, last_pb_set_ts, total_dist_fallen, pb_height, pb_floor, nb_resets, ggs_triggered, title_gags_triggered, title_gags_special_triggered, bye_byes_triggered, monument_triggers, reached_floor_count, floor_voice_lines_played, update_count, rank as rank_at_time
                    FROM ranked_stats WHERE user_id = $1;
                "#, user_id).execute(pool).await?;
            }
            // check that lb is up to date, should be close
            update_user_pb_height(pool, user_id, stats.pb_height as f64).await?;
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

pub async fn get_user_stats(pool: &Pool<Postgres>, user_id: &Uuid) -> Result<(Stats, u32), sqlx::Error> {
    let r = query!("SELECT seconds_spent_in_map, nb_jumps, nb_falls, nb_floors_fallen, last_pb_set_ts, total_dist_fallen, pb_height, pb_floor, nb_resets, ggs_triggered, title_gags_triggered, title_gags_special_triggered, bye_byes_triggered, monument_triggers, reached_floor_count, floor_voice_lines_played, update_count, rank as rank_at_time FROM ranked_stats WHERE user_id = $1;", user_id)
        .fetch_one(pool)
        .await?;
    let mut s = Stats {
        seconds_spent_in_map: r.seconds_spent_in_map.unwrap_or_default(),
        nb_jumps: r.nb_jumps.unwrap_or_default() as u32,
        nb_falls: r.nb_falls.unwrap_or_default() as u32,
        nb_floors_fallen: r.nb_floors_fallen.unwrap_or_default() as u32,
        last_pb_set_ts: r.last_pb_set_ts.map(|t| t.and_utc().timestamp()).unwrap_or(0) as u32,
        total_dist_fallen: r.total_dist_fallen.unwrap_or_default() as f32,
        pb_height: r.pb_height.unwrap_or_default() as f32,
        pb_floor: r.pb_floor.unwrap_or_default() as u32,
        nb_resets: r.nb_resets.unwrap_or_default() as u32,
        ggs_triggered: r.ggs_triggered.unwrap_or_default() as u32,
        title_gags_triggered: r.title_gags_triggered.unwrap_or_default() as u32,
        title_gags_special_triggered: r.title_gags_special_triggered.unwrap_or_default() as u32,
        bye_byes_triggered: r.bye_byes_triggered.unwrap_or_default() as u32,
        monument_triggers: r.monument_triggers.unwrap_or_default(),
        reached_floor_count: r.reached_floor_count.unwrap_or_default(),
        floor_voice_lines_played: r.floor_voice_lines_played.unwrap_or_default(),
    };
    let r2 = get_user_in_lb(pool, user_id).await?;
    match r2 {
        Some(lbe) => {
            s.pb_height = lbe.height as f32;
        }
        None => {}
    };
    Ok((s, r.rank_at_time.unwrap_or(u32::MAX as i64) as u32))
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
    // info!("Inserting respawn for session {:?} at race time {:?}", session_id, race_time);
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

pub struct PBUpdateRes {
    pub is_top_3: bool,
}

pub async fn update_user_pb_height(pool: &Pool<Postgres>, user_id: &Uuid, height: f64) -> Result<PBUpdateRes, sqlx::Error> {
    return match query!("SELECT height FROM leaderboard WHERE user_id = $1;", user_id)
        .fetch_one(pool)
        .await
    {
        Ok(r) => {
            if r.height < height {
                if height - r.height > 50. {
                    warn!("Player {:?} has large update to PB height: {:?} -> {:?}", user_id, r.height, height);
                }
                query!(
                    "UPDATE leaderboard SET height = $1, ts = NOW(), update_count = update_count + 1 WHERE user_id = $2 RETURNING update_count;",
                    height,
                    user_id
                )
                .fetch_one(pool)
                .await?;
                let r = query!(r#"--sql
                INSERT INTO leaderboard_archive (user_id, height, rank_at_time) SELECT user_id, height, rank FROM ranked_lb_view WHERE user_id = $1
                    RETURNING rank_at_time;"#, user_id).fetch_one(pool).await?;
                if r.rank_at_time < 10 {
                    return Ok(PBUpdateRes { is_top_3: true });
                }
            } else if r.height - height > 50. {
                warn!(
                    "Player {:?} tried to update PB height to {:?} but current PB is {:?}",
                    user_id, height, r.height
                );
            } else {
                // info!("Player {:?} tried to update PB height to {:?} but current PB is {:?}", user_id, height, r.height);
            }
            Ok(PBUpdateRes { is_top_3: false })
        }
        Err(sqlx::Error::RowNotFound) => {
            query!(
                "INSERT INTO leaderboard (user_id, height, ts) VALUES ($1, $2, NOW());",
                user_id,
                height
            )
            .execute(pool)
            .await?;
            Ok(PBUpdateRes { is_top_3: false })
        }
        Err(e) => Err(e),
    };
}

#[derive(FromRow, Debug, Clone)]
pub struct LBEntry {
    pub wsid: Uuid,
    pub height: f64,
    pub rank: Option<i64>,
    pub ts: NaiveDateTime,
    pub display_name: Option<String>,
    pub update_count: i32,
}
impl Into<LeaderboardEntry> for LBEntry {
    fn into(self) -> LeaderboardEntry {
        LeaderboardEntry {
            wsid: self.wsid.to_string(),
            height: self.height as f32,
            rank: self.rank.unwrap_or(99999) as u32,
            ts: self.ts.and_utc().timestamp() as u32,
            name: self.display_name.unwrap_or_else(|| "?".to_string()),
            update_count: self.update_count,
        }
    }
}

pub async fn get_global_lb(pool: &Pool<Postgres>, start: i32, end: i32) -> Result<Vec<LBEntry>, sqlx::Error> {
    if end < start {
        return Ok(vec![]);
    }
    let limit = end - start;
    let r = query!(
        "SELECT user_id as wsid, height, ts, rank, display_name, update_count FROM ranked_lb_view LIMIT $1 OFFSET $2;",
        limit as i64,
        (start as i64 - 1).max(0),
    )
    .fetch_all(pool)
    .await?;
    Ok(r.into_iter()
        .map(|e| LBEntry {
            wsid: e.wsid.unwrap(),
            height: e.height.unwrap(),
            rank: e.rank,
            ts: e.ts.unwrap(),
            display_name: e.display_name,
            update_count: e.update_count.unwrap_or_default(),
        })
        .collect())
}

pub async fn get_user_in_lb(pool: &Pool<Postgres>, user_id: &Uuid) -> Result<Option<LBEntry>, sqlx::Error> {
    let r = query!(
        r#"--sql
        SELECT user_id as wsid, height, ts, rank, display_name, update_count FROM ranked_lb_view WHERE user_id = $1;"#,
        user_id
    )
    .fetch_optional(pool)
    .await?;
    Ok(r.map(|e| LBEntry {
        wsid: e.wsid.unwrap(),
        height: e.height.unwrap(),
        rank: e.rank,
        ts: e.ts.unwrap(),
        display_name: e.display_name,
        update_count: e.update_count.unwrap_or_default(),
    }))
}

pub async fn get_friends_lb(pool: &Pool<Postgres>, user_id: &Uuid, friends: &[Uuid]) -> Result<Vec<LBEntry>, sqlx::Error> {
    let r = query!(
        "SELECT user_id as wsid, height, ts, rank, display_name, update_count FROM ranked_lb_view WHERE user_id = ANY($1) ORDER BY height DESC;",
        friends
    )
    .fetch_all(pool)
    .await?;
    Ok(r.into_iter()
        .map(|e| LBEntry {
            wsid: e.wsid.unwrap(),
            height: e.height.unwrap(),
            rank: e.rank,
            ts: e.ts.unwrap(),
            display_name: e.display_name,
            update_count: e.update_count.unwrap_or_default(),
        })
        .collect())
}

pub async fn get_global_overview(pool: &Pool<Postgres>) -> Result<serde_json::Value, sqlx::Error> {
    let r = query!("SELECT * FROM cached_json WHERE key = $1;", "global_overview")
        .fetch_one(pool)
        .await?;
    Ok(r.value)
}

pub async fn update_global_overview(pool: &Pool<Postgres>) -> Result<serde_json::Value, sqlx::Error> {
    let players = query!("SELECT COUNT(*) FROM users;").fetch_one(pool).await?.count;
    let sessions = query!("SELECT COUNT(*) FROM sessions;").fetch_one(pool).await?.count;
    let rjf = query!("SELECT SUM(nb_resets) as resets, SUM(nb_jumps) as jumps, SUM(nb_falls) as falls, SUM(nb_floors_fallen) as floors_fallen, SUM(total_dist_fallen) as height_fallen FROM stats;").fetch_one(pool).await?;
    let falls_raw = query!("SELECT COUNT(*) FROM falls;").fetch_one(pool).await?.count;
    // let jumps_count = query!("SELECT COUNT(*) FROM falls_only_jumps;").fetch_one(pool).await?.count;
    // let falls_count = query!("SELECT COUNT(*) FROM falls_no_jumps;").fetch_one(pool).await?.count;
    // let falls_minor = query!("SELECT COUNT(*) FROM falls_minor;").fetch_one(pool).await?.count;
    let nb_players_live = get_server_info(pool).await.unwrap_or(0);
    let live_lb = get_live_leaderboard(pool).await?;

    // let map_loads = query!("SELECT SUM(load_count) FROM maps WHERE uid = $1;", DD2_MAP_UID)
    //     .fetch_one(pool)
    //     .await
    //     .map(|r| r.load_count)
    //     .unwrap_or(0);
    let new_overview = serde_json::json!({
        "players": players,
        "sessions": sessions,
        "resets": rjf.resets,
        "jumps": rjf.jumps,
        "falls": rjf.falls,
        "floors_fallen": rjf.floors_fallen,
        "height_fallen": rjf.height_fallen,
        // "map_loads": map_loads,
        "ts": SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
        "falls_raw": falls_raw,
        // "jumps_count": jumps_count,
        // "falls_count": falls_count,
        // "falls_minor": falls_minor,
        "nb_players_live": nb_players_live,
        "nb_players_climbing": live_lb.len() as i32,
    });
    query!(
        "INSERT INTO cached_json (key, value) VALUES ($1, $2) ON CONFLICT (key) DO UPDATE SET value = $2;",
        "global_overview",
        new_overview
    )
    .execute(pool)
    .await?;
    Ok(new_overview)
}

pub async fn update_server_stats(pool: &Pool<Postgres>, nb_players_live: i32) -> Result<(), sqlx::Error> {
    let h = hostname::get();
    let server_id = h
        .map(|h| h.into_string().unwrap_or("cannot_decode".to_string()))
        .unwrap_or("unknown".to_string());
    query!(
        "INSERT INTO server_player_counts (server_id, player_count, updated_at) VALUES ($1, $2, NOW()) ON CONFLICT (server_id) DO UPDATE SET player_count = $2, updated_at = NOW();",
        server_id,
        nb_players_live,
    )
    .execute(pool)
    .await?;
    let nb_players_live = get_server_info(pool).await?;
    query!(
        "INSERT INTO server_connected_stats (nb_players) VALUES ($1);",
        nb_players_live as i32
    )
    .execute(pool)
    .await?;
    let lb = get_live_leaderboard(pool).await?;
    query!("INSERT INTO active_players_stats (nb_players) VALUES ($1);", lb.len() as i32)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn get_server_info(pool: &Pool<Postgres>) -> Result<u32, sqlx::Error> {
    let r = query!("SELECT SUM(player_count) FROM server_player_counts WHERE updated_at > NOW() - INTERVAL '60 seconds';")
        .fetch_one(pool)
        .await?;
    Ok(r.sum.unwrap_or(0) as u32)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerHeightLog {
    pub display_name: String,
    pub user_id: String,
    pub last_5_points: Vec<(f64, i64)>,
}

pub async fn get_users_latest_height(pool: &Pool<Postgres>, user_id: &Uuid) -> Result<PlayerHeightLog, sqlx::Error> {
    let r = query!(
        r#"--sql
        WITH latest_session AS (
            SELECT u.display_name, s.* FROM sessions as s
            INNER JOIN users as u ON s.user_id = u.web_services_user_id
            WHERE s.user_id = $1
            ORDER BY s.created_ts DESC LIMIT 1
        ),
        latest_official_vs AS (
            SELECT l.display_name, l.user_id, vs.ts, vs.pos[2] as height
            FROM latest_session AS l
            INNER JOIN vehicle_states AS vs ON vs.session_token = l.session_token AND vs.is_official = true
            ORDER BY vs.ts DESC LIMIT 5
        )
        SELECT * from latest_official_vs;
    "#,
        user_id
    )
    .fetch_all(pool)
    .await?;
    let last_5_points = r
        .iter()
        .map(|r| (r.height.unwrap_or_default(), r.ts.and_utc().timestamp()))
        .collect();
    Ok(PlayerHeightLog {
        display_name: r.get(0).map(|r| r.display_name.to_string()).unwrap_or_else(|| "? unknown ?".into()),
        user_id: user_id.to_string(),
        last_5_points,
    })
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PlayerAtHeight {
    pub display_name: String,
    pub user_id: String,
    pub height: f64,
    pub ts: i64,
    pub rank: i64,
}

pub async fn get_live_leaderboard(pool: &Pool<Postgres>) -> Result<Vec<PlayerAtHeight>, sqlx::Error> {
    let r = query!(
        r#"--sql
        WITH recent_points AS (
            SELECT * FROM vehicle_states
            WHERE ts > NOW() - INTERVAL '180 seconds' AND is_official = true
            ORDER BY ts
        ),
        rankings AS (
            SELECT *,
                    ROW_NUMBER() OVER (PARTITION BY session_token ORDER BY ts DESC) AS rn
            FROM recent_points
        ),
        rankings2 AS (
            SELECT s.user_id, r.pos[2] as height, r.ts, r.rn,
                    ROW_NUMBER() OVER (PARTITION BY s.user_id ORDER BY r.ts DESC) AS rn2
            FROM rankings r
            INNER JOIN sessions s ON s.session_token = r.session_token
        )
        SELECT u.display_name, r.user_id, r.height, r.ts FROM rankings2 r
        LEFT JOIN users u on r.user_id = u.web_services_user_id
        WHERE rn = 1 AND rn2 = 1
        ORDER BY height DESC;
    "#
    )
    .fetch_all(pool)
    .await?;
    Ok(r.into_iter()
        .enumerate()
        .map(|(i, r)| PlayerAtHeight {
            display_name: r.display_name,
            user_id: r.user_id.map(|u| u.to_string()).unwrap_or_else(|| "? unknown ?".to_string()),
            height: r.height.unwrap_or_default(),
            ts: r.ts.and_utc().timestamp(),
            rank: (i + 1) as i64,
        })
        .collect())
}
