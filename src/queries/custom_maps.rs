use sqlx::{query, Pool, Postgres};

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
        SELECT m.user_id, u.display_name, c.color, m.pos, m.race_time, m.updated_at, m.update_count, rank() OVER (ORDER BY m.height DESC) AS rank FROM map_leaderboard m
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
        SELECT m.user_id, u.display_name, c.color, m.pos, m.height, m.updated_at, m.update_count, 0 AS rank
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
            vel: None,
        })
        .collect();
    Ok(entries)
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
