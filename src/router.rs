use std::{fmt::Display, io, sync::Arc};

use bitflags::bitflags;
use log::warn;
use serde::{Deserialize, Serialize};
use sqlx::types::JsonValue;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf, WriteHalf},
        TcpStream,
    },
    sync::{Mutex, OnceCell},
};
use tokio_util::sync::CancellationToken;

use crate::{consts::DD2_MAP_UID, player::check_flags_sf_mi};

pub struct Router {
    methods: Vec<Method>,
    db: Database,
}

impl Router {
    fn new(db: Database) -> Self {
        Router { methods: Vec::new(), db }
    }

    fn add_method(mut self, method: Method) -> Self {
        self.methods.push(method);
        self
    }

    async fn run(&self, stream: TcpStream) {}
}

pub struct Database {}

pub struct Method {
    name: String,
    handler: fn(Request) -> Response,
}

pub enum ToPlayerMgr {
    RecheckRecords(),
    AuthFailed(),
    SocketError(),
    NewTop3(),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[repr(u8)]
pub enum Request {
    Authenticate {
        token: String,
        plugin_info: String,
        game_info: String,
        gamer_info: String,
    } = 1,
    ResumeSession {
        session_token: String,
        plugin_info: String,
        game_info: String,
        gamer_info: String,
    } = 2,
    ReportContext {
        sf: String,
        mi: String,
        map: Option<Map>,
        i: Option<bool>,
        bi: [i32; 2],
        e: Option<bool>,
    } = 3,
    ReportGCNodMsg {
        data: String,
    } = 4,

    Ping {} = 8,

    ReportVehicleState {
        pos: [f32; 3],
        rotq: [f32; 4],
        vel: [f32; 3],
    } = 32,
    ReportRespawn {
        race_time: i64,
    } = 33,
    ReportFinish {
        race_time: i32,
    } = 34,
    ReportFallStart {
        floor: u8,
        pos: [f32; 3],
        speed: f32,
        start_time: i32,
    } = 35,
    ReportFallEnd {
        floor: u8,
        pos: [f32; 3],
        end_time: i32,
    } = 36,
    ReportStats {
        stats: Stats,
    } = 37,
    // ReportMapLoad {
    //     uid: String,
    // } = 38,
    ReportPBHeight {
        h: f32,
    } = 39,
    ReportPlayerColor {
        wsid: String,
        color: [f64; 3],
    } = 40,
    ReportTwitch {
        twitch_name: String,
    } = 41,
    DowngradeStats {
        stats: Stats,
    } = 42,

    // arbitrary maps
    ReportMapCurrPos {
        uid: String,
        pos: [f64; 3],
        race_time: Option<i64>,
    } = 64,
    ReportCustomMapAuxSpec {
        id: u32,
        name_id: String,
        spec: serde_json::Value,
    } = 65,
    DeleteCustomMapAuxSpec {
        id: u32,
        name_id: String,
    } = 66,
    ListCustomMapAuxSpecs {
        id: u32,
    } = 67,
    // stats for custom maps
    ReportMapStats {
        uid: String,
        stats: JsonValue,
    } = 68,

    GetMyStats {} = 128,
    GetGlobalLB {
        start: u32,
        end: u32,
    } = 129,
    GetFriendsLB {
        friends: Vec<String>,
    } = 130,
    GetGlobalOverview {} = 131,
    GetServerStats {} = 132,
    GetMyRank {} = 133,
    GetPlayersPb {
        wsid: String,
    } = 134,
    GetDonations {} = 135,
    GetGfmDonations {} = 136,
    GetTwitch {
        wsid: Option<String>,
    } = 137,

    // profile and preferences
    GetMyProfile {} = 138,
    SetMyProfile {
        body: serde_json::Value,
    } = 139,
    GetMyPreferences {} = 140,
    SetMyPreferences {
        body: serde_json::Value,
    } = 141,
    GetUsersProfile {
        wsid: String,
    } = 142,
    GetPlayersSpecInfo {
        uid: String,
        wsid: String,
    } = 143,

    GetMapOverview {
        uid: String,
    } = 192,
    GetMapLB {
        uid: String,
        start: u32,
        end: u32,
    } = 193,
    GetMapLive {
        uid: String,
    } = 194,
    GetMapMyRank {
        uid: String,
    } = 195,
    GetMapRank {
        uid: String,
        wsid: String,
    } = 196,

    GetSecretAssets {} = 254,

    StressMe {} = 255,
}

impl Request {
    pub fn ty(&self) -> u8 {
        match self {
            Request::Authenticate { .. } => 1,
            Request::ResumeSession { .. } => 2,
            Request::ReportContext { .. } => 3,
            Request::ReportGCNodMsg { .. } => 4,
            Request::Ping { .. } => 8,
            Request::ReportVehicleState { .. } => 32,
            Request::ReportRespawn { .. } => 33,
            Request::ReportFinish { .. } => 34,
            Request::ReportFallStart { .. } => 35,
            Request::ReportFallEnd { .. } => 36,
            Request::ReportStats { .. } => 37,
            // Request::ReportMapLoad { .. } => 38,
            Request::ReportPBHeight { .. } => 39,
            Request::ReportPlayerColor { .. } => 40,
            Request::ReportTwitch { .. } => 41,
            Request::DowngradeStats { .. } => 42,
            // arbitrary maps
            Request::ReportMapCurrPos { .. } => 64,
            Request::ReportCustomMapAuxSpec { .. } => 65,
            Request::DeleteCustomMapAuxSpec { .. } => 66,
            Request::ListCustomMapAuxSpecs { .. } => 67,
            Request::ReportMapStats { .. } => 68,
            // get
            Request::GetMyStats { .. } => 128,
            Request::GetGlobalLB { .. } => 129,
            Request::GetFriendsLB { .. } => 130,
            Request::GetGlobalOverview {} => 131,
            Request::GetServerStats {} => 132,
            Request::GetMyRank {} => 133,
            Request::GetPlayersPb { .. } => 134,
            Request::GetDonations { .. } => 135,
            Request::GetGfmDonations { .. } => 136,
            Request::GetTwitch { .. } => 137,
            Request::GetMyProfile { .. } => 138,
            Request::SetMyProfile { .. } => 139,
            Request::GetMyPreferences { .. } => 140,
            Request::SetMyPreferences { .. } => 141,
            Request::GetUsersProfile { .. } => 142,
            Request::GetPlayersSpecInfo { .. } => 143,

            // get arb maps
            Request::GetMapOverview { .. } => 192,
            Request::GetMapLB { .. } => 193,
            Request::GetMapLive { .. } => 194,
            Request::GetMapMyRank { .. } => 195,
            Request::GetMapRank { .. } => 196,
            //
            Request::GetSecretAssets { .. } => 254,
            // debug
            Request::StressMe { .. } => 255,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Request::Authenticate { .. } => "Authenticate",
            Request::ResumeSession { .. } => "ResumeSession",
            Request::ReportContext { .. } => "ReportContext",
            Request::ReportGCNodMsg { .. } => "ReportGameCamNod",
            Request::Ping { .. } => "Ping",
            Request::ReportVehicleState { .. } => "ReportVehicleState",
            Request::ReportRespawn { .. } => "ReportRespawn",
            Request::ReportFinish { .. } => "ReportFinish",
            Request::ReportFallStart { .. } => "ReportFallStart",
            Request::ReportFallEnd { .. } => "ReportFallEnd",
            Request::ReportStats { .. } => "ReportStats",
            // Request::ReportMapLoad { .. } => "ReportMapLoad",
            Request::ReportPBHeight { .. } => "ReportPBHeight",
            Request::ReportPlayerColor { .. } => "ReportPlayerColor",
            Request::ReportTwitch { .. } => "ReportTwitch",
            Request::DowngradeStats { .. } => "DowngradeStats",
            // arbitrary maps
            Request::ReportMapCurrPos { .. } => "ReportMapCurrPos",
            Request::ReportCustomMapAuxSpec { .. } => "ReportCustomMapAuxSpec",
            Request::DeleteCustomMapAuxSpec { .. } => "DeleteCustomMapAuxSpec",
            Request::ListCustomMapAuxSpecs { .. } => "ListCustomMapAuxSpecs",
            Request::ReportMapStats { .. } => "ReportMapStats",
            // get
            Request::GetMyStats { .. } => "GetMyStats",
            Request::GetGlobalLB { .. } => "GetGlobalLB",
            Request::GetFriendsLB { .. } => "GetFriendsLB",
            Request::GetGlobalOverview {} => "GetGlobalOverview",
            Request::GetServerStats {} => "GetServerStats",
            Request::GetMyRank {} => "GetMyRank",
            Request::GetPlayersPb { .. } => "GetPlayersPb",
            Request::GetDonations { .. } => "GetDonations",
            Request::GetGfmDonations {} => "GetGfmDonations",
            Request::GetTwitch { .. } => "GetTwitch",
            Request::GetMyProfile {} => "GetMyProfile",
            Request::SetMyProfile { .. } => "SetMyProfile",
            Request::GetMyPreferences {} => "GetMyPreferences",
            Request::SetMyPreferences { .. } => "SetMyPreferences",
            Request::GetUsersProfile { .. } => "GetUsersProfile",
            Request::GetPlayersSpecInfo { .. } => "GetPlayersSpecInfo",
            // get arb maps
            Request::GetMapOverview { .. } => "GetMapOverview",
            Request::GetMapLB { .. } => "GetMapLB",
            Request::GetMapLive { .. } => "GetMapLive",
            Request::GetMapMyRank { .. } => "GetMapMyRank",
            Request::GetMapRank { .. } => "GetMapRank",

            Request::GetSecretAssets { .. } => "GetSecretAssets",
            // debug
            Request::StressMe { .. } => "StressMe",
        }
    }

    pub async fn read_from_socket(stream: &mut OwnedReadHalf, ct: CancellationToken) -> io::Result<Option<Self>> {
        // await 9 bytes to read
        let ct2 = ct.clone();
        await_bytes(stream, ct2, 9).await?;
        let len = stream.read_u32_le().await?;
        let ty = stream.read_u8().await?;
        let str_len = stream.read_u32_le().await?;
        if len != 5 + str_len as u32 {
            warn!("Invalid request length: {} != {}", len, 5 + str_len);
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Invalid request length"));
        }
        let ct2 = ct.clone();
        await_bytes(stream, ct2, str_len as usize).await?;
        let mut buf: Vec<u8> = vec![0; str_len as usize];
        stream.read_exact(&mut buf).await?;
        let req: Request = match serde_json::from_slice(&buf) {
            Ok(req) => req,
            Err(err) => {
                let s = String::from_utf8_lossy(&buf);
                warn!("Error deserializing request: {:?} for {}", err, s);
                // fail but keep the connection, useful for testing new messages without restarting prod
                return Ok(None);
                // return Err(io::Error::new(io::ErrorKind::InvalidData, "Error deserializing request"));
            }
        };
        if req.ty() != ty {
            warn!("Invalid request type: {} != {}", ty, req.ty());
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Invalid request type"));
        }
        Ok(Some(req))
    }
}

async fn await_bytes(read: &mut OwnedReadHalf, ct: CancellationToken, n_bytes: usize) -> io::Result<()> {
    let mut buf: Vec<u8> = vec![0; n_bytes];
    tokio::select! {
        r = read.readable() => {
            if r.is_err() {
                return Err(r.unwrap_err());
            }
        }
        _ = ct.cancelled() => return Err(io::Error::new(io::ErrorKind::Interrupted, "Cancelled")),
    }
    // loop {
    //     tokio::select! {
    //         _ = ct.cancelled() => return Err(io::Error::new(io::ErrorKind::Interrupted, "Cancelled")),
    //         n = read.peek(&mut buf[..]) => {
    //             let n = n?;
    //             if n < n_bytes {
    //                 warn!("Not enough bytes to read: {} < {}", n, n_bytes);
    //                 tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    //                 continue;
    //             }
    //             break;
    //         }
    //     }
    // }
    // Ok(buf)
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerCtx {
    pub sf: u64,
    pub mi: u64,
    pub map: Option<Map>,
    pub i: bool,
    pub e: Option<bool>,
    #[serde(skip)]
    pub is_official: Arc<std::sync::Mutex<Option<bool>>>,
}

impl PlayerCtx {
    pub fn new(sf: u64, mi: u64, map: Option<Map>, i: bool, e: Option<bool>) -> Self {
        PlayerCtx {
            sf,
            mi,
            map,
            i,
            e,
            is_official: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    pub fn is_official(&self) -> bool {
        let mut io = self.is_official.lock().unwrap();
        if let Some(v) = io.as_ref() {
            return *v;
        }
        let ans = !self.e.unwrap_or(false) && check_flags_sf_mi(self.sf, self.mi) && self.map.as_ref().map(|m| m.is_dd2()).unwrap_or(false);
        *io = Some(ans);
        ans
    }
}

impl Default for PlayerCtx {
    fn default() -> Self {
        PlayerCtx::new(0, 0, None, false, None)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Map {
    pub uid: String,
    pub name: String,
    // either 64 char hex, or raw string, or empty for none
    pub hash: String,
}

impl Map {
    pub fn is_dd2(&self) -> bool {
        self.uid.starts_with(DD2_MAP_UID)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[repr(u8)]
pub enum Response {
    AuthFail {
        err: String,
    },
    AuthSuccess {
        session_token: String,
    },
    ContextAck {
        ctx_session: String,
    },

    Ping {},
    ServerInfo {
        nb_players_live: u32,
    },
    NonFatalErrorMsg {
        // 0 = error, 1 = warning, 2 = info, 3 = success, 4 = debug
        level: u8,
        msg: String,
    },

    NewRecord {
        name: String,
        height: f32,
        ts: u32,
    },

    Stats {
        stats: Stats,
        rank: u32,
    },
    GlobalLB {
        entries: Vec<LeaderboardEntry>,
    },
    FriendsLB {
        entries: Vec<LeaderboardEntry>,
    },
    GlobalOverview {
        j: JsonValue, // players: u32,
                      // sessions: u32,
                      // resets: u32,
                      // jumps: u32,
                      // map_loads: u32,
                      // falls: u32,
                      // floors_fallen: u32,
                      // height_fallen: u32,
    },
    Top3 {
        top3: Vec<LeaderboardEntry>,
    },
    MyRank {
        r: Option<LeaderboardEntry>,
    },
    PlayersPB {
        name: String,
        height: f64,
        rank: i64,
        wsid: String,
        ts: i64,
        update_count: i32,
    },
    Donations {
        donors: Vec<Donor>,
        donations: Vec<Donation>,
    },
    GfmDonations {
        total: f64,
    },
    TwitchName {
        user_id: String,
        twitch_name: String,
    },

    UsersProfile {
        profile: JsonValue,
    },
    YourPreferences {
        preferences: JsonValue,
    },

    // Info given while spectating players only
    PlayersSpecInfo {
        wsid: String,
        uid: String,
        total_map_time: i64,
        now_ts: i64,
    },

    // arbitrary maps
    MapOverview {
        uid: String,
        nb_players_on_lb: u32,
        nb_playing_now: u32,
    },
    MapLB {
        uid: String,
        entries: Vec<LeaderboardEntry2>,
    },
    MapLivePlayers {
        uid: String,
        players: Vec<LeaderboardEntry2>,
    },
    MapRank {
        uid: String,
        r: Option<LeaderboardEntry2>,
    },
    // near end
    TaskResponse {
        id: u32,
        success: bool,
        error: Option<String>,
    },
    TaskResponseJson {
        id: u32,
        data: JsonValue,
    },
    // end
    SecretAssets {
        filenames_and_urls: Vec<AssetRef>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetRef {
    pub name: String,
    pub filename: String,
    pub url: String,
}

impl Response {
    pub fn into(&self) -> u8 {
        match self {
            Response::AuthFail { .. } => 1,
            Response::AuthSuccess { .. } => 2,
            Response::ContextAck { .. } => 3,
            Response::Ping { .. } => 8,
            Response::ServerInfo { .. } => 9,
            Response::NonFatalErrorMsg { .. } => 10,
            Response::NewRecord { .. } => 32,
            Response::Stats { .. } => 128,
            Response::GlobalLB { .. } => 129,
            Response::FriendsLB { .. } => 130,
            Response::GlobalOverview { .. } => 131,
            Response::Top3 { .. } => 132,
            Response::MyRank { .. } => 133,
            Response::PlayersPB { .. } => 134,
            Response::Donations { .. } => 135,
            Response::GfmDonations { .. } => 136,
            Response::TwitchName { .. } => 137,
            Response::UsersProfile { .. } => 138,
            Response::YourPreferences { .. } => 139,
            // space for some preference/profile stuff
            Response::PlayersSpecInfo { .. } => 143,
            // arb maps
            Response::MapOverview { .. } => 192,
            Response::MapLB { .. } => 193,
            Response::MapLivePlayers { .. } => 194,
            Response::MapRank { .. } => 195,

            Response::TaskResponseJson { .. } => 252,
            Response::TaskResponse { .. } => 253,
            Response::SecretAssets { .. } => 254,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Response::AuthFail { .. } => "AuthFail",
            Response::AuthSuccess { .. } => "AuthSuccess",
            Response::ContextAck { .. } => "ContextAck",
            Response::Ping { .. } => "Ping",
            Response::ServerInfo { .. } => "ServerInfo",
            Response::NonFatalErrorMsg { .. } => "NonFatalErrorMsg",
            Response::NewRecord { .. } => "NewRecord",
            Response::Stats { .. } => "Stats",
            Response::GlobalLB { .. } => "GlobalLB",
            Response::FriendsLB { .. } => "FriendsLB",
            Response::GlobalOverview { .. } => "GlobalOverview",
            Response::Top3 { .. } => "Top3",
            Response::MyRank { .. } => "MyRank",
            Response::PlayersPB { .. } => "PlayersPB",
            Response::Donations { .. } => "Donations",
            Response::GfmDonations { .. } => "GfmDonations",
            Response::TwitchName { .. } => "TwitchName",
            Response::UsersProfile { .. } => "UsersProfile",
            Response::YourPreferences { .. } => "YourPreferences",
            Response::PlayersSpecInfo { .. } => "PlayersSpecInfo",
            // arb maps
            Response::MapOverview { .. } => "MapOverview",
            Response::MapLB { .. } => "MapLB",
            Response::MapLivePlayers { .. } => "MapLivePlayers",
            Response::MapRank { .. } => "MapRank",
            //
            Response::TaskResponseJson { .. } => "TaskResponseJson",
            Response::TaskResponse { .. } => "TaskResponse",
            Response::SecretAssets { .. } => "SecretAssets",
        }
    }

    pub async fn write_to_socket(&self, stream: &mut OwnedWriteHalf) -> io::Result<()> {
        let s = serde_json::to_string(self)?;
        let len = s.len() as u32;
        stream.write_u32_le(5 + len).await?;
        stream.write_u8(self.into()).await?;
        stream.write_u32_le(len).await?;
        stream.write_all(s.as_bytes()).await?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    pub nb_players: u32,
    pub nb_sessions: u32,
    pub nb_falls: u32,
    pub nb_floors_fallen: u32,
    pub total_height_fallen: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stats {
    pub seconds_spent_in_map: i64,
    pub nb_jumps: u32,
    pub nb_falls: u32,
    pub nb_floors_fallen: u32,
    pub last_pb_set_ts: u32,
    pub total_dist_fallen: f32,
    pub pb_height: f32,
    pub pb_floor: u32,
    pub nb_resets: u32,
    pub ggs_triggered: u32,
    pub title_gags_triggered: u32,
    pub title_gags_special_triggered: u32,
    pub bye_byes_triggered: u32,
    pub monument_triggers: JsonValue,
    pub reached_floor_count: JsonValue,
    pub floor_voice_lines_played: JsonValue,
    pub extra: Option<JsonValue>,
}

// #[derive(Debug, Clone, Serialize, Deserialize)]
// pub struct Leaderboard {
//     entries: Vec<LeaderboardEntry>,
// }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaderboardEntry {
    pub rank: u32,
    pub wsid: String,
    pub height: f32,
    pub ts: u32,
    pub name: String,
    pub update_count: i32,
    pub color: [f64; 3],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaderboardEntry2 {
    pub rank: u32,
    pub wsid: String,
    pub pos: [f64; 3],
    pub ts: u32,
    pub name: String,
    pub update_count: i32,
    pub color: [f64; 3],
    pub race_time: i64,
}

impl PartialEq for LeaderboardEntry2 {
    fn eq(&self, other: &Self) -> bool {
        // only cheap stuff:
        self.ts == other.ts
            && self.rank == other.rank
            && self.pos == other.pos
            && self.update_count == other.update_count
            && self.race_time == other.race_time
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Donor {
    pub name: String,
    pub amount: f64,
}

impl From<(String, f64)> for Donor {
    fn from((name, amount): (String, f64)) -> Self {
        Donor { name, amount }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Donation {
    pub name: String,
    pub amount: f64,
    pub comment: String,
    pub ts: i64,
}

pub async fn write_response(stream: &mut OwnedWriteHalf, resp: Response) -> io::Result<()> {
    resp.write_to_socket(stream).await
}
