use serde_json::Value;
use sqlx::{Pool, Postgres, Transaction};
use uuid::Uuid;

use super::CatalogValidationError;
use crate::router::{MinigameLBEntry, MinigameListGame};

pub struct CatalogGame {
    pub slug: String,
    pub kind: String,
    pub name: String,
    pub lower_is_better: bool,
}

/// Prefer string `kind`, else int `type`. Unknown kinds are skipped.
pub fn map_kind_from_game(game: &Value) -> Option<(&'static str, bool)> {
    if let Some(k) = game.get("kind").and_then(|v| v.as_str()) {
        let k = k.trim();
        if !k.is_empty() {
            return map_kind_str(k);
        }
    }
    let t = game.get("type").and_then(|v| {
        v.as_i64()
            .or_else(|| v.as_u64().map(|u| u as i64))
            .or_else(|| v.as_f64().map(|f| f as i64))
    })?;
    map_kind_int(t)
}

fn map_kind_str(k: &str) -> Option<(&'static str, bool)> {
    match k {
        "Sprint" | "TimeTrial" | "PuzzleLocations" => Some(("Sprint", true)),
        "Survival" | "TimeOffGround" | "KingOfTheHill" => Some(("Survival", false)),
        "Darts" | "JumpAccuracy" => Some(("Darts", false)),
        "HighJump" | "JumpHigh" => Some(("HighJump", false)),
        "LongJump" => Some(("LongJump", false)),
        "MaxAvgSpeed" => Some(("MaxAvgSpeed", false)),
        _ => None,
    }
}

fn map_kind_int(t: i64) -> Option<(&'static str, bool)> {
    match t {
        1 => Some(("Sprint", true)),
        2 => Some(("HighJump", false)),
        3 => Some(("Darts", false)),
        4 => Some(("MaxAvgSpeed", false)),
        5 => Some(("Survival", false)),
        6 => Some(("Sprint", true)),
        _ => None,
    }
}

/// Replace catalog for `(user_id, name_id)` from `spec.minigames.games[]`.
/// Missing/empty games → delete prior rows.
pub fn parse_catalog_games(spec: &Value) -> Result<Vec<CatalogGame>, CatalogValidationError> {
    let Some(games) = spec.get("minigames").and_then(|m| m.get("games")).and_then(|g| g.as_array()) else {
        return Ok(vec![]);
    };
    let mut parsed = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut kind_counts = std::collections::HashMap::<&'static str, u32>::new();
    for g in games {
        if !g.is_object() {
            continue;
        }
        let Some((kind, lower_is_better)) = map_kind_from_game(g) else {
            continue;
        };
        let next = kind_counts.get(kind).copied().unwrap_or(0) + 1;
        let slug = match g.get("slug") {
            None => format!("{kind}-{next}"),
            Some(value) => {
                let Some(slug) = value.as_str() else {
                    return Err(CatalogValidationError::BlankSlug { catalog: "minigame" });
                };
                let slug = slug.trim();
                if slug.is_empty() {
                    return Err(CatalogValidationError::BlankSlug { catalog: "minigame" });
                }
                slug.to_string()
            }
        };
        if !seen.insert(slug.clone()) {
            return Err(CatalogValidationError::DuplicateSlug { catalog: "minigame", slug });
        }
        kind_counts.insert(kind, next);
        let name = g.get("name").and_then(|s| s.as_str()).unwrap_or("").to_string();
        parsed.push(CatalogGame {
            slug,
            kind: kind.to_string(),
            name,
            lower_is_better,
        });
    }
    Ok(parsed)
}

/// Replace catalog for a spec row from `spec.minigames.games[]`.
/// Missing/empty games → delete prior rows for this spec_id.
#[cfg(test)]
pub async fn replace_catalog(pool: &Pool<Postgres>, spec_id: i64, user_id: &Uuid, name_id: &str, spec: &Value) -> Result<(), sqlx::Error> {
    let games = parse_catalog_games(spec).map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
    let mut tx = pool.begin().await?;
    replace_catalog_in_tx(&mut tx, spec_id, user_id, name_id, &games).await?;
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn replace_catalog_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    spec_id: i64,
    user_id: &Uuid,
    name_id: &str,
    games: &[CatalogGame],
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"--sql
        DELETE FROM minigame_catalog WHERE spec_id = $1
        "#,
        spec_id
    )
    .execute(&mut **tx)
    .await?;
    for g in games {
        sqlx::query!(
            r#"--sql
            INSERT INTO minigame_catalog (spec_id, user_id, name_id, slug, kind, name, lower_is_better)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            "#,
            spec_id,
            user_id,
            name_id,
            &g.slug,
            &g.kind,
            &g.name,
            g.lower_is_better
        )
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

pub async fn delete_catalog(pool: &Pool<Postgres>, user_id: &Uuid, name_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"--sql
        DELETE FROM minigame_catalog WHERE user_id = $1 AND name_id = $2
        "#,
        user_id,
        name_id
    )
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
async fn ensure_catalog(pool: &Pool<Postgres>, spec_id: i64) -> Result<(), sqlx::Error> {
    let existing = sqlx::query!(
        r#"--sql
        SELECT 1 AS ok FROM minigame_catalog WHERE spec_id = $1 LIMIT 1
        "#,
        spec_id
    )
    .fetch_optional(pool)
    .await?;
    if existing.is_some() {
        return Ok(());
    }
    let Some(row) = crate::queries::custom_map_aux_specs::spec_source(pool, spec_id).await? else {
        return Ok(());
    };
    replace_catalog(pool, spec_id, &row.user_id, &row.name_id, &row.spec).await
}

/// Copy all catalog rows for a spec onto `minigame_on_map` for this uid.
/// Returns whether any rows were bound (unknown spec → false).
#[cfg(test)]
pub async fn bind_map_from_catalog(pool: &Pool<Postgres>, map_uid: &str, spec_id: i64) -> Result<bool, sqlx::Error> {
    ensure_catalog(pool, spec_id).await?;
    let mut tx = pool.begin().await?;
    let bound = bind_map_from_catalog_in_tx(&mut tx, map_uid, spec_id).await?;
    tx.commit().await?;
    Ok(bound)
}

pub(crate) async fn bind_map_from_catalog_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    map_uid: &str,
    spec_id: i64,
) -> Result<bool, sqlx::Error> {
    // The delete makes a (map_uid, slug) conflict impossible below, so the
    // insert deliberately carries no ON CONFLICT clause.
    sqlx::query!("DELETE FROM minigame_on_map WHERE map_uid = $1", map_uid)
        .execute(&mut **tx)
        .await?;
    let r = sqlx::query!(
        r#"--sql
        INSERT INTO minigame_on_map (map_uid, slug, minigame_key, kind, name, spec_user, name_id)
        SELECT $1, c.slug, substring(md5($3 || ':' || c.slug) from 1 for 16), c.kind, c.name, c.user_id, c.name_id
        FROM minigame_catalog c
        WHERE c.spec_id = $2
        "#,
        map_uid,
        spec_id,
        map_uid
    )
    .execute(&mut **tx)
    .await?;
    Ok(r.rows_affected() > 0)
}

/// Accepted attempt: validate the request-81 snapshot, insert the attempt, and
/// bump attempt_count (placeholder PB at 0).
/// Finish only: upsert-if-better score (inverted comparator when lower_is_better) + finish_count.
pub async fn report_attempt(
    pool: &Pool<Postgres>,
    user_id: &Uuid,
    map_uid: &str,
    slug: &str,
    spec_id: i64,
    elapsed_ms: i64,
    status: &str,
) -> Result<(), sqlx::Error> {
    if status == "finish" && elapsed_ms < 0 {
        return Ok(());
    }

    let mut tx = pool.begin().await?;
    if !crate::queries::listed_maps::lock_materialized_spec(&mut tx, map_uid, spec_id).await? {
        return Ok(());
    }
    let game = sqlx::query!(
        r#"--sql
        SELECT minigame_key, kind = 'Sprint' AS "lower_is_better!"
        FROM minigame_on_map
        WHERE map_uid = $1 AND slug = $2
        FOR SHARE
        "#,
        map_uid,
        slug
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(game) = game else {
        return Ok(());
    };

    sqlx::query!(
        r#"--sql
        INSERT INTO minigame_attempt (user_id, minigame_key, map_uid, slug, elapsed_ms, status)
        VALUES ($1, $2, $3, $4, $5, $6)
        "#,
        user_id,
        game.minigame_key,
        map_uid,
        slug,
        elapsed_ms,
        status
    )
    .execute(&mut *tx)
    .await?;

    sqlx::query!(
        r#"--sql
        INSERT INTO minigame_pb (user_id, minigame_key, map_uid, slug, score, finish_count, attempt_count, last_played, updated_at)
        VALUES ($1, $2, $3, $4, 0, 0, 1, now(), now())
        ON CONFLICT (user_id, minigame_key) DO UPDATE SET
            attempt_count = minigame_pb.attempt_count + 1,
            last_played = now()
        "#,
        user_id,
        game.minigame_key,
        map_uid,
        slug
    )
    .execute(&mut *tx)
    .await?;

    if status == "finish" {
        // Upsert-if-better (map_leaderboard pattern). Invert comparator when lower_is_better.
        // Placeholder PB has finish_count = 0 / score 0; a worse finish only bumps finish_count.
        let update_r = sqlx::query!(
            r#"--sql
            UPDATE minigame_pb SET
                score = $1,
                finish_count = minigame_pb.finish_count + 1,
                updated_at = now()
            WHERE user_id = $2
              AND minigame_key = $3
              AND (
                minigame_pb.finish_count = 0
                OR ($4 AND minigame_pb.score > $1)
                OR (NOT $4 AND minigame_pb.score < $1)
              )
            RETURNING id
            "#,
            elapsed_ms,
            user_id,
            game.minigame_key,
            game.lower_is_better
        )
        .fetch_one(&mut *tx)
        .await;

        match update_r {
            Ok(_) => {}
            Err(sqlx::Error::RowNotFound) => {
                // Placeholder row already exists in this tx; score was not better.
                sqlx::query!(
                    r#"--sql
                    UPDATE minigame_pb SET finish_count = finish_count + 1
                    WHERE user_id = $1 AND minigame_key = $2
                    "#,
                    user_id,
                    game.minigame_key
                )
                .execute(&mut *tx)
                .await?;
            }
            Err(e) => return Err(e),
        }
    }

    tx.commit().await?;
    Ok(())
}

pub async fn get_minigame_list(pool: &Pool<Postgres>, map_uid: &str) -> Result<Vec<MinigameListGame>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"--sql
        SELECT slug, kind, name, minigame_key
        FROM minigame_on_map
        WHERE map_uid = $1
        ORDER BY slug
        "#,
        map_uid
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| MinigameListGame {
            slug: r.slug,
            kind: r.kind,
            name: r.name,
            key: r.minigame_key.trim().to_string(),
        })
        .collect())
}

pub async fn get_minigame_leaderboard(
    pool: &Pool<Postgres>,
    map_uid: &str,
    slug: &str,
    start: i64,
    end: i64,
) -> Result<Vec<MinigameLBEntry>, sqlx::Error> {
    let limit = (end - start).max(0);
    let rows = sqlx::query!(
        r#"--sql
        WITH mom AS (
            SELECT minigame_key, kind FROM minigame_on_map WHERE map_uid = $1 AND slug = $2
        )
        SELECT pb.user_id, u.display_name, pb.score, pb.updated_at,
               rank() OVER (
                   ORDER BY CASE WHEN mom.kind = 'Sprint' THEN pb.score END ASC NULLS LAST,
                            CASE WHEN mom.kind <> 'Sprint' THEN pb.score END DESC NULLS LAST
               ) AS rank
        FROM minigame_pb pb
        JOIN mom ON pb.minigame_key = mom.minigame_key
        LEFT JOIN users u ON u.web_services_user_id = pb.user_id
        LEFT JOIN shadow_bans sb ON pb.user_id = sb.user_id
        WHERE sb.user_id IS NULL AND pb.finish_count > 0
        ORDER BY CASE WHEN mom.kind = 'Sprint' THEN pb.score END ASC NULLS LAST,
                 CASE WHEN mom.kind <> 'Sprint' THEN pb.score END DESC NULLS LAST
        LIMIT $3
        OFFSET $4
        "#,
        map_uid,
        slug,
        limit,
        start
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| MinigameLBEntry {
            rank: r.rank.unwrap_or_default() as u32,
            wsid: r.user_id.to_string(),
            name: r.display_name,
            score: r.score,
            ts: r.updated_at.timestamp() as u32,
        })
        .collect())
}

pub async fn get_minigame_leaderboard_len(pool: &Pool<Postgres>, map_uid: &str, slug: &str) -> Result<i64, sqlx::Error> {
    let r = sqlx::query!(
        r#"--sql
        SELECT COUNT(*) AS count
        FROM minigame_pb pb
        JOIN minigame_on_map mom ON pb.minigame_key = mom.minigame_key
        LEFT JOIN shadow_bans sb ON pb.user_id = sb.user_id
        WHERE mom.map_uid = $1 AND mom.slug = $2
          AND sb.user_id IS NULL
          AND pb.finish_count > 0
        "#,
        map_uid,
        slug
    )
    .fetch_one(pool)
    .await?;
    Ok(r.count.unwrap_or(0))
}

pub async fn get_minigame_my_rank(
    pool: &Pool<Postgres>,
    map_uid: &str,
    slug: &str,
    user_id: &Uuid,
) -> Result<Option<MinigameLBEntry>, sqlx::Error> {
    let r = sqlx::query!(
        r#"--sql
        WITH mom AS (
            SELECT minigame_key, kind FROM minigame_on_map WHERE map_uid = $1 AND slug = $2
        ),
        lb AS (
            SELECT pb.user_id, u.display_name, pb.score, pb.updated_at,
                   rank() OVER (
                       ORDER BY CASE WHEN mom.kind = 'Sprint' THEN pb.score END ASC NULLS LAST,
                                CASE WHEN mom.kind <> 'Sprint' THEN pb.score END DESC NULLS LAST
                   ) AS rank
            FROM minigame_pb pb
            JOIN mom ON pb.minigame_key = mom.minigame_key
            LEFT JOIN users u ON u.web_services_user_id = pb.user_id
            LEFT JOIN shadow_bans sb ON pb.user_id = sb.user_id
            WHERE sb.user_id IS NULL AND pb.finish_count > 0
        )
        SELECT * FROM lb WHERE user_id = $3
        "#,
        map_uid,
        slug,
        user_id
    )
    .fetch_optional(pool)
    .await?;
    Ok(r.map(|row| MinigameLBEntry {
        rank: row.rank.unwrap_or_default() as u32,
        wsid: row.user_id.to_string(),
        name: row.display_name,
        score: row.score,
        ts: row.updated_at.timestamp() as u32,
    }))
}

pub async fn get_minigame_leaderboard_page(
    pool: &Pool<Postgres>,
    map_uid: &str,
    slug: &str,
    page: u32,
) -> Result<Vec<MinigameLBEntry>, sqlx::Error> {
    let start = (page as i64) * 100;
    let end = start + 100;
    get_minigame_leaderboard(pool, map_uid, slug, start, end).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn skips_unknown_kind() {
        let spec = json!({"minigames":{"games":[
            {"name":"x","slug":"x","kind":"Nope","type":99}
        ]}});
        assert!(parse_catalog_games(&spec).expect("valid catalog").is_empty());
    }

    #[test]
    fn defaults_missing_slug_and_maps_kinds() {
        let spec = json!({"minigames":{"games":[
            {"name":"TT","kind":"TimeTrial","type":1},
            {"name":"JH","slug":"jh-1","kind":"JumpHigh","type":2},
            {"name":"PL","slug":"puz","type":6}
        ]}});
        let g = parse_catalog_games(&spec).expect("valid catalog");
        assert_eq!(g.len(), 3);
        assert_eq!(g[0].slug, "Sprint-1");
        assert_eq!(g[0].kind, "Sprint");
        assert!(g[0].lower_is_better);
        assert_eq!(g[1].kind, "HighJump");
        assert!(!g[1].lower_is_better);
        assert_eq!(g[2].kind, "Sprint");
        assert_eq!(g[2].slug, "puz");
        assert!(g[2].lower_is_better);
    }

    #[test]
    fn missing_minigames_is_empty() {
        assert!(parse_catalog_games(&json!({"info":{}})).expect("valid catalog").is_empty());
        assert!(parse_catalog_games(&json!({"minigames":{}})).expect("valid catalog").is_empty());
        assert!(parse_catalog_games(&json!({"minigames":{"games":[]}}))
            .expect("valid catalog")
            .is_empty());
    }

    #[test]
    fn kind_string_preferred_over_type() {
        let spec = json!({"minigames":{"games":[
            {"name":"x","slug":"s","kind":"MaxAvgSpeed","type":1}
        ]}});
        let g = parse_catalog_games(&spec).expect("valid catalog");
        assert_eq!(g[0].kind, "MaxAvgSpeed");
        assert!(!g[0].lower_is_better);
    }

    #[test]
    fn defaults_are_canonical_per_kind_and_advance_for_every_recognized_row() {
        let spec = json!({"minigames":{"games":[
            {"kind":"Sprint"},
            {"kind":"TimeTrial"},
            {"kind":"PuzzleLocations"},
            {"kind":"Survival"},
            {"kind":"TimeOffGround"},
            {"kind":"Darts"},
            {"kind":"JumpAccuracy"},
            {"kind":"HighJump"},
            {"kind":"JumpHigh"},
            {"kind":"LongJump"},
            {"kind":"MaxAvgSpeed"}
        ]}});
        let slugs: Vec<_> = parse_catalog_games(&spec)
            .expect("valid catalog")
            .into_iter()
            .map(|game| game.slug)
            .collect();
        assert_eq!(
            slugs,
            [
                "Sprint-1",
                "Sprint-2",
                "Sprint-3",
                "Survival-1",
                "Survival-2",
                "Darts-1",
                "Darts-2",
                "HighJump-1",
                "HighJump-2",
                "LongJump-1",
                "MaxAvgSpeed-1",
            ]
        );
    }

    #[test]
    fn explicit_non_string_slug_invalidates_entire_catalog() {
        // A silently defaulted slug renames the minigame, which changes
        // `MD5(mapUid || ':' || slug)` and orphans every PB on it.
        for bad in [json!(17), json!(null), json!(true), json!([]), json!({})] {
            let spec = json!({"minigames":{"games":[
                {"kind":"Sprint","slug":"valid"},
                {"kind":"TimeTrial","slug": bad}
            ]}});
            assert!(matches!(
                parse_catalog_games(&spec),
                Err(CatalogValidationError::BlankSlug { catalog: "minigame" })
            ));
        }
    }

    #[test]
    fn duplicate_canonical_slug_invalidates_catalog() {
        let spec = json!({"minigames":{"games":[
            {"kind":"Sprint","slug":"  race  "},
            {"kind":"TimeTrial","slug":"race"}
        ]}});
        assert!(matches!(
            parse_catalog_games(&spec),
            Err(CatalogValidationError::DuplicateSlug {
                catalog: "minigame",
                slug
            }) if slug == "race"
        ));
    }

    #[test]
    fn explicit_blank_slug_invalidates_catalog() {
        for slug in ["", "   "] {
            let spec = json!({"minigames":{"games":[
                {"kind":"Sprint"},
                {"kind":"Survival","slug":slug},
                {"kind":"Darts"}
            ]}});
            assert!(matches!(
                parse_catalog_games(&spec),
                Err(CatalogValidationError::BlankSlug { catalog: "minigame" })
            ));
        }
    }

    #[test]
    fn canonical_editor_kinds_and_legacy_aliases_are_normalized() {
        let canonical = [
            ("Sprint", "Sprint", true),
            ("Survival", "Survival", false),
            ("Darts", "Darts", false),
            ("HighJump", "HighJump", false),
            ("LongJump", "LongJump", false),
            ("MaxAvgSpeed", "MaxAvgSpeed", false),
        ];
        for (input, expected, lower_is_better) in canonical {
            assert_eq!(map_kind_str(input), Some((expected, lower_is_better)), "{input}");
        }

        let aliases = [
            ("TimeTrial", "Sprint", true),
            ("PuzzleLocations", "Sprint", true),
            ("JumpHigh", "HighJump", false),
            ("JumpAccuracy", "Darts", false),
            ("TimeOffGround", "Survival", false),
            ("KingOfTheHill", "Survival", false),
        ];
        for (input, expected, lower_is_better) in aliases {
            assert_eq!(map_kind_str(input), Some((expected, lower_is_better)), "{input}");
        }

        let legacy_ints = [
            (1, "Sprint", true),
            (2, "HighJump", false),
            (3, "Darts", false),
            (4, "MaxAvgSpeed", false),
            (5, "Survival", false),
            (6, "Sprint", true),
        ];
        for (input, expected, lower_is_better) in legacy_ints {
            assert_eq!(map_kind_int(input), Some((expected, lower_is_better)), "{input}");
        }
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use serde_json::json;
    use sqlx::postgres::PgPoolOptions;

    async fn test_pool() -> Pool<Postgres> {
        dotenv::dotenv().ok();
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for minigame DB tests");
        PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("connect to DATABASE_URL")
    }

    async fn seed_user(pool: &Pool<Postgres>, name: &str) -> Uuid {
        let user_id = Uuid::now_v7();
        sqlx::query!(
            "INSERT INTO users (web_services_user_id, display_name) VALUES ($1, $2)",
            user_id,
            name
        )
        .execute(pool)
        .await
        .expect("insert user");
        user_id
    }

    #[tokio::test]
    async fn all_canonical_kinds_bind_report_and_sort_in_the_right_direction() {
        let pool = test_pool().await;
        let author = seed_user(&pool, "minigame-kind-author").await;
        let low = seed_user(&pool, "minigame-kind-low").await;
        let high = seed_user(&pool, "minigame-kind-high").await;
        let suffix = Uuid::new_v4().simple().to_string();
        let map_uid = format!("Kinds{}", &suffix[..22]);
        let name_id = format!("kinds-{}", &suffix[..8]);
        let kinds = ["Sprint", "Survival", "Darts", "HighJump", "LongJump", "MaxAvgSpeed"];
        let games: Vec<_> = kinds
            .iter()
            .map(|kind| json!({"name": kind, "slug": format!("{kind}-1"), "kind": kind}))
            .collect();
        let spec = json!({"minigames":{"games": games}});
        let spec_id = crate::queries::custom_map_aux_specs::upsert_spec(&pool, &author, &name_id, &spec)
            .await
            .expect("upsert spec");
        replace_catalog(&pool, spec_id, &author, &name_id, &spec)
            .await
            .expect("replace catalog");
        assert!(bind_map_from_catalog(&pool, &map_uid, spec_id).await.expect("bind"));
        sqlx::query!(
            r#"INSERT INTO map_listing_bind
                   (map_uid, spec_id, verified_author_id, author_verified_at,
                    materialized_spec_updated_at, materialized_spec_hash)
               SELECT $1, id, user_id, NOW(), updated_at, spec_hash
               FROM custom_map_aux_specs WHERE id = $2"#,
            map_uid,
            spec_id
        )
        .execute(&pool)
        .await
        .expect("mark minigame snapshot materialized");

        let list = get_minigame_list(&pool, &map_uid).await.expect("list");
        let mut listed_kinds: Vec<_> = list.iter().map(|game| game.kind.as_str()).collect();
        listed_kinds.sort_unstable();
        let mut expected_kinds = kinds.to_vec();
        expected_kinds.sort_unstable();
        assert_eq!(listed_kinds, expected_kinds);

        for kind in kinds {
            let slug = format!("{kind}-1");
            if kind == "Sprint" {
                for invalid_score in [i64::MIN, -1] {
                    report_attempt(&pool, &low, &map_uid, &slug, spec_id, invalid_score, "finish")
                        .await
                        .expect("negative finish fails closed");
                }
                assert_eq!(
                    sqlx::query!(
                        "SELECT COUNT(*) AS count FROM minigame_attempt WHERE user_id = $1 AND map_uid = $2 AND slug = $3",
                        low,
                        map_uid,
                        slug
                    )
                    .fetch_one(&pool)
                    .await
                    .expect("count rejected attempts")
                    .count
                    .unwrap_or(0),
                    0
                );
                assert_eq!(
                    sqlx::query!(
                        "SELECT COUNT(*) AS count FROM minigame_pb WHERE user_id = $1 AND map_uid = $2 AND slug = $3",
                        low,
                        map_uid,
                        slug
                    )
                    .fetch_one(&pool)
                    .await
                    .expect("count rejected PBs")
                    .count
                    .unwrap_or(0),
                    0
                );
                assert!(get_minigame_leaderboard(&pool, &map_uid, &slug, 0, 100)
                    .await
                    .expect("read after rejected i64::MIN")
                    .is_empty());

                let key = format!("{:016}", 7);
                assert!(sqlx::query!(
                    "INSERT INTO minigame_attempt (user_id, minigame_key, map_uid, slug, elapsed_ms, status) VALUES ($1, $2, $3, $4, $5, 'finish')",
                    low,
                    key,
                    map_uid,
                    slug,
                    -1_i64
                )
                .execute(&pool)
                .await
                .is_err());
                assert!(sqlx::query!(
                    "INSERT INTO minigame_pb (user_id, minigame_key, map_uid, slug, score, finish_count, attempt_count) VALUES ($1, $2, $3, $4, $5, 1, 1)",
                    low,
                    key,
                    map_uid,
                    slug,
                    -1_i64
                )
                .execute(&pool)
                .await
                .is_err());
            }
            report_attempt(&pool, &low, &map_uid, &slug, spec_id, 100, "finish")
                .await
                .expect("low finish");
            report_attempt(&pool, &high, &map_uid, &slug, spec_id, 200, "finish")
                .await
                .expect("high finish");

            let board = get_minigame_leaderboard(&pool, &map_uid, &slug, 0, 100).await.expect("leaderboard");
            assert_eq!(board.len(), 2, "{kind}");
            let expected_winner = if kind == "Sprint" { low } else { high };
            assert_eq!(board[0].wsid, expected_winner.to_string(), "{kind} sort direction");
            assert_eq!(board[0].rank, 1, "{kind}");

            let winner_rank = get_minigame_my_rank(&pool, &map_uid, &slug, &expected_winner)
                .await
                .expect("my rank")
                .expect("winner row");
            assert_eq!(winner_rank.rank, 1, "{kind} my-rank direction");

            let (winner, worse, better) = if kind == "Sprint" { (low, 300, 50) } else { (high, 50, 300) };
            report_attempt(&pool, &winner, &map_uid, &slug, spec_id, worse, "finish")
                .await
                .expect("worse finish");
            let after_worse = get_minigame_my_rank(&pool, &map_uid, &slug, &winner)
                .await
                .expect("rank after worse")
                .expect("winner after worse");
            assert_eq!(after_worse.score, if kind == "Sprint" { 100 } else { 200 }, "{kind} keeps PB");
            report_attempt(&pool, &winner, &map_uid, &slug, spec_id, better, "finish")
                .await
                .expect("better finish");
            let after_better = get_minigame_my_rank(&pool, &map_uid, &slug, &winner)
                .await
                .expect("rank after better")
                .expect("winner after better");
            assert_eq!(after_better.score, better, "{kind} updates PB");
        }

        sqlx::query!("DELETE FROM map_listing_bind WHERE map_uid = $1", map_uid)
            .execute(&pool)
            .await
            .expect("cleanup binding");
        for user_id in [low, high, author] {
            sqlx::query!("DELETE FROM users WHERE web_services_user_id = $1", user_id)
                .execute(&pool)
                .await
                .expect("cleanup user");
        }
    }
}
