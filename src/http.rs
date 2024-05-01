use std::{
    net::{IpAddr, SocketAddr},
    str::FromStr,
    sync::Arc,
};

use base64::prelude::*;
use lets_encrypt_warp::lets_encrypt;
use log::{info, warn};
use sqlx::{Pool, Postgres};
use warp::{
    reply::{Reply, WithStatus},
    Filter,
};

use crate::queries::log_ml_ping;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn with_db(db: Arc<Pool<Postgres>>) -> impl Filter<Extract = (Arc<Pool<Postgres>>,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || db.clone())
}

pub async fn run_http_server(pool: Arc<Pool<Postgres>>) -> Result<(), Box<dyn std::error::Error>> {
    // only for dev mode
    let ip_addr = IpAddr::from_str("0.0.0.0")?;
    let soc_addr: SocketAddr = SocketAddr::new(ip_addr, 8077);

    let version_route = warp::path!("version").and(warp::path::end()).map(|| VERSION);

    let pool_ping = pool.clone();
    let ping_pool2 = pool.clone();

    info!("Enabling route: get /mlping");
    let ping_path = warp::path!("mlping.Script.txt")
        .and(warp::path::end())
        .and(warp::addr::remote())
        .and(warp::header::<String>("User-Agent"))
        .map(move |addr, hdr| (addr, hdr, pool_ping.clone()))
        .and_then(|(ip, hdr, pool): (Option<SocketAddr>, String, Arc<Pool<Postgres>>)| async move {
            handle_mlping(&pool, &ip, false, &hdr).await
        });

    let intro_ping_path = warp::path!("mlping_intro.Script.txt")
        .and(warp::path::end())
        .and(warp::addr::remote())
        .and(warp::header::<String>("User-Agent"))
        .map(move |addr, hdr| (addr, hdr, ping_pool2.clone()))
        .and_then(|(ip, hdr, pool): (Option<SocketAddr>, String, Arc<Pool<Postgres>>)| async move {
            handle_mlping(&pool, &ip, true, &hdr).await
        });

    // info!("Enabling route: get /mlping_intro");
    // let ping_path = warp::path!("mlping_intro")
    //     .and(warp::path::end())
    //     // .and(with_db(pool.clone()))
    //     .and(warp::addr::remote())
    //     .map(move |addr| (addr, pool_ping.clone()))
    //     .and_then(|(ip, pool): (Option<SocketAddr>, Arc<Pool<Postgres>>)| async move { handle_mlping(&pool, &ip).await });

    // let lm_analysis_local_route = warp::post()
    //     .and(warp::path!("e++" / "lm-analysis" / "convert" / "webp" / "local"))
    //     .and(warp::path::end())
    //     .and(warp::body::bytes())
    //     .and_then(move |b| async move {
    //         Ok(warp::reply::with_status(
    //             Into::<String>::into("Local conversion is disabled"),
    //             warp::http::StatusCode::FORBIDDEN,
    //         ))
    //     });

    let site = ping_path
        .or(intro_ping_path)
        .or(version_route)
        // .or(lm_analysis_local_route)
        // .or(version_route)
        .with(warp::log("dips++"));

    #[cfg(debug_assertions)]
    warp::serve(site).run(soc_addr).await;
    #[cfg(not(debug_assertions))]
    lets_encrypt(site, "dipspp.letsencrypt@xk.io", "dips-plus-plus-server.xk.io").await;

    info!("Server shutting down.");

    Ok(())
}

pub async fn handle_mlping(
    pool: &Arc<Pool<Postgres>>,
    ip: &Option<SocketAddr>,
    is_intro: bool,
    user_agent: &str,
) -> Result<impl Reply, warp::Rejection> {
    info!("Got ping from: {:?}", ip);
    if let Some(ip) = ip {
        let (v4, v6) = match ip.is_ipv4() {
            true => (ip.ip().to_string(), "".to_string()),
            false => ("".to_string(), ip.ip().to_string()),
        };
        match log_ml_ping(&pool, &v4, &v6, is_intro, user_agent).await {
            Ok(_) => {}
            Err(e) => {
                warn!("Error logging ml ping: {:?}", e);
                return Ok(warp::reply::with_status(
                    "".to_string(),
                    warp::http::StatusCode::INTERNAL_SERVER_ERROR,
                ));
            }
        }
    }
    return Ok(warp::reply::with_status("".to_string(), warp::http::StatusCode::OK));
}
