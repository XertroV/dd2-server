use sqlx::{prelude::FromRow, query, query_as, types::Uuid, Pool, Postgres};

use crate::router::PlayerCtx;

#[derive(Debug, Clone, FromRow)]
pub struct Context {
    pub context_id: Uuid,
    pub session_token: Uuid,
    pub is_editor: bool,
    pub is_mt_editor: bool,
    pub is_playground: bool,
    pub is_solo: bool,
    pub is_server: bool,
    pub has_vl_item: bool,
    pub map_id: i32,
    pub managers: i64,
    pub created_ts: chrono::NaiveDateTime,
    pub terminated: bool,
    pub predecessor: Option<Uuid>,
    pub successor: Option<Uuid>,
    pub ended_ts: Option<chrono::NaiveDateTime>,
}

#[derive(Debug, Clone, FromRow)]
pub struct Map {
    pub map_id: i32,
    pub uid: String,
    pub name: String,
    pub hash: String,
    pub load_count: i32,
    pub created_ts: chrono::NaiveDateTime,
}

/// Insert a new context into the database, returns the context_id
pub async fn insert_context(
    pool: &Pool<Postgres>,
    context_id: &Option<Uuid>,
    session_token: &Uuid,
    is_editor: bool,
    is_mt_editor: bool,
    is_playground: bool,
    is_solo: bool,
    is_server: bool,
    has_vl_item: bool,
    map_id: i32,
    managers: i64,
    predecessor: &Option<Uuid>,
) -> Result<Uuid, sqlx::Error> {
    let context_id = context_id.unwrap_or_else(Uuid::now_v7);
    let r = query!(
        "INSERT INTO contexts (context_id, session_token, is_editor, is_mt_editor, is_playground, is_solo, is_server, has_vl_item, map_id, managers, predecessor) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) RETURNING context_id;",
        context_id,
        session_token,
        is_editor,
        is_mt_editor,
        is_playground,
        is_solo,
        is_server,
        has_vl_item,
        map_id,
        managers,
        predecessor.clone()
    )
    .fetch_one(pool)
    .await?;
    Ok(r.context_id)
}

pub async fn context_mark_succeeded(pool: &Pool<Postgres>, prior: &Uuid, successor: &Uuid) -> Result<(), sqlx::Error> {
    query!("UPDATE contexts SET successor = $1 WHERE context_id = $2;", successor, prior)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn insert_context_packed(
    pool: &Pool<Postgres>,
    context: &PlayerCtx,
    prior: &Option<Uuid>,
    map_id: i32,
) -> Result<Uuid, sqlx::Error> {
    // let prior = prior.as_ref().cloned();
    insert_context(
        pool,
        &None,
        todo!(), // &context.session_token,
        todo!(), // context.is_editor,
        todo!(), // context.is_mt_editor,
        todo!(), // context.is_playground,
        todo!(), // context.is_solo,
        todo!(), // context.is_server,
        todo!(), // context.has_vl_item,
        map_id,
        context.managers,
        &prior,
    )
    .await
}

pub async fn get_or_insert_map(pool: &Pool<Postgres>, uid: &str, name: &str, hash: &str) -> Result<i32, sqlx::Error> {
    let r = query!("SELECT map_id FROM maps WHERE uid = $1;", uid)
        .fetch_one(pool)
        .await
        .map(|r| r.map_id);
    match r {
        Ok(id) => {
            increment_map_count(pool, id).await?;
            Ok(id)
        }
        Err(sqlx::Error::RowNotFound) => insert_map(pool, uid, name, hash).await,
        Err(e) => Err(e),
    }
}

async fn insert_map(pool: &Pool<Postgres>, uid: &str, name: &str, hash: &str) -> Result<i32, sqlx::Error> {
    let r = query!(
        "INSERT INTO maps (uid, name, hash) VALUES ($1, $2, $3) RETURNING map_id;",
        uid,
        name,
        hash
    )
    .fetch_one(pool)
    .await?;
    Ok(r.map_id)
}

async fn increment_map_count(pool: &Pool<Postgres>, map_id: i32) -> Result<(), sqlx::Error> {
    query!("UPDATE maps SET load_count = load_count + 1 WHERE map_id = $1;", map_id)
        .execute(pool)
        .await?;
    Ok(())
}
