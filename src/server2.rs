use api_error::Error as ApiError;
use base64::Engine;
use donations::{get_donations_and_donors, get_gfm_donations_latest};
use dotenv::dotenv;
use env_logger::Env;
use log::{debug, error, info, warn};
use miette::{Diagnostic, SourceSpan};
use op_auth::check_token;
use player::{parse_u64_str, LoginSession, Player};
use queries::{
    context_mark_succeeded, create_session, custom_maps::get_map_nb_playing_live, get_global_lb, get_global_overview, get_user_in_lb,
    get_user_stats, insert_context_packed, insert_finish, insert_gc_nod, insert_respawn, insert_start_fall, insert_vehicle_state,
    register_or_login, resume_session, update_fall_with_end, update_server_stats, update_user_color, update_users_stats, PBUpdateRes,
};
use router::{LeaderboardEntry2, Map, PlayerCtx, Request, Response, Stats, ToPlayerMgr};
use serde::ser::StdError;
use serde_json;
use sqlx::{
    postgres::{PgPool, PgPoolOptions},
    query, Pool, Postgres,
};
use std::{
    convert::Infallible,
    error::Error,
    net::SocketAddr,
    str::FromStr,
    sync::Arc,
    time::{Duration, SystemTime},
};
use thiserror::Error;
use tokio::{
    io::{self, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, Interest},
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
    http::{run_http_server, run_http_server_subsystem},
    op_auth::init_op_config,
    player::{check_flags_sf_mi, ToPlayer},
    queries::{get_server_info, update_user_pb_height},
    router::{write_response, LeaderboardEntry},
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
const MIN_PLUGIN_VERSION: [u32; 3] = [0, 4, 15];

#[tokio::main(flavor = "multi_thread", worker_threads = 10)]
async fn main() -> Result<(), Box<dyn Error>> {
    dotenv().ok();
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();
    info!("Starting DD2 Server for UID: {}", DD2_MAP_UID);

    // init db from env var
    let db_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let pool = PgPoolOptions::new().max_connections(10).connect(&db_url).await.unwrap();
    let db = Arc::new(pool);
    info!("Connected to db");
    let http_db = db.clone();

    match sqlx::migrate!().run(db.as_ref()).await {
        Ok(_) => info!("Migrations ran successfully"),
        Err(e) => panic!("Error running migrations: {:?}", e),
    }
    init_op_config().await;

    let subsys = Toplevel::new(|s| async move {
        s.start(SubsystemBuilder::new("AcceptConns", |a| accept_conns(a, db)));
    })
    .catch_signals();

    #[cfg(debug_assertions)]
    {
        subsys
            .handle_shutdown_requests(Duration::from_millis(500))
            .await
            .map_err(Into::into)
    }
    #[cfg(not(debug_assertions))]
    {
        let http_serv = run_http_server(http_db, "dips-plus-plus-server.xk.io".to_string(), None);

        tokio::select! {
            r = subsys.handle_shutdown_requests(Duration::from_millis(500)) => r.map_err(Into::into),
            _ = http_serv => Ok(()),
        }
    }
}

async fn accept_conns(subsys: SubsystemHandle, db: Arc<Pool<Postgres>>) -> miette::Result<()> {
    let bind_addr = "0.0.0.0:17677";
    warn!("Starting server on: {}", bind_addr);
    let listener = TcpListener::bind(bind_addr).await.unwrap();
    let limit_conns = Arc::new(Semaphore::new(MAX_CONNECTIONS as usize));
    subsys.start(SubsystemBuilder::new("PlayerMgr", |a| {
        PlayerMgr::new(db, limit_conns, listener).run(a)
    }));

    // tokio::select! {
    //     _ = subsys.on_shutdown_requested() => {
    //         info!("[accept_cons]: shutdown");
    //     },
    // }

    // subsys.start(SubsystemBuilder::new("HTTPServer", move |a| async {
    //     run_http_server(
    //         http_db,
    //         "dips-plus-plus-server.xk.io".to_string(),
    //         Some(a.create_cancellation_token()),
    //     )
    //     .await;
    //     Ok::<(), Infallible>(())
    // }));
    // run_http_server_subsystem(http_db, subsys).await?;

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

            let r = get_global_lb(&pool, 1, 11).await;
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
            let overview = get_global_overview(&pool).await;
            ps.iter()
                .map(|p| {
                    let _ = p.send(ToPlayer::Top3(top3.clone()));
                    let _ = p.send(ToPlayer::Send(server_info.clone()));
                    if let Ok(j) = &overview {
                        let _ = p.send(ToPlayer::Send(router::Response::GlobalOverview { j: j.clone() }));
                    }
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
        let (read, write) = stream.into_split();
        tokio::select! {
            _ = cancel_t.cancelled() => {
                info!("Player shutdown (cancelled)");
            },
            _ = self.handle(subsys, read, write) => {
                info!("Player shutdown (handle ended)");
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
            "Player {} ({}) authenticated / resumed: {} / PLUGIN_VER: {}",
            ls.display_name(),
            ls.user.web_services_user_id,
            ls.resumed,
            ls.plugin_ver
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
                Ok(Some(msg)) => msg,
                Ok(None) => {
                    // unknown msg
                    continue;
                }
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
                // report requests
                Request::ReportContext { sf, mi, map, i, bi, e } => {
                    debug!("Report context: sf/mi/map/i/bi/e: {}/{}/{:?}/{:?}/{:?}/{:?}", sf, mi, map, i, bi, e);
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
                                e,
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
                Request::ReportStats { stats } => XPlayer::on_report_stats(&pool, user_id, stats, ctx.as_ref()).await,
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
                Request::ReportPlayerColor { wsid, color } => XPlayer::on_report_color(&pool, wsid, color).await,
                Request::ReportTwitch { twitch_name } => XPlayer::on_report_twitch(&pool, user_id, twitch_name).await,
                Request::DowngradeStats { stats } => XPlayer::downgrade_stats(&pool, user_id, stats, &queue_tx).await,
                // arbitrary maps
                Request::ReportMapCurrPos { uid, pos, race_time } => {
                    XPlayer::report_map_curr_pos(&pool, user_id, uid, pos, race_time.unwrap_or(-1) as i32).await
                }
                // get requests
                Request::GetMyStats {} => XPlayer::get_stats(&pool, user_id, &queue_tx).await,
                Request::GetGlobalLB { start, end } => XPlayer::get_global_lb(&pool, &queue_tx, start as i32, end as i32).await,
                Request::GetFriendsLB { friends } => todo!(), // Player::get_friends_lb(&pool, p.clone(), &friends).await,
                Request::GetGlobalOverview {} => XPlayer::get_global_overview(&pool, &queue_tx).await,
                Request::GetServerStats {} => XPlayer::get_server_stats(&pool, &queue_tx).await,
                Request::GetMyRank {} => XPlayer::get_my_rank(&pool, user_id, &queue_tx).await,
                Request::GetPlayersPb { wsid } => XPlayer::get_players_pb(&pool, wsid, &queue_tx).await,
                Request::GetDonations {} => XPlayer::get_donations(&pool, &queue_tx).await.map_err(|e| e.into()),
                Request::GetGfmDonations {} => XPlayer::get_gfm_donations(&pool, &queue_tx).await.map_err(|e| e.into()),
                Request::GetTwitch { wsid } => {
                    XPlayer::get_twitch(
                        &pool,
                        wsid.and_then(|s| Uuid::from_str(&s).ok()).as_ref().unwrap_or(user_id),
                        &queue_tx,
                    )
                    .await
                }
                // get arb maps
                Request::GetMapOverview { uid } => XPlayer::get_map_overview(&pool, &queue_tx, uid).await,
                Request::GetMapLB { uid, start, end } => XPlayer::get_map_lb(&pool, &queue_tx, uid, start as i64, end as i64).await,
                Request::GetMapLive { uid } => XPlayer::get_map_live(&pool, &queue_tx, uid).await,
                Request::GetMapMyRank { uid } => XPlayer::get_map_rank(&pool, uid, user_id, &queue_tx).await,
                Request::GetMapRank { uid, wsid } => {
                    if let Ok(user_id) = Uuid::from_str(&wsid) {
                        XPlayer::get_map_rank(&pool, uid, &user_id, &queue_tx).await
                    } else {
                        Ok(())
                    }
                }
                // debug
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
            if let Ok(r) = get_global_lb(&pool, 1, 11).await {
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
            Some(Request::Authenticate {
                token,
                plugin_info,
                game_info,
                gamer_info,
            }) => {
                self.check_min_plugin_version(&plugin_info).await?;
                self.login_via_token(token, plugin_info, game_info, gamer_info).await
            }
            Some(Request::ResumeSession {
                session_token,
                plugin_info,
                game_info,
                gamer_info,
            }) => {
                self.check_min_plugin_version(&plugin_info).await?;
                self.login_via_session(session_token, plugin_info, game_info, gamer_info).await
            }
            _ => {
                return Err(format!("Invalid request {:?}", msg).into());
            }
        }
    }

    async fn check_min_plugin_version(&self, plugin_info: &str) -> Result<(), api_error::Error> {
        let version = plugin_info_extract_version(plugin_info);
        let parts = version_str_to_parts(&version);
        let parts = match parts {
            Ok(p) => p,
            Err(_) => vec![0, 0, 0],
        };
        let minv = MIN_PLUGIN_VERSION;
        if version_less(&parts, &minv) {
            tokio::time::sleep(Duration::from_secs(10)).await;
            return Err(format!("Update Plugin! Version too low: {}", version_to_string(&parts)).into());
        }
        Ok(())
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
        let plugin_ver = plugin_info_extract_version(&plugin_info);
        Ok(LoginSession {
            user,
            session,
            resumed: false,
            plugin_ver,
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
        let plugin_ver = plugin_info_extract_version(&plugin_info);
        // check for sesison and resume
        let (s, u) = resume_session(&self.pool, &session_token, &plugin_info, &game_info, &gamer_info, &self.ip_address).await?;
        Ok(LoginSession {
            user: u,
            session: s,
            resumed: true,
            plugin_ver,
        })
    }
}

pub fn version_less(v1: &[u32], v2: &[u32]) -> bool {
    for (a, b) in v1.iter().zip(v2.iter()) {
        if a < b {
            return true;
        }
    }
    false
}

pub fn version_to_string(v: &[u32]) -> String {
    v.iter().map(|i| i.to_string()).collect::<Vec<String>>().join(".")
}

pub fn version_str_to_parts(version: &str) -> Result<Vec<u32>, ()> {
    version.split('.').map(|s| s.parse().map_err(|_| ())).collect()
}

pub fn plugin_info_extract_version(plugin_info: &str) -> String {
    plugin_info
        .lines()
        .find(|&l| l.starts_with("Version:"))
        .and_then(|vl| vl.split(":").last())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "0.0.0".into())
}

/**
*
*
*















*/

impl XPlayer {
    // get_map_overview
    // get_map_lb
    // get_map_live

    pub async fn get_map_rank_query(
        pool: &Pool<Postgres>,
        map_uid: &str,
        user_id: &Uuid,
    ) -> Result<Option<LeaderboardEntry2>, api_error::Error> {
        let resp = query!(
            r#"--sql
            SELECT m.user_id, u.display_name, c.color, m.pos, m.race_time, rank() OVER (ORDER BY m.height DESC) AS rank, m.updated_at
            FROM map_leaderboard m
            LEFT JOIN users u ON u.web_services_user_id = m.user_id
            LEFT JOIN colors c ON c.user_id = m.user_id
            WHERE m.map_uid = $1 AND m.user_id = $2
        "#,
            map_uid,
            &user_id
        )
        .fetch_optional(pool)
        .await?
        .map(|r| LeaderboardEntry2 {
            rank: r.rank.unwrap_or_default() as u32,
            wsid: r.user_id.to_string(),
            pos: [r.pos[0], r.pos[1], r.pos[2]],
            ts: r.updated_at.and_utc().timestamp() as u32,
            name: r.display_name,
            update_count: 0,
            color: [r.color[0], r.color[1], r.color[2]],
            race_time: r.race_time as i64,
        });
        Ok(resp)
    }

    pub async fn get_map_rank(
        pool: &Pool<Postgres>,
        map_uid: String,
        user_id: &Uuid,
        queue_tx: &UnboundedSender<Response>,
    ) -> Result<(), api_error::Error> {
        let r = XPlayer::get_map_rank_query(pool, &map_uid, user_id).await?;
        queue_tx.send(Response::MapRank { uid: map_uid, r })?;
        Ok(())
    }

    pub async fn get_map_overview(
        pool: &Pool<Postgres>,
        queue_tx: &UnboundedSender<Response>,
        map_uid: String,
    ) -> Result<(), api_error::Error> {
        let nb_players_on_lb: u32 = query!(
            r#"--sql
            SELECT COUNT(*) FROM map_leaderboard WHERE map_uid = $1
        "#,
            &map_uid
        )
        .fetch_one(pool)
        .await?
        .count
        .unwrap_or(0) as u32;
        let nb_playing_now = get_map_nb_playing_live(pool, &map_uid).await? as u32;

        queue_tx.send(Response::MapOverview {
            uid: map_uid,
            nb_players_on_lb,
            nb_playing_now,
        })?;
        Ok(())
    }

    pub async fn get_map_lb(
        pool: &Pool<Postgres>,
        queue_tx: &UnboundedSender<Response>,
        map_uid: String,
        start: i64,
        end: i64,
    ) -> Result<(), api_error::Error> {
        let resp = query!(r#"--sql
            SELECT m.user_id, u.display_name, c.color, m.pos, m.race_time, m.updated_at, m.update_count, rank() OVER (ORDER BY m.height DESC) AS rank FROM map_leaderboard m
            LEFT JOIN users u ON u.web_services_user_id = m.user_id
            LEFT JOIN colors c ON c.user_id = m.user_id
            WHERE m.map_uid = $1
            ORDER BY m.height DESC
            LIMIT $2
            OFFSET $3
        "#,
            &map_uid,
            end - start,
            start
        )
        .fetch_all(pool)
        .await?;
        let entries = resp
            .into_iter()
            .map(|r| LeaderboardEntry2 {
                rank: r.rank.unwrap_or_default() as u32,
                wsid: r.user_id.to_string(),
                pos: [r.pos[0], r.pos[1], r.pos[2]],
                ts: r.updated_at.and_utc().timestamp() as u32,
                name: r.display_name,
                update_count: r.update_count,
                color: [r.color[0], r.color[1], r.color[2]],
                race_time: r.race_time as i64,
            })
            .collect();
        queue_tx.send(Response::MapLB {
            uid: map_uid.to_string(),
            entries,
        })?;
        Ok(())
    }

    pub async fn get_map_live(
        pool: &Pool<Postgres>,
        queue_tx: &UnboundedSender<Response>,
        map_uid: String,
    ) -> Result<(), api_error::Error> {
        let resp = query!(r#"--sql
            SELECT m.user_id, u.display_name, c.color, m.pos, m.race_time, m.updated_at, m.update_count, rank() OVER (ORDER BY m.height DESC) AS rank FROM map_curr_heights m
            LEFT JOIN users u ON u.web_services_user_id = m.user_id
            LEFT JOIN colors c ON c.user_id = m.user_id
            WHERE m.map_uid = $1
                AND m.updated_at > now() - interval '30 seconds'
            ORDER BY m.height DESC
        "#,
            &map_uid
        )
        .fetch_all(pool)
        .await?;
        let players = resp
            .into_iter()
            .map(|r| LeaderboardEntry2 {
                rank: r.rank.unwrap_or_default() as u32,
                wsid: r.user_id.to_string(),
                pos: [r.pos[0], r.pos[1], r.pos[2]],
                ts: r.updated_at.and_utc().timestamp() as u32,
                name: r.display_name,
                update_count: r.update_count,
                color: [r.color[0], r.color[1], r.color[2]],
                race_time: r.race_time as i64,
            })
            .collect();
        queue_tx.send(Response::MapLivePlayers { uid: map_uid, players })?;
        Ok(())
    }

    pub async fn report_map_curr_pos(
        pool: &Pool<Postgres>,
        user_id: &Uuid,
        map_uid: String,
        pos: [f64; 3],
        race_time: i32,
    ) -> Result<(), api_error::Error> {
        // bad height, discard report
        if pos[1] > 3000.0 {
            return Ok(());
        }
        if map_uid.len() != 27 {
            warn!("Invalid map_uid: {:?} from {:?}", map_uid, user_id);
            Err("Invalid map_uid".to_string())?;
        }
        let r = query!(
            r#"--sql
            INSERT INTO map_curr_heights (map_uid, user_id, height, pos, race_time) VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (map_uid, user_id)
            DO UPDATE SET height = $3, pos = $4, race_time = $5, updated_at = now(), update_count = map_curr_heights.update_count + 1
            RETURNING height, update_count
        "#,
            &map_uid,
            user_id,
            pos[1],
            &pos,
            race_time as i32
        )
        .fetch_one(pool)
        .await?;

        #[cfg(debug_assertions)]
        info!("User {:?} reported map curr pos: {:?}", user_id, r);

        // do insert and on conflict update if height is higher

        let update_r = query!(
            r#"--sql
            UPDATE map_leaderboard SET pos = $1, height = $2, race_time = $5, updated_at = now(), update_count = map_leaderboard.update_count + 1
            WHERE map_uid = $3 AND user_id = $4 AND height < $2
            RETURNING id
        "#,
            &pos,
            pos[1],
            &map_uid,
            user_id,
            race_time
        )
        .fetch_one(pool)
        .await;
        match update_r {
            Ok(_) => {}
            Err(sqlx::Error::RowNotFound) => {
                #[cfg(debug_assertions)]
                info!("inserting new map lb: {}, {}, {:?}", &map_uid, user_id, &pos);
                query!(
                    r#"--sql
                    INSERT INTO map_leaderboard (map_uid, user_id, pos, height, race_time)
                    VALUES ($1, $2, $3, $4, $5)
                    ON CONFLICT (map_uid, user_id) DO NOTHING
                "#,
                    &map_uid,
                    user_id,
                    &pos,
                    pos[1],
                    race_time
                )
                .execute(pool)
                .await?;
            }
            Err(e) => {
                return Err(e.into());
            }
        }

        #[cfg(debug_assertions)]
        {
            let lb = XPlayer::get_map_rank_query(pool, &map_uid, user_id).await?;
            info!("User {:?} new lb map rank: {:?}", user_id, lb);
        }

        Ok(())
    }

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
        editor_exists: Option<bool>,
    ) -> Result<(), api_error::Error> {
        if let Some(ctx_id) = ctx_id.as_ref() {
            let _ = previous_ctx.insert(ctx_id.clone());
        }
        debug!("New context with sf: {}, mi: {}", sf, mi);
        let new_ctx = PlayerCtx::new(sf, mi, map.cloned(), has_vl_item, editor_exists);
        let map_id = match map {
            Some(map) => {
                let map_id = crate::queries::get_or_insert_map(pool, &map.uid, &map.name, &map.hash).await?;
                Some(map_id)
            }
            None => None,
        };
        let new_id = insert_context_packed(pool, session_token, &new_ctx, &previous_ctx, map_id, bi, editor_exists).await?;
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

    pub async fn get_gfm_donations(pool: &Pool<Postgres>, queue_tx: &UnboundedSender<Response>) -> Result<(), ApiError> {
        let total = get_gfm_donations_latest(pool).await?;
        Ok(queue_tx.send(Response::GfmDonations { total })?)
    }

    pub async fn get_donations(pool: &Pool<Postgres>, queue_tx: &UnboundedSender<Response>) -> Result<(), ApiError> {
        let (donations, donors) = get_donations_and_donors(pool).await?;
        Ok(queue_tx.send(Response::Donations {
            donations,
            donors: donors.into_iter().map(|d| d.into()).collect(),
        })?)
    }

    pub async fn get_players_pb(pool: &Pool<Postgres>, wsid: String, queue_tx: &UnboundedSender<Response>) -> Result<(), ApiError> {
        let user_id = match Uuid::from_str(&wsid) {
            Ok(u) => u,
            Err(_) => return Ok(()), // ignore bad wsids
        };
        let pb = get_user_in_lb(pool, &user_id).await?;
        if let Some(pb) = pb {
            return Ok(queue_tx.send(Response::PlayersPB {
                name: pb.display_name.unwrap_or_else(|| wsid.clone()),
                wsid,
                height: pb.height,
                rank: pb.rank.unwrap_or(99999),
                ts: pb.ts.and_utc().timestamp(),
                update_count: pb.update_count,
            })?);
        } else {
            info!("No PB found for wsid: {}", wsid);
        };
        Ok(())
        // otherwise ignore, nothing to respond with
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

    pub async fn on_report_stats(pool: &Pool<Postgres>, user_id: &Uuid, stats: Stats, ctx: Option<&PlayerCtx>) -> Result<(), ApiError> {
        match ctx.map(|c| c.is_official()).unwrap_or(false) {
            true => {}
            false => {
                warn!("Dropping stats ({}) b/c context unofficial. ctx: {:?}", user_id, ctx);
                return Ok(());
            }
        }
        Ok(update_users_stats(pool, user_id, &stats).await?)
    }

    pub async fn on_report_color(pool: &Pool<Postgres>, wsid: String, color: [f64; 3]) -> Result<(), ApiError> {
        let user_id = match Uuid::from_str(&wsid) {
            Ok(u) => u,
            Err(_) => return Ok(()), // ignore bad wsid
        };
        Ok(update_user_color(pool, &user_id, color).await?)
    }

    pub async fn on_report_twitch(pool: &Pool<Postgres>, user_id: &Uuid, twitch_name: String) -> Result<(), ApiError> {
        query!(
            "INSERT INTO twitch_usernames (user_id, twitch_name) VALUES ($1, $2) ON CONFLICT (user_id) DO UPDATE SET twitch_name = $2;",
            user_id,
            &twitch_name
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    pub async fn get_twitch(pool: &Pool<Postgres>, user_id: &Uuid, queue_tx: &UnboundedSender<Response>) -> Result<(), ApiError> {
        let r = query!("SELECT user_id, twitch_name FROM twitch_usernames WHERE user_id = $1;", user_id)
            .fetch_one(pool)
            .await;
        let r = match r {
            Ok(r) => r,
            Err(e) => {
                // ignore missing
                return Ok(());
            }
        };
        queue_tx.send(Response::TwitchName {
            twitch_name: r.twitch_name,
            user_id: r.user_id.to_string(),
        })?;
        Ok(())
    }

    pub async fn downgrade_stats(
        pool: &Pool<Postgres>,
        user_id: &Uuid,
        stats: Stats,
        queue_tx: &UnboundedSender<Response>,
    ) -> Result<(), ApiError> {
        match queries::downgrade_stats(pool, user_id, &stats).await {
            Ok(_) => Ok(()),
            Err(e) => {
                queue_tx.send(Response::NonFatalErrorMsg {
                    level: 1,
                    msg: format!(
                        "Failed to downgrade stats (note: pb height must be less than or equal to existing)\nError: {:?}",
                        e
                    ),
                })?;
                Err(e.into())
            }
        }
    }

    pub async fn on_report_pb_height(
        pool: &Pool<Postgres>,
        ls: &LoginSession,
        ctx: Option<&PlayerCtx>,
        h: f32,
    ) -> Result<Option<PBUpdateRes>, ApiError> {
        if !ctx.map(|c| c.is_official()).unwrap_or(false) {
            warn!(
                "User has unofficial context but reported PB height: {} / {}",
                ls.display_name(),
                ls.user_id()
            );
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
