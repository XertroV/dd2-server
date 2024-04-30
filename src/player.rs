use std::str::FromStr;
use std::sync::Arc;

use log::{info, warn};
use sqlx::types::Uuid;
use sqlx::{query, Pool, Postgres};
use tokio::io::AsyncWriteExt;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{error::SendError, unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::{Mutex, OnceCell};

use crate::op_auth::{check_token, TokenResp};
use crate::queries::{
    context_mark_succeeded, create_session, insert_context, insert_context_packed, register_or_login, resume_session, update_fall_with_end,
    update_users_stats, Session, User,
};
use crate::router::{write_response, PlayerCtx, Request, Response, Stats};
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
    context: Mutex<Option<PlayerCtx>>,
    context_id: Mutex<Option<Uuid>>,
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
            context: Mutex::new(None),
            context_id: Mutex::new(None),
        }
    }

    pub fn get_session(&self) -> &LoginSession {
        self.session.get().unwrap()
    }

    pub fn is_connected(&self) -> bool {
        self.has_shutdown.get().is_none()
    }

    pub fn start(pool: Arc<Pool<Postgres>>, p: Arc<Player>, stream: TcpStream) {
        tokio::spawn(async move {
            let (mut s_read, mut s_write) = stream.into_split();
            // auth player first
            let ls = match Player::run_auth(pool.clone(), p.clone(), &mut s_read).await {
                Ok(ls) => ls,
                Err(err) => {
                    let err = match err {
                        Error::StrErr(err) => err,
                        Error::SqlxErr(err) => format!("DB Error: {:?}", err),
                        _ => format!("{:?}", err),
                    };
                    // notify mgr to remove player
                    warn!("Error authenticating player: {:?}", err);
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
                    session_token: ls.session_id().to_string(),
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
            info!("Player {} authenticated / resumed: {}", ls.display_name(), ls.resumed);
            p.session.set(ls).unwrap();

            // required loops: read player socket, write player socket, read incoming msgs
            // clone tx_mgr for where it's required
            // if passes, start loops
            // otherwise notify mgr to remove player
            Player::loop_read(pool.clone(), p.clone(), s_read);
            Player::loop_write(p.clone(), s_write);
            Player::loop_from_mgr(p.clone());
        });
    }

    pub async fn run_auth(pool: Arc<Pool<Postgres>>, p: Arc<Player>, s_read: &mut OwnedReadHalf) -> Result<LoginSession, Error> {
        // let mut stream = p.stream.lock().await;
        let msg = Request::read_from_socket(s_read).await;
        let msg = match msg {
            Ok(msg) => msg,
            Err(err) => {
                return Err(format!("Error reading from socket: {:?}", err).into());
            }
        };
        match msg {
            Request::Authenticate {
                token,
                plugin_info,
                game_info,
                gamer_info,
            } => Player::login_via_token(pool, p.clone(), token, plugin_info, game_info, gamer_info).await,
            Request::ResumeSession {
                session_token,
                plugin_info,
                game_info,
                gamer_info,
            } => Player::login_via_session(pool, p.clone(), session_token, plugin_info, game_info, gamer_info).await,
            _ => {
                return Err(format!("Invalid request {:?}", msg).into());
            }
        }
    }

    pub async fn login_via_token(
        pool: Arc<Pool<Postgres>>,
        p: Arc<Player>,
        token: String,
        plugin_info: String,
        game_info: String,
        gamer_info: String,
    ) -> Result<LoginSession, Error> {
        let token_resp = check_token(&token, 519).await;
        let token_resp = match token_resp {
            Some(token_resp) => token_resp,
            None => {
                return Err("Invalid token".to_string().into());
            }
        };
        let wsid = match Uuid::from_str(&token_resp.account_id) {
            Ok(wsid) => wsid,
            Err(err) => {
                return Err(format!("Invalid account_id: {:?}", err).into());
            }
        };
        let user = register_or_login(&pool, &wsid, &token_resp.display_name).await?;
        let session = create_session(&pool, user.id(), &plugin_info, &game_info, &gamer_info).await?;
        Ok(LoginSession {
            user,
            session,
            resumed: false,
        })
    }

    pub async fn login_via_session(
        pool: Arc<Pool<Postgres>>,
        p: Arc<Player>,
        session_token: String,
        plugin_info: String,
        game_info: String,
        gamer_info: String,
    ) -> Result<LoginSession, Error> {
        let session_token = match Uuid::from_str(&session_token) {
            Ok(session_token) => session_token,
            Err(err) => {
                return Err(format!("Invalid session_token: {:?}", err).into());
            }
        };
        // check for sesison and resume
        let (s, u) = resume_session(&pool, &session_token, &plugin_info, &game_info, &gamer_info).await?;
        Ok(LoginSession {
            user: u,
            session: s,
            resumed: true,
        })
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

    pub fn loop_read(pool: Arc<Pool<Postgres>>, p: Arc<Player>, mut s_read: OwnedReadHalf) {
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
                    Request::Authenticate { .. } => Ok(()),  // ignore
                    Request::ResumeSession { .. } => Ok(()), // ignore
                    Request::ReportContext { context } => Player::update_context(&pool, p.clone(), context).await,
                    Request::ReportGameCamNod { aux } => todo!(),
                    Request::Ping {} => p.queue_tx.send(Response::Ping {}).map_err(Into::into),
                    Request::ReportVehicleState { mat, vel } => todo!(),
                    Request::ReportRespawn { game_time } => todo!(),
                    Request::ReportFinish { game_time } => todo!(),
                    Request::ReportFallStart {
                        floor,
                        pos,
                        speed,
                        start_time,
                    } => Player::on_report_fall_start(&pool, p.clone(), floor as i32, pos.into(), speed, start_time as i32).await,
                    Request::ReportFallEnd { floor, pos, end_time } => {
                        Player::on_report_fall_end(&pool, p.clone(), floor as i32, pos.into(), end_time as i32).await
                    }
                    Request::ReportStats { stats } => Player::on_report_stats(pool.clone(), p.clone(), stats).await,
                    Request::ReportMapLoad {} => todo!(),
                    Request::GetMyStats {} => todo!(),
                    Request::GetGlobalLB {} => todo!(),
                    Request::GetFriendsLB { friends } => todo!(),
                    Request::StressMe {} => (0..100)
                        .map(|_| p.queue_tx.send(Response::Ping {}))
                        .collect::<Result<_, _>>()
                        .map_err(Into::into),
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

/// handle messages impls
impl Player {
    pub async fn update_context(pool: &Pool<Postgres>, p: Arc<Player>, context: PlayerCtx) -> Result<(), Error> {
        let mut ctx = p.context.lock().await;
        let ctx_id = p.context_id.lock().await;
        let mut previous_ctx = None;
        if let Some(ctx_id) = ctx_id.as_ref() {
            let _ = previous_ctx.insert(ctx_id.clone());
        }
        let map_id: i32 = todo!();
        let new_id = insert_context_packed(pool, &context, &previous_ctx, map_id).await?;
        if let Some(previous_ctx) = previous_ctx {
            context_mark_succeeded(pool, &previous_ctx, &new_id).await?;
        }
        *ctx = Some(context);
        *ctx_id = Some(new_id);
        Ok(())
    }

    pub async fn on_report_stats(pool: Arc<Pool<Postgres>>, p: Arc<Player>, stats: Stats) -> Result<(), Error> {
        Ok(update_users_stats(&pool, p.get_session().user_id(), &stats).await?)
    }

    pub async fn on_report_fall_start(
        pool: &Pool<Postgres>,
        p: Arc<Player>,
        floor: i32,
        pos: (f32, f32, f32),
        speed: f32,
        start_time: i32,
    ) -> Result<(), Error> {
        // insert
        query!(
            "INSERT INTO falls (session_token, user_id, start_floor, start_pos_x, start_pos_y, start_pos_z, start_speed, start_time) VALUES ($1, $2, $3, $4, $5, $6, $7, $8);",
            p.get_session().session_id(),
            p.get_session().user_id(),
            floor,
            pos.0 as f64,
            pos.1 as f64,
            pos.2 as f64,
            speed as f64,
            start_time,
        ).execute(pool).await?;
        Ok(())
    }

    pub async fn on_report_fall_end(
        pool: &Pool<Postgres>,
        p: Arc<Player>,
        floor: i32,
        pos: (f32, f32, f32),
        end_time: i32,
    ) -> Result<(), Error> {
        update_fall_with_end(pool, p.get_session().user_id(), floor, pos, end_time).await?;
        Ok(())
    }
}

pub enum ToPlayer {
    NotifyRecord(),
}

#[derive(Debug, Clone)]
pub struct LoginSession {
    pub user: User,
    pub session: Session,
    pub resumed: bool,
}
impl LoginSession {
    pub fn display_name(&self) -> &str {
        &self.user.display_name
    }
    pub fn user_id(&self) -> &Uuid {
        &self.user.web_services_user_id
    }
    pub fn session_id(&self) -> &Uuid {
        &self.session.session_token
    }
}

#[derive(Debug)]
pub enum Error {
    StrErr(String),
    SqlxErr(sqlx::Error),
    SendErr(SendError<Response>),
}

impl From<String> for Error {
    fn from(s: String) -> Self {
        Error::StrErr(s)
    }
}

impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Self {
        Error::SqlxErr(e)
    }
}

impl From<SendError<Response>> for Error {
    fn from(e: SendError<Response>) -> Self {
        Error::SendErr(e)
    }
}
