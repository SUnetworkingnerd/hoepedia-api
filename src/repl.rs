//! Live replication for read replicas.
//!
//! The primary keeps `repl_log`: one row per (table, key) with the sequence number of its
//! latest change (maintained by connection-local triggers, see `db.rs`). A replica repeatedly
//! asks "what changed after seq N?" and receives the *current* rows for every changed key (or
//! nothing for a key, meaning "deleted"). It applies everything up to the primary's head in a
//! single transaction, so readers on the replica only ever see states the primary really had.
//!
//! A brand-new replica starts at seq 0 and therefore receives the whole wiki: there is no
//! separate bootstrap step. Derived data that is not a plain table (the full-text index) is
//! rebuilt locally for the pages that changed.

use std::{
    collections::{BTreeSet, HashSet},
    io::Read,
    sync::atomic::Ordering,
    time::Duration,
};

use base64::Engine;
use rusqlite::{types::ValueRef, Connection, TransactionBehavior};
use serde_json::{json, Value};

use crate::{
    db::{get_state, set_state, REPLICATED},
    error::{ApiError, Res},
    util::now,
    wiki::Wiki,
};

#[derive(Default, Clone)]
pub struct Status {
    pub applied_seq: i64,
    pub primary_head: i64,
    pub last_sync: i64,
    pub last_error: String,
    pub rows_applied: u64,
}

pub fn head(c: &Connection) -> Res<i64> {
    Ok(c.query_row("SELECT coalesce(max(seq),0) FROM repl_log", [], |r| r.get(0))?)
}

fn key_col(table: &str) -> Option<&'static str> {
    REPLICATED.iter().find(|(t, _)| *t == table).map(|(_, k)| *k)
}

fn to_json(v: ValueRef) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => json!(i),
        ValueRef::Real(f) => json!(f),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => json!({ "$b": base64::engine::general_purpose::STANDARD.encode(b) }),
    }
}

fn from_json(v: &Value) -> Result<rusqlite::types::Value, String> {
    use rusqlite::types::Value as V;
    Ok(match v {
        Value::Null => V::Null,
        Value::Bool(b) => V::Integer(*b as i64),
        Value::Number(n) => match n.as_i64() {
            Some(i) => V::Integer(i),
            None => V::Real(n.as_f64().ok_or("bad number")?),
        },
        Value::String(s) => V::Text(s.clone()),
        Value::Object(o) => {
            let b = o.get("$b").and_then(Value::as_str).ok_or("bad value object")?;
            V::Blob(base64::engine::general_purpose::STANDARD.decode(b).map_err(|e| e.to_string())?)
        }
        Value::Array(_) => return Err("unexpected array value".into()),
    })
}

/// Everything that changed after `since`, read in one snapshot. Stops early (with
/// `more: true`) once roughly `max_bytes` of row data has been collected.
pub fn changes(c: &Connection, since: i64, max_bytes: usize) -> Res<Value> {
    let tx = c.unchecked_transaction()?;
    let head = head(&tx)?;
    let db_id = get_state(&tx, "meta", "db_id")?.unwrap_or_default();
    let history = term_history(&tx);
    let mut out: Vec<Value> = Vec::new();
    let (mut bytes, mut to, mut more) = (0usize, since, false);
    {
        let mut log = tx.prepare("SELECT seq, tbl, k FROM repl_log WHERE seq > ?1 ORDER BY seq")?;
        let mut rows = log.query([since])?;
        while let Some(r) = rows.next()? {
            if bytes >= max_bytes {
                more = true;
                break;
            }
            let (seq, tbl): (i64, String) = (r.get(0)?, r.get(1)?);
            let k: rusqlite::types::Value = r.get(2)?;
            to = seq;
            let Some(kc) = key_col(&tbl) else { continue };
            let mut st = tx.prepare_cached(&format!("SELECT * FROM {tbl} WHERE {kc} IS ?1"))?;
            let cols: Vec<String> = st.column_names().iter().map(|s| s.to_string()).collect();
            let mut data = st.query([&k])?;
            let mut list = Vec::new();
            while let Some(row) = data.next()? {
                let mut vals = Vec::with_capacity(cols.len());
                for i in 0..cols.len() {
                    let v = row.get_ref(i)?;
                    bytes += match v {
                        ValueRef::Text(t) => t.len(),
                        ValueRef::Blob(b) => b.len() * 4 / 3,
                        _ => 8,
                    };
                    vals.push(to_json(v));
                }
                list.push(Value::Array(vals));
            }
            bytes += 32;
            out.push(json!({ "seq": seq, "t": tbl, "k": to_json(ValueRef::from(&k)), "cols": cols, "rows": list }));
        }
    }
    Ok(json!({ "db_id": db_id, "since": since, "to": to, "head": head, "more": more, "changes": out, "term_history": history }))
}

pub fn status_json(w: &Wiki, c: &Connection) -> Res<Value> {
    let head = head(c)?;
    let db_id = get_state(c, "meta", "db_id")?.unwrap_or_default();
    let term = w.term.load(Ordering::SeqCst);
    Ok(match w.upstream() {
        None => {
            let acks: Vec<Value> = w
                .acks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .filter(|(_, (_, at))| at.elapsed().as_secs() < 90)
                .map(|(node, (seq, at))| json!({ "node": node, "applied": seq, "behind": (head - seq).max(0), "seconds_ago": at.elapsed().as_secs() }))
                .collect();
            let mut v = json!({ "role": "primary", "db_id": db_id, "head": head, "term": term, "node_id": w.node_id, "replicas": acks, "min_acks": required_acks(w) });
            if quorum_mode(w) {
                v["quorum"] = json!({ "cluster_size": cluster_size(w), "majority": majority(w), "accepting_writes": lease_ok(w) });
            }
            v
        }
        Some(primary) => {
            let s = w.repl.lock().unwrap_or_else(|e| e.into_inner()).clone();
            json!({
                "role": "replica", "term": term, "node_id": w.node_id,
                "primary": primary, "db_id": db_id, "applied": s.applied_seq, "primary_head": s.primary_head,
                "behind": (s.primary_head - s.applied_seq).max(0),
                "last_sync": if s.last_sync > 0 { json!(crate::util::iso(s.last_sync)) } else { Value::Null },
                "seconds_since_sync": if s.last_sync > 0 { json!(now() - s.last_sync) } else { Value::Null },
                "rows_applied": s.rows_applied,
                "last_error": if s.last_error.is_empty() { Value::Null } else { json!(s.last_error) },
            })
        }
    })
}

// ---------------------------------------------------------------------------------------
// replica side
// ---------------------------------------------------------------------------------------

const MAX_RESPONSE_BYTES: u64 = 512 * 1024 * 1024;

fn fetch(agent: &ureq::Agent, w: &Wiki, since: i64, wait: u64) -> Result<Value, String> {
    let base = w.upstream().unwrap_or_default();
    let url = format!("{base}/rest/v1/replication/changes?since={since}&wait={wait}&node={}", w.node_id);
    let resp = agent.get(&url).set("Authorization", &format!("Bearer {}", w.cfg.replica_token)).call().map_err(|e| match e {
        ureq::Error::Status(code, r) => {
            let body = r.into_string().unwrap_or_default();
            format!("primary answered HTTP {code}: {}", body.chars().take(200).collect::<String>())
        }
        other => format!("cannot reach primary: {other}"),
    })?;
    let mut body = String::new();
    resp.into_reader().take(MAX_RESPONSE_BYTES).read_to_string(&mut body).map_err(|e| format!("reading primary response: {e}"))?;
    serde_json::from_str(&body).map_err(|e| format!("invalid response from primary: {e}"))
}

fn ident(s: &str) -> bool {
    !s.is_empty() && s.len() < 64 && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

struct Pending {
    cols: Vec<String>,
    rows: Vec<Vec<rusqlite::types::Value>>,
}

fn parse_rows(ch: &Value) -> Result<Pending, String> {
    let cols: Vec<String> = ch["cols"].as_array().ok_or("missing cols")?.iter().filter_map(|c| c.as_str().map(String::from)).collect();
    if cols.is_empty() || !cols.iter().all(|c| ident(c)) {
        return Err("bad column list".into());
    }
    let mut rows = Vec::new();
    for r in ch["rows"].as_array().ok_or("missing rows")? {
        let vals = r.as_array().ok_or("bad row")?.iter().map(from_json).collect::<Result<Vec<_>, _>>()?;
        if vals.len() != cols.len() {
            return Err("row/column count mismatch".into());
        }
        rows.push(vals);
    }
    Ok(Pending { cols, rows })
}

fn insert_rows(c: &Connection, table: &str, p: &Pending) -> Result<(), String> {
    if p.rows.is_empty() {
        return Ok(());
    }
    let marks: Vec<String> = (1..=p.cols.len()).map(|i| format!("?{i}")).collect();
    let mut st = c
        .prepare_cached(&format!("INSERT INTO {table}({}) VALUES({})", p.cols.join(","), marks.join(",")))
        .map_err(|e| format!("{table}: {e}"))?;
    for r in &p.rows {
        st.execute(rusqlite::params_from_iter(r.iter())).map_err(|e| format!("{table}: {e}"))?;
    }
    Ok(())
}

fn wipe(c: &Connection) -> Result<(), String> {
    for (t, _) in REPLICATED {
        c.execute_batch(&format!("DELETE FROM {t}")).map_err(|e| e.to_string())?;
    }
    c.execute_batch("DELETE FROM repl_log; DELETE FROM search;").map_err(|e| e.to_string())
}

/// One full catch-up: returns how many keys were applied. Holds the writer for the duration
/// (nothing else writes on a replica); readers keep seeing the previous snapshot until commit.
pub(crate) fn sync_once(w: &Wiki, fetch: &mut dyn FnMut(i64, u64) -> Result<Value, String>, wait: u64) -> Result<usize, String> {
    let e = |x: rusqlite::Error| x.to_string();
    let (mut since, known_id) = {
        let g = w.db.writer();
        let s = get_state(&g, "local_state", "replica_seq").map_err(e)?.and_then(|v| v.parse().ok()).unwrap_or(0i64);
        (s, get_state(&g, "local_state", "primary_db_id").map_err(e)?)
    };
    let force = w.repl_resync.swap(false, Ordering::SeqCst);
    let mut batch = fetch(if force { 0 } else { since }, if force { 0 } else { wait })?;

    let mut g = w.db.writer();
    let tx = g.transaction_with_behavior(TransactionBehavior::Immediate).map_err(e)?;
    tx.execute_batch("PRAGMA defer_foreign_keys=ON").map_err(e)?;
    let mut applied = 0usize;
    let mut pages: Vec<(rusqlite::types::Value, Pending)> = Vec::new();
    let mut touched: BTreeSet<i64> = BTreeSet::new();
    let mut seen_pages: HashSet<i64> = HashSet::new();
    let mut reset_done = false;
    #[allow(unused_assignments)]
    let mut sync_mode = false;
    loop {
        let db_id = batch["db_id"].as_str().unwrap_or_default().to_string();
        let head = batch["head"].as_i64().ok_or("response has no head")?;
        let their_term = batch["term"].as_i64().unwrap_or(0);
        if their_term < w.term.load(Ordering::SeqCst) {
            // It was primary before a fail-over and has not noticed yet: never copy from it.
            return Err(format!("upstream is stale (its term {their_term} is older than ours)"));
        }
        sync_mode = batch["sync"].as_bool() == Some(true);
        if db_id.is_empty() {
            return Err("the primary did not report a database id (is it running a version with replication?)".into());
        }
        let different = known_id.as_deref().is_some_and(|k| k != db_id);
        // After a fail-over, writes this node took from an old primary that the new primary
        // never had must go: they show as a position past the start of a newer term.
        let diverged = !reset_done && since > 0 && {
            let upstream: Vec<(i64, i64)> = serde_json::from_value(batch["term_history"].clone()).unwrap_or_default();
            let mine = term_of(&term_history(&tx), since);
            upstream.iter().filter(|(t, _)| *t > mine).min_by_key(|(t, _)| *t).is_some_and(|(_, start)| since > *start)
        };
        if diverged {
            tracing::warn!(since, "this copy has changes the current primary does not have (from before a fail-over); copying everything again");
        }
        if !reset_done && (force || different || diverged || head < since) {
            // The primary is a different database (or was restored to an earlier state):
            // start over from an empty copy.
            tracing::warn!(different, head, since, "replica is resynchronising from scratch");
            wipe(&tx)?;
            pages.clear();
            touched.clear();
            reset_done = true;
            if batch["since"].as_i64() != Some(0) {
                since = 0;
                batch = fetch(0, 0)?;
                continue;
            }
            since = 0;
        }
        let list = batch["changes"].as_array().ok_or("response has no changes")?;
        // Delete every affected key first, then insert, so that uniqueness is only checked
        // against the new state.
        let mut inserts: Vec<(&'static str, Pending)> = Vec::new();
        for ch in list {
            let table = ch["t"].as_str().unwrap_or_default();
            let Some((t, kc)) = REPLICATED.iter().find(|(t, _)| *t == table) else { continue };
            let k = from_json(&ch["k"])?;
            let seq = ch["seq"].as_i64().ok_or("change without seq")?;
            let rows = parse_rows(ch)?;
            tx.prepare_cached("INSERT OR REPLACE INTO repl_log(seq,tbl,k) VALUES(?1,?2,?3)").map_err(e)?.execute(rusqlite::params![seq, t, k]).map_err(e)?;
            if *t == "page" {
                if let rusqlite::types::Value::Integer(id) = k {
                    touched.insert(id);
                    if !seen_pages.insert(id) {
                        pages.retain(|(pk, _)| *pk != k);
                    }
                }
                // Page rows are applied last (see below).
                pages.push((k, rows));
            } else {
                tx.prepare_cached(&format!("DELETE FROM {t} WHERE {kc} IS ?1")).map_err(e)?.execute([&k]).map_err(e)?;
                inserts.push((t, rows));
            }
            applied += 1;
        }
        for (t, rows) in &inserts {
            insert_rows(&tx, t, rows)?;
        }
        let to = batch["to"].as_i64().ok_or("response has no 'to'")?;
        since = since.max(to);
        if batch["more"].as_bool() != Some(true) {
            {
                let mut s = w.repl.lock().unwrap_or_else(|e| e.into_inner());
                s.primary_head = head;
            }
            if applied > 0 || reset_done {
                // Titles can be swapped between pages (moves), so no page row may be inserted
                // while another page's old row still holds its title.
                for (k, _) in &pages {
                    tx.prepare_cached("DELETE FROM page WHERE id IS ?1").map_err(e)?.execute([k]).map_err(e)?;
                }
                for (_, rows) in &pages {
                    insert_rows(&tx, "page", rows)?;
                }
                for id in &touched {
                    w.reindex_search(&tx, *id).map_err(|x| x.info)?;
                }
                set_state(&tx, "local_state", "replica_seq", &since.to_string()).map_err(e)?;
                set_state(&tx, "local_state", "primary_db_id", &db_id).map_err(e)?;
            }
            break;
        }
        batch = fetch(since, 0)?;
    }
    if applied > 0 || reset_done {
        tx.commit().map_err(e)?;
        w.epoch.fetch_add(1, Ordering::SeqCst);
        w.changed.notify_waiters();
        if let Some(t) = get_state(&g, "meta", "repl_term").map_err(e)?.and_then(|v| v.parse::<i64>().ok()) {
            w.term.fetch_max(t, Ordering::SeqCst);
        }
    } else {
        drop(tx);
    }
    w.repl_sync_mode.store(sync_mode, Ordering::SeqCst);
    drop(g);
    let mut s = w.repl.lock().unwrap_or_else(|e| e.into_inner());
    s.applied_seq = since;
    s.last_sync = now();
    s.last_error.clear();
    s.rows_applied += applied as u64;
    Ok(applied)
}

// ---------------------------------------------------------------------------------------
// roles and fail-over
// ---------------------------------------------------------------------------------------

fn persist_role(w: &Wiki, c: &Connection) {
    let role = match w.upstream() {
        Some(u) => format!("replica:{u}"),
        None => "primary".to_string(),
    };
    let _ = set_state(c, "local_state", "role", &role);
}

/// Turn this replica into the primary: start recording changes and open a new term.
pub fn promote(w: &Wiki) -> Result<i64, String> {
    promote_to(w, None)
}

/// Become the primary under `term` (an election's term, or the next one).
fn promote_to(w: &Wiki, term: Option<i64>) -> Result<i64, String> {
    let e = |x: rusqlite::Error| x.to_string();
    let g = w.db.writer();
    crate::db::install_change_triggers(&g).map_err(e)?;
    crate::db::ensure_identity(&g).map_err(e)?;
    let current = get_state(&g, "meta", "repl_term").map_err(e)?.and_then(|v| v.parse::<i64>().ok()).unwrap_or(0).max(w.term.load(Ordering::SeqCst));
    let term = term.unwrap_or(current + 1).max(current + 1);
    let start = head(&g).map_err(|x| x.info)?;
    // The term history is replicated: every node learns where each term began.
    let mut hist = term_history(&g);
    hist.push((term, start));
    set_state(&g, "meta", "term_history", &serde_json::to_string(&hist).unwrap_or_default()).map_err(e)?;
    set_state(&g, "meta", "repl_term", &term.to_string()).map_err(e)?;
    w.term.store(term, Ordering::SeqCst);
    w.last_seq.fetch_max(start, Ordering::SeqCst);
    w.set_upstream(None);
    *w.cluster.primary_since.lock().unwrap_or_else(|x| x.into_inner()) = Some(std::time::Instant::now());
    w.acks.lock().unwrap_or_else(|x| x.into_inner()).clear();
    persist_role(w, &g);
    Ok(term)
}

/// Turn this primary into a replica of `upstream`. Its data is saved to the backup directory
/// first, because the local copy is then replaced by the new primary's.
pub fn demote(w: &Wiki, upstream: &str) -> Result<(), String> {
    let g = w.db.writer();
    let dir = std::path::Path::new(&w.cfg.backup_dir);
    let name = format!("pre-demotion-{}.db", crate::util::iso(now()).replace([':', '-'], ""));
    match std::fs::create_dir_all(dir).map_err(|e| e.to_string()).and_then(|_| g.execute("VACUUM INTO ?1", [dir.join(&name).to_string_lossy().as_ref()]).map_err(|e| e.to_string())) {
        Ok(_) => tracing::warn!(backup = %name, "saved this node's data before it is replaced by the new primary's"),
        Err(err) => tracing::error!(error = %err, "could not save a backup before demotion"),
    }
    crate::db::remove_change_triggers(&g).map_err(|e| e.to_string())?;
    *w.cluster.primary_since.lock().unwrap_or_else(|x| x.into_inner()) = None;
    w.set_upstream(Some(upstream.to_string()));
    w.repl_resync.store(true, Ordering::SeqCst);
    persist_role(w, &g);
    Ok(())
}

struct Peer {
    url: String,
    term: i64,
    node_id: String,
}

/// The best primary among the configured peers: highest term, ties broken by node id.
fn find_primary(agent: &ureq::Agent, w: &Wiki) -> Option<Peer> {
    let mut best: Option<Peer> = None;
    let (mut answered, mut me_listed) = (0usize, false);
    let replies: Vec<(String, Option<Value>)> = std::thread::scope(|sc| {
        let hs: Vec<_> = w
            .cfg
            .peers
            .iter()
            .map(|url| {
                sc.spawn(move || {
                    let resp = agent.get(&format!("{url}/rest/v1/replication/status")).set("Authorization", &format!("Bearer {}", w.cfg.replica_token)).call();
                    let v = resp.ok().and_then(|r| r.into_string().ok()).and_then(|b| serde_json::from_str::<Value>(&b).ok());
                    (url.clone(), v)
                })
            })
            .collect();
        hs.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    for (url, v) in replies {
        let Some(v) = v else { continue };
        let node_id = v["node_id"].as_str().unwrap_or_default().to_string();
        answered += 1;
        if node_id == w.node_id {
            me_listed = true;
            continue;
        }
        if !node_id.is_empty() {
            w.cluster.voters.lock().unwrap_or_else(|e| e.into_inner()).insert(url.clone(), node_id.clone());
        }
        if v["role"] != "primary" || node_id.is_empty() {
            continue;
        }
        let p = Peer { url: url.clone(), term: v["term"].as_i64().unwrap_or(0), node_id };
        if best.as_ref().is_none_or(|b| (p.term, &p.node_id) > (b.term, &b.node_id)) {
            best = Some(p);
        }
    }
    // Whether PEERS includes this node decides the cluster size.
    if me_listed {
        w.cluster.self_listed.store(1, Ordering::SeqCst);
    } else if answered == w.cfg.peers.len() && answered > 0 && w.cluster.self_listed.load(Ordering::SeqCst) == 0 {
        w.cluster.self_listed.store(2, Ordering::SeqCst);
        tracing::warn!("PEERS does not list this node itself; counting it in anyway (list every node, including this one)");
    }
    best
}

// ---------------------------------------------------------------------------------------
// quorum: elections and fencing
// ---------------------------------------------------------------------------------------

/// State of the automatic fail-over (only used with `PEERS` and `FAILOVER_AFTER_SECS`).
#[derive(Default)]
pub struct Cluster {
    /// When this node became the primary (start-up or promotion): a short grace period for the
    /// replicas to reconnect before the majority check applies.
    pub primary_since: std::sync::Mutex<Option<std::time::Instant>>,
    /// Peer URL -> node id, learned from their status pages.
    pub voters: std::sync::Mutex<std::collections::HashMap<String, String>>,
    /// 0 = not known yet, 1 = PEERS lists this node, 2 = it does not.
    pub self_listed: std::sync::atomic::AtomicU8,
    /// Serialises vote decisions.
    vote_lock: std::sync::Mutex<()>,
}

/// Automatic fail-over by majority vote is on.
pub fn quorum_mode(w: &Wiki) -> bool {
    !w.cfg.peers.is_empty() && w.cfg.failover_after_secs > 0
}

pub fn cluster_size(w: &Wiki) -> usize {
    let n = w.cfg.peers.len().max(1);
    if w.cluster.self_listed.load(Ordering::SeqCst) == 2 {
        n + 1
    } else {
        n
    }
}

pub fn majority(w: &Wiki) -> usize {
    cluster_size(w) / 2 + 1
}

/// A primary that has not heard from a majority for this long stops taking writes. It is half
/// the fail-over delay, so it has stopped before any replica may start an election.
fn lease(w: &Wiki) -> Duration {
    Duration::from_millis((w.cfg.failover_after_secs * 1000 / 2).max(1000))
}

/// Node ids of the other voting members.
fn voter_ids(w: &Wiki) -> HashSet<String> {
    w.cluster.voters.lock().unwrap_or_else(|e| e.into_inner()).values().cloned().collect()
}

/// Replicas that have applied at least `seq` (voting members only in quorum mode).
pub fn confirmations(w: &Wiki, seq: i64) -> usize {
    let voters = quorum_mode(w).then(|| voter_ids(w));
    w.acks
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|(id, (s, at))| *s >= seq && at.elapsed().as_secs() < 90 && voters.as_ref().is_none_or(|v| v.contains(*id)))
        .count()
}

/// How many replicas must confirm a write before it counts as done.
pub fn required_acks(w: &Wiki) -> usize {
    if quorum_mode(w) {
        w.cfg.repl_min_acks.max(majority(w) - 1)
    } else {
        w.cfg.repl_min_acks
    }
}

/// True while this primary is in contact with a majority (always true outside quorum mode).
pub fn lease_ok(w: &Wiki) -> bool {
    if !quorum_mode(w) || w.is_replica() {
        return true;
    }
    let lease = lease(w);
    if w.cluster.primary_since.lock().unwrap_or_else(|e| e.into_inner()).is_some_and(|t| t.elapsed() < lease) {
        return true;
    }
    contact_majority(w, lease)
}

fn contact_majority(w: &Wiki, within: Duration) -> bool {
    let voters = voter_ids(w);
    let fresh = w.acks.lock().unwrap_or_else(|e| e.into_inner()).iter().filter(|(id, (_, at))| voters.contains(*id) && at.elapsed() < within).count();
    fresh + 1 >= majority(w)
}

/// (term of the last change this node holds, its position): newer terms win, then positions.
fn position(c: &Connection) -> (i64, i64) {
    let seq = head(c).unwrap_or(0);
    (term_of(&term_history(c), seq), seq)
}

/// `[(term, start)]`: changes after `start` were made under `term`.
pub fn term_history(c: &Connection) -> Vec<(i64, i64)> {
    get_state(c, "meta", "term_history").ok().flatten().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

pub fn term_of(hist: &[(i64, i64)], seq: i64) -> i64 {
    hist.iter().filter(|(_, start)| *start < seq).map(|(t, _)| *t).max().unwrap_or(0)
}

fn voted(c: &Connection) -> (i64, String) {
    let t = get_state(c, "local_state", "voted_term").ok().flatten().and_then(|v| v.parse().ok()).unwrap_or(0);
    (t, get_state(c, "local_state", "voted_for").ok().flatten().unwrap_or_default())
}

/// Decide a vote request. Granted only if the candidate's term is new, this node has not
/// voted for someone else in it, it no longer hears from a primary itself, and the candidate
/// holds at least everything this node holds.
pub fn decide_vote(w: &Wiki, term: i64, candidate: &str, cand_pos: (i64, i64)) -> Result<(), String> {
    let _serial = w.cluster.vote_lock.lock().unwrap_or_else(|e| e.into_inner());
    let g = w.db.writer();
    if term <= w.term.load(Ordering::SeqCst) {
        return Err("that term is not newer than the current one".into());
    }
    let (vt, vf) = voted(&g);
    if term < vt || (term == vt && vf != candidate) {
        return Err(format!("already voted in term {vt}"));
    }
    let f = w.cfg.failover_after_secs as i64;
    if w.is_replica() {
        let last = w.repl.lock().map(|s| s.last_sync).unwrap_or(0);
        if last > 0 && now() - last < f {
            return Err("the primary is still reachable from here".into());
        }
    } else if contact_majority(w, lease(w)) {
        return Err("this node is the primary and still reaches a majority".into());
    }
    if cand_pos < position(&g) {
        return Err("the candidate is missing changes this node has".into());
    }
    set_state(&g, "local_state", "voted_term", &term.to_string()).map_err(|e| e.to_string())?;
    set_state(&g, "local_state", "voted_for", candidate).map_err(|e| e.to_string())?;
    Ok(())
}

/// Ask every peer for its vote; become the primary with a majority. Returns the new term.
fn campaign(w: &Wiki, agent: &ureq::Agent) -> Option<i64> {
    let (term, pos) = {
        let _serial = w.cluster.vote_lock.lock().unwrap_or_else(|e| e.into_inner());
        let g = w.db.writer();
        let term = voted(&g).0.max(w.term.load(Ordering::SeqCst)) + 1;
        set_state(&g, "local_state", "voted_term", &term.to_string()).ok()?;
        set_state(&g, "local_state", "voted_for", &w.node_id).ok()?;
        (term, position(&g))
    };
    let body = json!({ "term": term, "candidate": w.node_id, "last_term": pos.0, "last_seq": pos.1 }).to_string();
    let replies: Vec<Option<Value>> = std::thread::scope(|sc| {
        let hs: Vec<_> = w
            .cfg
            .peers
            .iter()
            .map(|url| {
                let body = body.clone();
                sc.spawn(move || {
                    agent
                        .post(&format!("{url}/rest/v1/replication/vote"))
                        .set("Authorization", &format!("Bearer {}", w.cfg.replica_token))
                        .set("Content-Type", "application/json")
                        .send_string(&body)
                        .ok()
                        .and_then(|r| r.into_string().ok())
                        .and_then(|b| serde_json::from_str::<Value>(&b).ok())
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().ok().flatten()).collect()
    });
    let mut votes = 1; // our own
    let mut reasons = Vec::new();
    for v in replies.into_iter().flatten() {
        if v["node_id"].as_str() == Some(w.node_id.as_str()) {
            w.cluster.self_listed.store(1, Ordering::SeqCst);
            continue;
        }
        if v["granted"] == true {
            votes += 1;
        } else if let Some(r) = v["reason"].as_str() {
            reasons.push(r.to_string());
        }
    }
    let need = majority(w);
    if votes >= need {
        match promote_to(w, Some(term)) {
            Ok(t) => {
                tracing::warn!(term = t, votes, need, "won the election: this node is now the primary");
                return Some(t);
            }
            Err(e) => tracing::error!(error = %e, "promotion after the election failed"),
        }
    } else {
        tracing::info!(term, votes, need, ?reasons, "election not won; staying a replica");
    }
    None
}

/// Background thread: a replica follows its primary; with `PEERS` / `FAILOVER_AFTER_SECS`
/// nodes also notice a lost or superseded primary and change role.
///
/// * replica, primary unreachable: follow another primary if a peer is one; otherwise, after
///   `FAILOVER_AFTER_SECS`, promote itself (new term).
/// * primary: if a peer is primary with a higher term (or the same term and a higher node
///   id), step down and copy from it. This is what ends a split brain.
pub fn spawn_ha(w: Wiki) {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(Duration::from_secs(60))
        .timeout_write(Duration::from_secs(20))
        .redirects(0)
        .user_agent(concat!("hoepedia-api/", env!("CARGO_PKG_VERSION"), " (replica)"))
        .build();
    let probe = ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(2)).timeout(Duration::from_secs(4)).redirects(0).build();
    if w.cfg.replica_token.is_empty() {
        tracing::warn!("REPLICA_TOKEN is empty; other nodes will refuse replication and status requests");
    }
    {
        let g = w.db.writer();
        let seq = get_state(&g, "local_state", "replica_seq").ok().flatten().and_then(|v| v.parse().ok()).unwrap_or(0);
        w.repl.lock().unwrap_or_else(|e| e.into_inner()).applied_seq = seq;
        persist_role(&w, &g);
    }
    if !w.is_replica() {
        *w.cluster.primary_since.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::time::Instant::now());
    }
    let _ = std::thread::Builder::new().name("replication".into()).spawn(move || {
        let mut failures = 0u32;
        let mut first = true;
        let mut down_since: Option<std::time::Instant> = None;
        loop {
            if !w.is_replica() {
                // primary: watch for a peer that supersedes this node
                std::thread::sleep(Duration::from_secs(1));
                let (term, me) = (w.term.load(Ordering::SeqCst), w.node_id.clone());
                if let Some(p) = find_primary(&probe, &w).filter(|p| (p.term, &p.node_id) > (term, &me)) {
                    tracing::warn!(peer = %p.url, their_term = p.term, our_term = term, "another node is the primary now; stepping down");
                    if let Err(err) = demote(&w, &p.url) {
                        tracing::error!(error = %err, "demotion failed");
                    }
                    first = true;
                }
                continue;
            }
            // replica: long-poll, the upstream answers as soon as something changes
            let wait = (w.cfg.request_timeout.as_secs().saturating_sub(5)).clamp(0, 20);
            // The writer is held while a batch is applied: render offline (see remote.rs).
            match crate::remote::offline(|| sync_once(&w, &mut |since, wait| fetch(&agent, &w, since, wait), if first { 0 } else { wait })) {
                Ok(n) => {
                    if first || n > 0 {
                        tracing::info!(applied = n, seq = w.repl.lock().map(|s| s.applied_seq).unwrap_or(0), "replica synchronised");
                    }
                    first = false;
                    failures = 0;
                    down_since = None;
                    // Coalesce bursts of edits into a few requests (stays far below the
                    // primary's per-IP rate limit) unless the primary waits for confirmations.
                    let pause = if wait == 0 {
                        1000
                    } else if n > 0 && !w.repl_sync_mode.load(Ordering::SeqCst) {
                        300
                    } else {
                        0
                    };
                    std::thread::sleep(Duration::from_millis(pause));
                }
                Err(err) => {
                    failures += 1;
                    let lost = err.starts_with("cannot reach") || err.starts_with("upstream is stale");
                    if failures == 1 || failures.is_multiple_of(10) {
                        tracing::warn!(error = %err, "replication failed; retrying");
                    }
                    w.repl.lock().unwrap_or_else(|e| e.into_inner()).last_error = err;
                    if lost {
                        let since = *down_since.get_or_insert_with(std::time::Instant::now);
                        let term = w.term.load(Ordering::SeqCst);
                        if let Some(p) = find_primary(&probe, &w).filter(|p| p.term >= term && Some(&p.url) != w.upstream().as_ref()) {
                            tracing::warn!(primary = %p.url, "following the new primary");
                            w.set_upstream(Some(p.url));
                            persist_role(&w, &w.db.writer());
                            down_since = None;
                            first = true;
                            continue;
                        }
                        let synced = w.repl.lock().map(|s| s.last_sync > 0 || s.applied_seq > 0).unwrap_or(false);
                        if quorum_mode(&w) && synced && since.elapsed().as_secs() >= w.cfg.failover_after_secs {
                            // Only a majority can elect a new primary, so a node that is merely
                            // cut off from a healthy primary cannot take over.
                            if campaign(&w, &probe).is_some() {
                                down_since = None;
                                continue;
                            }
                            // spread retries so two candidates do not keep splitting the vote
                            use rand::Rng;
                            std::thread::sleep(Duration::from_millis(rand::thread_rng().gen_range(300..2500)));
                            continue;
                        }
                    } else {
                        down_since = None;
                    }
                    std::thread::sleep(Duration::from_millis(if lost { 1000 } else { (failures as u64).min(15) * 1000 }));
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------------------
// HTTP handlers
// ---------------------------------------------------------------------------------------

use axum::{
    extract::{Query, State},
    Extension, Json,
};

type Q = Query<std::collections::HashMap<String, String>>;

/// `GET /replication/changes?since=N&wait=S` (sysop). Long-polls up to `wait` seconds.
pub async fn changes_handler(State(st): State<crate::state::AppState>, Extension(a): Extension<crate::users::Actor>, Query(q): Q) -> Res<Json<Value>> {
    a.require("siteadmin")?;
    let since: i64 = q.get("since").and_then(|v| v.parse().ok()).unwrap_or(0).max(0);
    let max_wait = st.cfg.request_timeout.as_secs().saturating_sub(5).min(20);
    let mut wait = q.get("wait").and_then(|v| v.parse::<u64>().ok()).unwrap_or(0).min(max_wait);
    if quorum_mode(&st.wiki) {
        // replicas must check in well within the primary's lease
        wait = wait.min((st.cfg.failover_after_secs / 4).max(1));
    }
    let max_bytes = q.get("max_bytes").and_then(|v| v.parse::<usize>().ok()).unwrap_or(8 << 20).clamp(64 << 10, 64 << 20);
    // Asking for changes after N is also the replica's confirmation that it has applied N.
    if let Some(node) = q.get("node").filter(|n| !n.is_empty() && n.len() <= 64) {
        let mut acks = st.wiki.acks.lock().unwrap_or_else(|e| e.into_inner());
        if acks.len() < 256 || acks.contains_key(node) {
            acks.insert(node.clone(), (since, std::time::Instant::now()));
        }
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(wait);
    loop {
        // Register for the wake-up before looking, so a commit in between is not missed.
        let woken = st.wiki.changed.notified();
        let h = st.wiki.read(|_, c| head(c)).await?;
        let now = std::time::Instant::now();
        if h != since || now >= deadline {
            break;
        }
        // The timeout also covers changes made by another process or by replication itself.
        let _ = tokio::time::timeout((deadline - now).min(Duration::from_millis(500)), woken).await;
    }
    let mut v = st.wiki.read(move |_, c| changes(c, since, max_bytes)).await?;
    v["term"] = json!(st.wiki.term.load(Ordering::SeqCst));
    v["node_id"] = json!(st.wiki.node_id);
    v["sync"] = json!(required_acks(&st.wiki) > 0);
    Ok(Json(v))
}

/// `POST /replication/vote` (sysop token): another node asks for this node's vote.
pub async fn vote_handler(State(st): State<crate::state::AppState>, Extension(a): Extension<crate::users::Actor>, Json(v): Json<Value>) -> Res<Json<Value>> {
    a.require("siteadmin")?;
    let term = v["term"].as_i64().ok_or_else(|| ApiError::bad("badvalue", "term is required"))?;
    let candidate = v["candidate"].as_str().unwrap_or_default().to_string();
    let pos = (v["last_term"].as_i64().unwrap_or(0), v["last_seq"].as_i64().unwrap_or(0));
    let w = st.wiki.clone();
    let node = w.node_id.clone();
    if candidate == node {
        return Ok(Json(json!({ "granted": false, "node_id": node, "reason": "that is this node" })));
    }
    let r = tokio::task::spawn_blocking(move || decide_vote(&w, term, &candidate, pos)).await.map_err(ApiError::internal)?;
    Ok(Json(match r {
        Ok(()) => json!({ "granted": true, "node_id": node, "term": term }),
        Err(reason) => json!({ "granted": false, "node_id": node, "reason": reason }),
    }))
}

/// `POST /replication/promote` (sysop): make this replica the primary now (manual fail-over).
pub async fn promote_handler(State(st): State<crate::state::AppState>, Extension(a): Extension<crate::users::Actor>) -> Res<Json<Value>> {
    a.require("siteadmin")?;
    if !st.wiki.is_replica() {
        return Err(ApiError::bad("notareplica", "this instance is already a primary"));
    }
    let w = st.wiki.clone();
    let term = tokio::task::spawn_blocking(move || promote(&w)).await.map_err(ApiError::internal)?.map_err(ApiError::internal)?;
    Ok(Json(json!({ "role": "primary", "term": term })))
}

pub async fn status_handler(State(st): State<crate::state::AppState>, Extension(a): Extension<crate::users::Actor>) -> Res<Json<Value>> {
    a.require("siteadmin")?;
    st.wiki.read(status_json).await.map(Json)
}

/// `POST /replication/resync` on a replica: throw the local copy away and copy everything again.
pub async fn resync_handler(State(st): State<crate::state::AppState>, Extension(a): Extension<crate::users::Actor>) -> Res<Json<Value>> {
    a.require("siteadmin")?;
    if !st.wiki.is_replica() {
        return Err(ApiError::bad("notareplica", "this instance is not a replica"));
    }
    st.wiki.repl_resync.store(true, Ordering::SeqCst);
    Ok(Json(json!({ "resync": "scheduled" })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Config,
        db::Db,
        title::Title,
        wiki::{find_page, rev_text, testutil::*},
    };
    use std::sync::Arc;

    fn replica() -> (Wiki, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("hoepedia-replica-{}-{}", std::process::id(), crate::util::new_token()));
        let mut cfg = Config::for_tests(dir.join("r.db").to_str().unwrap());
        cfg.replica_of = Some("http://primary.invalid".into());
        let db = Db::open_as(&cfg.db_path, 2, true).unwrap();
        (Wiki::new(Arc::new(cfg), db), dir)
    }

    /// Pull from the primary's database directly (no HTTP), with tiny batches to exercise `more`.
    fn sync(primary: &Wiki, replica: &Wiki) -> usize {
        let p = primary.clone();
        sync_once(replica, &mut |since, _| changes(&p.db.reader(), since, 200).map_err(|e| e.info), 0).unwrap()
    }

    fn page_text(w: &Wiki, title: &str) -> Option<String> {
        let c = w.db.reader();
        let p = find_page(&c, &Title::parse(title).unwrap().title).unwrap()?;
        Some(rev_text(&c, p.latest).unwrap())
    }

    fn dump(w: &Wiki) -> String {
        let c = w.db.reader();
        let mut out = Vec::new();
        for (t, _) in REPLICATED {
            let mut st = c.prepare(&format!("SELECT * FROM {t}")).unwrap();
            let n = st.column_count();
            let mut rows = st.query([]).unwrap();
            while let Some(r) = rows.next().unwrap() {
                let mut line = t.to_string();
                for i in 0..n {
                    line.push_str(&format!("|{:?}", r.get_ref(i).unwrap()));
                }
                out.push(line);
            }
        }
        out.sort();
        out.join("\n")
    }

    #[test]
    fn replica_follows_primary() {
        let t = make(false);
        let (r, dir) = replica();
        {
            let c = t.wiki.db.writer();
            t.wiki.edit(&c, &t.admin, req("Template:Hi", "Hello {{{1}}}")).unwrap();
            t.wiki.edit(&c, &t.admin, req("Alpha", "{{Hi|world}} [[Beta]] [[Category:Greek]]")).unwrap();
            t.wiki.edit(&c, &t.admin, req("Beta", "second page about zebras")).unwrap();
            t.wiki.upload(&c, &t.admin, "Dot.gif", b"GIF89a\x01\x00\x01\x00\x00\x00\x00;", "", None, false).unwrap();
        }
        assert!(sync(&t.wiki, &r) > 0);
        assert_eq!(dump(&t.wiki), dump(&r), "replica tables equal the primary's");
        assert_eq!(page_text(&r, "Alpha").unwrap(), "{{Hi|world}} [[Beta]] [[Category:Greek]]");
        // the token of the primary's admin works on the replica, and search was rebuilt locally
        {
            let c = r.db.reader();
            let (total, hits) = crate::queries::search(&c, "zebras", &[0], 10, 0, false).unwrap();
            assert_eq!((total, hits[0].page.title.name.as_str()), (1, "Beta"));
            assert_eq!(crate::files::file_data(&c, "Dot.gif", None).unwrap().data.len(), 14);
        }
        assert_eq!(sync(&t.wiki, &r), 0, "nothing new");

        // edits, a move that swaps titles around, and a delete
        {
            let c = t.wiki.db.writer();
            t.wiki.edit(&c, &t.admin, req("Beta", "second page about yaks")).unwrap();
            t.wiki.move_page(&c, &t.admin, &Title::new(0, "Alpha"), &Title::new(0, "Gamma"), "", false).unwrap();
            t.wiki.delete_page(&c, &t.admin, &Title::new(0, "Alpha"), "").unwrap();
            t.wiki.edit(&c, &t.admin, req("Alpha", "a brand new Alpha")).unwrap();
        }
        assert!(sync(&t.wiki, &r) > 0);
        assert_eq!(dump(&t.wiki), dump(&r));
        assert_eq!(page_text(&r, "Alpha").unwrap(), "a brand new Alpha");
        assert_eq!(page_text(&r, "Gamma").unwrap(), "{{Hi|world}} [[Beta]] [[Category:Greek]]");
        {
            let c = r.db.reader();
            assert_eq!(crate::queries::search(&c, "zebras", &[0], 10, 0, false).unwrap().0, 0);
            assert_eq!(crate::queries::search(&c, "yaks", &[0], 10, 0, false).unwrap().0, 1);
        }
        // a forced resync rebuilds an identical copy
        r.repl_resync.store(true, Ordering::SeqCst);
        assert!(sync(&t.wiki, &r) > 0);
        assert_eq!(dump(&t.wiki), dump(&r));
        // a replica rejects writes
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn promotion_and_demotion() {
        let a = make(false);
        let (r, dir) = replica();
        a.wiki.edit(&a.wiki.db.writer(), &a.admin, req("Shared", "from the first primary")).unwrap();
        sync(&a.wiki, &r);
        assert!(r.is_replica() && r.term.load(Ordering::SeqCst) == 0);
        // fail-over: the replica becomes the primary and accepts writes under a new term
        assert_eq!(promote(&r).unwrap(), 1);
        assert!(!r.is_replica());
        let admin_on_r = crate::users::Actor::from_user(&crate::users::user_by_name(&r.db.reader(), "Admin").unwrap().unwrap());
        r.edit(&r.db.writer(), &admin_on_r, req("After failover", "written on the new primary")).unwrap();
        // the old primary took a write nobody else saw, then learns it was superseded
        a.wiki.edit(&a.wiki.db.writer(), &a.admin, req("Lost", "only on the old primary")).unwrap();
        // a node never copies from a primary with an older term
        let (r2, dir2) = replica();
        sync(&r, &r2);
        assert_eq!(r2.term.load(Ordering::SeqCst), 1);
        let stale = a.wiki.clone();
        let err = sync_once(&r2, &mut |since, _| changes(&stale.db.reader(), since, 1 << 20).map_err(|e| e.info).map(|mut v| { v["term"] = json!(0); v }), 0).unwrap_err();
        assert!(err.contains("stale"), "{err}");
        // demotion: saved to a backup, then replaced by the new primary's data
        demote(&a.wiki, "http://new-primary.invalid").unwrap();
        assert!(a.wiki.is_replica());
        let newp = r.clone();
        sync_once(&a.wiki, &mut |since, _| changes(&newp.db.reader(), since, 1 << 20).map_err(|e| e.info).map(|mut v| { v["term"] = json!(1); v }), 0).unwrap();
        assert_eq!(dump(&a.wiki), dump(&r));
        assert!(page_text(&a.wiki, "After failover").is_some() && page_text(&a.wiki, "Lost").is_none());
        let backups: Vec<_> = std::fs::read_dir(&a.wiki.cfg.backup_dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert!(backups.iter().any(|n| n.starts_with("pre-demotion-")), "{backups:?}");
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(dir2);
    }

    #[test]
    fn term_history_and_votes() {
        let hist = vec![(1, 100), (3, 250)];
        assert_eq!((term_of(&hist, 50), term_of(&hist, 100), term_of(&hist, 101), term_of(&hist, 250), term_of(&hist, 251)), (0, 0, 1, 1, 3));
        assert_eq!(term_of(&[], 9), 0);
        // a node in quorum mode, as a replica that has not heard from a primary yet
        let t = make(false);
        let mut cfg = (*t.wiki.cfg).clone();
        cfg.peers = vec!["http://a".into(), "http://b".into(), "http://c".into()];
        cfg.failover_after_secs = 4;
        let w = Wiki::new(std::sync::Arc::new(cfg), crate::db::Db::open(&t.wiki.cfg.db_path, 2).unwrap());
        w.set_upstream(Some("http://a".into()));
        assert_eq!((cluster_size(&w), majority(&w)), (3, 2));
        assert!(required_acks(&w) >= 1);
        w.edit(&w.db.writer(), &t.admin, req("Data", "x")).unwrap_or_else(|_| panic!());
        let here = position(&w.db.reader());
        // behind: refused; up to date: granted once per term; same term, other candidate: refused
        assert!(decide_vote(&w, 5, "cand-1", (0, here.1 - 1)).unwrap_err().contains("missing changes"));
        decide_vote(&w, 5, "cand-1", here).unwrap();
        decide_vote(&w, 5, "cand-1", here).unwrap();
        assert!(decide_vote(&w, 5, "cand-2", here).unwrap_err().contains("already voted"));
        assert!(decide_vote(&w, 4, "cand-2", here).unwrap_err().contains("already voted"));
        // a newer term beats more changes from an older one
        decide_vote(&w, 6, "cand-2", (1, 0)).unwrap();
        // a replica that still hears from its primary refuses
        w.repl.lock().unwrap().last_sync = now();
        assert!(decide_vote(&w, 7, "cand-3", (9, 9)).unwrap_err().contains("still reachable"));
        // a term not newer than the node's own is refused
        w.term.store(10, Ordering::SeqCst);
        w.repl.lock().unwrap().last_sync = 0;
        assert!(decide_vote(&w, 9, "cand-3", (9, 9)).unwrap_err().contains("not newer"));
    }

    #[test]
    fn different_primary_triggers_full_resync() {
        let a = make(false);
        let b = make(false);
        let (r, dir) = replica();
        a.wiki.edit(&a.wiki.db.writer(), &a.admin, req("OnlyA", "from a")).unwrap();
        b.wiki.edit(&b.wiki.db.writer(), &b.admin, req("OnlyB", "from b")).unwrap();
        sync(&a.wiki, &r);
        assert!(page_text(&r, "OnlyA").is_some());
        sync(&b.wiki, &r);
        assert!(page_text(&r, "OnlyA").is_none() && page_text(&r, "OnlyB").is_some());
        assert_eq!(dump(&b.wiki), dump(&r));
        let _ = std::fs::remove_dir_all(dir);
    }
}
