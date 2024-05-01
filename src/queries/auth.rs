use sqlx::{prelude::FromRow, query, query_as, types::Uuid, Pool, Postgres};

#[derive(Debug, Clone, FromRow)]
pub struct User {
    pub web_services_user_id: Uuid,
    pub display_name: String,
}
impl User {
    pub fn id(&self) -> &Uuid {
        &self.web_services_user_id
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct Session {
    pub session_token: Uuid,
}

pub async fn create_session(
    pool: &Pool<Postgres>,
    user_id: &Uuid,
    plugin_info: &str,
    game_info: &str,
    gamer_info: &str,
    ip_address: &str,
) -> Result<Session, sqlx::Error> {
    let session_token = Uuid::now_v7();

    let pi_id = select_or_insert_plugin_info(pool, plugin_info).await?;
    let gi_id = select_or_insert_game_info(pool, game_info).await?;
    let gri_id = select_or_insert_gamer_info(pool, gamer_info).await?;

    let r = query!(
        "INSERT INTO sessions (session_token, user_id, plugin_info_id, game_info_id, gamer_info_id, ip_address) VALUES ($1, $2, $3, $4, $5, $6) RETURNING session_token;",
        session_token,
        user_id,
        pi_id,
        gi_id,
        gri_id,
        &ip_address[..ip_address.len().min(39)],
    )
    .fetch_one(pool)
    .await?;
    Ok(Session {
        session_token: r.session_token,
    })
}

async fn select_or_insert_plugin_info(pool: &Pool<Postgres>, plugin_info: &str) -> Result<i32, sqlx::Error> {
    let r = query!("SELECT id FROM plugin_infos WHERE info = $1;", plugin_info)
        .fetch_one(pool)
        .await
        .map(|r| r.id);
    match r {
        Ok(id) => Ok(id),
        Err(sqlx::Error::RowNotFound) => insert_plugin_info(pool, plugin_info).await,
        Err(e) => Err(e),
    }
}

async fn insert_plugin_info(pool: &Pool<Postgres>, plugin_info: &str) -> Result<i32, sqlx::Error> {
    let pi_id = query!("INSERT INTO plugin_infos (info) VALUES ($1) RETURNING id;", plugin_info)
        .fetch_one(pool)
        .await?
        .id;
    Ok(pi_id)
}

async fn select_or_insert_game_info(pool: &Pool<Postgres>, game_info: &str) -> Result<i32, sqlx::Error> {
    let r = query!("SELECT id FROM game_infos WHERE info = $1;", game_info)
        .fetch_one(pool)
        .await
        .map(|r| r.id);
    match r {
        Ok(id) => Ok(id),
        Err(sqlx::Error::RowNotFound) => insert_game_info(pool, game_info).await,
        Err(e) => Err(e),
    }
}

async fn insert_game_info(pool: &Pool<Postgres>, game_info: &str) -> Result<i32, sqlx::Error> {
    let gi_id = query!("INSERT INTO game_infos (info) VALUES ($1) RETURNING id;", game_info)
        .fetch_one(pool)
        .await?
        .id;
    Ok(gi_id)
}

async fn select_or_insert_gamer_info(pool: &Pool<Postgres>, gamer_info: &str) -> Result<i32, sqlx::Error> {
    let r = query!("SELECT id FROM gamer_infos WHERE info = $1;", gamer_info)
        .fetch_one(pool)
        .await
        .map(|r| r.id);
    match r {
        Ok(id) => Ok(id),
        Err(sqlx::Error::RowNotFound) => insert_gamer_info(pool, gamer_info).await,
        Err(e) => Err(e),
    }
}

async fn insert_gamer_info(pool: &Pool<Postgres>, gamer_info: &str) -> Result<i32, sqlx::Error> {
    let gi_id = query!("INSERT INTO gamer_infos (info) VALUES ($1) RETURNING id;", gamer_info)
        .fetch_one(pool)
        .await?
        .id;
    Ok(gi_id)
}

pub async fn resume_session(
    pool: &Pool<Postgres>,
    session_token: &Uuid,
    plugin_info: &str,
    game_info: &str,
    gamer_info: &str,
    ip_address: &str,
) -> Result<(Session, User), sqlx::Error> {
    let user = query_as!(
        User,
        "SELECT web_services_user_id, display_name FROM users
            JOIN sessions ON users.web_services_user_id = sessions.user_id
            WHERE sessions.session_token = $1;",
        session_token
    )
    .fetch_one(pool)
    .await?;
    query!("UPDATE sessions SET replaced = true WHERE session_token = $1;", session_token)
        .execute(pool)
        .await?;

    Ok((
        create_session(pool, user.id(), plugin_info, game_info, gamer_info, ip_address).await?,
        user,
    ))
}

pub async fn register_or_login(pool: &Pool<Postgres>, account_id: &Uuid, display_name: &str) -> Result<User, sqlx::Error> {
    let user = query_as!(
        User,
        r#"SELECT web_services_user_id, display_name FROM users WHERE web_services_user_id = $1"#,
        account_id
    )
    .fetch_one(pool)
    .await;
    let insert_name;
    let user = match user {
        Ok(mut user) => {
            insert_name = user.display_name != display_name;
            if insert_name {
                user.display_name = display_name.to_string();
                query!(
                    "UPDATE users SET display_name = $1 WHERE web_services_user_id = $2;",
                    display_name,
                    account_id
                )
                .execute(pool)
                .await?;
            }
            user
        }
        Err(_) => {
            insert_name = true;
            query_as!(
                User,
                "INSERT INTO users (web_services_user_id, display_name) VALUES ($1, $2)
                    RETURNING web_services_user_id, display_name;",
                account_id,
                display_name
            )
            .fetch_one(pool)
            .await?
        }
    };

    if insert_name {
        query!(
            "INSERT INTO display_names (display_name, user_id) VALUES ($1, $2)",
            display_name,
            user.id()
        )
        .execute(pool)
        .await?;
    }

    Ok(user)
}
