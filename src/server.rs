use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use donations::{get_and_update_donations_from_gfm, get_donations_from_matcherino, update_donations_in_db};
use dotenv::dotenv;
use env_logger::Env;
use log::{error, info, warn};
use player::Player;
use queries::{get_global_lb, update_global_overview, update_server_stats};
use router::ToPlayerMgr;
use serde_json;
use sqlx::{
    postgres::{PgPool, PgPoolOptions},
    Pool, Postgres,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, Interest},
    net::{TcpListener, TcpStream},
    sync::{
        mpsc::{unbounded_channel, Receiver, Sender, UnboundedReceiver, UnboundedSender},
        Mutex, Semaphore,
    },
};

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

const MAX_CONNECTIONS: u32 = 4096;

#[tokio::main(flavor = "multi_thread", worker_threads = 10)]
async fn main() {
    dotenv().ok();
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();
    info!("Starting DD2 Server for UID: {}", DD2_MAP_UID);

    // init db from env var
    let db_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let pool = PgPoolOptions::new().max_connections(10).connect(&db_url).await.unwrap();
    let db = Arc::new(pool);
    info!("Connected to db");

    match sqlx::migrate!().run(db.as_ref()).await {
        Ok(_) => info!("Migrations ran successfully"),
        Err(e) => panic!("Error running migrations: {:?}", e),
    }

    // let rec = router::Response::NewRecord { name: "asdf".into(), height: 1234., testu64: 0x7ff7320b0000 };
    // // test: serialize using serde_json, print, and deserialize
    // let s = serde_json::to_string(&rec).unwrap();
    // println!("Serialized: {}", s);
    // let rec2: router::Response = serde_json::from_str(&s).unwrap();
    // println!("Deserialized: {:?}", rec2);
    // return;

    init_op_config().await;
    let bind_addr = "0.0.0.0:17677";
    warn!("Starting server on: {}", bind_addr);

    let limit_connections = Arc::new(Semaphore::new(MAX_CONNECTIONS as usize));
    let (player_mgr, player_mgr_tx) = PlayerMgr::new(db.clone(), limit_connections.clone());
    PlayerMgr::start(player_mgr.into());

    tokio::select! {
        _ = run_http_server(db.clone()) => {},
        // _ = listen(bind_addr, db.clone()) => {},
    };
}

// async fn listen(bind_addr: &str, pool: Arc<Pool<Postgres>>) -> Result<(), Box<dyn std::error::Error>> {
//     let limit_connections = Arc::new(Semaphore::new(MAX_CONNECTIONS as usize));
//     let listener = TcpListener::bind(bind_addr).await.unwrap();
//     info!("Listening on: {}", bind_addr);
//     let (player_mgr, player_mgr_tx) = PlayerMgr::new(pool.clone(), limit_connections.clone());
//     let player_mgr = Arc::new(player_mgr);
//     PlayerMgr::start(player_mgr.clone());
//     loop {
//         let permit = limit_connections.clone().acquire_owned().await.unwrap();
//         let mut backoff = 1;
//         loop {
//             match listener.accept().await {
//                 Ok((stream, _addr)) => {
//                     backoff = 1;
//                     info!("New connection from {:?}", stream.peer_addr().unwrap());
//                     let player_mgr = player_mgr.clone();
//                     let player_mgr_tx = player_mgr_tx.clone();
//                     let pool = pool.clone();
//                     tokio::spawn(async move {
//                         run_connection(pool, stream, player_mgr, player_mgr_tx).await;
//                         drop(permit);
//                     });
//                     break;
//                 }
//                 Err(e) => {
//                     if backoff > 64 {
//                         error!("Failed to accept connection: {:?}", e);
//                         break;
//                     }
//                     warn!("Failed to accept connection: {:?}", e);
//                     tokio::time::sleep(Duration::from_secs(backoff)).await;
//                     backoff *= 2;
//                 }
//             }
//         }
//         tokio::task::yield_now().await;
//     }
// }

// async fn run_connection(
//     pool: Arc<Pool<Postgres>>,
//     mut stream: TcpStream,
//     player_mgr: Arc<PlayerMgr>,
//     player_mgr_tx: UnboundedSender<ToPlayerMgr>,
// ) {
//     let r = stream.ready(Interest::READABLE | Interest::WRITABLE).await.unwrap();
//     let ip_address = match stream.peer_addr() {
//         Ok(addr) => addr.ip().to_string(),
//         Err(_) => {
//             warn!("Failed to get peer address: {:?}", stream);
//             return;
//         }
//     };
//     warn!("Player connected via deprecated server: {:?}, {:?}", ip_address, r);
//     let s = stream.shutdown().await;
//     if let Err(e) = s {
//         warn!("Failed to shutdown stream: {:?}", e);
//     }
//     // let p = Player::new(player_mgr_tx, ip_address);
//     // let p = player_mgr.add_player(pool, p, stream).await;
//     // loop {
//     //     tokio::select! {
//     //         _ = tokio::time::sleep(Duration::from_secs(1)) => {
//     //             if !p.is_connected() {
//     //                 break;
//     //             }
//     //         }
//     //     }
//     // }
// }

pub struct PlayerMgr {
    players: Mutex<Vec<Arc<Player>>>,
    rx: Mutex<UnboundedReceiver<ToPlayerMgr>>,
    state: Mutex<PlayerMgrState>,
    pool: Arc<Pool<Postgres>>,
    limit_conns: Arc<Semaphore>,
}

impl PlayerMgr {
    pub fn new(pool: Arc<Pool<Postgres>>, limit_conns: Arc<Semaphore>) -> (Self, UnboundedSender<ToPlayerMgr>) {
        let (tx, rx) = unbounded_channel();
        (
            Self {
                players: Mutex::new(Vec::new()),
                rx: rx.into(),
                state: Mutex::new(PlayerMgrState::default()),
                pool,
                limit_conns,
            },
            tx,
        )
    }

    pub async fn add_player(&self, pool: Arc<Pool<Postgres>>, player: Player, stream: TcpStream) -> Arc<Player> {
        let player: Arc<_> = player.into();
        self.players.lock().await.push(player.clone());
        // Player::start(pool, player.clone(), stream);
        player
    }

    pub fn watch_remove_players_dc(mgr: Arc<Self>) {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let mut players = mgr.players.lock().await;
                // for i in (0..players.len()).rev() {
                //     if !players[i].is_connected() {
                //         players[i].init_shutdown();
                //         players.remove(i);
                //     }
                // }
            }
        });
    }

    pub fn start(mgr: Arc<Self>) {
        let orig_mgr = mgr;
        let (new_top3_tx, mut new_top3_rx) = unbounded_channel();

        // receive msgs loop
        let mgr = orig_mgr.clone();
        tokio::spawn(async move {
            let mut rx = mgr.rx.lock().await;
            loop {
                match rx.recv().await {
                    Some(msg) => match msg {
                        ToPlayerMgr::RecheckRecords() => {
                            Self::start_recheck_records(mgr.clone());
                        }
                        ToPlayerMgr::AuthFailed() => {
                            error!("Auth failed");
                        }
                        ToPlayerMgr::SocketError() => {
                            warn!("Socket error");
                        }
                        ToPlayerMgr::NewTop3() => {
                            info!("New top3");
                            let _ = new_top3_tx.send(());
                        }
                    },
                    None => {
                        break;
                    }
                }
            }
        });

        // watch for dead players loop
        let mgr = orig_mgr.clone();
        tokio::spawn(async move {
            // loop {
            //     tokio::time::sleep(Duration::from_millis(500)).await;
            //     let mut players = mgr.players.lock().await;
            //     for i in (0..players.len()).rev() {
            //         if !players[i].is_connected() {
            //             let _ = players[i].tx.send(ToPlayer::Shutdown());
            //             let p = players.remove(i);
            //             info!("Removing disconnected player: {:?}", p.display_name());
            //         }
            //     }
            // }
        });

        // update global stats
        let mgr = orig_mgr.clone();
        tokio::spawn(async move {
            loop {
                match update_global_overview(&mgr.pool).await {
                    Ok(_o) => {
                        // info!("Updated global overview: {:?}", _o);
                    }
                    Err(e) => {
                        error!("Error updating global overview: {:?}", e);
                    }
                }
                tokio::time::sleep(Duration::from_secs(7)).await;
            }
        });

        // update GFM donations
        let mgr = orig_mgr.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            loop {
                match get_and_update_donations_from_gfm(&mgr.pool).await {
                    Ok(_o) => {
                        // info!("Updated GFM donations: {:?}", _o);
                    }
                    Err(e) => {
                        error!("Error updating GFM donations: {:?}", e);
                    }
                }
                tokio::time::sleep(Duration::from_secs(60 * 5)).await;
            }
        });

        // update server stats
        let mgr = orig_mgr.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let nb_players_live = mgr.players.lock().await.len();
                info!("Updating server(v1) stats: {:?}", nb_players_live);
                match update_server_stats(&mgr.pool, nb_players_live as i32).await {
                    Ok(_o) => {}
                    Err(e) => {
                        error!("Error updating server stats: {:?}", e);
                    }
                };
            }
        });

        let mgr = orig_mgr.clone();
        tokio::spawn(async move {
            loop {
                info!("Updating donations");
                match update_donations(&mgr.pool).await {
                    Ok(r) => {
                        info!("Updated donations: {:?}", r);
                    }
                    Err(e) => {
                        error!("Error updating donations: {:?}", e);
                    }
                }
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });

        // // send top3 to all players every 2 min
        // let mgr = orig_mgr.clone();
        // tokio::spawn(async move {
        //     loop {
        //         if let Ok(r) = get_global_lb(&mgr.pool, 1, 11).await {
        //             let top3 = r.into_iter().map(|r| r.into()).collect::<Vec<LeaderboardEntry>>();
        //             let top3 = router::Response::Top3 { top3 };
        //             let nb_players_live = get_server_info(&mgr.pool).await.unwrap_or_default();
        //             let server_info = router::Response::ServerInfo { nb_players_live };
        //             info!("Sending top3 to all players: {:?}", &top3);
        //             // let top3 = serde_json::to_string(&top3).unwrap();
        //             let players = mgr.players.lock().await;
        //             for p in players.iter() {
        //                 let _ = p.queue_tx.send(top3.clone());
        //                 let _ = p.queue_tx.send(server_info.clone());
        //             }
        //         };
        //         tokio::select! {
        //             _ = tokio::time::sleep(Duration::from_secs(120)) => {},
        //             _ = new_top3_rx.recv() => {},
        //         }
        //     }
        // });
    }

    pub fn start_recheck_records(mgr: Arc<Self>) {
        tokio::spawn(async move {
            // let mut state = mgr.state.lock().await;
            // let now = SystemTime::now();
            // if let Some(last_check) = state.last_record_check {
            //     if now.duration_since(last_check).unwrap() < Duration::from_secs(60) {
            //         return;
            //     }
            // }
            // state.last_record_check = Some(now);
            // info!("Rechecking records");
        });
    }
}

#[derive(Default)]
pub struct PlayerMgrState {
    last_record_check: Option<SystemTime>,
}

pub async fn update_donations(pool: &Pool<Postgres>) -> Result<(), Box<dyn std::error::Error>> {
    let donations = get_donations_from_matcherino().await?;
    let new_donations = update_donations_in_db(pool, &donations).await?;
    Ok(())
}
