use sqlx::{prelude::FromRow, query, query_as, types::Uuid, Pool, Postgres};

use crate::{
    player::{decode_context_flags, Player},
    router::PlayerCtx,
};

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
pub struct MapRow {
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
    flags: &[bool; 15],
    raw_flags: u64,
    has_vl_item: bool,
    map_id: Option<i32>,
    managers: i64,
    bi: [i32; 2],
    predecessor: &Option<Uuid>,
) -> Result<Uuid, sqlx::Error> {
    let context_id = context_id.unwrap_or_else(Uuid::now_v7);
    let r = query!(
        "INSERT INTO contexts (context_id, session_token, flags, flags_raw, has_vl_item, map_id, managers, block_count, item_count, predecessor) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING context_id;",
        context_id,
        session_token,
        flags,
        raw_flags as i64,
        has_vl_item,
        map_id,
        managers,
        bi[0],
        bi[1],
        predecessor.as_ref()
    )
    .fetch_one(pool)
    .await?;
    Ok(r.context_id)
}

pub async fn context_mark_succeeded(pool: &Pool<Postgres>, prior: &Uuid, successor: &Uuid) -> Result<(), sqlx::Error> {
    query!(
        "UPDATE contexts SET successor = $1, terminated = true WHERE context_id = $2;",
        successor,
        prior
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn insert_context_packed(
    pool: &Pool<Postgres>,
    session_token: &Uuid,
    context: &PlayerCtx,
    prior: &Option<Uuid>,
    map_id: Option<i32>,
    bi: [i32; 2],
) -> Result<Uuid, sqlx::Error> {
    // let prior = prior.as_ref().cloned();
    insert_context(
        pool,
        &None,
        session_token,                     // &context.session_token,
        &decode_context_flags(context.sf), // context.is_editor,
        context.sf,
        context.i, // context.has_vl_item,
        map_id,
        context.mi as i64,
        bi,
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
    let h = match hash.len() == 64 {
        true => match hex::decode(hash) {
            Ok(h) => h,
            Err(e) => hash[0..hash.len().min(32)].into(),
        },
        false => hash[..hash.len().min(32)].into(),
    };
    let r = query!(
        "INSERT INTO maps (uid, name, hash) VALUES ($1, $2, $3) RETURNING map_id;",
        uid,
        name,
        h
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

/*
CREATE TABLE game_cam_nods (
    id SERIAL PRIMARY KEY,
    session_token UUID REFERENCES sessions(session_token) NOT NULL,
    context_id UUID REFERENCES contexts(context_id) NOT NULL,
    raw BYTEA NOT NULL,
    init_byte SMALLINT NOT NULL,
    is_race_nod_null BOOLEAN NOT NULL,
    is_editor_cam_null BOOLEAN NOT NULL,
    is_race_88_null BOOLEAN NOT NULL,
    is_cam_1a8_16 BOOLEAN NOT NULL,
    ts TIMESTAMP WITHOUT TIME ZONE NOT NULL DEFAULT NOW()
);
CREATE INDEX context_id_idx ON game_cam_nods(context_id);
CREATE INDEX flags_idx ON game_cam_nods(init_byte, is_race_nod_null, is_editor_cam_null, is_race_88_null, is_cam_1a8_16);
*/

pub async fn insert_gc_nod(pool: &Pool<Postgres>, session_id: &Uuid, context_id: &Uuid, nod: &[u8]) -> Result<(), sqlx::Error> {
    let (init_byte, is_race_nod_null, is_editor_cam_null, is_race_88_null, is_cam_1a8_16) = match nod.len() < 0x2C0 {
        true => (0, true, true, true, false),
        false => (
            nod[0] as u8,
            nod[0x70..0x78] == [0; 8],
            nod[0x80..0x88] == [0; 8],
            nod[0x88..0x90] == [0; 8],
            nod[0x1a8] == 0x16,
        ),
    };
    query!(
        "INSERT INTO game_cam_nods (session_token, context_id, raw, init_byte, is_race_nod_null, is_editor_cam_null, is_race_88_null, is_cam_1a8_16) VALUES ($1, $2, $3, $4, $5, $6, $7, $8);",
        session_id,
        context_id,
        nod,
        init_byte as i16,
        is_race_nod_null,
        is_editor_cam_null,
        is_race_88_null,
        is_cam_1a8_16,
    ).execute(pool).await?;
    Ok(())
}
