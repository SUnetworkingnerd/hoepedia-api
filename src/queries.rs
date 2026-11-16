//! Read-only listing / search queries.

use rusqlite::{params, params_from_iter, types::Value as Sql, Connection};
use serde_json::Value;

use crate::{
    error::Res,
    title::{ns_by_name, Title},
    util::{collapse_ws, html_escape},
    wiki::{page_row, rev_row, PageRow, RevRow, PCOLS, RCOLS},
};

fn ns_filter(col: &str, ns: &[i64]) -> String {
    if ns.is_empty() {
        String::new()
    } else {
        format!(" AND {col} IN ({})", ns.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(","))
    }
}

fn ucfirst(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Upper bound (exclusive) for a prefix range scan.
fn prefix_upper(p: &str) -> String {
    let mut cs: Vec<char> = p.chars().collect();
    while let Some(last) = cs.pop() {
        if let Some(n) = char::from_u32(last as u32 + 1) {
            cs.push(n);
            return cs.into_iter().collect();
        }
    }
    "\u{10FFFF}".to_string()
}

/// "template:fo" -> (10, "Fo")
pub fn split_prefix_query(q: &str, default_ns: i64) -> (i64, String) {
    let q = collapse_ws(&q.replace('_', " "));
    if let Some((p, rest)) = q.split_once(':') {
        if let Some(ns) = ns_by_name(p) {
            return (ns, ucfirst(rest.trim_start()));
        }
    }
    (default_ns, ucfirst(&q))
}

// ---------------------------------------------------------------------------------------
// search
// ---------------------------------------------------------------------------------------

/// Turn free user input into a safe FTS5 expression (every token quoted; AND semantics;
/// `"quoted phrases"` supported). `prefix_last` makes the final bare term a prefix match.
pub fn fts_query(q: &str, prefix_last: bool) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut last_free: Option<usize> = None;
    for (i, seg) in q.split('"').enumerate() {
        let words: Vec<&str> = seg.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
        if words.is_empty() {
            continue;
        }
        if i % 2 == 1 {
            parts.push(format!("\"{}\"", words.join(" ")));
        } else {
            for w in words {
                parts.push(format!("\"{w}\""));
                last_free = Some(parts.len() - 1);
            }
        }
    }
    parts.truncate(16);
    if prefix_last && !q.ends_with(char::is_whitespace) && !q.ends_with('"') {
        if let Some(i) = last_free.filter(|i| *i < parts.len()) {
            parts[i].push('*');
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

pub struct Hit {
    pub page: PageRow,
    /// HTML; matches wrapped in `<span class="searchmatch">`.
    pub snippet: String,
}

pub fn search(c: &Connection, q: &str, ns: &[i64], limit: i64, offset: i64, prefix_last: bool) -> Res<(i64, Vec<Hit>)> {
    let Some(m) = fts_query(q, prefix_last) else { return Ok((0, Vec::new())) };
    let nsf = ns_filter("p.ns", ns);
    let total: i64 = c.query_row(
        &format!("SELECT count(*) FROM search JOIN page p ON p.id=search.rowid WHERE search MATCH ?1 AND p.deleted=0 AND p.is_redirect=0{nsf}"),
        params![m],
        |r| r.get(0),
    )?;
    let mut st = c.prepare(&format!(
        "SELECT {PCOLS}, snippet(search,1,char(1),char(2),'…',24)
         FROM search JOIN page p ON p.id=search.rowid
         WHERE search MATCH ?1 AND p.deleted=0 AND p.is_redirect=0{nsf}
         ORDER BY bm25(search,8.0,1.0) LIMIT ?2 OFFSET ?3"
    ))?;
    let hits = st
        .query_map(params![m, limit, offset], |r| {
            let snip: String = r.get(13)?;
            Ok(Hit {
                page: page_row(r)?,
                snippet: html_escape(&snip).replace('\u{1}', "<span class=\"searchmatch\">").replace('\u{2}', "</span>"),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok((total, hits))
}

pub fn prefix_search(c: &Connection, q: &str, default_ns: i64, limit: i64, offset: i64) -> Res<Vec<PageRow>> {
    let (ns, prefix) = split_prefix_query(q, default_ns);
    all_pages(c, ns, &prefix, "", limit, offset, "all")
}

/// Autocomplete: prefix matches first, then titles that merely contain the text.
pub fn suggest_titles(c: &Connection, q: &str, default_ns: i64, limit: i64) -> Res<Vec<PageRow>> {
    let mut out = prefix_search(c, q, default_ns, limit, 0)?;
    if (out.len() as i64) < limit {
        let (ns, needle) = split_prefix_query(q, default_ns);
        let like = format!("%{}%", needle.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
        let mut st = c.prepare(&format!(
            "SELECT {PCOLS} FROM page p WHERE p.ns=?1 AND p.deleted=0 AND p.title LIKE ?2 ESCAPE '\\' ORDER BY length(p.title), p.title LIMIT ?3"
        ))?;
        let more = st.query_map(params![ns, like, limit], page_row)?.collect::<Result<Vec<_>, _>>()?;
        for p in more {
            if (out.len() as i64) >= limit {
                break;
            }
            if !out.iter().any(|o| o.id == p.id) {
                out.push(p);
            }
        }
    }
    if (out.len() as i64) < limit {
        // Accent/case-insensitive word-prefix match on the title column ("eco" -> "École").
        let (ns, needle) = split_prefix_query(q, default_ns);
        if let Some(m) = fts_query(&needle, true) {
            let mut st = c.prepare(&format!(
                "SELECT {PCOLS} FROM search JOIN page p ON p.id=search.rowid
                 WHERE search MATCH ?1 AND p.deleted=0 AND p.ns=?2 ORDER BY bm25(search,8.0,1.0) LIMIT ?3"
            ))?;
            let more = st.query_map(params![format!("title : ({m})"), ns, limit], page_row)?.collect::<Result<Vec<_>, _>>()?;
            for p in more {
                if (out.len() as i64) < limit && !out.iter().any(|o| o.id == p.id) {
                    out.push(p);
                }
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------
// lists
// ---------------------------------------------------------------------------------------

/// `filter`: all | redirects | nonredirects
pub fn all_pages(c: &Connection, ns: i64, prefix: &str, from: &str, limit: i64, offset: i64, filter: &str) -> Res<Vec<PageRow>> {
    let lower = if from > prefix { from.to_string() } else { prefix.to_string() };
    let upper = if prefix.is_empty() { "\u{10FFFF}".to_string() } else { prefix_upper(prefix) };
    let redir = match filter {
        "redirects" => " AND p.is_redirect=1",
        "nonredirects" => " AND p.is_redirect=0",
        _ => "",
    };
    let mut st = c.prepare(&format!(
        "SELECT {PCOLS} FROM page p WHERE p.ns=?1 AND p.deleted=0 AND p.title>=?2 AND p.title<?3{redir} ORDER BY p.title LIMIT ?4 OFFSET ?5"
    ))?;
    let rows = st.query_map(params![ns, lower, upper, limit, offset], page_row)?.collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// `kind`: all | page | subcat | file
pub fn category_members(c: &Connection, cat: &str, kind: &str, limit: i64, offset: i64) -> Res<Vec<(PageRow, String)>> {
    let f = match kind {
        "subcat" => " AND p.ns=14",
        "file" => " AND p.ns=6",
        "page" => " AND p.ns NOT IN (6,14)",
        _ => "",
    };
    let mut st = c.prepare(&format!(
        "SELECT {PCOLS}, cl.sortkey FROM categorylink cl JOIN page p ON p.id=cl.from_page
         WHERE cl.cat=?1 AND p.deleted=0{f} ORDER BY cl.sortkey, p.title LIMIT ?2 OFFSET ?3"
    ))?;
    let rows = st.query_map(params![cat, limit, offset], |r| Ok((page_row(r)?, r.get::<_, String>(13)?)))?.collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// (name, member count, category page exists)
pub fn all_categories(c: &Connection, prefix: &str, limit: i64, offset: i64) -> Res<Vec<(String, i64, bool)>> {
    let lower = ucfirst(prefix);
    let upper = if lower.is_empty() { "\u{10FFFF}".to_string() } else { prefix_upper(&lower) };
    let mut st = c.prepare(
        "SELECT cl.cat, count(*), EXISTS(SELECT 1 FROM page q WHERE q.ns=14 AND q.title=cl.cat AND q.deleted=0)
         FROM categorylink cl JOIN page p ON p.id=cl.from_page
         WHERE p.deleted=0 AND cl.cat>=?1 AND cl.cat<?2 GROUP BY cl.cat ORDER BY cl.cat LIMIT ?3 OFFSET ?4",
    )?;
    let rows = st.query_map(params![lower, upper, limit, offset], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0)))?.collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Pages that link to (`pagelink`) or transclude (`templatelink`) `target`.
pub fn pages_linking_to(c: &Connection, table: &str, target: &Title, ns: &[i64], limit: i64, offset: i64) -> Res<Vec<PageRow>> {
    let table = if table == "templatelink" { "templatelink" } else { "pagelink" };
    let nsf = ns_filter("p.ns", ns);
    let mut st = c.prepare(&format!(
        "SELECT DISTINCT {PCOLS} FROM {table} l JOIN page p ON p.id=l.from_page
         WHERE l.to_ns=?1 AND l.to_title=?2 AND p.deleted=0{nsf} ORDER BY p.ns, p.title LIMIT ?3 OFFSET ?4"
    ))?;
    let rows = st.query_map(params![target.ns, target.name, limit, offset], page_row)?.collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Outgoing links or transclusions of a page: (title, exists)
pub fn page_outlinks(c: &Connection, table: &str, page_id: i64, limit: i64, offset: i64) -> Res<Vec<(Title, bool)>> {
    let table = if table == "templatelink" { "templatelink" } else { "pagelink" };
    let mut st = c.prepare(&format!(
        "SELECT l.to_ns, l.to_title, EXISTS(SELECT 1 FROM page q WHERE q.ns=l.to_ns AND q.title=l.to_title AND q.deleted=0)
         FROM {table} l WHERE l.from_page=?1 ORDER BY l.to_ns, l.to_title LIMIT ?2 OFFSET ?3"
    ))?;
    let rows = st
        .query_map(params![page_id, limit, offset], |r| Ok((Title { ns: r.get(0)?, name: r.get(1)? }, r.get::<_, i64>(2)? != 0)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn page_categories(c: &Connection, page_id: i64, limit: i64, offset: i64) -> Res<Vec<(String, String)>> {
    let mut st = c.prepare("SELECT cat, sortkey FROM categorylink WHERE from_page=?1 ORDER BY cat LIMIT ?2 OFFSET ?3")?;
    let rows = st.query_map(params![page_id, limit, offset], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn random_pages(c: &Connection, ns: &[i64], limit: i64) -> Res<Vec<PageRow>> {
    let nsf = if ns.is_empty() { " AND p.ns=0".to_string() } else { ns_filter("p.ns", ns) };
    let mut st = c.prepare(&format!("SELECT {PCOLS} FROM page p WHERE p.deleted=0 AND p.is_redirect=0{nsf} ORDER BY random() LIMIT ?1"))?;
    let rows = st.query_map(params![limit], page_row)?.collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

// ---------------------------------------------------------------------------------------
// history, changes, logs
// ---------------------------------------------------------------------------------------

/// Revision history. `cursor` is a revision id: with `newer=false` we return revisions older
/// than it (descending), otherwise newer than it (ascending).
pub fn history(c: &Connection, page_id: i64, limit: i64, cursor: Option<i64>, newer: bool, user: Option<&str>) -> Res<Vec<RevRow>> {
    let (cmp, ord) = if newer { (">", "ASC") } else { ("<", "DESC") };
    let mut st = c.prepare(&format!(
        "SELECT {RCOLS} FROM revision r WHERE r.page=?1 AND (?2 IS NULL OR r.id {cmp} ?2) AND (?3 IS NULL OR r.user_name=?3 COLLATE NOCASE)
         ORDER BY r.id {ord} LIMIT ?4"
    ))?;
    let rows = st.query_map(params![page_id, cursor, user, limit], rev_row)?.collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub struct HistoryCounts {
    pub total: i64,
    pub minor: i64,
    pub anonymous: i64,
    pub editors: i64,
}

pub fn history_counts(c: &Connection, page_id: i64) -> Res<HistoryCounts> {
    Ok(c.query_row(
        "SELECT count(*), coalesce(sum(minor),0), coalesce(sum(user_id IS NULL),0), count(DISTINCT user_name) FROM revision WHERE page=?1",
        params![page_id],
        |r| Ok(HistoryCounts { total: r.get(0)?, minor: r.get(1)?, anonymous: r.get(2)?, editors: r.get(3)? }),
    )?)
}

#[derive(Clone, Debug)]
pub struct Change {
    /// "new" | "edit" | "log"
    pub kind: &'static str,
    pub ts: i64,
    pub id: i64,
    pub ns: i64,
    pub title: String,
    pub user: String,
    pub comment: String,
    pub page_id: i64,
    pub rev_id: i64,
    pub old_rev: i64,
    pub oldlen: i64,
    pub newlen: i64,
    pub minor: bool,
    pub log_type: String,
    pub log_action: String,
}

pub fn recent_changes(c: &Connection, ns: &[i64], types: &[String], user: Option<&str>, limit: i64, offset: i64) -> Res<Vec<Change>> {
    recent_changes_in(c, ns, types, user, (i64::MIN, i64::MAX), false, limit, offset)
}

/// Changes with `lo <= ts <= hi`; newest first, or oldest first with `oldest_first`.
#[allow(clippy::too_many_arguments)]
pub fn recent_changes_in(c: &Connection, ns: &[i64], types: &[String], user: Option<&str>, (lo, hi): (i64, i64), oldest_first: bool, limit: i64, offset: i64) -> Res<Vec<Change>> {
    let want = |t: &str| types.is_empty() || types.iter().any(|x| x == t);
    let order = if oldest_first { "ASC" } else { "DESC" };
    let take = limit + offset;
    let mut out: Vec<Change> = Vec::new();
    if want("edit") || want("new") {
        let mut st = c.prepare(&format!(
            "SELECT r.id,r.ts,p.ns,p.title,r.user_name,r.comment,p.id,r.parent,COALESCE(pr.size,0),r.size,r.minor
             FROM revision r JOIN page p ON p.id=r.page LEFT JOIN revision pr ON pr.id=r.parent
             WHERE p.deleted=0{} AND (?1 IS NULL OR r.user_name=?1 COLLATE NOCASE) AND r.ts BETWEEN ?3 AND ?4 ORDER BY r.id {order} LIMIT ?2",
            ns_filter("p.ns", ns)
        ))?;
        let rows = st.query_map(params![user, take, lo, hi], |r| {
            let parent: i64 = r.get(7)?;
            Ok(Change {
                kind: if parent == 0 { "new" } else { "edit" },
                ts: r.get(1)?,
                id: r.get(0)?,
                ns: r.get(2)?,
                title: r.get(3)?,
                user: r.get(4)?,
                comment: r.get(5)?,
                page_id: r.get(6)?,
                rev_id: r.get(0)?,
                old_rev: parent,
                oldlen: r.get(8)?,
                newlen: r.get(9)?,
                minor: r.get::<_, i64>(10)? != 0,
                log_type: String::new(),
                log_action: String::new(),
            })
        })?;
        for ch in rows {
            let ch = ch?;
            if want(ch.kind) {
                out.push(ch);
            }
        }
    }
    if want("log") {
        for l in log_events(c, None, None, user, ns, if lo > i64::MIN { take.max(5000) } else { take }, 0)? {
            if l.ts < lo || l.ts > hi {
                continue;
            }
            out.push(Change {
                kind: "log",
                ts: l.ts,
                id: l.id,
                ns: l.ns,
                title: l.title,
                user: l.user,
                comment: l.comment,
                page_id: 0,
                rev_id: 0,
                old_rev: 0,
                oldlen: 0,
                newlen: 0,
                minor: false,
                log_type: l.log_type,
                log_action: l.action,
            });
        }
    }
    out.sort_by(|a, b| b.ts.cmp(&a.ts).then(b.id.cmp(&a.id)));
    if oldest_first {
        out.reverse();
    }
    Ok(out.into_iter().skip(offset as usize).take(limit as usize).collect())
}

/// Changes after the cursor `(rev_id, log_id)`, oldest first, plus the advanced cursor.
/// Each returned change carries the cursor *after* it in `Change.id`-independent `cursor`.
pub fn changes_since(c: &Connection, rev_after: i64, log_after: i64, limit: i64) -> Res<(Vec<(Change, (i64, i64))>, (i64, i64))> {
    let mut items: Vec<Change> = Vec::new();
    let mut st = c.prepare(
        "SELECT r.id,r.ts,p.ns,p.title,r.user_name,r.comment,p.id,r.parent,COALESCE(pr.size,0),r.size,r.minor
         FROM revision r JOIN page p ON p.id=r.page LEFT JOIN revision pr ON pr.id=r.parent
         WHERE r.id>?1 ORDER BY r.id ASC LIMIT ?2",
    )?;
    let rows = st.query_map(params![rev_after, limit], |r| {
        let parent: i64 = r.get(7)?;
        Ok(Change {
            kind: if parent == 0 { "new" } else { "edit" },
            ts: r.get(1)?,
            id: r.get(0)?,
            ns: r.get(2)?,
            title: r.get(3)?,
            user: r.get(4)?,
            comment: r.get(5)?,
            page_id: r.get(6)?,
            rev_id: r.get(0)?,
            old_rev: parent,
            oldlen: r.get(8)?,
            newlen: r.get(9)?,
            minor: r.get::<_, i64>(10)? != 0,
            log_type: String::new(),
            log_action: String::new(),
        })
    })?;
    for ch in rows {
        items.push(ch?);
    }
    let mut st = c.prepare("SELECT id,ts,type,action,user_name,ns,title,comment FROM log WHERE id>?1 ORDER BY id ASC LIMIT ?2")?;
    let rows = st.query_map(params![log_after, limit], |r| {
        Ok(Change {
            kind: "log",
            ts: r.get(1)?,
            id: r.get(0)?,
            ns: r.get(5)?,
            title: r.get(6)?,
            user: r.get(4)?,
            comment: r.get(7)?,
            page_id: 0,
            rev_id: 0,
            old_rev: 0,
            oldlen: 0,
            newlen: 0,
            minor: false,
            log_type: r.get(2)?,
            log_action: r.get(3)?,
        })
    })?;
    for ch in rows {
        items.push(ch?);
    }
    items.sort_by(|a, b| a.ts.cmp(&b.ts).then((a.kind == "log").cmp(&(b.kind == "log"))).then(a.id.cmp(&b.id)));
    items.truncate(limit as usize);
    let mut cur = (rev_after, log_after);
    let out = items
        .into_iter()
        .map(|ch| {
            if ch.kind == "log" {
                cur.1 = cur.1.max(ch.id);
            } else {
                cur.0 = cur.0.max(ch.id);
            }
            (ch, cur)
        })
        .collect();
    Ok((out, cur))
}

pub fn current_cursor(c: &Connection) -> Res<(i64, i64)> {
    Ok((
        c.query_row("SELECT COALESCE(max(id),0) FROM revision", [], |r| r.get(0))?,
        c.query_row("SELECT COALESCE(max(id),0) FROM log", [], |r| r.get(0))?,
    ))
}

pub struct Contrib {
    pub rev: RevRow,
    pub title: Title,
    pub oldlen: i64,
}

pub fn contribs(c: &Connection, user: &str, limit: i64, cursor: Option<i64>) -> Res<Vec<Contrib>> {
    let mut st = c.prepare(&format!(
        "SELECT {RCOLS}, p.ns, p.title, COALESCE(pr.size,0) FROM revision r JOIN page p ON p.id=r.page LEFT JOIN revision pr ON pr.id=r.parent
         WHERE r.user_name=?1 COLLATE NOCASE AND p.deleted=0 AND (?2 IS NULL OR r.id<?2) ORDER BY r.id DESC LIMIT ?3"
    ))?;
    let rows = st
        .query_map(params![user, cursor, limit], |r| Ok(Contrib { rev: rev_row(r)?, title: Title { ns: r.get(11)?, name: r.get(12)? }, oldlen: r.get(13)? }))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub struct LogRow {
    pub id: i64,
    pub ts: i64,
    pub log_type: String,
    pub action: String,
    pub user: String,
    pub ns: i64,
    pub title: String,
    pub comment: String,
    pub params: Value,
}

pub fn log_events(c: &Connection, typ: Option<&str>, title: Option<&Title>, user: Option<&str>, ns: &[i64], limit: i64, offset: i64) -> Res<Vec<LogRow>> {
    let mut q = String::from("SELECT id,ts,type,action,user_name,ns,title,comment,params FROM log WHERE 1=1");
    let mut args: Vec<Sql> = Vec::new();
    if let Some(t) = typ {
        args.push(Sql::Text(t.to_string()));
        q.push_str(&format!(" AND type=?{}", args.len()));
    }
    if let Some(t) = title {
        args.push(Sql::Integer(t.ns));
        q.push_str(&format!(" AND ns=?{}", args.len()));
        args.push(Sql::Text(t.name.clone()));
        q.push_str(&format!(" AND title=?{}", args.len()));
    }
    if let Some(u) = user {
        args.push(Sql::Text(u.to_string()));
        q.push_str(&format!(" AND user_name=?{} COLLATE NOCASE", args.len()));
    }
    q.push_str(&ns_filter("ns", ns));
    args.push(Sql::Integer(limit));
    q.push_str(&format!(" ORDER BY id DESC LIMIT ?{}", args.len()));
    args.push(Sql::Integer(offset));
    q.push_str(&format!(" OFFSET ?{}", args.len()));
    let mut st = c.prepare(&q)?;
    let rows = st
        .query_map(params_from_iter(args), |r| {
            let p: String = r.get(8)?;
            Ok(LogRow {
                id: r.get(0)?,
                ts: r.get(1)?,
                log_type: r.get(2)?,
                action: r.get(3)?,
                user: r.get(4)?,
                ns: r.get(5)?,
                title: r.get(6)?,
                comment: r.get(7)?,
                params: serde_json::from_str(&p).unwrap_or(Value::Null),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub struct Stats {
    pub pages: i64,
    pub articles: i64,
    pub edits: i64,
    pub users: i64,
}

pub fn stats(c: &Connection) -> Res<Stats> {
    Ok(Stats {
        pages: c.query_row("SELECT count(*) FROM page WHERE deleted=0", [], |r| r.get(0))?,
        articles: c.query_row("SELECT count(*) FROM page WHERE deleted=0 AND ns=0 AND is_redirect=0", [], |r| r.get(0))?,
        edits: c.query_row("SELECT count(*) FROM revision", [], |r| r.get(0))?,
        users: c.query_row("SELECT count(*) FROM user", [], |r| r.get(0))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wiki::testutil::*;

    #[test]
    fn fts_query_is_safe() {
        assert_eq!(fts_query("rust lang", false).unwrap(), "\"rust\" \"lang\"");
        assert_eq!(fts_query("rust la", true).unwrap(), "\"rust\" \"la\"*");
        assert_eq!(fts_query("rust ", true).unwrap(), "\"rust\"");
        assert_eq!(fts_query("\"borrow checker\" OR NEAR(", false).unwrap(), "\"borrow checker\" \"OR\" \"NEAR\"");
        assert_eq!(fts_query("*** :: ()", false), None);
        assert_eq!(fts_query("a\"b", false).unwrap(), "\"a\" \"b\"");
    }

    #[test]
    fn search_and_prefix_end_to_end() {
        let t = make(false);
        let c = t.wiki.db.writer();
        t.wiki.edit(&c, &t.admin, req("Rust (language)", "Rust is a systems programming language with a borrow checker.")).unwrap();
        t.wiki.edit(&c, &t.admin, req("Rusty nails", "A different page about nails.")).unwrap();
        t.wiki.edit(&c, &t.admin, req("Template:Rustbox", "A template about rust.")).unwrap();
        let (total, hits) = search(&c, "borrow checker", &[0], 10, 0, false).unwrap();
        assert_eq!(total, 1);
        assert_eq!(hits[0].page.title.name, "Rust (language)");
        assert!(hits[0].snippet.contains("<span class=\"searchmatch\">"), "{}", hits[0].snippet);
        let (total, _) = search(&c, "rust", &[], 10, 0, false).unwrap();
        assert_eq!(total, 2); // main page + template, not "Rusty"
        let (_, hits) = search(&c, "rus", &[0], 10, 0, true).unwrap();
        assert_eq!(hits.len(), 2);
        let p = prefix_search(&c, "rus", 0, 10, 0).unwrap();
        assert_eq!(p.iter().map(|p| p.title.name.as_str()).collect::<Vec<_>>(), vec!["Rust (language)", "Rusty nails"]);
        let p = prefix_search(&c, "template:rustb", 0, 10, 0).unwrap();
        assert_eq!(p.len(), 1);
        let s = suggest_titles(&c, "nails", 0, 10).unwrap();
        assert_eq!(s[0].title.name, "Rusty nails");
        // evil input never errors
        assert!(search(&c, "\" OR 1=1 -- NEAR(a b", &[], 10, 0, true).is_ok());
    }

    #[test]
    fn change_feed_is_ordered_and_resumable() {
        let t = make(false);
        let c = t.wiki.db.writer();
        let start = current_cursor(&c).unwrap();
        assert_eq!(start, (0, 0));
        t.wiki.edit(&c, &t.admin, req("A", "one")).unwrap();
        t.wiki.edit(&c, &t.admin, req("A", "two")).unwrap();
        t.wiki.protect(&c, &t.admin, &Title::parse("A").unwrap().title, Some("sysop"), None, "x").unwrap();
        t.wiki.edit(&c, &t.admin, req("B", "b")).unwrap();
        let (first, cur) = changes_since(&c, 0, 0, 2).unwrap();
        assert_eq!(first.len(), 2);
        let (rest, end) = changes_since(&c, cur.0, cur.1, 100).unwrap();
        assert_eq!(first.len() + rest.len(), 4, "nothing lost or duplicated across pages");
        let kinds: Vec<_> = first.iter().chain(rest.iter()).map(|(c, _)| c.kind).collect();
        assert_eq!(kinds.iter().filter(|k| **k == "log").count(), 1);
        assert_eq!(changes_since(&c, end.0, end.1, 100).unwrap().0.len(), 0);
        assert_eq!(current_cursor(&c).unwrap(), end);
    }

    #[test]
    fn recent_changes_history_categories() {
        let t = make(false);
        let c = t.wiki.db.writer();
        t.wiki.edit(&c, &t.admin, req("A", "one [[Category:C]]")).unwrap();
        t.wiki.edit(&c, &t.admin, req("A", "two [[Category:C]]")).unwrap();
        t.wiki.edit(&c, &t.admin, req("B", "b [[Category:C|zz]]")).unwrap();
        let rc = recent_changes(&c, &[], &[], None, 10, 0).unwrap();
        assert_eq!(rc.iter().map(|c| c.kind).collect::<Vec<_>>(), vec!["new", "edit", "new"]);
        assert_eq!(rc[1].oldlen, 18);
        let pid = crate::wiki::find_page(&c, &Title::parse("A").unwrap().title).unwrap().unwrap().id;
        let h = history(&c, pid, 10, None, false, None).unwrap();
        assert_eq!(h.len(), 2);
        assert!(h[0].id > h[1].id);
        let older = history(&c, pid, 10, Some(h[0].id), false, None).unwrap();
        assert_eq!(older.len(), 1);
        let m = category_members(&c, "C", "all", 10, 0).unwrap();
        assert_eq!(m.iter().map(|(p, _)| p.title.name.as_str()).collect::<Vec<_>>(), vec!["A", "B"]);
        let cats = all_categories(&c, "", 10, 0).unwrap();
        assert_eq!(cats, vec![("C".to_string(), 2, false)]);
    }
}
