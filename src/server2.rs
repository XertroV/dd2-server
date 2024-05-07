use api_error::Error as ApiError;
use base64::Engine;
use dotenv::dotenv;
use env_logger::Env;
use log::{debug, error, info, warn};
use miette::{Diagnostic, SourceSpan};
use op_auth::check_token;
use player::{parse_u64_str, LoginSession, Player};
use queries::{
    context_mark_succeeded, create_session, get_global_lb, get_global_overview, get_user_in_lb, get_user_stats, insert_context_packed,
    insert_finish, insert_gc_nod, insert_respawn, insert_start_fall, insert_vehicle_state, register_or_login, resume_session,
    update_fall_with_end, update_global_overview, update_server_stats, update_users_stats, PBUpdateRes,
};
use router::{Map, PlayerCtx, Request, Response, Stats, ToPlayerMgr};
use serde_json;
use sqlx::{
    postgres::{PgPool, PgPoolOptions},
    Pool, Postgres,
};
use std::{
    error::Error,
    net::SocketAddr,
    str::FromStr,
    sync::Arc,
    time::{Duration, SystemTime},
};
use thiserror::Error;
use tokio::{
    io::{self, AsyncReadExt, AsyncWriteExt, Interest},
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpListener, TcpStream,
    },
    select,
    sync::{
        mpsc::{unbounded_channel, Receiver, Sender, UnboundedReceiver, UnboundedSender},
        Mutex, OnceCell, OwnedSemaphorePermit, Semaphore,
    },
};
use tokio_graceful_shutdown::{FutureExt, SubsystemBuilder, SubsystemHandle, Toplevel};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use uuid::Uuid;

use crate::{
    consts::DD2_MAP_UID,
    http::run_http_server,
    op_auth::init_op_config,
    player::{check_flags_sf_mi, ToPlayer},
    queries::{get_server_info, update_user_pb_height},
    router::{write_response, LeaderboardEntry},
};

mod api_error;
mod consts;
mod db;
mod http;
mod op_auth;
mod player;
mod queries;
mod router;

const MAX_CONNECTIONS: u32 = 2048;

#[tokio::main(flavor = "multi_thread", worker_threads = 10)]
async fn main() -> Result<(), Box<dyn Error>> {
    dotenv().ok();
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();
    info!("Starting DD2 Server for UID: {}", DD2_MAP_UID);

    Toplevel::new(|s| async move {
        s.start(SubsystemBuilder::new("AcceptConns", accept_conns));
    })
    .catch_signals()
    .handle_shutdown_requests(Duration::from_millis(1000))
    .await
    .map_err(Into::into)
}

async fn accept_conns(subsys: SubsystemHandle) -> miette::Result<()> {
    // init db from env var
    let db_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let pool = PgPoolOptions::new().max_connections(10).connect(&db_url).await.unwrap();
    let db = Arc::new(pool);
    info!("Connected to db");

    match sqlx::migrate!().run(db.as_ref()).await {
        Ok(_) => info!("Migrations ran successfully"),
        Err(e) => panic!("Error running migrations: {:?}", e),
    }
    init_op_config().await;

    let bind_addr = "0.0.0.0:17677";
    warn!("Starting server on: {}", bind_addr);
    let listener = TcpListener::bind(bind_addr).await.unwrap();
    let limit_conns = Arc::new(Semaphore::new(MAX_CONNECTIONS as usize));
    subsys.start(SubsystemBuilder::new("PlayerMgr", |a| {
        PlayerMgr::new(db, limit_conns, listener).run(a)
    }));

    // listener
    Ok(())
}

pub struct PlayerMgr {
    // players: Mutex<Vec<Arc<Player>>>,
    players: Arc<Mutex<Vec<UnboundedSender<ToPlayer>>>>,
    rx: UnboundedReceiver<ToPlayerMgr>,
    tx: UnboundedSender<ToPlayerMgr>,
    // state: Mutex<PlayerMgrState>,
    pool: Arc<Pool<Postgres>>,
    limit_conns: Arc<Semaphore>,
    listener: TcpListener,
}

impl PlayerMgr {
    pub fn new(pool: Arc<Pool<Postgres>>, limit_conns: Arc<Semaphore>, mut listener: TcpListener) -> Self {
        let (tx, rx) = unbounded_channel();
        Self {
            players: Mutex::new(Vec::new()).into(),
            /// send ToPlayerMgr msgs
            tx,
            rx,
            // state: Mutex::new(PlayerMgrState::default()),
            /// the db
            pool,
            limit_conns,
            listener,
        }
    }

    async fn top_3_loop(
        pool: Arc<Pool<Postgres>>,
        players: Arc<Mutex<Vec<UnboundedSender<ToPlayer>>>>,
        subsys: SubsystemHandle,
    ) -> miette::Result<()> {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(15)) => {},
                _ = subsys.on_shutdown_requested() => {
                    break;
                }
            };

            let r = get_global_lb(&pool, 1, 6).await;
            let r = match r {
                Ok(r) => r,
                Err(e) => {
                    warn!("Error getting top 3: {:?}", e);
                    continue;
                }
            };
            let top3: Vec<LeaderboardEntry> = r.into_iter().map(Into::into).collect();
            let mut ps = players.lock().await;
            let to_rem: Vec<_> = ps
                .iter()
                .enumerate()
                .filter_map(|(i, p)| if p.is_closed() { Some(i) } else { None })
                .collect();

            to_rem.iter().rev().for_each(|i| {
                ps.remove(*i);
            });
            let nb_players = ps.len();
            let _ = update_server_stats(&pool, nb_players as i32).await;
            let nb_players_live = get_server_info(&pool).await.unwrap_or_default();
            let server_info = router::Response::ServerInfo { nb_players_live };
            ps.iter()
                .map(|p| {
                    let _ = p.send(ToPlayer::Top3(top3.clone()));
                    let _ = p.send(ToPlayer::Send(server_info.clone()));
                })
                .for_each(drop);
            drop(ps);
        }
        Ok(())
    }

    pub async fn run(mut self, subsys: SubsystemHandle) -> miette::Result<()> {
        let cancel_t = subsys.create_cancellation_token();
        let pool = self.pool.clone();
        let players = self.players.clone();
        subsys.start(SubsystemBuilder::new("PlayerMgr_TellTop3", |a| {
            PlayerMgr::top_3_loop(pool, players, a)
        }));
        let cancel_t = subsys.create_cancellation_token();
        // let conn_tracker = TaskTracker::new();
        loop {
            let pool = self.pool.clone();
            let tx = self.tx.clone();
            tokio::select! {
                _ = cancel_t.cancelled() => {
                    info!("PlayerMgr shutdown requested");
                    let mut ps = self.players.lock().await;
                    ps.iter().map(|p| p.send(ToPlayer::Shutdown())).for_each(drop);
                    ps.clear();
                    break;
                },
                r = self.accept(&subsys) => {
                    let r = match r {
                        Ok(r) => r,
                        Err(e) => {
                            subsys.request_shutdown();
                            return Err(PlayerMgrErr::IoError(e).into());
                        },
                    };
                    let (stream, permit, addr) = r;
                    let (p_tx, p_rx) = unbounded_channel();
                    subsys.start(SubsystemBuilder::new("Player", {
                        move |a| XPlayer::new(stream, permit, addr, pool, tx, p_rx).run(a)
                    }));
                    self.players.lock().await.push(p_tx);
                    // let player = Player::new(stream, permit, self.tx.clone());
                },
            }
        }
        subsys.wait_for_children().await;
        info!("PlayerMgr shutting down");
        Ok(())
    }

    // pub async fn run_server(mut self, subsys: SubsystemHandle) -> Result<(), Box<dyn Error>> {
    //     loop {
    //         let (stream, permit) = self.accept().await?;
    //     }
    //     Ok(())
    // }

    async fn accept(&mut self, subsys: &SubsystemHandle) -> io::Result<(TcpStream, OwnedSemaphorePermit, SocketAddr)> {
        let mut backoff = 1;
        let permit = self.limit_conns.clone().acquire_owned().await.unwrap();
        loop {
            match self.listener.accept().cancel_on_shutdown(subsys).await {
                Ok(r) => match r {
                    Ok((stream, remote)) => {
                        return Ok((stream, permit, remote));
                    }
                    Err(e) => {
                        if backoff > 64 {
                            error!("Error accepting connection: {:?}", e);
                            return Err(e);
                        }
                        error!("Error accepting connection: {:?}", e);
                        tokio::time::sleep(Duration::from_secs(backoff)).await;
                        backoff *= 2;
                    }
                },
                Err(e) => {
                    break;
                }
            }
        }
        return Err(io::Error::new(io::ErrorKind::Other, "Shutdown requested"));
    }
}

#[derive(Error, Diagnostic, Debug)]
pub enum PlayerMgrErr {
    #[error(transparent)]
    #[diagnostic(code(my_lib::io_error))]
    IoError(#[from] std::io::Error),
    // #[error("Oops it blew up")]
    // #[diagnostic(code(my_lib::bad_code))]
    // BadThingHappened,
    // #[error(transparent)]
    // // Use `#[diagnostic(transparent)]` to wrap another [`Diagnostic`]. You won't see labels otherwise
    // #[diagnostic(transparent)]
    // AnotherError(#[from] AnotherError),
}

pub struct XPlayer {
    stream: Option<TcpStream>,
    permit: OwnedSemaphorePermit,
    pool: Arc<Pool<Postgres>>,
    // to manager
    mgr_tx: UnboundedSender<ToPlayerMgr>,
    addr: SocketAddr,
    // from manager
    p_rx: Option<UnboundedReceiver<ToPlayer>>,

    session: OnceCell<LoginSession>,
    context: Mutex<Option<PlayerCtx>>,
    context_id: Mutex<Option<Uuid>>,
    ip_address: String,

    queue_rx: Option<UnboundedReceiver<Response>>,
    pub queue_tx: UnboundedSender<Response>,
}

impl XPlayer {
    pub fn new(
        stream: TcpStream,
        permit: OwnedSemaphorePermit,
        addr: SocketAddr,
        pool: Arc<Pool<Postgres>>,
        mgr_tx: UnboundedSender<ToPlayerMgr>,
        p_rx: UnboundedReceiver<ToPlayer>,
    ) -> Self {
        info!("New player connected: {}", addr.ip().to_string());
        let (queue_tx, queue_rx) = unbounded_channel();
        Self {
            stream: Some(stream),
            permit,
            pool,
            mgr_tx,
            addr,
            p_rx: Some(p_rx),
            session: Default::default(),
            context: Default::default(),
            context_id: Default::default(),
            ip_address: addr.ip().to_string(),
            queue_rx: Some(queue_rx),
            queue_tx,
        }
    }

    pub async fn run(mut self, subsys: SubsystemHandle) -> miette::Result<()> {
        let cancel_t = subsys.create_cancellation_token();
        let stream = self.stream.take().unwrap();
        let (mut read, mut write) = stream.into_split();
        tokio::select! {
            _ = cancel_t.cancelled() => {
                info!("Player shutdown requested");
            },
            _ = self.handle(subsys, read, write) => {
                info!("Player shutdown requested");
            },
        }
        let Self {
            stream,
            permit,
            pool: _,
            mgr_tx,
            addr,
            p_rx,
            queue_rx,
            ..
        } = self;
        drop(permit);
        drop(stream);
        drop(mgr_tx);
        drop(p_rx);
        drop(queue_rx);
        info!("Player {:?} shutting down", addr.ip());
        Ok(())
    }

    pub async fn handle(&mut self, subsys: SubsystemHandle, mut read: OwnedReadHalf, mut write: OwnedWriteHalf) -> Result<(), String> {
        let ls = match self.run_auth(&mut read, subsys.create_cancellation_token()).await {
            Ok(ls) => ls,
            Err(err) => {
                let err = match err {
                    api_error::Error::StrErr(err) => err,
                    api_error::Error::SqlxErr(err) => format!("DB Error: {:?}", err),
                    _ => format!("{:?}", err),
                };
                // notify mgr to remove player
                warn!("Error authenticating player: {:?}", err);
                let _ = write_response(&mut write, Response::AuthFail { err }).await;
                return Ok(());
            }
        };

        match write_response(
            &mut write,
            Response::AuthSuccess {
                session_token: ls.session_id().to_string(),
            },
        )
        .await
        {
            Ok(_) => {}
            Err(err) => {
                warn!("Error writing to socket: {:?}", err);
                return Ok(());
            }
        };
        info!(
            "Player {} ({}) authenticated / resumed: {}",
            ls.display_name(),
            ls.user.web_services_user_id,
            ls.resumed
        );
        self.session.set(ls).unwrap();

        let tracker = TaskTracker::new();
        // info!("sending init stats");
        self.send_init_stats(&tracker);
        // info!("done init stats begin");
        tracker.close();
        tracker.wait().await;
        // info!("done init stats done");

        tracker.reopen();
        let c1 = subsys.create_cancellation_token();
        let c2 = c1.clone();
        let c3 = c1.clone();
        self.start_write_loop(c2, write, &tracker);
        // info!("started write loop");
        self.start_mgr_loop(c3, &tracker);
        // info!("started mgr read loop");
        self.start_read_loop(c1, read, &tracker).await;
        // info!("finished read loop");
        subsys.create_cancellation_token().cancel();
        tracker.close();
        tracker.wait().await;
        info!(
            "Player {} ({}) disconnected",
            self.session.get().unwrap().display_name(),
            self.session.get().unwrap().user_id()
        );
        Ok(())
    }

    async fn start_read_loop(&mut self, cancel_t: CancellationToken, mut read: OwnedReadHalf, tracker: &TaskTracker) {
        let mgr_tx = self.mgr_tx.clone();
        let pool = self.pool.clone();
        let mut ctx = self.context.lock().await;
        let mut ctx_id = self.context_id.lock().await;
        let mut previous_ctx_id = None;
        let session = self.session.get().unwrap().clone();
        let queue_tx = self.queue_tx.clone();
        let user_id = session.user_id();

        let session_token = session.session_id();
        loop {
            let ct2 = cancel_t.clone();
            let msg = tokio::select! {
                r = Request::read_from_socket(&mut read, ct2) => r,
                _ = cancel_t.cancelled() => {
                    debug!("Player shutdown requested");
                    break;
                },
            };
            let msg = match msg {
                Ok(msg) => msg,
                Err(err) => {
                    warn!("Error reading from socket: {:?}", err);
                    // notify mgr to remove player
                    cancel_t.cancel();
                    break;
                }
            };
            let res = match msg {
                Request::Authenticate { .. } => Ok(()),  // ignore
                Request::ResumeSession { .. } => Ok(()), // ignore
                Request::ReportContext { sf, mi, map, i, bi } => {
                    debug!("Report context: sf/mi/map/i/bi: {}/{}/{:?}/{:?}/{:?}", sf, mi, map, i, bi);
                    match (parse_u64_str(&sf), parse_u64_str(&mi)) {
                        (Ok(sf), Ok(mi)) => {
                            XPlayer::update_context(
                                &pool,
                                session_token,
                                &mut ctx,
                                &mut ctx_id,
                                &mut previous_ctx_id,
                                sf,
                                mi,
                                map.as_ref(),
                                i.unwrap_or(false),
                                bi,
                            )
                            .await
                        }
                        _ => Err(format!("Invalid sf/mi: {}/{}", sf, mi).into()),
                    }
                }
                Request::ReportGCNodMsg { data } => XPlayer::on_report_gcnod_msg(&pool, session_token, ctx_id.as_ref(), &data).await,
                Request::Ping {} => queue_tx.send(Response::Ping {}).map_err(Into::into),
                Request::ReportVehicleState { vel, pos, rotq } => {
                    XPlayer::report_vehicle_state(&pool, session_token, ctx.as_ref(), ctx_id.as_ref(), pos, rotq, vel).await
                }
                Request::ReportRespawn { race_time } => XPlayer::report_respawn(&pool, session_token, race_time).await,
                Request::ReportFinish { race_time } => XPlayer::report_finish(&pool, session_token, race_time).await,
                Request::ReportFallStart {
                    floor,
                    pos,
                    speed,
                    start_time,
                } => XPlayer::on_report_fall_start(&pool, user_id, session_token, floor as i32, pos.into(), speed, start_time as i32).await,
                Request::ReportFallEnd { floor, pos, end_time } => {
                    XPlayer::on_report_fall_end(&pool, user_id, floor as i32, pos.into(), end_time as i32).await
                }
                Request::ReportStats { stats } => XPlayer::on_report_stats(&pool, user_id, stats).await,
                // // Request::ReportMapLoad { uid } => Player::on_report_map_load(&pool, p.clone(), &uid).await,
                Request::ReportPBHeight { h } => match XPlayer::on_report_pb_height(&pool, &session, ctx.as_ref(), h).await {
                    Ok(Some(res)) => {
                        if res.is_top_3 {
                            let _ = mgr_tx.send(ToPlayerMgr::NewTop3());
                        }
                        Ok(())
                    }
                    Ok(None) => Ok(()),
                    Err(e) => Err(e),
                },
                Request::GetMyStats {} => XPlayer::get_stats(&pool, user_id, &queue_tx).await,
                Request::GetGlobalLB { start, end } => XPlayer::get_global_lb(&pool, &queue_tx, start as i32, end as i32).await,
                Request::GetFriendsLB { friends } => todo!(), // Player::get_friends_lb(&pool, p.clone(), &friends).await,
                Request::GetGlobalOverview {} => XPlayer::get_global_overview(&pool, &queue_tx).await,
                Request::GetServerStats {} => XPlayer::get_server_stats(&pool, &queue_tx).await,
                Request::GetMyRank {} => XPlayer::get_my_rank(&pool, user_id, &queue_tx).await,
                Request::StressMe {} => (0..100)
                    .map(|_| queue_tx.send(Response::Ping {}))
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
        drop(read);
        drop(session);
    }

    fn start_write_loop(&mut self, cancel_t: CancellationToken, mut write: OwnedWriteHalf, tracker: &TaskTracker) {
        let mut queue_rx = self.queue_rx.take().unwrap();
        tracker.spawn(async move {
            loop {
                select! {
                    _ = cancel_t.cancelled() => {
                        debug!("Player shutdown requested");
                        break;
                    },
                    Some(msg) = queue_rx.recv() => {
                        match write_response(&mut write, msg).await {
                            Ok(_) => {},
                            Err(err) => {
                                warn!("Error writing to socket: {:?}", err);
                                cancel_t.cancel();
                                break;
                            }
                        }
                    },
                }
            }
            match write.shutdown().await {
                Ok(_) => {}
                Err(err) => {
                    // warn!("Error shutting down write half: {:?}", err);
                    cancel_t.cancel();
                }
            }
            drop(write);
        });
    }

    fn start_mgr_loop(&mut self, cancel_t: CancellationToken, tracker: &TaskTracker) {
        let mut p_rx = self.p_rx.take().unwrap();
        let queue_tx = self.queue_tx.clone();
        let mgr_tx = self.mgr_tx.clone();
        let session = self.session.get().unwrap().clone();
        tracker.spawn(async move {
            loop {
                select! {
                    _ = cancel_t.cancelled() => {
                        debug!("Player shutdown requested");
                        return;
                    },
                    Some(msg) = p_rx.recv() => {
                        match msg {
                            ToPlayer::Shutdown() => {
                                return;
                            },
                            ToPlayer::Top3(top3) => {
                                let _ = queue_tx.send(Response::Top3 { top3 });
                            },
                            ToPlayer::Send(msg) => {
                                let _ = queue_tx.send(msg);
                            },
                            // ToPlayer::
                            ToPlayer::NotifyRecord() => {}
                        }
                    },
                }
            }
        });
    }

    fn send_init_stats(&self, tracker: &TaskTracker) {
        let pool = self.pool.clone();
        let queue_tx = self.queue_tx.clone();
        tracker.spawn(async move {
            if let Ok(r) = get_global_lb(&pool, 1, 6).await {
                let top3 = r.into_iter().map(|r| r.into()).collect::<Vec<LeaderboardEntry>>();
                let top3 = Response::Top3 { top3 };
                let _ = queue_tx.send(top3.clone());
            };
            info!("done send global lb");
        });
        let pool = self.pool.clone();
        let queue_tx = self.queue_tx.clone();
        tracker.spawn(async move {
            if let Ok(j) = get_global_overview(&pool).await {
                let _ = queue_tx.send(Response::GlobalOverview { j });
            };
            info!("done send global overview");
        });
        let pool = self.pool.clone();
        let queue_tx = self.queue_tx.clone();
        tracker.spawn(async move {
            if let Ok(j) = get_server_info(&pool).await {
                let _ = queue_tx.send(Response::ServerInfo { nb_players_live: j });
            };
            info!("done send server info");
        });
    }

    pub async fn run_auth(&mut self, s_read: &mut OwnedReadHalf, ct: CancellationToken) -> Result<LoginSession, api_error::Error> {
        // let mut stream = p.stream.lock().await;
        let msg = Request::read_from_socket(s_read, ct).await;
        let msg = match msg {
            Ok(msg) => msg,
            Err(err) => {
                return Err(format!("[{:?}] Error reading from socket: {:?}", s_read.peer_addr(), err).into());
            }
        };
        match msg {
            Request::Authenticate {
                token,
                plugin_info,
                game_info,
                gamer_info,
            } => self.login_via_token(token, plugin_info, game_info, gamer_info).await,
            Request::ResumeSession {
                session_token,
                plugin_info,
                game_info,
                gamer_info,
            } => self.login_via_session(session_token, plugin_info, game_info, gamer_info).await,
            _ => {
                return Err(format!("Invalid request {:?}", msg).into());
            }
        }
    }

    pub async fn login_via_token(
        &mut self,
        token: String,
        plugin_info: String,
        game_info: String,
        gamer_info: String,
    ) -> Result<LoginSession, api_error::Error> {
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
        let user = register_or_login(&self.pool, &wsid, &token_resp.display_name).await?;
        let session = create_session(&self.pool, user.id(), &plugin_info, &game_info, &gamer_info, &self.ip_address).await?;
        Ok(LoginSession {
            user,
            session,
            resumed: false,
        })
    }

    pub async fn login_via_session(
        &mut self,
        session_token: String,
        plugin_info: String,
        game_info: String,
        gamer_info: String,
    ) -> Result<LoginSession, api_error::Error> {
        let session_token = match Uuid::from_str(&session_token) {
            Ok(session_token) => session_token,
            Err(err) => {
                return Err(format!("Invalid session_token: {:?}", err).into());
            }
        };
        // check for sesison and resume
        let (s, u) = resume_session(&self.pool, &session_token, &plugin_info, &game_info, &gamer_info, &self.ip_address).await?;
        Ok(LoginSession {
            user: u,
            session: s,
            resumed: true,
        })
    }
}

/**
*
*
*















*/

impl XPlayer {
    pub async fn update_context(
        pool: &Pool<Postgres>,
        session_token: &Uuid,
        ctx: &mut Option<PlayerCtx>,
        ctx_id: &mut Option<Uuid>,
        previous_ctx: &mut Option<Uuid>,
        sf: u64,
        mi: u64,
        map: Option<&Map>,
        has_vl_item: bool,
        bi: [i32; 2],
    ) -> Result<(), api_error::Error> {
        if let Some(ctx_id) = ctx_id.as_ref() {
            let _ = previous_ctx.insert(ctx_id.clone());
        }
        debug!("New context with sf: {}, mi: {}", sf, mi);
        let new_ctx = PlayerCtx::new(sf, mi, map.cloned(), has_vl_item);
        let map_id = match map {
            Some(map) => {
                let map_id = crate::queries::get_or_insert_map(pool, &map.uid, &map.name, &map.hash).await?;
                Some(map_id)
            }
            None => None,
        };
        let new_id = insert_context_packed(pool, session_token, &new_ctx, &previous_ctx, map_id, bi).await?;
        if let Some(previous_ctx) = previous_ctx {
            context_mark_succeeded(pool, &previous_ctx, &new_id).await?;
        }
        *ctx = Some(new_ctx);
        *ctx_id = Some(new_id);
        Ok(())
    }

    pub async fn on_report_gcnod_msg(pool: &Pool<Postgres>, session_id: &Uuid, ctx_id: Option<&Uuid>, data: &str) -> Result<(), ApiError> {
        // decode base64

        let x = if data.len() < 0x2E0 {
            data.into()
        } else {
            base64::prelude::BASE64_URL_SAFE.decode(data).unwrap_or(data.into())
        };

        match ctx_id {
            Some(ctx_id) => {
                insert_gc_nod(pool, session_id, ctx_id, &x).await?;
            }
            None => {
                warn!("Dropping GCNod b/c no context");
            }
        }
        Ok(())
    }

    pub async fn get_my_rank(pool: &Pool<Postgres>, user_id: &Uuid, queue_tx: &UnboundedSender<Response>) -> Result<(), ApiError> {
        let lb_entry: Option<LeaderboardEntry> = match get_user_in_lb(pool, user_id).await {
            Ok(r) => Ok::<Option<LeaderboardEntry>, ApiError>(r.map(|r| r.into())),
            Err(sqlx::Error::RowNotFound) => Ok(None),
            Err(e) => Err(e.into()),
        }?;
        Ok(queue_tx.send(Response::MyRank { r: lb_entry })?)
    }

    pub async fn get_server_stats(pool: &Pool<Postgres>, queue_tx: &UnboundedSender<Response>) -> Result<(), ApiError> {
        let nb_players_live = queries::get_server_info(pool).await?;
        Ok(queue_tx.send(Response::ServerInfo { nb_players_live })?)
    }

    pub async fn get_global_overview(pool: &Pool<Postgres>, queue_tx: &UnboundedSender<Response>) -> Result<(), ApiError> {
        let overview = queries::get_global_overview(pool).await?;
        Ok(queue_tx.send(Response::GlobalOverview { j: overview })?)
    }

    pub async fn get_friends_lb(
        pool: &Pool<Postgres>,
        user_id: &Uuid,
        queue_tx: &UnboundedSender<Response>,
        friends: &[Uuid],
    ) -> Result<(), ApiError> {
        let lb = queries::get_friends_lb(pool, user_id, friends).await?;
        Ok(queue_tx.send(Response::FriendsLB {
            entries: lb.into_iter().map(|e| e.into()).collect(),
        })?)
    }

    pub async fn get_global_lb(pool: &Pool<Postgres>, queue_tx: &UnboundedSender<Response>, start: i32, end: i32) -> Result<(), ApiError> {
        let lb = queries::get_global_lb(pool, start, end).await?;
        Ok(queue_tx.send(Response::GlobalLB {
            entries: lb.into_iter().map(|e| e.into()).collect(),
        })?)
    }

    pub async fn get_stats(pool: &Pool<Postgres>, user_id: &Uuid, queue_tx: &UnboundedSender<Response>) -> Result<(), ApiError> {
        match get_user_stats(pool, user_id).await {
            Ok((stats, rank)) => Ok(queue_tx.send(Response::Stats { stats, rank })?),
            // nothing to return
            Err(sqlx::Error::RowNotFound) => Ok(()),
            Err(e) => Ok(Err(e)?),
        }
    }

    pub async fn report_vehicle_state(
        pool: &Pool<Postgres>,
        sess: &Uuid,
        ctx: Option<&PlayerCtx>,
        ctx_id: Option<&Uuid>,
        pos: [f32; 3],
        rotq: [f32; 4],
        vel: [f32; 3],
    ) -> Result<(), ApiError> {
        let is_official = ctx.map(|ctx| ctx.is_official()).unwrap_or(false);
        Ok(insert_vehicle_state(pool, sess, ctx_id, is_official, pos, rotq, vel).await?)
    }

    pub async fn report_respawn(pool: &Pool<Postgres>, session_id: &Uuid, race_time: i64) -> Result<(), ApiError> {
        let race_time = match race_time > i32::MAX as i64 {
            true => -1,
            false => race_time as i32,
        };
        insert_respawn(pool, session_id, race_time).await?;
        Ok(())
    }

    pub async fn report_finish(pool: &Pool<Postgres>, session_id: &Uuid, race_time: i32) -> Result<(), ApiError> {
        insert_finish(pool, session_id, race_time).await?;
        Ok(())
    }

    pub async fn on_report_stats(pool: &Pool<Postgres>, user_id: &Uuid, stats: Stats) -> Result<(), ApiError> {
        Ok(update_users_stats(pool, user_id, &stats).await?)
    }

    pub async fn on_report_pb_height(
        pool: &Pool<Postgres>,
        ls: &LoginSession,
        ctx: Option<&PlayerCtx>,
        h: f32,
    ) -> Result<Option<PBUpdateRes>, ApiError> {
        if !ctx.map(|c| c.is_official()).unwrap_or(false) {
            match ctx {
                None => warn!("Dropping PB height b/c no context"),
                Some(ctx) => {
                    info!("context: {:?}", ctx);
                    match ctx.map.as_ref() {
                        None => warn!("Dropping PB height b/c no map"),
                        Some(map) => {
                            if map.uid != DD2_MAP_UID {
                                warn!("Dropping PB height b/c not DD2; user: {}", ls.display_name());
                                return Ok(None);
                            } else {
                                if !check_flags_sf_mi(ctx.sf, ctx.mi) {
                                    warn!("Dropping PB height b/c invalid sf/mi; user: {}", ls.display_name());
                                    return Ok(None);
                                } else {
                                    if !ctx.is_official() {
                                        warn!("Dropping PB height b/c not official; user: {}", ls.display_name());
                                        return Ok(None);
                                    } else {
                                        warn!("Dropping PB height b/c unknown reason; user: {}", ls.display_name());
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // let uid = ;
            // warn!("Dropping PB height b/c not official; user: {}", p.display_name());
            return Ok(None);
        }
        let res = update_user_pb_height(pool, ls.user_id(), h as f64).await?;
        info!("updated user pb height: {}: {}", ls.display_name(), h);
        // if res.is_top_3 {
        //     tx_mgr.send(ToPlayerMgr::NewTop3())?;
        // }
        Ok(Some(res))
    }

    pub async fn on_report_fall_start(
        pool: &Pool<Postgres>,
        user_id: &Uuid,
        session_id: &Uuid,
        floor: i32,
        pos: (f32, f32, f32),
        speed: f32,
        start_time: i32,
    ) -> Result<(), ApiError> {
        // insert
        Ok(insert_start_fall(pool, user_id, session_id, floor, pos, speed, start_time).await?)
    }

    pub async fn on_report_fall_end(
        pool: &Pool<Postgres>,
        user_id: &Uuid,
        floor: i32,
        pos: (f32, f32, f32),
        end_time: i32,
    ) -> Result<(), ApiError> {
        update_fall_with_end(pool, user_id, floor, pos, end_time).await?;
        Ok(())
    }
}
