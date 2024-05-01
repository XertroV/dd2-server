use log::{info, warn};
use sqlx::{prelude::FromRow, query, query_as, types::Uuid, Pool, Postgres};

use crate::api_error::Error;

/*
CREATE TABLE clip_submissions (
    id SERIAL PRIMARY KEY,
    user_id UUID REFERENCES users(web_services_user_id) NOT NULL,
    clip_url VARCHAR(255) NOT NULL UNIQUE,
    clip_start_time INTEGER NOT NULL DEFAULT -1,
    clip_end_time INTEGER NOT NULL DEFAULT -1,
    clip_description TEXT NOT NULL,
    hidden BOOLEAN NOT NULL DEFAULT false,
    ts TIMESTAMP WITHOUT TIME ZONE NOT NULL DEFAULT NOW()
);
CREATE INDEX clip_submissions_user_id_ts_idx ON clip_submissions(id, user_id, ts);
CREATE UNIQUE INDEX clip_submissions_clip_url_idx ON clip_submissions(clip_url);

CREATE TABLE clip_votes (
    id SERIAL PRIMARY KEY,
    clip_id INTEGER REFERENCES clip_submissions(id) NOT NULL,
    user_id UUID REFERENCES users(web_services_user_id) NOT NULL,
    vote INTEGER NOT NULL,
    ts TIMESTAMP WITHOUT TIME ZONE NOT NULL DEFAULT NOW()
);
CREATE UNIQUE INDEX clip_votes_clip_id_idx ON clip_votes(clip_id, user_id);

*/

#[derive(Debug, Clone, FromRow)]
pub struct ClipSubmission {
    pub id: i32,
    pub user_id: Uuid,
    pub clip_url: String,
    pub clip_start_time: i32,
    pub clip_end_time: i32,
    pub clip_description: String,
    pub hidden: bool,
    pub ts: chrono::NaiveDateTime,
}

#[derive(Debug, Clone, FromRow)]
pub struct ClipVote {
    pub id: i32,
    pub clip_id: i32,
    pub user_id: Uuid,
    pub vote: i32,
    pub ts: chrono::NaiveDateTime,
}

// raises errors on duplicate
pub async fn insert_clip_submission(
    pool: &Pool<Postgres>,
    user_id: &Uuid,
    clip_url: &str,
    clip_start_time: i32,
    clip_end_time: i32,
    clip_description: &str,
) -> Result<(), sqlx::Error> {
    query!(
        "INSERT INTO clip_submissions (user_id, clip_url, clip_start_time, clip_end_time, clip_description) VALUES ($1, $2, $3, $4, $5);",
        user_id,
        clip_url,
        clip_start_time,
        clip_end_time,
        clip_description,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn insert_clip_vote(pool: &Pool<Postgres>, clip_id: i32, user_id: &Uuid, vote: i32) -> Result<(), Error> {
    if !(-2..=2).contains(&vote) {
        warn!("Invalid vote value: {}", vote);
        return Err(Error::StrErr(format!("Invalid vote value: {} from {}", vote, user_id)));
    }
    query!(
        "INSERT INTO clip_votes (clip_id, user_id, vote) VALUES ($1, $2, $3)
        ON CONFLICT (clip_id, user_id) DO UPDATE SET vote = EXCLUDED.vote, ts = NOW()
        ;",
        clip_id,
        user_id,
        vote,
    )
    .execute(pool)
    .await?;
    Ok(())
}
