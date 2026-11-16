mod action;
mod admin;
mod config;
mod db;
mod diff;
mod error;
mod expr;
mod htmlwt;
mod files;
mod lpat;
mod lua;
mod media;
mod merge;
mod middleware;
mod openapi;
mod present;
mod queries;
mod remote;
mod repl;
mod setup;
mod svg;
mod rest;
mod state;
mod timefmt;
mod title;
mod users;
mod util;
mod wiki;
mod wikitext;

use std::{
    io::{Read, Write},
    net::SocketAddr,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

use axum::{
    extract::{DefaultBodyLimit, State},
    http::header,
    middleware::from_fn_with_state,
    response::IntoResponse,
    routing::get,
    Router,
};
use tower_http::{compression::CompressionLayer, cors::CorsLayer, timeout::TimeoutLayer, trace::TraceLayer};
use tracing_subscriber::EnvFilter;

use crate::{
    config::Config,
    state::{AppState, Metrics, RateLimiter},
    wiki::Wiki,
};

#[tokio::main]
async fn main() {
    // Used by the Docker HEALTHCHECK (the distroless image has no curl/shell).
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--healthcheck") {
        std::process::exit(healthcheck());
    }
    // Settings: command line > environment > .env file > defaults.
    setup::load_dotenv();
    match setup::parse_args(&args) {
        setup::Command::Check(url) => std::process::exit(tokio::task::spawn_blocking(move || setup::check(url)).await.unwrap_or(1)),
        setup::Command::Token => std::process::exit(setup::token_command()),
        setup::Command::Serve => {}
    }
    let token_source = setup::ensure_admin_token();

    // Quiet by default (the banner says what matters); RUST_LOG=info shows every request.
    let colours = std::io::IsTerminal::is_terminal(&std::io::stdout()) && (cfg!(not(windows)) || std::env::var_os("WT_SESSION").is_some());
    tracing_subscriber::fmt()
        .with_ansi(colours)
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("hoepedia_api=info,tower_http=warn,warn")))
        .init();

    let cfg = Arc::new(Config::from_env());
    let db = db::Db::open_as(&cfg.db_path, cfg.read_connections, cfg.replica_of.is_some()).unwrap_or_else(|e| {
        eprintln!("Cannot open the database {}: {e}\nCheck that the folder exists and is writable, or choose another one with --data DIR.", cfg.db_path);
        std::process::exit(1)
    });
    let wiki = Wiki::new(cfg.clone(), db);

    {
        let g = wiki.db.writer();
        if let Ok(Some(salt)) = db::get_state(&g, "meta", "anon_salt") {
            let _ = users::ANON_SALT.set(salt);
        }
    }
    wiki.init_thumb_dir();
    let media = media::detect();
    {
        // Other wikis: re-render dependants when a transcluded page changes, and look for
        // such changes regularly.
        let w = wiki.clone();
        let handle = tokio::runtime::Handle::current();
        wiki.remote.set_on_change(Box::new(move |api, title| {
            let (w, api, title) = (w.clone(), api.to_string(), title.to_string());
            handle.spawn(async move { w.refresh_remote_dependants(&api, &title).await });
        }));
        if cfg.interwiki_poll_secs > 0 {
            let w = wiki.clone();
            let every = std::time::Duration::from_secs(cfg.interwiki_poll_secs.max(5));
            let _ = std::thread::Builder::new().name("interwiki-poll".into()).spawn(move || {
                let mut last = std::collections::HashMap::new();
                loop {
                    std::thread::sleep(every);
                    w.remote.poll_changes(&mut last);
                }
            });
        }
    }
    {
        // Media metadata is read in the background after each upload (and for older files once).
        let w = wiki.clone();
        tokio::spawn(async move {
            loop {
                let woken = w.changed.notified();
                while w.probe_pending().await > 0 {}
                let _ = tokio::time::timeout(std::time::Duration::from_secs(300), woken).await;
            }
        });
    }
    // A node that changed role at run time (fail-over) keeps that role across restarts.
    let dynamic = !cfg.peers.is_empty() || cfg.failover_after_secs > 0;
    if dynamic {
        let saved = db::get_state(&wiki.db.writer(), "local_state", "role").ok().flatten();
        match saved.as_deref() {
            Some("primary") if cfg.replica_of.is_some() => {
                tracing::warn!("this node was promoted to primary earlier; staying primary (REPLICA_OF is ignored until it is demoted)");
                let _ = db::install_change_triggers(&wiki.db.writer());
                wiki.set_upstream(None);
            }
            Some(r) if r.starts_with("replica:") && cfg.replica_of.is_none() => {
                let up = r["replica:".len()..].to_string();
                tracing::warn!(upstream = %up, "this node was demoted to a replica earlier; staying a replica");
                let _ = db::remove_change_triggers(&wiki.db.writer());
                wiki.set_upstream(Some(up));
            }
            _ => {}
        }
    }
    if !wiki.is_replica() {
        // Databases created by older versions lack the newer derived tables: rebuild once.
        let pending = db::get_state(&wiki.db.writer(), "local_state", "needs_reindex").ok().flatten().is_some_and(|v| v == "1");
        if pending {
            tracing::info!("upgrading the index (one-off): re-rendering all pages");
            match wiki.write_always(|w, c| {
                let n = w.reindex_all(c)?;
                db::set_state(c, "local_state", "needs_reindex", "0")?;
                Ok(n)
            })
            .await
            {
                Ok(n) => tracing::info!(pages = n, "index upgraded"),
                Err(e) => tracing::error!(error = %e.info, "index upgrade failed; POST /rest/v1/admin/reindex to retry"),
            }
        }
    }
    if wiki.is_replica() || dynamic {
        repl::spawn_ha(wiki.clone());
    }
    // A replica never creates local users or content: everything comes from the primary.
    if !wiki.is_replica() {
        let c = cfg.clone();
        match wiki.write_always(move |_, conn| users::ensure_admin(conn, &c)).await {
            Ok(Some(token)) => tracing::warn!(
                "no sysop existed, created user 'Admin'. API token (shown ONCE, store it or set ADMIN_TOKEN): {token}"
            ),
            Ok(None) => {}
            Err(e) => {
                eprintln!("Cannot set up the admin account: {}", e.info);
                std::process::exit(1)
            }
        }
        // A brand-new wiki gets a welcome page, so the first request shows something.
        let seeded = wiki
            .write_always(|w, c| {
                let pages: i64 = c.query_row("SELECT count(*) FROM page", [], |r| r.get(0))?;
                let Some(admin) = users::user_by_name(c, "Admin")? else { return Ok(false) };
                if pages > 0 {
                    return Ok(false);
                }
                let req = wiki::EditReq {
                    title: title::Title::new(0, "Main Page"),
                    text: setup::WELCOME_PAGE.to_string(),
                    summary: "Welcome page".into(),
                    minor: false,
                    base_rev: None,
                    create_only: true,
                    no_create: false,
                };
                w.edit(c, &users::Actor::from_user(&admin), req).map(|_| true)
            })
            .await;
        if let Err(e) = seeded {
            tracing::warn!(error = %e.info, "could not create the welcome page");
        }
    }
    let role = if wiki.is_replica() { "replica" } else { "primary" };

    let state = AppState {
        cfg: cfg.clone(),
        wiki,
        limiter: Arc::new(RateLimiter::new(cfg.rate_limit_per_min, Duration::from_secs(60))),
        metrics: Arc::new(Metrics::default()),
    };

    // Layers only wrap the routes added before them: authenticate runs inside rate_limit.
    let api = Router::new()
        .route("/api.php", get(action::api).post(action::api))
        .route("/w/api.php", get(action::api).post(action::api))
        .nest("/rest/v1", rest::router())
        .nest("/api/rest_v1", rest::wikimedia_compat())
        .layer(DefaultBodyLimit::max(cfg.max_page_bytes.max(cfg.upload_max_bytes) * 4 / 3 + 64 * 1024))
        .layer(from_fn_with_state(state.clone(), middleware::forward_writes))
        .layer(from_fn_with_state(state.clone(), middleware::authenticate))
        .layer(from_fn_with_state(state.clone(), middleware::rate_limit));

    let app = Router::new()
        .route("/", get(rest::index))
        .route("/healthz", get(|| async { "ok" }))
        .route("/openapi.json", get(|State(st): State<AppState>| async move { axum::Json(openapi::spec(&st.cfg)) }))
        .route("/metrics", get(metrics))
        .merge(api)
        .fallback(middleware::not_found)
        .with_state(state)
        .layer(CompressionLayer::new())
        .layer(cors_layer(&cfg))
        .layer(TimeoutLayer::new(cfg.request_timeout))
        .layer(TraceLayer::new_for_http());

    let listener = match tokio::net::TcpListener::bind(cfg.addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Cannot listen on {}: {e}\nAnother program may be using this port. Start with another one, for example: hoepedia-api --port {}", cfg.addr, cfg.addr.port().saturating_add(1));
            std::process::exit(1)
        }
    };
    tracing::info!(addr = %cfg.addr, db = %cfg.db_path, anon_read = cfg.anon_read, anon_edit = cfg.anon_edit, "hoepedia-api listening");
    setup::banner(&cfg, &token_source, role, &media.summary());

    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server error");
}

async fn metrics(State(st): State<AppState>) -> impl IntoResponse {
    let m = &st.metrics;
    let (pages, edits, users) = st
        .wiki
        .read(|_, c| queries::stats(c))
        .await
        .map(|s| (s.pages, s.edits, s.users))
        .unwrap_or((0, 0, 0));
    let body = format!(
        "# TYPE hoepedia_requests_total counter\nhoepedia_requests_total {}\n\
         # TYPE hoepedia_rate_limited_total counter\nhoepedia_rate_limited_total {}\n\
         # TYPE hoepedia_edits_total counter\nhoepedia_edits_total {}\n\
         # TYPE hoepedia_searches_total counter\nhoepedia_searches_total {}\n\
         # TYPE hoepedia_pages gauge\nhoepedia_pages {pages}\n\
         # TYPE hoepedia_revisions gauge\nhoepedia_revisions {edits}\n\
         # TYPE hoepedia_users gauge\nhoepedia_users {users}\n\
         # TYPE hoepedia_render_cache_entries gauge\nhoepedia_render_cache_entries {}\n",
        m.requests.load(Ordering::Relaxed),
        m.rate_limited.load(Ordering::Relaxed),
        m.edits.load(Ordering::Relaxed),
        m.searches.load(Ordering::Relaxed),
        st.wiki.cache.entry_count(),
    );
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], body)
}

/// Plain-std HTTP probe so the binary needs no HTTP client dependency.
fn healthcheck() -> i32 {
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(3000);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut s) = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2)) else { return 1 };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    if s.write_all(b"GET /healthz HTTP/1.0\r\nHost: localhost\r\n\r\n").is_err() {
        return 1;
    }
    let mut buf = String::new();
    let _ = s.read_to_string(&mut buf);
    if buf.starts_with("HTTP/1.0 200") || buf.starts_with("HTTP/1.1 200") {
        0
    } else {
        1
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.expect("failed to install Ctrl+C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
    tracing::info!("shutting down");
}

/// `CORS_ORIGINS=*` (default) allows any site to call the API from a browser, which is safe
/// because authentication is by explicit header, never by cookie. A comma-separated list
/// restricts browser access to those origins.
fn cors_layer(cfg: &config::Config) -> CorsLayer {
    use tower_http::cors::{AllowOrigin, Any};
    let list: Vec<axum::http::HeaderValue> = cfg.cors_origins.split(',').map(str::trim).filter(|o| !o.is_empty() && *o != "*").filter_map(|o| o.trim_end_matches('/').parse().ok()).collect();
    if list.is_empty() {
        return CorsLayer::permissive();
    }
    CorsLayer::new().allow_origin(AllowOrigin::list(list)).allow_methods(Any).allow_headers(Any).expose_headers(Any)
}
