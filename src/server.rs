use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use env_logger::Env;
use log::{info, warn};
use player::Player;
use serde_json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, Interest},
    net::{TcpListener, TcpStream},
    sync::mpsc::{unbounded_channel, Receiver, Sender, UnboundedReceiver, UnboundedSender},
    sync::Mutex,
};

use crate::op_auth::init_op_config;

mod op_auth;
mod player;
mod router;

#[tokio::main(flavor = "multi_thread", worker_threads = 10)]
async fn main() {
    // let rec = router::Response::NewRecord { name: "asdf".into(), height: 1234., testu64: 0x7ff7320b0000 };
    // // test: serialize using serde_json, print, and deserialize
    // let s = serde_json::to_string(&rec).unwrap();
    // println!("Serialized: {}", s);
    // let rec2: router::Response = serde_json::from_str(&s).unwrap();
    // println!("Deserialized: {:?}", rec2);
    // return;

    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();
    init_op_config().await;
    let bind_addr = "0.0.0.0:17677";
    warn!("Starting server on: {}", bind_addr);
    listen(bind_addr).await;
}

async fn listen(bind_addr: &str) {
    let listener = TcpListener::bind(bind_addr).await.unwrap();
    info!("Listening on: {}", bind_addr);
    let (player_mgr, player_mgr_tx) = PlayerMgr::new();
    let player_mgr = Arc::new(player_mgr);
    PlayerMgr::start(player_mgr.clone());
    loop {
        let (stream, _) = listener.accept().await.unwrap();
        let player_mgr = player_mgr.clone();
        let player_mgr_tx = player_mgr_tx.clone();
        tokio::spawn(async move {
            run_connection(stream, player_mgr, player_mgr_tx).await;
        });
    }
}

async fn run_connection(stream: TcpStream, player_mgr: Arc<PlayerMgr>, player_mgr_tx: UnboundedSender<ToPlayerMgr>) {
    info!("New connection from {:?}", stream.peer_addr().unwrap());
    let r = stream.ready(Interest::READABLE | Interest::WRITABLE).await.unwrap();
    info!("Ready: {:?}", r);
    let p = Player::new(player_mgr_tx);
    player_mgr.add_player(p, stream).await;
}

pub struct PlayerMgr {
    players: Mutex<Vec<Arc<Player>>>,
    rx: Mutex<UnboundedReceiver<ToPlayerMgr>>,
    state: Mutex<PlayerMgrState>,
}

impl PlayerMgr {
    pub fn new() -> (Self, UnboundedSender<ToPlayerMgr>) {
        let (tx, rx) = unbounded_channel();
        (
            Self {
                players: Mutex::new(Vec::new()),
                rx: rx.into(),
                state: Mutex::new(PlayerMgrState::default()),
            },
            tx,
        )
    }

    pub async fn add_player(&self, player: Player, stream: TcpStream) {
        let player: Arc<_> = player.into();
        self.players.lock().await.push(player.clone());
        Player::start(player, stream);
    }

    pub fn start(mgr: Arc<Self>) {
        tokio::spawn(async move {
            let mut rx = mgr.rx.lock().await;
            loop {
                match rx.recv().await {
                    Some(msg) => match msg {
                        ToPlayerMgr::RecheckRecords() => {
                            Self::start_recheck_records(mgr.clone());
                        }
                        ToPlayerMgr::AuthFailed() => todo!(),
                    },
                    None => {
                        break;
                    }
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
}
