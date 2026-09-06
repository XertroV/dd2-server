use api_error::Error as ApiError;
use donations::{get_donations_and_donors, get_gfm_donations_latest};
use dotenv::dotenv;
use env_logger::Env;
use lazy_static::lazy_static;
use log::{debug, error, info, warn};
use miette::Diagnostic;
use num_traits::ToPrimitive;
use op_auth::check_token;
use player::{parse_u64_str, LoginSession};
use queries::{
    context_mark_succeeded, create_session, get_global_lb, get_global_overview, get_user_in_lb, get_user_stats, insert_context_packed,
    insert_finish, insert_respawn, insert_start_fall, register_or_login, resume_session, update_fall_with_end, update_server_stats,
    update_user_color, update_users_stats, user_settings, PBUpdateRes,
};
use router::{AssetRef, LeaderboardEntry2, Map, PlayerCtx, Request, Response, Stats, ToPlayerMgr};

use sqlx::{postgres::PgPoolOptions, query, Pool, Postgres};
use std::{
    collections::{HashMap, HashSet},
    error::Error,
    net::SocketAddr,
    str::FromStr,
    sync::{Arc, Mutex as StdMutex, Weak},
    time::{self, Duration},
};
use thiserror::Error;
use tokio::{
    io::{self, AsyncReadExt, AsyncWriteExt},
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpListener, TcpStream,
    },
    select,
    sync::{
        mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
        Mutex, OnceCell, OwnedSemaphorePermit, Semaphore,
    },
};
use tokio_graceful_shutdown::{FutureExt, SubsystemBuilder, SubsystemHandle, Toplevel};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use uuid::Uuid;

use crate::{
    cache_util::AsyncCache,
    consts::DD2_MAP_UID,
    op_auth::init_op_config,
    player::{check_flags_sf_mi, ToPlayer},
    queries::{custom_map_aux_specs, custom_maps::get_map_overview_player_counts, get_server_info, update_user_pb_height},
    router::{write_response, LeaderboardEntry},
};

mod api_error;
mod cache_util;
mod consts;
mod db;
mod donations;
mod http;
mod moderation_http;
mod nadeo;
mod op_auth;
mod player;
mod queries;
mod router;
mod tm_oauth;

const MAX_CONNECTIONS: u32 = 4096;
const MIN_PLUGIN_VERSION: [u32; 3] = [0, 4, 15];
const MAX_HEIGHT: f64 = 5000.0;

/// Exact per-map locks whose registry holds only weak references. Dead entries
/// are pruned on each lookup, so retained state is bounded by peak in-flight
/// Wayfinder operations rather than attacker-controlled map history.
#[derive(Default)]
struct WayfinderMapOperationLocks {
    locks: StdMutex<HashMap<String, Weak<Mutex<()>>>>,
}

impl WayfinderMapOperationLocks {
    fn for_map(&self, map_uid: &str) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(map_uid).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(Mutex::new(()));
        locks.insert(map_uid.to_string(), Arc::downgrade(&lock));
        lock
    }
}

lazy_static! {
    static ref CACHED_LB_INFO: AsyncCache<String, Vec<LeaderboardEntry2>> = Default::default();
    static ref CACHED_LIVE_INFO: AsyncCache<String, Vec<LeaderboardEntry2>> = Default::default();
    static ref NADEO_CLIENT: Arc<nadeo::NadeoLiveClient> = nadeo::NadeoLiveClient::from_env();
    static ref NADEO_MAP_AUTHOR_VERIFIER: Arc<nadeo::CachedMapAuthorVerifier<nadeo::NadeoLiveClient>> =
        nadeo::CachedMapAuthorVerifier::new(NADEO_CLIENT.clone());
    static ref NADEO_RECORD_VERIFIER: Arc<nadeo::CachedOfficialRecordVerifier<nadeo::NadeoLiveClient>> =
        nadeo::CachedOfficialRecordVerifier::new(NADEO_CLIENT.clone());
    static ref HAT_EQUIP_LIMITER: queries::hats::HatEquipRateLimiter =
        queries::hats::HatEquipRateLimiter::new(4096, Duration::from_millis(250), Duration::from_secs(1), 2048);
    static ref WAYFINDER_MAP_OPERATION_LOCKS: WayfinderMapOperationLocks = Default::default();
}

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
        let http_serv = http::run_http_server(http_db, "localhost".to_string(), None, false);
        tokio::select! {
            r = subsys.handle_shutdown_requests(Duration::from_millis(500)) => r.map_err(Into::into),
            _ = http_serv => Ok(()),
        }
    }
    #[cfg(not(debug_assertions))]
    {
        let http_serv = http::run_http_server(http_db, "dips-plus-plus-server.xk.io".to_string(), None, false);

        tokio::select! {
            r = subsys.handle_shutdown_requests(Duration::from_millis(100)) => r.map_err(Into::into),
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
    players: Arc<Mutex<Vec<PlayerDelivery>>>,
    rx: Option<UnboundedReceiver<ToPlayerMgr>>,
    tx: UnboundedSender<ToPlayerMgr>,
    // state: Mutex<PlayerMgrState>,
    pool: Arc<Pool<Postgres>>,
    limit_conns: Arc<Semaphore>,
    listener: TcpListener,
}

#[derive(Clone)]
struct PlayerDelivery {
    tx: UnboundedSender<ToPlayer>,
    /// Real 27-char UID of the Wayfinder map this connection is on, as reported
    /// in the bodies of requests 81/213/214. Deliberately not the `ReportContext`
    /// map id: the plugin only sends the true UID there for Deep Dip maps and
    /// otherwise sends a 30-char MD5 prefix, so map-scoped hat delivery keyed on
    /// it would never match on an ordinary Wayfinder map.
    wayfinder_map_uid: Arc<Mutex<Option<String>>>,
}

fn delivery_matches_map(current: Option<&str>, target: &str) -> bool {
    current == Some(target)
}

async fn deliver_player_mgr_message(players: &mut Vec<PlayerDelivery>, msg: ToPlayerMgr) {
    players.retain(|player| !player.tx.is_closed());
    match msg {
        ToPlayerMgr::Broadcast(response) => {
            for player in players.iter() {
                let _ = player.tx.send(ToPlayer::Send(response.clone()));
            }
        }
        ToPlayerMgr::BroadcastToMap { map_uid, response } => {
            for player in players.iter() {
                let current = player.wayfinder_map_uid.lock().await;
                if delivery_matches_map(current.as_deref(), &map_uid) {
                    let _ = player.tx.send(ToPlayer::Send(response.clone()));
                }
            }
        }
        _ => {}
    }
}

/// The plugin's `ReportContext` map id for a map it does not consider Deep Dip:
/// `Crypto::MD5(map.EdChallengeId).SubStr(0, 30)` (`PluginInfo.as:220-227`).
fn obfuscated_map_uid(map_uid: &str) -> String {
    use md5::Digest;
    let mut hasher = md5::Md5::new();
    hasher.update(map_uid.as_bytes());
    let mut digest = hex::encode(hasher.finalize());
    digest.truncate(30);
    digest
}

/// Whether a `ReportContext` map id names the map we already track.
///
/// The context report and request 81 come from independent plugin coroutines
/// (`WatchAndSendContextChanges` polls on a ~117ms sleep, `CheckMapChange` runs
/// every frame), so on a map load either can land first. Treating "the context
/// string changed" as "the player left the map" would clear the map request 81
/// just adopted -- and broadcast a spurious leave push -- roughly half the time.
/// The context id is the true UID on Deep Dip maps and the 30-char MD5 prefix
/// everywhere else, so both forms have to be recognised.
fn context_uid_refers_to_map(context_uid: Option<&str>, tracked_map_uid: &str) -> bool {
    // Short-circuits on the cheap comparison; the hash only runs on an actual
    // context change, which is at most once per map load per player.
    // Case-insensitive on the hex: nothing in either repo pins the case of
    // Openplanet's `Crypto::MD5`, and getting it wrong would silently un-fix
    // this whole path.
    matches!(context_uid, Some(uid) if uid == tracked_map_uid || uid.eq_ignore_ascii_case(&obfuscated_map_uid(tracked_map_uid)))
}

/// Per-connection bookkeeping for deferred hat-unlock validation.
///
/// Two separate jobs, deliberately not conflated:
///
/// - `owed` records every `(map_uid, slug)` whose 213 answer is still
///   outstanding. Request 213 for a map with an outstanding entry is withheld
///   (an inline answer would still read the pre-unlock state), and the
///   validation that drains the last entry for that map pushes the answer.
///   Duplicates coalesce. Its bound exists only to stop a client streaming 84s:
///   the plugin's `RecomputeUnlocks` fires 84 for every newly-satisfied hat in
///   one synchronous pass, so a batch as large as the map's hat count is
///   ordinary traffic and must never be discarded.
/// - `validation_permits` caps how much Nadeo work this connection has running
///   at once. It is acquired *inside* the spawned task, so a batch that exceeds
///   it queues instead of being dropped.
pub struct PendingHatUnlocks {
    owed: StdMutex<HashSet<(String, String)>>,
    validation_permits: Arc<Semaphore>,
}

/// Generous: only a client streaming 84s at socket rate should ever reach it.
/// Must stay comfortably above the number of hats one map can grant in a single
/// `RecomputeUnlocks` pass.
const MAX_PENDING_HAT_UNLOCKS: usize = 16;

/// How many hat validations may touch Nadeo at once, per connection.
const MAX_CONCURRENT_HAT_VALIDATIONS: usize = 2;

impl Default for PendingHatUnlocks {
    fn default() -> Self {
        Self {
            owed: Default::default(),
            validation_permits: Arc::new(Semaphore::new(MAX_CONCURRENT_HAT_VALIDATIONS)),
        }
    }
}

impl PendingHatUnlocks {
    /// `None` when an answer is already owed for this map+hat (the pending
    /// validation will answer for both), or the connection is over the spam
    /// bound.
    fn begin(self: &Arc<Self>, map_uid: &str, slug: &str) -> Option<PendingHatUnlock> {
        let key = (map_uid.to_string(), slug.to_string());
        let mut owed = self.owed.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if owed.len() >= MAX_PENDING_HAT_UNLOCKS || !owed.insert(key.clone()) {
            return None;
        }
        Some(PendingHatUnlock {
            owner: self.clone(),
            key: Some(key),
        })
    }

    fn any_for_map(&self, map_uid: &str) -> bool {
        self.owed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .any(|(pending_map, _)| pending_map == map_uid)
    }

    fn remove(&self, key: &(String, String)) -> bool {
        let mut owed = self.owed.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        owed.remove(key);
        !owed.iter().any(|(pending_map, _)| pending_map == &key.0)
    }
}

struct PendingHatUnlock {
    owner: Arc<PendingHatUnlocks>,
    /// `None` once released; `Drop` then has nothing to do.
    key: Option<(String, String)>,
}

impl PendingHatUnlock {
    /// Release this entry and report whether the map now has no answers owed.
    ///
    /// Only the release that drains the map should push the 213: a push issued
    /// while a sibling validation is still running would report that sibling's
    /// hat as locked, and the client persists what it is told.
    fn release(&mut self) -> bool {
        match self.key.take() {
            Some(key) => self.owner.remove(&key),
            None => false,
        }
    }
}

impl Drop for PendingHatUnlock {
    fn drop(&mut self) {
        // Covers the unwind path; the normal path has already released.
        if let Some(key) = self.key.take() {
            self.owner.remove(&key);
        }
    }
}

/// Spawn detached work whose failure would otherwise be invisible: a panic in a
/// spawned task is swallowed with the dropped `JoinHandle`, so watch the handle
/// and log it.
fn spawn_logged<F>(what: &'static str, task: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let handle = tokio::spawn(task);
    tokio::spawn(async move {
        if let Err(error) = handle.await {
            warn!("Deferred {} task panicked or was cancelled: {:?}", what, error);
        }
    });
}

fn update_last_map(last: &mut Option<String>, current: Option<&str>) -> bool {
    if last.as_deref() == current {
        return false;
    }
    *last = current.map(str::to_string);
    true
}

impl PlayerMgr {
    pub fn new(pool: Arc<Pool<Postgres>>, limit_conns: Arc<Semaphore>, mut listener: TcpListener) -> Self {
        let (tx, rx) = unbounded_channel();
        Self {
            players: Mutex::new(Vec::new()).into(),
            /// send ToPlayerMgr msgs
            tx,
            rx: Some(rx),
            // state: Mutex::new(PlayerMgrState::default()),
            /// the db
            pool,
            limit_conns,
            listener,
        }
    }

    async fn top_3_loop(
        pool: Arc<Pool<Postgres>>,
        players: Arc<Mutex<Vec<PlayerDelivery>>>,
        subsys: SubsystemHandle,
    ) -> miette::Result<()> {
        // offset the count by some random amount [0, 9] to stagger server stats updates between servers
        let mut count: u64 = (time::Instant::now().elapsed().as_millis() % 10) as u64;
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(15)) => {},
                _ = subsys.on_shutdown_requested() => {
                    break;
                }
            };
            count += 1;

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
                .filter_map(|(i, p)| if p.tx.is_closed() { Some(i) } else { None })
                .collect();

            to_rem.iter().rev().for_each(|i| {
                ps.remove(*i);
            });

            // only update server stats every 10th iteration (every 2.5 minutes)
            if count % 10 == 0 {
                let nb_players = ps.len();
                if nb_players > 0 {
                    let _ = update_server_stats(&pool, nb_players as i32).await;
                    let nb_players_live = get_server_info(&pool).await.unwrap_or_default();
                    let server_info = router::Response::ServerInfo { nb_players_live };
                    let overview = get_global_overview(&pool).await;
                    ps.iter()
                        .map(|p| {
                            let _ = p.tx.send(ToPlayer::Top3(top3.clone()));
                            let _ = p.tx.send(ToPlayer::Send(server_info.clone()));
                            if let Ok(j) = &overview {
                                let _ = p.tx.send(ToPlayer::Send(router::Response::GlobalOverview { j: j.clone() }));
                            }
                        })
                        .for_each(drop);
                }
            }
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
        let players = self.players.clone();
        let mut inbox = self.rx.take().expect("player mgr inbox");
        subsys.start(SubsystemBuilder::new("PlayerMgr_Inbox", move |a| async move {
            loop {
                tokio::select! {
                    _ = a.on_shutdown_requested() => break,
                    msg = inbox.recv() => {
                        let Some(msg) = msg else { break };
                        let mut players = players.lock().await;
                        deliver_player_mgr_message(&mut players, msg).await;
                    }
                }
            }
            Ok::<(), miette::Error>(())
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
                    ps.iter().map(|p| p.tx.send(ToPlayer::Shutdown())).for_each(drop);
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
                    let wayfinder_map_uid = Arc::new(Mutex::new(None));
                    subsys.start(SubsystemBuilder::new("Player", {
                        let wayfinder_map_uid = wayfinder_map_uid.clone();
                        move |a| XPlayer::new(stream, permit, addr, pool, tx, p_rx, wayfinder_map_uid).run(a)
                    }));
                    self.players.lock().await.push(PlayerDelivery { tx: p_tx, wayfinder_map_uid });
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
    /// to manager
    mgr_tx: UnboundedSender<ToPlayerMgr>,
    addr: SocketAddr,
    /// from manager
    p_rx: Option<UnboundedReceiver<ToPlayer>>,

    session: OnceCell<LoginSession>,
    context: Mutex<Option<PlayerCtx>>,
    context_id: Mutex<Option<Uuid>>,
    ip_address: String,

    queue_rx: Option<UnboundedReceiver<Response>>,
    pub queue_tx: UnboundedSender<Response>,

    /// Real UID of the Wayfinder map this connection is on, shared with
    /// map-scoped delivery. Adopted from the bodies of requests 81/213/214 and
    /// cleared when a context report shows the player changed map.
    wayfinder_map_uid: Arc<Mutex<Option<String>>>,
    /// Hat unlock validations this connection has deferred; bounds the spawned
    /// work and holds request 213 back until they answer it.
    pending_hat_unlocks: Arc<PendingHatUnlocks>,
    /// Tracks map transitions for the on-ping automatic responses.
    last_ping_map_uid: Mutex<Option<String>>,
}

impl XPlayer {
    pub fn new(
        stream: TcpStream,
        permit: OwnedSemaphorePermit,
        addr: SocketAddr,
        pool: Arc<Pool<Postgres>>,
        mgr_tx: UnboundedSender<ToPlayerMgr>,
        p_rx: UnboundedReceiver<ToPlayer>,
        wayfinder_map_uid: Arc<Mutex<Option<String>>>,
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
            wayfinder_map_uid,
            pending_hat_unlocks: Default::default(),
            last_ping_map_uid: Default::default(),
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
        let wayfinder_map_uid = self.wayfinder_map_uid.clone();
        let pending_hat_unlocks = self.pending_hat_unlocks.clone();
        // The `ReportContext` map id, kept only to notice that the player
        // changed map. It is the true UID on Deep Dip maps and a 30-char MD5
        // prefix everywhere else, so it can never be compared against a real
        // Wayfinder UID -- only against its own previous value.
        let mut context_map_uid: Option<String> = None;
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
                            let result = XPlayer::update_context(
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
                            .await;
                            if result.is_ok() {
                                // A context map change means the player left the
                                // Wayfinder map they were on -- unless the new
                                // context id names that same map. Request 81 and
                                // the context report race on a map load, so a
                                // context report that merely caught up with a
                                // map 81 already adopted must not clear it and
                                // broadcast a spurious leave push.
                                let context_uid = map.as_ref().map(|map| map.uid.as_str());
                                if update_last_map(&mut context_map_uid, context_uid) {
                                    if let Err(error) = XPlayer::apply_context_map_change(
                                        &pool,
                                        &mgr_tx,
                                        &wayfinder_map_uid,
                                        user_id,
                                        session.display_name(),
                                        context_uid,
                                    )
                                    .await
                                    {
                                        warn!("Failed to broadcast hat context transition for user {}: {:?}", user_id, error);
                                    }
                                }
                            }
                            result
                        }
                        _ => Err(format!("Invalid sf/mi: {}/{}", sf, mi).into()),
                    }
                }
                Request::ReportGCNodMsg { data } => Ok(()), // XPlayer::on_report_gcnod_msg(&pool, session_token, ctx_id.as_ref(), &data).await,
                Request::Ping {} => {
                    let r = queue_tx.send(Response::Ping {}).map_err(Into::into);
                    if let Some(ctx) = ctx.as_ref() {
                        let r2 = self.check_regular_on_ping(&pool, &queue_tx, ctx).await;
                        if let Err(e) = r2 {
                            warn!("Error checking regular on ping (user={}): {:?}", session.display_name(), e);
                        }
                    }
                    r
                }
                Request::ReportVehicleState { vel, pos, rotq } => {
                    XPlayer::report_vehicle_state(&pool, session_token, user_id, ctx.as_ref(), ctx_id.as_ref(), pos, rotq, vel).await
                }
                Request::ReportRespawn { race_time } => XPlayer::report_respawn(&pool, session_token, race_time).await,
                Request::ReportFinish { race_time } => XPlayer::report_finish(&pool, session_token, race_time).await,
                Request::ReportFallStart {
                    floor,
                    pos,
                    speed,
                    start_time,
                } => Ok(()), // XPlayer::on_report_fall_start(&pool, user_id, session_token, floor as i32, pos.into(), speed, start_time as i32).await,
                Request::ReportFallEnd { floor, pos, end_time } => {
                    // XPlayer::on_report_fall_end(&pool, user_id, floor as i32, pos.into(), end_time as i32).await
                    Ok(())
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
                // custom map aux spec upload
                Request::ReportCustomMapAuxSpec { id, name_id, spec } => {
                    XPlayer::report_custom_map_aux_spec(&pool, id, user_id, &name_id, &spec, &queue_tx).await
                }
                Request::DeleteCustomMapAuxSpec { id, name_id } => {
                    XPlayer::delete_custom_map_aux_spec(&pool, id, user_id, &name_id, &queue_tx).await
                }
                Request::ListCustomMapAuxSpecs { id } => XPlayer::list_custom_map_aux_spec(&pool, id, user_id, &queue_tx).await,

                Request::ReportMapStats { uid, stats } => XPlayer::report_map_stats(&pool, user_id, uid, stats).await,

                Request::ReportMinigameAttempt {
                    uid,
                    slug,
                    spec_id,
                    elapsed_ms,
                    status,
                } => XPlayer::report_minigame_attempt(&pool, user_id, uid, slug, spec_id, elapsed_ms, status).await,
                Request::ReportMinigameMap { uid, spec_id } => {
                    let result = XPlayer::report_minigame_map(&pool, &mgr_tx, user_id, uid.clone(), spec_id).await;
                    if let Err(error) =
                        XPlayer::adopt_wayfinder_map(&pool, &mgr_tx, &wayfinder_map_uid, user_id, session.display_name(), &uid).await
                    {
                        warn!("Failed to track Wayfinder map {} for user {}: {:?}", uid, user_id, error);
                    }
                    result
                }
                Request::ReportCollectibleUnlock { uid, slug, spec_id } => {
                    XPlayer::report_collectible_unlock(&pool, user_id, uid, slug, spec_id).await
                }
                Request::ReportCollectibleCollect {
                    uid,
                    slug,
                    spec_id,
                    x,
                    y,
                    z,
                    source,
                } => XPlayer::report_collectible_collect(&pool, user_id, uid, slug, spec_id, x, y, z, source).await,
                Request::ReportHatUnlock { uid, slug, spec_id } => {
                    XPlayer::report_hat_unlock(&pool, &queue_tx, &pending_hat_unlocks, user_id, uid, slug, spec_id).await
                }
                Request::ReportHatEquip { uid, slug } => {
                    // Equips never adopt a map: the player has to already be
                    // tracked on it through 81/213/214.
                    let current = wayfinder_map_uid.lock().await.clone();
                    XPlayer::report_hat_equip(&pool, &mgr_tx, &queue_tx, user_id, current.as_deref(), uid, slug).await
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

                Request::GetMyProfile {} => XPlayer::get_users_profile(&pool, &user_id.to_string(), &queue_tx).await,
                Request::SetMyProfile { mut body } => {
                    body["wsid"] = serde_json::Value::String(user_id.to_string());
                    body["name"] = serde_json::Value::String(session.display_name().to_string());
                    XPlayer::set_users_profile(&pool, user_id, body).await
                }
                Request::GetMyPreferences {} => XPlayer::get_users_preferences(&pool, user_id, &queue_tx).await,
                Request::SetMyPreferences { body } => XPlayer::set_users_preferences(&pool, user_id, body).await,
                Request::GetUsersProfile { wsid } => XPlayer::get_users_profile(&pool, &wsid, &queue_tx).await,

                Request::GetPlayersSpecInfo { uid, wsid } => XPlayer::get_players_spec_info(&pool, &uid, &wsid, &queue_tx).await,

                // get arb maps
                Request::GetMapOverview { uid } => XPlayer::get_map_overview(&pool, &queue_tx, uid).await,
                Request::GetMapLB { uid, start, end } => XPlayer::send_map_lb(&pool, &queue_tx, &uid, start as i64, end as i64).await,
                Request::GetMapLive { uid } => XPlayer::send_map_live(&pool, &queue_tx, uid).await,
                Request::GetMapMyRank { uid } => XPlayer::get_map_rank(&pool, uid, user_id, &queue_tx).await,
                Request::GetMapRank { uid, wsid } => {
                    if let Ok(user_id) = Uuid::from_str(&wsid) {
                        XPlayer::get_map_rank(&pool, uid, &user_id, &queue_tx).await
                    } else {
                        Ok(())
                    }
                }
                Request::GetMinigameLB { uid, slug, start, end } => {
                    XPlayer::send_minigame_lb(&pool, &queue_tx, uid, slug, start as i64, end as i64).await
                }
                Request::GetMinigameMyRank { uid, slug } => XPlayer::get_minigame_my_rank(&pool, uid, slug, user_id, &queue_tx).await,
                Request::GetMinigameList { uid } => XPlayer::send_minigame_list(&pool, &queue_tx, uid).await,
                Request::GetCollectibleList { uid } => XPlayer::send_collectible_list(&pool, &queue_tx, uid).await,
                Request::GetCollectibleMine { uid } => XPlayer::send_collectible_mine(&pool, &queue_tx, uid, user_id).await,
                Request::GetHatsMine { uid } => {
                    // A failed join push must not suppress the hat response:
                    // the tracked map has already moved, so fall back to it.
                    let current =
                        XPlayer::adopt_wayfinder_map_or_tracked(&pool, &mgr_tx, &wayfinder_map_uid, user_id, session.display_name(), &uid)
                            .await;
                    XPlayer::send_hats_mine(&pool, &queue_tx, &pending_hat_unlocks, current.as_deref(), uid, user_id).await
                }
                Request::GetHatsOnMap { uid } => {
                    let current =
                        XPlayer::adopt_wayfinder_map_or_tracked(&pool, &mgr_tx, &wayfinder_map_uid, user_id, session.display_name(), &uid)
                            .await;
                    XPlayer::send_hats_on_map(&pool, &queue_tx, current.as_deref(), uid).await
                }

                Request::GetSecretAssets {} => XPlayer::get_secret_assets(&pool, user_id, &queue_tx).await,
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
            // info!("done send global lb");
        });
        let pool = self.pool.clone();
        let queue_tx = self.queue_tx.clone();
        tracker.spawn(async move {
            if let Ok(j) = get_global_overview(&pool).await {
                let _ = queue_tx.send(Response::GlobalOverview { j });
            };
            // info!("done send global overview");
        });
        let pool = self.pool.clone();
        let queue_tx = self.queue_tx.clone();
        tracker.spawn(async move {
            if let Ok(j) = get_server_info(&pool).await {
                let _ = queue_tx.send(Response::ServerInfo { nb_players_live: j });
            };
            // info!("done send server info");
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
        let session = create_session(&self.pool, user.id(), &self.ip_address).await?;
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
        let (s, u) = resume_session(&self.pool, &session_token, &self.ip_address).await?;
        Ok(LoginSession {
            user: u,
            session: s,
            resumed: true,
            plugin_ver,
        })
    }

    async fn check_regular_on_ping(
        &self,
        pool: &Pool<Postgres>,
        queue_tx: &UnboundedSender<Response>,
        ctx: &PlayerCtx,
    ) -> Result<(), api_error::Error> {
        // check if this map is new
        let map_changed: bool = self.on_ping_check_update_last_map(ctx.map.as_ref()).await;

        // do nothing if not in a map
        let map = match ctx.map.as_ref() {
            Some(map) if (20..30).contains(&map.uid.len()) => map,
            _ => {
                return Ok(());
            }
        };
        let map_uid = map.uid.clone();
        // pings come in every 6.7s with current version and 9-10s with next version (0.5.6).
        let cache_ttl = Duration::from_millis(9000);
        // add some tolerance for sending updates to avoid missing any (better to send twice than not at all)
        let send_ttl = Duration::from_millis(11000);

        // send cached info (note: this will also update it if it's old)
        // map top 10 LB, note: pings are sent from the client every 9-10 seconds (random spread).
        let (age, entries) = CACHED_LB_INFO
            .get_or_insert_with(&map_uid, cache_ttl, async || {
                queries::get_map_leaderboard(pool, &map_uid, 0, 10).await
            })
            .await?;
        if map_changed || age <= send_ttl {
            queue_tx.send(Response::MapLB {
                uid: map_uid.to_string(),
                entries,
            })?;
        }
        // map top 10 live players
        let (age, players) = CACHED_LIVE_INFO
            .get_or_insert_with(&map.uid, cache_ttl, async || {
                queries::get_map_live_heights_top_n(pool, &map_uid, 10).await
            })
            .await?;
        if map_changed || age <= send_ttl {
            queue_tx.send(Response::MapLivePlayers {
                uid: map_uid.to_string(),
                players,
            })?;
        }

        // // send map LB info always
        // Self::send_map_lb(pool, queue_tx, &map.uid, 0, 10).await?;
        Ok(())
    }

    async fn on_ping_check_update_last_map(&self, map: Option<&Map>) -> bool {
        let mut last_map = self.last_ping_map_uid.lock().await;
        update_last_map(&mut last_map, map.map(|map| map.uid.as_str()))
    }

    async fn report_map_stats(pool: &Pool<Postgres>, user_id: &Uuid, uid: String, stats: serde_json::Value) -> Result<(), ApiError> {
        queries::custom_maps::report_map_stats(pool, user_id, &uid, stats).await?;
        Ok(())
    }

    async fn get_players_spec_info(
        pool: &Pool<Postgres>,
        uid: &str,
        wsid: &str,
        queue_tx: &UnboundedSender<Response>,
    ) -> Result<(), ApiError> {
        let wsid = Uuid::from_str(wsid).map_err(|_| ApiError::StrErr("Invalid WSID".to_string()))?;
        let (total_map_time, updated_at) = match queries::custom_maps::get_players_spec_info(pool, uid, &wsid).await {
            Ok(r) => r,
            Err(e) => {
                warn!("Error getting players spec info: {:?}", e);
                return Ok(());
            }
        };
        let now_ts = updated_at.timestamp();
        queue_tx.send(Response::PlayersSpecInfo {
            uid: uid.to_string(),
            wsid: wsid.to_string(),
            total_map_time,
            now_ts,
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod wayfinder_delivery_tests {
    use super::{
        context_uid_refers_to_map, deliver_player_mgr_message, delivery_matches_map, obfuscated_map_uid, update_last_map,
        PendingHatUnlocks, PlayerDelivery, XPlayer, MAX_CONCURRENT_HAT_VALIDATIONS, MAX_PENDING_HAT_UNLOCKS,
    };
    use crate::nadeo::MapAuthorVerifier;
    use crate::player::ToPlayer;
    use crate::router::{Response, ToPlayerMgr};
    use serde_json::json;
    use sqlx::{postgres::PgPoolOptions, Pool, Postgres};
    use std::{sync::Arc, time::Duration};
    use tokio::{
        sync::{mpsc::unbounded_channel, Notify},
        time::{sleep, timeout},
    };
    use uuid::Uuid;

    struct FixedAuthorVerifier(Uuid);

    impl MapAuthorVerifier for FixedAuthorVerifier {
        fn author_id_for_uid<'a>(
            &'a self,
            _account_id: &'a Uuid,
            _map_uid: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Uuid, String>> + Send + 'a>> {
            Box::pin(async move { Ok(self.0) })
        }
    }

    /// One `finishThisMap` hat, materialized against a verified binding. No
    /// unlock: the hat is available but not yet earned.
    async fn seed_hat_catalog(pool: &Pool<Postgres>, user_id: &Uuid, map_uid: &str, name_id: &str) -> i64 {
        seed_hat_catalog_slugs(pool, user_id, map_uid, name_id, &["Hat-1"]).await
    }

    async fn seed_hat_catalog_slugs(pool: &Pool<Postgres>, user_id: &Uuid, map_uid: &str, name_id: &str, slugs: &[&str]) -> i64 {
        let hats: Vec<_> = slugs
            .iter()
            .map(|slug| json!({"slug": slug, "unlock": {"finishThisMap": true}}))
            .collect();
        let spec = json!({ "hats": hats });
        let spec_id = crate::queries::custom_map_aux_specs::upsert_spec_and_catalogs(pool, user_id, name_id, &spec)
            .await
            .expect("insert join-broadcast spec");
        crate::queries::hats::bind_map_from_catalog(pool, map_uid, spec_id)
            .await
            .expect("bind join-broadcast hats");
        sqlx::query!(
            r#"INSERT INTO map_listing_bind
                   (map_uid, spec_id, verified_author_id, author_verified_at,
                    materialized_spec_updated_at, materialized_spec_hash)
               SELECT $1, id, user_id, NOW(), updated_at, spec_hash
               FROM custom_map_aux_specs WHERE id = $2"#,
            map_uid,
            spec_id
        )
        .execute(pool)
        .await
        .expect("mark join-broadcast snapshot materialized");
        spec_id
    }

    async fn seed_unlocked_hat(pool: &Pool<Postgres>, user_id: &Uuid, map_uid: &str, name_id: &str) -> i64 {
        let spec_id = seed_hat_catalog(pool, user_id, map_uid, name_id).await;
        sqlx::query!(
            r#"INSERT INTO hat_unlock (user_id, hat_key, map_uid, slug, unlocked_at)
               VALUES ($1, substring(md5($2 || ':h:Hat-1') from 1 for 16), $2, 'Hat-1', NOW())"#,
            user_id,
            map_uid
        )
        .execute(pool)
        .await
        .expect("insert join-broadcast unlock");
        spec_id
    }

    async fn seed_equipped_hat(pool: &Pool<Postgres>, user_id: &Uuid, map_uid: &str, name_id: &str) -> i64 {
        let spec_id = seed_unlocked_hat(pool, user_id, map_uid, name_id).await;
        assert!(crate::queries::hats::report_equip(pool, user_id, map_uid, "Hat-1")
            .await
            .expect("persist equipped hat")
            .is_some());
        spec_id
    }

    async fn seed_user(pool: &Pool<Postgres>, display_name: &str) -> Uuid {
        let user_id = Uuid::now_v7();
        sqlx::query!(
            "INSERT INTO users (web_services_user_id, display_name) VALUES ($1, $2)",
            user_id,
            display_name
        )
        .execute(pool)
        .await
        .expect("insert test user");
        user_id
    }

    /// The plugin's `ReportContext` map id for any map not named like Deep Dip.
    async fn context_uid_for(pool: &Pool<Postgres>, map_uid: &str) -> String {
        sqlx::query!(r#"SELECT substring(md5($1) from 1 for 30) AS "prefix!""#, map_uid)
            .fetch_one(pool)
            .await
            .expect("compute context uid")
            .prefix
    }

    /// The plugin obfuscates the map id in `ReportContext` unless the map looks
    /// like Deep Dip, so a context report naming the map already tracked can
    /// arrive in either form. Both must read as "same map".
    #[test]
    fn context_uid_recognises_both_forms_of_the_tracked_map() {
        let map_uid = "SomeWayfinderMapUid00000000";
        let obfuscated = obfuscated_map_uid(map_uid);
        assert_eq!(obfuscated.len(), 30);
        assert!(context_uid_refers_to_map(Some(map_uid), map_uid), "true uid (Deep Dip maps)");
        assert!(
            context_uid_refers_to_map(Some(&obfuscated), map_uid),
            "md5 prefix (every other map)"
        );
        assert!(
            context_uid_refers_to_map(Some(&obfuscated.to_ascii_uppercase()), map_uid),
            "Openplanet's Crypto::MD5 hex case is not pinned anywhere, so the compare is case-insensitive"
        );
        assert!(!context_uid_refers_to_map(Some("AnotherWayfinderMapUid00000"), map_uid));
        assert!(
            !context_uid_refers_to_map(Some(&obfuscated_map_uid("AnotherWayfinderMapUid00000")), map_uid),
            "another map's md5 prefix is a real change"
        );
        assert!(!context_uid_refers_to_map(None, map_uid), "no map means the player left");
    }

    /// H2: request 81 and the context report race on a map load. A context
    /// report that catches up with the map 81 already adopted must not clear
    /// the tracked map or broadcast a leave push.
    #[tokio::test]
    async fn context_report_for_the_tracked_map_does_not_clear_it() {
        dotenv::dotenv().ok();
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for delivery tests");
        let pool = PgPoolOptions::new().max_connections(3).connect(&url).await.expect("connect");
        let user_id = seed_user(&pool, "context-race-player").await;
        let suffix = Uuid::new_v4().simple().to_string();
        let map_uid: String = format!("CtxRace{suffix}").chars().take(27).collect();
        let other_map_uid: String = format!("CtxOther{suffix}").chars().take(27).collect();
        let name_id = format!("ctx-race-{}", &suffix[..8]);
        seed_equipped_hat(&pool, &user_id, &map_uid, &name_id).await;

        let tracked: Arc<tokio::sync::Mutex<Option<String>>> = Arc::new(tokio::sync::Mutex::new(None));
        let (mgr_tx, mut mgr_rx) = unbounded_channel();

        // Request 81 lands first and adopts the real uid (join push).
        XPlayer::adopt_wayfinder_map(&pool, &mgr_tx, &tracked, &user_id, "context-race-player", &map_uid)
            .await
            .expect("adopt from request 81");
        assert!(mgr_rx.try_recv().is_ok(), "join push for the equipped hat");

        // The context report for the same map arrives afterwards, obfuscated.
        let obfuscated = obfuscated_map_uid(&map_uid);
        assert_eq!(
            obfuscated,
            context_uid_for(&pool, &map_uid).await,
            "server reproduces the plugin's obfuscation"
        );
        assert_eq!(
            XPlayer::apply_context_map_change(&pool, &mgr_tx, &tracked, &user_id, "context-race-player", Some(&obfuscated))
                .await
                .expect("obfuscated context for the tracked map"),
            Some(map_uid.clone())
        );
        assert!(
            mgr_rx.try_recv().is_err(),
            "a context report that caught up with request 81 must not broadcast a leave push"
        );

        // The unobfuscated form (Deep Dip named maps) is equally a no-op.
        assert_eq!(
            XPlayer::apply_context_map_change(&pool, &mgr_tx, &tracked, &user_id, "context-race-player", Some(&map_uid))
                .await
                .expect("true-uid context for the tracked map"),
            Some(map_uid.clone())
        );
        assert!(mgr_rx.try_recv().is_err());
        assert_eq!(tracked.lock().await.as_deref(), Some(map_uid.as_str()));

        // A genuine map change still clears and pushes the leave event.
        assert_eq!(
            XPlayer::apply_context_map_change(
                &pool,
                &mgr_tx,
                &tracked,
                &user_id,
                "context-race-player",
                Some(&obfuscated_map_uid(&other_map_uid)),
            )
            .await
            .expect("real map change"),
            None
        );
        match mgr_rx.try_recv().expect("real map change emits the leave push") {
            ToPlayerMgr::BroadcastToMap {
                map_uid: target,
                response: Response::HatsOnMapUpdate { hats, .. },
            } => {
                assert_eq!(target, map_uid);
                assert!(hats[0].slug.is_empty(), "leave push unequips");
            }
            _ => panic!("unexpected broadcast"),
        }
        assert!(tracked.lock().await.is_none());

        sqlx::query!("DELETE FROM map_listing_bind WHERE map_uid = $1", map_uid)
            .execute(&pool)
            .await
            .expect("cleanup context-race binding");
        sqlx::query!("DELETE FROM users WHERE web_services_user_id = $1", user_id)
            .execute(&pool)
            .await
            .expect("cleanup context-race player");
    }

    struct SlowRecordVerifier {
        delay: Duration,
        started: Arc<Notify>,
    }

    impl crate::nadeo::OfficialRecordVerifier for SlowRecordVerifier {
        fn has_official_record<'a>(
            &'a self,
            _account_id: &'a Uuid,
            _map_uid: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, String>> + Send + 'a>> {
            Box::pin(async move {
                self.started.notify_one();
                sleep(self.delay).await;
                Ok(true)
            })
        }
    }

    /// H1: the plugin pushes 84 and 213 back to back on one FIFO and treats the
    /// 213 as authoritative, persisting anything it reports as locked. With the
    /// validation deferred, the inline 213 would read the pre-unlock state, so
    /// it is withheld and the validation answers it once the unlock commits.
    #[tokio::test]
    async fn deferred_unlock_answers_the_213_that_followed_it() {
        dotenv::dotenv().ok();
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for delivery tests");
        let pool = PgPoolOptions::new().max_connections(4).connect(&url).await.expect("connect");
        let user_id = seed_user(&pool, "deferred-unlock-player").await;
        let suffix = Uuid::new_v4().simple().to_string();
        let map_uid: String = format!("DeferUnlock{suffix}").chars().take(27).collect();
        let name_id = format!("defer-{}", &suffix[..8]);
        let spec_id = seed_hat_catalog(&pool, &user_id, &map_uid, &name_id).await;

        let pending: Arc<PendingHatUnlocks> = Default::default();
        let (queue_tx, mut queue_rx) = unbounded_channel();
        let started = Arc::new(Notify::new());
        let verifier: Arc<dyn crate::nadeo::OfficialRecordVerifier> = Arc::new(SlowRecordVerifier {
            delay: Duration::from_millis(400),
            started: started.clone(),
        });

        // Request 84.
        XPlayer::report_hat_unlock_with_verifier(
            &pool,
            verifier,
            &queue_tx,
            &pending,
            &user_id,
            map_uid.clone(),
            "Hat-1".into(),
            spec_id,
        )
        .await
        .expect("84 is accepted and deferred");
        timeout(Duration::from_secs(5), started.notified())
            .await
            .expect("validation actually started");
        assert!(pending.any_for_map(&map_uid), "the validation is registered as in flight");

        // Request 213, dequeued while the validation is still running. Answering
        // it now would tell the client the hat is locked and it would persist that.
        XPlayer::send_hats_mine(&pool, &queue_tx, &pending, Some(&map_uid), map_uid.clone(), &user_id)
            .await
            .expect("213 is handled");
        assert!(
            queue_rx.try_recv().is_err(),
            "213 must be withheld while its 84 is still being validated"
        );

        // The validation's completion push is the answer, and it reports the unlock.
        match timeout(Duration::from_secs(10), queue_rx.recv())
            .await
            .expect("the deferred validation answers the 213")
            .expect("channel stays open")
        {
            Response::HatsMine { uid, items, .. } => {
                assert_eq!(uid, map_uid);
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].slug, "Hat-1");
                assert!(items[0].unlocked_at.is_some(), "the client must end up unlocked, not relocked");
            }
            other => panic!("unexpected response: {:?}", other.name()),
        }
        assert!(!pending.any_for_map(&map_uid), "the in-flight guard is released");

        // With nothing pending, 213 is answered inline again.
        XPlayer::send_hats_mine(&pool, &queue_tx, &pending, Some(&map_uid), map_uid.clone(), &user_id)
            .await
            .expect("213 is answered once nothing is pending");
        assert!(matches!(
            queue_rx.try_recv().expect("inline 213"),
            Response::HatsMine { ref uid, .. } if uid == &map_uid
        ));

        sqlx::query!("DELETE FROM map_listing_bind WHERE map_uid = $1", map_uid)
            .execute(&pool)
            .await
            .expect("cleanup deferred-unlock binding");
        sqlx::query!("DELETE FROM users WHERE web_services_user_id = $1", user_id)
            .execute(&pool)
            .await
            .expect("cleanup deferred-unlock player");
    }

    /// M1: a client streaming 84s must not be able to spawn unbounded work.
    #[tokio::test]
    async fn concurrent_unlock_validations_are_bounded_and_coalesced() {
        dotenv::dotenv().ok();
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for delivery tests");
        let pool = PgPoolOptions::new().max_connections(4).connect(&url).await.expect("connect");
        let user_id = seed_user(&pool, "unlock-flood-player").await;
        let suffix = Uuid::new_v4().simple().to_string();
        let map_uid: String = format!("Flood{suffix}").chars().take(27).collect();
        let name_id = format!("flood-{}", &suffix[..8]);
        let spec_id = seed_hat_catalog(&pool, &user_id, &map_uid, &name_id).await;

        let pending: Arc<PendingHatUnlocks> = Default::default();
        let (queue_tx, mut queue_rx) = unbounded_channel();
        let started = Arc::new(Notify::new());
        let verifier: Arc<dyn crate::nadeo::OfficialRecordVerifier> = Arc::new(SlowRecordVerifier {
            delay: Duration::from_millis(300),
            started: started.clone(),
        });

        for _ in 0..8 {
            XPlayer::report_hat_unlock_with_verifier(
                &pool,
                verifier.clone(),
                &queue_tx,
                &pending,
                &user_id,
                map_uid.clone(),
                "Hat-1".into(),
                spec_id,
            )
            .await
            .expect("84 flood is accepted");
        }
        assert_eq!(
            pending.owed.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).len(),
            1,
            "repeats of the same map+hat coalesce onto the pending validation"
        );

        // Exactly one validation ran, so exactly one completion push arrives.
        assert!(matches!(
            timeout(Duration::from_secs(10), queue_rx.recv())
                .await
                .expect("the single validation answers")
                .expect("channel stays open"),
            Response::HatsMine { .. }
        ));
        timeout(Duration::from_secs(5), async {
            while pending.any_for_map(&map_uid) {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("in-flight guard released");
        assert!(queue_rx.try_recv().is_err(), "coalesced 84s do not each push a 213");

        sqlx::query!("DELETE FROM map_listing_bind WHERE map_uid = $1", map_uid)
            .execute(&pool)
            .await
            .expect("cleanup flood binding");
        sqlx::query!("DELETE FROM users WHERE web_services_user_id = $1", user_id)
            .execute(&pool)
            .await
            .expect("cleanup flood player");
    }

    struct PanickingRecordVerifier;

    impl crate::nadeo::OfficialRecordVerifier for PanickingRecordVerifier {
        fn has_official_record<'a>(
            &'a self,
            _account_id: &'a Uuid,
            _map_uid: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, String>> + Send + 'a>> {
            Box::pin(async move { panic!("deliberate validation panic") })
        }
    }

    /// L5: a validation that panics still owes the 213 it caused to be
    /// withheld. Without an answer the client keeps its optimistic state until
    /// it happens to send another 213 -- next equip, unlock, or map load.
    #[tokio::test]
    async fn a_panicking_validation_still_answers_the_withheld_213() {
        dotenv::dotenv().ok();
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for delivery tests");
        let pool = PgPoolOptions::new().max_connections(3).connect(&url).await.expect("connect");
        let user_id = seed_user(&pool, "panicking-validation-player").await;
        let suffix = Uuid::new_v4().simple().to_string();
        let map_uid: String = format!("Panic{suffix}").chars().take(27).collect();
        let name_id = format!("panic-{}", &suffix[..8]);
        let spec_id = seed_hat_catalog(&pool, &user_id, &map_uid, &name_id).await;

        let pending: Arc<PendingHatUnlocks> = Default::default();
        let (queue_tx, mut queue_rx) = unbounded_channel();
        let verifier: Arc<dyn crate::nadeo::OfficialRecordVerifier> = Arc::new(PanickingRecordVerifier);

        XPlayer::report_hat_unlock_with_verifier(
            &pool,
            verifier,
            &queue_tx,
            &pending,
            &user_id,
            map_uid.clone(),
            "Hat-1".into(),
            spec_id,
        )
        .await
        .expect("84 is accepted");

        match timeout(Duration::from_secs(20), queue_rx.recv())
            .await
            .expect("a panicking validation still answers the withheld 213")
            .expect("channel stays open")
        {
            Response::HatsMine { uid, items, .. } => {
                assert_eq!(uid, map_uid);
                assert_eq!(items.len(), 1);
                assert!(items[0].unlocked_at.is_none(), "the panicking validation granted nothing");
            }
            other => panic!("unexpected response: {:?}", other.name()),
        }
        timeout(Duration::from_secs(5), async {
            while pending.any_for_map(&map_uid) {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the pending entry is released");

        sqlx::query!("DELETE FROM map_listing_bind WHERE map_uid = $1", map_uid)
            .execute(&pool)
            .await
            .expect("cleanup panic binding");
        sqlx::query!("DELETE FROM users WHERE web_services_user_id = $1", user_id)
            .execute(&pool)
            .await
            .expect("cleanup panic player");
    }

    /// H3/L4: the pending set is "an answer is owed", not a concurrency cap. A
    /// normal multi-hat batch must never be refused, and only the release that
    /// drains a map may push the answer.
    #[test]
    fn pending_hat_unlocks_dedupe_bound_and_drain() {
        let pending: Arc<PendingHatUnlocks> = Default::default();
        let mut owed: Vec<_> = (0..MAX_PENDING_HAT_UNLOCKS)
            .map(|i| {
                pending
                    .begin("map-a", &format!("Hat-{i}"))
                    .expect("a multi-hat batch is queued, never refused")
            })
            .collect();
        assert!(
            MAX_PENDING_HAT_UNLOCKS > MAX_CONCURRENT_HAT_VALIDATIONS,
            "the spam bound must not double as the Nadeo concurrency cap"
        );
        assert!(pending.begin("map-a", "Hat-0").is_none(), "a repeat coalesces");
        assert!(pending.begin("map-a", "Hat-spam").is_none(), "the spam bound still applies");
        assert!(pending.any_for_map("map-a"));
        assert!(!pending.any_for_map("map-b"));

        // Only the final release drains the map, so only it pushes the 213.
        let last = owed.len() - 1;
        for (index, entry) in owed.iter_mut().enumerate() {
            assert_eq!(
                entry.release(),
                index == last,
                "a release with siblings still pending must not push"
            );
            assert!(!entry.release(), "releasing twice is a no-op");
        }
        assert!(!pending.any_for_map("map-a"));

        // Dropping without releasing still clears the entry.
        {
            let _dropped = pending.begin("map-a", "Hat-0").expect("re-registers after draining");
            assert!(pending.any_for_map("map-a"));
        }
        assert!(!pending.any_for_map("map-a"));
    }

    /// H3: `RecomputeUnlocks` fires 84 for every newly-satisfied hat in one
    /// synchronous pass, so a map granting three hats at once is ordinary
    /// traffic. None may be discarded, and no push may report one of them as
    /// locked -- the client persists whatever the 213 says.
    #[tokio::test]
    async fn a_batch_of_distinct_hat_unlocks_is_queued_not_dropped() {
        dotenv::dotenv().ok();
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for delivery tests");
        let pool = PgPoolOptions::new().max_connections(4).connect(&url).await.expect("connect");
        let user_id = seed_user(&pool, "batch-unlock-player").await;
        let suffix = Uuid::new_v4().simple().to_string();
        let map_uid: String = format!("Batch{suffix}").chars().take(27).collect();
        let name_id = format!("batch-{}", &suffix[..8]);
        let slugs = ["Hat-1", "Hat-2", "Hat-3"];
        assert!(
            slugs.len() > MAX_CONCURRENT_HAT_VALIDATIONS,
            "the batch has to exceed the Nadeo concurrency cap to be a regression test"
        );
        let spec_id = seed_hat_catalog_slugs(&pool, &user_id, &map_uid, &name_id, &slugs).await;

        let pending: Arc<PendingHatUnlocks> = Default::default();
        let (queue_tx, mut queue_rx) = unbounded_channel();
        let verifier: Arc<dyn crate::nadeo::OfficialRecordVerifier> = Arc::new(SlowRecordVerifier {
            delay: Duration::from_millis(150),
            started: Arc::new(Notify::new()),
        });

        // What the plugin actually sends: 84 then 213, per newly-met hat.
        for slug in slugs {
            XPlayer::report_hat_unlock_with_verifier(
                &pool,
                verifier.clone(),
                &queue_tx,
                &pending,
                &user_id,
                map_uid.clone(),
                slug.into(),
                spec_id,
            )
            .await
            .expect("84 is accepted");
            XPlayer::send_hats_mine(&pool, &queue_tx, &pending, Some(&map_uid), map_uid.clone(), &user_id)
                .await
                .expect("213 is handled");
        }
        assert_eq!(
            pending.owed.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).len(),
            slugs.len(),
            "every distinct hat in the batch is queued"
        );

        // The batch drains into exactly one push, and it must not report any of
        // the three as locked.
        let mut pushes = 0;
        let first = timeout(Duration::from_secs(20), queue_rx.recv())
            .await
            .expect("the batch answers the withheld 213s")
            .expect("channel stays open");
        let mut check = |response: Response| match response {
            Response::HatsMine { uid, items, .. } => {
                assert_eq!(uid, map_uid);
                assert_eq!(items.len(), slugs.len());
                for item in &items {
                    assert!(
                        item.unlocked_at.is_some(),
                        "a push reported {} as locked; the client would persist that",
                        item.slug
                    );
                }
                pushes += 1;
            }
            other => panic!("unexpected response: {:?}", other.name()),
        };
        check(first);
        sleep(Duration::from_millis(400)).await;
        while let Ok(response) = queue_rx.try_recv() {
            check(response);
        }
        assert_eq!(pushes, 1, "only the release that drains the map pushes");

        let unlocked = sqlx::query!(
            "SELECT slug FROM hat_unlock WHERE user_id = $1 AND map_uid = $2 ORDER BY slug",
            user_id,
            map_uid
        )
        .fetch_all(&pool)
        .await
        .expect("read unlocks");
        assert_eq!(
            unlocked.iter().map(|row| row.slug.as_str()).collect::<Vec<_>>(),
            slugs,
            "every hat in the batch is validated and written"
        );

        sqlx::query!("DELETE FROM map_listing_bind WHERE map_uid = $1", map_uid)
            .execute(&pool)
            .await
            .expect("cleanup batch binding");
        sqlx::query!("DELETE FROM users WHERE web_services_user_id = $1", user_id)
            .execute(&pool)
            .await
            .expect("cleanup batch player");
    }

    /// M2: the deferred first-bind path (81 on a map that still needs Nadeo
    /// author verification) has to actually land the binding.
    #[tokio::test]
    async fn deferred_first_bind_materializes_the_map() {
        dotenv::dotenv().ok();
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for delivery tests");
        let pool = PgPoolOptions::new().max_connections(3).connect(&url).await.expect("connect");
        let user_id = seed_user(&pool, "deferred-bind-author").await;
        let suffix = Uuid::new_v4().simple().to_string();
        let map_uid: String = format!("DeferBind{suffix}").chars().take(27).collect();
        let name_id = format!("defer-bind-{}", &suffix[..8]);
        let spec = json!({"hats":[{"slug":"Hat-1","unlock":{"finishThisMap":true}}]});
        let spec_id = crate::queries::custom_map_aux_specs::upsert_spec_and_catalogs(&pool, &user_id, &name_id, &spec)
            .await
            .expect("insert deferred-bind spec");

        assert!(
            crate::queries::listed_maps::bind_requires_author_verification(&pool, &user_id, &map_uid, spec_id)
                .await
                .expect("verification is required for a first bind"),
            "an unbound map owned by the reporter must take the deferred path"
        );

        let (mgr_tx, _mgr_rx) = unbounded_channel();
        let verifier: Arc<dyn MapAuthorVerifier> = Arc::new(FixedAuthorVerifier(user_id));
        XPlayer::report_minigame_map_deferring(&pool, &mgr_tx, verifier, &user_id, map_uid.clone(), spec_id)
            .await
            .expect("81 is accepted and deferred");

        timeout(Duration::from_secs(10), async {
            loop {
                if crate::queries::listed_maps::verified_binding_spec_id(&pool, &map_uid)
                    .await
                    .expect("read binding")
                    .is_some()
                {
                    return;
                }
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the deferred bind materializes the map");
        assert_eq!(
            crate::queries::listed_maps::verified_binding_spec_id(&pool, &map_uid)
                .await
                .expect("read binding"),
            Some(spec_id)
        );
        assert!(
            !crate::queries::listed_maps::bind_requires_author_verification(&pool, &user_id, &map_uid, spec_id)
                .await
                .expect("verification no longer required"),
            "a verified binding takes the inline path on the next report"
        );

        sqlx::query!("DELETE FROM map_listing_bind WHERE map_uid = $1", map_uid)
            .execute(&pool)
            .await
            .expect("cleanup deferred-bind binding");
        sqlx::query!("DELETE FROM users WHERE web_services_user_id = $1", user_id)
            .execute(&pool)
            .await
            .expect("cleanup deferred-bind author");
    }

    /// The whole hat surface used to be gated on the `ReportContext` map id,
    /// which is a 30-char MD5 prefix on every map that is not Deep Dip. That
    /// made hats dead on exactly the Wayfinder maps they target. The real UID
    /// arrives in the bodies of 81/213/214, and that is what the gate, the
    /// broadcasts and the leave push now key on.
    #[tokio::test]
    async fn hat_surface_works_when_the_context_uid_is_an_md5_prefix() {
        dotenv::dotenv().ok();
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for delivery tests");
        let pool = PgPoolOptions::new().max_connections(3).connect(&url).await.expect("connect");
        let user_id = seed_user(&pool, "md5-context-player").await;
        let suffix = Uuid::new_v4().simple().to_string();
        let map_uid: String = format!("HatMd5Ctx{suffix}").chars().take(27).collect();
        let name_id = format!("hat-md5-{}", &suffix[..8]);
        seed_unlocked_hat(&pool, &user_id, &map_uid, &name_id).await;
        sqlx::query!(
            r#"INSERT INTO map_curr_heights (map_uid, user_id, height, pos, race_time)
               VALUES ($1, $2, 0.0, ARRAY[0.0, 0.0, 0.0], 0)"#,
            map_uid,
            user_id
        )
        .execute(&pool)
        .await
        .expect("mark player present on map");

        let context_uid = context_uid_for(&pool, &map_uid).await;
        assert_eq!(context_uid.len(), 30);
        assert_ne!(context_uid, map_uid);

        let tracked: Arc<tokio::sync::Mutex<Option<String>>> = Arc::new(tokio::sync::Mutex::new(None));
        let pending: Arc<PendingHatUnlocks> = Default::default();
        let (mgr_tx, mut mgr_rx) = unbounded_channel();
        let (queue_tx, mut queue_rx) = unbounded_channel();

        // A context report never supplies a real UID, so it must not be adopted.
        assert_eq!(
            XPlayer::adopt_wayfinder_map(&pool, &mgr_tx, &tracked, &user_id, "md5-context-player", &context_uid)
                .await
                .expect("context uid is ignored"),
            None
        );
        assert!(mgr_rx.try_recv().is_err());

        // Request 81 carries the true UID.
        let current = XPlayer::adopt_wayfinder_map(&pool, &mgr_tx, &tracked, &user_id, "md5-context-player", &map_uid)
            .await
            .expect("adopt real map uid");
        assert_eq!(current.as_deref(), Some(map_uid.as_str()));
        assert!(mgr_rx.try_recv().is_err(), "nothing is equipped yet, so no join push");

        // 213 is answered rather than silently dropped.
        XPlayer::send_hats_mine(&pool, &queue_tx, &pending, current.as_deref(), map_uid.clone(), &user_id)
            .await
            .expect("send hats mine");
        match queue_rx.try_recv().expect("213 is answered on an md5-context map") {
            Response::HatsMine { uid, equipped, items } => {
                assert_eq!(uid, map_uid);
                assert!(equipped.is_empty());
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].slug, "Hat-1");
                assert!(items[0].unlocked_at.is_some());
            }
            other => panic!("unexpected 213 response: {:?}", other.name()),
        }

        // 85 equips and broadcasts to the real map uid.
        XPlayer::report_hat_equip(
            &pool,
            &mgr_tx,
            &queue_tx,
            &user_id,
            current.as_deref(),
            map_uid.clone(),
            "Hat-1".into(),
        )
        .await
        .expect("equip on an md5-context map");
        match mgr_rx.try_recv().expect("equip broadcasts") {
            ToPlayerMgr::BroadcastToMap {
                map_uid: target,
                response: Response::HatsOnMapUpdate { uid, hats },
            } => {
                assert_eq!(target, map_uid);
                assert_eq!(uid, map_uid);
                assert_eq!(hats[0].slug, "Hat-1");
            }
            _ => panic!("unexpected equip broadcast"),
        }

        // 214 is answered too.
        XPlayer::send_hats_on_map(&pool, &queue_tx, current.as_deref(), map_uid.clone())
            .await
            .expect("send hats on map");
        match queue_rx.try_recv().expect("214 is answered on an md5-context map") {
            Response::HatsOnMap { uid, hats } => {
                assert_eq!(uid, map_uid);
                assert_eq!(hats.len(), 1);
                assert_eq!(hats[0].wsid, user_id.to_string());
                assert_eq!(hats[0].slug, "Hat-1");
            }
            other => panic!("unexpected 214 response: {:?}", other.name()),
        }

        // A context map change clears the tracked map, which is what fires the
        // leave push for the map the player actually left.
        assert_eq!(
            XPlayer::set_wayfinder_map(&pool, &mgr_tx, &tracked, &user_id, "md5-context-player", None)
                .await
                .expect("context change clears the tracked map"),
            None
        );
        match mgr_rx.try_recv().expect("leave push fires") {
            ToPlayerMgr::BroadcastToMap {
                map_uid: target,
                response: Response::HatsOnMapUpdate { uid, hats },
            } => {
                assert_eq!(target, map_uid);
                assert_eq!(uid, map_uid);
                assert!(hats[0].slug.is_empty(), "leave push unequips");
            }
            _ => panic!("unexpected leave broadcast"),
        }
        assert!(tracked.lock().await.is_none());

        // An unbound map must be answered with silence, never an empty 213.
        sqlx::query!("DELETE FROM map_listing_bind WHERE map_uid = $1", map_uid)
            .execute(&pool)
            .await
            .expect("drop binding");
        XPlayer::send_hats_mine(&pool, &queue_tx, &pending, Some(&map_uid), map_uid.clone(), &user_id)
            .await
            .expect("send hats mine without a binding");
        assert!(
            queue_rx.try_recv().is_err(),
            "an unbound map must withhold 213, not answer it with an empty list"
        );

        sqlx::query!("DELETE FROM users WHERE web_services_user_id = $1", user_id)
            .execute(&pool)
            .await
            .expect("cleanup md5-context player");
    }

    /// Catalog validation now rejects the whole spec. Without a failure
    /// `TaskResponse` the editor's save never resolves.
    #[tokio::test]
    async fn invalid_catalog_answers_the_mapper_with_the_validation_error() {
        dotenv::dotenv().ok();
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for delivery tests");
        let pool = PgPoolOptions::new().max_connections(2).connect(&url).await.expect("connect");
        let user_id = seed_user(&pool, "spec-validation-mapper").await;
        let name_id = format!("spec-invalid-{}", &Uuid::new_v4().simple().to_string()[..8]);
        let (queue_tx, mut queue_rx) = unbounded_channel();

        let good = json!({"hats":[{"slug":"Hat-1","unlock":{"finishThisMap":true}}]});
        XPlayer::report_custom_map_aux_spec(&pool, 11, &user_id, &name_id, &good, &queue_tx)
            .await
            .expect("handler completes");
        assert!(matches!(
            queue_rx.try_recv().expect("save is acknowledged"),
            Response::TaskResponse {
                id: 11,
                success: true,
                error: None
            }
        ));

        let duplicate = json!({"hats":[
            {"slug":"Hat-1","unlock":{"finishThisMap":true}},
            {"slug":" Hat-1 ","unlock":{"finishThisMap":true}}
        ]});
        XPlayer::report_custom_map_aux_spec(&pool, 12, &user_id, &name_id, &duplicate, &queue_tx)
            .await
            .expect("handler completes");
        match queue_rx.try_recv().expect("a rejected save still answers the editor") {
            Response::TaskResponse { id, success, error } => {
                assert_eq!(id, 12);
                assert!(!success);
                let error = error.expect("rejection carries a reason");
                assert!(error.contains("duplicate hat slug"), "unhelpful reason: {error}");
            }
            other => panic!("unexpected response: {:?}", other.name()),
        }
        let stored = sqlx::query!(
            "SELECT spec FROM custom_map_aux_specs WHERE user_id = $1 AND name_id = $2",
            user_id,
            name_id
        )
        .fetch_one(&pool)
        .await
        .expect("previous spec survives");
        assert_eq!(stored.spec, good);

        sqlx::query!("DELETE FROM users WHERE web_services_user_id = $1", user_id)
            .execute(&pool)
            .await
            .expect("cleanup validation mapper");
    }

    /// `map_listing_bind.spec_id` is `ON DELETE RESTRICT`, so deleting a bound
    /// spec used to raise a foreign key violation and return before any
    /// `TaskResponse` was sent, hanging the editor on an undeletable spec.
    #[tokio::test]
    async fn deleting_a_bound_spec_answers_the_mapper_instead_of_hanging() {
        dotenv::dotenv().ok();
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for delivery tests");
        let pool = PgPoolOptions::new().max_connections(2).connect(&url).await.expect("connect");
        let user_id = seed_user(&pool, "bound-spec-mapper").await;
        let suffix = Uuid::new_v4().simple().to_string();
        let map_uid: String = format!("BoundSpec{suffix}").chars().take(27).collect();
        let name_id = format!("bound-{}", &suffix[..8]);
        let spec_id = seed_unlocked_hat(&pool, &user_id, &map_uid, &name_id).await;
        let (queue_tx, mut queue_rx) = unbounded_channel();

        XPlayer::delete_custom_map_aux_spec(&pool, 21, &user_id, &name_id, &queue_tx)
            .await
            .expect("handler completes instead of erroring out");
        match queue_rx.try_recv().expect("a refused delete still answers the editor") {
            Response::TaskResponse { id, success, error } => {
                assert_eq!(id, 21);
                assert!(!success);
                let error = error.expect("refusal carries a reason");
                assert!(error.contains(&map_uid), "reason should name the bound map: {error}");
            }
            other => panic!("unexpected response: {:?}", other.name()),
        }
        assert!(
            sqlx::query!("SELECT id FROM custom_map_aux_specs WHERE id = $1", spec_id)
                .fetch_optional(&pool)
                .await
                .expect("read spec")
                .is_some(),
            "a refused delete must leave the spec in place"
        );

        sqlx::query!("DELETE FROM map_listing_bind WHERE map_uid = $1", map_uid)
            .execute(&pool)
            .await
            .expect("unbind");
        XPlayer::delete_custom_map_aux_spec(&pool, 22, &user_id, &name_id, &queue_tx)
            .await
            .expect("handler completes");
        assert!(matches!(
            queue_rx.try_recv().expect("an unbound delete succeeds"),
            Response::TaskResponse {
                id: 22,
                success: true,
                error: None
            }
        ));
        assert!(
            sqlx::query!("SELECT id FROM custom_map_aux_specs WHERE id = $1", spec_id)
                .fetch_optional(&pool)
                .await
                .expect("read spec")
                .is_none(),
            "an unbound spec is deleted"
        );

        sqlx::query!("DELETE FROM users WHERE web_services_user_id = $1", user_id)
            .execute(&pool)
            .await
            .expect("cleanup bound-spec mapper");
    }

    #[test]
    fn hat_updates_are_delivered_only_to_the_target_map() {
        assert!(delivery_matches_map(Some("map-a"), "map-a"));
        assert!(!delivery_matches_map(Some("map-b"), "map-a"));
        assert!(!delivery_matches_map(None, "map-a"));
    }

    #[test]
    fn ping_map_tracker_updates_across_every_transition() {
        let mut last = None;
        assert!(update_last_map(&mut last, Some("map-a")));
        assert_eq!(last.as_deref(), Some("map-a"));
        assert!(!update_last_map(&mut last, Some("map-a")));
        assert!(update_last_map(&mut last, Some("map-b")));
        assert_eq!(last.as_deref(), Some("map-b"));
        assert!(update_last_map(&mut last, None));
        assert_eq!(last, None);
        assert!(!update_last_map(&mut last, None));
    }

    #[tokio::test]
    async fn persisted_equipped_player_join_is_delivered_to_current_map_only() {
        dotenv::dotenv().ok();
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for delivery tests");
        let pool = PgPoolOptions::new().max_connections(3).connect(&url).await.expect("connect");
        let user_id = Uuid::now_v7();
        let suffix = Uuid::new_v4().simple().to_string();
        let map_uid: String = format!("HatJoin{suffix}").chars().take(27).collect();
        let other_map_uid: String = format!("HatOther{suffix}").chars().take(27).collect();
        let name_id = format!("hat-join-{}", &suffix[..8]);
        sqlx::query!(
            "INSERT INTO users (web_services_user_id, display_name) VALUES ($1, $2)",
            user_id,
            "persisted-hat-joiner"
        )
        .execute(&pool)
        .await
        .expect("insert joining user");
        seed_equipped_hat(&pool, &user_id, &map_uid, &name_id).await;

        let (mgr_tx, mut mgr_rx) = unbounded_channel();
        XPlayer::broadcast_hat_context_transition(&pool, &mgr_tx, &user_id, "persisted-hat-joiner", None, Some(&map_uid))
            .await
            .expect("broadcast persisted hat on join");
        let join_event = mgr_rx.try_recv().expect("join emits one manager event");
        match &join_event {
            ToPlayerMgr::BroadcastToMap {
                map_uid: target,
                response: Response::HatsOnMapUpdate { uid, hats },
            } => {
                assert_eq!(target, &map_uid);
                assert_eq!(uid, &map_uid);
                assert_eq!(hats.len(), 1);
                assert_eq!(hats[0].wsid, user_id.to_string());
                assert_eq!(hats[0].slug, "Hat-1");
            }
            _ => panic!("unexpected join manager event"),
        }
        assert!(mgr_rx.try_recv().is_err());

        let existing_map = Arc::new(tokio::sync::Mutex::new(Some(map_uid.clone())));
        let joining_map = Arc::new(tokio::sync::Mutex::new(Some(map_uid.clone())));
        let off_map = Arc::new(tokio::sync::Mutex::new(Some(other_map_uid.clone())));
        let (existing_tx, mut existing_rx) = unbounded_channel();
        let (joining_tx, mut joining_rx) = unbounded_channel();
        let (off_tx, mut off_rx) = unbounded_channel();
        let mut players = vec![
            PlayerDelivery {
                tx: existing_tx,
                wayfinder_map_uid: existing_map,
            },
            PlayerDelivery {
                tx: joining_tx,
                wayfinder_map_uid: joining_map.clone(),
            },
            PlayerDelivery {
                tx: off_tx,
                wayfinder_map_uid: off_map,
            },
        ];
        deliver_player_mgr_message(&mut players, join_event).await;
        for delivered in [
            existing_rx.try_recv().expect("existing player receives joining hat"),
            joining_rx.try_recv().expect("joining player receives current-map hat state"),
        ] {
            assert!(matches!(
                delivered,
                ToPlayer::Send(Response::HatsOnMapUpdate { uid, hats })
                    if uid == map_uid && hats.len() == 1 && hats[0].slug == "Hat-1"
            ));
        }
        assert!(off_rx.try_recv().is_err(), "off-map player must not receive join update");

        XPlayer::broadcast_hat_context_transition(&pool, &mgr_tx, &user_id, "persisted-hat-joiner", Some(&map_uid), Some(&map_uid))
            .await
            .expect("same-map context is a no-op");
        assert!(mgr_rx.try_recv().is_err(), "unchanged same-map contexts must not spam updates");

        *joining_map.lock().await = Some(other_map_uid.clone());
        XPlayer::broadcast_hat_context_transition(
            &pool,
            &mgr_tx,
            &user_id,
            "persisted-hat-joiner",
            Some(&map_uid),
            Some(&other_map_uid),
        )
        .await
        .expect("map transition broadcasts old-map removal");
        let transition_event = mgr_rx.try_recv().expect("map transition emits old-map removal");
        assert!(mgr_rx.try_recv().is_err(), "no equipped hat exists on the destination map");
        deliver_player_mgr_message(&mut players, transition_event).await;
        assert!(matches!(
            existing_rx.try_recv(),
            Ok(ToPlayer::Send(Response::HatsOnMapUpdate { uid, hats }))
                if uid == map_uid && hats.len() == 1 && hats[0].slug.is_empty()
        ));
        assert!(joining_rx.try_recv().is_err(), "transitioned player is no longer on the old map");
        assert!(
            off_rx.try_recv().is_err(),
            "destination-map player must not receive old-map removal"
        );

        *joining_map.lock().await = None;
        XPlayer::broadcast_hat_context_transition(&pool, &mgr_tx, &user_id, "persisted-hat-joiner", Some(&other_map_uid), None)
            .await
            .expect("leave emits destination-map removal");
        let leave_event = mgr_rx.try_recv().expect("leave emits one removal");
        deliver_player_mgr_message(&mut players, leave_event).await;
        assert!(
            joining_rx.try_recv().is_err(),
            "leaving player must not receive its old-map removal"
        );
        assert!(matches!(
            off_rx.try_recv(),
            Ok(ToPlayer::Send(Response::HatsOnMapUpdate { uid, hats }))
                if uid == other_map_uid && hats.len() == 1 && hats[0].slug.is_empty()
        ));

        sqlx::query!("DELETE FROM map_listing_bind WHERE map_uid = $1", map_uid)
            .execute(&pool)
            .await
            .expect("cleanup join-broadcast binding");
        sqlx::query!("DELETE FROM users WHERE web_services_user_id = $1", user_id)
            .execute(&pool)
            .await
            .expect("cleanup joining user");
    }

    #[tokio::test]
    async fn equip_handler_broadcasts_only_changes_and_hats_on_map_is_direct_only() {
        dotenv::dotenv().ok();
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for delivery tests");
        let pool = PgPoolOptions::new().max_connections(2).connect(&url).await.expect("connect");
        let user_id = Uuid::now_v7();
        let suffix = Uuid::new_v4().simple().to_string();
        let map_uid: String = format!("Equip{suffix}").chars().take(27).collect();
        let off_map_uid: String = format!("Other{suffix}").chars().take(27).collect();
        let name_id = format!("equip-{}", &suffix[..8]);
        sqlx::query!(
            "INSERT INTO users (web_services_user_id, display_name) VALUES ($1, $2)",
            user_id,
            "equip-broadcast-test"
        )
        .execute(&pool)
        .await
        .expect("insert user");
        let spec = json!({"hats":[{"slug":"Hat-1","unlock":{"finishThisMap":true}}]});
        let spec_id = crate::queries::custom_map_aux_specs::upsert_spec_and_catalogs(&pool, &user_id, &name_id, &spec)
            .await
            .expect("insert spec");
        crate::queries::hats::bind_map_from_catalog(&pool, &map_uid, spec_id)
            .await
            .expect("bind hats");
        sqlx::query!(
            r#"INSERT INTO map_listing_bind
                   (map_uid, spec_id, verified_author_id, author_verified_at,
                    materialized_spec_updated_at, materialized_spec_hash)
               SELECT $1, id, user_id, NOW(), updated_at, spec_hash
               FROM custom_map_aux_specs WHERE id = $2"#,
            map_uid,
            spec_id
        )
        .execute(&pool)
        .await
        .expect("mark handler test snapshot materialized");
        sqlx::query!(
            r#"INSERT INTO hat_unlock (user_id, hat_key, map_uid, slug, unlocked_at)
               VALUES ($1, substring(md5($2 || ':h:Hat-1') from 1 for 16), $2, 'Hat-1', NOW())"#,
            user_id,
            map_uid
        )
        .execute(&pool)
        .await
        .expect("insert unlock");

        let (mgr_tx, mut mgr_rx) = unbounded_channel();
        let (queue_tx, mut queue_rx) = unbounded_channel();
        let pending: Arc<PendingHatUnlocks> = Default::default();
        XPlayer::report_hat_equip(
            &pool,
            &mgr_tx,
            &queue_tx,
            &user_id,
            Some(&off_map_uid),
            map_uid.clone(),
            "Hat-1".into(),
        )
        .await
        .expect("off-map equip is ignored");
        assert!(mgr_rx.try_recv().is_err());
        assert_eq!(
            sqlx::query!("SELECT COUNT(*) AS count FROM hat_equipped WHERE user_id = $1", user_id)
                .fetch_one(&pool)
                .await
                .expect("count equipment")
                .count
                .unwrap_or(0),
            0
        );

        XPlayer::report_hat_equip(&pool, &mgr_tx, &queue_tx, &user_id, Some(&map_uid), map_uid.clone(), "Hat-1".into())
            .await
            .expect("equip");
        match mgr_rx.try_recv().expect("equip broadcast") {
            ToPlayerMgr::BroadcastToMap {
                map_uid: target,
                response: Response::HatsOnMapUpdate { uid, hats },
            } => {
                assert_eq!(target, map_uid);
                assert_eq!(uid, map_uid);
                assert_eq!(hats.len(), 1);
                assert_eq!(hats[0].slug, "Hat-1");
            }
            _ => panic!("unexpected equip broadcast"),
        }

        XPlayer::report_hat_equip(&pool, &mgr_tx, &queue_tx, &user_id, Some(&map_uid), map_uid.clone(), "Hat-1".into())
            .await
            .expect("rate-limited duplicate");
        assert!(mgr_rx.try_recv().is_err());
        assert!(
            matches!(queue_rx.try_recv(), Ok(Response::NonFatalErrorMsg { level: 2, .. })),
            "a throttled equip must tell the client instead of vanishing"
        );
        sleep(Duration::from_millis(260)).await;
        XPlayer::report_hat_equip(&pool, &mgr_tx, &queue_tx, &user_id, Some(&map_uid), map_uid.clone(), "Hat-1".into())
            .await
            .expect("unchanged duplicate");
        assert!(mgr_rx.try_recv().is_err());
        assert!(queue_rx.try_recv().is_err(), "an accepted no-op equip must stay silent");

        XPlayer::send_hats_on_map(&pool, &queue_tx, Some(&off_map_uid), map_uid.clone())
            .await
            .expect("off-map read is ignored");
        assert!(queue_rx.try_recv().is_err());
        XPlayer::send_hats_on_map(&pool, &queue_tx, Some(&map_uid), map_uid.clone())
            .await
            .expect("direct hats response");
        assert!(matches!(
            queue_rx.try_recv(),
            Ok(Response::HatsOnMap { uid, .. }) if uid == map_uid
        ));
        assert!(mgr_rx.try_recv().is_err(), "request 214 must not emit a manager broadcast");

        XPlayer::send_hats_mine(&pool, &queue_tx, &pending, Some(&off_map_uid), map_uid.clone(), &user_id)
            .await
            .expect("off-map mine read is ignored");
        assert!(queue_rx.try_recv().is_err());
        XPlayer::send_hats_mine(&pool, &queue_tx, &pending, Some(&map_uid), map_uid.clone(), &user_id)
            .await
            .expect("current-map mine response");
        assert!(matches!(
            queue_rx.try_recv(),
            Ok(Response::HatsMine { uid, .. }) if uid == map_uid
        ));

        sqlx::query!("DELETE FROM map_listing_bind WHERE map_uid = $1", map_uid)
            .execute(&pool)
            .await
            .expect("cleanup handler test binding");
        sqlx::query!("DELETE FROM users WHERE web_services_user_id = $1", user_id)
            .execute(&pool)
            .await
            .expect("cleanup user");
    }

    #[tokio::test]
    async fn equip_event_cannot_overtake_request_81_removal_event() {
        dotenv::dotenv().ok();
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for delivery tests");
        let pool = PgPoolOptions::new().max_connections(2).connect(&url).await.expect("connect");
        let user_id = Uuid::now_v7();
        let suffix = Uuid::new_v4().simple().to_string();
        let map_uid: String = format!("EquipRace{suffix}").chars().take(27).collect();
        let name_id = format!("equip-race-{}", &suffix[..8]);
        sqlx::query!(
            "INSERT INTO users (web_services_user_id, display_name) VALUES ($1, $2)",
            user_id,
            "equip-event-order-test"
        )
        .execute(&pool)
        .await
        .expect("insert user");
        let initial_spec = json!({"hats":[{"slug":"Hat-1","unlock":{"finishThisMap":true}}]});
        let spec_id = crate::queries::custom_map_aux_specs::upsert_spec_and_catalogs(&pool, &user_id, &name_id, &initial_spec)
            .await
            .expect("insert initial spec");
        crate::queries::hats::bind_map_from_catalog(&pool, &map_uid, spec_id)
            .await
            .expect("bind initial hats");
        sqlx::query!(
            r#"INSERT INTO map_listing_bind
                   (map_uid, spec_id, verified_author_id, author_verified_at,
                    materialized_spec_updated_at, materialized_spec_hash)
               SELECT $1, id, user_id, NOW(), updated_at, spec_hash
               FROM custom_map_aux_specs WHERE id = $2"#,
            map_uid,
            spec_id
        )
        .execute(&pool)
        .await
        .expect("mark initial snapshot materialized");
        sqlx::query!(
            r#"INSERT INTO hat_unlock (user_id, hat_key, map_uid, slug, unlocked_at)
               VALUES ($1, substring(md5($2 || ':h:Hat-1') from 1 for 16), $2, 'Hat-1', NOW())"#,
            user_id,
            map_uid
        )
        .execute(&pool)
        .await
        .expect("insert unlock");

        let (mgr_tx, mut mgr_rx) = unbounded_channel();
        let post_commit_reached = Arc::new(Notify::new());
        let resume_equip = Arc::new(Notify::new());
        let equip_pool = pool.clone();
        let equip_tx = mgr_tx.clone();
        let equip_map_uid = map_uid.clone();
        let equip_reached = post_commit_reached.clone();
        let equip_resume = resume_equip.clone();
        let equip = tokio::spawn(async move {
            let current_map_uid = equip_map_uid.clone();
            let (equip_queue_tx, _equip_queue_rx) = unbounded_channel();
            XPlayer::report_hat_equip_with_post_commit(
                &equip_pool,
                &equip_tx,
                &equip_queue_tx,
                &user_id,
                Some(&current_map_uid),
                equip_map_uid,
                "Hat-1".into(),
                async move {
                    equip_reached.notify_one();
                    equip_resume.notified().await;
                },
            )
            .await
        });
        timeout(Duration::from_secs(5), post_commit_reached.notified())
            .await
            .expect("equip reaches post-commit pause");
        assert_eq!(
            sqlx::query!(
                "SELECT COUNT(*) AS count FROM hat_equipped WHERE user_id = $1 AND map_uid = $2 AND slug = 'Hat-1'",
                user_id,
                map_uid
            )
            .fetch_one(&pool)
            .await
            .expect("equip committed before pause")
            .count
            .unwrap_or(0),
            1
        );

        let replacement_spec = json!({"hats":[{"slug":"Hat-2","unlock":{"finishThisMap":true}}]});
        assert_eq!(
            crate::queries::custom_map_aux_specs::upsert_spec_and_catalogs(&pool, &user_id, &name_id, &replacement_spec)
                .await
                .expect("replace source spec while equip is paused"),
            spec_id
        );

        let rebuild_pool = pool.clone();
        let rebuild_tx = mgr_tx.clone();
        let rebuild_map_uid = map_uid.clone();
        let mut rebuild = tokio::spawn(async move {
            let verifier = FixedAuthorVerifier(user_id);
            XPlayer::report_minigame_map_with_verifier(&rebuild_pool, &rebuild_tx, &verifier, &user_id, rebuild_map_uid, spec_id).await
        });
        assert!(
            timeout(Duration::from_millis(150), &mut rebuild).await.is_err(),
            "request 81 must wait until the older equip event is enqueued"
        );

        resume_equip.notify_one();
        timeout(Duration::from_secs(5), equip)
            .await
            .expect("equip resumes")
            .expect("equip task joins")
            .expect("equip handler succeeds");
        timeout(Duration::from_secs(5), rebuild)
            .await
            .expect("request 81 resumes")
            .expect("request-81 task joins")
            .expect("request-81 handler succeeds");

        match timeout(Duration::from_secs(5), mgr_rx.recv())
            .await
            .expect("equip event arrives")
            .expect("equip event channel remains open")
        {
            ToPlayerMgr::BroadcastToMap {
                map_uid: target,
                response: Response::HatsOnMapUpdate { uid, hats },
            } => {
                assert_eq!(target, map_uid);
                assert_eq!(uid, map_uid);
                assert_eq!(hats.len(), 1);
                assert_eq!(hats[0].slug, "Hat-1");
            }
            _ => panic!("unexpected first event"),
        }
        match timeout(Duration::from_secs(5), mgr_rx.recv())
            .await
            .expect("request-81 removal event arrives")
            .expect("request-81 event channel remains open")
        {
            ToPlayerMgr::BroadcastToMap {
                map_uid: target,
                response: Response::HatsOnMapUpdate { uid, hats },
            } => {
                assert_eq!(target, map_uid);
                assert_eq!(uid, map_uid);
                assert_eq!(hats.len(), 1);
                assert_eq!(hats[0].slug, "", "final delivered state must be unequipped");
            }
            _ => panic!("unexpected second event"),
        }
        assert!(mgr_rx.try_recv().is_err());
        assert_eq!(
            sqlx::query!(
                "SELECT COUNT(*) AS count FROM hat_equipped WHERE user_id = $1 AND map_uid = $2",
                user_id,
                map_uid
            )
            .fetch_one(&pool)
            .await
            .expect("request 81 clears stale equipment")
            .count
            .unwrap_or(0),
            0
        );

        sqlx::query!("DELETE FROM map_listing_bind WHERE map_uid = $1", map_uid)
            .execute(&pool)
            .await
            .expect("cleanup ordering test binding");
        sqlx::query!("DELETE FROM users WHERE web_services_user_id = $1", user_id)
            .execute(&pool)
            .await
            .expect("cleanup ordering test user");
    }

    #[tokio::test]
    async fn join_event_cannot_overtake_request_81_removal_event() {
        dotenv::dotenv().ok();
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for delivery tests");
        let pool = PgPoolOptions::new().max_connections(3).connect(&url).await.expect("connect");
        let user_id = Uuid::now_v7();
        let suffix = Uuid::new_v4().simple().to_string();
        let map_uid: String = format!("JoinRace{suffix}").chars().take(27).collect();
        let name_id = format!("join-race-{}", &suffix[..8]);
        sqlx::query!(
            "INSERT INTO users (web_services_user_id, display_name) VALUES ($1, $2)",
            user_id,
            "join-event-order-test"
        )
        .execute(&pool)
        .await
        .expect("insert joining user");
        let spec_id = seed_equipped_hat(&pool, &user_id, &map_uid, &name_id).await;

        let (mgr_tx, mut mgr_rx) = unbounded_channel();
        let join_ready = Arc::new(Notify::new());
        let resume_join = Arc::new(Notify::new());
        let join_pool = pool.clone();
        let join_tx = mgr_tx.clone();
        let join_map_uid = map_uid.clone();
        let reached = join_ready.clone();
        let resume = resume_join.clone();
        let join = tokio::spawn(async move {
            XPlayer::broadcast_hat_context_transition_with_join_ready(
                &join_pool,
                &join_tx,
                &user_id,
                "join-event-order-test",
                None,
                Some(&join_map_uid),
                async move {
                    reached.notify_one();
                    resume.notified().await;
                },
            )
            .await
        });
        timeout(Duration::from_secs(5), join_ready.notified())
            .await
            .expect("join fetches the valid equipped hat and pauses before enqueue");

        let replacement_spec = json!({"hats":[{"slug":"Hat-2","unlock":{"finishThisMap":true}}]});
        assert_eq!(
            crate::queries::custom_map_aux_specs::upsert_spec_and_catalogs(&pool, &user_id, &name_id, &replacement_spec)
                .await
                .expect("replace source spec while join is paused"),
            spec_id
        );

        let rebuild_pool = pool.clone();
        let rebuild_tx = mgr_tx.clone();
        let rebuild_map_uid = map_uid.clone();
        let mut rebuild = tokio::spawn(async move {
            let verifier = FixedAuthorVerifier(user_id);
            XPlayer::report_minigame_map_with_verifier(&rebuild_pool, &rebuild_tx, &verifier, &user_id, rebuild_map_uid, spec_id).await
        });
        assert!(
            timeout(Duration::from_millis(150), &mut rebuild).await.is_err(),
            "request 81 must wait until the older join event is enqueued"
        );

        resume_join.notify_one();
        timeout(Duration::from_secs(5), join)
            .await
            .expect("join resumes")
            .expect("join task joins")
            .expect("join broadcast succeeds");
        timeout(Duration::from_secs(5), rebuild)
            .await
            .expect("request 81 resumes")
            .expect("request-81 task joins")
            .expect("request-81 handler succeeds");

        for expected_slug in ["Hat-1", ""] {
            match timeout(Duration::from_secs(5), mgr_rx.recv())
                .await
                .expect("ordered event arrives")
                .expect("manager channel remains open")
            {
                ToPlayerMgr::BroadcastToMap {
                    map_uid: target,
                    response: Response::HatsOnMapUpdate { uid, hats },
                } => {
                    assert_eq!(target, map_uid);
                    assert_eq!(uid, map_uid);
                    assert_eq!(hats.len(), 1);
                    assert_eq!(hats[0].slug, expected_slug);
                }
                _ => panic!("unexpected join/request-81 event"),
            }
        }
        assert!(mgr_rx.try_recv().is_err());
        assert_eq!(
            sqlx::query!(
                "SELECT COUNT(*) AS count FROM hat_equipped WHERE user_id = $1 AND map_uid = $2",
                user_id,
                map_uid
            )
            .fetch_one(&pool)
            .await
            .expect("request 81 clears joined stale equipment")
            .count
            .unwrap_or(0),
            0
        );

        sqlx::query!("DELETE FROM map_listing_bind WHERE map_uid = $1", map_uid)
            .execute(&pool)
            .await
            .expect("cleanup join-order binding");
        sqlx::query!("DELETE FROM users WHERE web_services_user_id = $1", user_id)
            .execute(&pool)
            .await
            .expect("cleanup join-order user");
    }
}

pub fn version_less(v1: &[u32], v2: &[u32]) -> bool {
    for (a, b) in v1.iter().zip(v2.iter()) {
        if a < b {
            return true;
        }
        if a > b {
            return false;
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

//
//
//
//

lazy_static! {
    pub static ref DD2_TOP_10: Vec<String> = {
        vec![
            "5d6b14db-4d41-47a4-93e2-36a3bf229f9b".to_string(),
            "e5a9863b-1844-4436-a8a8-cea583888f8b".to_string(),
            "d46fb45d-d422-47c9-9785-67270a311e25".to_string(),
            "e3ff2309-bc24-414a-b9f1-81954236c34b".to_string(),
            "0fd26a9f-8f70-4f51-85e1-fe99a4ed6ffb".to_string(),
            "bd45204c-80f1-4809-b983-38b3f0ffc1ef".to_string(),
            "da4642f9-6acf-43fe-88b6-b120ff1308ba".to_string(),
            "d320a237-1b0a-4069-af83-f2c09fbf042e".to_string(),
            "803695f6-8319-4b8e-8c28-44856834fe3b".to_string(),
            "076d23a5-51a6-48aa-8d99-9d618cd13c93".to_string(),
            // xertrov
            "0a2d1bc0-4aaa-4374-b2db-3d561bdab1c9".to_string(),
        ]
    };
}

impl XPlayer {
    pub async fn get_secret_assets(
        pool: &Pool<Postgres>,
        user_id: &Uuid,
        queue_tx: &UnboundedSender<Response>,
    ) -> Result<(), api_error::Error> {
        let happened =
            query!("SELECT has_happened FROM map_events WHERE map_uid = 'DeepDip2__The_Storm_Is_Here' AND event_type = '3finish'")
                .fetch_one(pool)
                .await;
        let top3_done = match happened {
            Ok(r) => r.has_happened,
            Err(_) => false,
        };
        let has_special_ff = DD2_TOP_10.contains(&user_id.to_string());
        let fanfare_name = match has_special_ff || !top3_done {
            true => user_id.to_string(),
            false => "generic".to_string(),
        };
        if top3_done || has_special_ff {
            queue_tx.send(Response::SecretAssets {
                filenames_and_urls: vec![
                    AssetRef {
                        name: "head".to_string(),
                        filename: "img/s1head.jpg".to_string(),
                        url: "https://assets.xk.io/d++/secret/s1head-3948765.jpg".to_string(),
                    },
                    AssetRef {
                        name: "s-flight-vae".to_string(),
                        filename: "subs/flight-vae.txt".to_string(),
                        url: "https://assets.xk.io/d++/secret/subs-vae-3948765.txt".to_string(),
                    },
                    AssetRef {
                        name: "s-flight".to_string(),
                        filename: "subs/flight.txt".to_string(),
                        url: "https://assets.xk.io/d++/secret/subs-3948765.txt".to_string(),
                    },
                    AssetRef {
                        name: "flight-vae".to_string(),
                        filename: "vl/flight-vae.mp3".to_string(),
                        url: "https://assets.xk.io/d++/secret/flight-vae-3948765.mp3".to_string(),
                    },
                    AssetRef {
                        name: "flight".to_string(),
                        filename: "vl/flight.mp3".to_string(),
                        url: "https://assets.xk.io/d++/secret/flight-3948765.mp3".to_string(),
                    },
                    AssetRef {
                        name: "fanfare".to_string(),
                        filename: "img/fanfare-self.png".to_string(),
                        url: format!("https://assets.xk.io/d++/secret/{}.png", fanfare_name),
                    },
                ],
            })?;
        }
        Ok(())
    }

    // get_users_profile
    // set_users_profile
    // get_users_preferences
    // set_users_preferences

    pub async fn get_users_profile(
        pool: &Pool<Postgres>,
        user_id: &str,
        queue_tx: &UnboundedSender<Response>,
    ) -> Result<(), api_error::Error> {
        let user_id: Uuid = Uuid::from_str(user_id)?;
        let mut profile_r = user_settings::get_profile(pool, &user_id).await;
        if let Err(sqlx::Error::RowNotFound) = profile_r {
            let r = query!("SELECT display_name FROM users WHERE web_services_user_id = $1", user_id)
                .fetch_one(pool)
                .await?;
            let name = r.display_name;
            let s_value = serde_json::json!({
                "wsid": user_id.to_string(),
                "name": name,
            });
            query!(
                "INSERT INTO profile_settings (user_id, s_value) VALUES ($1, $2) ON CONFLICT (user_id) DO NOTHING",
                user_id,
                s_value
            )
            .execute(pool)
            .await?;
            profile_r = user_settings::get_profile(pool, &user_id).await;
        }
        queue_tx.send(Response::UsersProfile { profile: profile_r? })?;
        Ok(())
    }

    pub async fn set_users_profile(pool: &Pool<Postgres>, user_id: &Uuid, body: serde_json::Value) -> Result<(), api_error::Error> {
        Ok(user_settings::set_profile(pool, user_id, body).await?)
    }

    pub async fn get_users_preferences(
        pool: &Pool<Postgres>,
        user_id: &Uuid,
        queue_tx: &UnboundedSender<Response>,
    ) -> Result<(), api_error::Error> {
        let preferences = user_settings::get_preferences(pool, user_id).await?;
        queue_tx.send(Response::YourPreferences { preferences })?;
        Ok(())
    }

    pub async fn set_users_preferences(pool: &Pool<Postgres>, user_id: &Uuid, body: serde_json::Value) -> Result<(), api_error::Error> {
        Ok(user_settings::set_preferences(pool, user_id, body).await?)
    }

    pub async fn get_map_rank_query(
        pool: &Pool<Postgres>,
        map_uid: &str,
        user_id: &Uuid,
    ) -> Result<Option<LeaderboardEntry2>, api_error::Error> {
        Ok(queries::get_map_rank_entry(pool, map_uid, user_id).await?)
    }

    pub async fn get_map_rank(
        pool: &Pool<Postgres>,
        map_uid: String,
        user_id: &Uuid,
        queue_tx: &UnboundedSender<Response>,
    ) -> Result<(), api_error::Error> {
        let r = XPlayer::get_map_rank_query(pool, &map_uid, user_id).await?;
        let status = queries::moderation::get_ban_status(pool, &map_uid, user_id).await?;
        queue_tx.send(Response::MapRank {
            uid: map_uid,
            r,
            banned: status.banned,
            ban_public_message: status.public_message,
            unofficial: status.unofficial,
        })?;
        Ok(())
    }

    pub async fn get_map_overview(
        pool: &Pool<Postgres>,
        queue_tx: &UnboundedSender<Response>,
        map_uid: String,
    ) -> Result<(), api_error::Error> {
        let (nb_players_on_lb, nb_playing_now) = get_map_overview_player_counts(pool, &map_uid).await?;

        queue_tx.send(Response::MapOverview {
            uid: map_uid,
            nb_players_on_lb: nb_players_on_lb as u32,
            nb_playing_now: nb_playing_now as u32,
        })?;
        Ok(())
    }

    pub async fn send_map_lb(
        pool: &Pool<Postgres>,
        queue_tx: &UnboundedSender<Response>,
        map_uid: &String,
        start: i64,
        end: i64,
    ) -> Result<(), api_error::Error> {
        let entries = queries::get_map_leaderboard(pool, &map_uid, start, end).await?;
        queue_tx.send(Response::MapLB {
            uid: map_uid.to_string(),
            entries,
        })?;
        Ok(())
    }

    pub async fn send_map_live(
        pool: &Pool<Postgres>,
        queue_tx: &UnboundedSender<Response>,
        map_uid: String,
    ) -> Result<(), api_error::Error> {
        let players = queries::get_map_live_heights_top_n(pool, &map_uid, 100).await?;
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
        if pos[1] > MAX_HEIGHT {
            return Ok(());
        }
        if map_uid.len() != 27 {
            warn!("Invalid map_uid: {:?} from {:?}", map_uid, user_id);
            Err("Invalid map_uid".to_string())?;
        }
        queries::custom_maps::upsert_map_curr_height(pool, &map_uid, user_id, pos, race_time).await?;

        #[cfg(debug_assertions)]
        info!("User {:?} reported map curr pos: {:?}", user_id, pos);

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

    // pub async fn on_report_gcnod_msg(pool: &Pool<Postgres>, session_id: &Uuid, ctx_id: Option<&Uuid>, data: &str) -> Result<(), ApiError> {
    //     // decode base64

    //     let x = if data.len() < 0x2E0 {
    //         data.into()
    //     } else {
    //         base64::prelude::BASE64_URL_SAFE.decode(data).unwrap_or(data.into())
    //     };

    //     match ctx_id {
    //         Some(ctx_id) => {
    //             insert_gc_nod(pool, session_id, ctx_id, &x).await?;
    //         }
    //         None => {
    //             warn!("Dropping GCNod b/c no context");
    //         }
    //     }
    //     Ok(())
    // }

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
        user_id: &Uuid,
        ctx: Option<&PlayerCtx>,
        ctx_id: Option<&Uuid>,
        pos: [f32; 3],
        rotq: [f32; 4],
        vel: [f32; 3],
    ) -> Result<(), ApiError> {
        let is_official = ctx.map(|ctx| ctx.is_official()).unwrap_or(false);
        Ok(queries::report_live_vehicle_state(pool, sess, user_id, ctx_id, is_official, pos, rotq, vel).await?)
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

    /// Turn a mapper-facing spec task outcome into the `TaskResponse` the
    /// editor is waiting on. Without this the request id never resolves and
    /// the editor's save/delete action hangs forever.
    fn send_spec_task_result(
        queue_tx: &UnboundedSender<Response>,
        request_id: u32,
        user_id: &Uuid,
        name_id: &str,
        action: &str,
        outcome: Result<(), String>,
    ) -> Result<(), ApiError> {
        let error = match outcome {
            Ok(()) => None,
            Err(error) => {
                warn!("Aux spec {} refused for user {} spec {}: {}", action, user_id, name_id, error);
                Some(error)
            }
        };
        queue_tx.send(Response::TaskResponse {
            id: request_id,
            success: error.is_none(),
            error,
        })?;
        Ok(())
    }

    /// Never hand a raw driver/database error to the client; log it and return
    /// a stable mapper-facing message instead.
    fn spec_task_internal_error(action: &str, error: impl std::fmt::Debug) -> String {
        warn!("Aux spec {} failed internally: {:?}", action, error);
        format!("Server error while {action} the spec")
    }

    pub async fn report_custom_map_aux_spec(
        pool: &Pool<Postgres>,
        request_id: u32,
        user_id: &Uuid,
        name_id: &str,
        spec: &serde_json::Value,
        queue_tx: &UnboundedSender<Response>,
    ) -> Result<(), ApiError> {
        let outcome = Self::upsert_custom_map_aux_spec(pool, user_id, name_id, spec).await;
        Self::send_spec_task_result(queue_tx, request_id, user_id, name_id, "save", outcome)
    }

    /// `Err` is the reason shown to the mapper. Catalog validation now rejects
    /// the whole spec, so the reason has to reach the editor.
    async fn upsert_custom_map_aux_spec(
        pool: &Pool<Postgres>,
        user_id: &Uuid,
        name_id: &str,
        spec: &serde_json::Value,
    ) -> Result<(), String> {
        if !spec.is_object() || spec.as_object().unwrap().is_empty() {
            return Err("Invalid spec: must be a non-empty object".to_string());
        }
        let existing = queries::custom_map_aux_specs::get_spec(pool, user_id, name_id)
            .await
            .map_err(|error| Self::spec_task_internal_error("saving", error))?;
        if let Some(existing) = existing {
            if existing.created_at < chrono::Utc::now() - chrono::Duration::hours(24) {
                return Err("Spec can no longer be modified".to_string());
            }
        }
        queries::custom_map_aux_specs::upsert_spec_and_catalogs(pool, user_id, name_id, spec)
            .await
            .map_err(|error| match error {
                queries::custom_map_aux_specs::UpsertWayfinderSpecError::Validation(error) => error.to_string(),
                error => Self::spec_task_internal_error("saving", error),
            })?;
        Ok(())
    }

    pub async fn delete_custom_map_aux_spec(
        pool: &Pool<Postgres>,
        request_id: u32,
        user_id: &Uuid,
        name_id: &str,
        queue_tx: &UnboundedSender<Response>,
    ) -> Result<(), ApiError> {
        let outcome = Self::delete_custom_map_aux_spec_inner(pool, user_id, name_id).await;
        Self::send_spec_task_result(queue_tx, request_id, user_id, name_id, "delete", outcome)
    }

    /// A spec a map is bound to cannot be deleted: the binding is that map's
    /// immutable authority and `map_listing_bind.spec_id` is `ON DELETE
    /// RESTRICT`, so the delete would raise a foreign key violation. Detect it
    /// up front and tell the mapper which map holds the spec.
    async fn delete_custom_map_aux_spec_inner(pool: &Pool<Postgres>, user_id: &Uuid, name_id: &str) -> Result<(), String> {
        let existing = queries::custom_map_aux_specs::get_spec(pool, user_id, name_id)
            .await
            .map_err(|error| Self::spec_task_internal_error("deleting", error))?;
        if let Some(existing) = existing {
            if existing.created_at < chrono::Utc::now() - chrono::Duration::hours(24) {
                return Err("Spec can no longer be deleted".to_string());
            }
        }
        let bound_map = queries::custom_map_aux_specs::bound_map_uid(pool, user_id, name_id)
            .await
            .map_err(|error| Self::spec_task_internal_error("deleting", error))?;
        if let Some(map_uid) = bound_map {
            return Err(format!("Spec is bound to map {map_uid} and can no longer be deleted"));
        }
        queries::custom_map_aux_specs::delete_spec(pool, user_id, name_id)
            .await
            .map_err(|error| Self::spec_task_internal_error("deleting", error))?;
        queries::minigames::delete_catalog(pool, user_id, name_id)
            .await
            .map_err(|error| Self::spec_task_internal_error("deleting", error))?;
        queries::collectibles::delete_catalog(pool, user_id, name_id)
            .await
            .map_err(|error| Self::spec_task_internal_error("deleting", error))?;
        queries::hats::delete_catalog(pool, user_id, name_id)
            .await
            .map_err(|error| Self::spec_task_internal_error("deleting", error))?;
        Ok(())
    }

    pub async fn report_minigame_attempt(
        pool: &Pool<Postgres>,
        user_id: &Uuid,
        map_uid: String,
        slug: String,
        spec_id: i64,
        elapsed_ms: i64,
        status: String,
    ) -> Result<(), api_error::Error> {
        if map_uid.len() != 27 {
            return Ok(());
        }
        if spec_id <= 0 {
            return Ok(());
        }
        if status != "finish" && status != "cancel" {
            return Ok(());
        }
        let Some(spec_id) = queries::listed_maps::resolve_bound_spec(pool, &map_uid, spec_id).await? else {
            return Ok(());
        };
        queries::minigames::report_attempt(pool, user_id, &map_uid, &slug, spec_id, elapsed_ms, &status).await?;
        Ok(())
    }

    /// Map join / bind report. The steady-state path (an already verified
    /// binding) runs inline so the 213/214 that follow it on the same FIFO
    /// still see the binding. A first bind or a re-verification has to call
    /// Nadeo, so it is deferred off the connection's request loop: the client
    /// treats its hat cache as tentative until a 213 confirms it.
    pub async fn report_minigame_map(
        pool: &Pool<Postgres>,
        mgr_tx: &UnboundedSender<ToPlayerMgr>,
        user_id: &Uuid,
        map_uid: String,
        spec_id: i64,
    ) -> Result<(), api_error::Error> {
        Self::report_minigame_map_deferring(pool, mgr_tx, NADEO_MAP_AUTHOR_VERIFIER.clone(), user_id, map_uid, spec_id).await
    }

    async fn report_minigame_map_deferring(
        pool: &Pool<Postgres>,
        mgr_tx: &UnboundedSender<ToPlayerMgr>,
        verifier: Arc<dyn crate::nadeo::MapAuthorVerifier>,
        user_id: &Uuid,
        map_uid: String,
        spec_id: i64,
    ) -> Result<(), api_error::Error> {
        if map_uid.len() != 27 || spec_id <= 0 {
            return Ok(());
        }
        if queries::listed_maps::bind_requires_author_verification(pool, user_id, &map_uid, spec_id).await? {
            let pool = pool.clone();
            let mgr_tx = mgr_tx.clone();
            let user_id = *user_id;
            spawn_logged("Wayfinder bind", async move {
                if let Err(error) =
                    Self::report_minigame_map_with_verifier(&pool, &mgr_tx, verifier.as_ref(), &user_id, map_uid.clone(), spec_id).await
                {
                    warn!(
                        "Deferred Wayfinder bind failed for player {} on map {} spec {}: {:?}",
                        user_id, map_uid, spec_id, error
                    );
                }
            });
            return Ok(());
        }
        Self::report_minigame_map_with_verifier(pool, mgr_tx, verifier.as_ref(), user_id, map_uid, spec_id).await
    }

    /// Track the real Wayfinder map UID this connection is on, broadcasting the
    /// leave/join hat pushes when it changes.
    ///
    /// The `ReportContext` map id cannot serve this purpose: the plugin sends
    /// the true `EdChallengeId` only when the map is Deep Dip (by UID, or by a
    /// name containing deep/dip/dd2) and otherwise sends the first 30 chars of
    /// its MD5. Requests 81, 213 and 214 all carry the real 27-char UID in
    /// their body, so that is what the hat gate and `BroadcastToMap` key on.
    async fn set_wayfinder_map(
        pool: &Pool<Postgres>,
        mgr_tx: &UnboundedSender<ToPlayerMgr>,
        tracked: &Arc<Mutex<Option<String>>>,
        user_id: &Uuid,
        display_name: &str,
        next: Option<&str>,
    ) -> Result<Option<String>, sqlx::Error> {
        let previous = {
            let mut tracked = tracked.lock().await;
            if tracked.as_deref() == next {
                return Ok(tracked.clone());
            }
            std::mem::replace(&mut *tracked, next.map(str::to_string))
        };
        Self::broadcast_hat_context_transition(pool, mgr_tx, user_id, display_name, previous.as_deref(), next).await?;
        Ok(next.map(str::to_string))
    }

    /// Adopt the map UID carried by a request body, ignoring anything that is
    /// not a real map UID so a malformed request cannot silently move the
    /// player off the map they are on. Returns the tracked map either way.
    async fn adopt_wayfinder_map(
        pool: &Pool<Postgres>,
        mgr_tx: &UnboundedSender<ToPlayerMgr>,
        tracked: &Arc<Mutex<Option<String>>>,
        user_id: &Uuid,
        display_name: &str,
        uid: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        if uid.len() != 27 {
            return Ok(tracked.lock().await.clone());
        }
        Self::set_wayfinder_map(pool, mgr_tx, tracked, user_id, display_name, Some(uid)).await
    }

    /// Apply a `ReportContext` map id to the tracked Wayfinder map.
    ///
    /// Clears it -- emitting the leave push -- only when the context genuinely
    /// names some other map. A context report that has merely caught up with
    /// the map request 81 already adopted names the same map, in either the
    /// true-UID or the MD5-prefix form, and must be a no-op: the two arrive
    /// from independent plugin coroutines and 81 wins the race about half the
    /// time.
    async fn apply_context_map_change(
        pool: &Pool<Postgres>,
        mgr_tx: &UnboundedSender<ToPlayerMgr>,
        tracked: &Arc<Mutex<Option<String>>>,
        user_id: &Uuid,
        display_name: &str,
        context_uid: Option<&str>,
    ) -> Result<Option<String>, sqlx::Error> {
        let current = tracked.lock().await.clone();
        if current
            .as_deref()
            .is_some_and(|tracked_uid| context_uid_refers_to_map(context_uid, tracked_uid))
        {
            return Ok(current);
        }
        Self::set_wayfinder_map(pool, mgr_tx, tracked, user_id, display_name, None).await
    }

    /// `adopt_wayfinder_map`, but a failed leave/join push is logged rather than
    /// propagated: requests 213/214 owe the client a response either way, and
    /// the tracked map has already been updated by the time a push can fail.
    async fn adopt_wayfinder_map_or_tracked(
        pool: &Pool<Postgres>,
        mgr_tx: &UnboundedSender<ToPlayerMgr>,
        tracked: &Arc<Mutex<Option<String>>>,
        user_id: &Uuid,
        display_name: &str,
        uid: &str,
    ) -> Option<String> {
        match Self::adopt_wayfinder_map(pool, mgr_tx, tracked, user_id, display_name, uid).await {
            Ok(current) => current,
            Err(error) => {
                warn!(
                    "Hat presence push failed for player {} on map {} (answering anyway): {:?}",
                    user_id, uid, error
                );
                tracked.lock().await.clone()
            }
        }
    }

    /// Emit the leave push for the map the player was on and the join push for
    /// the map they are now on. "Context" here means the tracked Wayfinder map
    /// (see `set_wayfinder_map`), not the `ReportContext` map id.
    async fn broadcast_hat_context_transition(
        pool: &Pool<Postgres>,
        mgr_tx: &UnboundedSender<ToPlayerMgr>,
        user_id: &Uuid,
        display_name: &str,
        previous_map_uid: Option<&str>,
        current_map_uid: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        Self::broadcast_hat_context_transition_with_join_ready(
            pool,
            mgr_tx,
            user_id,
            display_name,
            previous_map_uid,
            current_map_uid,
            std::future::ready(()),
        )
        .await
    }

    async fn broadcast_hat_context_transition_with_join_ready<F>(
        pool: &Pool<Postgres>,
        mgr_tx: &UnboundedSender<ToPlayerMgr>,
        user_id: &Uuid,
        display_name: &str,
        previous_map_uid: Option<&str>,
        current_map_uid: Option<&str>,
        join_ready: F,
    ) -> Result<(), sqlx::Error>
    where
        F: std::future::Future<Output = ()> + Send,
    {
        if previous_map_uid == current_map_uid {
            return Ok(());
        }

        if let Some(previous_map_uid) = previous_map_uid.filter(|uid| uid.len() == 27) {
            let map_operation = WAYFINDER_MAP_OPERATION_LOCKS.for_map(previous_map_uid);
            let _map_operation = map_operation.lock().await;
            let _ = mgr_tx.send(ToPlayerMgr::BroadcastToMap {
                map_uid: previous_map_uid.to_string(),
                response: Response::HatsOnMapUpdate {
                    uid: previous_map_uid.to_string(),
                    hats: vec![crate::router::HatOnMapPlayer {
                        wsid: user_id.to_string(),
                        name: display_name.to_string(),
                        slug: String::new(),
                    }],
                },
            });
        }

        if let Some(current_map_uid) = current_map_uid.filter(|uid| uid.len() == 27) {
            let map_operation = WAYFINDER_MAP_OPERATION_LOCKS.for_map(current_map_uid);
            let _map_operation = map_operation.lock().await;
            if let Some(entry) = queries::hats::equipped_push_entry(pool, user_id, current_map_uid).await? {
                join_ready.await;
                let _ = mgr_tx.send(ToPlayerMgr::BroadcastToMap {
                    map_uid: current_map_uid.to_string(),
                    response: Response::HatsOnMapUpdate {
                        uid: current_map_uid.to_string(),
                        hats: vec![entry],
                    },
                });
            }
        }
        Ok(())
    }

    async fn report_minigame_map_with_verifier(
        pool: &Pool<Postgres>,
        mgr_tx: &UnboundedSender<ToPlayerMgr>,
        verifier: &dyn crate::nadeo::MapAuthorVerifier,
        user_id: &Uuid,
        map_uid: String,
        spec_id: i64,
    ) -> Result<(), api_error::Error> {
        if map_uid.len() != 27 {
            return Ok(());
        }
        if spec_id <= 0 {
            return Ok(());
        }
        // Nadeo I/O runs before the per-map lock is taken. Holding the lock
        // across an HTTP round trip queues every other player's 81/85 on this
        // map behind one mapper's verification.
        let prepared = queries::listed_maps::prepare_authoritative_bind(pool, verifier, user_id, &map_uid, spec_id)
            .await
            .map_err(|error| ApiError::StrErr(error.to_string()))?;
        let map_operation = WAYFINDER_MAP_OPERATION_LOCKS.for_map(&map_uid);
        let _map_operation = map_operation.lock().await;
        let binding = queries::listed_maps::commit_authoritative_bind(pool, &prepared, user_id, &map_uid, spec_id)
            .await
            .map_err(|error| ApiError::StrErr(error.to_string()))?;
        if !binding.removed_hat_wearers.is_empty() {
            let _ = mgr_tx.send(ToPlayerMgr::BroadcastToMap {
                map_uid: map_uid.clone(),
                response: Response::HatsOnMapUpdate {
                    uid: map_uid.clone(),
                    hats: binding.removed_hat_wearers,
                },
            });
        }
        debug!(
            "Authoritative Wayfinder spec {} for map {} materialized={}",
            binding.spec_id, map_uid, binding.materialized
        );
        Ok(())
    }

    pub async fn report_collectible_unlock(
        pool: &Pool<Postgres>,
        user_id: &Uuid,
        map_uid: String,
        slug: String,
        spec_id: i64,
    ) -> Result<(), api_error::Error> {
        if map_uid.len() != 27 || spec_id <= 0 || slug.is_empty() {
            return Ok(());
        }
        let Some(spec_id) = queries::listed_maps::resolve_bound_spec(pool, &map_uid, spec_id).await? else {
            return Ok(());
        };
        queries::collectibles::report_unlock(pool, user_id, &map_uid, &slug, spec_id).await?;
        Ok(())
    }

    pub async fn report_collectible_collect(
        pool: &Pool<Postgres>,
        user_id: &Uuid,
        map_uid: String,
        slug: String,
        spec_id: i64,
        x: f32,
        y: f32,
        z: f32,
        source: String,
    ) -> Result<(), api_error::Error> {
        if map_uid.len() != 27 || spec_id <= 0 || slug.is_empty() {
            return Ok(());
        }
        let Some(source) = queries::collectibles::collect_log_source(&source) else {
            return Ok(());
        };
        let Some(spec_id) = queries::listed_maps::resolve_bound_spec(pool, &map_uid, spec_id).await? else {
            return Ok(());
        };
        queries::collectibles::report_collect(pool, user_id, &map_uid, &slug, spec_id, x, y, z, source).await?;
        Ok(())
    }

    /// Hat unlock report. Validation calls Nadeo (up to two record lookups per
    /// declared record uid, each retried), so it runs off the connection's
    /// request loop and nothing else on the socket stalls behind that I/O.
    ///
    /// The plugin pushes 84 and 213 back to back on the same FIFO and treats
    /// the 213 as the authoritative acknowledgement, relocking and persisting
    /// any hat the answer reports as locked. Answering that 213 inline while
    /// the validation is still running would therefore make the client persist
    /// the pre-unlock state. So the request is registered as in flight, 213 for
    /// that map is withheld while it is, and the validation pushes the 213
    /// itself when it commits. `ApplyMine` is idempotent and keyed on the map
    /// uid, so the late push reconciles with no plugin change.
    pub async fn report_hat_unlock(
        pool: &Pool<Postgres>,
        queue_tx: &UnboundedSender<Response>,
        pending: &Arc<PendingHatUnlocks>,
        user_id: &Uuid,
        map_uid: String,
        slug: String,
        spec_id: i64,
    ) -> Result<(), api_error::Error> {
        Self::report_hat_unlock_with_verifier(
            pool,
            NADEO_RECORD_VERIFIER.clone(),
            queue_tx,
            pending,
            user_id,
            map_uid,
            slug,
            spec_id,
        )
        .await
    }

    async fn report_hat_unlock_with_verifier(
        pool: &Pool<Postgres>,
        verifier: Arc<dyn crate::nadeo::OfficialRecordVerifier>,
        queue_tx: &UnboundedSender<Response>,
        pending: &Arc<PendingHatUnlocks>,
        user_id: &Uuid,
        map_uid: String,
        slug: String,
        spec_id: i64,
    ) -> Result<(), api_error::Error> {
        if map_uid.len() != 27 || spec_id <= 0 || slug.is_empty() {
            return Ok(());
        }
        // Resolving the binding is one indexed query and never touches Nadeo,
        // so it stays on the request path: an unbound map costs a query, not a
        // spawned task.
        let Some(bound_spec_id) = queries::listed_maps::resolve_bound_spec(pool, &map_uid, spec_id).await? else {
            warn!(
                "Hat unlock dropped for player {} on map {} hat {}: no verified binding for spec {}",
                user_id, map_uid, slug, spec_id
            );
            return Ok(());
        };
        // Records that a 213 answer is owed for this hat, and dedupes a repeat
        // of the same map+hat onto the pending validation. Registered before
        // the spawn so no 213 can slip in ahead of it.
        let Some(mut owed) = pending.begin(&map_uid, &slug) else {
            warn!(
                "Hat unlock for player {} on map {} hat {} coalesced or refused: an answer is already owed for it, \
                 or the connection is over its pending-unlock bound",
                user_id, map_uid, slug
            );
            return Ok(());
        };
        let pool = pool.clone();
        let queue_tx = queue_tx.clone();
        let user_id = *user_id;
        let permits = pending.validation_permits.clone();
        let validation_pool = pool.clone();
        let validation_map_uid = map_uid.clone();
        spawn_logged("hat unlock validation", async move {
            {
                // Acquired here rather than in the handler so a batch larger
                // than the cap queues instead of being discarded: the plugin
                // fires 84 for every newly-satisfied hat in one pass, and a
                // dropped unlock is later reported back to the client as
                // locked and persisted to disk.
                //
                // Only closed at shutdown; running unthrottled beats leaving a
                // withheld 213 unanswered.
                let _permit = permits.acquire_owned().await.ok();
                // Run the validation as its own task so a panic inside it
                // surfaces here as an error rather than skipping the answer
                // below.
                let validation = tokio::spawn(async move {
                    Self::run_hat_unlock_validation(&validation_pool, verifier.as_ref(), &user_id, &validation_map_uid, &slug, bound_spec_id)
                        .await
                });
                if let Err(error) = validation.await {
                    warn!(
                        "Hat unlock validation for player {} on map {} panicked or was cancelled: {:?}",
                        user_id, map_uid, error
                    );
                }
            }
            // The 213s the client sent behind its 84s were withheld, so answer
            // them -- but only once the whole batch for this map has drained.
            // Pushing while a sibling validation is still running would report
            // that sibling's hat as locked, and the client persists that.
            if owed.release() {
                if let Err(error) = Self::push_hats_mine(&pool, &queue_tx, &map_uid, &user_id).await {
                    warn!(
                        "Failed to push post-validation HatsMine to player {} for map {}: {:?}",
                        user_id, map_uid, error
                    );
                }
            }
        });
        Ok(())
    }

    /// Validate and log the verdict. There is no unlock response in the
    /// protocol, so a denial has to be visible in the server log.
    async fn run_hat_unlock_validation(
        pool: &Pool<Postgres>,
        verifier: &dyn crate::nadeo::OfficialRecordVerifier,
        user_id: &Uuid,
        map_uid: &str,
        slug: &str,
        bound_spec_id: i64,
    ) -> bool {
        match queries::hats::report_unlock(pool, verifier, user_id, map_uid, slug, bound_spec_id).await {
            Ok(true) => true,
            Ok(false) => {
                warn!("Hat unlock denied for player {} on map {} hat {}", user_id, map_uid, slug);
                false
            }
            Err(error) => {
                warn!(
                    "Hat unlock failed for player {} on map {} hat {}: {:?}",
                    user_id, map_uid, slug, error
                );
                false
            }
        }
    }

    pub async fn report_hat_equip(
        pool: &Pool<Postgres>,
        mgr_tx: &UnboundedSender<ToPlayerMgr>,
        queue_tx: &UnboundedSender<Response>,
        user_id: &Uuid,
        current_map_uid: Option<&str>,
        map_uid: String,
        slug: String,
    ) -> Result<(), api_error::Error> {
        Self::report_hat_equip_with_post_commit(
            pool,
            mgr_tx,
            queue_tx,
            user_id,
            current_map_uid,
            map_uid,
            slug,
            std::future::ready(()),
        )
        .await
    }

    async fn report_hat_equip_with_post_commit<F>(
        pool: &Pool<Postgres>,
        mgr_tx: &UnboundedSender<ToPlayerMgr>,
        queue_tx: &UnboundedSender<Response>,
        user_id: &Uuid,
        current_map_uid: Option<&str>,
        map_uid: String,
        slug: String,
        post_commit: F,
    ) -> Result<(), api_error::Error>
    where
        F: std::future::Future<Output = ()> + Send,
    {
        if map_uid.len() != 27 || !delivery_matches_map(current_map_uid, &map_uid) {
            warn!(
                "Hat equip dropped for player {}: reported map {} is not the tracked Wayfinder map {:?}",
                user_id, map_uid, current_map_uid
            );
            return Ok(());
        }
        match HAT_EQUIP_LIMITER.check(user_id) {
            queries::hats::HatEquipRateVerdict::Allowed => {}
            queries::hats::HatEquipRateVerdict::PerUser => {
                // The client applies the equip optimistically and reconciles
                // from the next 213, so a silent drop looks like the UI
                // snapping back for no reason. `NonFatalErrorMsg` is an
                // existing response the plugin already handles.
                warn!("Hat equip throttled for player {} on map {}", user_id, map_uid);
                queue_tx.send(Response::NonFatalErrorMsg {
                    level: 2,
                    msg: "Hat change was too fast - try again in a moment.".to_string(),
                })?;
                return Ok(());
            }
            queries::hats::HatEquipRateVerdict::Global => {
                warn!(
                    "Hat equip dropped by the global rate limiter for player {} on map {}",
                    user_id, map_uid
                );
                return Ok(());
            }
        }
        let map_operation = WAYFINDER_MAP_OPERATION_LOCKS.for_map(&map_uid);
        let _map_operation = map_operation.lock().await;
        let Some(entry) = queries::hats::report_equip(pool, user_id, &map_uid, &slug).await? else {
            return Ok(());
        };
        post_commit.await;
        let _ = mgr_tx.send(ToPlayerMgr::BroadcastToMap {
            map_uid: map_uid.clone(),
            response: Response::HatsOnMapUpdate {
                uid: map_uid,
                hats: vec![entry],
            },
        });
        Ok(())
    }

    pub async fn send_hats_mine(
        pool: &Pool<Postgres>,
        queue_tx: &UnboundedSender<Response>,
        pending: &Arc<PendingHatUnlocks>,
        current_map_uid: Option<&str>,
        map_uid: String,
        user_id: &Uuid,
    ) -> Result<(), api_error::Error> {
        if !delivery_matches_map(current_map_uid, &map_uid) {
            debug!(
                "Withholding HatsMine for player {}: requested map {} is not the tracked Wayfinder map {:?}",
                user_id, map_uid, current_map_uid
            );
            return Ok(());
        }
        // The client sends 213 immediately behind 84 and treats the answer as
        // authoritative, relocking and persisting anything reported locked. An
        // answer read now would still show the pre-unlock state, so leave it to
        // the push that `report_hat_unlock`'s validation issues on completion.
        if pending.any_for_map(&map_uid) {
            debug!(
                "Withholding HatsMine for player {} on map {}: an unlock validation is in flight and will answer it",
                user_id, map_uid
            );
            return Ok(());
        }
        Self::push_hats_mine(pool, queue_tx, &map_uid, user_id).await
    }

    /// Send the 213 payload without the request-time gates. Used to answer a
    /// 213 that was withheld while a hat unlock validation was in flight.
    async fn push_hats_mine(
        pool: &Pool<Postgres>,
        queue_tx: &UnboundedSender<Response>,
        map_uid: &str,
        user_id: &Uuid,
    ) -> Result<(), api_error::Error> {
        // An empty `HatsMine` is authoritative to the client, so an unbound or
        // unverified map is answered with silence, not an empty list.
        let Some((equipped, items)) = queries::hats::get_hats_mine(pool, map_uid, user_id).await? else {
            debug!("Withholding HatsMine for map {}: no verified Wayfinder binding", map_uid);
            return Ok(());
        };
        queue_tx.send(Response::HatsMine {
            uid: map_uid.to_string(),
            equipped,
            items,
        })?;
        Ok(())
    }

    pub async fn send_hats_on_map(
        pool: &Pool<Postgres>,
        queue_tx: &UnboundedSender<Response>,
        current_map_uid: Option<&str>,
        map_uid: String,
    ) -> Result<(), api_error::Error> {
        if !delivery_matches_map(current_map_uid, &map_uid) {
            return Ok(());
        }
        let hats = queries::hats::get_hats_on_map(pool, &map_uid).await?;
        queue_tx.send(Response::HatsOnMap {
            uid: map_uid.clone(),
            hats,
        })?;
        Ok(())
    }

    pub async fn send_minigame_lb(
        pool: &Pool<Postgres>,
        queue_tx: &UnboundedSender<Response>,
        map_uid: String,
        slug: String,
        start: i64,
        end: i64,
    ) -> Result<(), api_error::Error> {
        let entries = queries::minigames::get_minigame_leaderboard(pool, &map_uid, &slug, start, end).await?;
        queue_tx.send(Response::MinigameLB {
            uid: map_uid,
            slug,
            entries,
        })?;
        Ok(())
    }

    pub async fn get_minigame_my_rank(
        pool: &Pool<Postgres>,
        map_uid: String,
        slug: String,
        user_id: &Uuid,
        queue_tx: &UnboundedSender<Response>,
    ) -> Result<(), api_error::Error> {
        let r = queries::minigames::get_minigame_my_rank(pool, &map_uid, &slug, user_id).await?;
        queue_tx.send(Response::MinigameRank { uid: map_uid, slug, r })?;
        Ok(())
    }

    pub async fn send_minigame_list(
        pool: &Pool<Postgres>,
        queue_tx: &UnboundedSender<Response>,
        map_uid: String,
    ) -> Result<(), api_error::Error> {
        let games = queries::minigames::get_minigame_list(pool, &map_uid).await?;
        queue_tx.send(Response::MinigameList { uid: map_uid, games })?;
        Ok(())
    }

    pub async fn send_collectible_list(
        pool: &Pool<Postgres>,
        queue_tx: &UnboundedSender<Response>,
        map_uid: String,
    ) -> Result<(), api_error::Error> {
        let items = queries::collectibles::get_collectible_list(pool, &map_uid).await?;
        queue_tx.send(Response::CollectibleList { uid: map_uid, items })?;
        Ok(())
    }

    pub async fn send_collectible_mine(
        pool: &Pool<Postgres>,
        queue_tx: &UnboundedSender<Response>,
        map_uid: String,
        user_id: &Uuid,
    ) -> Result<(), api_error::Error> {
        let items = queries::collectibles::get_collectible_mine(pool, &map_uid, user_id).await?;
        queue_tx.send(Response::CollectibleMine { uid: map_uid, items })?;
        Ok(())
    }

    pub async fn list_custom_map_aux_spec(
        pool: &Pool<Postgres>,
        request_id: u32,
        user_id: &Uuid,
        queue_tx: &UnboundedSender<Response>,
    ) -> Result<(), ApiError> {
        let specs = queries::custom_map_aux_specs::list_specs(pool, user_id).await?;
        queue_tx.send(Response::TaskResponseJson {
            id: request_id,
            data: specs.into(),
        })?;
        Ok(())
    }
}
