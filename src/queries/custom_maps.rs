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
        SELECT COUNT(*) FROM map_curr_heights m
            LEFT JOIN shadow_bans sb ON m.user_id = sb.user_id
            WHERE sb.user_id IS NULL
              AND m.afk_update_count < 360
              AND m.updated_at > now() - interval '120 seconds'
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
        SELECT COUNT(*) FROM map_curr_heights m
            LEFT JOIN shadow_bans sb ON m.user_id = sb.user_id
            WHERE sb.user_id IS NULL
              AND m.afk_update_count < 360
              AND m.map_uid = $1
              AND m.updated_at > now() - interval '120 seconds'
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
        SELECT COUNT(*) FROM map_leaderboard m
        LEFT JOIN shadow_bans sb ON m.user_id = sb.user_id
        WHERE sb.user_id IS NULL AND m.map_uid = $1
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
        LEFT JOIN shadow_bans sb ON m.user_id = sb.user_id
        WHERE sb.user_id IS NULL AND m.map_uid = $1
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

/// Upsert a player's current live position for a map (custom-map live heights path).
pub async fn upsert_map_curr_height(
    pool: &Pool<Postgres>,
    map_uid: &str,
    user_id: &Uuid,
    pos: [f64; 3],
    race_time: i32,
) -> Result<(), sqlx::Error> {
    if map_uid.len() != 27 {
        return Err(sqlx::Error::RowNotFound);
    }
    query!(
        r#"--sql
            INSERT INTO map_curr_heights (map_uid, user_id, height, pos, race_time) VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (map_uid, user_id)
            DO UPDATE SET height = $3, pos = $4, race_time = $5, updated_at = now(), update_count = map_curr_heights.update_count + 1
        "#,
        map_uid,
        user_id,
        pos[1],
        &pos,
        race_time
    )
    .execute(pool)
    .await?;
    Ok(())
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
        LEFT JOIN shadow_bans sb ON m.user_id = sb.user_id
        WHERE sb.user_id IS NULL
          AND m.afk_update_count < 360
          AND m.map_uid = $1
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
            LEFT JOIN shadow_bans sb ON m.user_id = sb.user_id
            WHERE sb.user_id IS NULL
                AND m.afk_update_count < 360
                AND m.map_uid = $1
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consts::DD2_MAP_UID;
    use crate::queries::api::handle_get_map_uid_live_heights;
    use crate::queries::stats::{get_live_leaderboard, report_live_vehicle_state};
    use sqlx::postgres::PgPoolOptions;
    use std::sync::{Mutex, OnceLock};
    use uuid::Uuid;

    /// Serialize DB-backed live-height tests (shared DD2_MAP_UID rows).
    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    async fn test_pool() -> Pool<Postgres> {
        dotenv::dotenv().ok();
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for live height tests");
        PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("connect to DATABASE_URL")
    }

    async fn seed_user(pool: &Pool<Postgres>, name: &str) -> Uuid {
        let user_id = Uuid::now_v7();
        query!(
            "INSERT INTO users (web_services_user_id, display_name) VALUES ($1, $2)",
            user_id,
            name
        )
        .execute(pool)
        .await
        .expect("insert user");
        query!(
            "INSERT INTO colors (user_id, color) VALUES ($1, $2)",
            user_id,
            &[0.1_f64, 0.2, 0.3] as &[f64]
        )
        .execute(pool)
        .await
        .expect("insert color");
        user_id
    }

    async fn seed_session(pool: &Pool<Postgres>, user_id: &Uuid) -> Uuid {
        let session_token = Uuid::now_v7();
        query!(
            "INSERT INTO sessions (session_token, user_id, ip_address) VALUES ($1, $2, $3)",
            session_token,
            user_id,
            "127.0.0.1"
        )
        .execute(pool)
        .await
        .expect("insert session");
        session_token
    }

    async fn cleanup_user(pool: &Pool<Postgres>, user_id: &Uuid) {
        let _ = query!("DELETE FROM users WHERE web_services_user_id = $1", user_id)
            .execute(pool)
            .await;
    }

    /// Characterization: non-DD2 custom maps keep working via map_curr_heights.
    #[tokio::test]
    async fn custom_map_live_heights_unaffected_for_other_uids() {
        let _guard = test_lock();
        let pool = test_pool().await;
        let user_id = seed_user(&pool, "custom-map-live-ctrl").await;
        let map_uid = "CustomClimbMap_____________"; // 27 chars
        assert_eq!(map_uid.len(), 27);

        upsert_map_curr_height(&pool, map_uid, &user_id, [1.0, 55.5, 2.0], 500)
            .await
            .expect("upsert custom map height");

        let rows = get_map_live_heights(&pool, map_uid).await.expect("get_map_live_heights");
        let dd2_rows = get_map_live_heights(&pool, DD2_MAP_UID).await.expect("dd2 live");

        cleanup_user(&pool, &user_id).await;

        assert!(
            rows.iter().any(|p| p.user_id == user_id.to_string() && (p.height - 55.5).abs() < 1e-6),
            "custom map live heights must still work; got: {:?}",
            rows
        );
        assert!(
            !dd2_rows.iter().any(|p| p.user_id == user_id.to_string()),
            "custom-map upsert must not appear under DD2 live heights"
        );
    }

    /// Control: custom-map live heights read path works when `map_curr_heights` is populated
    /// for the DD2 UID (same table/API as other maps).
    #[tokio::test]
    async fn dd2_map_live_heights_api_returns_rows_from_map_curr_heights() {
        let _guard = test_lock();
        let pool = test_pool().await;
        let user_id = seed_user(&pool, "dd2-live-ctrl").await;
        let pos = [10.0_f64, 123.45, 20.0];
        upsert_map_curr_height(&pool, DD2_MAP_UID, &user_id, pos, 1000)
            .await
            .expect("upsert map_curr_heights");

        let rows = get_map_live_heights(&pool, DD2_MAP_UID)
            .await
            .expect("get_map_live_heights");
        let handler_ok = handle_get_map_uid_live_heights(&pool, DD2_MAP_UID.to_string())
            .await
            .is_ok();

        cleanup_user(&pool, &user_id).await;

        assert!(handler_ok, "handle_get_map_uid_live_heights should succeed for DD2 UID");
        assert!(
            rows.iter().any(|p| p.user_id == user_id.to_string() && (p.height - 123.45).abs() < 1e-6),
            "expected seeded DD2 player in map live heights, got: {:?}",
            rows
        );
    }

    /// Official DD2 vehicle reports must surface on `/map/{DD2}/live_heights`.
    #[tokio::test]
    async fn dd2_vehicle_live_report_visible_on_map_live_heights_api() {
        let _guard = test_lock();
        let pool = test_pool().await;
        let user_id = seed_user(&pool, "dd2-live-red").await;
        let session = seed_session(&pool, &user_id).await;

        report_live_vehicle_state(
            &pool,
            &session,
            &user_id,
            None,
            true, // official DD2 climb
            [10.0, 250.0, 20.0],
            [0.0, 0.0, 0.0, 1.0],
            [0.0, 1.0, 0.0],
        )
        .await
        .expect("report_live_vehicle_state");

        let rows = get_map_live_heights(&pool, DD2_MAP_UID)
            .await
            .expect("get_map_live_heights should not error");

        cleanup_user(&pool, &user_id).await;

        assert!(
            rows.iter().any(|p| p.user_id == user_id.to_string() && (p.height - 250.0).abs() < 1e-6),
            "DD2 vehicle-state live report must appear on map live heights API \
             (migration onto custom-map path). got: {:?}",
            rows
        );
    }

    /// Unofficial vehicle reports must not pollute DD2 map live heights.
    #[tokio::test]
    async fn unofficial_vehicle_report_does_not_appear_on_dd2_map_live_heights() {
        let _guard = test_lock();
        let pool = test_pool().await;
        let user_id = seed_user(&pool, "dd2-live-unofficial").await;
        let session = seed_session(&pool, &user_id).await;

        report_live_vehicle_state(
            &pool,
            &session,
            &user_id,
            None,
            false,
            [10.0, 999.0, 20.0],
            [0.0, 0.0, 0.0, 1.0],
            [0.0, 0.0, 0.0],
        )
        .await
        .expect("report_live_vehicle_state");

        let rows = get_map_live_heights(&pool, DD2_MAP_UID)
            .await
            .expect("get_map_live_heights");

        cleanup_user(&pool, &user_id).await;

        assert!(
            !rows.iter().any(|p| p.user_id == user_id.to_string()),
            "unofficial vehicle reports must not mirror into DD2 map live heights; got: {:?}",
            rows
        );
    }

    /// `/live_heights/global` must read DD2 live heights from map_curr_heights
    /// (same source as `/map/{DD2}/live_heights`).
    #[tokio::test]
    async fn dd2_live_heights_global_reads_map_curr_heights() {
        let _guard = test_lock();
        let pool = test_pool().await;
        let user_id = seed_user(&pool, "dd2-global-map").await;

        upsert_map_curr_height(&pool, DD2_MAP_UID, &user_id, [10.0, 300.0, 20.0], -1)
            .await
            .expect("upsert map_curr_heights");

        let rows = get_live_leaderboard(&pool).await.expect("get_live_leaderboard");

        cleanup_user(&pool, &user_id).await;

        assert!(
            rows.iter().any(|p| p.user_id == user_id.to_string() && (p.height - 300.0).abs() < 1e-6),
            "global live heights should include DD2 map_curr_heights rows; got: {:?}",
            rows
        );
    }

    /// End-to-end: official vehicle report must also appear on global live heights.
    #[tokio::test]
    async fn dd2_official_vehicle_report_visible_on_global_live_heights() {
        let _guard = test_lock();
        let pool = test_pool().await;
        let user_id = seed_user(&pool, "dd2-global-veh").await;
        let session = seed_session(&pool, &user_id).await;

        report_live_vehicle_state(
            &pool,
            &session,
            &user_id,
            None,
            true,
            [10.0, 275.0, 20.0],
            [0.0, 0.0, 0.0, 1.0],
            [0.0, 1.0, 0.0],
        )
        .await
        .expect("report_live_vehicle_state");

        let rows = get_live_leaderboard(&pool).await.expect("get_live_leaderboard");

        cleanup_user(&pool, &user_id).await;

        assert!(
            rows.iter().any(|p| p.user_id == user_id.to_string() && (p.height - 275.0).abs() < 1e-6),
            "global live heights should include official DD2 vehicle reports after mirror; got: {:?}",
            rows
        );
    }
}
