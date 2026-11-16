//! JSON shapes shared by the Action-style and REST-style APIs.

use rusqlite::Connection;
use serde_json::{json, Map, Value};

use crate::{
    error::{ApiError, Res},
    title::{ns_name, Title, NAMESPACES},
    util::{iso, path_encode},
    wiki::{PageRow, RevRow, Wiki},
};

pub fn parse_title(s: &str) -> Res<Title> {
    Title::parse(s).map(|p| p.title).map_err(ApiError::bad_title)
}

pub fn article_url(w: &Wiki, t: &Title) -> String {
    w.cfg.article_path.replace("$1", &path_encode(&t.full()))
}

pub fn ns_json(ns: i64) -> Value {
    json!({ "id": ns, "text": ns_name(ns) })
}

pub fn namespaces_json() -> Value {
    let mut m = Map::new();
    for (id, name) in NAMESPACES {
        m.insert(
            id.to_string(),
            json!({ "id": id, "name": if *id == 0 { "(Main)" } else { name }, "canonical": name, "content": *id == 0, "subpages": *id != 0 && id % 2 == 1 || *id == 0 || *id == 2 }),
        );
    }
    Value::Object(m)
}

pub fn protection_json(p: &PageRow) -> Value {
    let lvl = |s: &str| if s.is_empty() { "all".to_string() } else { s.to_string() };
    json!({ "edit": lvl(&p.protect_edit), "move": lvl(&p.protect_move) })
}

pub fn page_info(w: &Wiki, p: &PageRow) -> Value {
    json!({
        "pageid": p.id,
        "ns": p.title.ns,
        "title": p.title.full(),
        "key": p.title.key(),
        "contentmodel": "wikitext",
        "touched": iso(p.touched),
        "lastrevid": p.latest,
        "length": p.len,
        "redirect": p.is_redirect,
        "new": p.latest == 0,
        "protection": protection_json(p),
        "fullurl": article_url(w, &p.title),
    })
}

pub fn rev_json(r: &RevRow) -> Value {
    json!({
        "revid": r.id,
        "parentid": r.parent,
        "timestamp": iso(r.ts),
        "user": r.user_name,
        "anon": r.user_id.is_none(),
        "comment": r.comment,
        "size": r.size,
        "sha256": r.sha256,
        "minor": r.minor,
        "contentmodel": r.model,
    })
}

/// Wikimedia-REST-style page summary.
pub fn summary_json(w: &Wiki, c: &Connection, p: &PageRow, redirected_from: Option<&Title>) -> Res<Value> {
    let (r, _) = w.render_page(c, p)?;
    let rev = crate::wiki::get_rev(c, p.latest)?;
    let display = r.display_title.clone().unwrap_or_else(|| p.title.full());
    let mut v = json!({
        "type": if r.extract.is_empty() { "no-extract" } else { "standard" },
        "title": p.title.key(),
        "displaytitle": display,
        "namespace": ns_json(p.title.ns),
        "titles": { "canonical": p.title.key(), "normalized": p.title.full(), "display": display },
        "pageid": p.id,
        "extract": r.extract,
        "extract_html": r.extract_html,
        "revision": p.latest.to_string(),
        "timestamp": rev.as_ref().map(|r| iso(r.ts)),
        "content_urls": { "page": article_url(w, &p.title) },
    });
    if let Some(d) = &r.description {
        v["description"] = json!(d);
    }
    if let Some(f) = redirected_from {
        v["redirected_from"] = json!(f.key());
    }
    Ok(v)
}

/// "0|14", "Template", "*" -> namespace ids (empty = all namespaces).
pub fn parse_ns_list(s: &str) -> Res<Vec<i64>> {
    let mut out = Vec::new();
    for part in s.split(['|', ',']).map(str::trim).filter(|p| !p.is_empty()) {
        if part == "*" {
            return Ok(Vec::new());
        }
        let ns = part
            .parse::<i64>()
            .ok()
            .filter(|n| NAMESPACES.iter().any(|(i, _)| i == n))
            .or_else(|| crate::title::ns_by_name(part))
            .or_else(|| part.eq_ignore_ascii_case("main").then_some(0))
            .ok_or_else(|| ApiError::bad("badnamespace", format!("unknown namespace '{part}'")))?;
        out.push(ns);
    }
    Ok(out)
}
