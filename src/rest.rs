//! REST API. Mounted twice:
//!   /rest/v1      – native routes (modelled on MediaWiki's core REST API + extras)
//!   /api/rest_v1  – Wikimedia-REST-compatible aliases (page/summary/{t}, page/html/{t}, ...)

use std::{collections::HashMap, sync::atomic::Ordering};

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Extension, Json, Router,
};
use rusqlite::Connection;
use serde_json::{json, Value};

use crate::{
    diff,
    error::{ApiError, Res},
    present::*,
    queries,
    state::AppState,
    title::{ns_name, Title, NS_CATEGORY},
    users::{self, Actor},
    util::iso,
    wiki::{self, find_page, follow_redirects, EditReq, PageRow, Wiki},
};

type Q = Query<HashMap<String, String>>;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(index))
        .route("/site", get(site))
        .route("/me", get(me))
        .route("/me/token", post(rotate_my_token))
        .route("/page/:title", get(page_bare).post(page_create).put(page_put).delete(page_delete))
        .route("/page/:title/bare", get(page_bare))
        .route("/page/:title/html", get(page_html))
        .route("/page/:title/with_html", get(page_with_html))
        .route("/page/:title/summary", get(page_summary))
        .route("/page/:title/history", get(page_history))
        .route("/page/:title/history/counts", get(page_history_counts))
        .route("/page/:title/links", get(page_links))
        .route("/page/:title/linkshere", get(page_linkshere))
        .route("/page/:title/categories", get(page_categories))
        .route("/page/:title/templates", get(page_templates))
        .route("/page/:title/sections", get(page_sections))
        .route("/page/:title/move", post(page_move))
        .route("/page/:title/undelete", post(page_undelete))
        .route("/page/:title/protect", post(page_protect))
        .route("/page/:title/rollback", post(page_rollback))
        .route("/page/:title/undo", post(page_undo))
        .route("/page/:title/restore", post(page_restore))
        .route("/revision/:id", get(revision_get))
        .route("/revision/:id/html", get(revision_html))
        .route("/revision/:id/compare/:to", get(revision_compare))
        .route("/pages", get(pages_list))
        .route("/random", get(random_summary))
        .route("/search/page", get(search_page))
        .route("/search/title", get(search_title))
        .route("/category/:name/members", get(category_members))
        .route("/recentchanges", get(recent_changes))
        .route("/logs", get(logs))
        .route("/users", get(users_list).post(user_create))
        .route("/users/:name/contributions", get(user_contribs))
        .route("/users/:name/groups", put(user_groups))
        .route("/users/:name/block", post(user_block))
        .route("/users/:name/unblock", post(user_unblock))
        .route("/transform/wikitext/to/html", post(transform_html))
        .route("/transform/wikitext/expand", post(transform_expand))
        .route("/transform/html/to/wikitext", post(transform_html_to_wikitext))
        .route("/file/:name", get(file_get).put(file_put).post(file_post).delete(file_delete))
        .route("/file/:name/info", get(file_info))
        .route("/file/:name/history", get(file_versions))
        .route("/files", get(files_list))
        .route("/admin/backups", get(crate::admin::backup_list).post(crate::admin::backup_create))
        .route("/admin/backups/:name", get(crate::admin::backup_download).delete(crate::admin::backup_delete))
        .route("/admin/reindex", post(admin_reindex))
        .route("/watchlist", get(watchlist_get))
        .route("/watchlist/:title", put(watchlist_put).delete(watchlist_delete))
        .route("/interwiki/:prefix/purge", post(interwiki_purge))
        .route("/replication/changes", get(crate::repl::changes_handler))
        .route("/replication/status", get(crate::repl::status_handler))
        .route("/replication/resync", post(crate::repl::resync_handler))
        .route("/replication/promote", post(crate::repl::promote_handler))
        .route("/replication/vote", post(crate::repl::vote_handler))
        .route("/feed/changes", get(crate::admin::feed_changes))
        .route("/stream/changes", get(crate::admin::stream_changes))
        .route("/interwiki", get(interwiki_list))
        .route("/interwiki/:prefix", put(interwiki_put).delete(interwiki_delete))
}

/// Wikimedia REST API compatible paths (same handlers, different shapes of URL).
pub fn wikimedia_compat() -> Router<AppState> {
    Router::new()
        .route("/page/summary/:title", get(page_summary))
        .route("/page/html/:title", get(page_html))
        .route("/page/title/:title", get(page_history))
        .route("/page/random/summary", get(random_summary))
        .route("/page/random/html", get(random_summary))
        .route("/page/revision/:id", get(revision_get))
}

// ---------------------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------------------

fn lim(q: &HashMap<String, String>, key: &str, default: i64, max: i64) -> i64 {
    q.get(key).and_then(|v| v.parse::<i64>().ok()).unwrap_or(default).clamp(1, max)
}
fn off(q: &HashMap<String, String>) -> i64 {
    q.get("offset").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0).clamp(0, 100_000)
}
fn opt_id(q: &HashMap<String, String>, key: &str) -> Option<i64> {
    q.get(key).and_then(|v| v.parse().ok())
}
fn follow(q: &HashMap<String, String>) -> bool {
    q.get("redirect").is_none_or(|v| v != "false" && v != "0")
}
fn ns_param(q: &HashMap<String, String>) -> Res<Vec<i64>> {
    parse_ns_list(q.get("namespace").map(String::as_str).unwrap_or(""))
}

fn body_json(b: &Bytes) -> Res<Value> {
    if b.iter().all(|c| c.is_ascii_whitespace()) {
        return Ok(json!({}));
    }
    match serde_json::from_slice::<Value>(b) {
        Ok(v) if v.is_object() => Ok(v),
        Ok(_) => Err(ApiError::bad("badjson", "request body must be a JSON object")),
        Err(e) => Err(ApiError::bad("badjson", format!("invalid JSON body: {e}"))),
    }
}
fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}
fn comment(v: &Value, q: &HashMap<String, String>) -> String {
    s(v, "comment").map(String::from).or_else(|| q.get("comment").cloned()).unwrap_or_default()
}

fn rev_id(s: &str) -> Res<i64> {
    s.parse().map_err(|_| ApiError::bad("badrevid", format!("'{s}' is not a valid revision id")))
}

fn load_page(c: &Connection, raw: &str, follow_redir: bool) -> Res<(PageRow, Option<Title>)> {
    let t = parse_title(raw)?;
    let p = find_page(c, &t)?.ok_or_else(|| ApiError::not_found("missingtitle", format!("page '{}' does not exist", t.full())))?;
    if follow_redir {
        follow_redirects(c, p)
    } else {
        Ok((p, None))
    }
}

enum Out {
    Json(Value),
    Html(String),
}

fn finish(etag: String, out: Option<Out>) -> Response {
    let mut resp = match out {
        None => StatusCode::NOT_MODIFIED.into_response(),
        Some(Out::Json(v)) => Json(v).into_response(),
        Some(Out::Html(h)) => ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], h).into_response(),
    };
    if let Ok(v) = HeaderValue::from_str(&etag) {
        resp.headers_mut().insert(header::ETAG, v);
    }
    resp
}

fn inm(h: &HeaderMap) -> Option<String> {
    h.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()).map(String::from)
}

fn etag(w: &Wiki, p: &PageRow, kind: &str) -> String {
    format!("W/\"{}-{}-{}\"", p.latest, w.epoch.load(Ordering::SeqCst), kind)
}

fn bare(w: &Wiki, p: &PageRow, rev: &wiki::RevRow, text: &str) -> Value {
    let mut v = json!({
        "id": p.id,
        "key": p.title.key(),
        "title": p.title.full(),
        "namespace": ns_json(p.title.ns),
        "latest": { "id": rev.id, "timestamp": iso(rev.ts) },
        "content_model": rev.model,
        "protection": protection_json(p),
        "source": text,
        "url": article_url(w, &p.title),
    });
    if let Some(t) = p.redirect.as_ref().filter(|_| p.is_redirect) {
        v["redirect_target"] = json!(t.key());
    }
    v
}

// ---------------------------------------------------------------------------------------
// meta
// ---------------------------------------------------------------------------------------

pub async fn index(State(st): State<AppState>) -> Json<Value> {
    Json(json!({
        "name": st.cfg.site_name,
        "engine": "hoepedia-api",
        "version": env!("CARGO_PKG_VERSION"),
        "docs": "README.md (beginner guide), ADVANCED.md (reference), /openapi.json",
        "apis": { "action": "/api.php?action=help", "rest": "/rest/v1/", "wikimedia_compat": "/api/rest_v1/", "openapi": "/openapi.json" },
        "rest_endpoints": [
            "GET /rest/v1/site", "GET /rest/v1/me",
            "GET|POST|PUT|DELETE /rest/v1/page/{title}",
            "GET /rest/v1/page/{title}/{bare|html|with_html|summary|history|history/counts|links|linkshere|categories|templates|sections}",
            "POST /rest/v1/page/{title}/{move|undelete|protect|rollback}",
            "GET /rest/v1/revision/{id}[/html]", "GET /rest/v1/revision/{id}/compare/{to}",
            "GET /rest/v1/pages", "GET /rest/v1/random",
            "GET /rest/v1/search/page?q=", "GET /rest/v1/search/title?q=",
            "GET /rest/v1/category/{name}/members", "GET /rest/v1/recentchanges", "GET /rest/v1/logs",
            "GET /rest/v1/users", "POST /rest/v1/users", "GET /rest/v1/users/{name}/contributions",
            "PUT /rest/v1/users/{name}/groups", "POST /rest/v1/users/{name}/{block|unblock}", "POST /rest/v1/me/token",
            "POST /rest/v1/transform/wikitext/to/html"
        ]
    }))
}

pub async fn site(State(st): State<AppState>) -> Res<Json<Value>> {
    let stats = st.wiki.read(|_, c| queries::stats(c)).await?;
    Ok(Json(json!({
        "sitename": st.cfg.site_name,
        "generator": format!("hoepedia-api {}", env!("CARGO_PKG_VERSION")),
        "article_path": st.cfg.article_path,
        "anonymous_read": st.cfg.anon_read,
        "anonymous_edit": st.cfg.anon_edit,
        "max_page_bytes": st.cfg.max_page_bytes,
        "namespaces": namespaces_json(),
        "statistics": { "pages": stats.pages, "articles": stats.articles, "edits": stats.edits, "users": stats.users },
    })))
}

pub fn actor_json(a: &Actor) -> Value {
    json!({ "id": a.user_id.unwrap_or(0), "name": a.name, "anonymous": a.is_anon(), "groups": a.groups, "rights": a.rights(), "blocked": a.blocked })
}

pub async fn me(Extension(a): Extension<Actor>) -> Json<Value> {
    Json(actor_json(&a))
}

async fn rotate_my_token(State(st): State<AppState>, Extension(a): Extension<Actor>) -> Res<Json<Value>> {
    let Some(uid) = a.user_id else { return Err(ApiError::unauthorized("log in with a token first")) };
    let token = st.wiki.write(move |_, c| users::rotate_token(c, uid)).await?;
    Ok(Json(json!({ "token": token, "note": "store this now; the old token no longer works" })))
}

// ---------------------------------------------------------------------------------------
// page reads
// ---------------------------------------------------------------------------------------

async fn page_bare(State(st): State<AppState>, Path(title): Path<String>, Query(q): Q, h: HeaderMap) -> Res<Response> {
    let (inm, fl) = (inm(&h), follow(&q) && q.contains_key("redirect"));
    st.wiki
        .read(move |w, c| {
            let (p, _) = load_page(c, &title, fl)?;
            let tag = format!("\"{}\"", p.latest);
            if inm.as_deref() == Some(&tag) {
                return Ok(finish(tag, None));
            }
            let rev = wiki::get_rev(c, p.latest)?.ok_or_else(|| ApiError::internal("missing revision"))?;
            let text = wiki::rev_text(c, p.latest)?;
            Ok(finish(tag, Some(Out::Json(bare(w, &p, &rev, &text)))))
        })
        .await
}

async fn page_html(State(st): State<AppState>, Path(title): Path<String>, Query(q): Q, h: HeaderMap) -> Res<Response> {
    let (inm, fl) = (inm(&h), follow(&q));
    st.wiki
        .read(move |w, c| {
            let (p, _) = load_page(c, &title, fl)?;
            let tag = etag(w, &p, "html");
            if inm.as_deref() == Some(&tag) {
                return Ok(finish(tag, None));
            }
            let (r, _) = w.render_page(c, &p)?;
            Ok(finish(tag, Some(Out::Html(r.html.clone()))))
        })
        .await
}

async fn page_with_html(State(st): State<AppState>, Path(title): Path<String>, Query(q): Q, h: HeaderMap) -> Res<Response> {
    let (inm, fl) = (inm(&h), follow(&q));
    st.wiki
        .read(move |w, c| {
            let (p, from) = load_page(c, &title, fl)?;
            let tag = etag(w, &p, "wh");
            if inm.as_deref() == Some(&tag) {
                return Ok(finish(tag, None));
            }
            let rev = wiki::get_rev(c, p.latest)?.ok_or_else(|| ApiError::internal("missing revision"))?;
            let (r, text) = w.render_page(c, &p)?;
            let mut v = bare(w, &p, &rev, &text);
            v["html"] = json!(r.html);
            if let Some(f) = from {
                v["redirected_from"] = json!(f.key());
            }
            Ok(finish(tag, Some(Out::Json(v))))
        })
        .await
}

async fn page_summary(State(st): State<AppState>, Path(title): Path<String>, Query(q): Q, h: HeaderMap) -> Res<Response> {
    let (inm, fl) = (inm(&h), follow(&q));
    st.wiki
        .read(move |w, c| {
            let (p, from) = load_page(c, &title, fl)?;
            let tag = etag(w, &p, "sum");
            if inm.as_deref() == Some(&tag) {
                return Ok(finish(tag, None));
            }
            Ok(finish(tag, Some(Out::Json(summary_json(w, c, &p, from.as_ref())?))))
        })
        .await
}

fn delta(c: &Connection, r: &wiki::RevRow) -> Res<i64> {
    if r.parent == 0 {
        return Ok(r.size);
    }
    Ok(wiki::get_rev(c, r.parent)?.map_or(r.size, |p| r.size - p.size))
}

async fn page_history(State(st): State<AppState>, Path(title): Path<String>, Query(q): Q) -> Res<Json<Value>> {
    let limit = lim(&q, "limit", 20, 100);
    let (older, newer, user) = (opt_id(&q, "older_than"), opt_id(&q, "newer_than"), q.get("user").cloned());
    st.wiki
        .read(move |w, c| {
            let (p, _) = load_page(c, &title, false)?;
            let is_newer = newer.is_some();
            let revs = queries::history(c, p.id, limit, newer.or(older), is_newer, user.as_deref())?;
            let mut items = Vec::new();
            for r in &revs {
                let mut j = rev_json(r);
                j["id"] = json!(r.id);
                j["delta"] = json!(delta(c, r)?);
                items.push(j);
            }
            let more = revs.len() as i64 == limit;
            let cursor = revs.last().map(|r| r.id);
            Ok(Json(json!({
                "page": { "id": p.id, "key": p.title.key(), "title": p.title.full(), "url": article_url(w, &p.title) },
                "revisions": items,
                "next": if more { json!({ if is_newer { "newer_than" } else { "older_than" }: cursor }) } else { Value::Null },
            })))
        })
        .await
}

async fn page_history_counts(State(st): State<AppState>, Path(title): Path<String>) -> Res<Json<Value>> {
    st.wiki
        .read(move |_, c| {
            let (p, _) = load_page(c, &title, false)?;
            let n = queries::history_counts(c, p.id)?;
            Ok(Json(json!({ "count": n.total, "minor": n.minor, "anonymous": n.anonymous, "editors": n.editors, "latest": p.latest })))
        })
        .await
}

fn title_obj(w: &Wiki, t: &Title, exists: Option<bool>) -> Value {
    let mut v = json!({ "key": t.key(), "title": t.full(), "namespace": ns_json(t.ns), "url": article_url(w, t) });
    if let Some(e) = exists {
        v["exists"] = json!(e);
    }
    v
}

async fn outlinks(st: AppState, title: String, q: HashMap<String, String>, table: &'static str, key: &'static str) -> Res<Json<Value>> {
    let (limit, offset) = (lim(&q, "limit", 100, 500), off(&q));
    st.wiki
        .read(move |w, c| {
            let (p, _) = load_page(c, &title, false)?;
            let items: Vec<Value> = queries::page_outlinks(c, table, p.id, limit, offset)?.iter().map(|(t, e)| title_obj(w, t, Some(*e))).collect();
            Ok(Json(json!({ key: items })))
        })
        .await
}
async fn page_links(State(st): State<AppState>, Path(t): Path<String>, Query(q): Q) -> Res<Json<Value>> {
    outlinks(st, t, q, "pagelink", "links").await
}
async fn page_templates(State(st): State<AppState>, Path(t): Path<String>, Query(q): Q) -> Res<Json<Value>> {
    outlinks(st, t, q, "templatelink", "templates").await
}

async fn page_linkshere(State(st): State<AppState>, Path(title): Path<String>, Query(q): Q) -> Res<Json<Value>> {
    let (limit, offset) = (lim(&q, "limit", 100, 500), off(&q));
    let table = if q.get("type").map(String::as_str) == Some("transclusions") { "templatelink" } else { "pagelink" };
    let ns = ns_param(&q)?;
    st.wiki
        .read(move |w, c| {
            let t = parse_title(&title)?;
            let items: Vec<Value> = queries::pages_linking_to(c, table, &t, &ns, limit, offset)?
                .iter()
                .map(|p| {
                    let mut v = title_obj(w, &p.title, None);
                    v["id"] = json!(p.id);
                    v["redirect"] = json!(p.is_redirect);
                    v
                })
                .collect();
            Ok(Json(json!({ "target": t.key(), "pages": items })))
        })
        .await
}

async fn page_categories(State(st): State<AppState>, Path(title): Path<String>, Query(q): Q) -> Res<Json<Value>> {
    let (limit, offset) = (lim(&q, "limit", 100, 500), off(&q));
    st.wiki
        .read(move |_, c| {
            let (p, _) = load_page(c, &title, false)?;
            let items: Vec<Value> = queries::page_categories(c, p.id, limit, offset)?
                .into_iter()
                .map(|(cat, key)| json!({ "key": Title::new(NS_CATEGORY, &cat).key(), "title": cat, "sortkey": key }))
                .collect();
            Ok(Json(json!({ "categories": items })))
        })
        .await
}

async fn page_sections(State(st): State<AppState>, Path(title): Path<String>, Query(q): Q) -> Res<Json<Value>> {
    let fl = follow(&q);
    st.wiki
        .read(move |w, c| {
            let (p, _) = load_page(c, &title, fl)?;
            let (r, _) = w.render_page(c, &p)?;
            Ok(Json(json!({ "sections": r.sections })))
        })
        .await
}

// ---------------------------------------------------------------------------------------
// revisions
// ---------------------------------------------------------------------------------------

async fn revision_get(State(st): State<AppState>, Path(id): Path<String>) -> Res<Json<Value>> {
    let id = rev_id(&id)?;
    st.wiki
        .read(move |w, c| {
            let (rev, p) = wiki::rev_with_page(c, id)?;
            let text = wiki::rev_text(c, id)?;
            let mut v = bare(w, &p, &rev, &text);
            v["revision"] = rev_json(&rev);
            v["is_latest"] = json!(rev.id == p.latest);
            Ok(Json(v))
        })
        .await
}

async fn revision_html(State(st): State<AppState>, Path(id): Path<String>) -> Res<Response> {
    let id = rev_id(&id)?;
    st.wiki
        .read(move |w, c| {
            let (rev, p) = wiki::rev_with_page(c, id)?;
            let text = wiki::rev_text(c, id)?;
            let r = w.render_rev(c, rev.id, &p.title, &text);
            Ok(finish(format!("W/\"r{}-{}\"", rev.id, w.epoch.load(Ordering::SeqCst)), Some(Out::Html(r.html.clone()))))
        })
        .await
}

pub fn compare_json(c: &Connection, from: i64, to: i64) -> Res<Value> {
    let (fr, fp) = wiki::rev_with_page(c, from)?;
    let (tr, tp) = wiki::rev_with_page(c, to)?;
    let d = diff::diff(&wiki::rev_text(c, from)?, &wiki::rev_text(c, to)?);
    let side = |r: &wiki::RevRow, p: &PageRow| json!({ "id": r.id, "page_id": p.id, "title": p.title.full(), "timestamp": iso(r.ts), "user": r.user_name, "comment": r.comment, "size": r.size });
    Ok(json!({ "from": side(&fr, &fp), "to": side(&tr, &tp), "diff": d }))
}

async fn revision_compare(State(st): State<AppState>, Path((id, to)): Path<(String, String)>) -> Res<Json<Value>> {
    let (id, to) = (rev_id(&id)?, rev_id(&to)?);
    st.wiki.read(move |_, c| Ok(Json(compare_json(c, id, to)?))).await
}

// ---------------------------------------------------------------------------------------
// lists & search
// ---------------------------------------------------------------------------------------

async fn pages_list(State(st): State<AppState>, Query(q): Q) -> Res<Json<Value>> {
    let ns = ns_param(&q)?.first().copied().unwrap_or(0);
    let limit = lim(&q, "limit", 50, 500);
    let prefix = q.get("prefix").map(|p| Title::parse_in(p, ns).map(|t| t.title.name).unwrap_or_default()).unwrap_or_default();
    let from = q.get("from").cloned().unwrap_or_default().replace('_', " ");
    let filter = match q.get("redirects").map(String::as_str) {
        Some("only") => "redirects",
        Some("none") => "nonredirects",
        _ => "all",
    };
    st.wiki
        .read(move |w, c| {
            let mut rows = queries::all_pages(c, ns, &prefix, &from, limit + 1, 0, filter)?;
            let next = if rows.len() as i64 > limit { rows.pop().map(|p| p.title.name) } else { None };
            let items: Vec<Value> = rows
                .iter()
                .map(|p| {
                    let mut v = title_obj(w, &p.title, None);
                    v["id"] = json!(p.id);
                    v["redirect"] = json!(p.is_redirect);
                    v["size"] = json!(p.len);
                    v
                })
                .collect();
            Ok(Json(json!({ "pages": items, "next": next })))
        })
        .await
}

async fn random_summary(State(st): State<AppState>, Query(q): Q) -> Res<Response> {
    let ns = ns_param(&q)?;
    st.wiki
        .read(move |w, c| {
            let p = queries::random_pages(c, &ns, 1)?.into_iter().next().ok_or_else(|| ApiError::not_found("nopages", "the wiki has no pages in that namespace"))?;
            let mut resp = Json(summary_json(w, c, &p, None)?).into_response();
            resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            Ok(resp)
        })
        .await
}

async fn search_page(State(st): State<AppState>, Query(q): Q) -> Res<Json<Value>> {
    let query = q.get("q").cloned().ok_or_else(|| ApiError::bad("missingparam", "the q parameter is required"))?;
    if query.len() > 300 {
        return Err(ApiError::bad("badquery", "q is too long"));
    }
    let (limit, offset) = (lim(&q, "limit", 10, 50), off(&q));
    let ns = if q.contains_key("namespace") { ns_param(&q)? } else { vec![0] };
    st.metrics.searches.fetch_add(1, Ordering::Relaxed);
    // ?interwiki=1 also asks the remote wikis configured with an API (after the local query).
    let federate = q.get("interwiki").is_some_and(|v| !matches!(v.as_str(), "0" | "false" | "no"));
    let q2 = query.clone();
    let (mut out, targets) = st
        .wiki
        .read(move |w, c| {
            let targets = if federate { Wiki::search_targets(c)? } else { Vec::new() };
            let (total, hits) = queries::search(c, &query, &ns, limit, offset, false)?;
            let pages: Vec<Value> = hits
                .iter()
                .map(|h| {
                    let mut v = title_obj(w, &h.page.title, None);
                    v["id"] = json!(h.page.id);
                    v["excerpt"] = json!(h.snippet);
                    v["size"] = json!(h.page.len);
                    v["wordcount"] = json!(h.page.words);
                    v["timestamp"] = json!(iso(h.page.touched));
                    v
                })
                .collect();
            Ok((json!({ "total": total, "pages": pages, "next_offset": if offset + limit < total { json!(offset + limit) } else { Value::Null } }), targets))
        })
        .await?;
    if federate {
        let wk = st.wiki.clone();
        out["interwiki"] = tokio::task::spawn_blocking(move || wk.federated_search(&targets, &q2, limit)).await.map_err(ApiError::internal)?;
    }
    Ok(Json(out))
}

/// `GET /watchlist`: recent changes to watched pages; `?raw=1`: the watched titles.
async fn watchlist_get(State(st): State<AppState>, Extension(a): Extension<Actor>, Query(q): Q) -> Res<Json<Value>> {
    let uid = a.user_id.ok_or_else(|| ApiError::unauthorized("a watchlist needs a user account (send an API token)"))?;
    let (limit, offset) = (lim(&q, "limit", 50, 500), off(&q));
    let raw = q.get("raw").is_some_and(|v| !matches!(v.as_str(), "0" | "false"));
    st.wiki
        .read(move |w, c| {
            let next = |n: usize| if n as i64 > limit { json!(offset + limit) } else { Value::Null };
            if raw {
                let rows = Wiki::watchlist_raw(c, uid, limit + 1, offset)?;
                let n = rows.len();
                Ok(Json(json!({ "pages": rows.iter().take(limit as usize).map(|t| title_obj(w, t, None)).collect::<Vec<_>>(), "next_offset": next(n) })))
            } else {
                let rows = Wiki::watchlist_changes(c, uid, limit + 1, offset)?;
                let n = rows.len();
                Ok(Json(json!({ "changes": rows.into_iter().take(limit as usize).collect::<Vec<_>>(), "next_offset": next(n) })))
            }
        })
        .await
}

async fn watchlist_put(State(st): State<AppState>, Extension(a): Extension<Actor>, Path(t): Path<String>) -> Res<Json<Value>> {
    let t = parse_title(&t)?;
    st.wiki.write(move |w, c| { w.watch(c, &a, &t, true)?; Ok(Json(json!({ "watched": t.full() }))) }).await
}

async fn watchlist_delete(State(st): State<AppState>, Extension(a): Extension<Actor>, Path(t): Path<String>) -> Res<Json<Value>> {
    let t = parse_title(&t)?;
    st.wiki.write(move |w, c| { w.watch(c, &a, &t, false)?; Ok(Json(json!({ "unwatched": t.full() }))) }).await
}

/// `POST /interwiki/{prefix}/purge` (sysop): forget cached remote templates so the next
/// render fetches them again.
async fn interwiki_purge(State(st): State<AppState>, Extension(a): Extension<Actor>, Path(prefix): Path<String>) -> Res<Json<Value>> {
    a.require("siteadmin")?;
    st.wiki.remote.purge();
    st.wiki.mark_dirty();
    Ok(Json(json!({ "purged": prefix })))
}

/// `POST /admin/reindex` (sysop): re-render every page and rebuild links, props and search.
async fn admin_reindex(State(st): State<AppState>, Extension(a): Extension<Actor>) -> Res<Json<Value>> {
    a.require("siteadmin")?;
    st.wiki.write(|w, c| Ok(Json(json!({ "reindexed": w.reindex_all(c)? })))).await
}

async fn search_title(State(st): State<AppState>, Query(q): Q) -> Res<Json<Value>> {
    let query = q.get("q").cloned().ok_or_else(|| ApiError::bad("missingparam", "the q parameter is required"))?;
    let limit = lim(&q, "limit", 10, 50);
    let ns = ns_param(&q)?.first().copied().unwrap_or(0);
    st.wiki
        .read(move |w, c| {
            let rows = queries::suggest_titles(c, &query, ns, limit)?;
            let pages: Vec<Value> = rows
                .iter()
                .map(|p| {
                    let mut v = title_obj(w, &p.title, None);
                    v["id"] = json!(p.id);
                    v["redirect"] = json!(p.is_redirect);
                    v
                })
                .collect();
            Ok(Json(json!({ "pages": pages })))
        })
        .await
}

async fn category_members(State(st): State<AppState>, Path(name): Path<String>, Query(q): Q) -> Res<Json<Value>> {
    let (limit, offset) = (lim(&q, "limit", 100, 500), off(&q));
    let kind = q.get("type").cloned().unwrap_or_else(|| "all".into());
    st.wiki
        .read(move |w, c| {
            let t = Title::parse_in(&name, NS_CATEGORY).map_err(ApiError::bad_title)?.title;
            let rows = queries::category_members(c, &t.name, &kind, limit, offset)?;
            let items: Vec<Value> = rows
                .iter()
                .map(|(p, key)| {
                    let mut v = title_obj(w, &p.title, None);
                    v["id"] = json!(p.id);
                    v["sortkey"] = json!(key);
                    v["type"] = json!(match p.title.ns { 14 => "subcat", 6 => "file", _ => "page" });
                    v
                })
                .collect();
            Ok(Json(json!({ "category": t.key(), "members": items })))
        })
        .await
}

pub fn change_json(ch: &queries::Change) -> Value {
    let mut v = json!({
        "type": ch.kind, "ns": ch.ns, "title": Title::new(ch.ns, &ch.title).full(), "timestamp": iso(ch.ts),
        "user": ch.user, "comment": ch.comment,
    });
    if ch.kind == "log" {
        v["logid"] = json!(ch.id);
        v["logtype"] = json!(ch.log_type);
        v["logaction"] = json!(ch.log_action);
    } else {
        v["pageid"] = json!(ch.page_id);
        v["revid"] = json!(ch.rev_id);
        v["old_revid"] = json!(ch.old_rev);
        v["oldlen"] = json!(ch.oldlen);
        v["newlen"] = json!(ch.newlen);
        v["minor"] = json!(ch.minor);
    }
    v
}

async fn recent_changes(State(st): State<AppState>, Query(q): Q) -> Res<Json<Value>> {
    let (limit, offset) = (lim(&q, "limit", 50, 500), off(&q));
    let ns = ns_param(&q)?;
    let types: Vec<String> = q.get("type").map(|t| t.split(['|', ',']).map(String::from).collect()).unwrap_or_default();
    let user = q.get("user").cloned();
    st.wiki
        .read(move |_, c| {
            let ch = queries::recent_changes(c, &ns, &types, user.as_deref(), limit, offset)?;
            Ok(Json(json!({ "changes": ch.iter().map(change_json).collect::<Vec<_>>() })))
        })
        .await
}

pub fn log_json(l: &queries::LogRow) -> Value {
    json!({ "logid": l.id, "type": l.log_type, "action": l.action, "ns": l.ns, "title": Title::new(l.ns, &l.title).full(),
            "user": l.user, "timestamp": iso(l.ts), "comment": l.comment, "params": l.params })
}

async fn logs(State(st): State<AppState>, Query(q): Q) -> Res<Json<Value>> {
    let (limit, offset) = (lim(&q, "limit", 50, 500), off(&q));
    let (typ, user, title) = (q.get("type").cloned(), q.get("user").cloned(), q.get("title").cloned());
    st.wiki
        .read(move |_, c| {
            let t = title.as_deref().map(parse_title).transpose()?;
            let rows = queries::log_events(c, typ.as_deref(), t.as_ref(), user.as_deref(), &[], limit, offset)?;
            Ok(Json(json!({ "events": rows.iter().map(log_json).collect::<Vec<_>>() })))
        })
        .await
}

fn user_json(u: &users::UserRow) -> Value {
    let mut groups = vec!["*".to_string(), "user".to_string()];
    groups.extend(u.groups.iter().cloned());
    json!({ "id": u.id, "name": u.name, "groups": groups, "editcount": u.editcount, "registration": iso(u.registered), "blocked": u.blocked })
}

async fn users_list(State(st): State<AppState>, Query(q): Q) -> Res<Json<Value>> {
    let (limit, offset) = (lim(&q, "limit", 50, 500), off(&q));
    let (prefix, group) = (q.get("prefix").cloned().unwrap_or_default(), q.get("group").cloned());
    st.wiki
        .read(move |_, c| Ok(Json(json!({ "users": users::list_users(c, &prefix, group.as_deref(), limit, offset)?.iter().map(user_json).collect::<Vec<_>>() }))))
        .await
}

async fn user_contribs(State(st): State<AppState>, Path(name): Path<String>, Query(q): Q) -> Res<Json<Value>> {
    let limit = lim(&q, "limit", 50, 500);
    let cursor = opt_id(&q, "older_than");
    st.wiki
        .read(move |_, c| {
            let rows = queries::contribs(c, &name.replace('_', " "), limit, cursor)?;
            let items: Vec<Value> = rows
                .iter()
                .map(|x| {
                    let mut v = rev_json(&x.rev);
                    v["title"] = json!(x.title.full());
                    v["ns"] = json!(x.title.ns);
                    v["delta"] = json!(x.rev.size - x.oldlen);
                    v["new"] = json!(x.rev.parent == 0);
                    v
                })
                .collect();
            let next = if rows.len() as i64 == limit { rows.last().map(|r| r.rev.id) } else { None };
            Ok(Json(json!({ "contributions": items, "next": next.map(|n| json!({ "older_than": n })) })))
        })
        .await
}

// ---------------------------------------------------------------------------------------
// page writes
// ---------------------------------------------------------------------------------------

fn edit_json(r: &wiki::EditResult) -> Value {
    json!({
        "id": r.page_id, "key": r.title.key(), "title": r.title.full(),
        "latest": { "id": r.rev_id, "timestamp": iso(r.ts) },
        "created": r.new, "nochange": r.nochange, "merged": r.merged, "size": r.size,
        "content_model": "wikitext",
    })
}

async fn save(st: AppState, a: Actor, title: String, body: Bytes, create_only: bool) -> Res<Response> {
    let v = body_json(&body)?;
    let source = s(&v, "source").ok_or_else(|| ApiError::bad("missingparam", "the JSON body needs a string 'source'"))?.to_string();
    let t = parse_title(&title)?;
    let req = EditReq {
        title: t,
        text: source,
        summary: s(&v, "comment").unwrap_or("").to_string(),
        minor: v.get("minor").and_then(Value::as_bool).unwrap_or(false),
        base_rev: v.get("latest").and_then(|l| l.get("id")).and_then(Value::as_i64),
        create_only,
        no_create: false,
    };
    let res = st.wiki.write(move |w, c| w.edit(c, &a, req)).await?;
    if !res.nochange {
        st.metrics.edits.fetch_add(1, Ordering::Relaxed);
    }
    let status = if res.new { StatusCode::CREATED } else { StatusCode::OK };
    Ok((status, Json(edit_json(&res))).into_response())
}

async fn page_create(State(st): State<AppState>, Extension(a): Extension<Actor>, Path(t): Path<String>, body: Bytes) -> Res<Response> {
    save(st, a, t, body, true).await
}
async fn page_put(State(st): State<AppState>, Extension(a): Extension<Actor>, Path(t): Path<String>, body: Bytes) -> Res<Response> {
    save(st, a, t, body, false).await
}

async fn page_delete(State(st): State<AppState>, Extension(a): Extension<Actor>, Path(t): Path<String>, Query(q): Q, body: Bytes) -> Res<Json<Value>> {
    let reason = comment(&body_json(&body)?, &q);
    let title = parse_title(&t)?;
    st.wiki
        .write(move |w, c| {
            let id = w.delete_page(c, &a, &title, &reason)?;
            Ok(Json(json!({ "deleted": true, "id": id, "key": title.key() })))
        })
        .await
}

async fn page_move(State(st): State<AppState>, Extension(a): Extension<Actor>, Path(t): Path<String>, body: Bytes) -> Res<Json<Value>> {
    let v = body_json(&body)?;
    let to = parse_title(s(&v, "target").ok_or_else(|| ApiError::bad("missingparam", "'target' is required"))?)?;
    let from = parse_title(&t)?;
    let noredirect = v.get("noredirect").and_then(Value::as_bool).unwrap_or(false);
    let reason = comment(&v, &HashMap::new());
    st.wiki
        .write(move |w, c| {
            let p = w.move_page(c, &a, &from, &to, &reason, noredirect)?;
            Ok(Json(json!({ "moved": true, "from": from.key(), "to": p.title.key(), "id": p.id, "redirect_created": !noredirect })))
        })
        .await
}

async fn page_undelete(State(st): State<AppState>, Extension(a): Extension<Actor>, Path(t): Path<String>, body: Bytes) -> Res<Json<Value>> {
    let reason = comment(&body_json(&body)?, &HashMap::new());
    let title = parse_title(&t)?;
    st.wiki.write(move |w, c| Ok(Json(json!({ "restored": true, "id": w.undelete_page(c, &a, &title, &reason)?.id, "key": title.key() })))).await
}

async fn page_protect(State(st): State<AppState>, Extension(a): Extension<Actor>, Path(t): Path<String>, body: Bytes) -> Res<Json<Value>> {
    let v = body_json(&body)?;
    let (e, m) = (s(&v, "edit").map(String::from), s(&v, "move").map(String::from));
    let reason = comment(&v, &HashMap::new());
    let title = parse_title(&t)?;
    st.wiki
        .write(move |w, c| {
            let p = w.protect(c, &a, &title, e.as_deref(), m.as_deref(), &reason)?;
            Ok(Json(json!({ "protection": protection_json(&p), "key": title.key() })))
        })
        .await
}

async fn page_rollback(State(st): State<AppState>, Extension(a): Extension<Actor>, Path(t): Path<String>, body: Bytes) -> Res<Json<Value>> {
    let v = body_json(&body)?;
    let user = s(&v, "user").map(String::from);
    let reason = comment(&v, &HashMap::new());
    let title = parse_title(&t)?;
    let (res, undone) = st.wiki.write(move |w, c| w.rollback(c, &a, &title, user.as_deref(), &reason)).await?;
    let mut j = edit_json(&res);
    j["reverted_user"] = json!(undone);
    Ok(Json(j))
}

async fn transform_html_to_wikitext(State(st): State<AppState>, body: Bytes) -> Res<Json<Value>> {
    let v = body_json(&body)?;
    let html = s(&v, "html").ok_or_else(|| ApiError::bad("missingparam", "'html' is required"))?;
    if html.len() > st.cfg.max_page_bytes {
        return Err(ApiError::bad("contenttoobig", "html is too large"));
    }
    let cx = crate::htmlwt::Ctx { article_path: &st.cfg.article_path, file_path: &st.cfg.file_path };
    Ok(Json(json!({ "wikitext": crate::htmlwt::html_to_wikitext(html, &cx) })))
}

async fn transform_expand(State(st): State<AppState>, body: Bytes) -> Res<Json<Value>> {
    let v = body_json(&body)?;
    let text = s(&v, "wikitext").ok_or_else(|| ApiError::bad("missingparam", "'wikitext' is required"))?.to_string();
    let title = s(&v, "title").unwrap_or("API").to_string();
    st.wiki
        .read(move |w, c| {
            let t = parse_title(&title)?;
            let text = w.clean_text(&text)?;
            Ok(Json(json!({ "wikitext": w.expand_text(c, &t, &text) })))
        })
        .await
}

fn upload_opts(q: &HashMap<String, String>) -> (String, Option<String>) {
    (q.get("comment").cloned().unwrap_or_default(), q.get("description").cloned())
}

async fn do_file_upload(st: AppState, a: Actor, name: String, q: HashMap<String, String>, body: Bytes, create_only: bool) -> Res<Response> {
    let (comment, desc) = upload_opts(&q);
    let res = st.wiki.write(move |w, c| w.upload(c, &a, &name, &body, &comment, desc.as_deref(), create_only).map(|r| { let i = crate::files::info_json(w, &r.file); (r, i) })).await?;
    let (r, mut j) = res;
    j["created"] = json!(r.new);
    j["nochange"] = json!(r.nochange);
    Ok((if r.new { StatusCode::CREATED } else { StatusCode::OK }, Json(j)).into_response())
}

async fn file_put(State(st): State<AppState>, Extension(a): Extension<Actor>, Path(name): Path<String>, Query(q): Q, body: Bytes) -> Res<Response> {
    do_file_upload(st, a, name, q, body, false).await
}
async fn file_post(State(st): State<AppState>, Extension(a): Extension<Actor>, Path(name): Path<String>, Query(q): Q, body: Bytes) -> Res<Response> {
    do_file_upload(st, a, name, q, body, true).await
}

fn dim_param(q: &HashMap<String, String>, key: &str) -> Res<Option<u32>> {
    match q.get(key) {
        None => Ok(None),
        Some(v) => v.trim_end_matches("px").parse::<u32>().ok().filter(|n| (1..=10_000).contains(n)).map(Some).ok_or_else(|| ApiError::bad("badvalue", format!("{key} must be a number of pixels between 1 and 10000"))),
    }
}

/// `GET /file/{name}[?version=N][&width=W][&height=H]`: the original, or a scaled-down rendition.
async fn file_get(State(st): State<AppState>, Path(name): Path<String>, Query(q): Q, h: HeaderMap) -> Res<Response> {
    let ver = opt_id(&q, "version");
    let inm = inm(&h);
    let (mut want_w, want_h) = (dim_param(&q, "width")?, dim_param(&q, "height")?);
    // ?page=N (PDF) and ?time=S (video, seconds) choose what the preview shows.
    let page = match q.get("page") {
        None => 0,
        Some(v) => v.parse::<u32>().ok().filter(|p| (1..=100_000).contains(p)).ok_or_else(|| ApiError::bad("badvalue", "page must be a page number from 1"))?,
    };
    let time_ds = match q.get("time") {
        None => None,
        Some(v) => Some(v.parse::<f64>().ok().filter(|t| t.is_finite() && (0.0..=1e7).contains(t)).map(|t| (t * 10.0).round() as u32).ok_or_else(|| ApiError::bad("badvalue", "time must be a number of seconds"))?),
    };
    let var = crate::files::Variant { page, time_ds };
    if (page > 0 || time_ds.is_some()) && want_w.is_none() && want_h.is_none() {
        want_w = Some(640);
    }
    let wiki = st.wiki.clone();
    st.wiki
        .read(move |_, c| {
            let t = crate::files::file_title(&name)?;
            crate::files::file_data(c, &t.name, ver)
        })
        .await
        .and_then(|fd| {
            let size = crate::files::rendition(&fd.mime, fd.width, fd.height, want_w, want_h);
            if (want_w.is_some() || want_h.is_some()) && !crate::files::can_thumbnail(&fd.mime) {
                let why = match crate::media::kind(&fd.mime) {
                    Some(crate::media::Kind::Pdf) => "PDF previews need Poppler (pdftoppm) on the server".to_string(),
                    Some(_) => "video and audio previews need ffmpeg on the server".to_string(),
                    None => format!("{} files cannot be scaled", fd.mime),
                };
                return Err(ApiError::bad("nothumbnail", why));
            }
            Ok((fd, size))
        })
        .map(|(fd, size)| async move {
            let tag = match size {
                Some((w, _)) if var.key() != 0 => format!("\"{}-{w}-{}\"", fd.sha256, var.key()),
                Some((w, _)) => format!("\"{}-{w}\"", fd.sha256),
                None => format!("\"{}\"", fd.sha256),
            };
            if inm.as_deref() == Some(tag.as_str()) {
                return Ok(finish(tag, None));
            }
            let svg = fd.mime == "image/svg+xml" && size.is_none();
            let (mime, body) = match size {
                Some((w, h)) => {
                    let t = tokio::task::spawn_blocking(move || wiki.thumbnail_of(&fd, w, h, var)).await.map_err(ApiError::internal)??;
                    (t.0.to_string(), t.1.clone())
                }
                None => (fd.mime, fd.data),
            };
            let mut resp = (StatusCode::OK, body).into_response();
            let hd = resp.headers_mut();
            hd.insert(header::CONTENT_TYPE, HeaderValue::from_str(&mime).map_err(ApiError::internal)?);
            hd.insert(header::ETAG, HeaderValue::from_str(&tag).map_err(ApiError::internal)?);
            hd.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
            // Uploaded files can never run scripts in a browser, whatever they contain.
            hd.insert(
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(if svg { "default-src 'none'; style-src 'unsafe-inline'; img-src data:; sandbox" } else { "default-src 'none'; sandbox" }),
            );
            if ver.is_some() {
                hd.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=31536000, immutable"));
            }
            Ok(resp)
        })?
        .await
}

async fn file_info(State(st): State<AppState>, Path(name): Path<String>) -> Res<Json<Value>> {
    st.wiki
        .read(move |w, c| {
            let t = crate::files::file_title(&name)?;
            let f = crate::files::find_file(c, &t.name)?.ok_or_else(|| ApiError::not_found("missingfile", "the file does not exist"))?;
            Ok(Json(crate::files::info_json(w, &f)))
        })
        .await
}

async fn file_versions(State(st): State<AppState>, Path(name): Path<String>, Query(q): Q) -> Res<Json<Value>> {
    let limit = lim(&q, "limit", 50, 500);
    st.wiki
        .read(move |_, c| {
            let t = crate::files::file_title(&name)?;
            let f = crate::files::find_file(c, &t.name)?.ok_or_else(|| ApiError::not_found("missingfile", "the file does not exist"))?;
            let v: Vec<Value> = crate::files::file_history(c, f.id, limit)?.iter().map(crate::files::version_json).collect();
            Ok(Json(json!({ "name": f.name, "versions": v })))
        })
        .await
}

async fn file_delete(State(st): State<AppState>, Extension(a): Extension<Actor>, Path(name): Path<String>, Query(q): Q) -> Res<Json<Value>> {
    let reason = q.get("comment").cloned().unwrap_or_default();
    st.wiki.write(move |w, c| { w.delete_file(c, &a, &name, &reason)?; Ok(Json(json!({ "deleted": true }))) }).await
}

async fn files_list(State(st): State<AppState>, Query(q): Q) -> Res<Json<Value>> {
    let limit = lim(&q, "limit", 50, 500);
    let (prefix, from, mime) = (q.get("prefix").cloned().unwrap_or_default(), q.get("from").cloned().unwrap_or_default().replace('_', " "), q.get("mime").cloned());
    st.wiki
        .read(move |w, c| {
            let mut rows = crate::files::all_files(c, &prefix, &from, mime.as_deref(), limit + 1)?;
            let next = if rows.len() as i64 > limit { rows.pop().map(|f| f.name) } else { None };
            Ok(Json(json!({ "files": rows.iter().map(|f| crate::files::info_json(w, f)).collect::<Vec<_>>(), "next": next })))
        })
        .await
}

async fn interwiki_list(State(st): State<AppState>) -> Res<Json<Value>> {
    st.wiki
        .read(|_, c| {
            let mut stmt = c.prepare("SELECT prefix,url,api,trans,lang FROM interwiki ORDER BY prefix")?;
            let rows = stmt
                .query_map([], |r| Ok(json!({ "prefix": r.get::<_, String>(0)?, "url": r.get::<_, String>(1)?, "api": r.get::<_, String>(2)?, "trans": r.get::<_, i64>(3)? != 0, "language": r.get::<_, i64>(4)? != 0 })))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Json(json!({ "interwiki": rows })))
        })
        .await
}

async fn interwiki_put(State(st): State<AppState>, Extension(a): Extension<Actor>, Path(prefix): Path<String>, body: Bytes) -> Res<Json<Value>> {
    let v = body_json(&body)?;
    let url = s(&v, "url").ok_or_else(|| ApiError::bad("missingparam", "'url' (containing $1) is required"))?.to_string();
    let api = s(&v, "api").unwrap_or("").to_string();
    let trans = v.get("trans").and_then(Value::as_bool).unwrap_or(false);
    let lang = v.get("language").and_then(Value::as_bool).unwrap_or(false);
    st.wiki
        .write(move |w, c| {
            w.set_interwiki(c, &a, &prefix, Some(&url), &api, trans, lang)?;
            Ok(Json(json!({ "prefix": prefix.trim().to_lowercase(), "url": url, "api": api, "trans": trans, "language": lang })))
        })
        .await
}

async fn interwiki_delete(State(st): State<AppState>, Extension(a): Extension<Actor>, Path(prefix): Path<String>) -> Res<Json<Value>> {
    st.wiki.write(move |w, c| { w.set_interwiki(c, &a, &prefix, None, "", false, false)?; Ok(Json(json!({ "deleted": prefix }))) }).await
}

async fn transform_html(State(st): State<AppState>, body: Bytes) -> Res<Json<Value>> {
    let v = body_json(&body)?;
    let text = s(&v, "wikitext").ok_or_else(|| ApiError::bad("missingparam", "'wikitext' is required"))?.to_string();
    let title = s(&v, "title").unwrap_or("API").to_string();
    st.wiki
        .read(move |w, c| {
            let t = parse_title(&title)?;
            let text = w.clean_text(&text)?;
            let r = w.render_text(c, &t, &text);
            Ok(Json(json!({
                "html": r.html,
                "extract": r.extract,
                "categories": r.categories.iter().map(|(c, k)| json!({ "title": c, "sortkey": k })).collect::<Vec<_>>(),
                "links": r.links.iter().map(|t| t.key()).collect::<Vec<_>>(),
                "templates": r.templates.iter().map(|t| t.key()).collect::<Vec<_>>(),
                "sections": r.sections,
            })))
        })
        .await
}

// ---------------------------------------------------------------------------------------
// users
// ---------------------------------------------------------------------------------------

fn str_list(v: &Value, k: &str) -> Vec<String> {
    v.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect()).unwrap_or_default()
}

async fn page_undo(State(st): State<AppState>, Extension(a): Extension<Actor>, Path(t): Path<String>, body: Bytes) -> Res<Json<Value>> {
    let v = body_json(&body)?;
    let rev = v.get("revision").and_then(Value::as_i64).ok_or_else(|| ApiError::bad("missingparam", "'revision' (integer) is required"))?;
    let after = v.get("after").and_then(Value::as_i64);
    let reason = comment(&v, &HashMap::new());
    let title = parse_title(&t)?;
    let res = st.wiki.write(move |w, c| w.undo(c, &a, &title, rev, after, &reason)).await?;
    st.metrics.edits.fetch_add(1, Ordering::Relaxed);
    Ok(Json(edit_json(&res)))
}

async fn page_restore(State(st): State<AppState>, Extension(a): Extension<Actor>, Path(t): Path<String>, body: Bytes) -> Res<Json<Value>> {
    let v = body_json(&body)?;
    let rev = v.get("revision").and_then(Value::as_i64).ok_or_else(|| ApiError::bad("missingparam", "'revision' (integer) is required"))?;
    let reason = comment(&v, &HashMap::new());
    let title = parse_title(&t)?;
    let res = st.wiki.write(move |w, c| w.restore(c, &a, &title, rev, &reason)).await?;
    st.metrics.edits.fetch_add(1, Ordering::Relaxed);
    Ok(Json(edit_json(&res)))
}

async fn user_create(State(st): State<AppState>, Extension(a): Extension<Actor>, body: Bytes) -> Res<Response> {
    a.require("createuser")?;
    let v = body_json(&body)?;
    let name = s(&v, "name").ok_or_else(|| ApiError::bad("missingparam", "'name' is required"))?.to_string();
    let groups = str_list(&v, "groups");
    let (u, token) = st.wiki.write(move |_, c| users::create_user(c, &name, &groups)).await?;
    let mut j = user_json(&u);
    j["token"] = json!(token);
    j["note"] = json!("this token is shown once; store it now");
    Ok((StatusCode::CREATED, Json(j)).into_response())
}

async fn user_groups(State(st): State<AppState>, Extension(a): Extension<Actor>, Path(name): Path<String>, body: Bytes) -> Res<Json<Value>> {
    a.require("userrights")?;
    let v = body_json(&body)?;
    let (add, remove) = (str_list(&v, "add"), str_list(&v, "remove"));
    let u = st.wiki.write(move |_, c| users::set_groups(c, &name, &add, &remove)).await?;
    Ok(Json(user_json(&u)))
}

async fn user_block(State(st): State<AppState>, Extension(a): Extension<Actor>, Path(name): Path<String>, body: Bytes) -> Res<Json<Value>> {
    a.require("block")?;
    let reason = comment(&body_json(&body)?, &HashMap::new());
    let who = a.name.clone();
    let u = st
        .wiki
        .write(move |_, c| {
            let u = users::set_block(c, &name, true, &reason)?;
            wiki::log_event(c, "block", "block", &who, None, &reason, json!({ "target": u.name }))?;
            Ok(u)
        })
        .await?;
    Ok(Json(user_json(&u)))
}

async fn user_unblock(State(st): State<AppState>, Extension(a): Extension<Actor>, Path(name): Path<String>) -> Res<Json<Value>> {
    a.require("block")?;
    let who = a.name.clone();
    let u = st
        .wiki
        .write(move |_, c| {
            let u = users::set_block(c, &name, false, "")?;
            wiki::log_event(c, "block", "unblock", &who, None, "", json!({ "target": u.name }))?;
            Ok(u)
        })
        .await?;
    Ok(Json(user_json(&u)))
}

#[allow(dead_code)]
fn _ns(n: i64) -> &'static str {
    ns_name(n)
}
