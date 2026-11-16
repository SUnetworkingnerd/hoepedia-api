use std::{
    net::{IpAddr, SocketAddr},
    sync::atomic::Ordering,
};

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

use crate::{error::ApiError, state::AppState, users, users::Actor};

/// True if the request proves knowledge of this node's admin token in `name`
/// (how the nodes of a cluster recognise each other without a database lookup).
fn has_admin_token(headers: &HeaderMap, name: &str, cfg: &crate::config::Config) -> bool {
    let Some(admin) = cfg.admin_token.as_deref().filter(|t| !t.is_empty()) else { return false };
    let given = if name == "authorization" { bearer(headers) } else { headers.get(name).and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string()) };
    given.is_some_and(|g| {
        use sha2::{Digest, Sha256};
        // compare digests so the comparison time does not depend on the token
        Sha256::digest(g.as_bytes()) == Sha256::digest(admin.as_bytes())
    })
}

pub fn client_ip(headers: &HeaderMap, peer: SocketAddr, cfg: &crate::config::Config) -> IpAddr {
    // A replica that forwards a write names the real client; believed only with the admin token.
    if let Some(ip) = headers.get("x-hoepedia-client-ip").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<IpAddr>().ok()) {
        if has_admin_token(headers, "x-hoepedia-replica-token", cfg) {
            return ip;
        }
    }
    let trust_proxy = cfg.trust_proxy;
    if trust_proxy {
        if let Some(ip) = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .and_then(|v| v.trim().parse::<IpAddr>().ok())
        {
            return ip;
        }
    }
    peer.ip()
}

/// Per-IP rate limit + request counting.
pub async fn rate_limit(State(st): State<AppState>, ConnectInfo(peer): ConnectInfo<SocketAddr>, req: Request, next: Next) -> Response {
    st.metrics.requests.fetch_add(1, Ordering::Relaxed);
    let ip = client_ip(req.headers(), peer, &st.cfg);
    // Replicas poll once per change; with the admin token they are not rate limited.
    let replication = req.uri().path().ends_with("/replication/changes") && has_admin_token(req.headers(), "authorization", &st.cfg);
    if !replication && !st.limiter.allow(ip) {
        st.metrics.rate_limited.fetch_add(1, Ordering::Relaxed);
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "60")],
            Json(json!({ "error": { "code": "ratelimited", "info": "rate limit exceeded" }, "httpCode": 429 })),
        )
            .into_response();
    }
    next.run(req).await
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        let v = v.trim();
        if v.len() > 7 && v[..7].eq_ignore_ascii_case("bearer ") {
            return Some(v[7..].trim().to_string());
        }
    }
    headers.get("x-api-token").and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string())
}

/// Resolves the caller (token or anonymous), enforces the `read` right, stores the `Actor`
/// in the request extensions and adds caching headers to successful reads.
pub async fn authenticate(State(st): State<AppState>, ConnectInfo(peer): ConnectInfo<SocketAddr>, mut req: Request, next: Next) -> Response {
    let ip = client_ip(req.headers(), peer, &st.cfg);
    let token = bearer(req.headers());
    let actor = match &token {
        Some(t) if !t.is_empty() => {
            let t = t.clone();
            match st.wiki.read(move |_, c| users::user_by_token(c, &t)).await {
                Ok(Some(u)) => Actor::from_user(&u),
                Ok(None) => return ApiError::unauthorized("invalid API token").into_response(),
                Err(e) => return e.into_response(),
            }
        }
        _ => Actor::anon(ip, &st.cfg),
    };
    if let Err(e) = actor.require("read") {
        return e.into_response();
    }
    let authed = !actor.is_anon();
    let is_read = matches!(*req.method(), Method::GET | Method::HEAD);
    req.extensions_mut().insert(actor);
    let mut resp = next.run(req).await;
    if is_read && resp.status().is_success() && !resp.headers().contains_key(header::CACHE_CONTROL) {
        let scope = if authed || !st.cfg.anon_read { "private" } else { "public" };
        if let Ok(v) = HeaderValue::from_str(&format!("{scope}, max-age={}, must-revalidate", st.cfg.cache_max_age)) {
            resp.headers_mut().insert(header::CACHE_CONTROL, v);
        }
    }
    resp = jsonify_rejection(resp).await;
    resp.headers_mut().append(header::VARY, HeaderValue::from_static("Authorization, Accept-Encoding"));
    resp
}

/// Axum extractor rejections (bad path/query, body too large, 405...) are plain text;
/// rewrite them into the engine's JSON error format so clients only ever parse one shape.
async fn jsonify_rejection(resp: Response) -> Response {
    let status = resp.status();
    if !(status.is_client_error() || status.is_server_error()) {
        return resp;
    }
    let ct = resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    if ct.contains("json") {
        return resp;
    }
    let (_, body) = resp.into_parts();
    let bytes = axum::body::to_bytes(body, 4096).await.unwrap_or_default();
    let msg = String::from_utf8_lossy(&bytes).trim().to_string();
    let code = match status.as_u16() {
        404 => "notfound",
        405 => "badmethod",
        413 => "contenttoobig",
        415 => "badcontenttype",
        408 => "timeout",
        s if s >= 500 => "internal_api_error",
        _ => "badrequest",
    };
    let info = if msg.is_empty() { status.canonical_reason().unwrap_or("error").to_string() } else { msg };
    ApiError::new(status, code, info).into_response()
}

fn is_api_write(action: &str) -> bool {
    crate::action::WRITE_ACTIONS.contains(&action)
}

/// On a primary: tells clients (and forwarding replicas) the replication position after a
/// write. On a replica: write requests are passed on to the primary, and the answer is held
/// back until this replica has applied the change, so the client can read its own write here.
pub async fn forward_writes(State(st): State<AppState>, ConnectInfo(peer): ConnectInfo<SocketAddr>, req: Request, next: Next) -> Response {
    let is_read = matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    let Some(upstream) = st.wiki.upstream() else {
        let mut resp = next.run(req).await;
        if !is_read {
            if let Ok(v) = HeaderValue::from_str(&st.wiki.last_seq.load(Ordering::SeqCst).to_string()) {
                resp.headers_mut().insert("x-repl-seq", v);
            }
        }
        return resp;
    };
    let path = req.uri().path().to_string();
    // Local on purpose: conversions, this node's own replication controls and backups.
    let local = path.contains("/transform/") || path.contains("/replication/") || path.contains("/admin/backups");
    if is_read || local || !st.cfg.forward_writes || st.cfg.read_only || req.headers().contains_key("x-hoepedia-forwarded") {
        return next.run(req).await;
    }
    let ip = client_ip(req.headers(), peer, &st.cfg);
    let (parts, body) = req.into_parts();
    let limit = st.cfg.max_page_bytes.max(st.cfg.upload_max_bytes) * 4 / 3 + 64 * 1024;
    let Ok(bytes) = axum::body::to_bytes(body, limit).await else {
        return ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "contenttoobig", "request body too large").into_response();
    };
    if path.ends_with("api.php") {
        // Reads sent by POST (action=query, parse ...) are answered here.
        let ct = parts.headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
        let from = |src: &[u8]| form_urlencoded::parse(src).find(|(k, _)| k == "action").map(|(_, v)| v.into_owned());
        let action = from(parts.uri.query().unwrap_or("").as_bytes()).or_else(|| if ct.starts_with("application/x-www-form-urlencoded") { from(&bytes) } else { None });
        let write = match &action {
            Some(a) => is_api_write(a),
            None => ct.starts_with("multipart/"),
        };
        if !write {
            return next.run(Request::from_parts(parts, axum::body::Body::from(bytes))).await;
        }
    }
    let url = format!("{upstream}{}", parts.uri.path_and_query().map(|p| p.as_str()).unwrap_or(&path));
    let method = parts.method.as_str().to_string();
    let mut headers: Vec<(String, String)> = Vec::new();
    for name in ["authorization", "x-api-token", "content-type", "if-match", "if-none-match", "accept"] {
        if let Some(v) = parts.headers.get(name).and_then(|v| v.to_str().ok()) {
            headers.push((name.to_string(), v.to_string()));
        }
    }
    headers.push(("x-hoepedia-forwarded".into(), "1".into()));
    headers.push(("x-hoepedia-client-ip".into(), ip.to_string()));
    headers.push(("x-hoepedia-replica-token".into(), st.cfg.replica_token.clone()));
    let timeout = st.cfg.request_timeout;
    let sent = tokio::task::spawn_blocking(move || {
        let agent = ureq::AgentBuilder::new().timeout_connect(std::time::Duration::from_secs(5)).timeout(timeout).redirects(0).build();
        let mut r = agent.request(&method, &url);
        for (k, v) in &headers {
            r = r.set(k, v);
        }
        let resp = match if bytes.is_empty() { r.call() } else { r.send_bytes(&bytes) } {
            Ok(resp) => resp,
            Err(ureq::Error::Status(_, resp)) => resp,
            Err(e) => return Err(e.to_string()),
        };
        let status = resp.status();
        let ct = resp.header("content-type").unwrap_or("application/json").to_string();
        let seq = resp.header("x-repl-seq").and_then(|v| v.parse::<i64>().ok());
        let mut body = Vec::new();
        use std::io::Read;
        resp.into_reader().take(256 * 1024 * 1024).read_to_end(&mut body).map_err(|e| e.to_string())?;
        Ok((status, ct, seq, body))
    })
    .await;
    let (status, ct, seq, body) = match sent {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            return ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "primaryunreachable", format!("this replica could not pass the write on to the primary: {e}")).into_response();
        }
        Err(e) => return ApiError::internal(e).into_response(),
    };
    // Read-your-writes: wait (briefly) until the change has arrived here too.
    if let Some(seq) = seq.filter(|_| (200..300).contains(&status)) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while st.wiki.repl.lock().map(|s| s.applied_seq).unwrap_or(i64::MAX) < seq && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
    let mut resp = (StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY), body).into_response();
    if let Ok(v) = HeaderValue::from_str(&ct) {
        resp.headers_mut().insert(header::CONTENT_TYPE, v);
    }
    resp.headers_mut().insert("x-hoepedia-served-by", HeaderValue::from_static("primary"));
    resp
}

pub async fn not_found() -> Response {
    ApiError::not_found("notfound", "no such endpoint; see GET /").into_response()
}
