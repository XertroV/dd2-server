use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{query, Pool, Postgres};
use thiserror::Error;
use uuid::Uuid;

use crate::consts::DD2_MAP_UID;
use crate::nadeo::MapAuthorVerifier;
use crate::queries::{collectibles, hats, minigames, CatalogValidationError};

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
    Catalog(#[from] CatalogValidationError),
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
        SELECT b.spec_id,
               (b.author_verified_at IS NOT NULL AND b.verified_author_id = s.user_id) AS "verified!"
        FROM map_listing_bind b
        INNER JOIN custom_map_aux_specs s ON s.id = b.spec_id
        WHERE b.map_uid = $1
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
#[derive(Debug)]
pub struct AuthoritativeBindResult {
    pub spec_id: i64,
    pub removed_hat_wearers: Vec<crate::router::HatOnMapPlayer>,
    pub materialized: bool,
}

pub async fn bind_authoritative_spec(
    pool: &Pool<Postgres>,
    verifier: &dyn MapAuthorVerifier,
    reporter_id: &Uuid,
    map_uid: &str,
    requested_spec_id: i64,
) -> Result<AuthoritativeBindResult, BindSpecError> {
    let existing_binding = get_spec_binding(pool, map_uid).await?;
    if let Some(binding) = &existing_binding {
        if binding.verified && binding.spec_id != requested_spec_id {
            return Err(BindSpecError::ConflictingBinding);
        }
    }

    let spec = query!("SELECT user_id FROM custom_map_aux_specs WHERE id = $1", requested_spec_id)
        .fetch_optional(pool)
        .await?
        .ok_or(BindSpecError::SpecNotFound)?;
    let needs_verification = !existing_binding.is_some_and(|binding| binding.verified);
    if needs_verification {
        if spec.user_id != *reporter_id {
            return Err(BindSpecError::SpecNotOwned);
        }
        let nadeo_author = verifier
            .author_id_for_uid(reporter_id, map_uid)
            .await
            .map_err(BindSpecError::AuthorVerification)?;
        if nadeo_author != spec.user_id {
            return Err(BindSpecError::AuthorMismatch);
        }
    }

    let mut tx = pool.begin().await?;
    let locked_spec = query!(
        r#"
        SELECT user_id, name_id, spec, updated_at, md5(spec::text) AS "spec_hash!"
        FROM custom_map_aux_specs
        WHERE id = $1
        FOR UPDATE
        "#,
        requested_spec_id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(BindSpecError::SpecNotFound)?;
    if needs_verification && locked_spec.user_id != *reporter_id {
        return Err(BindSpecError::SpecNotOwned);
    }
    let locked_binding = query!(
        r#"
        SELECT b.spec_id,
               (b.author_verified_at IS NOT NULL AND b.verified_author_id = s.user_id) AS "verified!",
               b.materialized_spec_updated_at, b.materialized_spec_hash
        FROM map_listing_bind b
        INNER JOIN custom_map_aux_specs s ON s.id = b.spec_id
        WHERE b.map_uid = $1
        FOR UPDATE OF b
        "#,
        map_uid
    )
    .fetch_optional(&mut *tx)
    .await?;
    let recovering_legacy = locked_binding.as_ref().is_some_and(|binding| !binding.verified);
    if let Some(binding) = &locked_binding {
        if binding.verified && binding.spec_id != requested_spec_id {
            return Err(BindSpecError::ConflictingBinding);
        }
        if binding.verified
            && binding.materialized_spec_updated_at == Some(locked_spec.updated_at)
            && binding.materialized_spec_hash.as_deref() == Some(locked_spec.spec_hash.as_str())
        {
            tx.commit().await?;
            return Ok(AuthoritativeBindResult {
                spec_id: binding.spec_id,
                removed_hat_wearers: vec![],
                materialized: false,
            });
        }
    }
    let games = minigames::parse_catalog_games(&locked_spec.spec)?;
    let collectible_rows = collectibles::parse_catalog_collectibles(&locked_spec.spec)?;
    let hat_rows = hats::parse_catalog_hats(&locked_spec.spec)?;
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
        UPDATE map_listing_bind b
        SET spec_id = $2,
            verified_author_id = $3,
            author_verified_at = NOW(),
            materialized_spec_updated_at = NULL,
            materialized_spec_hash = NULL
        WHERE b.map_uid = $1
          AND (
              b.author_verified_at IS NULL
              OR b.verified_author_id IS DISTINCT FROM (
                  SELECT s.user_id FROM custom_map_aux_specs s WHERE s.id = b.spec_id
              )
          )
        "#,
        map_uid,
        requested_spec_id,
        reporter_id
    )
    .execute(&mut *tx)
    .await?;
    let winner = query!(
        r#"
        SELECT b.spec_id,
               (b.author_verified_at IS NOT NULL AND b.verified_author_id = s.user_id) AS "verified!"
        FROM map_listing_bind b
        INNER JOIN custom_map_aux_specs s ON s.id = b.spec_id
        WHERE b.map_uid = $1
        FOR UPDATE OF b
        "#,
        map_uid
    )
    .fetch_optional(&mut *tx)
    .await?;

    let Some(winner) = winner else {
        return Err(BindSpecError::SpecNotFound);
    };
    if winner.spec_id != requested_spec_id {
        return Err(BindSpecError::ConflictingBinding);
    }
    if !winner.verified {
        return Err(BindSpecError::AuthorVerification("binding was not durably verified".to_string()));
    }
    let mut removed_hat_wearers = if recovering_legacy {
        hats::purge_map_equipment_in_tx(&mut tx, map_uid).await?
    } else {
        Vec::new()
    };
    minigames::replace_catalog_in_tx(&mut tx, requested_spec_id, &locked_spec.user_id, &locked_spec.name_id, &games).await?;
    collectibles::replace_catalog_in_tx(
        &mut tx,
        requested_spec_id,
        &locked_spec.user_id,
        &locked_spec.name_id,
        &collectible_rows,
    )
    .await?;
    hats::replace_catalog_in_tx(&mut tx, requested_spec_id, &locked_spec.user_id, &locked_spec.name_id, &hat_rows).await?;
    minigames::bind_map_from_catalog_in_tx(&mut tx, map_uid, requested_spec_id).await?;
    collectibles::bind_map_from_catalog_in_tx(&mut tx, map_uid, requested_spec_id).await?;
    let hat_bind = hats::bind_map_from_catalog_in_tx(&mut tx, map_uid, requested_spec_id).await?;
    removed_hat_wearers.extend(hat_bind.removed_wearers);
    query!(
        r#"
        UPDATE map_listing_bind
        SET materialized_spec_updated_at = $2,
            materialized_spec_hash = $3
        WHERE map_uid = $1 AND spec_id = $4
        "#,
        map_uid,
        locked_spec.updated_at,
        locked_spec.spec_hash,
        requested_spec_id
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(AuthoritativeBindResult {
        spec_id: winner.spec_id,
        removed_hat_wearers,
        materialized: true,
    })
}

/// Resolve an already verified binding, treating the client spec id only as a
/// consistency hint. Unbound, legacy-unverified, and mismatched reports fail
/// closed without consulting client-selected catalog rows.
pub async fn resolve_bound_spec(pool: &Pool<Postgres>, map_uid: &str, client_spec_id: i64) -> Result<Option<i64>, sqlx::Error> {
    let row = query!(
        r#"
        SELECT b.spec_id
        FROM map_listing_bind b
        INNER JOIN custom_map_aux_specs s ON s.id = b.spec_id
        WHERE b.map_uid = $1
          AND b.spec_id = $2
          AND b.author_verified_at IS NOT NULL
          AND b.verified_author_id = s.user_id
          AND b.materialized_spec_updated_at = s.updated_at
          AND b.materialized_spec_hash = md5(s.spec::text)
        "#,
        map_uid,
        client_spec_id
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|row| row.spec_id))
}

/// Lock the source spec and its map binding in the same order as request 81,
/// then prove the map's materialized snapshot still matches the source. A
/// concurrent spec upload either completes first (and this fails closed) or
/// waits until the gameplay transaction has committed against the old, still
/// authoritative snapshot.
pub(crate) async fn lock_materialized_spec(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    map_uid: &str,
    spec_id: i64,
) -> Result<bool, sqlx::Error> {
    let source = query!(
        r#"
        SELECT user_id, updated_at, md5(spec::text) AS "spec_hash!"
        FROM custom_map_aux_specs
        WHERE id = $1
        FOR SHARE
        "#,
        spec_id
    )
    .fetch_optional(&mut **tx)
    .await?;
    let Some(source) = source else {
        return Ok(false);
    };
    let binding = query!(
        r#"
        SELECT verified_author_id, materialized_spec_updated_at, materialized_spec_hash
        FROM map_listing_bind
        WHERE map_uid = $1
          AND spec_id = $2
          AND author_verified_at IS NOT NULL
        FOR SHARE
        "#,
        map_uid,
        spec_id
    )
    .fetch_optional(&mut **tx)
    .await?;
    Ok(binding.is_some_and(|binding| {
        binding.verified_author_id == Some(source.user_id)
            && binding.materialized_spec_updated_at == Some(source.updated_at)
            && binding.materialized_spec_hash.as_deref() == Some(source.spec_hash.as_str())
    }))
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
    use crate::queries::custom_map_aux_specs::{listing_threshold_from_spec, upsert_spec, upsert_spec_and_catalogs};
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

    struct AlwaysRecordVerifier;

    impl crate::nadeo::OfficialRecordVerifier for AlwaysRecordVerifier {
        fn has_official_record<'a>(
            &'a self,
            _account_id: &'a Uuid,
            _map_uid: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, String>> + Send + 'a>> {
            Box::pin(async { Ok(true) })
        }
    }

    impl MapAuthorVerifier for FakeAuthorVerifier {
        fn author_id_for_uid<'a>(
            &'a self,
            _account_id: &'a Uuid,
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
            .max_connections(8)
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
                .expect("first bind")
                .spec_id,
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
    async fn authoritative_owner_replaces_wrong_legacy_spec_and_purges_equipment() {
        let _guard = test_lock();
        let pool = test_pool().await;
        let owner = seed_user(&pool, "bind-recovery-owner").await;
        let legacy_owner = seed_user(&pool, "bind-recovery-legacy").await;
        let map_uid = unique_map_uid("Recover_");
        let legacy_spec = upsert_spec_and_catalogs(
            &pool,
            &legacy_owner,
            "legacy-wrong",
            &json!({"hats":[{"slug":"Hat-1","unlock":{"finishThisMap":true}}]}),
        )
        .await
        .expect("legacy spec");
        let owner_spec = upsert_spec_and_catalogs(
            &pool,
            &owner,
            "authoritative",
            &json!({"hats":[{"slug":"Hat-1","unlock":{"finishThisMap":true}}]}),
        )
        .await
        .expect("owner spec");
        hats::bind_map_from_catalog(&pool, &map_uid, legacy_spec)
            .await
            .expect("legacy materialization");
        query!(
            "INSERT INTO map_listing_bind (map_uid, spec_id) VALUES ($1, $2)",
            map_uid,
            legacy_spec
        )
        .execute(&pool)
        .await
        .expect("legacy binding");
        query!(
            r#"
            INSERT INTO hat_unlock (user_id, hat_key, map_uid, slug)
            VALUES ($1, substring(md5($2 || ':h:Hat-1') from 1 for 16), $2, 'Hat-1')
            "#,
            owner,
            map_uid
        )
        .execute(&pool)
        .await
        .expect("legacy unlock");
        query!(
            r#"
            INSERT INTO hat_equipped (user_id, map_uid, slug, hat_key)
            VALUES ($1, $2, 'Hat-1', substring(md5($3 || ':h:Hat-1') from 1 for 16))
            "#,
            owner,
            map_uid,
            map_uid
        )
        .execute(&pool)
        .await
        .expect("legacy equipment");

        assert_eq!(resolve_bound_spec(&pool, &map_uid, legacy_spec).await.unwrap(), None);
        let verifier = FakeAuthorVerifier {
            author: owner,
            barrier: None,
        };
        let result = bind_authoritative_spec(&pool, &verifier, &owner, &map_uid, owner_spec)
            .await
            .expect("authoritative recovery");
        assert_eq!(result.spec_id, owner_spec);
        assert_eq!(result.removed_hat_wearers.len(), 1);
        assert_eq!(result.removed_hat_wearers[0].wsid, owner.to_string());
        assert!(result.removed_hat_wearers[0].slug.is_empty());
        assert_eq!(resolve_bound_spec(&pool, &map_uid, legacy_spec).await.unwrap(), None);
        assert_eq!(resolve_bound_spec(&pool, &map_uid, owner_spec).await.unwrap(), Some(owner_spec));
        let binding = query!(
            r#"
            SELECT b.spec_id, b.verified_author_id,
                   b.materialized_spec_updated_at = s.updated_at AS "revision_matches!",
                   b.materialized_spec_hash = md5(s.spec::text) AS "hash_matches!"
            FROM map_listing_bind b
            JOIN custom_map_aux_specs s ON s.id = b.spec_id
            WHERE b.map_uid = $1
            "#,
            map_uid
        )
        .fetch_one(&pool)
        .await
        .expect("recovered binding");
        assert_eq!(binding.spec_id, owner_spec);
        assert_eq!(binding.verified_author_id, Some(owner));
        assert!(binding.revision_matches);
        assert!(binding.hash_matches);
        assert_eq!(
            query!("SELECT COUNT(*) AS count FROM hat_equipped WHERE map_uid = $1", map_uid)
                .fetch_one(&pool)
                .await
                .expect("equipment count")
                .count
                .unwrap_or(0),
            0,
            "same-slug legacy equipment must not survive recovery"
        );

        let _ = query!("DELETE FROM hat_unlock WHERE user_id = $1 AND map_uid = $2", owner, map_uid)
            .execute(&pool)
            .await;
        cleanup_map(&pool, &map_uid).await;
        cleanup_user(&pool, &legacy_owner).await;
        cleanup_user(&pool, &owner).await;
    }

    #[tokio::test]
    async fn concurrent_legacy_takeover_has_exactly_one_authoritative_winner() {
        let _guard = test_lock();
        let pool = test_pool().await;
        let owner = seed_user(&pool, "bind-takeover-owner").await;
        let legacy_owner = seed_user(&pool, "bind-takeover-legacy").await;
        let map_uid = unique_map_uid("Takeover");
        let legacy_spec = upsert_spec(&pool, &legacy_owner, "legacy", &json!({"listingThreshold": 25}))
            .await
            .expect("legacy spec");
        let first = upsert_spec(&pool, &owner, "takeover-first", &json!({"listingThreshold": 10}))
            .await
            .expect("first candidate");
        let second = upsert_spec(&pool, &owner, "takeover-second", &json!({"listingThreshold": 20}))
            .await
            .expect("second candidate");
        query!(
            "INSERT INTO map_listing_bind (map_uid, spec_id) VALUES ($1, $2)",
            map_uid,
            legacy_spec
        )
        .execute(&pool)
        .await
        .expect("legacy binding");
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
        assert_eq!(resolve_bound_spec(&pool, &map_uid, winner).await.unwrap(), Some(winner));

        cleanup_map(&pool, &map_uid).await;
        cleanup_user(&pool, &legacy_owner).await;
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
    async fn synchronized_bind_is_write_free_and_changed_revision_materializes_once() {
        let _guard = test_lock();
        let pool = test_pool().await;
        let owner = seed_user(&pool, "bind-revision-owner").await;
        let map_uid = unique_map_uid("Revision");
        let name_id = format!("revision-{}", &Uuid::new_v4().simple().to_string()[..8]);
        let initial = json!({
            "minigames":{"games":[{"slug":"old-game","kind":"Sprint"}]},
            "collectibles":[{"slug":"old-collectible","pickup":"grant"}],
            "hats":[{"slug":"old-hat","unlock":{"finishThisMap":true}}]
        });
        let spec_id = upsert_spec_and_catalogs(&pool, &owner, &name_id, &initial)
            .await
            .expect("initial spec");
        let verifier = FakeAuthorVerifier {
            author: owner,
            barrier: None,
        };
        let first = bind_authoritative_spec(&pool, &verifier, &owner, &map_uid, spec_id)
            .await
            .expect("initial materialization");
        assert!(first.materialized);

        let before_binding = query!(
            r#"SELECT xmin::text AS "xmin!", materialized_spec_hash AS "hash!"
               FROM map_listing_bind WHERE map_uid = $1"#,
            map_uid
        )
        .fetch_one(&pool)
        .await
        .expect("binding version");
        let before_catalog = query!(
            r#"SELECT xmin::text AS "xmin!" FROM minigame_catalog
               WHERE spec_id = $1 AND slug = 'old-game'"#,
            spec_id
        )
        .fetch_one(&pool)
        .await
        .expect("catalog version");
        let before_on_map = query!(
            r#"SELECT xmin::text AS "xmin!" FROM minigame_on_map
               WHERE map_uid = $1 AND slug = 'old-game'"#,
            map_uid
        )
        .fetch_one(&pool)
        .await
        .expect("on-map version");

        let repeat = bind_authoritative_spec(&pool, &verifier, &owner, &map_uid, spec_id)
            .await
            .expect("idempotent repeat");
        assert!(!repeat.materialized);
        assert!(repeat.removed_hat_wearers.is_empty());
        let after_binding = query!(
            r#"SELECT xmin::text AS "xmin!", materialized_spec_hash AS "hash!"
               FROM map_listing_bind WHERE map_uid = $1"#,
            map_uid
        )
        .fetch_one(&pool)
        .await
        .expect("unchanged binding version");
        let after_catalog = query!(
            r#"SELECT xmin::text AS "xmin!" FROM minigame_catalog
               WHERE spec_id = $1 AND slug = 'old-game'"#,
            spec_id
        )
        .fetch_one(&pool)
        .await
        .expect("unchanged catalog version");
        let after_on_map = query!(
            r#"SELECT xmin::text AS "xmin!" FROM minigame_on_map
               WHERE map_uid = $1 AND slug = 'old-game'"#,
            map_uid
        )
        .fetch_one(&pool)
        .await
        .expect("unchanged on-map version");
        assert_eq!(after_binding.xmin, before_binding.xmin);
        assert_eq!(after_binding.hash, before_binding.hash);
        assert_eq!(after_catalog.xmin, before_catalog.xmin);
        assert_eq!(after_on_map.xmin, before_on_map.xmin);

        query!(
            r#"
            INSERT INTO hat_unlock (user_id, hat_key, map_uid, slug, unlocked_at)
            VALUES ($1, substring(md5($2 || ':h:old-hat') from 1 for 16), $2, 'old-hat', NOW())
            "#,
            owner,
            map_uid
        )
        .execute(&pool)
        .await
        .expect("seed old hat unlock");
        query!(
            r#"
            INSERT INTO hat_equipped (user_id, map_uid, slug, hat_key, updated_at)
            VALUES ($1, $2, 'old-hat', substring(md5($3 || ':h:old-hat') from 1 for 16), NOW())
            "#,
            owner,
            map_uid,
            map_uid
        )
        .execute(&pool)
        .await
        .expect("seed old equipped hat");

        let changed = json!({
            "minigames":{"games":[{"slug":"new-game","kind":"Darts"}]},
            "collectibles":[{"slug":"new-collectible","pickup":"grant"}],
            "hats":[{"slug":"new-hat","unlock":{"finishThisMap":true}}]
        });
        assert_eq!(
            upsert_spec_and_catalogs(&pool, &owner, &name_id, &changed)
                .await
                .expect("changed revision"),
            spec_id
        );
        let verifier = Arc::new(verifier);
        let bind_changed_revision = || {
            let pool = pool.clone();
            let verifier = verifier.clone();
            let map_uid = map_uid.clone();
            tokio::spawn(async move { bind_authoritative_spec(&pool, verifier.as_ref(), &owner, &map_uid, spec_id).await })
        };
        let first_rebuild = bind_changed_revision();
        let second_rebuild = bind_changed_revision();
        let rebuilds = [
            first_rebuild.await.expect("first rebuild task").expect("first changed bind"),
            second_rebuild.await.expect("second rebuild task").expect("second changed bind"),
        ];
        assert_eq!(rebuilds.iter().filter(|result| result.materialized).count(), 1);
        let removed_wearers: Vec<_> = rebuilds.iter().flat_map(|result| result.removed_hat_wearers.iter()).collect();
        assert_eq!(removed_wearers.len(), 1);
        assert_eq!(removed_wearers[0].wsid, owner.to_string());
        assert!(removed_wearers[0].slug.is_empty());
        let current = query!("SELECT slug FROM minigame_on_map WHERE map_uid = $1", map_uid)
            .fetch_one(&pool)
            .await
            .expect("new materialization");
        assert_eq!(current.slug, "new-game");

        cleanup_map(&pool, &map_uid).await;
        cleanup_user(&pool, &owner).await;
    }

    #[tokio::test]
    async fn concurrent_source_update_blocks_all_gameplay_until_request_81_materializes() {
        let _guard = test_lock();
        let pool = test_pool().await;
        let owner = seed_user(&pool, "snapshot-gameplay-owner").await;
        let map_uid = unique_map_uid("Snapshot");
        let name_id = format!("snapshot-{}", &Uuid::new_v4().simple().to_string()[..8]);
        let old_spec = json!({
            "minigames":{"games":[{"slug":"old-game","kind":"Sprint"}]},
            "collectibles":[
                {"slug":"old-unlock","pickup":"zone"},
                {"slug":"old-collect","pickup":"grant"}
            ],
            "hats":[{"slug":"old-hat","unlock":{"finishThisMap":true}}]
        });
        let spec_id = upsert_spec_and_catalogs(&pool, &owner, &name_id, &old_spec)
            .await
            .expect("seed source snapshot");
        let author_verifier = FakeAuthorVerifier {
            author: owner,
            barrier: None,
        };
        bind_authoritative_spec(&pool, &author_verifier, &owner, &map_uid, spec_id)
            .await
            .expect("materialize old snapshot");

        let new_spec = json!({
            "minigames":{"games":[{"slug":"new-game","kind":"Darts"}]},
            "collectibles":[
                {"slug":"new-unlock","pickup":"zone"},
                {"slug":"new-collect","pickup":"grant"}
            ],
            "hats":[{"slug":"new-hat","unlock":{"finishThisMap":true}}]
        });
        let games = minigames::parse_catalog_games(&new_spec).expect("parse new games");
        let collectible_rows = collectibles::parse_catalog_collectibles(&new_spec).expect("parse new collectibles");
        let hat_rows = hats::parse_catalog_hats(&new_spec).expect("parse new hats");
        let mut update_tx = pool.begin().await.expect("begin concurrent source update");
        query!(
            "UPDATE custom_map_aux_specs SET spec = $2, updated_at = updated_at + INTERVAL '1 second' WHERE id = $1",
            spec_id,
            new_spec
        )
        .execute(&mut *update_tx)
        .await
        .expect("update source revision");
        minigames::replace_catalog_in_tx(&mut update_tx, spec_id, &owner, &name_id, &games)
            .await
            .expect("replace global games");
        collectibles::replace_catalog_in_tx(&mut update_tx, spec_id, &owner, &name_id, &collectible_rows)
            .await
            .expect("replace global collectibles");
        hats::replace_catalog_in_tx(&mut update_tx, spec_id, &owner, &name_id, &hat_rows)
            .await
            .expect("replace global hats");

        let record_verifier = Arc::new(AlwaysRecordVerifier);
        let game_task = {
            let pool = pool.clone();
            let map_uid = map_uid.clone();
            tokio::spawn(async move { minigames::report_attempt(&pool, &owner, &map_uid, "old-game", spec_id, 123, "finish").await })
        };
        let unlock_task = {
            let pool = pool.clone();
            let map_uid = map_uid.clone();
            tokio::spawn(async move { collectibles::report_unlock(&pool, &owner, &map_uid, "old-unlock", spec_id).await })
        };
        let collect_task = {
            let pool = pool.clone();
            let map_uid = map_uid.clone();
            tokio::spawn(async move {
                collectibles::report_collect(&pool, &owner, &map_uid, "old-collect", spec_id, 1.0, 2.0, 3.0, "grant").await
            })
        };
        let hat_task = {
            let pool = pool.clone();
            let map_uid = map_uid.clone();
            let record_verifier = record_verifier.clone();
            tokio::spawn(async move { hats::report_unlock(&pool, record_verifier.as_ref(), &owner, &map_uid, "old-hat", spec_id).await })
        };

        tokio::time::sleep(Duration::from_millis(100)).await;
        update_tx.commit().await.expect("publish new source and global catalogs");
        game_task.await.expect("gameplay task").expect("gameplay query");
        unlock_task.await.expect("unlock task").expect("unlock query");
        collect_task.await.expect("collect task").expect("collect query");
        assert!(!hat_task.await.expect("hat task").expect("hat query"));

        assert_eq!(
            query!("SELECT COUNT(*) AS count FROM minigame_attempt WHERE user_id = $1", owner)
                .fetch_one(&pool)
                .await
                .expect("attempt count")
                .count
                .unwrap_or(0),
            0
        );
        assert_eq!(
            query!("SELECT COUNT(*) AS count FROM minigame_pb WHERE user_id = $1", owner)
                .fetch_one(&pool)
                .await
                .expect("pb count")
                .count
                .unwrap_or(0),
            0
        );
        assert_eq!(
            query!("SELECT COUNT(*) AS count FROM collectible_progress WHERE user_id = $1", owner)
                .fetch_one(&pool)
                .await
                .expect("progress count")
                .count
                .unwrap_or(0),
            0
        );
        assert_eq!(
            query!("SELECT COUNT(*) AS count FROM hat_unlock WHERE user_id = $1", owner)
                .fetch_one(&pool)
                .await
                .expect("hat count")
                .count
                .unwrap_or(0),
            0
        );
        assert_eq!(
            query!("SELECT slug FROM minigame_on_map WHERE map_uid = $1", map_uid)
                .fetch_one(&pool)
                .await
                .expect("old on-map snapshot")
                .slug,
            "old-game"
        );
        assert_eq!(
            query!("SELECT slug FROM minigame_catalog WHERE spec_id = $1", spec_id)
                .fetch_one(&pool)
                .await
                .expect("new global catalog")
                .slug,
            "new-game"
        );

        let rebuilt = bind_authoritative_spec(&pool, &author_verifier, &owner, &map_uid, spec_id)
            .await
            .expect("request 81 rebuild");
        assert!(rebuilt.materialized);
        minigames::report_attempt(&pool, &owner, &map_uid, "new-game", spec_id, 456, "finish")
            .await
            .expect("new game report");
        collectibles::report_unlock(&pool, &owner, &map_uid, "new-unlock", spec_id)
            .await
            .expect("new unlock report");
        collectibles::report_collect(&pool, &owner, &map_uid, "new-collect", spec_id, 4.0, 5.0, 6.0, "grant")
            .await
            .expect("new collect report");
        assert!(
            hats::report_unlock(&pool, record_verifier.as_ref(), &owner, &map_uid, "new-hat", spec_id)
                .await
                .expect("new hat report")
        );
        assert_eq!(
            query!("SELECT COUNT(*) AS count FROM minigame_pb WHERE user_id = $1", owner)
                .fetch_one(&pool)
                .await
                .expect("materialized pb count")
                .count
                .unwrap_or(0),
            1
        );
        assert_eq!(
            query!("SELECT COUNT(*) AS count FROM collectible_progress WHERE user_id = $1", owner)
                .fetch_one(&pool)
                .await
                .expect("materialized progress count")
                .count
                .unwrap_or(0),
            2
        );
        assert_eq!(
            query!("SELECT COUNT(*) AS count FROM hat_unlock WHERE user_id = $1", owner)
                .fetch_one(&pool)
                .await
                .expect("materialized hat count")
                .count
                .unwrap_or(0),
            1
        );

        cleanup_map(&pool, &map_uid).await;
        cleanup_user(&pool, &owner).await;
    }

    #[tokio::test]
    async fn authoritative_rebuild_rolls_back_catalogs_on_map_rows_and_hat_cleanup_together() {
        let _guard = test_lock();
        let pool = test_pool().await;
        let owner = seed_user(&pool, "bind-atomic-owner").await;
        let map_uid = unique_map_uid("Atomic_");
        let name_id = format!("atomic-{}", &Uuid::new_v4().simple().to_string()[..8]);
        let old_spec = json!({
            "listingThreshold": 11,
            "minigames":{"games":[{"slug":"old-game","kind":"Sprint"}]},
            "collectibles":[{"slug":"old-collectible","pickup":"grant"}],
            "hats":[{"slug":"old-hat","unlock":{"finishThisMap":true}}]
        });
        let spec_id = upsert_spec_and_catalogs(&pool, &owner, &name_id, &old_spec)
            .await
            .expect("seed spec and catalogs");
        let verifier = FakeAuthorVerifier {
            author: owner,
            barrier: None,
        };
        bind_authoritative_spec(&pool, &verifier, &owner, &map_uid, spec_id)
            .await
            .expect("initial bind");
        query!(
            r#"
            INSERT INTO hat_unlock (user_id, hat_key, map_uid, slug, unlocked_at)
            VALUES ($1, substring(md5($2 || ':h:old-hat') from 1 for 16), $2, 'old-hat', NOW())
            "#,
            owner,
            map_uid
        )
        .execute(&pool)
        .await
        .expect("seed unlock");
        query!(
            r#"
            INSERT INTO hat_equipped (user_id, map_uid, slug, hat_key, updated_at)
            VALUES ($1, $2, 'old-hat', substring(md5($3 || ':h:old-hat') from 1 for 16), NOW())
            "#,
            owner,
            map_uid,
            map_uid
        )
        .execute(&pool)
        .await
        .expect("seed equipment");

        let new_spec = json!({
            "listingThreshold": 77,
            "minigames":{"games":[{"slug":"new-game","kind":"Darts"}]},
            "collectibles":[{"slug":"new-collectible","pickup":"grant"}],
            "hats":[{"slug":"new-hat","unlock":{"finishThisMap":true}}]
        });
        assert_eq!(
            upsert_spec(&pool, &owner, &name_id, &new_spec).await.expect("update source spec"),
            spec_id
        );

        sqlx::query("DROP TRIGGER IF EXISTS dd2_test_fail_hat_on_map ON hat_on_map")
            .execute(&pool)
            .await
            .expect("drop stale trigger");
        sqlx::query("DROP FUNCTION IF EXISTS dd2_test_fail_hat_on_map()")
            .execute(&pool)
            .await
            .expect("drop stale trigger function");
        sqlx::query(
            "CREATE FUNCTION dd2_test_fail_hat_on_map() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'forced hat on map failure'; END $$",
        )
        .execute(&pool)
        .await
        .expect("create trigger function");
        sqlx::query(
            "CREATE TRIGGER dd2_test_fail_hat_on_map BEFORE INSERT ON hat_on_map FOR EACH ROW EXECUTE FUNCTION dd2_test_fail_hat_on_map()",
        )
        .execute(&pool)
        .await
        .expect("create trigger");

        let result = bind_authoritative_spec(&pool, &verifier, &owner, &map_uid, spec_id).await;

        sqlx::query("DROP TRIGGER dd2_test_fail_hat_on_map ON hat_on_map")
            .execute(&pool)
            .await
            .expect("drop trigger");
        sqlx::query("DROP FUNCTION dd2_test_fail_hat_on_map()")
            .execute(&pool)
            .await
            .expect("drop trigger function");
        assert!(matches!(result, Err(BindSpecError::Database(_))));

        let stored = query!("SELECT spec, listing_threshold FROM custom_map_aux_specs WHERE id = $1", spec_id)
            .fetch_one(&pool)
            .await
            .expect("source spec remains coherent");
        assert_eq!(stored.spec, new_spec);
        assert_eq!(stored.listing_threshold, 77);
        let game_catalog = query!("SELECT slug FROM minigame_catalog WHERE spec_id = $1", spec_id)
            .fetch_one(&pool)
            .await
            .expect("old game catalog retained");
        let collectible_catalog = query!("SELECT slug FROM collectible_catalog WHERE spec_id = $1", spec_id)
            .fetch_one(&pool)
            .await
            .expect("old collectible catalog retained");
        let hat_catalog = query!("SELECT slug FROM hat_catalog WHERE spec_id = $1", spec_id)
            .fetch_one(&pool)
            .await
            .expect("old hat catalog retained");
        assert_eq!(game_catalog.slug, "old-game");
        assert_eq!(collectible_catalog.slug, "old-collectible");
        assert_eq!(hat_catalog.slug, "old-hat");

        let game_on_map = query!("SELECT slug FROM minigame_on_map WHERE map_uid = $1", map_uid)
            .fetch_one(&pool)
            .await
            .expect("old game binding retained");
        let collectible_on_map = query!("SELECT slug FROM collectible_on_map WHERE map_uid = $1", map_uid)
            .fetch_one(&pool)
            .await
            .expect("old collectible binding retained");
        let hat_on_map = query!("SELECT slug FROM hat_on_map WHERE map_uid = $1", map_uid)
            .fetch_one(&pool)
            .await
            .expect("old hat binding retained");
        assert_eq!(game_on_map.slug, "old-game");
        assert_eq!(collectible_on_map.slug, "old-collectible");
        assert_eq!(hat_on_map.slug, "old-hat");
        assert_eq!(
            query!(
                "SELECT COUNT(*) AS count FROM hat_equipped WHERE user_id = $1 AND map_uid = $2",
                owner,
                map_uid
            )
            .fetch_one(&pool)
            .await
            .expect("equipment retained")
            .count
            .unwrap_or(0),
            1
        );

        let _ = query!("DELETE FROM hat_equipped WHERE user_id = $1 AND map_uid = $2", owner, map_uid)
            .execute(&pool)
            .await;
        let _ = query!("DELETE FROM hat_unlock WHERE user_id = $1 AND map_uid = $2", owner, map_uid)
            .execute(&pool)
            .await;
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
