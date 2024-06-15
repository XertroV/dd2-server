use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration, SystemTime},
};

use donations::{get_and_update_donations_from_gfm, get_donations_from_matcherino, update_donations_in_db};
use dotenv::dotenv;
use env_logger::Env;
use log::{debug, error, info, warn};
use player::Player;
use queries::{get_global_lb, update_global_overview, update_server_stats};
use router::ToPlayerMgr;
use serde_json;
use sqlx::{
    postgres::{PgPool, PgPoolOptions},
    query, query_as, Pool, Postgres,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, Interest},
    net::{TcpListener, TcpStream},
    sync::{
        mpsc::{unbounded_channel, Receiver, Sender, UnboundedReceiver, UnboundedSender},
        Mutex, Semaphore,
    },
};
use uuid::Uuid;

use crate::{
    consts::DD2_MAP_UID, http::run_http_server, op_auth::init_op_config, player::ToPlayer, queries::get_server_info,
    router::LeaderboardEntry,
};

mod api_error;
mod consts;
mod db;
mod donations;
mod http;
mod op_auth;
mod player;
mod queries;
mod router;

#[tokio::main(flavor = "multi_thread", worker_threads = 10)]
async fn main() {
    dotenv::from_path(".env-clean").ok();
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();

    let db_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(Duration::from_secs(2))
        .connect(&db_url)
        .await
        .unwrap();
    let db = Arc::new(pool);
    info!("Connected to db");

    match sqlx::migrate!().run(db.as_ref()).await {
        Ok(_) => info!("Migrations ran successfully"),
        Err(e) => panic!("Error running migrations: {:?}", e),
    }

    let players_to_del = find_players_to_del(&db).await.unwrap();
    info!("Got {} players to delete", players_to_del.len());

    if let Ok(tx) = db.begin().await {
        for (i, (user_id, user_name)) in players_to_del.iter().enumerate() {
            // break;
            if i < 5 {
                warn!("Deleting user {} ({})", user_name, user_id);
                delete_user_contexts(&db, user_id).await;
                query!(
                    r#"
                DELETE FROM users WHERE web_services_user_id = $1
                "#,
                    user_id
                )
                .execute(db.as_ref())
                .await
                .unwrap();
                warn!("Deleted user {} ({})", user_name, user_id);
            }
        }
    }

    // let twitch = queries::api::get_twitch_profiles_all(&db).await.unwrap();
    // info!("Got {} twitch profiles", twitch.as_array().unwrap().len());
}

async fn delete_user_contexts(pool: &Pool<Postgres>, user_id: &Uuid) {
    let sessions = query!("SELECT session_token FROM sessions WHERE user_id = $1", user_id)
        .fetch_all(pool)
        .await
        .unwrap();

    let nb_sessions = sessions.len();
    info!("Deleting {:?} sessions (user: {:?})", nb_sessions, user_id);

    // panic!("Sessions: {:?}, {}", sessions[0], sessions.len());
    for sess in sessions {
        let contexts = query!("SELECT * FROM contexts WHERE session_token = $1", sess.session_token)
            .fetch_all(pool)
            .await
            .unwrap();
        let count = contexts.len();
        if count > 0 {
            info!("Deleting {:?} contexts (session: {:?})", contexts.len(), sess.session_token);
        }

        // if count > 0 {
        //     panic!("Deleting {:?} contexts (session: {:?})", count, sess.session_token);
        // }

        let nb_falls = query!(r#"SELECT COUNT(*) FROM falls WHERE session_token = $1"#, sess.session_token)
            .fetch_one(pool)
            .await
            .unwrap()
            .count
            .unwrap();
        if nb_falls > 0 {
            info!("Deleting {:?} falls (session: {:?})", nb_falls, sess.session_token);
            query!(r#"DELETE FROM falls WHERE session_token = $1"#, sess.session_token)
                .execute(pool)
                .await
                .unwrap();
        }

        let nb_respawns = query!(r#"SELECT COUNT(*) FROM respawns WHERE session_token = $1"#, sess.session_token)
            .fetch_one(pool)
            .await
            .unwrap()
            .count
            .unwrap();

        if nb_respawns > 0 {
            info!("Deleting {:?} respawns (session: {:?})", nb_respawns, sess.session_token);
            query!(r#"DELETE FROM respawns WHERE session_token = $1"#, sess.session_token)
                .execute(pool)
                .await
                .unwrap();
        }

        for ctx in contexts {
            query!(r#"DELETE FROM vehicle_states WHERE context_id = $1;"#, ctx.context_id)
                .execute(pool)
                .await
                .unwrap();
            debug!("Deleted vehicle states for context {:?}", ctx.context_id);
            query!(r#"DELETE FROM game_cam_nods WHERE context_id = $1;"#, ctx.context_id)
                .execute(pool)
                .await
                .unwrap();
            debug!("Deleted game cam nods for context {:?}", ctx.context_id);
        }

        if count > 0 {
            let _res = query!(
                r#"
                DELETE FROM contexts c
                WHERE c.session_token = $1
                "#,
                sess.session_token
            )
            .execute(pool)
            .await
            .unwrap();
            info!("Deleted {:?} contexts (session: {:?})", count, sess.session_token);
            // panic!("Deleted {:?} contexts (session: {:?})", count, sess.session_token);
        }
    }

    info!("Deleting {:?} sessions (user: {:?})", nb_sessions, user_id);

    query!(r#"DELETE FROM sessions WHERE user_id = $1"#, user_id)
        .execute(pool)
        .await
        .unwrap();

    info!("Deleted {:?} sessions (user: {:?})", nb_sessions, user_id);
}

async fn find_players_to_del(pool: &Pool<Postgres>) -> Result<Vec<(Uuid, String)>, sqlx::Error> {
    let rows = query!(
        r#"
        SELECT user_id as "user_id?", display_name as "name?", rank, height FROM ranked_lb_view WHERE height > 800.0
        "#,
    )
    .fetch_all(pool)
    .await?;

    let keep_users: HashSet<_> = rows.iter().map(|r| (r.user_id.unwrap())).collect();

    let all_users = query!(
        r#"
        SELECT web_services_user_id as user_id, display_name as name FROM users
        "#,
    )
    .fetch_all(pool)
    .await?;

    let all_users_len = all_users.len();

    let mut ret = vec![];
    for user in all_users.into_iter() {
        if !keep_users.contains(&user.user_id) {
            ret.push((user.user_id, user.name));
        }
    }

    info!("Kept {} users", keep_users.len());
    info!("Deleting {} users", ret.len());
    info!("Total users: {}", all_users_len);

    Ok(ret)
}
