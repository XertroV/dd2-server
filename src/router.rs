use std::{fmt::Display, io};

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
};

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
        context: PlayerCtx,
    } = 3,
    ReportGameCamNod {
        // data: [u8; 0x2d0],
        aux: AuxGCNod,
    } = 4,

    Ping {} = 8,

    ReportVehicleState {
        mat: [[f32; 3]; 4],
        vel: [f32; 3],
    } = 32,
    ReportRespawn {
        game_time: u32,
    } = 33,
    ReportFinish {
        game_time: u32,
    } = 34,
    ReportFallStart {
        floor: u8,
        pos: [f32; 3],
        speed: f32,
        start_time: u32,
    } = 35,
    ReportFallEnd {
        floor: u8,
        pos: [f32; 3],
        end_time: u32,
    } = 36,
    ReportStats {
        stats: Stats,
    } = 37,
    ReportMapLoad {} = 38,

    GetMyStats {} = 128,
    GetGlobalLB {} = 129,
    GetFriendsLB {
        friends: Vec<String>,
    } = 130,

    StressMe {} = 255,
}

impl Request {
    pub fn ty(&self) -> u8 {
        match self {
            Request::Authenticate { .. } => 1,
            Request::ResumeSession { .. } => 2,
            Request::ReportContext { .. } => 3,
            Request::ReportGameCamNod { .. } => 4,
            Request::Ping { .. } => 8,
            Request::ReportVehicleState { .. } => 32,
            Request::ReportRespawn { .. } => 33,
            Request::ReportFinish { .. } => 34,
            Request::ReportFallStart { .. } => 35,
            Request::ReportFallEnd { .. } => 36,
            Request::ReportStats { .. } => 37,
            Request::ReportMapLoad { .. } => 38,
            Request::GetMyStats { .. } => 128,
            Request::GetGlobalLB { .. } => 129,
            Request::GetFriendsLB { .. } => 130,
            Request::StressMe { .. } => 255,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Request::Authenticate { .. } => "Authenticate",
            Request::ResumeSession { .. } => "ResumeSession",
            Request::ReportContext { .. } => "ReportContext",
            Request::ReportGameCamNod { .. } => "ReportGameCamNod",
            Request::Ping { .. } => "Ping",
            Request::ReportVehicleState { .. } => "ReportVehicleState",
            Request::ReportRespawn { .. } => "ReportRespawn",
            Request::ReportFinish { .. } => "ReportFinish",
            Request::ReportFallStart { .. } => "ReportFallStart",
            Request::ReportFallEnd { .. } => "ReportFallEnd",
            Request::ReportStats { .. } => "ReportStats",
            Request::ReportMapLoad { .. } => "ReportMapLoad",
            Request::GetMyStats { .. } => "GetMyStats",
            Request::GetGlobalLB { .. } => "GetGlobalLB",
            Request::GetFriendsLB { .. } => "GetFriendsLB",
            Request::StressMe { .. } => "StressMe",
        }
    }

    pub async fn read_from_socket(stream: &mut OwnedReadHalf) -> io::Result<Self> {
        let len = stream.read_u32_le().await?;
        let ty = stream.read_u8().await?;
        let str_len = stream.read_u32_le().await?;
        if len != 5 + str_len as u32 {
            warn!("Invalid request length: {} != {}", len, 5 + str_len);
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Invalid request length"));
        }
        let mut buf: Vec<u8> = vec![0; str_len as usize];
        stream.read_exact(&mut buf).await?;
        let req: Request = match serde_json::from_slice(&buf) {
            Ok(req) => req,
            Err(err) => {
                let s = String::from_utf8_lossy(&buf);
                warn!("Error deserializing request: {:?} for {}", err, s);
                return Err(io::Error::new(io::ErrorKind::InvalidData, "Error deserializing request"));
            }
        };
        if req.ty() != ty {
            warn!("Invalid request type: {} != {}", ty, req.ty());
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Invalid request type"));
        }
        Ok(req)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuxGCNod {}

bitflags! {
    // todo: choose good coprimes
    // context is a big mix of many redundant variables. we can always divide by 2 if possible. Can obfuscate by multiplying numbers and shifting right.
    // also, send entire game camera nod every once in a while.
    #[derive(Debug, Clone)]
    pub struct CtxFlags: u64 {
        const IN_MAP       = 3;
        const IN_EDITOR    = 5;
        const IN_MT_EDITOR = 7;
        const IN_DD2_MAP   = 11;
        const IN_DD2_LIKE  = 13;
        const NOT_DD2      = 17;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerCtx {
    pub flags: u64,
    pub managers: i64,
    pub map_uid: String,
    pub map_name: String,
    pub map_hash: [u8; 32],
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
        nb_players_total: u32,
        nb_sessions: u32,
        nb_resets: u32,
        nb_jumps: u32,
        nb_map_loads: u32,
        nb_falls: u32,
        nb_floors_fallen: u32,
        total_height_fallen: u32,
    },

    NewRecord {
        name: String,
        height: f32,
        ts: u32,
    },

    Stats {
        stats: Stats,
    },
    GlobalLB {
        entries: Vec<LeaderboardEntry>,
    },
    FriendsLB {
        entries: Vec<LeaderboardEntry>,
    },
}

impl Response {
    pub fn into(&self) -> u8 {
        match self {
            Response::AuthFail { .. } => 1,
            Response::AuthSuccess { .. } => 2,
            Response::ContextAck { .. } => 3,
            Response::Ping { .. } => 8,
            Response::ServerInfo { .. } => 9,
            Response::NewRecord { .. } => 32,
            Response::Stats { .. } => 128,
            Response::GlobalLB { .. } => 129,
            Response::FriendsLB { .. } => 130,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Response::AuthFail { .. } => "AuthFail",
            Response::AuthSuccess { .. } => "AuthSuccess",
            Response::ContextAck { .. } => "ContextAck",
            Response::Ping { .. } => "Ping",
            Response::ServerInfo { .. } => "ServerInfo",
            Response::NewRecord { .. } => "NewRecord",
            Response::Stats { .. } => "Stats",
            Response::GlobalLB { .. } => "GlobalLB",
            Response::FriendsLB { .. } => "FriendsLB",
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
}

// #[derive(Debug, Clone, Serialize, Deserialize)]
// pub struct Leaderboard {
//     entries: Vec<LeaderboardEntry>,
// }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaderboardEntry {
    rank: u32,
    name: String,
    wsid: String,
    height: f32,
    ts: u32,
}

pub async fn write_response(stream: &mut OwnedWriteHalf, resp: Response) -> io::Result<()> {
    resp.write_to_socket(stream).await
}
