use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use dotenv::dotenv;
use env_logger::Env;
use log::{error, info, warn};
use player::Player;
use queries::{get_global_lb, update_global_overview, update_server_stats};
use serde_json;
use sqlx::{
    postgres::{PgPool, PgPoolOptions},
    Pool, Postgres,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, Interest},
    net::{TcpListener, TcpStream},
    sync::mpsc::{unbounded_channel, Receiver, Sender, UnboundedReceiver, UnboundedSender},
    sync::Mutex,
};

use crate::{
    consts::DD2_MAP_UID, http::run_http_server, op_auth::init_op_config, player::ToPlayer, queries::get_server_info,
    router::LeaderboardEntry,
};

mod api_error;
mod consts;
mod db;
mod http;
mod op_auth;
mod player;
mod queries;
mod router;

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
    tokio::select! {
        _ = run_http_server(db.clone()) => {},
        _ = listen(bind_addr, db.clone()) => {},
    };
}

async fn listen(bind_addr: &str, pool: Arc<Pool<Postgres>>) {
    let listener = TcpListener::bind(bind_addr).await.unwrap();
    info!("Listening on: {}", bind_addr);
    let (player_mgr, player_mgr_tx) = PlayerMgr::new(pool.clone());
    let player_mgr = Arc::new(player_mgr);
    PlayerMgr::start(player_mgr.clone());
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                info!("New connection from {:?}", stream.peer_addr().unwrap());
                let player_mgr = player_mgr.clone();
                let player_mgr_tx = player_mgr_tx.clone();
                let pool = pool.clone();
                tokio::spawn(async move {
                    run_connection(pool, stream, player_mgr, player_mgr_tx).await;
                });
            }
            Err(e) => {
                warn!("Failed to accept connection: {:?}", e);
            }
        }
        tokio::task::yield_now().await;
    }
}

async fn run_connection(
    pool: Arc<Pool<Postgres>>,
    stream: TcpStream,
    player_mgr: Arc<PlayerMgr>,
    player_mgr_tx: UnboundedSender<ToPlayerMgr>,
) {
    let r = stream.ready(Interest::READABLE | Interest::WRITABLE).await.unwrap();
    let ip_address = match stream.peer_addr() {
        Ok(addr) => addr.ip().to_string(),
        Err(_) => {
            warn!("Failed to get peer address: {:?}", stream);
            return;
        }
    };
    info!("Ready: {:?}", r);
    let p = Player::new(player_mgr_tx, ip_address);
    player_mgr.add_player(pool, p, stream).await;
}

pub struct PlayerMgr {
    players: Mutex<Vec<Arc<Player>>>,
    rx: Mutex<UnboundedReceiver<ToPlayerMgr>>,
    state: Mutex<PlayerMgrState>,
    pool: Arc<Pool<Postgres>>,
}

impl PlayerMgr {
    pub fn new(pool: Arc<Pool<Postgres>>) -> (Self, UnboundedSender<ToPlayerMgr>) {
        let (tx, rx) = unbounded_channel();
        (
            Self {
                players: Mutex::new(Vec::new()),
                rx: rx.into(),
                state: Mutex::new(PlayerMgrState::default()),
                pool,
            },
            tx,
        )
    }

    pub async fn add_player(&self, pool: Arc<Pool<Postgres>>, player: Player, stream: TcpStream) {
        let player: Arc<_> = player.into();
        self.players.lock().await.push(player.clone());
        Player::start(pool, player, stream);
    }

    pub fn watch_remove_players_dc(mgr: Arc<Self>) {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let mut players = mgr.players.lock().await;
                for i in (0..players.len()).rev() {
                    if !players[i].is_connected() {
                        players.remove(i);
                    }
                }
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
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let mut players = mgr.players.lock().await;
                for i in (0..players.len()).rev() {
                    if !players[i].is_connected() {
                        let p = players.remove(i);
                        info!("Removing disconnected player: {:?}", p.display_name());
                    }
                }
            }
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
                tokio::time::sleep(Duration::from_secs(15)).await;
            }
        });

        // update server stats
        let mgr = orig_mgr.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let nb_players_live = mgr.players.lock().await.len();
                match update_server_stats(&mgr.pool, nb_players_live).await {
                    Ok(_o) => {}
                    Err(e) => {
                        error!("Error updating server stats: {:?}", e);
                    }
                };
            }
        });

        // send top3 to all players every 2 min
        let mgr = orig_mgr.clone();
        tokio::spawn(async move {
            loop {
                if let Ok(r) = get_global_lb(&mgr.pool, 1, 6).await {
                    let top3 = r.into_iter().map(|r| r.into()).collect::<Vec<LeaderboardEntry>>();
                    let top3 = router::Response::Top3 { top3 };
                    let nb_players_live = get_server_info(&mgr.pool).await.unwrap_or_default();
                    let server_info = router::Response::ServerInfo { nb_players_live };
                    info!("Sending top3 to all players: {:?}", &top3);
                    // let top3 = serde_json::to_string(&top3).unwrap();
                    let players = mgr.players.lock().await;
                    for p in players.iter() {
                        let _ = p.queue_tx.send(top3.clone());
                        let _ = p.queue_tx.send(server_info.clone());
                    }
                };
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(120)) => {},
                    _ = new_top3_rx.recv() => {},
                }
            }
        });
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

pub enum ToPlayerMgr {
    RecheckRecords(),
    AuthFailed(),
    SocketError(),
    NewTop3(),
}
