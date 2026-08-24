use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{query, Pool, Postgres};
use thiserror::Error;
use uuid::Uuid;

use crate::consts::DD2_MAP_UID;
use crate::nadeo::MapAuthorVerifier;

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

#[derive(Debug, Error)]
pub enum BindSpecError {
    #[error("map is already bound to a different spec")]
    ConflictingBinding,
    #[error("spec does not exist")]
    SpecNotFound,
    #[error("authenticated reporter does not own the spec")]
    SpecNotOwned,
    #[error("Nadeo map author does not own the spec")]
    AuthorMismatch,
    #[error("Nadeo map author verification failed: {0}")]
    AuthorVerification(String),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

struct SpecBinding {
    spec_id: i64,
    verified: bool,
}

async fn get_spec_binding(pool: &Pool<Postgres>, map_uid: &str) -> Result<Option<SpecBinding>, sqlx::Error> {
    let row = query!(
        r#"
        SELECT spec_id, author_verified_at IS NOT NULL AS "verified!"
        FROM map_listing_bind
        WHERE map_uid = $1
        "#,
        map_uid
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| SpecBinding {
        spec_id: row.spec_id,
        verified: row.verified,
    }))
}

/// Establish or reuse the immutable authoritative map -> spec binding.
///
/// A first bind (including promotion of a legacy unverified bind) is accepted
/// only when the authenticated reporter owns the spec and Nadeo says that same
/// account authored the map. A unique map row resolves concurrent first binds;
/// after the race, every contender reads and checks the actual winner.
pub async fn bind_authoritative_spec(
    pool: &Pool<Postgres>,
    verifier: &dyn MapAuthorVerifier,
    reporter_id: &Uuid,
    map_uid: &str,
    requested_spec_id: i64,
) -> Result<i64, BindSpecError> {
    if let Some(binding) = get_spec_binding(pool, map_uid).await? {
        if binding.spec_id != requested_spec_id {
            return Err(BindSpecError::ConflictingBinding);
        }
        if binding.verified {
            return Ok(binding.spec_id);
        }
    }

    let spec = query!("SELECT user_id FROM custom_map_aux_specs WHERE id = $1", requested_spec_id)
        .fetch_optional(pool)
        .await?
        .ok_or(BindSpecError::SpecNotFound)?;
    if spec.user_id != *reporter_id {
        return Err(BindSpecError::SpecNotOwned);
    }

    let nadeo_author = verifier
        .author_id_for_uid(map_uid)
        .await
        .map_err(BindSpecError::AuthorVerification)?;
    if nadeo_author != spec.user_id {
        return Err(BindSpecError::AuthorMismatch);
    }

    let mut tx = pool.begin().await?;
    query!(
        r#"
        INSERT INTO map_listing_bind (map_uid, spec_id, verified_author_id, author_verified_at)
        SELECT $1, id, user_id, NOW()
        FROM custom_map_aux_specs
        WHERE id = $2 AND user_id = $3
        ON CONFLICT (map_uid) DO NOTHING
        "#,
        map_uid,
        requested_spec_id,
        reporter_id
    )
    .execute(&mut *tx)
    .await?;
    query!(
        r#"
        UPDATE map_listing_bind
        SET verified_author_id = $3, author_verified_at = NOW()
        WHERE map_uid = $1 AND spec_id = $2 AND author_verified_at IS NULL
        "#,
        map_uid,
        requested_spec_id,
        reporter_id
    )
    .execute(&mut *tx)
    .await?;
    let winner = query!(
        r#"
        SELECT spec_id, author_verified_at IS NOT NULL AS "verified!"
        FROM map_listing_bind
        WHERE map_uid = $1
        "#,
        map_uid
    )
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;

    let Some(winner) = winner else {
        return Err(BindSpecError::SpecNotFound);
    };
    if winner.spec_id != requested_spec_id {
        return Err(BindSpecError::ConflictingBinding);
    }
    if !winner.verified {
        return Err(BindSpecError::AuthorVerification("binding was not durably verified".to_string()));
    }
    Ok(winner.spec_id)
}

/// Resolve an already verified binding, treating the client spec id only as a
/// consistency hint. Unbound, legacy-unverified, and mismatched reports fail
/// closed without consulting client-selected catalog rows.
pub async fn resolve_bound_spec(pool: &Pool<Postgres>, map_uid: &str, client_spec_id: i64) -> Result<Option<i64>, sqlx::Error> {
    let row = query!(
        r#"
        SELECT spec_id
        FROM map_listing_bind
        WHERE map_uid = $1
          AND spec_id = $2
          AND author_verified_at IS NOT NULL
        "#,
        map_uid,
        client_spec_id
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| row.spec_id))
}

/// First spec-on-map bind wins. Later specs do not replace it.
#[cfg(test)]
pub async fn bind_first_spec(pool: &Pool<Postgres>, map_uid: &str, spec_id: i64) -> Result<(), sqlx::Error> {
    query!(
        r#"
        INSERT INTO map_listing_bind (map_uid, spec_id, verified_author_id, author_verified_at)
        SELECT $1, id, user_id, NOW() FROM custom_map_aux_specs WHERE id = $2
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
    Ok(row.map(|r| r.spec_id))
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
    use std::{
        sync::{Arc, Mutex, OnceLock},
        time::Duration,
    };
    use tokio::sync::{Barrier, Notify};

    struct FakeAuthorVerifier {
        author: Uuid,
        barrier: Option<Arc<Barrier>>,
    }

    impl MapAuthorVerifier for FakeAuthorVerifier {
        fn author_id_for_uid<'a>(
            &'a self,
            _map_uid: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Uuid, String>> + Send + 'a>> {
            Box::pin(async move {
                if let Some(barrier) = &self.barrier {
                    barrier.wait().await;
                }
                Ok(self.author)
            })
        }
    }

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
        let _ = query!("DELETE FROM minigame_on_map WHERE map_uid = $1", map_uid)
            .execute(pool)
            .await;
        let _ = query!("DELETE FROM collectible_on_map WHERE map_uid = $1", map_uid)
            .execute(pool)
            .await;
        let _ = query!("DELETE FROM hat_on_map WHERE map_uid = $1", map_uid).execute(pool).await;
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
    async fn authoritative_bind_rejects_two_user_spoofing() {
        let _guard = test_lock();
        let pool = test_pool().await;
        let owner = seed_user(&pool, "bind-real-author").await;
        let attacker = seed_user(&pool, "bind-attacker").await;
        let map_uid = unique_map_uid("Spoof_");
        let owner_spec = upsert_spec(&pool, &owner, "owner", &json!({"listingThreshold": 10}))
            .await
            .expect("owner spec");
        let attacker_spec = upsert_spec(&pool, &attacker, "attacker", &json!({"listingThreshold": 10}))
            .await
            .expect("attacker spec");
        let verifier = FakeAuthorVerifier {
            author: owner,
            barrier: None,
        };

        assert!(matches!(
            bind_authoritative_spec(&pool, &verifier, &attacker, &map_uid, owner_spec).await,
            Err(BindSpecError::SpecNotOwned)
        ));
        assert!(matches!(
            bind_authoritative_spec(&pool, &verifier, &attacker, &map_uid, attacker_spec).await,
            Err(BindSpecError::AuthorMismatch)
        ));
        assert_eq!(get_bound_spec_id(&pool, &map_uid).await.unwrap(), None);

        cleanup_map(&pool, &map_uid).await;
        cleanup_user(&pool, &attacker).await;
        cleanup_user(&pool, &owner).await;
    }

    #[tokio::test]
    async fn authoritative_binding_rejects_conflicting_rebind() {
        let _guard = test_lock();
        let pool = test_pool().await;
        let owner = seed_user(&pool, "bind-conflict-owner").await;
        let map_uid = unique_map_uid("Conflict");
        let first = upsert_spec(&pool, &owner, "first", &json!({"listingThreshold": 10}))
            .await
            .expect("first spec");
        let second = upsert_spec(&pool, &owner, "second", &json!({"listingThreshold": 20}))
            .await
            .expect("second spec");
        let verifier = FakeAuthorVerifier {
            author: owner,
            barrier: None,
        };

        assert_eq!(
            bind_authoritative_spec(&pool, &verifier, &owner, &map_uid, first)
                .await
                .expect("first bind"),
            first
        );
        assert!(matches!(
            bind_authoritative_spec(&pool, &verifier, &owner, &map_uid, second).await,
            Err(BindSpecError::ConflictingBinding)
        ));
        assert_eq!(resolve_bound_spec(&pool, &map_uid, first).await.unwrap(), Some(first));
        assert_eq!(resolve_bound_spec(&pool, &map_uid, second).await.unwrap(), None);

        cleanup_map(&pool, &map_uid).await;
        cleanup_user(&pool, &owner).await;
    }

    #[tokio::test]
    async fn authoritative_binding_prevents_spec_deletion_and_rebinding() {
        let _guard = test_lock();
        let pool = test_pool().await;
        let owner = seed_user(&pool, "bind-delete-owner").await;
        let map_uid = unique_map_uid("BindDel_");
        let spec_id = upsert_spec(&pool, &owner, "bound", &json!({"listingThreshold": 10}))
            .await
            .expect("spec");
        let verifier = FakeAuthorVerifier {
            author: owner,
            barrier: None,
        };
        bind_authoritative_spec(&pool, &verifier, &owner, &map_uid, spec_id)
            .await
            .expect("bind");

        let deletion = query!("DELETE FROM custom_map_aux_specs WHERE id = $1", spec_id)
            .execute(&pool)
            .await;
        assert!(deletion.is_err(), "the authoritative spec FK must restrict deletion");
        assert_eq!(resolve_bound_spec(&pool, &map_uid, spec_id).await.unwrap(), Some(spec_id));

        cleanup_map(&pool, &map_uid).await;
        cleanup_user(&pool, &owner).await;
    }

    #[tokio::test]
    async fn concurrent_authoritative_first_bind_has_exactly_one_winner() {
        let _guard = test_lock();
        let pool = test_pool().await;
        let owner = seed_user(&pool, "bind-race-owner").await;
        let map_uid = unique_map_uid("BindRace");
        let first = upsert_spec(&pool, &owner, "race-first", &json!({"listingThreshold": 10}))
            .await
            .expect("first spec");
        let second = upsert_spec(&pool, &owner, "race-second", &json!({"listingThreshold": 20}))
            .await
            .expect("second spec");
        let verifier = Arc::new(FakeAuthorVerifier {
            author: owner,
            barrier: Some(Arc::new(Barrier::new(2))),
        });

        let task = |spec_id| {
            let pool = pool.clone();
            let map_uid = map_uid.clone();
            let verifier = verifier.clone();
            tokio::spawn(async move { bind_authoritative_spec(&pool, verifier.as_ref(), &owner, &map_uid, spec_id).await })
        };
        let first_result = task(first);
        let second_result = task(second);
        let results = [first_result.await.unwrap(), second_result.await.unwrap()];
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(BindSpecError::ConflictingBinding)))
                .count(),
            1
        );
        let winner = get_bound_spec_id(&pool, &map_uid).await.unwrap().expect("winner");
        assert!(winner == first || winner == second);

        cleanup_map(&pool, &map_uid).await;
        cleanup_user(&pool, &owner).await;
    }

    #[tokio::test]
    async fn unbound_legacy_and_mismatched_reports_fail_closed() {
        let _guard = test_lock();
        let pool = test_pool().await;
        let owner = seed_user(&pool, "bind-legacy-owner").await;
        let map_uid = unique_map_uid("Legacy_");
        let spec_id = upsert_spec(&pool, &owner, "legacy", &json!({"listingThreshold": 10}))
            .await
            .expect("spec");
        assert_eq!(resolve_bound_spec(&pool, &map_uid, spec_id).await.unwrap(), None);
        query!("INSERT INTO map_listing_bind (map_uid, spec_id) VALUES ($1, $2)", map_uid, spec_id)
            .execute(&pool)
            .await
            .expect("legacy bind");
        assert_eq!(resolve_bound_spec(&pool, &map_uid, spec_id).await.unwrap(), None);

        let verifier = FakeAuthorVerifier {
            author: owner,
            barrier: None,
        };
        bind_authoritative_spec(&pool, &verifier, &owner, &map_uid, spec_id)
            .await
            .expect("promote legacy bind");
        assert_eq!(resolve_bound_spec(&pool, &map_uid, spec_id).await.unwrap(), Some(spec_id));
        assert_eq!(resolve_bound_spec(&pool, &map_uid, spec_id + 1).await.unwrap(), None);

        cleanup_map(&pool, &map_uid).await;
        cleanup_user(&pool, &owner).await;
    }

    #[tokio::test]
    async fn authoritative_catalog_replaces_stale_on_map_rows() {
        let _guard = test_lock();
        let pool = test_pool().await;
        let owner = seed_user(&pool, "bind-catalog-owner").await;
        let map_uid = unique_map_uid("Catalog_");
        let authoritative_json = json!({"minigames":{"games":[{"slug":"race","name":"Authoritative","kind":"Sprint"}]}});
        let stale_json = json!({"minigames":{"games":[{"slug":"race","name":"Spoofed","kind":"Survival"},{"slug":"extra","name":"Extra","kind":"Darts"}]}});
        let authoritative = upsert_spec(&pool, &owner, "catalog-a", &authoritative_json)
            .await
            .expect("authoritative spec");
        let stale = upsert_spec(&pool, &owner, "catalog-b", &stale_json).await.expect("stale spec");
        crate::queries::minigames::replace_catalog(&pool, authoritative, &owner, "catalog-a", &authoritative_json)
            .await
            .expect("authoritative catalog");
        crate::queries::minigames::replace_catalog(&pool, stale, &owner, "catalog-b", &stale_json)
            .await
            .expect("stale catalog");
        crate::queries::minigames::bind_map_from_catalog(&pool, &map_uid, stale)
            .await
            .expect("seed stale rows");

        let verifier = FakeAuthorVerifier {
            author: owner,
            barrier: None,
        };
        bind_authoritative_spec(&pool, &verifier, &owner, &map_uid, authoritative)
            .await
            .expect("authoritative bind");
        crate::queries::minigames::bind_map_from_catalog(&pool, &map_uid, authoritative)
            .await
            .expect("authoritative rows");
        assert!(matches!(
            bind_authoritative_spec(&pool, &verifier, &owner, &map_uid, stale).await,
            Err(BindSpecError::ConflictingBinding)
        ));
        let rows = query!(
            "SELECT slug, name, kind FROM minigame_on_map WHERE map_uid = $1 ORDER BY slug",
            map_uid
        )
        .fetch_all(&pool)
        .await
        .expect("on-map rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].slug, "race");
        assert_eq!(rows[0].name, "Authoritative");
        assert_eq!(rows[0].kind, "Sprint");

        cleanup_map(&pool, &map_uid).await;
        cleanup_user(&pool, &owner).await;
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
    async fn concurrent_threshold_crossing_is_serialized_per_map() {
        let _guard = test_lock();
        let pool = test_pool().await;
        let author = seed_user(&pool, "listed-race-author").await;
        let map_uid = unique_map_uid("RaceMap");
        let spec_id = upsert_spec(&pool, &author, "race", &json!({"listingThreshold": 5}))
            .await
            .expect("spec");
        bind_first_spec(&pool, &map_uid, spec_id).await.expect("bind");

        let mut players = Vec::new();
        for i in 0..3 {
            let user = seed_user(&pool, &format!("listed-race-{i}")).await;
            insert_lb(&pool, &map_uid, &user, i as f64).await;
            players.push(user);
        }
        let fourth = seed_user(&pool, "listed-race-3").await;
        let fifth = seed_user(&pool, "listed-race-4").await;

        let mut first_tx = pool.begin().await.expect("first transaction");
        query!(
            "INSERT INTO map_leaderboard (map_uid, user_id, height, pos, race_time) VALUES ($1, $2, 3, $3, 1000)",
            map_uid,
            fourth,
            &[1.0_f64, 3.0, 2.0] as &[f64]
        )
        .execute(&mut *first_tx)
        .await
        .expect("fourth insert");

        let started = Arc::new(Notify::new());
        let finished = Arc::new(Notify::new());
        let second_pool = pool.clone();
        let second_uid = map_uid.clone();
        let second_started = started.clone();
        let second_finished = finished.clone();
        let second = tokio::spawn(async move {
            let mut tx = second_pool.begin().await.expect("second transaction");
            second_started.notify_one();
            query!(
                "INSERT INTO map_leaderboard (map_uid, user_id, height, pos, race_time) VALUES ($1, $2, 4, $3, 1000)",
                second_uid,
                fifth,
                &[1.0_f64, 4.0, 2.0] as &[f64]
            )
            .execute(&mut *tx)
            .await
            .expect("fifth insert");
            second_finished.notify_one();
            tx.commit().await.expect("second commit");
        });

        started.notified().await;
        // The fixed trigger waits on the first transaction's per-map advisory
        // lock. The old trigger finishes here, having counted only four rows.
        let second_finished_while_first_open = tokio::time::timeout(Duration::from_millis(500), finished.notified()).await.is_ok();
        first_tx.commit().await.expect("first commit");
        second.await.expect("second task");

        assert!(
            get_listed_map(&pool, &map_uid).await.expect("listed lookup").is_some(),
            "the fifth unique concurrent insert must list the map (second finished early: {second_finished_while_first_open})"
        );

        cleanup_map(&pool, &map_uid).await;
        for user in players.into_iter().chain([fourth, fifth, author]) {
            cleanup_user(&pool, &user).await;
        }
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
