use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    str::FromStr,
    sync::Arc,
};

#[cfg(not(debug_assertions))]
use lets_encrypt_warp::lets_encrypt;
use log::{info, warn};
use rustls_acme::{caches::DirCache, AcmeConfig};
use sqlx::{Pool, Postgres};
use tokio_graceful_shutdown::SubsystemHandle;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::sync::CancellationToken;
use warp::{
    http::Response,
    reject,
    reply::{Reply, WithStatus},
    Filter, Rejection,
};

use crate::queries::{api, log_ml_ping};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn with_db(db: Arc<Pool<Postgres>>) -> impl Filter<Extract = (Arc<Pool<Postgres>>,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || db.clone())
}

// fn domain_filter<T>(allowed_domain: &'static str) -> impl Filter<Extract = (Response<T>,), Error = Rejection> + Clone {
//     warp::header::<String>("host")
//         .and_then(move |host: String| {
//             let allowed_domain = allowed_domain.to_string();
//             async move {
//                 if host.starts_with(&allowed_domain) {
//                     Ok(())
//                 } else {
//                     Err(warp::reject::custom(DomainMismatch))
//                 }
//             }
//         })
//         .untuple_one()
//         .or(warp::any().map(warp::reply).map(|x| {
//             Response::builder()
//                 .status(301)
//                 .header("Location", "http://example.com")
//                 .body("Redirecting to the correct domain".into())
//                 .unwrap()
//         }))
//         .unify()
// }
fn domain_filter(allowed_domain: &'static str) -> impl Filter<Extract = (impl Reply,), Error = Rejection> + Clone {
    warp::header::<String>("host")
        .and_then(move |host: String| {
            let allowed_domain = allowed_domain.to_string();
            async move {
                if host.starts_with(&allowed_domain) {
                    Ok::<_, Rejection>(())
                } else {
                    Err(reject::custom(DomainMismatch))
                }
            }
        })
        .map(|_| warp::reply::with_status("Correct domain", warp::http::StatusCode::OK))
        .recover(|_| async {
            Ok::<_, Rejection>(
                warp::reply::with_status(
                    warp::reply::html("Redirecting to correct domain"),
                    warp::http::StatusCode::MOVED_PERMANENTLY,
                )
                .into_response(),
            )
        })
}

#[derive(Debug)]
struct DomainMismatch;
impl warp::reject::Reject for DomainMismatch {}

// pub async fn run_http_server_subsystem(pool: Arc<Pool<Postgres>>, subsys: SubsystemHandle) -> miette::Result<()> {
//     let cancel_t = subsys.create_cancellation_token();
//     tokio::select! {
//         _ = run_http_server(pool, "dips-plus-plus-server.xk.io".to_string(), Some(cancel_t.clone())) => {
//             warn!("HTTP server ended early");
//         }
//         _ = cancel_t.cancelled() => {
//             warn!("HTTP Server shutting down due to subsystem cancellation.");
//         },
//     };
//     Ok(())
// }

pub async fn run_http_server(
    pool: Arc<Pool<Postgres>>,
    lets_enc_domain: String,
    cancel_t: Option<CancellationToken>,
    add_extra_domains: bool,
) {
    #[cfg(debug_assertions)]
    let dev_mode = true;
    #[cfg(not(debug_assertions))]
    let dev_mode = false;

    // only for dev mode
    let ip_addr = IpAddr::from_str("0.0.0.0").expect("ip to be sane");
    let soc_addr = match dev_mode {
        true => SocketAddr::new(ip_addr, 8077),
        false => SocketAddr::new(ip_addr, 443),
    };

    let version_route = warp::path!("version").and(warp::path::end()).map(|| VERSION);

    let pool_ping = pool.clone();
    let ping_pool2 = pool.clone();

    let api_routes = warp::path!("api" / "routes").and(warp::path::end()).map(|| {
        let routes = vec![
            "/leaderboard/global",
            "/leaderboard/global/len",
            "/leaderboard/global/<page>",
            "/leaderboard/<wsid>",
            "/live_heights/global",
            "/live_heights/<wsid> (prefer global)",
            "/overview",
            "/server_info",
            "/donations",
            "/twitch/list",
            "/aux_spec/<wsid>/<name_id>.json",
            "/map/<uid>/nb_active_climbers",
            "/map/<uid>/leaderboard",
            "/map/<uid>/leaderboard/len",
            "/map/<uid>/leaderboard/<page>",
            "/map/<uid>/live_heights",
        ];
        "Routes: ".to_owned() + &routes.join(", ")
    });

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
    let req_pool = pool.clone();

    let get_global_lb = warp::path!("leaderboard" / "global")
        .and(warp::path::end())
        .map(move || req_pool.clone())
        .and_then(|pool: Arc<Pool<Postgres>>| async move { api::handle_get_global_lb(&pool, 0).await });
    let req_pool = pool.clone();

    let get_global_lb_len = warp::path!("leaderboard" / "global" / "len")
        .and(warp::path::end())
        .map(move || req_pool.clone())
        .and_then(|pool: Arc<Pool<Postgres>>| async move { api::handle_get_global_lb_len(&pool).await });
    let req_pool = pool.clone();

    let get_global_lb_page = warp::path!("leaderboard" / "global" / u32)
        .and(warp::path::end())
        .map(move |op| (op, req_pool.clone()))
        .and_then(|(op, pool): (u32, Arc<Pool<Postgres>>)| async move { api::handle_get_global_lb(&pool, op).await });
    let req_pool = pool.clone();

    let get_global_overview = warp::path!("overview")
        .and(warp::path::end())
        .map(move || req_pool.clone())
        .and_then(|pool: Arc<Pool<Postgres>>| async move { api::handle_get_global_overview(&pool).await });
    let req_pool = pool.clone();

    let get_server_info = warp::path!("server_info")
        .and(warp::path::end())
        .map(move || req_pool.clone())
        .and_then(|pool: Arc<Pool<Postgres>>| async move { api::handle_get_server_info(&pool).await });
    let req_pool = pool.clone();

    let get_lb_pos_of_user = warp::path!("leaderboard" / String)
        .and(warp::path::end())
        .map(move |s| (s, req_pool.clone()))
        .and_then(|(s, pool): (String, Arc<Pool<Postgres>>)| async move { api::handle_get_user_lb_pos(pool.as_ref(), s).await });
    let req_pool = pool.clone();

    let get_live_height_lb = warp::path!("live_heights" / "global")
        .and(warp::path::end())
        .map(move || req_pool.clone())
        .and_then(|pool: Arc<Pool<Postgres>>| async move { api::get_live_leaderboard(pool.as_ref()).await });
    let req_pool = pool.clone();

    let get_last_heights_of_user = warp::path!("live_heights" / String)
        .and(warp::path::end())
        .map(move |s| (s, req_pool.clone()))
        .and_then(|(s, pool): (String, Arc<Pool<Postgres>>)| async move { api::handle_get_user_live_heights(pool.as_ref(), s).await });
    let req_pool = pool.clone();

    let get_donations_totals = warp::path!("donations")
        .and(warp::path::end())
        .map(move || req_pool.clone())
        .and_then(|pool: Arc<Pool<Postgres>>| async move { api::handle_get_donations(pool.as_ref()).await });
    let req_pool = pool.clone();

    let get_twitch_handles = warp::path!("twitch" / "list")
        .and(warp::path::end())
        .map(move || req_pool.clone())
        .and_then(|pool: Arc<Pool<Postgres>>| async move { api::handle_get_twitch_list(pool.as_ref()).await });
    let req_pool = pool.clone();

    let get_custom_map_aux_spec = warp::path!("aux_spec" / String / String)
        .and(warp::path::end())
        .map(move |wsid: String, name_id: String| (wsid, name_id, req_pool.clone()))
        .and_then(|(wsid, name_id, pool): (String, String, Arc<Pool<Postgres>>)| async move {
            if !name_id.ends_with(".json") {
                return Err(warp::reject::not_found());
            }
            let name_id = name_id.trim_end_matches(".json").to_string();
            api::handle_get_custom_map_aux_spec(pool.as_ref(), wsid, name_id).await
        });
    let req_pool = pool.clone();

    let get_map_uid_nb_climbers = warp::path!("map" / String / "nb_active_climbers")
        .and(warp::path::end())
        .map(move |uid| (uid, req_pool.clone()))
        .and_then(
            |(uid, pool): (String, Arc<Pool<Postgres>>)| async move { api::handle_get_map_uid_nb_climbers(pool.as_ref(), uid).await },
        );
    let req_pool = pool.clone();

    let get_map_uid_leaderboard = warp::path!("map" / String / "leaderboard")
        .and(warp::path::end())
        .map(move |uid| (uid, req_pool.clone()))
        .and_then(
            |(uid, pool): (String, Arc<Pool<Postgres>>)| async move { api::handle_get_map_uid_leaderboard(pool.as_ref(), uid, 0).await },
        );
    let req_pool = pool.clone();

    let get_map_uid_leaderboard_len = warp::path!("map" / String / "leaderboard" / "len")
        .and(warp::path::end())
        .map(move |uid| (uid, req_pool.clone()))
        .and_then(
            |(uid, pool): (String, Arc<Pool<Postgres>>)| async move { api::handle_get_map_uid_leaderboard_len(pool.as_ref(), uid).await },
        );
    let req_pool = pool.clone();

    let get_map_uid_leaderboard_page = warp::path!("map" / String / "leaderboard" / u32)
        .and(warp::path::end())
        .map(move |uid, page| (uid, page, req_pool.clone()))
        .and_then(|(uid, page, pool): (String, u32, Arc<Pool<Postgres>>)| async move {
            api::handle_get_map_uid_leaderboard(pool.as_ref(), uid, page).await
        });
    let req_pool = pool.clone();

    let get_map_uid_live_heights = warp::path!("map" / String / "live_heights")
        .and(warp::path::end())
        .map(move |uid| (uid, req_pool.clone()))
        .and_then(
            |(uid, pool): (String, Arc<Pool<Postgres>>)| async move { api::handle_get_map_uid_live_heights(pool.as_ref(), uid).await },
        );
    let req_pool = pool.clone();

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
        .or(api_routes)
        .or(version_route)
        .or(get_global_lb)
        .or(get_global_lb_len)
        .or(get_global_lb_page)
        .or(get_global_overview)
        .or(get_server_info)
        .or(get_lb_pos_of_user)
        .or(get_live_height_lb)
        .or(get_last_heights_of_user)
        .or(get_donations_totals)
        .or(get_twitch_handles)
        .or(get_custom_map_aux_spec)
        .or(get_map_uid_nb_climbers)
        .or(get_map_uid_leaderboard)
        .or(get_map_uid_leaderboard_len)
        .or(get_map_uid_leaderboard_page)
        .or(get_map_uid_live_heights)
        .with(warp::log("dips++"));

    info!("Starting HTTP server: {}", soc_addr.to_string());

    // init variables for Prod and Certificate stuff
    #[allow(unused_mut)]
    let mut cache_dir: Option<String> = Some("acme_cache_staging".to_string());
    #[allow(unused_mut)]
    let mut prod = !dev_mode;
    // update for production
    #[cfg(not(debug_assertions))]
    {
        cache_dir = Some("acme_cache_prod".to_string());
        prod = true;
    }

    if prod {
        let tcp_listener = tokio::net::TcpListener::bind(soc_addr).await.unwrap();
        let tcp_incoming = TcpListenerStream::new(tcp_listener);

        let mut domains = vec![lets_enc_domain.clone()];
        if add_extra_domains {
            domains.push("proximity.xk.io".to_string());
        };

        let contact = "mailto:dipspp.letsencrypt@xk.io".to_string();
        let contacts = vec![contact.clone(), contact];

        let tls_incoming = AcmeConfig::new(domains)
            .contact(&contacts)
            .cache_option(cache_dir.clone().map(DirCache::new))
            .directory_lets_encrypt(prod)
            .tokio_incoming(tcp_incoming, Vec::new());

        info!("Running in production mode, using TLS & LetsEncrypt.");
        warp::serve(site).run_incoming(tls_incoming).await;
    } else {
        info!("Running in development mode, using HTTP (no TLS).");
        warp::serve(site)
            .bind_with_graceful_shutdown(soc_addr, async move {
                match cancel_t {
                    Some(ct) => ct.cancelled().await,
                    None => tokio::signal::ctrl_c().await.unwrap(),
                }
            })
            .1
            .await;
    }
    // #[cfg(debug_assertions)]
    // .bind_with_graceful_shutdown(soc_addr, async move {
    //     match cancel_t {
    //         Some(ct) => ct.cancelled().await,
    //         None => tokio::signal::ctrl_c().await.unwrap(),
    //     }
    // })
    // .1
    // .await;
    // #[cfg(not(debug_assertions))]
    // lets_encrypt(site, "dipspp.letsencrypt@xk.io", &lets_enc_domain).await;

    info!("Server shutting down.");

    // Ok(())
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
