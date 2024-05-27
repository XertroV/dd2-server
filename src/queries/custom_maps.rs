use sqlx::{query, Pool, Postgres};

pub async fn get_map_nb_playing_live(pool: &Pool<Postgres>, map_uid: &str) -> Result<i64, sqlx::Error> {
    let nb_playing_now = query!(
        r#"--sql
        SELECT COUNT(*) FROM map_curr_heights WHERE map_uid = $1
            AND updated_at > now() - interval '30 seconds'
    "#,
        map_uid
    )
    .fetch_one(pool)
    .await?
    .count
    .unwrap_or(0);
    Ok(nb_playing_now)
}
