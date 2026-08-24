use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{query, Pool, Postgres};
use uuid::Uuid;

use crate::consts::DD2_MAP_UID;

pub const LISTED_MAPS_PAGE_SIZE: u32 = 100;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ListedMap {
    pub uid: String,
    pub became_listed: i64,
    pub map_name: Option<String>,
    pub author_id: Option<String>,
}

pub fn listed_maps_page_count(len: i64) -> i64 {
    (len as f64 / LISTED_MAPS_PAGE_SIZE as f64).ceil() as i64
}

/// First spec-on-map bind wins. Later specs do not replace it.
pub async fn bind_first_spec(pool: &Pool<Postgres>, map_uid: &str, spec_id: i64) -> Result<(), sqlx::Error> {
    query!(
        r#"
        INSERT INTO map_listing_bind (map_uid, spec_id)
        SELECT $1, id FROM custom_map_aux_specs WHERE id = $2
        ON CONFLICT (map_uid) DO NOTHING
        "#,
        map_uid,
        spec_id
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_bound_spec_id(pool: &Pool<Postgres>, map_uid: &str) -> Result<Option<i64>, sqlx::Error> {
    let row = query!(r#"SELECT spec_id FROM map_listing_bind WHERE map_uid = $1"#, map_uid)
        .fetch_optional(pool)
        .await?;
    Ok(row.and_then(|r| r.spec_id))
}

pub async fn get_listed_map(pool: &Pool<Postgres>, map_uid: &str) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
    let row = query!(r#"SELECT became_listed FROM listed_maps WHERE map_uid = $1 AND listed"#, map_uid)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| r.became_listed))
}

pub async fn get_listed_maps_len(pool: &Pool<Postgres>) -> Result<i64, sqlx::Error> {
    let row = query!(
        r#"
        SELECT COUNT(*) AS count
        FROM listed_maps
        WHERE listed AND map_uid <> $1
        "#,
        DD2_MAP_UID
    )
    .fetch_one(pool)
    .await?;
    Ok(row.count.unwrap_or(0))
}

pub async fn get_listed_maps_page(pool: &Pool<Postgres>, page: u32) -> Result<Vec<ListedMap>, sqlx::Error> {
    let limit = LISTED_MAPS_PAGE_SIZE as i64;
    let offset = (page as i64) * limit;
    let rows = query!(
        r#"
        SELECT lm.map_uid, lm.became_listed, mm.map_name, mm.author_id AS "author_id?"
        FROM listed_maps lm
        LEFT JOIN map_meta mm ON mm.map_uid = lm.map_uid
        WHERE lm.listed AND lm.map_uid <> $1
        ORDER BY lm.became_listed DESC, lm.map_uid ASC
        LIMIT $2 OFFSET $3
        "#,
        DD2_MAP_UID,
        limit,
        offset
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| ListedMap {
            uid: r.map_uid,
            became_listed: r.became_listed.timestamp(),
            map_name: r.map_name,
            author_id: r.author_id.map(|id| id.to_string()),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queries::custom_map_aux_specs::{listing_threshold_from_spec, upsert_spec};
    use crate::queries::moderation::{apply_map_ban, upsert_map_meta};
    use serde_json::json;
    use sqlx::postgres::PgPoolOptions;
    use std::sync::{Mutex, OnceLock};

    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner())
    }

    async fn test_pool() -> Pool<Postgres> {
        dotenv::dotenv().ok();
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for listed map tests");
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
        user_id
    }

    async fn cleanup_user(pool: &Pool<Postgres>, user_id: &Uuid) {
        let _ = query!("DELETE FROM users WHERE web_services_user_id = $1", user_id)
            .execute(pool)
            .await;
    }

    fn unique_map_uid(prefix: &str) -> String {
        let raw = format!("{}{}", prefix, Uuid::new_v4().simple());
        raw[..27].to_string()
    }

    async fn cleanup_map(pool: &Pool<Postgres>, map_uid: &str) {
        let _ = query!("DELETE FROM listed_maps WHERE map_uid = $1", map_uid).execute(pool).await;
        let _ = query!("DELETE FROM map_listing_bind WHERE map_uid = $1", map_uid)
            .execute(pool)
            .await;
        let _ = query!("DELETE FROM map_leaderboard WHERE map_uid = $1", map_uid)
            .execute(pool)
            .await;
        let _ = query!("DELETE FROM map_meta WHERE map_uid = $1", map_uid).execute(pool).await;
        let _ = query!("DELETE FROM map_bans WHERE map_uid = $1", map_uid).execute(pool).await;
    }

    async fn insert_lb(pool: &Pool<Postgres>, map_uid: &str, user_id: &Uuid, height: f64) {
        query!(
            r#"
            INSERT INTO map_leaderboard (map_uid, user_id, height, pos, race_time)
            VALUES ($1, $2, $3, $4, $5)
            "#,
            map_uid,
            user_id,
            height,
            &[1.0_f64, height, 2.0] as &[f64],
            1000
        )
        .execute(pool)
        .await
        .expect("insert map_leaderboard");
    }

    #[test]
    fn listed_map_json_shape_includes_unix_ts_and_optional_meta() {
        let row = ListedMap {
            uid: "abcdefghijklmnopqrstuvwxyza".into(),
            became_listed: 1_700_000_000,
            map_name: Some("Foo".into()),
            author_id: Some("0a2d1bc0-4aaa-4374-b2db-3d561bdab1c9".into()),
        };
        let v = serde_json::to_value(&row).expect("serialize");
        assert_eq!(v["uid"], "abcdefghijklmnopqrstuvwxyza");
        assert_eq!(v["became_listed"], 1_700_000_000);
        assert_eq!(v["map_name"], "Foo");
        assert_eq!(v["author_id"], "0a2d1bc0-4aaa-4374-b2db-3d561bdab1c9");
        assert_eq!(v.as_object().unwrap().len(), 4);
    }

    #[test]
    fn listed_map_json_null_meta_when_join_misses() {
        let row = ListedMap {
            uid: "abcdefghijklmnopqrstuvwxyza".into(),
            became_listed: 1_700_000_001,
            map_name: None,
            author_id: None,
        };
        let v = serde_json::to_value(&row).expect("serialize");
        assert!(v["map_name"].is_null());
        assert!(v["author_id"].is_null());
    }

    #[test]
    fn listed_maps_page_size_is_100() {
        assert_eq!(LISTED_MAPS_PAGE_SIZE, 100);
        assert_eq!(listed_maps_page_count(0), 0);
        assert_eq!(listed_maps_page_count(1), 1);
        assert_eq!(listed_maps_page_count(100), 1);
        assert_eq!(listed_maps_page_count(101), 2);
    }

    #[tokio::test]
    async fn upsert_writes_clamped_listing_threshold_column() {
        let _guard = test_lock();
        let pool = test_pool().await;
        let user = seed_user(&pool, "listed-thresh-upsert").await;
        let name_id = format!("t{}", &Uuid::new_v4().simple().to_string()[..8]);

        let id = upsert_spec(&pool, &user, &name_id, &json!({"listingThreshold": 3}))
            .await
            .expect("upsert low");
        let row = query!("SELECT listing_threshold FROM custom_map_aux_specs WHERE id = $1", id)
            .fetch_one(&pool)
            .await
            .expect("read threshold");
        assert_eq!(row.listing_threshold, 5);

        upsert_spec(&pool, &user, &name_id, &json!({"listingThreshold": 80}))
            .await
            .expect("upsert in window");
        let row = query!("SELECT listing_threshold FROM custom_map_aux_specs WHERE id = $1", id)
            .fetch_one(&pool)
            .await
            .expect("read updated threshold");
        assert_eq!(row.listing_threshold, 80);
        assert_eq!(listing_threshold_from_spec(&json!({"listingThreshold": 80})), 80);

        cleanup_user(&pool, &user).await;
    }

    #[tokio::test]
    async fn first_spec_on_map_bind_wins() {
        let _guard = test_lock();
        let pool = test_pool().await;
        let user = seed_user(&pool, "listed-bind-user").await;
        let map_uid = unique_map_uid("BindWin");
        let other_uid = unique_map_uid("BindShare");
        let a = upsert_spec(&pool, &user, "spec-a", &json!({"listingThreshold": 10}))
            .await
            .expect("spec a");
        let b = upsert_spec(&pool, &user, "spec-b", &json!({"listingThreshold": 50}))
            .await
            .expect("spec b");

        bind_first_spec(&pool, &map_uid, a).await.expect("bind a");
        bind_first_spec(&pool, &map_uid, b).await.expect("later bind ignored");
        bind_first_spec(&pool, &other_uid, a).await.expect("same spec on second map");

        assert_eq!(get_bound_spec_id(&pool, &map_uid).await.expect("bound"), Some(a));
        assert_eq!(get_bound_spec_id(&pool, &other_uid).await.expect("shared"), Some(a));

        cleanup_map(&pool, &map_uid).await;
        cleanup_map(&pool, &other_uid).await;
        cleanup_user(&pool, &user).await;
    }

    #[tokio::test]
    async fn flip_on_insert_not_update() {
        let _guard = test_lock();
        let pool = test_pool().await;
        let author = seed_user(&pool, "listed-flip-author").await;
        let map_uid = unique_map_uid("FlipIns");
        let spec_id = upsert_spec(&pool, &author, "flip", &json!({"listingThreshold": 5}))
            .await
            .expect("spec");
        bind_first_spec(&pool, &map_uid, spec_id).await.expect("bind");

        let mut players = Vec::new();
        for i in 0..4 {
            let u = seed_user(&pool, &format!("listed-flip-{i}")).await;
            insert_lb(&pool, &map_uid, &u, 10.0 + i as f64).await;
            players.push(u);
        }
        assert!(get_listed_map(&pool, &map_uid).await.expect("not listed yet").is_none());

        query!(
            "UPDATE map_leaderboard SET height = height + 10 WHERE map_uid = $1 AND user_id = $2",
            map_uid,
            players[0]
        )
        .execute(&pool)
        .await
        .expect("update existing standing");
        assert!(
            get_listed_map(&pool, &map_uid)
                .await
                .expect("still unlisted after update")
                .is_none(),
            "UPDATE of someone already on the board must not flip listed"
        );

        let fifth = seed_user(&pool, "listed-flip-5").await;
        insert_lb(&pool, &map_uid, &fifth, 50.0).await;
        let listed_at = get_listed_map(&pool, &map_uid)
            .await
            .expect("listed after 5th insert")
            .expect("became_listed");

        query!(
            "UPDATE map_leaderboard SET height = height + 1 WHERE map_uid = $1 AND user_id = $2",
            map_uid,
            fifth
        )
        .execute(&pool)
        .await
        .expect("update after listed");
        let sixth = seed_user(&pool, "listed-flip-6").await;
        insert_lb(&pool, &map_uid, &sixth, 60.0).await;
        let still = get_listed_map(&pool, &map_uid).await.expect("still listed").expect("ts");
        assert_eq!(still, listed_at, "once listed, later inserts must not change became_listed");

        upsert_spec(&pool, &author, "flip", &json!({"listingThreshold": 100}))
            .await
            .expect("raise threshold");
        assert!(
            get_listed_map(&pool, &map_uid).await.expect("raised").is_some(),
            "raising the threshold must not unlist"
        );

        cleanup_map(&pool, &map_uid).await;
        cleanup_user(&pool, &fifth).await;
        cleanup_user(&pool, &sixth).await;
        for u in players {
            cleanup_user(&pool, &u).await;
        }
        cleanup_user(&pool, &author).await;
    }

    #[tokio::test]
    async fn unbound_map_uses_default_25_and_dd2_is_never_listed() {
        let _guard = test_lock();
        let pool = test_pool().await;
        let map_uid = unique_map_uid("Def25_");
        let mut players = Vec::new();
        for i in 0..24 {
            let u = seed_user(&pool, &format!("listed-def25-{i}")).await;
            insert_lb(&pool, &map_uid, &u, i as f64).await;
            players.push(u);
        }
        assert!(get_listed_map(&pool, &map_uid).await.expect("24 < 25").is_none());
        let last = seed_user(&pool, "listed-def25-24").await;
        insert_lb(&pool, &map_uid, &last, 24.0).await;
        assert!(get_listed_map(&pool, &map_uid).await.expect("25th lists").is_some());

        let dd2_users: Vec<Uuid> = {
            let mut v = Vec::new();
            for i in 0..3 {
                let u = seed_user(&pool, &format!("listed-dd2-{i}")).await;
                insert_lb(&pool, DD2_MAP_UID, &u, 100.0 + i as f64).await;
                v.push(u);
            }
            v
        };
        assert!(
            get_listed_map(&pool, DD2_MAP_UID).await.expect("dd2").is_none(),
            "Deep Dip 2 must never become listed"
        );

        cleanup_map(&pool, &map_uid).await;
        for u in dd2_users {
            let _ = query!("DELETE FROM map_leaderboard WHERE map_uid = $1 AND user_id = $2", DD2_MAP_UID, u)
                .execute(&pool)
                .await;
            cleanup_user(&pool, &u).await;
        }
        cleanup_user(&pool, &last).await;
        for u in players {
            cleanup_user(&pool, &u).await;
        }
    }

    #[tokio::test]
    async fn shadowban_and_map_ban_do_not_count_toward_threshold() {
        let _guard = test_lock();
        let pool = test_pool().await;
        let author = seed_user(&pool, "listed-ban-author").await;
        let map_uid = unique_map_uid("BanCnt");
        let spec_id = upsert_spec(&pool, &author, "bancnt", &json!({"listingThreshold": 5}))
            .await
            .expect("spec");
        bind_first_spec(&pool, &map_uid, spec_id).await.expect("bind");

        let mut publics = Vec::new();
        for i in 0..4 {
            let u = seed_user(&pool, &format!("listed-ban-p{i}")).await;
            insert_lb(&pool, &map_uid, &u, i as f64).await;
            publics.push(u);
        }
        let shadowed = seed_user(&pool, "listed-ban-sb").await;
        query!("INSERT INTO shadow_bans (user_id, reason) VALUES ($1, $2)", shadowed, "test")
            .execute(&pool)
            .await
            .expect("shadowban");
        insert_lb(&pool, &map_uid, &shadowed, 10.0).await;
        assert!(get_listed_map(&pool, &map_uid).await.expect("shadowban ignored").is_none());

        let banned = seed_user(&pool, "listed-ban-mb").await;
        apply_map_ban(&pool, &map_uid, &banned, &author, "priv", "pub", None)
            .await
            .expect("map ban");
        insert_lb(&pool, &map_uid, &banned, 11.0).await;
        assert!(get_listed_map(&pool, &map_uid).await.expect("map ban ignored").is_none());

        let fifth = seed_user(&pool, "listed-ban-p4").await;
        insert_lb(&pool, &map_uid, &fifth, 12.0).await;
        assert!(get_listed_map(&pool, &map_uid).await.expect("5 public lists").is_some());

        cleanup_map(&pool, &map_uid).await;
        let _ = query!("DELETE FROM shadow_bans WHERE user_id = $1", shadowed).execute(&pool).await;
        cleanup_user(&pool, &fifth).await;
        for u in publics {
            cleanup_user(&pool, &u).await;
        }
        cleanup_user(&pool, &shadowed).await;
        cleanup_user(&pool, &banned).await;
        cleanup_user(&pool, &author).await;
    }

    #[tokio::test]
    async fn listed_route_sorts_and_pages_with_optional_meta() {
        let _guard = test_lock();
        let pool = test_pool().await;
        let author = seed_user(&pool, "listed-page-author").await;
        let early = unique_map_uid("PageA_");
        let late = unique_map_uid("PageB_");
        let tie_a = unique_map_uid("PageC0");
        let tie_b = unique_map_uid("PageC1");

        query!(
            "INSERT INTO listed_maps (map_uid, listed, became_listed) VALUES ($1, TRUE, NOW() - interval '2 hours')",
            early
        )
        .execute(&pool)
        .await
        .expect("early");
        query!(
            "INSERT INTO listed_maps (map_uid, listed, became_listed) VALUES ($1, TRUE, NOW() - interval '1 hour')",
            late
        )
        .execute(&pool)
        .await
        .expect("late");
        let tie_ts = chrono::Utc::now();
        query!(
            "INSERT INTO listed_maps (map_uid, listed, became_listed) VALUES ($1, TRUE, $2), ($3, TRUE, $2)",
            tie_a,
            tie_ts,
            tie_b
        )
        .execute(&pool)
        .await
        .expect("ties");

        upsert_map_meta(&pool, &late, author, Some("Late Map"), None).await.expect("meta");

        let page0 = get_listed_maps_page(&pool, 0).await.expect("page 0");
        let ours: Vec<_> = page0
            .iter()
            .filter(|r| r.uid == early || r.uid == late || r.uid == tie_a || r.uid == tie_b)
            .cloned()
            .collect();
        assert_eq!(ours.len(), 4);
        // newest first: ties (now) before late (-1h) before early (-2h); ties by uid ASC
        let mut expected = vec![tie_a.clone(), tie_b.clone()];
        expected.sort();
        assert_eq!(ours[0].uid, expected[0]);
        assert_eq!(ours[1].uid, expected[1]);
        assert_eq!(ours[2].uid, late);
        assert_eq!(ours[3].uid, early);
        assert_eq!(ours[2].map_name.as_deref(), Some("Late Map"));
        assert_eq!(ours[2].author_id.as_deref(), Some(author.to_string().as_str()));
        assert!(ours[3].map_name.is_none());
        assert!(ours[3].author_id.is_none());

        let far = get_listed_maps_page(&pool, 99).await.expect("empty page");
        assert!(far.is_empty());

        cleanup_map(&pool, &early).await;
        cleanup_map(&pool, &late).await;
        cleanup_map(&pool, &tie_a).await;
        cleanup_map(&pool, &tie_b).await;
        cleanup_user(&pool, &author).await;
    }
}
