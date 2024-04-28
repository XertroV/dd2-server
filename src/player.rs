use std::borrow::BorrowMut;
use std::sync::Arc;

use log::{info, warn};
use tokio::io::AsyncWriteExt;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf, ReadHalf};
use tokio::net::TcpStream;
use tokio::stream;
use tokio::sync::{Mutex, OnceCell};

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::op_auth::{check_token, TokenResp};
use crate::router::{write_response, Request, Response};
use crate::ToPlayerMgr;

pub struct Player {
    pub tx: UnboundedSender<ToPlayer>,
    tx_mgr: UnboundedSender<ToPlayerMgr>,
    rx: Mutex<UnboundedReceiver<ToPlayer>>,
    queue_rx: Mutex<UnboundedReceiver<Response>>,
    queue_tx: UnboundedSender<Response>,
    // stream: Arc<Mutex<TcpStream>>,
    has_shutdown: OnceCell<bool>,
    session: OnceCell<LoginSession>,
}

impl Player {
    pub fn new(tx_mgr: UnboundedSender<ToPlayerMgr>) -> Self {
        let (tx, rx) = unbounded_channel();
        let (queue_tx, queue_rx) = unbounded_channel();
        Player {
            tx,
            tx_mgr,
            rx: rx.into(),
            queue_rx: queue_rx.into(),
            queue_tx,
            // stream: Arc::new(stream.into()),
            has_shutdown: OnceCell::new(),
            session: OnceCell::new(),
        }
    }

    pub fn start(p: Arc<Player>, stream: TcpStream) {
        tokio::spawn(async move {
            let (mut s_read, mut s_write) = stream.into_split();
            // auth player first
            let ls = match Player::run_auth(p.clone(), &mut s_read).await {
                Ok(ls) => ls,
                Err(err) => {
                    // notify mgr to remove player
                    let _ = write_response(&mut s_write, Response::AuthFail { err }).await;
                    let _ = p.has_shutdown.set(true);
                    let _ = p.tx_mgr.send(ToPlayerMgr::AuthFailed());
                    let _ = s_write.shutdown().await;
                    return;
                }
            };
            match write_response(
                &mut s_write,
                Response::AuthSuccess {
                    session_token: ls.session_token.clone(),
                },
            )
            .await
            {
                Ok(_) => {}
                Err(err) => {
                    warn!("Error writing to socket: {:?}", err);
                    let _ = p.has_shutdown.set(true);
                    let _ = s_write.shutdown().await;
                    return;
                }
            };
            info!("Player {} authenticated", ls.display_name);
            p.session.set(ls).unwrap();

            // required loops: read player socket, write player socket, read incoming msgs
            // clone tx_mgr for where it's required
            // if passes, start loops
            // otherwise notify mgr to remove player
            Player::loop_read(p.clone(), s_read);
            Player::loop_write(p.clone(), s_write);
            Player::loop_from_mgr(p.clone());
        });
    }

    pub async fn run_auth(p: Arc<Player>, s_read: &mut OwnedReadHalf) -> Result<LoginSession, String> {
        // let mut stream = p.stream.lock().await;
        let msg = Request::read_from_socket(s_read).await;
        let msg = match msg {
            Ok(msg) => msg,
            Err(err) => {
                return Err(format!("Error reading from socket: {:?}", err));
            }
        };
        match msg {
            Request::Authenticate {
                token,
                plugin_info,
                game_info,
            } => Player::login_via_token(p.clone(), token, plugin_info, game_info).await,
            Request::ResumeSession {
                session_token,
                plugin_info,
                game_info,
            } => Player::login_via_session(p.clone(), session_token, plugin_info, game_info).await,
            _ => {
                return Err("Invalid request".to_string());
            }
        }
    }

    pub async fn login_via_token(p: Arc<Player>, token: String, plugin_info: String, game_info: String) -> Result<LoginSession, String> {
        let token_resp = check_token(&token, 519).await;
        let token_resp = match token_resp {
            Some(token_resp) => token_resp,
            None => {
                return Err("Invalid token".to_string());
            }
        };
        // todo: check DB for registration
        // check_registration(&token_resp).await?;
        // get_new_session(&token_resp, plugin_info, game_info).await
        // todo!()
        let TokenResp {
            account_id,
            display_name,
            token_time,
        } = token_resp;
        Ok(LoginSession {
            account_id,
            session_token: "asdf".into(),
            display_name,
        })
    }

    pub async fn login_via_session(
        p: Arc<Player>,
        session_token: String,
        plugin_info: String,
        game_info: String,
    ) -> Result<LoginSession, String> {
        // check for sesison and resume
        todo!()
    }

    pub fn loop_from_mgr(p: Arc<Player>) {
        tokio::spawn(async move {
            let tx_mgr = p.tx_mgr.clone();
            let mut rx = p.rx.lock().await;
            loop {
                match rx.recv().await {
                    Some(msg) => match msg {
                        ToPlayer::NotifyRecord() => {
                            // notify record
                        }
                    },
                    None => {
                        break;
                    }
                }
            }
        });
    }

    pub fn loop_read(p: Arc<Player>, mut s_read: OwnedReadHalf) {
        tokio::spawn(async move {
            let tx_mgr = p.tx_mgr.clone();
            while !p.has_shutdown.initialized() {
                let msg = Request::read_from_socket(&mut s_read).await;
                let msg = match msg {
                    Ok(msg) => msg,
                    Err(err) => {
                        warn!("Error reading from socket: {:?}", err);
                        // notify mgr to remove player
                        let _ = p.tx_mgr.send(ToPlayerMgr::AuthFailed());
                        let _ = p.has_shutdown.set(true);
                        return;
                    }
                };
                let res = match msg {
                    Request::Authenticate {
                        token,
                        plugin_info,
                        game_info,
                    } => todo!(),
                    Request::ResumeSession {
                        session_token,
                        plugin_info,
                        game_info,
                    } => todo!(),
                    Request::ReportContext { context } => todo!(),
                    Request::ReportGameCamNod { aux } => todo!(),
                    Request::Ping {} => p.queue_tx.send(Response::Ping {}),
                    Request::ReportVehicleState { mat, vel } => todo!(),
                    Request::ReportRespawn { game_time } => todo!(),
                    Request::ReportFinish { game_time } => todo!(),
                    Request::ReportFallStart {
                        floor,
                        pos,
                        vel,
                        start_time,
                    } => todo!(),
                    Request::ReportFallEnd { floor, pos, end_time } => todo!(),
                    Request::ReportStats { stats } => todo!(),
                    Request::GetMyStats {} => todo!(),
                    Request::GetGlobalLB {} => todo!(),
                    Request::GetFriendsLB { friends } => todo!(),
                    Request::StressMe {} => todo!(),
                };
                match res {
                    Ok(_) => {}
                    Err(err) => {
                        warn!("Error sending response: {:?}", err);
                    }
                }
            }
        });
    }

    pub fn loop_write(p: Arc<Player>, mut s_write: OwnedWriteHalf) {
        tokio::spawn(async move {
            let mut queue_rx = p.queue_rx.lock().await;
            while !p.has_shutdown.initialized() {
                let res = queue_rx.recv().await;
                let res = match res {
                    Some(res) => res,
                    None => {
                        break;
                    }
                };
                let res = write_response(&mut s_write, res).await;
                match res {
                    Ok(_) => {}
                    Err(err) => {
                        warn!("Error writing to socket: {:?}", err);
                    }
                }
            }
            let _ = s_write.shutdown().await;
        });
    }
}

pub enum ToPlayer {
    NotifyRecord(),
}

#[derive(Debug, Clone)]
pub struct LoginSession {
    pub account_id: String,
    pub session_token: String,
    pub display_name: String,
}
