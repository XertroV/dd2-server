use std::str::FromStr;
use std::sync::Arc;

use base64::Engine;
use log::{info, warn};
use sqlx::types::Uuid;
use sqlx::{query, Pool, Postgres};
use tokio::io::AsyncWriteExt;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{error::SendError, unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::{Mutex, OnceCell};

use crate::api_error::Error;
use crate::consts::DD2_MAP_UID;
use crate::op_auth::{check_token, TokenResp};
use crate::queries::{
    self, context_mark_succeeded, create_session, get_user_stats, insert_context, insert_context_packed, insert_finish, insert_gc_nod,
    insert_respawn, insert_start_fall, insert_vehicle_state, register_or_login, resume_session, update_fall_with_end,
    update_user_pb_height, update_users_stats, Session, User,
};
use crate::router::{write_response, Map, PlayerCtx, Request, Response, Stats};
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
    ip_address: String,
}

impl Player {
    pub fn new(tx_mgr: UnboundedSender<ToPlayerMgr>, ip_address: String) -> Self {
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
            ip_address,
        }
    }

    pub fn get_session(&self) -> &LoginSession {
        self.session.get().unwrap()
    }

    pub fn display_name(&self) -> &str {
        self.get_session().display_name()
    }

    pub fn user_id(&self) -> &Uuid {
        self.get_session().user_id()
    }

    pub fn session_id(&self) -> &Uuid {
        self.get_session().session_id()
    }

    pub fn is_connected(&self) -> bool {
        self.has_shutdown.get().is_none() || self.tx.is_closed() || self.queue_tx.is_closed()
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
            info!(
                "Player {} ({}) authenticated / resumed: {}",
                ls.display_name(),
                ls.user.web_services_user_id,
                ls.resumed
            );
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
                return Err(format!("[{}] Error reading from socket: {:?}", p.display_name(), err).into());
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
        let session = create_session(&pool, user.id(), &plugin_info, &game_info, &gamer_info, &p.ip_address).await?;
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
        let (s, u) = resume_session(&pool, &session_token, &plugin_info, &game_info, &gamer_info, &p.ip_address).await?;
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
                        let _ = p.tx_mgr.send(ToPlayerMgr::SocketError());
                        let _ = p.has_shutdown.set(true);
                        return;
                    }
                };
                let res = match msg {
                    Request::Authenticate { .. } => Ok(()),  // ignore
                    Request::ResumeSession { .. } => Ok(()), // ignore
                    Request::ReportContext { sf, mi, map, i, bi } => match (parse_u64_str(&sf), parse_u64_str(&mi)) {
                        (Ok(sf), Ok(mi)) => Player::update_context(&pool, p.clone(), sf, mi, map.as_ref(), i.unwrap_or(false), bi).await,
                        _ => Err("Invalid sf/mi".to_string().into()),
                    },
                    Request::ReportGCNodMsg { data } => Player::on_report_gcnod_msg(&pool, p.clone(), &data).await,
                    Request::Ping {} => p.queue_tx.send(Response::Ping {}).map_err(Into::into),
                    Request::ReportVehicleState { vel, pos, rotq } => Player::report_vehicle_state(&pool, p.clone(), pos, rotq, vel).await,
                    Request::ReportRespawn { race_time } => Player::report_respawn(&pool, p.clone(), race_time).await,
                    Request::ReportFinish { race_time } => Player::report_finish(&pool, p.clone(), race_time).await,
                    Request::ReportFallStart {
                        floor,
                        pos,
                        speed,
                        start_time,
                    } => Player::on_report_fall_start(&pool, p.clone(), floor as i32, pos.into(), speed, start_time as i32).await,
                    Request::ReportFallEnd { floor, pos, end_time } => {
                        Player::on_report_fall_end(&pool, p.clone(), floor as i32, pos.into(), end_time as i32).await
                    }
                    Request::ReportStats { stats } => Player::on_report_stats(&pool, p.clone(), stats).await,
                    // Request::ReportMapLoad { uid } => Player::on_report_map_load(&pool, p.clone(), &uid).await,
                    Request::ReportPBHeight { h } => Player::on_report_pb_height(&pool, p.clone(), h).await,
                    Request::GetMyStats {} => Player::get_stats(&pool, p.clone()).await,
                    Request::GetGlobalLB { start, end } => Player::get_global_lb(&pool, p.clone(), start as i32, end as i32).await,
                    Request::GetFriendsLB { friends } => todo!(), // Player::get_friends_lb(&pool, p.clone(), &friends).await,
                    Request::GetGlobalOverview {} => Player::get_global_overview(&pool, p.clone()).await,
                    Request::GetServerStats {} => Player::get_server_stats(&pool, p.clone()).await,
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
    pub async fn get_server_stats(pool: &Pool<Postgres>, p: Arc<Player>) -> Result<(), Error> {
        let nb_players_live = queries::get_server_info(pool).await?;
        Ok(p.queue_tx.send(Response::ServerInfo { nb_players_live })?)
    }

    pub async fn get_global_overview(pool: &Pool<Postgres>, p: Arc<Player>) -> Result<(), Error> {
        let overview = queries::get_global_overview(pool).await?;
        Ok(p.queue_tx.send(Response::GlobalOverview { j: overview })?)
    }

    pub async fn get_friends_lb(pool: &Pool<Postgres>, p: Arc<Player>, friends: &[Uuid]) -> Result<(), Error> {
        let lb = queries::get_friends_lb(pool, p.user_id(), friends).await?;
        Ok(p.queue_tx.send(Response::FriendsLB {
            entries: lb.into_iter().map(|e| e.into()).collect(),
        })?)
    }

    pub async fn get_global_lb(pool: &Pool<Postgres>, p: Arc<Player>, start: i32, end: i32) -> Result<(), Error> {
        let lb = queries::get_global_lb(pool, start, end).await?;
        Ok(p.queue_tx.send(Response::GlobalLB {
            entries: lb.into_iter().map(|e| e.into()).collect(),
        })?)
    }

    pub async fn get_stats(pool: &Pool<Postgres>, p: Arc<Player>) -> Result<(), Error> {
        match get_user_stats(pool, p.get_session().user_id()).await {
            Ok((stats, rank)) => Ok(p.queue_tx.send(Response::Stats { stats, rank })?),
            // nothing to return
            Err(sqlx::Error::RowNotFound) => Ok(()),
            Err(e) => Ok(Err(e)?),
        }
    }

    pub async fn report_vehicle_state(
        pool: &Pool<Postgres>,
        p: Arc<Player>,
        pos: [f32; 3],
        rotq: [f32; 4],
        vel: [f32; 3],
    ) -> Result<(), Error> {
        let ctx_id_lock = p.context_id.lock().await;
        let ctx_id = ctx_id_lock.as_ref();
        let is_official = p.context.lock().await.as_ref().map(|ctx| ctx.is_official()).unwrap_or(false);
        Ok(insert_vehicle_state(pool, p.get_session().session_id(), ctx_id, is_official, pos, rotq, vel).await?)
    }

    pub async fn update_context(
        pool: &Pool<Postgres>,
        p: Arc<Player>,
        sf: u64,
        mi: u64,
        map: Option<&Map>,
        has_vl_item: bool,
        bi: [i32; 2],
    ) -> Result<(), Error> {
        let mut ctx = p.context.lock().await;
        let mut ctx_id = p.context_id.lock().await;
        let mut previous_ctx = None;
        if let Some(ctx_id) = ctx_id.as_ref() {
            let _ = previous_ctx.insert(ctx_id.clone());
        }
        let new_ctx = PlayerCtx::new(sf, mi, map.cloned(), has_vl_item);
        let map_id = match map {
            Some(map) => {
                let map_id = crate::queries::get_or_insert_map(pool, &map.uid, &map.name, &map.hash).await?;
                Some(map_id)
            }
            None => None,
        };
        let new_id = insert_context_packed(pool, p.get_session().session_id(), &new_ctx, &previous_ctx, map_id, bi).await?;
        if let Some(previous_ctx) = previous_ctx {
            context_mark_succeeded(pool, &previous_ctx, &new_id).await?;
        }
        *ctx = Some(new_ctx);
        *ctx_id = Some(new_id);
        Ok(())
    }

    pub async fn on_report_gcnod_msg(pool: &Pool<Postgres>, p: Arc<Player>, data: &str) -> Result<(), Error> {
        // decode base64

        let x = if data.len() < 0x2E0 {
            data.into()
        } else {
            base64::prelude::BASE64_URL_SAFE.decode(data).unwrap_or(data.into())
        };

        match p.context_id.lock().await.as_ref() {
            Some(ctx_id) => {
                insert_gc_nod(pool, p.get_session().session_id(), ctx_id, &x).await?;
            }
            None => {
                if (data.len() > 0x20) {
                    warn!("GCNod message without context");
                }
            }
        };
        Ok(())
    }

    pub async fn report_respawn(pool: &Pool<Postgres>, p: Arc<Player>, race_time: i32) -> Result<(), Error> {
        insert_respawn(pool, p.session_id(), race_time).await?;
        Ok(())
    }

    pub async fn report_finish(pool: &Pool<Postgres>, p: Arc<Player>, race_time: i32) -> Result<(), Error> {
        insert_finish(pool, p.session_id(), race_time).await?;
        Ok(())
    }

    pub async fn on_report_stats(pool: &Pool<Postgres>, p: Arc<Player>, stats: Stats) -> Result<(), Error> {
        Ok(update_users_stats(pool, p.user_id(), &stats).await?)
    }

    pub async fn on_report_pb_height(pool: &Pool<Postgres>, p: Arc<Player>, h: f32) -> Result<(), Error> {
        if !p.context.lock().await.as_ref().map(|c| c.is_official()).unwrap_or(false) {
            let lock = p.context.lock().await;
            match lock.as_ref() {
                None => warn!("Dropping PB height b/c no context"),
                Some(ctx) => {
                    info!("context: {:?}", ctx);
                    match ctx.map.as_ref() {
                        None => warn!("Dropping PB height b/c no map"),
                        Some(map) => {
                            if map.uid != DD2_MAP_UID {
                                warn!("Dropping PB height b/c not DD2; user: {}", p.get_session().display_name());
                                return Ok(());
                            } else {
                                if !check_flags_sf_mi(ctx.sf, ctx.mi) {
                                    warn!("Dropping PB height b/c invalid sf/mi; user: {}", p.get_session().display_name());
                                    return Ok(());
                                } else {
                                    if !ctx.is_official() {
                                        warn!("Dropping PB height b/c not official; user: {}", p.get_session().display_name());
                                        return Ok(());
                                    } else {
                                        warn!("Dropping PB height b/c unknown reason; user: {}", p.get_session().display_name());
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // let uid = ;
            // warn!("Dropping PB height b/c not official; user: {}", p.display_name());
            return Ok(());
        }
        update_user_pb_height(pool, p.get_session().user_id(), h as f64).await?;
        info!("updated user pb height: {}: {}", p.display_name(), h);
        Ok(())
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
        Ok(insert_start_fall(pool, p.user_id(), p.session_id(), floor, pos, speed, start_time).await?)
    }

    pub async fn on_report_fall_end(
        pool: &Pool<Postgres>,
        p: Arc<Player>,
        floor: i32,
        pos: (f32, f32, f32),
        end_time: i32,
    ) -> Result<(), Error> {
        update_fall_with_end(pool, p.user_id(), floor, pos, end_time).await?;
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

/// Context stuff

const CONTEXT_COEFFICIENTS: [i32; 15] = [69, 59, 136, 1, 26, 77, 41, 1, 95, 53, 1, 1, 86, 62, 89];
// from shuffled to plain order
const CONTEXT_MAP_CYPHER_TO_PLAIN: [i32; 15] = [6, 1, 14, -1, 12, 0, 8, -1, 13, 5, -1, -1, 2, 4, 9];
const CONTEXT_MAP_PLAIN_TO_CYPHER: [i32; 15] = [5, 1, 12, -1, 13, 9, 0, -1, 6, 14, -1, -1, 4, 8, 2];

pub fn context_map_cypher_to_plain(cypher: i32) -> i32 {
    let ret = CONTEXT_MAP_CYPHER_TO_PLAIN[cypher as usize];
    if ret == -1 {
        // panic!("invalid index: {}", cypher);
        return cypher;
    }
    ret
}

pub fn context_map_plain_to_cypher(plain: i32) -> i32 {
    let ret = CONTEXT_MAP_PLAIN_TO_CYPHER[plain as usize];
    if ret == -1 {
        // panic!("invalid index: {}", plain);
        return plain;
    }
    ret
}

pub fn check_flags_sf_mi(sf: u64, mi: u64) -> bool {
    let scene_flags = decode_context_flags(sf);
    // todo
    true
}

/*

    // !
    const int[] unsafeMap = {5, 1, 12, -1, 13, 9, 0, -1, 6, 14, -1, -1, 4, 8, 2};
    bool[]@ unsafeEncodeShuffle(const array<bool>@ arr) {
        bool[] ret = array<bool>(15);
        for (uint i = 0; i < 15; i++) {
            if (unsafeMap[i] == -1) {
                ret[i] = arr[i];
            } else {
                ret[unsafeMap[i]] = arr[i];
            }
        }
        return ret;
    }
    uint64 encode(const array<bool>@ arr) {
        uint64 ret = 1;
        for (uint i = 0; i < 15; i++) {
            if (arr[i]) {
                ret *= uint64(lambda[i]);
            }
            while (ret & 1 == 0) {
                trace("trimming: " + Text::FormatPointer(ret));
                ret = ret >> 1;
            }
            trace("ret: " + Text::FormatPointer(ret));
        }
        return ret;
    }
    void runEncTest() {
        bool[] arr = {true, false, true, false, true, false, true, false, true, false, true, false, true, false, true};
        runEncTestCase(arr);
        for (uint i = 0; i < 15; i++) {
            arr[i] = true;
        }
        runEncTestCase(arr);
        for (uint i = 0; i < 15; i++) {
            arr[i] = i % 3 == 0;
        }
        runEncTestCase(arr);
        print("^ mod 3");
        for (uint i = 0; i < 15; i++) {
            arr[i] = i % 5 == 0;
        }
        runEncTestCase(arr);
        print("^ mod 5");
    }

    void runEncTestCase(const array<bool>@ arr) {
        auto sarr = unsafeEncodeShuffle(arr);
        trace('Encoded: ' + Text::FormatPointer(encode(sarr)));
        // auto enc = encode(arr);
        // trace("enc: " + Text::FormatPointer(enc));
        // auto dec = decode(enc);
        // trace("dec: " + dec);
        // auto enc2 = encode(dec);
        // trace("enc2: " + Text::FormatPointer(enc2));
        // auto dec2 = decode(enc2);
        // trace("dec2: " + dec2);
    }

*/

pub fn encode_context_flags(arr: &[bool; 15]) -> u64 {
    let mut ret = 1;
    for i in 0..15 {
        if arr[i] {
            ret *= CONTEXT_COEFFICIENTS[context_map_plain_to_cypher(i as i32) as usize] as u64;
        }
        while ret & 1 == 0 {
            ret = ret >> 1;
        }
    }
    ret
}

pub fn decode_context_flags(flags: u64) -> [bool; 15] {
    let mut plain = [false; 15];
    let mut flags = flags;
    for i in 0..15 {
        if CONTEXT_COEFFICIENTS[i] < 2 {
            continue;
        }
        let mut c = CONTEXT_COEFFICIENTS[i] as u64;
        while c & 1 == 0 && c > 0 {
            c = c >> 1;
        }
        if flags % c == 0 {
            plain[context_map_cypher_to_plain(i as i32) as usize] = true;
            flags = flags / c;
        }
    }
    plain
}

pub fn parse_u64_str(s: &str) -> Result<u64, std::num::ParseIntError> {
    if &s[..2] == "0x" {
        return u64::from_str_radix(&s[2..], 16);
    }
    u64::from_str_radix(s, 16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode() {
        let all_true: [bool; 15] = [true; 15];
        let enc = encode_context_flags(&all_true);
        // eprintln!("{:?}", enc);
        let plain = decode_context_flags(enc);
        // eprintln!("{:?}", plain);
        for i in 0..15 {
            if CONTEXT_MAP_CYPHER_TO_PLAIN[i] < 0 {
                continue;
            }
            assert_eq!(plain[i], true);
        }

        let all_false: [bool; 15] = [false; 15];
        let enc = encode_context_flags(&all_false);
        // eprintln!("{:?}", enc);
        let plain = decode_context_flags(enc);
        // eprintln!("{:?}", plain);
        for i in 0..15 {
            if CONTEXT_MAP_CYPHER_TO_PLAIN[i] < 0 {
                continue;
            }
            assert_eq!(plain[i], false);
        }

        let alternating: [bool; 15] = [
            true, false, true, false, true, false, true, false, true, false, true, false, true, false, true,
        ];
        let enc = encode_context_flags(&alternating);
        // eprintln!("{:?}", enc);
        let plain = decode_context_flags(enc);
        // eprintln!("{:?}", plain);
        for i in 0..15 {
            if CONTEXT_MAP_CYPHER_TO_PLAIN[i] < 0 {
                continue;
            }
            assert_eq!(plain[i], i % 2 == 0);
        }

        let mod3: [bool; 15] = [
            true, false, false, true, false, false, true, false, false, true, false, false, true, false, false,
        ];
        let enc = encode_context_flags(&mod3);
        let plain = decode_context_flags(enc);
        for i in 0..15 {
            if CONTEXT_MAP_CYPHER_TO_PLAIN[i] < 0 {
                continue;
            }
            assert_eq!(plain[i], i % 3 == 0);
        }

        let mod5: [bool; 15] = [
            true, false, false, false, false, true, false, false, false, false, true, false, false, false, false,
        ];
        let enc = encode_context_flags(&mod5);
        let plain = decode_context_flags(enc);
        for i in 0..15 {
            if CONTEXT_MAP_CYPHER_TO_PLAIN[i] < 0 {
                continue;
            }
            assert_eq!(plain[i], i % 5 == 0);
        }
    }

    #[test]
    fn test_decode() {
        let all_true: u64 = 0x178BA59576325029;
        let plain = decode_context_flags(all_true);
        for i in 0..15 {
            if CONTEXT_COEFFICIENTS[i] < 2 {
                assert_eq!(plain[i], false);
            } else {
                assert_eq!(plain[i], true);
                if plain[i] {
                    // eprintln!("{}: {}", i, CONTEXT_COEFFICIENTS[i]);
                }
            }
        }

        let alternating: u64 = 0x0000000EF0F42FA9;
        let plain = decode_context_flags(alternating);
        for i in 0..15 {
            if CONTEXT_COEFFICIENTS[i] < 2 {
                assert_eq!(plain[i], false);
            } else {
                assert_eq!(plain[i], i % 2 == 0);
                if plain[i] {
                    // eprintln!("{}: {}", i, CONTEXT_COEFFICIENTS[i]);
                }
            }
        }

        let mod3: u64 = 0x00000000005DCC45;
        let plain = decode_context_flags(mod3);
        for i in 0..15 {
            if CONTEXT_COEFFICIENTS[i] < 2 {
                assert_eq!(plain[i], false);
            } else {
                assert_eq!(plain[i], i % 3 == 0);
                if plain[i] {
                    // eprintln!("{}: {}", i, CONTEXT_COEFFICIENTS[i]);
                }
            }
        }

        let mod5: u64 = 0x0000000000000FF1;
        let plain = decode_context_flags(mod5);
        for i in 0..15 {
            if CONTEXT_COEFFICIENTS[i] < 2 {
                assert_eq!(plain[i], false);
            } else {
                assert_eq!(plain[i], i % 5 == 0);
                if plain[i] {
                    // eprintln!("{}: {}", i, CONTEXT_COEFFICIENTS[i]);
                }
            }
        }
    }

    #[test]
    fn test_context_map() {
        for i in 0..15 {
            let p2c = CONTEXT_MAP_PLAIN_TO_CYPHER[i];
            if (p2c) == -1 {
                continue;
            }
            let c2p = CONTEXT_MAP_CYPHER_TO_PLAIN[p2c as usize];
            assert_eq!(c2p, i as i32);
        }
    }

    #[test]
    fn test_context_map2() {
        for i in 0..15 {
            if i == 3 || i == 7 || i == 10 || i == 11 {
                continue;
            }
            assert_eq!(i, context_map_cypher_to_plain(context_map_plain_to_cypher(i)));
        }
    }
}
