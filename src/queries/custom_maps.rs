use chrono::{DateTime, NaiveDateTime, Utc};
use sqlx::{query, Pool, Postgres};
use uuid::Uuid;

use crate::{
    queries::{vec_to_color, PlayerAtHeight},
    router::LeaderboardEntry2,
};

pub async fn get_nb_playing_live(pool: &Pool<Postgres>) -> Result<i64, sqlx::Error> {
    let nb_playing_now = query!(
        r#"--sql
        SELECT COUNT(*) FROM map_curr_heights
            WHERE updated_at > now() - interval '120 seconds'
    "#,
    )
    .fetch_one(pool)
    .await?
    .count
    .unwrap_or(0);
    Ok(nb_playing_now)
}

pub async fn get_map_nb_playing_live(pool: &Pool<Postgres>, map_uid: &str) -> Result<i64, sqlx::Error> {
    let nb_playing_now = query!(
        r#"--sql
        SELECT COUNT(*) FROM map_curr_heights WHERE map_uid = $1
            AND updated_at > now() - interval '120 seconds'
    "#,
        map_uid
    )
    .fetch_one(pool)
    .await?
    .count
    .unwrap_or(0);
    Ok(nb_playing_now)
}

pub async fn get_map_leaderboard_len(pool: &Pool<Postgres>, map_uid: &str) -> Result<i64, sqlx::Error> {
    if map_uid.len() > 30 {
        return Err(sqlx::Error::RowNotFound);
    }
    let resp = query!(
        r#"--sql
        SELECT COUNT(*) FROM map_leaderboard WHERE map_uid = $1
    "#,
        map_uid
    )
    .fetch_one(pool)
    .await?;
    Ok(resp.count.unwrap_or(0))
}

pub async fn get_map_leaderboard_page(pool: &Pool<Postgres>, map_uid: &str, page: u32) -> Result<Vec<LeaderboardEntry2>, sqlx::Error> {
    let start = (page * 100) as i64;
    let end = (start + 100) as i64;
    get_map_leaderboard(pool, map_uid, start, end).await
}

pub async fn get_map_leaderboard(
    pool: &Pool<Postgres>,
    map_uid: &str,
    start: i64,
    end: i64,
) -> Result<Vec<LeaderboardEntry2>, sqlx::Error> {
    if map_uid.len() > 30 {
        return Err(sqlx::Error::RowNotFound);
    }
    let resp = query!(r#"--sql
        SELECT m.user_id, u.display_name, c.color, m.pos, m.race_time, m.updated_at, m.update_count, rank() OVER (ORDER BY m.height DESC) AS rank
        FROM map_leaderboard m
        LEFT JOIN users u ON u.web_services_user_id = m.user_id
        LEFT JOIN colors c ON c.user_id = m.user_id
        WHERE m.map_uid = $1
        ORDER BY m.height DESC
        LIMIT $2
        OFFSET $3
    "#,
        &map_uid,
        end - start,
        start
    )
    .fetch_all(pool)
    .await?;
    let entries = resp
        .into_iter()
        .map(|r| LeaderboardEntry2 {
            rank: r.rank.unwrap_or_default() as u32,
            wsid: r.user_id.to_string(),
            pos: [r.pos[0], r.pos[1], r.pos[2]],
            ts: r.updated_at.and_utc().timestamp() as u32,
            name: r.display_name,
            update_count: r.update_count,
            color: [r.color[0], r.color[1], r.color[2]],
            race_time: r.race_time as i64,
        })
        .collect();
    Ok(entries)
}

/// This returns PlayerAtHeight entries for API
pub async fn get_map_live_heights(pool: &Pool<Postgres>, map_uid: &str) -> Result<Vec<PlayerAtHeight>, sqlx::Error> {
    if !(20..30).contains(&map_uid.len()) {
        return Err(sqlx::Error::RowNotFound);
    }
    let resp = query!(
        r#"--sql
        SELECT m.user_id, u.display_name, c.color, m.pos, m.height, m.updated_at, m.update_count, m.afk_update_count, m.velocity, m.dt, 0 AS rank
        FROM map_curr_heights m
        LEFT JOIN users u ON u.web_services_user_id = m.user_id
        LEFT JOIN colors c ON c.user_id = m.user_id
        WHERE m.map_uid = $1
          AND m.updated_at > now() - interval '120 seconds'
        ORDER BY m.height DESC
    "#,
        map_uid
    )
    .fetch_all(pool)
    .await?;
    let entries = resp
        .into_iter()
        .enumerate()
        .map(|(i, r)| PlayerAtHeight {
            user_id: r.user_id.to_string(),
            pos: Some([r.pos[0], r.pos[1], r.pos[2]]),
            ts: r.updated_at.and_utc().timestamp(),
            display_name: r.display_name,
            color: Some([r.color[0], r.color[1], r.color[2]]),
            height: r.height,
            rank: i as i64 + 1,
            vel: Some(round_v3([r.velocity[0], r.velocity[1], r.velocity[2]], 3)),
            afk_count: r.afk_update_count as i32,
            update_count: r.update_count as i32,
            dt: r.dt as f32,
        })
        .collect();
    Ok(entries)
}

fn round_v3(v: [f64; 3], precision: usize) -> [f64; 3] {
    let factor = 10f64.powi(precision as i32);
    [
        (v[0] * factor).round() / factor,
        (v[1] * factor).round() / factor,
        (v[2] * factor).round() / factor,
    ]
}

/// This returns LeaderboardEntry2 entries for Plugin
pub async fn get_map_live_heights_top_n(pool: &Pool<Postgres>, map_uid: &str, n: u32) -> Result<Vec<LeaderboardEntry2>, sqlx::Error> {
    if !(20..30).contains(&map_uid.len()) {
        return Err(sqlx::Error::RowNotFound);
    }
    let resp = query!(
        r#"--sql
            SELECT m.user_id, u.display_name, c.color, m.pos, m.race_time, m.updated_at, m.update_count, rank() OVER (ORDER BY m.height DESC) AS rank FROM map_curr_heights m
            LEFT JOIN users u ON u.web_services_user_id = m.user_id
            LEFT JOIN colors c ON c.user_id = m.user_id
            WHERE m.map_uid = $1
                AND m.updated_at > now() - interval '120 seconds'
            ORDER BY m.height DESC
            LIMIT $2
        "#,
        map_uid,
        n as i64
    )
    .fetch_all(pool)
    .await?;

    let entries = resp
        .into_iter()
        .enumerate()
        .map(|(i, r)| LeaderboardEntry2 {
            rank: r.rank.unwrap_or_default() as u32,
            wsid: r.user_id.to_string(),
            pos: [r.pos[0], r.pos[1], r.pos[2]],
            ts: r.updated_at.and_utc().timestamp() as u32,
            name: r.display_name,
            update_count: r.update_count,
            color: [r.color[0], r.color[1], r.color[2]],
            race_time: r.race_time as i64,
        })
        .collect();
    Ok(entries)
}

pub(crate) async fn get_players_spec_info(pool: &Pool<Postgres>, uid: &str, wsid: &Uuid) -> Result<(i64, DateTime<Utc>), sqlx::Error> {
    let resp = query!(
        r#"--sql
        SELECT updated_at, (stats ->> 'seconds_spent_in_map')::BIGINT AS seconds_spent_in_map
        FROM custom_map_stats
        WHERE user_id = $1 AND map_uid = $2
    "#,
        wsid,
        uid
    )
    .fetch_one(pool)
    .await?;
    Ok((resp.seconds_spent_in_map.unwrap_or(0), resp.updated_at))
}

pub(crate) async fn report_map_stats(
    pool: &Pool<Postgres>,
    user_id: &Uuid,
    uid: &str,
    stats: serde_json::Value,
) -> Result<(), sqlx::Error> {
    query!(
        r#"--sql
        INSERT INTO custom_map_stats (user_id, map_uid, stats)
        VALUES ($1, $2, $3)
        ON CONFLICT (user_id, map_uid)
        DO UPDATE SET stats = $3, updated_at = NOW()
    "#,
        user_id,
        uid,
        stats
    )
    .execute(pool)
    .await?;
    Ok(())
}
