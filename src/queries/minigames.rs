use serde_json::Value;
use sqlx::{Pool, Postgres};
use uuid::Uuid;

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
        "TimeTrial" => Some(("TimeTrial", true)),
        "PuzzleLocations" => Some(("TimeTrial", true)),
        "JumpHigh" => Some(("HighJump", false)),
        "JumpAccuracy" => Some(("Darts", false)),
        "MaxAvgSpeed" => Some(("MaxAvgSpeed", false)),
        "TimeOffGround" => Some(("KingOfTheHill", false)),
        _ => None,
    }
}

fn map_kind_int(t: i64) -> Option<(&'static str, bool)> {
    match t {
        1 => Some(("TimeTrial", true)),
        2 => Some(("HighJump", false)),
        3 => Some(("Darts", false)),
        4 => Some(("MaxAvgSpeed", false)),
        5 => Some(("KingOfTheHill", false)),
        6 => Some(("TimeTrial", true)),
        _ => None,
    }
}

/// Replace catalog for `(user_id, name_id)` from `spec.minigames.games[]`.
/// Missing/empty games → delete prior rows.
pub fn parse_catalog_games(spec: &Value) -> Vec<CatalogGame> {
    let Some(games) = spec
        .get("minigames")
        .and_then(|m| m.get("games"))
        .and_then(|g| g.as_array())
    else {
        return vec![];
    };
    let mut last_by_slug = std::collections::HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for g in games {
        if !g.is_object() {
            continue;
        }
        let Some((kind, lower_is_better)) = map_kind_from_game(g) else {
            continue;
        };
        let slug_raw = g.get("slug").and_then(|s| s.as_str()).unwrap_or("");
        let slug = if slug_raw.is_empty() {
            "TimeTrial-1".to_string()
        } else {
            slug_raw.to_string()
        };
        let name = g.get("name").and_then(|s| s.as_str()).unwrap_or("").to_string();
        if !last_by_slug.contains_key(&slug) {
            order.push(slug.clone());
        }
        last_by_slug.insert(
            slug.clone(),
            CatalogGame {
                slug,
                kind: kind.to_string(),
                name,
                lower_is_better,
            },
        );
    }
    order
        .into_iter()
        .filter_map(|s| last_by_slug.remove(&s))
        .collect()
}

/// Replace catalog for a spec row from `spec.minigames.games[]`.
/// Missing/empty games → delete prior rows for this spec_id.
pub async fn replace_catalog(
    pool: &Pool<Postgres>,
    spec_id: i64,
    user_id: &Uuid,
    name_id: &str,
    spec: &Value,
) -> Result<(), sqlx::Error> {
    let games = parse_catalog_games(spec);
    let mut tx = pool.begin().await?;
    sqlx::query!(
        r#"--sql
        DELETE FROM minigame_catalog WHERE spec_id = $1
        "#,
        spec_id
    )
    .execute(&mut *tx)
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
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
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
    let row = sqlx::query!(
        r#"--sql
        SELECT user_id, name_id, spec FROM custom_map_aux_specs WHERE id = $1
        "#,
        spec_id
    )
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(());
    };
    replace_catalog(pool, spec_id, &row.user_id, &row.name_id, &row.spec).await
}

/// Copy all catalog rows for a spec onto `minigame_on_map` for this uid.
/// Returns whether any rows were bound (unknown spec → false).
pub async fn bind_map_from_catalog(pool: &Pool<Postgres>, map_uid: &str, spec_id: i64) -> Result<bool, sqlx::Error> {
    ensure_catalog(pool, spec_id).await?;
    let r = sqlx::query!(
        r#"--sql
        INSERT INTO minigame_on_map (map_uid, slug, minigame_key, kind, name, spec_user, name_id)
        SELECT $1, c.slug, substring(md5($3 || ':' || c.slug) from 1 for 16), c.kind, c.name, c.user_id, c.name_id
        FROM minigame_catalog c
        WHERE c.spec_id = $2
        ON CONFLICT (map_uid, slug) DO UPDATE SET
            minigame_key = EXCLUDED.minigame_key,
            kind = EXCLUDED.kind,
            name = EXCLUDED.name,
            spec_user = EXCLUDED.spec_user,
            name_id = EXCLUDED.name_id
        "#,
        map_uid,
        spec_id,
        map_uid
    )
    .execute(pool)
    .await?;
    Ok(r.rows_affected() > 0)
}

async fn catalog_lower_is_better(pool: &Pool<Postgres>, spec_id: i64, slug: &str) -> Result<Option<bool>, sqlx::Error> {
    let r = sqlx::query!(
        r#"--sql
        SELECT lower_is_better FROM minigame_catalog
        WHERE spec_id = $1 AND slug = $2
        "#,
        spec_id,
        slug
    )
    .fetch_optional(pool)
    .await?;
    Ok(r.map(|row| row.lower_is_better))
}

/// Accepted attempt: insert attempt, bind map, bump attempt_count (placeholder PB at 0).
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
    ensure_catalog(pool, spec_id).await?;
    let lower_is_better = match catalog_lower_is_better(pool, spec_id, slug).await? {
        Some(v) => v,
        None => return Ok(()), // unknown catalog slug: silent drop
    };

    let mut tx = pool.begin().await?;

    sqlx::query!(
        r#"--sql
        INSERT INTO minigame_attempt (user_id, minigame_key, map_uid, slug, elapsed_ms, status)
        VALUES ($1, substring(md5($6 || ':' || $3) from 1 for 16), $2, $3, $4, $5)
        "#,
        user_id,
        map_uid,
        slug,
        elapsed_ms,
        status,
        map_uid
    )
    .execute(&mut *tx)
    .await?;

    sqlx::query!(
        r#"--sql
        INSERT INTO minigame_on_map (map_uid, slug, minigame_key, kind, name, spec_user, name_id)
        SELECT $1, c.slug, substring(md5($4 || ':' || c.slug) from 1 for 16), c.kind, c.name, c.user_id, c.name_id
        FROM minigame_catalog c
        WHERE c.spec_id = $2 AND c.slug = $3
        ON CONFLICT (map_uid, slug) DO UPDATE SET
            minigame_key = EXCLUDED.minigame_key,
            kind = EXCLUDED.kind,
            name = EXCLUDED.name,
            spec_user = EXCLUDED.spec_user,
            name_id = EXCLUDED.name_id
        "#,
        map_uid,
        spec_id,
        slug,
        map_uid
    )
    .execute(&mut *tx)
    .await?;

    sqlx::query!(
        r#"--sql
        INSERT INTO minigame_pb (user_id, minigame_key, map_uid, slug, score, finish_count, attempt_count, last_played, updated_at)
        VALUES ($1, substring(md5($4 || ':' || $3) from 1 for 16), $2, $3, 0, 0, 1, now(), now())
        ON CONFLICT (user_id, minigame_key) DO UPDATE SET
            attempt_count = minigame_pb.attempt_count + 1,
            last_played = now()
        "#,
        user_id,
        map_uid,
        slug,
        map_uid
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
              AND minigame_key = substring(md5($3 || ':' || $4) from 1 for 16)
              AND (
                minigame_pb.finish_count = 0
                OR ($5 AND minigame_pb.score > $1)
                OR (NOT $5 AND minigame_pb.score < $1)
              )
            RETURNING id
            "#,
            elapsed_ms,
            user_id,
            map_uid,
            slug,
            lower_is_better
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
                    WHERE user_id = $1 AND minigame_key = substring(md5($2 || ':' || $3) from 1 for 16)
                    "#,
                    user_id,
                    map_uid,
                    slug
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
               rank() OVER (ORDER BY CASE WHEN mom.kind = 'TimeTrial' THEN pb.score ELSE -pb.score END ASC) AS rank
        FROM minigame_pb pb
        JOIN mom ON pb.minigame_key = mom.minigame_key
        LEFT JOIN users u ON u.web_services_user_id = pb.user_id
        LEFT JOIN shadow_bans sb ON pb.user_id = sb.user_id
        WHERE sb.user_id IS NULL AND pb.finish_count > 0
        ORDER BY CASE WHEN mom.kind = 'TimeTrial' THEN pb.score ELSE -pb.score END ASC
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
                   rank() OVER (ORDER BY CASE WHEN mom.kind = 'TimeTrial' THEN pb.score ELSE -pb.score END ASC) AS rank
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
        assert!(parse_catalog_games(&spec).is_empty());
    }

    #[test]
    fn defaults_empty_slug_and_maps_kinds() {
        let spec = json!({"minigames":{"games":[
            {"name":"TT","slug":"","kind":"TimeTrial","type":1},
            {"name":"JH","slug":"jh-1","kind":"JumpHigh","type":2},
            {"name":"PL","slug":"puz","type":6}
        ]}});
        let g = parse_catalog_games(&spec);
        assert_eq!(g.len(), 3);
        assert_eq!(g[0].slug, "TimeTrial-1");
        assert_eq!(g[0].kind, "TimeTrial");
        assert!(g[0].lower_is_better);
        assert_eq!(g[1].kind, "HighJump");
        assert!(!g[1].lower_is_better);
        assert_eq!(g[2].kind, "TimeTrial");
        assert!(g[2].lower_is_better);
    }

    #[test]
    fn missing_minigames_is_empty() {
        assert!(parse_catalog_games(&json!({"info":{}})).is_empty());
        assert!(parse_catalog_games(&json!({"minigames":{}})).is_empty());
        assert!(parse_catalog_games(&json!({"minigames":{"games":[]}})).is_empty());
    }

    #[test]
    fn kind_string_preferred_over_type() {
        let spec = json!({"minigames":{"games":[
            {"name":"x","slug":"s","kind":"MaxAvgSpeed","type":1}
        ]}});
        let g = parse_catalog_games(&spec);
        assert_eq!(g[0].kind, "MaxAvgSpeed");
        assert!(!g[0].lower_is_better);
    }
}
