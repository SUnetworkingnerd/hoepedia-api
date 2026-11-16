//! `/api.php` — an Action-API-style endpoint (`?action=query&list=search&srsearch=...`).
//!
//! Parameter names and response shapes follow MediaWiki's `formatversion=2` JSON closely, so
//! existing client code usually works, but this is a native implementation: it reads and
//! writes this engine's own database and nothing is proxied.
//! Differences: JSON only; bearer-token auth instead of login + CSRF (`meta=tokens` returns a
//! dummy `+\` token for client compatibility); continuation uses `<prefix>offset` / `rvcontinue`.

use std::collections::{HashMap, HashSet};

use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{header, HeaderMap, Method},
    Extension, Json,
};
use rusqlite::Connection;
use serde_json::{json, Map, Value};

use crate::{
    error::{ApiError, Res},
    present::*,
    queries,
    rest::{actor_json, change_json, compare_json, log_json},
    state::AppState,
    title::{Title, NAMESPACES, NS_CATEGORY},
    users::{self, Actor},
    util::iso,
    wiki::{self, find_page, find_page_by_id, follow_redirects, EditReq, PageRow, Wiki},
    wikitext,
};

pub struct Params(HashMap<String, String>, Option<Vec<u8>>);

impl Params {
    fn get(&self, k: &str) -> Option<&str> {
        self.0.get(k).map(String::as_str)
    }
    fn req(&self, k: &str) -> Res<&str> {
        self.get(k).ok_or_else(|| ApiError::bad("missingparam", format!("the '{k}' parameter must be set")))
    }
    fn flag(&self, k: &str) -> bool {
        self.0.get(k).is_some_and(|v| !matches!(v.as_str(), "false" | "0" | "no"))
    }
    fn list(&self, k: &str) -> Vec<String> {
        self.get(k).map(|v| v.split('|').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()).unwrap_or_default()
    }
    fn int(&self, k: &str) -> Option<i64> {
        self.get(k).and_then(|v| v.parse().ok())
    }
    fn limit(&self, k: &str, default: i64, max: i64) -> i64 {
        match self.get(k) {
            Some("max") => max,
            Some(v) => v.parse().unwrap_or(default).clamp(1, max),
            None => default,
        }
    }
    fn offset(&self, prefix: &str) -> i64 {
        self.int(&format!("{prefix}offset")).unwrap_or(0).clamp(0, 100_000)
    }
    fn ns(&self, k: &str, default: &[i64]) -> Res<Vec<i64>> {
        match self.get(k) {
            Some(v) => parse_ns_list(v),
            None => Ok(default.to_vec()),
        }
    }
}

pub const WRITE_ACTIONS: &[&str] =
    &["watch", "edit", "upload", "move", "delete", "undelete", "protect", "rollback", "createuser", "userrights", "block", "unblock", "rotatetoken"];

pub async fn api(
    State(st): State<AppState>,
    Extension(actor): Extension<Actor>,
    method: Method,
    headers: HeaderMap,
    Query(qs): Query<HashMap<String, String>>,
    body: Bytes,
) -> Res<Json<Value>> {
    let mut map = qs;
    let mut file: Option<Vec<u8>> = None;
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    if method == Method::POST && !body.is_empty() && ct.starts_with("multipart/form-data") {
        let boundary = multer::parse_boundary(&ct).map_err(|e| ApiError::bad("badmultipart", e.to_string()))?;
        let stream = futures_util::stream::once(async move { Ok::<Bytes, std::convert::Infallible>(body.clone()) });
        let mut mp = multer::Multipart::new(stream, boundary);
        while let Some(field) = mp.next_field().await.map_err(|e| ApiError::bad("badmultipart", e.to_string()))? {
            let name = field.name().unwrap_or("").to_string();
            let bytes = field.bytes().await.map_err(|e| ApiError::bad("badmultipart", e.to_string()))?;
            if name == "file" {
                file = Some(bytes.to_vec());
            } else if !name.is_empty() {
                map.insert(name, String::from_utf8_lossy(&bytes).into_owned());
            }
        }
    } else if method == Method::POST && !body.is_empty() {
        let is_json = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.contains("json"));
        if is_json {
            match serde_json::from_slice::<Value>(&body) {
                Ok(Value::Object(o)) => {
                    for (k, v) in o {
                        let s = match v {
                            Value::String(s) => s,
                            Value::Bool(true) => "1".into(),
                            Value::Bool(false) => continue,
                            Value::Array(a) => a.iter().map(|x| x.as_str().map(String::from).unwrap_or_else(|| x.to_string())).collect::<Vec<_>>().join("|"),
                            other => other.to_string(),
                        };
                        map.insert(k, s);
                    }
                }
                _ => return Err(ApiError::bad("badjson", "JSON body must be an object")),
            }
        } else {
            let form: HashMap<String, String> = serde_urlencoded::from_bytes(&body).map_err(|e| ApiError::bad("badform", e.to_string()))?;
            map.extend(form);
        }
    }
    let p = Params(map, file);
    let action = p.get("action").unwrap_or("help").to_string();
    if WRITE_ACTIONS.contains(&action.as_str()) && method != Method::POST {
        return Err(ApiError::new(axum::http::StatusCode::METHOD_NOT_ALLOWED, "mustbeposted", format!("the '{action}' action requires a POST request")));
    }
    let w = &st.wiki;
    let v = match action.as_str() {
        "help" => help(),
        "query" => {
            // Remote wikis are asked after the database work, never while holding a connection.
            let federate = p.flag("srinterwiki") && p.list("list").iter().any(|l| l == "search");
            let (query, limit) = (p.get("srsearch").unwrap_or("").to_string(), p.limit("srlimit", 10, 50));
            let (mut v, targets) = w
                .read(move |w, c| {
                    let targets = if federate { Wiki::search_targets(c)? } else { Vec::new() };
                    Ok((do_query(w, c, &actor, &p)?, targets))
                })
                .await?;
            if federate && !targets.is_empty() {
                let wk = w.clone();
                v["query"]["interwikisearch"] = tokio::task::spawn_blocking(move || wk.federated_search(&targets, &query, limit)).await.map_err(ApiError::internal)?;
            }
            v
        }
        "parse" => w.read(move |w, c| do_parse(w, c, &p)).await?,
        "expandtemplates" => w.read(move |w, c| {
            let t = parse_title(p.get("title").unwrap_or("API"))?;
            let text = w.clean_text(p.req("text")?)?;
            Ok(json!({ "expandtemplates": { "wikitext": w.expand_text(c, &t, &text) } }))
        }).await?,
        "opensearch" => w.read(move |w, c| do_opensearch(w, c, &p)).await?,
        "compare" => w.read(move |_, c| do_compare(c, &p)).await?,
        "edit" => {
            let r = w.write(move |w, c| do_edit(w, c, &actor, &p)).await?;
            if r["edit"]["nochange"].is_null() {
                st.metrics.edits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            r
        }
        "upload" => {
            let name = p.req("filename")?.to_string();
            let data = match (&p.1, p.get("filedata")) {
                (Some(f), _) => f.clone(),
                (None, Some(b64)) => {
                    use base64::Engine;
                    base64::engine::general_purpose::STANDARD.decode(b64.trim()).map_err(|_| ApiError::bad("badbase64", "filedata is not valid base64"))?
                }
                _ => return Err(ApiError::bad("missingparam", "send the file as a multipart field named 'file' or as base64 in 'filedata'")),
            };
            let (comment, text, create_only) = (p.get("comment").unwrap_or("").to_string(), p.get("text").map(String::from), p.flag("createonly"));
            let (r, info) = w.write(move |w, c| w.upload(c, &actor, &name, &data, &comment, text.as_deref(), create_only).map(|r| { let i = crate::files::info_json(w, &r.file); (r, i) })).await?;
            let mut u = json!({ "result": "Success", "filename": r.file.name, "imageinfo": info });
            if r.nochange {
                u["nochange"] = json!(true);
            }
            json!({ "upload": u })
        }
        "move" => w.write(move |w, c| do_move(w, c, &actor, &p)).await?,
        "delete" => w.write(move |w, c| do_delete(w, c, &actor, &p)).await?,
        "undelete" => w.write(move |w, c| do_undelete(w, c, &actor, &p)).await?,
        "protect" => w.write(move |w, c| do_protect(w, c, &actor, &p)).await?,
        "rollback" => w.write(move |w, c| do_rollback(w, c, &actor, &p)).await?,
        "createuser" => {
            actor.require("createuser")?;
            let (name, groups) = (p.req("name")?.to_string(), p.list("groups"));
            let (u, token) = w.write(move |_, c| users::create_user(c, &name, &groups)).await?;
            json!({ "createuser": { "name": u.name, "userid": u.id, "groups": u.groups, "token": token, "note": "shown once" } })
        }
        "userrights" => {
            actor.require("userrights")?;
            let (user, add, remove) = (p.req("user")?.to_string(), p.list("add"), p.list("remove"));
            let u = w.write(move |_, c| users::set_groups(c, &user, &add, &remove)).await?;
            json!({ "userrights": { "user": u.name, "userid": u.id, "groups": u.groups } })
        }
        "watch" => {
            let titles = p.list("titles");
            let on = !p.flag("unwatch");
            if titles.is_empty() {
                return Err(ApiError::bad("missingparam", "the titles parameter is required"));
            }
            w.write(move |w, c| {
                let mut out = Vec::new();
                for raw in titles.iter().take(500) {
                    let t = parse_title(raw)?;
                    w.watch(c, &actor, &t, on)?;
                    out.push(json!({ "title": t.full(), (if on { "watched" } else { "unwatched" }): true }));
                }
                Ok(json!({ "watch": out }))
            })
            .await?
        }
        "block" | "unblock" => {
            actor.require("block")?;
            let (user, reason, block, who) = (p.req("user")?.to_string(), p.get("reason").unwrap_or("").to_string(), action == "block", actor.name.clone());
            let act = action.clone();
            let u = w
                .write(move |_, c| {
                    let u = users::set_block(c, &user, block, &reason)?;
                    wiki::log_event(c, "block", &act, &who, None, &reason, json!({ "target": u.name }))?;
                    Ok(u)
                })
                .await?;
            json!({ action.clone(): { "user": u.name, "blocked": u.blocked, "reason": u.block_reason } })
        }
        "rotatetoken" => {
            let Some(uid) = actor.user_id else { return Err(ApiError::unauthorized("log in with a token first")) };
            let token = w.write(move |_, c| users::rotate_token(c, uid)).await?;
            json!({ "rotatetoken": { "token": token } })
        }
        "login" | "clientlogin" | "logout" => {
            return Err(ApiError::bad("nologin", "this engine uses bearer tokens: send 'Authorization: Bearer <token>'; there is no login action"))
        }
        other => return Err(ApiError::bad("badvalue", format!("unrecognized action '{other}'; see action=help"))),
    };
    Ok(Json(v))
}

fn help() -> Value {
    json!({
        "help": {
            "info": "hoepedia-api Action-style API. Read: GET. Write actions: POST (form or JSON). Auth: Authorization: Bearer <token>.",
            "actions": {
                "query": "prop=info|revisions|categories|links|templates|linkshere|extracts|pageprops; list=search|prefixsearch|allpages|allcategories|categorymembers|backlinks|embeddedin|recentchanges|logevents|random|allusers|usercontribs; meta=siteinfo|userinfo|tokens",
                "parse": "page=|oldid=|text= (+title=) -> rendered HTML, links, categories, sections",
                "edit": "title=, text= | appendtext= | prependtext=, section=N|new, summary=, minor, createonly, nocreate, baserevid=",
                "move": "from=, to=, reason=, noredirect", "delete": "title=, reason=", "undelete": "title=, reason=",
                "protect": "title=, protections=edit=sysop|move=user, reason=", "rollback": "title=, user=, summary=",
                "opensearch": "search=, limit=, namespace=", "compare": "fromrev=|fromtitle=, torev=|totitle=",
                "createuser": "name=, groups= (sysop only)", "userrights": "user=, add=, remove=", "block": "user=, reason=", "unblock": "user=",
                "rotatetoken": "replace your own token"
            }
        }
    })
}

// ---------------------------------------------------------------------------------------
// page set resolution
// ---------------------------------------------------------------------------------------

enum Entry {
    Live(PageRow),
    Missing(Title),
    MissingId(i64),
    MissingRev(i64),
    Invalid(String, String),
}

fn base(e: &Entry) -> Value {
    match e {
        Entry::Live(p) => json!({ "pageid": p.id, "ns": p.title.ns, "title": p.title.full() }),
        Entry::Missing(t) => json!({ "ns": t.ns, "title": t.full(), "missing": true }),
        Entry::MissingId(i) => json!({ "pageid": i, "missing": true }),
        Entry::MissingRev(i) => json!({ "revid": i, "missing": true }),
        Entry::Invalid(raw, why) => json!({ "title": raw, "invalid": true, "invalidreason": why }),
    }
}

fn resolve_pages(c: &Connection, p: &Params, q: &mut Map<String, Value>) -> Res<Vec<(Entry, Option<i64>)>> {
    let mut out: Vec<(Entry, Option<i64>)> = Vec::new();
    let (mut normalized, mut redirects) = (Vec::new(), Vec::new());
    for raw in p.list("titles").into_iter().take(500) {
        match Title::parse(&raw) {
            Ok(parsed) => {
                let t = parsed.title;
                if raw != t.full() {
                    normalized.push(json!({ "fromencoded": false, "from": raw, "to": t.full() }));
                }
                out.push((find_page(c, &t)?.map_or(Entry::Missing(t), Entry::Live), None));
            }
            Err(why) => out.push((Entry::Invalid(raw, why), None)),
        }
    }
    for id in p.list("pageids").iter().take(500) {
        let id: i64 = id.parse().map_err(|_| ApiError::bad("badvalue", "pageids must be integers"))?;
        out.push((find_page_by_id(c, id)?.map_or(Entry::MissingId(id), Entry::Live), None));
    }
    for id in p.list("revids").iter().take(500) {
        let id: i64 = id.parse().map_err(|_| ApiError::bad("badvalue", "revids must be integers"))?;
        out.push(match wiki::rev_with_page(c, id) {
            Ok((_, pg)) => (Entry::Live(pg), Some(id)),
            Err(_) => (Entry::MissingRev(id), None),
        });
    }
    if p.flag("redirects") {
        for (e, _) in out.iter_mut() {
            if let Entry::Live(pg) = e {
                if pg.is_redirect {
                    let (fin, from) = follow_redirects(c, pg.clone())?;
                    if let Some(f) = from {
                        redirects.push(json!({ "from": f.full(), "to": fin.title.full() }));
                    }
                    *pg = fin;
                }
            }
        }
    }
    if !normalized.is_empty() {
        q.insert("normalized".into(), Value::Array(normalized));
    }
    if !redirects.is_empty() {
        q.insert("redirects".into(), Value::Array(redirects));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------
// action=query
// ---------------------------------------------------------------------------------------

type Cont = Map<String, Value>;

/// `generator=<list>`: run the list module with `g`-prefixed parameters and feed its titles into `prop=`.
fn apply_generator(w: &Wiki, c: &Connection, actor: &Actor, p: &Params, cont: &mut Cont) -> Res<Option<Params>> {
    let Some(gen) = p.get("generator") else { return Ok(None) };
    let (prefix, key) = match gen {
        "allpages" => ("ap", "allpages"),
        "categorymembers" => ("cm", "categorymembers"),
        "search" => ("sr", "search"),
        "backlinks" => ("bl", "backlinks"),
        "embeddedin" => ("ei", "embeddedin"),
        "prefixsearch" => ("ps", "prefixsearch"),
        "random" => ("rn", "random"),
        "recentchanges" => ("rc", "recentchanges"),
        "usercontribs" => ("uc", "usercontribs"),
        "allcategories" => ("ac", "allcategories"),
        "allimages" => ("ai", "allimages"),
        "imageusage" => ("iu", "imageusage"),
        "exturlusage" => ("eu", "exturlusage"),
        "iwbacklinks" => ("iwbl", "iwbacklinks"),
        "pageswithprop" => ("pwp", "pageswithprop"),
        "logevents" => ("le", "logevents"),
        "watchlist" => ("wl", "watchlist"),
        "watchlistraw" => ("wr", "watchlistraw"),
        "allredirects" => ("ar", "allredirects"),
        "alllinks" => ("al", "alllinks"),
        "alltransclusions" => ("at", "alltransclusions"),
        "allfileusages" => ("af", "allfileusages"),
        "allrevisions" => ("arv", "allrevisions"),
        "alldeletedrevisions" => ("adr", "alldeletedrevisions"),
        "querypage" => ("qp", "querypage"),
        // generators that start from the given titles/pageids
        "links" => ("pl", "links"),
        "templates" => ("tl", "templates"),
        "categories" => ("cl", "categories"),
        "images" => ("im", "images"),
        "linkshere" => ("lh", "linkshere"),
        "transcludedin" => ("ti", "transcludedin"),
        "redirects" => ("rd", "redirects"),
        "fileusage" => ("fu", "fileusage"),
        other => return Err(ApiError::bad("badvalue", format!("unsupported generator '{other}'"))),
    };
    let from_pages = matches!(gen, "links" | "templates" | "categories" | "images" | "linkshere" | "transcludedin" | "redirects" | "fileusage");
    let mut sub = HashMap::new();
    for (k, v) in &p.0 {
        if let Some(rest) = k.strip_prefix('g') {
            if rest.starts_with(prefix) {
                sub.insert(rest.to_string(), v.clone());
            }
        }
    }
    let mut tmp = Map::new();
    let mut gcont = Cont::new();
    let sub = Params(sub, None);
    let mut titles: Vec<String> = Vec::new();
    let mut add = |v: Option<&Value>| {
        for item in v.and_then(Value::as_array).into_iter().flatten() {
            if let Some(t) = item.get("title").and_then(Value::as_str) {
                if titles.len() < 500 && !titles.iter().any(|x| x == t) {
                    titles.push(t.to_string());
                }
            }
        }
    };
    if from_pages {
        for (entry, _) in resolve_pages(c, p, &mut Map::new())? {
            if let Entry::Live(pg) = entry {
                let mut obj = json!({});
                apply_prop(w, c, &sub, gen, &pg, None, false, &mut obj, &mut gcont)?;
                add(obj.get(key));
            }
        }
    } else {
        list_module(w, c, actor, &sub, gen, &mut tmp, &mut gcont)?;
        add(if gen == "querypage" { tmp.get(key).and_then(|v| v.get("results")) } else { tmp.get(key) });
    }
    for (k, v) in gcont {
        cont.insert(format!("g{k}"), v);
    }
    let mut map = p.0.clone();
    map.insert("titles".into(), titles.join("|"));
    map.remove("pageids");
    map.remove("revids");
    Ok(Some(Params(map, p.1.clone())))
}

fn do_query(w: &Wiki, c: &Connection, actor: &Actor, p: &Params) -> Res<Value> {
    let mut q = Map::new();
    let mut cont = Cont::new();
    let generated = apply_generator(w, c, actor, p, &mut cont)?;
    let p = generated.as_ref().unwrap_or(p);
    let props = p.list("prop");
    if p.get("titles").is_some() || p.get("pageids").is_some() || p.get("revids").is_some() {
        let entries = resolve_pages(c, p, &mut q)?;
        let single = entries.len() == 1;
        let mut pages = Vec::new();
        for (entry, rev) in &entries {
            let mut obj = base(entry);
            if let Entry::Live(pg) = entry {
                for prop in &props {
                    apply_prop(w, c, p, prop, pg, *rev, single, &mut obj, &mut cont)?;
                }
            }
            pages.push(obj);
        }
        q.insert("pages".into(), Value::Array(pages));
    }
    for l in p.list("list") {
        list_module(w, c, actor, p, &l, &mut q, &mut cont)?;
    }
    for m in p.list("meta") {
        meta_module(w, c, actor, p, &m, &mut q)?;
    }
    let mut out = json!({ "batchcomplete": cont.is_empty(), "query": Value::Object(q) });
    if !cont.is_empty() {
        cont.insert("continue".into(), json!("-||"));
        out["continue"] = Value::Object(cont);
    }
    Ok(out)
}

fn rev_props(c: &Connection, r: &wiki::RevRow, props: &HashSet<String>, section: Option<&str>) -> Res<Value> {
    let mut v = Map::new();
    let has = |k: &str| props.contains(k);
    if has("ids") {
        v.insert("revid".into(), json!(r.id));
        v.insert("parentid".into(), json!(r.parent));
    }
    if has("flags") {
        v.insert("minor".into(), json!(r.minor));
    }
    if has("timestamp") {
        v.insert("timestamp".into(), json!(iso(r.ts)));
    }
    if has("user") {
        v.insert("user".into(), json!(r.user_name));
        v.insert("anon".into(), json!(r.user_id.is_none()));
    }
    if has("comment") {
        v.insert("comment".into(), json!(r.comment));
    }
    if has("size") {
        v.insert("size".into(), json!(r.size));
    }
    if has("sha256") || has("sha1") {
        v.insert("sha256".into(), json!(r.sha256));
    }
    if has("contentmodel") {
        v.insert("contentmodel".into(), json!(r.model));
    }
    if has("content") {
        let mut text = wiki::rev_text(c, r.id)?;
        if let Some(sec) = section {
            text = section_text(&text, sec)?;
        }
        v.insert("slots".into(), json!({ "main": { "contentmodel": r.model, "contentformat": "text/x-wiki", "content": text } }));
    }
    Ok(Value::Object(v))
}

fn section_text(text: &str, sec: &str) -> Res<String> {
    let n: usize = sec.parse().map_err(|_| ApiError::bad("badsection", "section must be a number"))?;
    if n == 0 {
        let (a, b) = wikitext::lead_range(text);
        return Ok(text[a..b].to_string());
    }
    let secs = wikitext::raw_sections(text);
    let s = secs.get(n - 1).ok_or_else(|| ApiError::bad("nosuchsection", format!("there is no section {n}")))?;
    Ok(text[s.start..s.end].to_string())
}

#[allow(clippy::too_many_arguments)]
fn apply_prop(w: &Wiki, c: &Connection, p: &Params, prop: &str, pg: &PageRow, rev: Option<i64>, single: bool, obj: &mut Value, cont: &mut Cont) -> Res<()> {
    match prop {
        "info" => {
            for (k, v) in page_info(w, pg).as_object().cloned().unwrap_or_default() {
                obj[&k] = v;
            }
        }
        "revisions" => {
            let default = ["ids", "timestamp", "flags", "comment", "user"];
            let rvprop: HashSet<String> = if p.get("rvprop").is_some() { p.list("rvprop").into_iter().collect() } else { default.iter().map(|s| s.to_string()).collect() };
            let section = p.get("rvsection");
            let revs: Vec<wiki::RevRow> = if let Some(rid) = rev {
                wiki::get_rev(c, rid)?.into_iter().collect()
            } else if p.get("rvlimit").is_some() && single {
                let limit = p.limit("rvlimit", 10, 500);
                let rows = queries::history(c, pg.id, limit, p.int("rvcontinue"), p.get("rvdir") == Some("newer"), p.get("rvuser"))?;
                if rows.len() as i64 == limit {
                    if let Some(l) = rows.last() {
                        cont.insert("rvcontinue".into(), json!(l.id.to_string()));
                    }
                }
                rows
            } else {
                wiki::get_rev(c, pg.latest)?.into_iter().collect()
            };
            let items = revs.iter().map(|r| rev_props(c, r, &rvprop, section)).collect::<Res<Vec<_>>>()?;
            obj["revisions"] = Value::Array(items);
        }
        "categories" => {
            let (limit, off) = (p.limit("cllimit", 10, 500), p.offset("cl"));
            let rows = queries::page_categories(c, pg.id, limit + 1, off)?;
            if rows.len() as i64 > limit {
                cont.insert("cloffset".into(), json!(off + limit));
            }
            obj["categories"] = rows.iter().take(limit as usize).map(|(cat, _)| json!({ "ns": NS_CATEGORY, "title": Title::new(NS_CATEGORY, cat).full() })).collect();
        }
        "links" | "templates" => {
            let (pfx, table) = if prop == "links" { ("pl", "pagelink") } else { ("tl", "templatelink") };
            let (limit, off) = (p.limit(&format!("{pfx}limit"), 10, 500), p.offset(pfx));
            let rows = queries::page_outlinks(c, table, pg.id, limit + 1, off)?;
            if rows.len() as i64 > limit {
                cont.insert(format!("{pfx}offset"), json!(off + limit));
            }
            obj[prop] = rows.iter().take(limit as usize).map(|(t, _)| json!({ "ns": t.ns, "title": t.full() })).collect();
        }
        "linkshere" => {
            let (limit, off) = (p.limit("lhlimit", 10, 500), p.offset("lh"));
            let ns = p.ns("lhnamespace", &[])?;
            let rows = queries::pages_linking_to(c, "pagelink", &pg.title, &ns, limit + 1, off)?;
            if rows.len() as i64 > limit {
                cont.insert("lhoffset".into(), json!(off + limit));
            }
            obj["linkshere"] = rows.iter().take(limit as usize).map(|x| json!({ "pageid": x.id, "ns": x.title.ns, "title": x.title.full(), "redirect": x.is_redirect })).collect();
        }
        "extracts" => {
            let (r, _) = w.render_page(c, pg)?;
            let intro = p.flag("exintro");
            let mut s = match (p.flag("explaintext"), intro) {
                (true, true) => r.extract.clone(),
                (true, false) => r.text.clone(),
                (false, true) => r.extract_html.clone(),
                (false, false) => r.html.clone(),
            };
            if let Some(n) = p.int("exchars").filter(|n| *n > 0) {
                if p.flag("explaintext") {
                    s = s.chars().take(n as usize).collect();
                }
            }
            obj["extract"] = json!(s);
        }
        "imageinfo" => {
            if pg.title.ns != crate::title::NS_FILE {
                return Ok(());
            }
            let Some(f) = crate::files::find_file(c, &pg.title.name)? else { return Ok(()) };
            let limit = p.limit("iilimit", 1, 500);
            let versions = if limit > 1 { crate::files::file_history(c, f.id, limit)? } else { vec![f.latest.clone()] };
            let url = w.cfg.file_path.replace("$1", &crate::util::path_encode(&f.name));
            let (tw, th) = (p.int("iiurlwidth").map(|n| n.clamp(1, 10_000) as u32), p.int("iiurlheight").map(|n| n.clamp(1, 10_000) as u32));
            obj["imageinfo"] = versions.iter().map(|v| {
                let vurl = if v.id == f.latest.id { url.clone() } else { format!("{url}?version={}", v.id) };
                let mut o = json!({
                    "timestamp": iso(v.ts), "user": v.user, "comment": v.comment, "size": v.size, "width": v.width, "height": v.height,
                    "mime": v.mime, "sha256": v.sha256, "url": vurl,
                });
                if tw.is_some() || th.is_some() {
                    let sep = if vurl.contains('?') { '&' } else { '?' };
                    match crate::files::rendition(&v.mime, v.width, v.height, tw, th) {
                        Some((w2, h2)) => {
                            o["thumburl"] = json!(format!("{vurl}{sep}width={w2}"));
                            o["thumbwidth"] = json!(w2);
                            o["thumbheight"] = json!(h2);
                        }
                        None => {
                            o["thumburl"] = json!(vurl);
                            o["thumbwidth"] = json!(v.width);
                            o["thumbheight"] = json!(v.height);
                        }
                    }
                }
                o
            }).collect();
        }
        "pageprops" | "description" => {
            let (r, _) = w.render_page(c, pg)?;
            let mut props = Map::new();
            if let Some(d) = &r.description {
                props.insert("wikibase-shortdesc".into(), json!(d));
            }
            if let Some(d) = &r.display_title {
                props.insert("displaytitle".into(), json!(d));
            }
            if prop == "description" {
                if let Some(d) = &r.description {
                    obj["description"] = json!(d);
                }
            } else {
                obj["pageprops"] = Value::Object(props);
            }
        }
        "images" => {
            let (limit, off) = (p.limit("imlimit", 10, 500), p.offset("im"));
            let mut st = c.prepare_cached("SELECT DISTINCT to_title FROM pagelink WHERE from_page=?1 AND to_ns=6 ORDER BY to_title LIMIT ?2 OFFSET ?3")?;
            let rows: Vec<String> = st.query_map(rusqlite::params![pg.id, limit + 1, off], |r| r.get(0))?.collect::<Result<_, _>>()?;
            if rows.len() as i64 > limit {
                cont.insert("imoffset".into(), json!(off + limit));
            }
            obj["images"] = rows.iter().take(limit as usize).map(|n| json!({ "ns": 6, "title": Title::new(crate::title::NS_FILE, n).full() })).collect();
        }
        "fileusage" | "transcludedin" => {
            let (pfx, table) = if prop == "fileusage" { ("fu", "pagelink") } else { ("ti", "templatelink") };
            let (limit, off) = (p.limit(&format!("{pfx}limit"), 10, 500), p.offset(pfx));
            let rows = if prop == "fileusage" && pg.title.ns != crate::title::NS_FILE { Vec::new() } else { queries::pages_linking_to(c, table, &pg.title, &[], limit + 1, off)? };
            if rows.len() as i64 > limit {
                cont.insert(format!("{pfx}offset"), json!(off + limit));
            }
            obj[prop] = rows.iter().take(limit as usize).map(|x| json!({ "pageid": x.id, "ns": x.title.ns, "title": x.title.full(), "redirect": x.is_redirect })).collect();
        }
        "redirects" => {
            let (limit, off) = (p.limit("rdlimit", 10, 500), p.offset("rd"));
            let mut st = c.prepare_cached("SELECT id, ns, title FROM page WHERE deleted=0 AND is_redirect=1 AND redirect_ns=?1 AND redirect_title=?2 ORDER BY id LIMIT ?3 OFFSET ?4")?;
            let rows: Vec<(i64, i64, String)> = st.query_map(rusqlite::params![pg.title.ns, pg.title.name, limit + 1, off], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?;
            if rows.len() as i64 > limit {
                cont.insert("rdoffset".into(), json!(off + limit));
            }
            obj["redirects"] = rows.iter().take(limit as usize).map(|(id, ns, t)| json!({ "pageid": id, "ns": ns, "title": Title::new(*ns, t).full() })).collect();
        }
        "extlinks" => {
            let (limit, off) = (p.limit("ellimit", 10, 500), p.offset("el"));
            let mut st = c.prepare_cached("SELECT url FROM extlink WHERE from_page=?1 ORDER BY url LIMIT ?2 OFFSET ?3")?;
            let rows: Vec<String> = st.query_map(rusqlite::params![pg.id, limit + 1, off], |r| r.get(0))?.collect::<Result<_, _>>()?;
            if rows.len() as i64 > limit {
                cont.insert("eloffset".into(), json!(off + limit));
            }
            obj["extlinks"] = rows.iter().take(limit as usize).map(|u| json!({ "url": u })).collect();
        }
        "iwlinks" => {
            let (limit, off) = (p.limit("iwlimit", 10, 500), p.offset("iw"));
            let mut st = c.prepare_cached(
                "SELECT l.prefix, l.title, coalesce(i.url,'') FROM iwlink l LEFT JOIN interwiki i ON i.prefix=l.prefix WHERE l.from_page=?1 ORDER BY l.prefix, l.title LIMIT ?2 OFFSET ?3",
            )?;
            let rows: Vec<(String, String, String)> = st.query_map(rusqlite::params![pg.id, limit + 1, off], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?;
            if rows.len() as i64 > limit {
                cont.insert("iwoffset".into(), json!(off + limit));
            }
            obj["iwlinks"] = rows.iter().take(limit as usize).map(|(pfx, t, url)| json!({ "prefix": pfx, "title": t, "url": url.replace("$1", &crate::util::path_encode(t)) })).collect();
        }
        "langlinks" => {
            let mut st = c.prepare_cached("SELECT l.lang, l.title, coalesce(i.url,'') FROM langlink l LEFT JOIN interwiki i ON i.prefix=l.lang WHERE l.from_page=?1 ORDER BY l.lang LIMIT 500")?;
            let rows: Vec<(String, String, String)> = st.query_map([pg.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?;
            obj["langlinks"] = rows.iter().map(|(l, t, url)| json!({ "lang": l, "title": t, "url": url.replace("$1", &crate::util::path_encode(t)) })).collect();
        }
        "contributors" => {
            let limit = p.limit("pclimit", 10, 500);
            let mut st = c.prepare_cached("SELECT user_id, user_name, count(*) FROM revision WHERE page=?1 AND user_id IS NOT NULL GROUP BY user_id ORDER BY user_name LIMIT ?2")?;
            let rows: Vec<(i64, String, i64)> = st.query_map(rusqlite::params![pg.id, limit], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?;
            let anons: i64 = c.prepare_cached("SELECT count(DISTINCT user_name) FROM revision WHERE page=?1 AND user_id IS NULL")?.query_row([pg.id], |r| r.get(0))?;
            obj["anoncontributors"] = json!(anons);
            obj["contributors"] = rows.iter().map(|(id, name, n)| json!({ "userid": id, "name": name, "edits": n })).collect();
        }
        "categoryinfo" => {
            if pg.title.ns == NS_CATEGORY {
                let (size, files, subcats): (i64, i64, i64) = c
                    .prepare_cached(
                        "SELECT count(*), coalesce(sum(p.ns=6),0), coalesce(sum(p.ns=14),0) FROM categorylink l JOIN page p ON p.id=l.from_page WHERE l.cat=?1 AND p.deleted=0",
                    )?
                    .query_row([&pg.title.name], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
                obj["categoryinfo"] = json!({ "size": size, "pages": size - files - subcats, "files": files, "subcats": subcats, "hidden": false });
            }
        }
        "duplicatefiles" => {
            if pg.title.ns == crate::title::NS_FILE {
                let mut st = c.prepare_cached(
                    "SELECT f2.name FROM file f JOIN file_version v ON v.id=f.latest JOIN file_version v2 ON v2.sha256=v.sha256 JOIN file f2 ON f2.latest=v2.id
                     WHERE f.name=?1 AND f2.id<>f.id AND f2.deleted=0 ORDER BY f2.name LIMIT 500",
                )?;
                let rows: Vec<String> = st.query_map([&pg.title.name], |r| r.get(0))?.collect::<Result<_, _>>()?;
                obj["duplicatefiles"] = rows.iter().map(|n| json!({ "name": n.replace(' ', "_"), "title": Title::new(crate::title::NS_FILE, n).full() })).collect();
            }
        }
        "pageimages" => {
            let first: Option<String> = c
                .prepare_cached("SELECT to_title FROM pagelink WHERE from_page=?1 AND to_ns=6 ORDER BY rowid LIMIT 1")?
                .query_row([pg.id], |r| r.get(0))
                .map(Some)
                .or_else(|e| if e == rusqlite::Error::QueryReturnedNoRows { Ok(None) } else { Err(e) })?;
            let own = (pg.title.ns == crate::title::NS_FILE).then(|| pg.title.name.clone());
            if let Some(f) = own.or(first).map(|n| crate::files::find_file(c, &n)).transpose()?.flatten() {
                let url = w.cfg.file_path.replace("$1", &crate::util::path_encode(&f.name));
                let want = p.int("pithumbsize").unwrap_or(50).clamp(1, 10_000) as u32;
                let v = &f.latest;
                obj["pageimage"] = json!(f.name.replace(' ', "_"));
                obj["original"] = json!({ "source": url, "width": v.width, "height": v.height });
                if v.mime.starts_with("image/") {
                    let sep = if url.contains('?') { '&' } else { '?' };
                    obj["thumbnail"] = match crate::files::rendition(&v.mime, v.width, v.height, Some(want), None) {
                        Some((tw, th)) => json!({ "source": format!("{url}{sep}width={tw}"), "width": tw, "height": th }),
                        None => json!({ "source": url, "width": v.width, "height": v.height }),
                    };
                }
            }
        }
        other => return Err(ApiError::bad("badvalue", format!("unrecognized prop '{other}'"))),
    }
    Ok(())
}

fn put(q: &mut Map<String, Value>, k: &str, items: Vec<Value>) {
    q.insert(k.into(), Value::Array(items));
}

fn list_module(w: &Wiki, c: &Connection, actor: &Actor, p: &Params, name: &str, q: &mut Map<String, Value>, cont: &mut Cont) -> Res<()> {
    match name {
        "search" => {
            let query = p.req("srsearch")?;
            let (limit, off) = (p.limit("srlimit", 10, 50), p.offset("sr"));
            let ns = p.ns("srnamespace", &[0])?;
            let (total, hits) = queries::search(c, query, &ns, limit, off, false)?;
            q.insert("searchinfo".into(), json!({ "totalhits": total }));
            if off + limit < total {
                cont.insert("sroffset".into(), json!(off + limit));
            }
            put(q, "search", hits.iter().map(|h| json!({
                "ns": h.page.title.ns, "title": h.page.title.full(), "pageid": h.page.id, "size": h.page.len,
                "wordcount": h.page.words, "snippet": h.snippet, "timestamp": iso(h.page.touched),
            })).collect());
        }
        "prefixsearch" => {
            let (limit, off) = (p.limit("pslimit", 10, 100), p.offset("ps"));
            let ns = p.ns("psnamespace", &[0])?.first().copied().unwrap_or(0);
            let rows = queries::prefix_search(c, p.req("pssearch")?, ns, limit + 1, off)?;
            if rows.len() as i64 > limit {
                cont.insert("psoffset".into(), json!(off + limit));
            }
            put(q, "prefixsearch", rows.iter().take(limit as usize).map(|x| json!({ "ns": x.title.ns, "title": x.title.full(), "pageid": x.id })).collect());
        }
        "allpages" => {
            let ns = p.ns("apnamespace", &[0])?.first().copied().unwrap_or(0);
            let limit = p.limit("aplimit", 10, 500);
            let prefix = p.get("apprefix").map(|x| Title::parse_in(x, ns).map(|t| t.title.name).unwrap_or_default()).unwrap_or_default();
            let from = p.get("apfrom").or(p.get("apcontinue")).unwrap_or("").replace('_', " ");
            let filter = match p.get("apfilterredir") {
                Some("redirects") => "redirects",
                Some("nonredirects") => "nonredirects",
                _ => "all",
            };
            let mut rows = queries::all_pages(c, ns, &prefix, &from, limit + 1, 0, filter)?;
            if rows.len() as i64 > limit {
                if let Some(n) = rows.pop() {
                    cont.insert("apcontinue".into(), json!(n.title.name.replace(' ', "_")));
                }
            }
            put(q, "allpages", rows.iter().map(|x| json!({ "pageid": x.id, "ns": x.title.ns, "title": x.title.full() })).collect());
        }
        "allcategories" => {
            let (limit, off) = (p.limit("aclimit", 10, 500), p.offset("ac"));
            let rows = queries::all_categories(c, p.get("acprefix").unwrap_or(""), limit + 1, off)?;
            if rows.len() as i64 > limit {
                cont.insert("acoffset".into(), json!(off + limit));
            }
            put(q, "allcategories", rows.iter().take(limit as usize).map(|(n, size, _)| json!({ "category": n, "title": Title::new(NS_CATEGORY, n).full(), "size": size })).collect());
        }
        "categorymembers" => {
            let t = Title::parse_in(p.req("cmtitle")?, NS_CATEGORY).map_err(ApiError::bad_title)?.title;
            let (limit, off) = (p.limit("cmlimit", 10, 500), p.offset("cm"));
            let kind = p.list("cmtype").first().cloned().unwrap_or_else(|| "all".into());
            let rows = queries::category_members(c, &t.name, &kind, limit + 1, off)?;
            if rows.len() as i64 > limit {
                cont.insert("cmoffset".into(), json!(off + limit));
            }
            put(q, "categorymembers", rows.iter().take(limit as usize).map(|(x, key)| json!({
                "pageid": x.id, "ns": x.title.ns, "title": x.title.full(), "sortkeyprefix": key,
                "type": match x.title.ns { 14 => "subcat", 6 => "file", _ => "page" },
            })).collect());
        }
        "backlinks" | "embeddedin" => {
            let (pfx, key, table) = if name == "backlinks" { ("bl", "bltitle", "pagelink") } else { ("ei", "eititle", "templatelink") };
            let t = parse_title(p.req(key)?)?;
            let (limit, off) = (p.limit(&format!("{pfx}limit"), 10, 500), p.offset(pfx));
            let ns = p.ns(&format!("{pfx}namespace"), &[])?;
            let rows = queries::pages_linking_to(c, table, &t, &ns, limit + 1, off)?;
            if rows.len() as i64 > limit {
                cont.insert(format!("{pfx}offset"), json!(off + limit));
            }
            put(q, name, rows.iter().take(limit as usize).map(|x| json!({ "pageid": x.id, "ns": x.title.ns, "title": x.title.full(), "redirect": x.is_redirect })).collect());
        }
        "recentchanges" => {
            let (limit, off) = (p.limit("rclimit", 10, 500), p.offset("rc"));
            let ns = p.ns("rcnamespace", &[])?;
            // rcstart/rcend bound the time window; rcdir=newer lists oldest first (as in MediaWiki,
            // rcstart is the newer end by default and the older end with rcdir=newer).
            let newer = p.get("rcdir") == Some("newer");
            let ts = |k: &str| -> Res<Option<i64>> {
                match p.get(k) {
                    None => Ok(None),
                    Some(v) => crate::timefmt::parse(v, crate::util::now()).map(Some).ok_or_else(|| ApiError::bad("badtimestamp", format!("invalid {k}"))),
                }
            };
            let (start, end) = (ts("rcstart")?, ts("rcend")?);
            let window = if newer { (start.unwrap_or(i64::MIN), end.unwrap_or(i64::MAX)) } else { (end.unwrap_or(i64::MIN), start.unwrap_or(i64::MAX)) };
            let rows = queries::recent_changes_in(c, &ns, &p.list("rctype"), p.get("rcuser"), window, newer, limit + 1, off)?;
            if rows.len() as i64 > limit {
                cont.insert("rcoffset".into(), json!(off + limit));
            }
            put(q, "recentchanges", rows.iter().take(limit as usize).map(change_json).collect());
        }
        "logevents" => {
            let (limit, off) = (p.limit("lelimit", 10, 500), p.offset("le"));
            let t = p.get("letitle").map(parse_title).transpose()?;
            let rows = queries::log_events(c, p.get("letype"), t.as_ref(), p.get("leuser"), &[], limit + 1, off)?;
            if rows.len() as i64 > limit {
                cont.insert("leoffset".into(), json!(off + limit));
            }
            put(q, "logevents", rows.iter().take(limit as usize).map(log_json).collect());
        }
        "random" => {
            let ns = p.ns("rnnamespace", &[0])?;
            let rows = queries::random_pages(c, &ns, p.limit("rnlimit", 1, 500))?;
            put(q, "random", rows.iter().map(|x| json!({ "id": x.id, "ns": x.title.ns, "title": x.title.full() })).collect());
        }
        "allimages" => {
            let limit = p.limit("ailimit", 10, 500);
            let from = p.get("aifrom").or(p.get("aicontinue")).unwrap_or("").replace('_', " ");
            let mut rows = crate::files::all_files(c, p.get("aiprefix").unwrap_or(""), &from, p.get("aimime"), limit + 1)?;
            if rows.len() as i64 > limit {
                if let Some(n) = rows.pop() {
                    cont.insert("aicontinue".into(), json!(n.name.replace(' ', "_")));
                }
            }
            put(q, "allimages", rows.iter().map(|f| {
                let mut v = crate::files::info_json(w, f);
                v["title"] = json!(Title::new(crate::title::NS_FILE, &f.name).full());
                v
            }).collect());
        }
        "allusers" => {
            let (limit, off) = (p.limit("aulimit", 10, 500), p.offset("au"));
            let rows = users::list_users(c, p.get("auprefix").unwrap_or(""), p.get("augroup"), limit + 1, off)?;
            if rows.len() as i64 > limit {
                cont.insert("auoffset".into(), json!(off + limit));
            }
            put(q, "allusers", rows.iter().take(limit as usize).map(|u| json!({
                "userid": u.id, "name": u.name, "editcount": u.editcount, "registration": iso(u.registered), "groups": u.groups,
            })).collect());
        }
        "usercontribs" => {
            let limit = p.limit("uclimit", 10, 500);
            let user = p.req("ucuser")?.replace('_', " ");
            let rows = queries::contribs(c, &user, limit, p.int("uccontinue"))?;
            if rows.len() as i64 == limit {
                if let Some(l) = rows.last() {
                    cont.insert("uccontinue".into(), json!(l.rev.id.to_string()));
                }
            }
            put(q, "usercontribs", rows.iter().map(|x| json!({
                "user": x.rev.user_name, "pageid": x.rev.page, "revid": x.rev.id, "parentid": x.rev.parent, "ns": x.title.ns,
                "title": x.title.full(), "timestamp": iso(x.rev.ts), "comment": x.rev.comment, "size": x.rev.size,
                "sizediff": x.rev.size - x.oldlen, "minor": x.rev.minor, "new": x.rev.parent == 0,
            })).collect());
        }
        "watchlist" | "watchlistraw" => {
            let uid = actor.user_id.ok_or_else(|| ApiError::unauthorized("a watchlist needs a user account (send an API token)"))?;
            let pfx = if name == "watchlist" { "wl" } else { "wr" };
            let (limit, off) = (p.limit(&format!("{pfx}limit"), 10, 500), p.offset(pfx));
            let rows: Vec<Value> = if name == "watchlist" {
                Wiki::watchlist_changes(c, uid, limit + 1, off)?
            } else {
                Wiki::watchlist_raw(c, uid, limit + 1, off)?.iter().map(|t| json!({ "ns": t.ns, "title": t.full() })).collect()
            };
            if rows.len() as i64 > limit {
                cont.insert(format!("{pfx}offset"), json!(off + limit));
            }
            put(q, name, rows.into_iter().take(limit as usize).collect());
        }
        "allredirects" => {
            let (limit, off) = (p.limit("arlimit", 10, 500), p.offset("ar"));
            let ns = p.ns("arnamespace", &[0])?.first().copied().unwrap_or(0);
            let mut st = c.prepare_cached(
                "SELECT id, ns, title, redirect_ns, redirect_title FROM page WHERE deleted=0 AND is_redirect=1 AND redirect_ns=?1 ORDER BY redirect_title, id LIMIT ?2 OFFSET ?3",
            )?;
            let rows: Vec<(i64, i64, String, i64, String)> = st.query_map(rusqlite::params![ns, limit + 1, off], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect::<Result<_, _>>()?;
            if rows.len() as i64 > limit {
                cont.insert("aroffset".into(), json!(off + limit));
            }
            put(q, name, rows.iter().take(limit as usize).map(|(id, fns, ft, tns, tt)| json!({ "fromid": id, "from": Title::new(*fns, ft).full(), "ns": tns, "title": Title::new(*tns, tt).full() })).collect());
        }
        "alllinks" | "alltransclusions" | "allfileusages" => {
            let (pfx, table, default_ns) = match name {
                "alllinks" => ("al", "pagelink", 0),
                "alltransclusions" => ("at", "templatelink", 10),
                _ => ("af", "pagelink", 6),
            };
            let (limit, off) = (p.limit(&format!("{pfx}limit"), 10, 500), p.offset(pfx));
            let ns = if name == "allfileusages" { 6 } else { p.ns(&format!("{pfx}namespace"), &[default_ns])?.first().copied().unwrap_or(default_ns) };
            let prefix = p.get(&format!("{pfx}prefix")).unwrap_or("").replace('_', " ");
            let like = format!("{}%", prefix.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
            let mut st = c.prepare_cached(&format!("SELECT DISTINCT to_title FROM {table} WHERE to_ns=?1 AND to_title LIKE ?2 ESCAPE '\\' ORDER BY to_title LIMIT ?3 OFFSET ?4"))?;
            let rows: Vec<String> = st.query_map(rusqlite::params![ns, like, limit + 1, off], |r| r.get(0))?.collect::<Result<_, _>>()?;
            if rows.len() as i64 > limit {
                cont.insert(format!("{pfx}offset"), json!(off + limit));
            }
            put(q, name, rows.iter().take(limit as usize).map(|t| json!({ "ns": ns, "title": Title::new(ns, t).full() })).collect());
        }
        "allrevisions" | "alldeletedrevisions" => {
            let deleted = name == "alldeletedrevisions";
            if deleted {
                actor.require("viewdeleted")?;
            }
            let pfx = if deleted { "adr" } else { "arv" };
            let limit = p.limit(&format!("{pfx}limit"), 10, 500);
            let cursor = p.int(&format!("{pfx}continue")).unwrap_or(i64::MAX);
            let user = p.get(&format!("{pfx}user")).map(|u| u.replace('_', " "));
            let with_content = deleted && p.list(&format!("{pfx}prop")).iter().any(|x| x == "content");
            let mut st = c.prepare_cached(
                "SELECT r.id, r.parent, p.id, p.ns, p.title, r.ts, r.user_name, r.comment, r.size, r.sha256 FROM revision r JOIN page p ON p.id=r.page
                 WHERE p.deleted=?1 AND r.id<?2 AND (?3 IS NULL OR r.user_name=?3) ORDER BY r.id DESC LIMIT ?4",
            )?;
            let rows: Vec<(i64, i64, i64, i64, String, i64, String, String, i64, String)> = st
                .query_map(rusqlite::params![deleted as i64, cursor, user, limit], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?)))?
                .collect::<Result<_, _>>()?;
            if rows.len() as i64 == limit {
                if let Some(l) = rows.last() {
                    cont.insert(format!("{pfx}continue"), json!(l.0.to_string()));
                }
            }
            let mut items = Vec::new();
            for (id, parent, pid, ns, t, ts, user, comment, size, sha) in &rows {
                let mut rev = json!({ "revid": id, "parentid": parent, "user": user, "timestamp": iso(*ts), "comment": comment, "size": size, "sha256": sha });
                if with_content {
                    rev["slots"] = json!({ "main": { "contentmodel": "wikitext", "content": wiki::rev_text(c, *id)? } });
                }
                items.push(json!({ "pageid": pid, "ns": ns, "title": Title::new(*ns, t).full(), "revisions": [rev] }));
            }
            put(q, name, items);
        }
        "users" => {
            let mut items = Vec::new();
            for raw in p.list("ususers").iter().take(500) {
                let nm = raw.replace('_', " ");
                match users::user_by_name(c, &nm)? {
                    Some(u) => items.push(json!({ "userid": u.id, "name": u.name, "editcount": u.editcount, "registration": iso(u.registered), "groups": u.groups, "blocked": u.blocked })),
                    None => items.push(json!({ "name": nm, "missing": true })),
                }
            }
            put(q, name, items);
        }
        "blocks" => {
            let (limit, off) = (p.limit("bklimit", 10, 500), p.offset("bk"));
            let mut st = c.prepare_cached("SELECT id, name, block_reason FROM user WHERE blocked=1 ORDER BY name LIMIT ?1 OFFSET ?2")?;
            let rows: Vec<(i64, String, String)> = st.query_map(rusqlite::params![limit + 1, off], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?;
            if rows.len() as i64 > limit {
                cont.insert("bkoffset".into(), json!(off + limit));
            }
            put(q, name, rows.iter().take(limit as usize).map(|(id, n, why)| json!({ "userid": id, "user": n, "reason": why })).collect());
        }
        "querypage" => {
            let (limit, off) = (p.limit("qplimit", 10, 500), p.offset("qp"));
            let page = p.req("qppage")?.to_string();
            // (sql returning value, ns, title)
            let sql = match page.to_lowercase().as_str() {
                "wantedpages" => "SELECT count(*), l.to_ns, l.to_title FROM pagelink l JOIN page s ON s.id=l.from_page AND s.deleted=0
                                  WHERE l.to_ns NOT IN (6,14) AND NOT EXISTS(SELECT 1 FROM page p WHERE p.ns=l.to_ns AND p.title=l.to_title AND p.deleted=0)
                                  GROUP BY l.to_ns, l.to_title ORDER BY 1 DESC, 3",
                "wantedcategories" => "SELECT count(*), 14, l.cat FROM categorylink l JOIN page s ON s.id=l.from_page AND s.deleted=0
                                  WHERE NOT EXISTS(SELECT 1 FROM page p WHERE p.ns=14 AND p.title=l.cat AND p.deleted=0) GROUP BY l.cat ORDER BY 1 DESC, 3",
                "wantedtemplates" => "SELECT count(*), l.to_ns, l.to_title FROM templatelink l JOIN page s ON s.id=l.from_page AND s.deleted=0
                                  WHERE NOT EXISTS(SELECT 1 FROM page p WHERE p.ns=l.to_ns AND p.title=l.to_title AND p.deleted=0)
                                  GROUP BY l.to_ns, l.to_title ORDER BY 1 DESC, 3",
                "lonelypages" => "SELECT 0, p.ns, p.title FROM page p WHERE p.deleted=0 AND p.ns=0 AND p.is_redirect=0
                                  AND NOT EXISTS(SELECT 1 FROM pagelink l JOIN page s ON s.id=l.from_page AND s.deleted=0 WHERE l.to_ns=p.ns AND l.to_title=p.title AND l.from_page<>p.id)
                                  AND NOT EXISTS(SELECT 1 FROM templatelink l WHERE l.to_ns=p.ns AND l.to_title=p.title) ORDER BY p.title",
                "uncategorizedpages" => "SELECT 0, p.ns, p.title FROM page p WHERE p.deleted=0 AND p.ns=0 AND p.is_redirect=0
                                  AND NOT EXISTS(SELECT 1 FROM categorylink l WHERE l.from_page=p.id) ORDER BY p.title",
                "deadendpages" => "SELECT 0, p.ns, p.title FROM page p WHERE p.deleted=0 AND p.ns=0 AND p.is_redirect=0
                                  AND NOT EXISTS(SELECT 1 FROM pagelink l WHERE l.from_page=p.id AND l.to_ns<>6) ORDER BY p.title",
                "longpages" => "SELECT p.len, p.ns, p.title FROM page p WHERE p.deleted=0 AND p.ns=0 AND p.is_redirect=0 ORDER BY p.len DESC, p.title",
                "shortpages" => "SELECT p.len, p.ns, p.title FROM page p WHERE p.deleted=0 AND p.ns=0 AND p.is_redirect=0 ORDER BY p.len, p.title",
                "ancientpages" => "SELECT p.touched, p.ns, p.title FROM page p WHERE p.deleted=0 AND p.ns=0 AND p.is_redirect=0 ORDER BY p.touched, p.title",
                "mostlinked" => "SELECT count(*), l.to_ns, l.to_title FROM pagelink l JOIN page s ON s.id=l.from_page AND s.deleted=0 GROUP BY l.to_ns, l.to_title ORDER BY 1 DESC, 3",
                "mostrevisions" => "SELECT count(*), p.ns, p.title FROM revision r JOIN page p ON p.id=r.page WHERE p.deleted=0 GROUP BY p.id ORDER BY 1 DESC, 3",
                "mostcategories" => "SELECT count(*), p.ns, p.title FROM categorylink l JOIN page p ON p.id=l.from_page WHERE p.deleted=0 GROUP BY p.id ORDER BY 1 DESC, 3",
                "unusedtemplates" => "SELECT 0, p.ns, p.title FROM page p WHERE p.deleted=0 AND p.ns=10 AND p.is_redirect=0
                                  AND NOT EXISTS(SELECT 1 FROM templatelink l JOIN page s ON s.id=l.from_page AND s.deleted=0 WHERE l.to_ns=10 AND l.to_title=p.title) ORDER BY p.title",
                "unusedimages" => "SELECT 0, 6, f.name FROM file f WHERE f.deleted=0
                                  AND NOT EXISTS(SELECT 1 FROM pagelink l JOIN page s ON s.id=l.from_page AND s.deleted=0 WHERE l.to_ns=6 AND l.to_title=f.name) ORDER BY f.name",
                "unusedcategories" => "SELECT 0, p.ns, p.title FROM page p WHERE p.deleted=0 AND p.ns=14 AND NOT EXISTS(SELECT 1 FROM categorylink l WHERE l.cat=p.title) ORDER BY p.title",
                "brokenredirects" => "SELECT 0, p.ns, p.title FROM page p WHERE p.deleted=0 AND p.is_redirect=1
                                  AND NOT EXISTS(SELECT 1 FROM page t WHERE t.ns=p.redirect_ns AND t.title=p.redirect_title AND t.deleted=0) ORDER BY p.title",
                "doubleredirects" => "SELECT 0, p.ns, p.title FROM page p JOIN page t ON t.ns=p.redirect_ns AND t.title=p.redirect_title AND t.deleted=0 AND t.is_redirect=1
                                  WHERE p.deleted=0 AND p.is_redirect=1 ORDER BY p.title",
                "listredirects" => "SELECT 0, p.ns, p.title FROM page p WHERE p.deleted=0 AND p.is_redirect=1 ORDER BY p.title",
                "withoutinterwiki" => "SELECT 0, p.ns, p.title FROM page p WHERE p.deleted=0 AND p.ns=0 AND p.is_redirect=0
                                  AND NOT EXISTS(SELECT 1 FROM langlink l WHERE l.from_page=p.id) ORDER BY p.title",
                _ => return Err(ApiError::bad("badvalue", "unknown qppage (try Wantedpages, Wantedcategories, Wantedtemplates, Lonelypages, Uncategorizedpages, Deadendpages, Longpages, Shortpages, Ancientpages, Mostlinked, Mostrevisions, Mostcategories, Unusedtemplates, Unusedimages, Unusedcategories, BrokenRedirects, DoubleRedirects, Listredirects, Withoutinterwiki)")),
            };
            let mut st = c.prepare_cached(&format!("{sql} LIMIT ?1 OFFSET ?2"))?;
            let rows: Vec<(i64, i64, String)> = st.query_map(rusqlite::params![limit + 1, off], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?;
            if rows.len() as i64 > limit {
                cont.insert("qpoffset".into(), json!(off + limit));
            }
            let results: Vec<Value> = rows.iter().take(limit as usize).map(|(v, ns, t)| json!({ "value": v, "ns": ns, "title": Title::new(*ns, t).full() })).collect();
            q.insert("querypage".into(), json!({ "name": page, "results": results }));
        }
        "imageusage" => {
            let t = Title::parse_in(p.req("iutitle")?, crate::title::NS_FILE).map_err(ApiError::bad_title)?.title;
            let (limit, off) = (p.limit("iulimit", 10, 500), p.offset("iu"));
            let rows = queries::pages_linking_to(c, "pagelink", &t, &p.ns("iunamespace", &[])?, limit + 1, off)?;
            if rows.len() as i64 > limit {
                cont.insert("iuoffset".into(), json!(off + limit));
            }
            put(q, name, rows.iter().take(limit as usize).map(|x| json!({ "pageid": x.id, "ns": x.title.ns, "title": x.title.full(), "redirect": x.is_redirect })).collect());
        }
        "exturlusage" => {
            // euquery matches the URL without its protocol, `*` is a wildcard (as in MediaWiki).
            let (limit, off) = (p.limit("eulimit", 10, 500), p.offset("eu"));
            let query = p.get("euquery").unwrap_or("").trim().to_string();
            let like = if query.is_empty() {
                "%".to_string()
            } else {
                let esc = query.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_").replace('*', "%");
                format!("%://{esc}%")
            };
            let mut st = c.prepare_cached(
                "SELECT p.id, p.ns, p.title, e.url FROM extlink e JOIN page p ON p.id=e.from_page WHERE p.deleted=0 AND e.url LIKE ?1 ESCAPE '\\' ORDER BY e.url, p.id LIMIT ?2 OFFSET ?3",
            )?;
            let rows: Vec<(i64, i64, String, String)> = st.query_map(rusqlite::params![like, limit + 1, off], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<_, _>>()?;
            if rows.len() as i64 > limit {
                cont.insert("euoffset".into(), json!(off + limit));
            }
            put(q, name, rows.iter().take(limit as usize).map(|(id, ns, t, url)| json!({ "pageid": id, "ns": ns, "title": Title::new(*ns, t).full(), "url": url })).collect());
        }
        "iwbacklinks" => {
            let (limit, off) = (p.limit("iwbllimit", 10, 500), p.offset("iwbl"));
            let prefix = p.req("iwblprefix")?.to_lowercase();
            let title = p.get("iwbltitle").map(|t| crate::util::collapse_ws(&t.replace('_', " ")));
            let mut st = c.prepare_cached(
                "SELECT p.id, p.ns, p.title, l.title FROM iwlink l JOIN page p ON p.id=l.from_page WHERE p.deleted=0 AND l.prefix=?1 AND (?2 IS NULL OR l.title=?2) ORDER BY l.title, p.id LIMIT ?3 OFFSET ?4",
            )?;
            let rows: Vec<(i64, i64, String, String)> = st.query_map(rusqlite::params![prefix, title, limit + 1, off], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<_, _>>()?;
            if rows.len() as i64 > limit {
                cont.insert("iwbloffset".into(), json!(off + limit));
            }
            put(q, name, rows.iter().take(limit as usize).map(|(id, ns, t, it)| json!({ "pageid": id, "ns": ns, "title": Title::new(*ns, t).full(), "iwprefix": prefix, "iwtitle": it })).collect());
        }
        "pageswithprop" => {
            let (limit, off) = (p.limit("pwplimit", 10, 500), p.offset("pwp"));
            let prop = match p.req("pwppropname")? {
                "wikibase-shortdesc" => "description",
                other => other,
            }
            .to_string();
            let mut st = c.prepare_cached(
                "SELECT p.id, p.ns, p.title, x.value FROM pageprop x JOIN page p ON p.id=x.page WHERE p.deleted=0 AND x.name=?1 ORDER BY p.id LIMIT ?2 OFFSET ?3",
            )?;
            let rows: Vec<(i64, i64, String, String)> = st.query_map(rusqlite::params![prop, limit + 1, off], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<_, _>>()?;
            if rows.len() as i64 > limit {
                cont.insert("pwpoffset".into(), json!(off + limit));
            }
            put(q, name, rows.iter().take(limit as usize).map(|(id, ns, t, v)| json!({ "pageid": id, "ns": ns, "title": Title::new(*ns, t).full(), "value": v })).collect());
        }
        other => return Err(ApiError::bad("badvalue", format!("unrecognized list '{other}'"))),
    }
    let _ = w;
    Ok(())
}

fn meta_module(w: &Wiki, c: &Connection, actor: &Actor, p: &Params, name: &str, q: &mut Map<String, Value>) -> Res<()> {
    match name {
        "siteinfo" => {
            let want: Vec<String> = if p.get("siprop").is_some() { p.list("siprop") } else { vec!["general".into()] };
            for k in want {
                match k.as_str() {
                    "general" => {
                        q.insert("general".into(), json!({
                            "sitename": w.cfg.site_name, "generator": format!("hoepedia-api {}", env!("CARGO_PKG_VERSION")),
                            "articlepath": w.cfg.article_path, "case": "first-letter", "readonly": w.cfg.read_only || w.is_replica(), "lang": w.cfg.site_lang, "timezone": w.cfg.timezone,
                            "anonymousread": w.cfg.anon_read, "anonymousedit": w.cfg.anon_edit, "maxarticlesize": w.cfg.max_page_bytes,
                            "time": iso(crate::util::now()),
                        }));
                    }
                    "interwikimap" => {
                        let mut stmt = c.prepare("SELECT prefix,url,api,trans,lang FROM interwiki ORDER BY prefix")?;
                        let rows = stmt
                            .query_map([], |r| Ok(json!({ "prefix": r.get::<_, String>(0)?, "url": r.get::<_, String>(1)?, "api": r.get::<_, String>(2)?, "trans": r.get::<_, i64>(3)? != 0, "language": r.get::<_, i64>(4)? != 0 })))?
                            .collect::<Result<Vec<_>, _>>()?;
                        q.insert("interwikimap".into(), Value::Array(rows));
                    }
                    "namespaces" => {
                        q.insert("namespaces".into(), namespaces_json());
                    }
                    "statistics" => {
                        let s = queries::stats(c)?;
                        q.insert("statistics".into(), json!({ "pages": s.pages, "articles": s.articles, "edits": s.edits, "users": s.users }));
                    }
                    "usergroups" => {
                        q.insert("usergroups".into(), json!([
                            { "name": "*", "rights": ["read"] },
                            { "name": "user", "rights": ["read", "edit", "createpage", "move"] },
                            { "name": "sysop", "rights": ["editprotected", "delete", "undelete", "protect", "rollback", "block", "userrights", "createuser", "suppressredirect"] },
                        ]));
                    }
                    other => return Err(ApiError::bad("badvalue", format!("unrecognized siprop '{other}'"))),
                }
            }
        }
        "userinfo" => {
            let mut v = actor_json(actor);
            v["editcount"] = json!(match actor.user_id {
                Some(id) => c.query_row("SELECT editcount FROM user WHERE id=?1", [id], |r| r.get::<_, i64>(0)).unwrap_or(0),
                None => 0,
            });
            q.insert("userinfo".into(), v);
        }
        "tokens" => {
            // Present only so MediaWiki client libraries keep working; auth is by bearer token.
            q.insert("tokens".into(), json!({ "csrftoken": "+\\" }));
        }
        other => return Err(ApiError::bad("badvalue", format!("unrecognized meta '{other}'"))),
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// action=parse / opensearch / compare
// ---------------------------------------------------------------------------------------

fn do_parse(w: &Wiki, c: &Connection, p: &Params) -> Res<Value> {
    let (title, text, revid, page_id, rendered) = if let Some(txt) = p.get("text") {
        let t = parse_title(p.get("title").unwrap_or("API"))?;
        let txt = w.clean_text(txt)?;
        let r = std::sync::Arc::new(w.render_text(c, &t, &txt));
        (t, txt, 0, 0, r)
    } else {
        let (pg, rev) = if let Some(old) = p.int("oldid") {
            let (r, pg) = wiki::rev_with_page(c, old)?;
            (pg, r.id)
        } else {
            let t = parse_title(p.get("page").or(p.get("title")).ok_or_else(|| ApiError::bad("missingparam", "one of page, oldid or text is required"))?)?;
            let pg = find_page(c, &t)?.ok_or_else(|| ApiError::not_found("missingtitle", "the page you specified doesn't exist"))?;
            let pg = if p.flag("redirects") { follow_redirects(c, pg)?.0 } else { pg };
            let l = pg.latest;
            (pg, l)
        };
        let txt = wiki::rev_text(c, rev)?;
        let r = w.render_rev(c, rev, &pg.title, &txt);
        (pg.title.clone(), txt, rev, pg.id, r)
    };
    let want: HashSet<String> = if p.get("prop").is_some() {
        p.list("prop").into_iter().collect()
    } else {
        ["text", "categories", "links", "templates", "sections", "displaytitle", "revid", "properties"].iter().map(|s| s.to_string()).collect()
    };
    let mut out = Map::new();
    out.insert("title".into(), json!(title.full()));
    out.insert("pageid".into(), json!(page_id));
    if want.contains("revid") {
        out.insert("revid".into(), json!(revid));
    }
    if want.contains("text") {
        out.insert("text".into(), json!(rendered.html));
    }
    if want.contains("wikitext") {
        out.insert("wikitext".into(), json!(text));
    }
    if want.contains("displaytitle") {
        out.insert("displaytitle".into(), json!(rendered.display_title.clone().unwrap_or_else(|| title.full())));
    }
    if want.contains("categories") {
        out.insert("categories".into(), rendered.categories.iter().map(|(cat, key)| json!({ "sortkey": key, "category": cat.replace(' ', "_") })).collect());
    }
    let existing = |t: &Title| find_page(c, t).map(|x| x.is_some()).unwrap_or(false);
    if want.contains("links") {
        out.insert("links".into(), rendered.links.iter().map(|t| json!({ "ns": t.ns, "title": t.full(), "exists": existing(t) })).collect());
    }
    if want.contains("templates") {
        out.insert("templates".into(), rendered.templates.iter().map(|t| json!({ "ns": t.ns, "title": t.full(), "exists": existing(t) })).collect());
    }
    if want.contains("sections") {
        out.insert("sections".into(), json!(rendered.sections));
    }
    if want.contains("properties") {
        let mut props = Map::new();
        if let Some(d) = &rendered.description {
            props.insert("wikibase-shortdesc".into(), json!(d));
        }
        out.insert("properties".into(), Value::Object(props));
    }
    Ok(json!({ "parse": Value::Object(out) }))
}

fn do_opensearch(w: &Wiki, c: &Connection, p: &Params) -> Res<Value> {
    let q = p.req("search")?;
    let limit = p.limit("limit", 10, 100);
    let ns = p.ns("namespace", &[0])?.first().copied().unwrap_or(0);
    let rows = queries::suggest_titles(c, q, ns, limit)?;
    Ok(json!([
        q,
        rows.iter().map(|x| x.title.full()).collect::<Vec<_>>(),
        rows.iter().map(|_| "").collect::<Vec<_>>(),
        rows.iter().map(|x| article_url(w, &x.title)).collect::<Vec<_>>(),
    ]))
}

fn rev_of(c: &Connection, p: &Params, rev_key: &str, title_key: &str) -> Res<i64> {
    if let Some(r) = p.int(rev_key) {
        return Ok(r);
    }
    let t = parse_title(p.get(title_key).ok_or_else(|| ApiError::bad("missingparam", format!("one of {rev_key} or {title_key} is required")))?)?;
    Ok(find_page(c, &t)?.ok_or_else(|| ApiError::not_found("missingtitle", format!("page '{}' does not exist", t.full())))?.latest)
}

fn do_compare(c: &Connection, p: &Params) -> Res<Value> {
    let (from, to) = (rev_of(c, p, "fromrev", "fromtitle")?, rev_of(c, p, "torev", "totitle")?);
    let j = compare_json(c, from, to)?;
    Ok(json!({ "compare": {
        "fromid": j["from"]["page_id"], "fromrevid": from, "fromtitle": j["from"]["title"],
        "toid": j["to"]["page_id"], "torevid": to, "totitle": j["to"]["title"],
        "body": j["diff"]["html"], "diff": j["diff"],
    }}))
}

// ---------------------------------------------------------------------------------------
// write actions
// ---------------------------------------------------------------------------------------

fn target_title(c: &Connection, p: &Params, key: &str, id_key: &str) -> Res<Title> {
    if let Some(t) = p.get(key) {
        return parse_title(t);
    }
    if let Some(id) = p.int(id_key) {
        return Ok(find_page_by_id(c, id)?.ok_or_else(|| ApiError::not_found("nosuchpageid", format!("there is no page with id {id}")))?.title);
    }
    Err(ApiError::bad("missingparam", format!("one of the parameters {key}, {id_key} is required")))
}

fn replace_section(cur: &str, sec: &str, new: &str) -> Res<String> {
    let n: usize = sec.parse().map_err(|_| ApiError::bad("badsection", "section must be a number or 'new'"))?;
    let (start, end) = if n == 0 {
        wikitext::lead_range(cur)
    } else {
        let secs = wikitext::raw_sections(cur);
        let s = secs.get(n - 1).ok_or_else(|| ApiError::bad("nosuchsection", format!("there is no section {n}")))?;
        (s.start, s.end)
    };
    let mid = new.trim_end();
    let tail = &cur[end..];
    let sep = if tail.is_empty() || mid.is_empty() { "" } else { "\n\n" };
    Ok(format!("{}{}{}{}", &cur[..start], mid, sep, tail.trim_start_matches('\n')))
}

fn edit_response(r: &wiki::EditResult) -> Value {
    let mut e = json!({ "result": "Success", "pageid": r.page_id, "title": r.title.full(), "contentmodel": "wikitext" });
    if r.merged {
        e["merged"] = json!(true);
    }
    if r.nochange {
        e["nochange"] = json!(true);
    } else {
        e["oldrevid"] = json!(r.old_rev);
        e["newrevid"] = json!(r.rev_id);
        e["newtimestamp"] = json!(iso(r.ts));
        if r.new {
            e["new"] = json!(true);
        }
    }
    json!({ "edit": e })
}

fn do_edit(w: &Wiki, c: &Connection, actor: &Actor, p: &Params) -> Res<Value> {
    let title = target_title(c, p, "title", "pageid")?;
    if let Some(undo) = p.int("undo") {
        let r = w.undo(c, actor, &title, undo, p.int("undoafter"), p.get("summary").unwrap_or(""))?;
        return Ok(edit_response(&r));
    }
    let current = match find_page(c, &title)? {
        Some(pg) => Some(wiki::rev_text(c, pg.latest)?),
        None => None,
    };
    let section = p.get("section");
    let new_text = match (p.get("text"), p.get("appendtext"), p.get("prependtext")) {
        (Some(t), None, None) => match section {
            None => t.to_string(),
            Some("new") => {
                let heading = p.get("sectiontitle").or(p.get("summary")).filter(|h| !h.trim().is_empty()).ok_or_else(|| ApiError::bad("missingparam", "section=new needs sectiontitle (or summary)"))?;
                format!("{}\n\n== {} ==\n{}", current.clone().unwrap_or_default().trim_end(), heading.trim(), t)
            }
            Some(n) => replace_section(current.as_deref().ok_or_else(|| ApiError::not_found("missingtitle", "cannot edit a section of a missing page"))?, n, t)?,
        },
        (None, ap, pre) if ap.is_some() || pre.is_some() => {
            format!("{}{}{}", pre.unwrap_or(""), current.clone().unwrap_or_default(), ap.unwrap_or(""))
        }
        (Some(_), _, _) => return Err(ApiError::bad("invalidparammix", "text cannot be combined with appendtext/prependtext")),
        _ => return Err(ApiError::bad("missingparam", "one of text, appendtext or prependtext is required")),
    };
    let req = EditReq {
        title: title.clone(),
        text: new_text,
        summary: p.get("summary").unwrap_or("").to_string(),
        minor: p.flag("minor"),
        base_rev: p.int("baserevid"),
        create_only: p.flag("createonly"),
        no_create: p.flag("nocreate"),
    };
    let r = w.edit(c, actor, req)?;
    Ok(edit_response(&r))
}

fn do_move(w: &Wiki, c: &Connection, actor: &Actor, p: &Params) -> Res<Value> {
    let from = target_title(c, p, "from", "fromid")?;
    let to = parse_title(p.req("to")?)?;
    let noredirect = p.flag("noredirect");
    let reason = p.get("reason").unwrap_or("");
    w.move_page(c, actor, &from, &to, reason, noredirect)?;
    let mut m = json!({ "from": from.full(), "to": to.full(), "reason": reason });
    if !noredirect {
        m["redirectcreated"] = json!(true);
    }
    Ok(json!({ "move": m }))
}

fn do_delete(w: &Wiki, c: &Connection, actor: &Actor, p: &Params) -> Res<Value> {
    let t = target_title(c, p, "title", "pageid")?;
    let reason = p.get("reason").unwrap_or("");
    w.delete_page(c, actor, &t, reason)?;
    Ok(json!({ "delete": { "title": t.full(), "reason": reason } }))
}

fn do_undelete(w: &Wiki, c: &Connection, actor: &Actor, p: &Params) -> Res<Value> {
    let t = parse_title(p.req("title")?)?;
    let reason = p.get("reason").unwrap_or("");
    let pg = w.undelete_page(c, actor, &t, reason)?;
    Ok(json!({ "undelete": { "title": t.full(), "pageid": pg.id, "reason": reason } }))
}

fn do_protect(w: &Wiki, c: &Connection, actor: &Actor, p: &Params) -> Res<Value> {
    let t = target_title(c, p, "title", "pageid")?;
    let (mut edit, mut mv) = (None, None);
    for item in p.list("protections") {
        match item.split_once('=') {
            Some(("edit", l)) => edit = Some(l.to_string()),
            Some(("move", l)) => mv = Some(l.to_string()),
            _ => return Err(ApiError::bad("badvalue", "protections must look like edit=sysop|move=user")),
        }
    }
    let reason = p.get("reason").unwrap_or("");
    let pg = w.protect(c, actor, &t, edit.as_deref(), mv.as_deref(), reason)?;
    Ok(json!({ "protect": { "title": t.full(), "reason": reason, "protections": protection_json(&pg) } }))
}

fn do_rollback(w: &Wiki, c: &Connection, actor: &Actor, p: &Params) -> Res<Value> {
    let t = target_title(c, p, "title", "pageid")?;
    let (r, undone) = w.rollback(c, actor, &t, p.get("user"), p.get("summary").unwrap_or(""))?;
    Ok(json!({ "rollback": { "title": t.full(), "pageid": r.page_id, "summary": format!("Reverted edits by {undone}"), "revid": r.rev_id, "old_revid": r.old_rev } }))
}

#[allow(dead_code)]
fn _ns_table() -> usize {
    NAMESPACES.len()
}
