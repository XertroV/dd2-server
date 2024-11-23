use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    thread,
    time::{Duration, Instant, SystemTime},
};

use ahash::random_state::RandomState;
use chrono::{NaiveDate, NaiveDateTime};
use consts::DD2_MAP_UID;
use env_logger::Env;
use itertools::{EitherOrBoth, Itertools};
use log::{error, info, warn};
use player::check_flags_sf_mi;
use queries::{
    adm__get_game_cam_nods, adm__get_user_contexts, adm__get_user_sessions, adm__get_user_vehicle_states, vec3_avg, vec3_len, vec3_sub,
    vec_to_color, SqlResult, UserContext, UserSession, Vec3, VehicleState,
};
use router::ToPlayerMgr;
use serde_json::json;
use sqlx::{postgres::PgPoolOptions, query, Pool, Postgres};
use uuid::Uuid;

mod api_error;
mod consts;
mod db;
mod donations;
mod http;
mod op_auth;
mod player;
mod queries;
mod router;

#[tokio::main(flavor = "multi_thread", worker_threads = 10)]
async fn main() {
    dotenv::from_path(".env-clean").ok();
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();

    let db_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    info!("Connecting to db: {}", db_url);
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(Duration::from_secs(2))
        .connect(&db_url)
        .await
        .unwrap();
    let db = Arc::new(pool);
    info!("Connected to db");

    let start = SystemTime::now();
    match sqlx::migrate!().run(db.as_ref()).await {
        Ok(_) => info!("Migrations ran successfully"),
        Err(e) => panic!("Error running migrations: {:?}", e),
    }
    let end = SystemTime::now();
    info!("Migration time: {:?}", end.duration_since(start).unwrap());

    let pool = db.as_ref();

    if false {
        // let players_to_del = find_players_to_del(pool).await.unwrap();
        // info!("Got {} players to delete", players_to_del.len());
        // if players_to_del.len() > 0 {
        //     clean_away_irrelevant_users(db.as_ref()).await;
        // }
    }

    run_generate_data_for_users(db.as_ref()).await;
    // run_get_unique_scene_flags(db.as_ref()).await;
    // run_get_unique_mgrs(db.as_ref()).await;
    // run_generate_wr_over_time(db.as_ref()).await;
    // run_generate_wr_over_time_from_vehicle_states(db.as_ref()).await;
}

pub async fn run_generate_wr_over_time_from_vehicle_states(pool: &Pool<Postgres>) {
    let wrs = query!(
        r#"
        WITH shadow_banned AS (
            SELECT user_id FROM shadow_bans
        ),
        joined_vs AS (
            SELECT vs.id as vs_id, vs.pos[2] as h, vs.ts, s.user_id, u.display_name as name, c.color FROM vehicle_states vs
            INNER JOIN sessions s ON vs.session_token = s.session_token
            INNER JOIN users u ON s.user_id = u.web_services_user_id
            INNER JOIN colors c ON s.user_id = c.user_id
            WHERE vs.is_official
                AND s.user_id NOT IN (SELECT * FROM shadow_banned)
        ),
        ranked_vs AS (
            SELECT
                vs_id, user_id, ts, h, name, color,
                MAX(h) OVER (ORDER BY ts) AS max_height_so_far
            FROM joined_vs
        ),
        new_wr AS (
            SELECT
                vs_id, user_id, ts, h, name, color,
                max_height_so_far,
                LAG(max_height_so_far, 1, 0) OVER (ORDER BY ts) AS previous_max_height
            FROM ranked_vs
        )
        SELECT
            vs_id, user_id, ts, h, name, color,
            max_height_so_far,
            previous_max_height
        FROM
            new_wr
        WHERE
            h > previous_max_height
        ORDER BY
            ts;
        "#
    )
    .fetch_all(pool)
    .await
    .unwrap();
    info!("Got {} WRs", wrs.len());
    let mut rows = vec![];
    for wr in wrs {
        rows.push(format!(
            "{},{},{},{},{}",
            wr.ts.and_utc().timestamp(),
            wr.h.unwrap_or_default(),
            &wr.user_id.unwrap(),
            wr.name,
            color_to_str(vec_to_color(wr.color).unwrap_or([0.7, 0.5, 0.7]))
        ));
    }

    let data = format!("ts,height,wsid,name,color\n{}", rows.join("\n"));
    write_wr_over_time("wr_over_time_vs.csv", &data);
    info!("Wrote WRs to file");
}

pub async fn run_generate_wr_over_time(pool: &Pool<Postgres>) {
    let lb_entries = query!(
        r#"
        WITH shadow_banned AS (
            SELECT user_id FROM shadow_bans
        )
        SELECT l.*, u.display_name as name, c.color FROM leaderboard_archive l
        LEFT JOIN users u ON l.user_id = u.web_services_user_id
        LEFT JOIN colors c ON l.user_id = c.user_id
        WHERE l.user_id NOT IN (SELECT * FROM shadow_banned)
        ORDER BY ts ASC
        "#
    )
    .fetch_all(pool)
    .await
    .unwrap();

    let mut wr_prog = vec![];
    let mut best_h = 0.0;
    for (ix, entry) in lb_entries.into_iter().enumerate() {
        if entry.height > best_h {
            best_h = entry.height;
            wr_prog.push((entry.ts, entry));
        }
    }

    let data = format!(
        "ts,height,wsid,name,color\n{}",
        wr_prog
            .into_iter()
            .map(|(ts, h)| format!(
                "{},{},{},{},{}",
                ts.and_utc().timestamp(),
                h.height,
                &h.user_id,
                h.name,
                color_to_str(vec_to_color(h.color).unwrap_or([0.7, 0.5, 0.7]))
            ))
            .collect::<Vec<_>>()
            .join("\n")
    );
    write_wr_over_time("wr_over_time.csv", &data);
}

fn write_wr_over_time(file_name: &str, data: &str) {
    let base_path = PathBuf::from("output");
    let file_path = base_path.join(file_name);
    std::fs::create_dir_all(base_path).unwrap();
    std::fs::write(file_path, data).unwrap();
}

fn color_to_str(c: Vec3) -> String {
    format!(
        "#{:02x}{:02x}{:02x}",
        (c[0] * 255.0) as u8,
        (c[1] * 255.0) as u8,
        (c[2] * 255.0) as u8
    )
}

pub async fn run_generate_data_for_users(pool: &Pool<Postgres>) {
    let users = query!(
        r#"
        WITH shadow_banned AS (
            SELECT user_id FROM shadow_bans
        ),
        top_44 AS (
            SELECT user_id FROM ranked_lb_view
            WHERE height > 800.0
              AND user_id NOT IN (SELECT * FROM shadow_banned)
        )
        SELECT web_services_user_id as user_id, display_name as name FROM users
            WHERE web_services_user_id IN (SELECT * FROM top_44)
        "#,
    )
    .fetch_all(pool)
    .await
    .unwrap();

    info!("Got {} users to process", users.len());

    for user in users {
        info!("Generating data for user: {:?}", user.name);
        // let (sessions, contexts) = queries::adm__get_editor_contexts_for(pool, &user.user_id).await.unwrap();
        // info!("Got {} sessions and {} contexts", sessions.len(), contexts.len());
        // !
        // check_user_session_contexts(sessions, contexts, user.name).await;
        // break;
        // thread::sleep(Duration::from_secs(1));

        if false {
            generate_user_timeline_data(pool, &user.user_id, &user.name).await.unwrap();
            info!("Processed user TL data: {}", &user.user_id);
            generate_position_deltas_data(pool, &user.user_id, &user.name).await.unwrap();
            info!("Processed user Pos Delta data: {}", &user.user_id);
            generate_map_data_for_user(pool, &user.user_id, &user.name).await.unwrap();
            info!("Processed user Map data: {}", &user.user_id);
        }
        run_find_user_pings(pool, &user.user_id, &user.name).await.unwrap();
        info!("Processed user map ML pings: {}", &user.name);
        info!("Finished processing user: {} / {}", &user.user_id, &user.name);
    }
}

pub async fn run_find_user_pings(pool: &Pool<Postgres>, user_id: &Uuid, user_name: &str) -> SqlResult<()> {
    let pings = query!(
        r#"
        SELECT DISTINCT p.ip_v4, p.user_agent, p.ts, p.is_intro, s.user_id
        FROM ml_pings p
        LEFT JOIN sessions s ON p.ip_v4 = s.ip_address
        WHERE s.user_id = $1
        ORDER BY p.ts ASC
        "#,
        user_id
    )
    .fetch_all(pool)
    .await
    .unwrap();

    let mut rows = vec![];
    rows.push("ts,ip_v4,user_agent,is_intro".to_string());
    rows.extend(
        pings
            .iter()
            .map(|p| {
                format!(
                    "{},{},{},{}",
                    p.ts.and_utc().timestamp(),
                    p.ip_v4.as_ref().unwrap(),
                    p.user_agent.as_ref().unwrap(),
                    p.is_intro
                )
            })
            .collect::<Vec<_>>(),
    );
    // let pings_data = rows.join("\n");
    // write_user_pings(user_name, pings_data.as_bytes());

    let (_sessions, contexts) = adm__get_user_contexts(pool, user_id).await?;

    let mut ctx_iter = contexts.iter().enumerate();
    let mut ping_to_ctxs = vec![-1; pings.len()];
    let mut last_i: usize = 0;
    let mut last_c: Option<&UserContext> = None;
    for (ix, ping) in pings.iter().enumerate() {
        if let Some(_last_c) = last_c {
            ping_to_ctxs[ix] = last_i as i64;
        }

        while let Some((i, c)) = ctx_iter.next() {
            last_i = i;
            last_c = Some(c);
            if c.created_ts > ping.ts - Duration::from_millis(900) {
                break;
            }
            ping_to_ctxs[ix] = i as i64;
        }
    }

    let mut data = vec![];
    let chunks = ping_to_ctxs
        .iter()
        .enumerate()
        .chunk_by(|(_, i)| **i)
        .into_iter()
        .map(|(ix, g)| (ix, g.collect_vec()))
        .collect_vec();
    for chunk in chunks {
        if chunk.1.len() > 1 {
            let tmp_pings = chunk.1.iter().map(|(i, _)| &pings[*i]).collect_vec();
            warn!("Chunk: {:?} = {:?}", chunk.1.len(), tmp_pings);
        }
    }
    for (p2c_ix, (ping, &c_ix)) in pings.iter().zip(ping_to_ctxs.iter()).enumerate() {
        if c_ix < 0 {
            warn!("[{}] no context found for ping: {:?}", p2c_ix, ping);
            continue;
        }
        let ctxs = contexts[c_ix.max(1) as usize - 1..].iter().take(5).collect_vec();

        let ping_j = json!({
            "ip_v4": ping.ip_v4,
            "user_agent": ping.user_agent,
            "ts": ping.ts.and_utc().timestamp(),
            "is_intro": ping.is_intro,
            "user_id": ping.user_id.as_ref().unwrap().to_string(),
        });
        let ctxs = ctxs
            .iter()
            .map(|&c| {
                json!({
                    "ty": CtxType::from(c).as_int(),
                    "ctx_id": c.context_id.to_string(),
                    "map_name": c.map_name.clone().unwrap_or_else(|| c.map_uid.clone().unwrap_or("no-map".to_string())),
                    "bi_count": c.bi_count,
                    "created_ts": c.created_ts.and_utc().timestamp(),
                    "is_dd2_uid": c.is_dd2_uid(),
                    "flags": c.flags_raw.unwrap_or(0),
                    "managers": c.managers,
                    "editor": c.editor.unwrap_or(false),
                    "has_vl_item": c.has_vl_item,
                })
            })
            .collect::<Vec<_>>();
        data.push(json!({
            "ping": ping_j,
            "ctxs": ctxs
        }));
    }

    let start = Instant::now();
    // let data = serde_json::to_string_pretty(&data).unwrap();
    let data = serde_json::to_string(&data).unwrap();
    let end = Instant::now();
    info!("JSON serialization took: {:?}", end.duration_since(start));
    write_user_pings_json(user_name, data.as_bytes());

    Ok(())
}

fn write_user_pings_json(user_name: &str, data: &[u8]) {
    let base_path = PathBuf::from("output/pings");
    let file_path = base_path.join(format!("{}.csv", clean_username(user_name)));
    std::fs::create_dir_all(base_path).unwrap();
    std::fs::write(file_path, data).unwrap();
    info!("Wrote user pings: {}", user_name);
}

pub async fn generate_map_data_for_user(pool: &Pool<Postgres>, user_id: &Uuid, user_name: &str) -> SqlResult<()> {
    let (sessions, contexts) = adm__get_user_contexts(pool, user_id).await?;

    let s_lookup: HashMap<Uuid, UserSession, std::hash::RandomState> =
        HashMap::from_iter(sessions.into_iter().map(|s| (s.session_token.clone(), s)));

    let mut rows = vec![];
    rows.push("ctx_ix,start_ts,map_name,bi_count,since_init_s,end_ts,duration_s".to_string());

    rows.reserve(contexts.len());

    let mut last_row = None;

    // let mut map_ctxs = vec![];
    for (ix, ctx) in contexts.iter().enumerate() {
        // let is_dd2 = ctx.is_dd2_uid();
        let mut map_name = ctx
            .map_name
            .clone()
            .unwrap_or_else(|| ctx.map_uid.clone().unwrap_or("no-map".to_string()));
        if map_name.starts_with("<!:;") {
            map_name = ctx.map_uid.clone().unwrap_or("not-rel&no-uid".to_string());
        }
        let bi_count = ctx.bi_count;
        let ts: i64 = ctx.created_ts.and_utc().timestamp();
        let session = s_lookup.get(&ctx.session_token).unwrap();
        let since_init_str = session
            .gamer_info
            .split("SinceInit:")
            .nth(1)
            .unwrap_or_default()
            .lines()
            .nth(0)
            .unwrap_or_default();
        let since_init = Duration::from_millis(since_init_str.parse::<u64>().unwrap());

        if let Some((l_ix, l_ts, l_map_name, l_bi_count, l_since_init, l_end_ts, l_secs)) = last_row {
            if l_map_name != map_name || l_bi_count != bi_count {
                let last_secs = ts - l_ts;
                if last_secs < 0 {
                    error!("Negative time diff: {} -> {}", l_ts, ts);
                }
                rows.push(format!(
                    "{},{},{},{},{},{},{}",
                    l_ix, l_ts, l_map_name, l_bi_count, l_since_init, ts, last_secs
                ));
                last_row = Some((ix, ts, map_name, bi_count, since_init.as_secs_f32(), ts + 60, 60));
            } else {
                //
                last_row = Some((l_ix, l_ts, l_map_name, l_bi_count, l_since_init, ts, ts - l_ts));
            }
        } else {
            last_row = Some((ix, ts, map_name, bi_count, since_init.as_secs_f32(), ts + 60, 60));
        }
    }

    let (l_ix, l_ts, l_map_name, l_bi_count, l_since_init, end_ts, dur_s) = last_row.unwrap();
    rows.push(format!(
        "{},{},{},{},{},{},{}",
        l_ix, l_ts, l_map_name, l_bi_count, l_since_init, end_ts, dur_s
    ));

    let data = rows.join("\n");

    write_user_map_session(user_name, data.as_bytes()).await;

    Ok(())
}

async fn write_user_map_session(user_name: &str, data: &[u8]) {
    let base_path = PathBuf::from(format!("output/maps/{}", clean_username(user_name)));
    let file_path = base_path.join(format!("all-maps.csv"));
    std::fs::create_dir_all(base_path).unwrap();
    std::fs::write(file_path, data).unwrap();
}

pub async fn generate_user_timeline_data(pool: &Pool<Postgres>, user_id: &Uuid, user_name: &str) -> SqlResult<()> {
    let (sessions, contexts) = adm__get_user_contexts(pool, user_id).await?;

    for (sess, mut ctxs) in &contexts.into_iter().chunk_by(|c| c.session_token) {
        //
        let ctxs: Vec<_> = ctxs.collect();
        let row_futs = ctxs.iter().zip_longest(&ctxs[1..]).map(|x| match x {
            EitherOrBoth::Right(_) => panic!("impossible"),
            EitherOrBoth::Both(ctx, next_ctx) => gen_ctx_timeline_desc_row(ctx, Some(next_ctx)),
            EitherOrBoth::Left(ctx) => gen_ctx_timeline_desc_row(ctx, None),
        });
        let mut rows = vec![];
        for row in row_futs {
            rows.push(row.await);
        }

        let data = vec![CtxTlRow::headers().iter().join(", ")]
            .into_iter()
            .chain(rows.iter().map(|c| c.to_row_string()))
            .join("\n");

        write_user_timeline_session(user_name, &sess, data.as_bytes()).await;

        info!("Processed user session ctxs: {} / {} / {}", &user_id, &sess, rows.len());
    }

    Ok(())
}

#[derive(Debug)]
enum CtxType {
    MainMenu,
    EditorMap,
    EditorMT,
    EditorDriving,
    EditorUnk,
    MapDriving,
    MapMenu,
    MapUnk,
    Unk,
}

impl CtxType {
    pub fn as_int(&self) -> i32 {
        match self {
            CtxType::MainMenu => 0,
            CtxType::MapDriving => 2,
            CtxType::MapMenu => 3,
            CtxType::MapUnk => 5,
            CtxType::Unk => 7,
            CtxType::EditorUnk => 9,
            CtxType::EditorMap => 10,
            CtxType::EditorMT => 11,
            CtxType::EditorDriving => 12,
        }
    }
}

/// Context timeline row
struct CtxTlRow {
    start_ts: i64,
    end_ts: Option<i64>,
    ty: CtxType,
    map_id: i32,
    is_dd2_uid: bool,
}

impl CtxTlRow {
    pub fn headers() -> Vec<String> {
        vec!["start_ts", "end_ts", "ty", "map_id", "is_dd2_uid"]
            .into_iter()
            .map(|s| s.to_string())
            .collect_vec()
    }

    pub fn to_row_string(&self) -> String {
        vec![
            self.start_ts.to_string(),
            self.end_ts.map(|d| d.to_string()).unwrap_or(String::new()),
            format!("{}", self.ty.as_int()),
            format!("{}", self.map_id),
            format!("{}", self.is_dd2_uid as usize),
        ]
        .into_iter()
        .join(", ")
    }
}

// 0x05FC777FF1F6BF6B: on server, in MT driving, in solo
const MGRS_IN_MAP: i64 = 431351055724756843;
const MGRS_IN_MAP_UNK: i64 = 431351055724756971;
// map or MT editor
const MGRS_IN_MAP_EDITOR: i64 = 305250248978513771;
const MGRS_LOADING: i64 = 305250248978513899;

impl From<&UserContext> for CtxType {
    fn from(c: &UserContext) -> Self {
        let editor_flagged = c.editor.unwrap_or(false);
        let mgrs_map_editor = c.managers == MGRS_IN_MAP_EDITOR;
        let mgrs_map_pg = c.managers == MGRS_IN_MAP;
        let mgrs_loading = c.managers == MGRS_LOADING;
        let mgrs_map_unk = c.managers == MGRS_IN_MAP_UNK;
        if c.managers > 0 && !mgrs_map_editor && !mgrs_map_pg && !mgrs_loading && !mgrs_map_unk {
            warn!("Unknown managers: {}", c.managers);
        }
        let some_menu_open_mb = c.flags[10];
        c.has_vl_item;
        c.bi_count;

        let some_editor = c.flags[5] || c.flags[7] || c.flags[9] || c.flags[11];
        let pg_or_driving = c.flags[2] || c.flags[3] || c.flags[4];

        let is_dd2_uid = c.is_dd2_uid();

        if editor_flagged || some_editor {
            if some_menu_open_mb {
                return CtxType::EditorUnk;
            }
            if mgrs_map_pg || mgrs_map_unk || pg_or_driving {
                return CtxType::EditorDriving;
            }
            if mgrs_map_editor {
                return CtxType::EditorUnk;
            }
            return CtxType::EditorUnk;
        }

        if mgrs_map_pg || mgrs_map_unk {
            if some_menu_open_mb {
                return CtxType::MapMenu;
            }
            if c.flags[0] {
                return CtxType::MapDriving;
            }
            return CtxType::MapUnk;
        }

        CtxType::Unk
    }
}

async fn gen_ctx_timeline_desc_row(ctx: &UserContext, next_ctx: Option<&UserContext>) -> CtxTlRow {
    // let gc_nods = adm__get_game_cam_nods(pool, &ctx.context_id).await?;
    let start_ts = ctx.created_ts.and_utc().timestamp();
    let end_ts = next_ctx.map(|c| c.created_ts.and_utc().timestamp());
    let ty = CtxType::from(ctx);
    let map_id = ctx.map_id.unwrap_or(-1);
    let is_dd2_uid = ctx.is_dd2_uid() || ctx.has_vl_item;

    CtxTlRow {
        start_ts,
        end_ts,
        ty,
        map_id,
        is_dd2_uid,
    }
}

async fn write_user_timeline_session(user_name: &str, session_id: &Uuid, data: &[u8]) {
    let base_path = PathBuf::from(format!("output/timeline/{}", clean_username(user_name)));
    let file_path = base_path.join(format!("{}.csv", session_id));
    std::fs::create_dir_all(base_path).unwrap();
    std::fs::write(file_path, data).unwrap();
}

pub struct VehicleStateDelta {
    dt: f64,
    ts: NaiveDateTime,
    pub dist: Vec3,
    pub vel: Vec3,
    pub reported_vel: (Vec3, Vec3),
    pub reported_pos: (Vec3, Vec3),
}

impl From<(&VehicleState, Option<&VehicleState>)> for VehicleStateDelta {
    fn from(v: (&VehicleState, Option<&VehicleState>)) -> Self {
        let v1 = v.1.cloned().unwrap_or_else(|| VehicleState {
            st: v.0.st,
            ctx: v.0.ctx,
            is_official: v.0.is_official,
            pos: [0.0, 0.0, 0.0],
            vel: [0.0, 0.0, 0.0],
            rotq: [0.0, 0.0, 0.0, 1.0],
            ts: v.0.ts - Duration::from_secs(1),
        });
        VehicleStateDelta {
            dt: ((v.0.ts - v1.ts).num_milliseconds() as f64) / 1000.0,
            ts: v.0.ts,
            dist: vec3_sub(v.0.pos, v1.pos),
            reported_vel: (v1.vel, v.0.vel),
            vel: vec3_avg(v1.vel, v.0.vel),
            reported_pos: (v1.pos, v.0.pos),
        }
    }
}

impl VehicleStateDelta {
    pub fn to_row_string(&self) -> String {
        format!(
            "{},{},{},{},{},{}",
            self.ts.and_utc().timestamp(),
            self.dt,
            vec3_psv(self.dist),
            vec3_len(self.dist),
            vec3_psv(self.reported_pos.0),
            vec3_psv(self.reported_pos.1),
        )
    }

    pub fn csv_headers() -> String {
        "ts,dt,dist3,dist,pos0,pos1".into()
    }
}

pub fn vec3_psv(v: Vec3) -> String {
    format!("[{}|{}|{}]", v[0], v[1], v[2])
}

pub async fn generate_position_deltas_data(pool: &Pool<Postgres>, user_id: &Uuid, user_name: &str) -> SqlResult<()> {
    let (sessions, contexts) = adm__get_user_contexts(pool, user_id).await?;
    let vehicle_states = adm__get_user_vehicle_states(pool, user_id).await?;

    let mut vs = vehicle_states
        .into_iter()
        .sorted_by(|v1, v2| v1.ts.cmp(&v2.ts))
        // .chunk_by(|vs| vs.ts.and_utc().timestamp() / (86400 / 4/* 6 hrs */));
        .collect_vec();

    let mut deltas: Vec<VehicleStateDelta> = vec![];
    {
        let pairs = vs.iter().zip(vec![None].into_iter().chain(vs.iter().map(Some)));
        for pair in pairs {
            deltas.push(pair.into());
        }
    }

    info!("Processed user vehicle state deltas: {} / {}", &user_id, deltas.len());

    let mut rows = vec![VehicleStateDelta::csv_headers()];
    rows.extend(deltas.into_iter().map(|d| d.to_row_string()));
    let data = rows.join("\n");
    write_user_pos_deltas_session(user_name, data.as_bytes());

    Ok(())
}

fn write_user_pos_deltas_session(user_name: &str, data: &[u8]) {
    let base_path = PathBuf::from(format!("output/pos-deltas/{}", clean_username(user_name)));
    let file_path = base_path.join(format!("all-vehicle-states.csv"));
    std::fs::create_dir_all(base_path).unwrap();
    std::fs::write(file_path, data).unwrap();
}

pub async fn check_user_session_contexts(sessions: Vec<UserSession>, contexts: Vec<UserContext>, user_name: String) {
    let mut editor_count = 0;
    let mut editor_ctxs = vec![];
    for ctx in contexts.iter() {
        if ctx.editor.unwrap_or(false) {
            editor_count += 1;
            editor_ctxs.push(ctx.clone());
        }
        if ctx.has_vl_item && !check_flags_sf_mi(ctx.flags_raw.unwrap_or(0) as u64, ctx.managers as u64) {
            warn!(
                "Context {:?} has vl item check flags = false; map; {:?}",
                ctx.context_id, ctx.map_name
            );
        }
    }
    if editor_count > 0 {
        warn!("User has {} editor contexts", editor_count);
        for ctx in editor_ctxs.iter().take(32) {
            warn!("[{}] Editor context: {:?}", user_name, ctx);
        }
    }
}

pub async fn generate_data_for_user(pool: &Pool<Postgres>, user_id: &Uuid) {}

async fn clean_away_irrelevant_users(pool: &Pool<Postgres>) {
    let players_to_del = find_players_to_del(pool).await.unwrap();
    info!("Got {} players to delete", players_to_del.len());

    let nb_deletions = 99999.min(players_to_del.len()) / 200;

    // if let Ok(tx) = db.begin().await {
    for (i, users) in players_to_del.iter().as_slice().chunks(200).enumerate() {
        // break;
        let nb_users = users.len();
        warn!("Deleting users ({})", nb_users);
        // delete_user_contexts(&db, user_id).await;
        query!(
            r#"
                DELETE FROM users WHERE web_services_user_id = ANY($1)
            "#,
            &users.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>()
        )
        .execute(pool)
        .await
        .unwrap();
        warn!(
            "Deleted user ({}) -- {:.2}%",
            nb_users,
            ((i + 1) as f32 / nb_deletions as f32) * 100.0
        );
    }
}

async fn delete_user_contexts(pool: &Pool<Postgres>, user_id: &Uuid) {
    let sessions = query!("SELECT session_token FROM sessions WHERE user_id = $1", user_id)
        .fetch_all(pool)
        .await
        .unwrap();

    let nb_sessions = sessions.len();
    // info!("Deleting {:?} sessions (user: {:?})", nb_sessions, user_id);

    // // panic!("Sessions: {:?}, {}", sessions[0], sessions.len());
    // for sess in sessions {
    //     let contexts = query!("SELECT * FROM contexts WHERE session_token = $1", sess.session_token)
    //         .fetch_all(pool)
    //         .await
    //         .unwrap();
    //     let count = contexts.len();
    //     if count > 0 {
    //         info!("Deleting {:?} contexts (session: {:?})", contexts.len(), sess.session_token);
    //     }

    //     // if count > 0 {
    //     //     panic!("Deleting {:?} contexts (session: {:?})", count, sess.session_token);
    //     // }

    //     let nb_falls = query!(r#"SELECT COUNT(*) FROM falls WHERE session_token = $1"#, sess.session_token)
    //         .fetch_one(pool)
    //         .await
    //         .unwrap()
    //         .count
    //         .unwrap();
    //     if nb_falls > 0 && false {
    //         info!("Deleting {:?} falls (session: {:?})", nb_falls, sess.session_token);
    //         query!(r#"DELETE FROM falls WHERE session_token = $1"#, sess.session_token)
    //             .execute(pool)
    //             .await
    //             .unwrap();
    //     }

    //     let nb_respawns = query!(r#"SELECT COUNT(*) FROM respawns WHERE session_token = $1"#, sess.session_token)
    //         .fetch_one(pool)
    //         .await
    //         .unwrap()
    //         .count
    //         .unwrap();

    //     if nb_respawns > 0 && false {
    //         info!("Deleting {:?} respawns (session: {:?})", nb_respawns, sess.session_token);
    //         query!(r#"DELETE FROM respawns WHERE session_token = $1"#, sess.session_token)
    //             .execute(pool)
    //             .await
    //             .unwrap();
    //     }

    //     for ctx in contexts {
    //         query!(r#"DELETE FROM vehicle_states WHERE context_id = $1;"#, ctx.context_id)
    //             .execute(pool)
    //             .await
    //             .unwrap();
    //         debug!("Deleted vehicle states for context {:?}", ctx.context_id);
    //         query!(r#"DELETE FROM game_cam_nods WHERE context_id = $1;"#, ctx.context_id)
    //             .execute(pool)
    //             .await
    //             .unwrap();
    //         debug!("Deleted game cam nods for context {:?}", ctx.context_id);
    //     }

    //     if count > 0 {
    //         let _res = query!(
    //             r#"
    //             DELETE FROM contexts c
    //             WHERE c.session_token = $1
    //             "#,
    //             sess.session_token
    //         )
    //         .execute(pool)
    //         .await
    //         .unwrap();
    //         info!("Deleted {:?} contexts (session: {:?})", count, sess.session_token);
    //         // panic!("Deleted {:?} contexts (session: {:?})", count, sess.session_token);
    //     }
    // }

    info!("Deleting {:?} sessions (user: {:?})", nb_sessions, user_id);

    query!(r#"DELETE FROM sessions WHERE user_id = $1"#, user_id)
        .execute(pool)
        .await
        .unwrap();

    info!("Deleted {:?} sessions (user: {:?})", nb_sessions, user_id);
}

async fn find_players_to_del(pool: &Pool<Postgres>) -> Result<Vec<(Uuid, String)>, sqlx::Error> {
    let rows = query!(
        r#"
        SELECT user_id as "user_id?", display_name as "name?", rank, height FROM ranked_lb_view WHERE height > 800.0
        "#,
    )
    .fetch_all(pool)
    .await?;

    let keep_users: HashSet<_> = rows.iter().map(|r| (r.user_id.unwrap())).collect();

    let all_users = query!(
        r#"
        SELECT web_services_user_id as user_id, display_name as name FROM users
        "#,
    )
    .fetch_all(pool)
    .await?;

    let all_users_len = all_users.len();

    let mut ret = vec![];
    for user in all_users.into_iter() {
        if !keep_users.contains(&user.user_id) {
            ret.push((user.user_id, user.name));
        }
    }

    info!("Kept {} users", keep_users.len());
    info!("Deleting {} users", ret.len());
    info!("Total users: {}", all_users_len);

    Ok(ret)
}

pub async fn run_get_unique_scene_flags(pool: &Pool<Postgres>) {
    let r = query!(
        r#"--sql
        SELECT flags, flags_raw, COUNT(*) as count FROM contexts GROUP BY (flags, flags_raw)
    "#
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let mut rows: Vec<_> = vec!["flags_raw, count, flags_bits".to_string()];
    rows.extend(r.into_iter().map(|r| (r.flags, r.flags_raw, r.count)).map(|(f, fr, c)| {
        format!(
            "{},{},{}",
            fr.unwrap_or(-111),
            c.unwrap(),
            f.into_iter().map(|f| format!("{}", f as usize)).join("")
        )
    }));
    write_uniq_summary(&rows, "scene_flags").await;
    info!("got unique scene flags");
}

pub async fn run_get_unique_mgrs(pool: &Pool<Postgres>) {
    let r = query!(
        r#"--sql
        SELECT managers, COUNT(*) as count FROM contexts GROUP BY managers ORDER BY count DESC
    "#
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let mut rows: Vec<_> = vec!["managers, count".to_string()];
    rows.extend(
        r.into_iter()
            .map(|r| (r.managers, r.count))
            .map(|(m, c)| format!("{},{}", m, c.unwrap())),
    );
    write_uniq_summary(&rows, "managers").await;
    info!("got managers summary");
}

async fn write_uniq_summary(data: &[String], summary_name: &str) {
    let base_path = PathBuf::from("output/");
    let file_path = base_path.join(format!("{}.csv", summary_name));
    std::fs::create_dir_all(base_path).unwrap();
    std::fs::write(file_path, data.join("\n")).unwrap();
}

pub fn clean_username(name: &str) -> String {
    let mut name: String = name.into();
    while name.ends_with(|c| "._".contains(c)) {
        name.pop();
    }
    name
}
