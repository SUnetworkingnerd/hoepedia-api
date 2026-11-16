//! Operations endpoints: consistent online backups and the change feed (polling + SSE).

use std::{
    collections::VecDeque,
    convert::Infallible,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue},
    response::{
        sse::{Event, KeepAlive, Sse},
        Response,
    },
    Extension, Json,
};
use futures_util::stream::{self, Stream};
use serde_json::{json, Value};
use std::collections::HashMap;

use crate::{
    error::{ApiError, Res},
    queries,
    rest::change_json,
    state::AppState,
    users::Actor,
    util::{civil, iso, now},
    wiki::log_event,
};

type Q = Query<HashMap<String, String>>;

// ---------------------------------------------------------------------------------------
// backups
// ---------------------------------------------------------------------------------------

fn valid_backup_name(n: &str) -> bool {
    n.len() < 64 && n.starts_with("wiki-") && n.ends_with(".db") && n[5..n.len() - 3].chars().all(|c| c.is_ascii_digit() || c == '-')
}

fn list_backups(dir: &str) -> Vec<Value> {
    let mut out: Vec<(String, u64, i64)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !valid_backup_name(&name) {
                continue;
            }
            if let Ok(md) = e.metadata() {
                let ts = md.modified().ok().and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs() as i64);
                out.push((name, md.len(), ts));
            }
        }
    }
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out.into_iter().map(|(n, s, t)| json!({ "name": n, "size": s, "created": iso(t) })).collect()
}

/// `VACUUM INTO` a new file while holding the writer lock: the copy is transactionally consistent
/// and defragmented. Writers wait for the duration of the copy; readers are unaffected.
pub async fn backup_create(State(st): State<AppState>, Extension(a): Extension<Actor>) -> Res<Json<Value>> {
    a.require("siteadmin")?;
    let dir = st.cfg.backup_dir.clone();
    let who = a.name.clone();
    let wiki = st.wiki.clone();
    let res = tokio::task::spawn_blocking(move || -> Res<Value> {
        std::fs::create_dir_all(&dir).map_err(ApiError::internal)?;
        let (y, mo, d, h, mi, s) = civil(now());
        let name = format!("wiki-{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}.db");
        let path = std::path::Path::new(&dir).join(&name);
        if path.exists() {
            return Err(ApiError::conflict("backupexists", "a backup was already made in this second; retry"));
        }
        let g = wiki.db.writer();
        g.execute("VACUUM INTO ?1", [path.to_string_lossy().as_ref()])?;
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let _ = log_event(&g, "admin", "backup", &who, None, &name, json!({ "size": size }));
        Ok(json!({ "name": name, "size": size, "created": iso(now()) }))
    })
    .await
    .map_err(ApiError::internal)??;
    Ok(Json(res))
}

pub async fn backup_list(State(st): State<AppState>, Extension(a): Extension<Actor>) -> Res<Json<Value>> {
    a.require("siteadmin")?;
    Ok(Json(json!({ "backups": list_backups(&st.cfg.backup_dir) })))
}

pub async fn backup_download(State(st): State<AppState>, Extension(a): Extension<Actor>, Path(name): Path<String>) -> Res<Response> {
    a.require("siteadmin")?;
    if !valid_backup_name(&name) {
        return Err(ApiError::bad("badbackupname", "invalid backup name"));
    }
    let path = std::path::Path::new(&st.cfg.backup_dir).join(&name);
    let file = tokio::fs::File::open(&path).await.map_err(|_| ApiError::not_found("nosuchbackup", "no such backup"))?;
    let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    let body = axum::body::Body::from_stream(tokio_util::io::ReaderStream::new(file));
    let mut resp = Response::new(body);
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/vnd.sqlite3"));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(resp)
}

pub async fn backup_delete(State(st): State<AppState>, Extension(a): Extension<Actor>, Path(name): Path<String>) -> Res<Json<Value>> {
    a.require("siteadmin")?;
    if !valid_backup_name(&name) {
        return Err(ApiError::bad("badbackupname", "invalid backup name"));
    }
    std::fs::remove_file(std::path::Path::new(&st.cfg.backup_dir).join(&name)).map_err(|_| ApiError::not_found("nosuchbackup", "no such backup"))?;
    Ok(Json(json!({ "deleted": name })))
}

// ---------------------------------------------------------------------------------------
// change feed
// ---------------------------------------------------------------------------------------

fn parse_cursor(s: &str) -> Res<(i64, i64)> {
    let bad = || ApiError::bad("badcursor", "cursor must look like <revid>-<logid>, e.g. 0-0");
    let (r, l) = s.split_once('-').ok_or_else(bad)?;
    Ok((r.parse().map_err(|_| bad())?, l.parse().map_err(|_| bad())?))
}

fn fmt_cursor(c: (i64, i64)) -> String {
    format!("{}-{}", c.0, c.1)
}

/// `GET /feed/changes?cursor=R-L&limit=N`. Without a cursor it returns no changes and the
/// current cursor, so a consumer can start "from now". Persist `cursor` and pass it back.
pub async fn feed_changes(State(st): State<AppState>, Query(q): Q) -> Res<Json<Value>> {
    let limit = q.get("limit").and_then(|v| v.parse::<i64>().ok()).unwrap_or(100).clamp(1, 1000);
    let cur = q.get("cursor").map(|c| parse_cursor(c)).transpose()?;
    st.wiki
        .read(move |_, c| {
            let Some(cur) = cur else {
                return Ok(Json(json!({ "changes": [], "cursor": fmt_cursor(queries::current_cursor(c)?), "more": false })));
            };
            let (items, next) = queries::changes_since(c, cur.0, cur.1, limit)?;
            let more = items.len() as i64 == limit;
            Ok(Json(json!({
                "changes": items.iter().map(|(ch, cu)| { let mut j = change_json(ch); j["cursor"] = json!(fmt_cursor(*cu)); j }).collect::<Vec<_>>(),
                "cursor": fmt_cursor(next),
                "more": more,
            })))
        })
        .await
}

static STREAMS: AtomicUsize = AtomicUsize::new(0);
const MAX_STREAMS: usize = 200;

struct Slot;
impl Drop for Slot {
    fn drop(&mut self) {
        STREAMS.fetch_sub(1, Ordering::SeqCst);
    }
}

struct Feed {
    st: AppState,
    cursor: (i64, i64),
    buf: VecDeque<(Value, (i64, i64))>,
    started: bool,
    _slot: Slot,
}

/// `GET /stream/changes` — Server-Sent Events. Resumes from `Last-Event-ID` (or `?cursor=`);
/// otherwise starts from now. Each event is `event: change`, `id: <cursor>`, JSON data.
pub async fn stream_changes(State(st): State<AppState>, Query(q): Q, headers: HeaderMap) -> Res<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    if STREAMS.fetch_add(1, Ordering::SeqCst) >= MAX_STREAMS {
        STREAMS.fetch_sub(1, Ordering::SeqCst);
        return Err(ApiError::new(axum::http::StatusCode::SERVICE_UNAVAILABLE, "toomanystreams", "too many open event streams"));
    }
    let slot = Slot;
    let given = q.get("cursor").cloned().or_else(|| headers.get("last-event-id").and_then(|v| v.to_str().ok()).map(String::from));
    let cursor = match given {
        Some(c) => Some(parse_cursor(&c)?),
        None => None,
    };
    let start = match cursor {
        Some(c) => c,
        None => st.wiki.read(|_, c| queries::current_cursor(c)).await?,
    };
    let feed = Feed { st, cursor: start, buf: VecDeque::new(), started: false, _slot: slot };
    let s = stream::unfold(feed, |mut f| async move {
        loop {
            if let Some((v, cur)) = f.buf.pop_front() {
                let ev = Event::default().event("change").id(fmt_cursor(cur)).data(v.to_string());
                return Some((Ok(ev), f));
            }
            if f.started {
                tokio::time::sleep(Duration::from_millis(1000)).await;
            }
            f.started = true;
            let cur = f.cursor;
            match f.st.wiki.read(move |_, c| queries::changes_since(c, cur.0, cur.1, 200)).await {
                Ok((items, next)) => {
                    f.cursor = next;
                    for (ch, cu) in items {
                        f.buf.push_back((change_json(&ch), cu));
                    }
                }
                Err(_) => tokio::time::sleep(Duration::from_secs(2)).await,
            }
        }
    });
    Ok(Sse::new(s).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}


