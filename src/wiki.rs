//! The wiki core: pages, revisions, editing, moving, deleting, protection, rendering and
//! link tracking. Everything here is synchronous and runs inside `Wiki::read` / `Wiki::write`
//! (a blocking thread holding a SQLite connection / transaction).

use std::{
    ops::Deref,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};

use moka::sync::Cache;
use rusqlite::{params, Connection, OptionalExtension, Row, TransactionBehavior};
use serde_json::{json, Value};

use crate::{
    config::Config,
    db::Db,
    error::{ApiError, Res},
    title::{Title, NS_TEMPLATE},
    users::Actor,
    util::{now, sha256_hex, truncate_chars},
    wikitext::{self, Opts, Rendered, Resolver},
};

pub struct Inner {
    pub cfg: Arc<Config>,
    pub db: Db,
    pub cache: Cache<(i64, u64), Arc<Rendered>>,
    /// Bumped whenever something happens that can change how *other* pages render
    /// (page created/deleted/moved, template edited). Part of the render-cache key.
    pub epoch: AtomicU64,
    dirty: AtomicBool,
    /// Replica state (see `repl.rs`).
    pub repl: std::sync::Mutex<crate::repl::Status>,
    /// Scaled images, keyed by (file version, width).
    pub thumbs: Cache<(i64, u32, u32), crate::files::Thumb>,
    /// Client and cache for interwiki transclusion / federated search.
    pub remote: Arc<crate::remote::Remote>,
    pub repl_resync: AtomicBool,
    /// Woken after every commit, so waiting replicas are answered immediately.
    pub changed: tokio::sync::Notify,
    /// The upstream waits for confirmations, so the follower must not pause between polls.
    pub repl_sync_mode: AtomicBool,
    /// `Some(url)` while this node is a replica of `url`; `None` = primary. Can change at
    /// run time (fail-over).
    upstream: std::sync::RwLock<Option<String>>,
    pub node_id: String,
    /// Election term: bumped by every promotion, so nodes can tell a stale primary.
    pub term: std::sync::atomic::AtomicI64,
    /// Highest replication sequence committed by this process (primary).
    pub last_seq: std::sync::atomic::AtomicI64,
    /// Replica node id -> (sequence it has applied, when it said so).
    pub acks: std::sync::Mutex<std::collections::HashMap<String, (i64, std::time::Instant)>>,
    /// Automatic fail-over state (see `repl.rs`).
    pub cluster: crate::repl::Cluster,
}

#[derive(Clone)]
pub struct Wiki(Arc<Inner>);

impl Deref for Wiki {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

impl Wiki {
    pub fn new(cfg: Arc<Config>, db: Db) -> Wiki {
        let cache = Cache::builder().max_capacity(cfg.render_cache_entries).time_to_idle(std::time::Duration::from_secs(3600)).build();
        let thumbs = Cache::builder()
            .max_capacity(cfg.thumb_cache_bytes)
            .weigher(|_k, v: &crate::files::Thumb| (v.1.len() as u32).saturating_add(64))
            .time_to_idle(std::time::Duration::from_secs(6 * 3600))
            .build();
        let remote = Arc::new(crate::remote::Remote::new(cfg.interwiki_cache_secs));
        let (node_id, term, last_seq) = {
            let g = db.writer();
            (
                crate::db::get_state(&g, "local_state", "node_id").ok().flatten().unwrap_or_default(),
                crate::db::get_state(&g, "meta", "repl_term").ok().flatten().and_then(|v| v.parse().ok()).unwrap_or(0i64),
                g.query_row("SELECT coalesce(max(seq),0) FROM repl_log", [], |r| r.get(0)).unwrap_or(0i64),
            )
        };
        let upstream = std::sync::RwLock::new(cfg.replica_of.clone());
        Wiki(Arc::new(Inner {
            cfg,
            db,
            cache,
            epoch: AtomicU64::new(0),
            dirty: AtomicBool::new(false),
            repl: Default::default(),
            repl_resync: AtomicBool::new(false),
            changed: tokio::sync::Notify::new(),
            repl_sync_mode: AtomicBool::new(false),
            upstream,
            node_id,
            term: std::sync::atomic::AtomicI64::new(term),
            last_seq: std::sync::atomic::AtomicI64::new(last_seq),
            acks: Default::default(),
            cluster: Default::default(),
            thumbs,
            remote,
        }))
    }

    /// The node this one replicates from, if it is a replica.
    pub fn upstream(&self) -> Option<String> {
        self.upstream.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn is_replica(&self) -> bool {
        self.upstream.read().unwrap_or_else(|e| e.into_inner()).is_some()
    }

    pub(crate) fn set_upstream(&self, to: Option<String>) {
        *self.upstream.write().unwrap_or_else(|e| e.into_inner()) = to;
    }

    /// With `REPL_MIN_ACKS`: wait until enough replicas have applied everything committed so
    /// far. The write is already durable locally; on timeout it stands and `false` is returned.
    pub async fn wait_for_acks(&self) -> bool {
        let need = crate::repl::required_acks(self);
        if need == 0 || self.is_replica() {
            return true;
        }
        let seq = self.last_seq.load(Ordering::SeqCst);
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(self.cfg.repl_ack_timeout_ms);
        loop {
            let have = crate::repl::confirmations(self, seq);
            if have >= need {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                tracing::warn!(seq, have, need, "write committed but not confirmed by enough replicas in time");
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(4)).await;
        }
    }

    /// Run `f` on a blocking thread with a read-only connection.
    pub async fn read<T, F>(&self, f: F) -> Res<T>
    where
        F: FnOnce(&Wiki, &Connection) -> Res<T> + Send + 'static,
        T: Send + 'static,
    {
        let w = self.clone();
        tokio::task::spawn_blocking(move || {
            let g = w.db.reader();
            f(&w, &g)
        })
        .await
        .map_err(ApiError::internal)?
    }

    /// Run `f` inside an IMMEDIATE transaction on the single writer connection.
    pub async fn write<T, F>(&self, f: F) -> Res<T>
    where
        F: FnOnce(&Wiki, &Connection) -> Res<T> + Send + 'static,
        T: Send + 'static,
    {
        let up = self.upstream();
        if self.cfg.read_only || up.is_some() {
            let info = match up {
                Some(p) => format!("this is a read-only replica; send writes to the primary ({p})"),
                None => "this wiki is in read-only mode".to_string(),
            };
            return Err(ApiError::new(axum::http::StatusCode::SERVICE_UNAVAILABLE, "readonly", info));
        }
        let quorum = crate::repl::quorum_mode(self);
        if quorum && !crate::repl::lease_ok(self) {
            return Err(ApiError::new(
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "noquorum",
                "this node cannot reach a majority of the cluster, so it is not accepting changes; another node may be taking over",
            ));
        }
        let v = self.write_always(f).await?;
        if !self.wait_for_acks().await && quorum {
            return Err(ApiError::new(
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "notconfirmed",
                "the change was saved here but a majority of the cluster has not confirmed it yet; it stays if this node remains the primary and may be lost otherwise, so check before retrying",
            ));
        }
        Ok(v)
    }

    /// Like `write` but ignores `READ_ONLY` (startup bootstrap only).
    pub async fn write_always<T, F>(&self, f: F) -> Res<T>
    where
        F: FnOnce(&Wiki, &Connection) -> Res<T> + Send + 'static,
        T: Send + 'static,
    {
        let w = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut g = w.db.writer();
            let tx = g.transaction_with_behavior(TransactionBehavior::Immediate)?;
            // No waiting for other wikis while every other write waits for us.
            match crate::remote::offline(|| f(&w, &tx)) {
                Ok(v) => {
                    tx.commit()?;
                    if let Ok(seq) = g.query_row("SELECT coalesce(max(seq),0) FROM repl_log", [], |r| r.get::<_, i64>(0)) {
                        w.last_seq.fetch_max(seq, Ordering::SeqCst);
                    }
                    w.changed.notify_waiters();
                    if w.dirty.swap(false, Ordering::SeqCst) {
                        w.epoch.fetch_add(1, Ordering::SeqCst);
                    }
                    Ok(v)
                }
                Err(e) => {
                    w.dirty.store(false, Ordering::SeqCst);
                    Err(e)
                }
            }
        })
        .await
        .map_err(ApiError::internal)?
    }

    pub(crate) fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------------------
// rows
// ---------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct PageRow {
    pub id: i64,
    pub title: Title,
    pub latest: i64,
    pub is_redirect: bool,
    pub redirect: Option<Title>,
    pub protect_edit: String,
    pub protect_move: String,
    pub len: i64,
    pub words: i64,
    pub touched: i64,
    #[allow(dead_code)]
    pub created: i64,
}

pub const PCOLS: &str =
    "p.id,p.ns,p.title,p.latest,p.is_redirect,p.redirect_ns,p.redirect_title,p.protect_edit,p.protect_move,p.len,p.words,p.touched,p.created";

pub fn page_row(r: &Row) -> rusqlite::Result<PageRow> {
    let rns: Option<i64> = r.get(5)?;
    let rt: Option<String> = r.get(6)?;
    Ok(PageRow {
        id: r.get(0)?,
        title: Title { ns: r.get(1)?, name: r.get(2)? },
        latest: r.get(3)?,
        is_redirect: r.get::<_, i64>(4)? != 0,
        redirect: match (rns, rt) {
            (Some(n), Some(t)) => Some(Title { ns: n, name: t }),
            _ => None,
        },
        protect_edit: r.get(7)?,
        protect_move: r.get(8)?,
        len: r.get(9)?,
        words: r.get(10)?,
        touched: r.get(11)?,
        created: r.get(12)?,
    })
}

#[derive(Clone, Debug)]
pub struct RevRow {
    pub id: i64,
    pub page: i64,
    pub parent: i64,
    pub user_id: Option<i64>,
    pub user_name: String,
    pub ts: i64,
    pub comment: String,
    pub minor: bool,
    pub size: i64,
    pub sha256: String,
    pub model: String,
}

pub const RCOLS: &str = "r.id,r.page,r.parent,r.user_id,r.user_name,r.ts,r.comment,r.minor,r.size,r.sha256,r.model";

pub fn rev_row(r: &Row) -> rusqlite::Result<RevRow> {
    Ok(RevRow {
        id: r.get(0)?,
        page: r.get(1)?,
        parent: r.get(2)?,
        user_id: r.get(3)?,
        user_name: r.get(4)?,
        ts: r.get(5)?,
        comment: r.get(6)?,
        minor: r.get::<_, i64>(7)? != 0,
        size: r.get(8)?,
        sha256: r.get(9)?,
        model: r.get(10)?,
    })
}

pub fn find_page(c: &Connection, t: &Title) -> Res<Option<PageRow>> {
    Ok(c.prepare_cached(&format!("SELECT {PCOLS} FROM page p WHERE p.ns=?1 AND p.title=?2 AND p.deleted=0"))?
        .query_row(params![t.ns, t.name], page_row)
        .optional()?)
}

pub fn find_page_by_id(c: &Connection, id: i64) -> Res<Option<PageRow>> {
    Ok(c.prepare_cached(&format!("SELECT {PCOLS} FROM page p WHERE p.id=?1 AND p.deleted=0"))?.query_row(params![id], page_row).optional()?)
}

pub fn get_rev(c: &Connection, id: i64) -> Res<Option<RevRow>> {
    Ok(c.prepare_cached(&format!("SELECT {RCOLS} FROM revision r WHERE r.id=?1"))?.query_row(params![id], rev_row).optional()?)
}

pub fn rev_text(c: &Connection, id: i64) -> Res<String> {
    c.prepare_cached("SELECT text FROM revision WHERE id=?1")?
        .query_row(params![id], |r| r.get(0))
        .optional()?
        .ok_or_else(|| ApiError::not_found("nosuchrevid", format!("there is no revision with id {id}")))
}

/// A revision together with the page it belongs to (must be live).
pub fn rev_with_page(c: &Connection, rev_id: i64) -> Res<(RevRow, PageRow)> {
    let rev = get_rev(c, rev_id)?.ok_or_else(|| ApiError::not_found("nosuchrevid", format!("there is no revision with id {rev_id}")))?;
    let page = find_page_by_id(c, rev.page)?.ok_or_else(|| ApiError::not_found("nosuchrevid", "the page of this revision was deleted"))?;
    Ok((rev, page))
}

/// Follow redirects (max 5 hops). Returns the final page and the title we started from, if any hop happened.
pub fn follow_redirects(c: &Connection, mut page: PageRow) -> Res<(PageRow, Option<Title>)> {
    let origin = page.title.clone();
    let mut hopped = false;
    for _ in 0..5 {
        let Some(target) = page.redirect.clone().filter(|_| page.is_redirect) else { break };
        match find_page(c, &target)? {
            Some(next) => {
                page = next;
                hopped = true;
            }
            None => break,
        }
    }
    Ok((page, hopped.then_some(origin)))
}

// ---------------------------------------------------------------------------------------
// rendering
// ---------------------------------------------------------------------------------------

struct DbResolver<'a> {
    c: &'a Connection,
    w: &'a Wiki,
    /// Time spent waiting for other wikis during this render.
    remote_spent: std::time::Duration,
}

/// A render stops asking other wikis once it has waited this long in total.
const REMOTE_BUDGET: std::time::Duration = std::time::Duration::from_secs(3);

impl Resolver for DbResolver<'_> {
    fn template_source(&mut self, t: &Title) -> Option<String> {
        self.c
            .prepare_cached("SELECT r.text FROM page p JOIN revision r ON r.id=p.latest WHERE p.ns=?1 AND p.title=?2 AND p.deleted=0")
            .ok()?
            .query_row(params![t.ns, t.name], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }
    fn interwiki(&mut self, prefix: &str) -> Option<String> {
        self.c.prepare_cached("SELECT url FROM interwiki WHERE prefix=?1").ok()?.query_row(params![prefix], |r| r.get(0)).optional().ok().flatten()
    }
    fn remote_template(&mut self, prefix: &str, title: &str) -> Option<String> {
        let api: String = self
            .c
            .prepare_cached("SELECT api FROM interwiki WHERE prefix=?1 AND trans=1 AND api<>''")
            .ok()?
            .query_row(params![prefix], |r| r.get(0))
            .optional()
            .ok()
            .flatten()?;
        // A render waits for other wikis only for text it has never seen, and only up to the
        // budget; everything else is served from the cache and refreshed in the background.
        let may_wait = self.remote_spent < REMOTE_BUDGET;
        let t0 = std::time::Instant::now();
        let r = self.w.remote.wikitext(&api, title, may_wait).map(|t| t.as_str().to_string());
        self.remote_spent += t0.elapsed();
        r
    }
    fn interwiki_transcludes(&mut self, prefix: &str) -> bool {
        self.c
            .prepare_cached("SELECT 1 FROM interwiki WHERE prefix=?1 AND trans=1 AND api<>''")
            .ok()
            .and_then(|mut s| s.exists(params![prefix]).ok())
            .unwrap_or(false)
    }
    fn file_mime(&mut self, name: &str) -> Option<String> {
        self.c
            .prepare_cached("SELECT v.mime FROM file f JOIN file_version v ON v.id=f.latest WHERE f.name=?1 AND f.deleted=0")
            .ok()?
            .query_row(params![name], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }
    fn interwiki_is_language(&mut self, prefix: &str) -> bool {
        self.c
            .prepare_cached("SELECT lang FROM interwiki WHERE prefix=?1")
            .ok()
            .and_then(|mut s| s.query_row(params![prefix], |r| r.get::<_, i64>(0)).optional().ok().flatten())
            .is_some_and(|l| l != 0)
    }
    fn stat(&mut self, what: &str, arg: &str) -> i64 {
        let one = |sql: &str, p: &[&dyn rusqlite::ToSql]| -> i64 { self.c.prepare_cached(sql).and_then(|mut s| s.query_row(p, |r| r.get(0))).unwrap_or(0) };
        let cat = crate::title::Title::parse_in(arg, crate::title::NS_CATEGORY).map(|p| p.title.name).unwrap_or_default();
        const IN_CAT: &str = "SELECT count(*) FROM categorylink l JOIN page p ON p.id=l.from_page WHERE p.deleted=0 AND l.cat=?1";
        match what {
            "pages" => one("SELECT count(*) FROM page WHERE deleted=0", &[]),
            "articles" => one("SELECT count(*) FROM page WHERE deleted=0 AND ns=0 AND is_redirect=0", &[]),
            "files" => one("SELECT count(*) FROM file WHERE deleted=0", &[]),
            "edits" => one("SELECT count(*) FROM revision", &[]),
            "users" => one("SELECT count(*) FROM user", &[]),
            "admins" => one("SELECT count(*) FROM user WHERE (',' || groups || ',') LIKE '%,sysop,%'", &[]),
            "group" => one("SELECT count(*) FROM user WHERE (',' || groups || ',') LIKE '%,' || ?1 || ',%'", &[&arg]),
            "activeusers" => one("SELECT count(DISTINCT user_id) FROM revision WHERE user_id IS NOT NULL AND ts > ?1", &[&(now() - 30 * 86_400)]),
            "ns" => one("SELECT count(*) FROM page WHERE deleted=0 AND ns=?1", &[&arg.parse::<i64>().unwrap_or(-1)]),
            "cat_all" => one(IN_CAT, &[&cat]),
            "cat_files" => one(&format!("{IN_CAT} AND p.ns=6"), &[&cat]),
            "cat_subcats" => one(&format!("{IN_CAT} AND p.ns=14"), &[&cat]),
            "cat_pages" => one(&format!("{IN_CAT} AND p.ns NOT IN (6,14)"), &[&cat]),
            _ => 0,
        }
    }
    fn page_exists(&mut self, t: &Title) -> bool {
        self.c
            .prepare_cached("SELECT 1 FROM page WHERE ns=?1 AND title=?2 AND deleted=0")
            .ok()
            .and_then(|mut s| s.query_row(params![t.ns, t.name], |_| Ok(())).optional().ok().flatten())
            .is_some()
    }
}

impl Wiki {
    pub fn render_text(&self, c: &Connection, page: &Title, text: &str) -> Rendered {
        let opts = Opts { page, site_name: &self.cfg.site_name, article_path: &self.cfg.article_path, file_path: &self.cfg.file_path, now: now(), tz: &self.cfg.timezone, lang: &self.cfg.site_lang };
        wikitext::render(text, &opts, &mut DbResolver { c, w: self, remote_spent: Default::default() })
    }

    pub fn render_rev(&self, c: &Connection, rev_id: i64, page: &Title, text: &str) -> Arc<Rendered> {
        let key = (rev_id, self.epoch.load(Ordering::SeqCst));
        self.cache.get_with(key, || Arc::new(self.render_text(c, page, text)))
    }

    /// Expand templates only (for `expandtemplates`).
    pub fn expand_text(&self, c: &Connection, page: &Title, text: &str) -> String {
        let opts = Opts { page, site_name: &self.cfg.site_name, article_path: &self.cfg.article_path, file_path: &self.cfg.file_path, now: now(), tz: &self.cfg.timezone, lang: &self.cfg.site_lang };
        wikitext::expand_only(text, &opts, &mut DbResolver { c, w: self, remote_spent: Default::default() })
    }

    /// Add / replace / remove (`url = None`) an interwiki prefix. http(s) URLs containing `$1` only.
    /// `api` is the remote wiki's Action API endpoint (enables federated search); with `trans`
    /// its pages can also be transcluded as `{{prefix:Page}}`.
    /// `lang` marks a language prefix: `[[fr:Titre]]` then becomes a language link of the page.
    pub fn set_interwiki(&self, c: &Connection, actor: &Actor, prefix: &str, url: Option<&str>, api: &str, trans: bool, lang: bool) -> Res<()> {
        actor.require("siteadmin")?;
        let prefix = prefix.trim().to_lowercase().replace(' ', "_");
        if prefix.is_empty() || prefix.len() > 32 || !prefix.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-') {
            return Err(ApiError::bad("badprefix", "prefix must be 1-32 characters of a-z, 0-9, _ or -"));
        }
        if crate::title::ns_by_name(&prefix).is_some() {
            return Err(ApiError::bad("badprefix", "that prefix is a namespace name"));
        }
        match url {
            Some(u) => {
                if !(u.starts_with("https://") || u.starts_with("http://")) || !u.contains("$1") || u.len() > 500 || u.chars().any(|ch| ch.is_whitespace() || ch.is_control() || matches!(ch, '"' | '<' | '>')) {
                    return Err(ApiError::bad("badurl", "url must be an http(s) URL containing $1, without spaces or quotes"));
                }
                let api = api.trim();
                if !api.is_empty() && (!(api.starts_with("https://") || api.starts_with("http://")) || api.len() > 500 || api.chars().any(|ch| ch.is_whitespace() || ch.is_control() || matches!(ch, '"' | '<' | '>' | '#'))) {
                    return Err(ApiError::bad("badurl", "api must be the http(s) URL of the remote wiki's Action API (e.g. https://other.example/api.php)"));
                }
                if trans && api.is_empty() {
                    return Err(ApiError::bad("missingparam", "transclusion needs 'api' (the remote wiki's Action API URL)"));
                }
                c.execute(
                    "INSERT INTO interwiki(prefix,url,api,trans,lang) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(prefix) DO UPDATE SET url=excluded.url, api=excluded.api, trans=excluded.trans, lang=excluded.lang",
                    params![prefix, u, api, trans as i64, lang as i64],
                )?;
            }
            None => {
                if c.execute("DELETE FROM interwiki WHERE prefix=?1", params![prefix])? == 0 {
                    return Err(ApiError::not_found("nosuchprefix", "no such interwiki prefix"));
                }
            }
        }
        log_event(c, "interwiki", if url.is_some() { "iw_set" } else { "iw_delete" }, &actor.name, None, &prefix, json!({ "url": url, "api": api, "trans": trans, "lang": lang }))?;
        self.remote.purge();
        self.mark_dirty();
        Ok(())
    }

    /// Render the latest revision of a live page (cached).
    pub fn render_page(&self, c: &Connection, p: &PageRow) -> Res<(Arc<Rendered>, String)> {
        let text = rev_text(c, p.latest)?;
        Ok((self.render_rev(c, p.latest, &p.title, &text), text))
    }

    /// Refresh link tables, redirect info and the search index for a page.
    fn reindex(&self, c: &Connection, page_id: i64, title: &Title, text: &str) -> Res<Rendered> {
        let r = self.render_text(c, title, text);
        let (rns, rt) = match &r.redirect {
            Some((t, _)) => (Some(t.ns), Some(t.name.clone())),
            None => (None, None),
        };
        c.execute(
            "UPDATE page SET is_redirect=?1, redirect_ns=?2, redirect_title=?3, len=?4, words=?5 WHERE id=?6",
            params![r.redirect.is_some() as i64, rns, rt, text.len() as i64, r.text.split_whitespace().count() as i64, page_id],
        )?;
        for t in ["pagelink", "templatelink", "categorylink", "extlink", "iwlink", "langlink"] {
            c.prepare_cached(&format!("DELETE FROM {t} WHERE from_page=?1"))?.execute(params![page_id])?;
        }
        c.prepare_cached("DELETE FROM pageprop WHERE page=?1")?.execute(params![page_id])?;
        {
            let mut s = c.prepare_cached("INSERT INTO extlink(from_page,url) VALUES(?1,?2)")?;
            for u in &r.extlinks {
                s.execute(params![page_id, u])?;
            }
            let mut s = c.prepare_cached("INSERT INTO iwlink(from_page,prefix,title) VALUES(?1,?2,?3)")?;
            for (p, t) in &r.iwlinks {
                s.execute(params![page_id, p, t])?;
            }
            let mut s = c.prepare_cached("INSERT INTO langlink(from_page,lang,title) VALUES(?1,?2,?3)")?;
            for (l, t) in &r.langlinks {
                s.execute(params![page_id, l, t])?;
            }
            let mut s = c.prepare_cached("INSERT INTO pageprop(page,name,value) VALUES(?1,?2,?3)")?;
            for (name, value) in [("displaytitle", &r.display_title), ("description", &r.description)] {
                if let Some(v) = value {
                    s.execute(params![page_id, name, v])?;
                }
            }
            let mut s = c.prepare_cached("INSERT INTO pagelink(from_page,to_ns,to_title) VALUES(?1,?2,?3)")?;
            for l in &r.links {
                s.execute(params![page_id, l.ns, l.name])?;
            }
            let mut s = c.prepare_cached("INSERT INTO templatelink(from_page,to_ns,to_title) VALUES(?1,?2,?3)")?;
            for l in &r.templates {
                s.execute(params![page_id, l.ns, l.name])?;
            }
            let mut s = c.prepare_cached("INSERT INTO categorylink(from_page,cat,sortkey) VALUES(?1,?2,?3)")?;
            for (cat, key) in &r.categories {
                s.execute(params![page_id, cat, if key.is_empty() { title.name.as_str() } else { key.as_str() }])?;
            }
        }
        let body: String = r.text.chars().filter(|ch| !ch.is_control()).collect();
        c.prepare_cached("DELETE FROM search WHERE rowid=?1")?.execute(params![page_id])?;
        c.prepare_cached("INSERT INTO search(rowid,title,body) VALUES(?1,?2,?3)")?.execute(params![page_id, title.full(), body])?;
        Ok(r)
    }

    /// Add or remove a page from the caller's watchlist (pages need not exist).
    pub fn watch(&self, c: &Connection, actor: &Actor, t: &Title, on: bool) -> Res<()> {
        let uid = actor.user_id.ok_or_else(|| ApiError::unauthorized("a watchlist needs a user account (send an API token)"))?;
        if on {
            let n: i64 = c.query_row("SELECT count(*) FROM watchlist WHERE user=?1", [uid], |r| r.get(0))?;
            if n >= 50_000 {
                return Err(ApiError::bad("watchlistfull", "the watchlist is limited to 50000 entries"));
            }
            c.execute("INSERT INTO watchlist(user,ns,title,added) VALUES(?1,?2,?3,?4) ON CONFLICT DO NOTHING", params![uid, t.ns, t.name, now()])?;
        } else {
            c.execute("DELETE FROM watchlist WHERE user=?1 AND ns=?2 AND title=?3", params![uid, t.ns, t.name])?;
        }
        Ok(())
    }

    pub fn watchlist_raw(c: &Connection, uid: i64, limit: i64, offset: i64) -> Res<Vec<Title>> {
        let mut st = c.prepare_cached("SELECT ns,title FROM watchlist WHERE user=?1 ORDER BY ns,title LIMIT ?2 OFFSET ?3")?;
        let rows = st.query_map(params![uid, limit, offset], |r| Ok(Title::new(r.get(0)?, &r.get::<_, String>(1)?)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Recent revisions of the pages a user watches, newest first.
    pub fn watchlist_changes(c: &Connection, uid: i64, limit: i64, offset: i64) -> Res<Vec<Value>> {
        let mut st = c.prepare_cached(
            "SELECT r.id, r.parent, p.id, p.ns, p.title, r.ts, r.user_name, r.user_id IS NULL, r.comment, r.size, r.minor,
                    coalesce((SELECT size FROM revision WHERE id=r.parent),0)
             FROM watchlist w JOIN page p ON p.ns=w.ns AND p.title=w.title AND p.deleted=0
             JOIN revision r ON r.page=p.id
             WHERE w.user=?1 ORDER BY r.id DESC LIMIT ?2 OFFSET ?3",
        )?;
        let rows = st
            .query_map(params![uid, limit, offset], |r| {
                let t = Title::new(r.get(3)?, &r.get::<_, String>(4)?);
                let (size, old): (i64, i64) = (r.get(9)?, r.get(11)?);
                Ok(json!({
                    "type": if r.get::<_, i64>(1)? == 0 { "new" } else { "edit" }, "revid": r.get::<_, i64>(0)?, "old_revid": r.get::<_, i64>(1)?,
                    "pageid": r.get::<_, i64>(2)?, "ns": t.ns, "title": t.full(), "timestamp": crate::util::iso(r.get(5)?),
                    "user": r.get::<_, String>(6)?, "anon": r.get::<_, bool>(7)?, "comment": r.get::<_, String>(8)?,
                    "newlen": size, "oldlen": old, "minor": r.get::<_, i64>(10)? != 0,
                }))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// A page on another wiki changed: forget rendered HTML and, on the primary, re-render the
    /// pages that transclude it (bounded), so links, search and HTML pick up the new text.
    pub async fn refresh_remote_dependants(&self, api: &str, title: &str) {
        let (api, title) = (api.to_string(), title.to_string());
        if self.is_replica() {
            // Replicas receive the re-rendered tables from the primary; only their caches go.
            self.epoch.fetch_add(1, Ordering::SeqCst);
            return;
        }
        let r = self
            .write_always(move |w, c| {
                let mut st = c.prepare(
                    "SELECT DISTINCT l.from_page, l.title FROM iwlink l JOIN interwiki i ON i.prefix=l.prefix WHERE i.api=?1 AND i.trans=1",
                )?;
                let rows: Vec<(i64, String)> = st.query_map(params![api], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
                let mut n = 0;
                for (pid, t) in rows {
                    if !crate::remote::same_title(&t, &title) || n >= w.cfg.refresh_limit {
                        continue;
                    }
                    if let Some(p) = find_page_by_id(c, pid)? {
                        let text = rev_text(c, p.latest)?;
                        w.reindex(c, pid, &p.title, &text)?;
                        n += 1;
                    }
                }
                w.mark_dirty();
                Ok(n)
            })
            .await;
        match r {
            Ok(n) if n > 0 => tracing::info!(pages = n, "re-rendered pages after a change on another wiki"),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e.info, "could not re-render pages after a change on another wiki"),
        }
    }

    /// Re-render every live page and rebuild all derived tables (links, props, search).
    pub fn reindex_all(&self, c: &Connection) -> Res<usize> {
        let ids: Vec<i64> = c.prepare("SELECT id FROM page WHERE deleted=0 ORDER BY id")?.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
        for id in &ids {
            if let Some(p) = find_page_by_id(c, *id)? {
                let text = rev_text(c, p.latest)?;
                self.reindex(c, *id, &p.title, &text)?;
            }
        }
        self.mark_dirty();
        Ok(ids.len())
    }

    /// Interwiki prefixes that have a remote API: (prefix, url template, api).
    pub fn search_targets(c: &Connection) -> Res<Vec<(String, String, String)>> {
        let mut st = c.prepare_cached("SELECT prefix,url,api FROM interwiki WHERE api<>'' ORDER BY prefix LIMIT 8")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Ask every configured remote wiki in parallel. Does blocking HTTP: call it outside any
    /// database closure. Failures are reported per wiki, never as an error of the request.
    pub fn federated_search(&self, targets: &[(String, String, String)], query: &str, limit: i64) -> Value {
        let mut out = serde_json::Map::new();
        let results: Vec<(String, Value)> = std::thread::scope(|sc| {
            let handles: Vec<_> = targets
                .iter()
                .map(|(prefix, url, api)| {
                    sc.spawn(move || {
                        let v = match self.remote.search(api, query, limit) {
                            Ok((total, hits)) => json!({
                                "totalhits": total,
                                "pages": hits.iter().map(|(title, snippet)| json!({
                                    "title": title,
                                    "prefixed": format!("{prefix}:{title}"),
                                    "url": url.replace("$1", &crate::util::path_encode(title)),
                                    "excerpt": crate::util::strip_html(snippet),
                                })).collect::<Vec<_>>(),
                            }),
                            Err(e) => json!({ "error": e, "pages": [] }),
                        };
                        (prefix.clone(), v)
                    })
                })
                .collect();
            handles.into_iter().filter_map(|h| h.join().ok()).collect()
        });
        for (p, v) in results {
            out.insert(p, v);
        }
        Value::Object(out)
    }

    /// Rebuild only the full-text entry of a page (replicas: every other table arrives by replication).
    pub fn reindex_search(&self, c: &Connection, page_id: i64) -> Res<()> {
        c.prepare_cached("DELETE FROM search WHERE rowid=?1")?.execute(params![page_id])?;
        if let Some(p) = find_page_by_id(c, page_id)? {
            let text = rev_text(c, p.latest)?;
            let r = self.render_text(c, &p.title, &text);
            let body: String = r.text.chars().filter(|ch| !ch.is_control()).collect();
            c.prepare_cached("INSERT INTO search(rowid,title,body) VALUES(?1,?2,?3)")?.execute(params![page_id, p.title.full(), body])?;
        }
        Ok(())
    }

    /// Pages that transclude `title` have stale link tables; re-render them (bounded).
    fn refresh_dependents(&self, c: &Connection, title: &Title) -> Res<()> {
        let ids: Vec<i64> = {
            let mut s = c.prepare_cached(
                "SELECT DISTINCT t.from_page FROM templatelink t JOIN page p ON p.id=t.from_page
                 WHERE t.to_ns=?1 AND t.to_title=?2 AND p.deleted=0 LIMIT ?3",
            )?;
            let rows = s.query_map(params![title.ns, title.name, self.cfg.refresh_limit as i64 + 1], |r| r.get(0))?.collect::<Result<_, _>>()?;
            rows
        };
        if ids.is_empty() {
            return Ok(());
        }
        self.mark_dirty();
        if ids.len() > self.cfg.refresh_limit {
            tracing::warn!(template = %title.full(), limit = self.cfg.refresh_limit, "more dependents than REFRESH_LINKS_LIMIT; some link tables are stale");
        }
        for id in ids.into_iter().take(self.cfg.refresh_limit) {
            if let Some(p) = find_page_by_id(c, id)? {
                let text = rev_text(c, p.latest)?;
                self.reindex(c, p.id, &p.title, &text)?;
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------
// logging
// ---------------------------------------------------------------------------------------

pub fn log_event(c: &Connection, typ: &str, action: &str, user: &str, t: Option<&Title>, comment: &str, params_json: Value) -> Res<()> {
    c.execute(
        "INSERT INTO log(ts,type,action,user_name,ns,title,comment,params) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        params![now(), typ, action, user, t.map_or(0, |t| t.ns), t.map_or("", |t| t.name.as_str()), comment, params_json.to_string()],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------------------
// editing
// ---------------------------------------------------------------------------------------

pub struct EditReq {
    pub title: Title,
    pub text: String,
    pub summary: String,
    pub minor: bool,
    /// If set and not equal to the page's latest revision → `editconflict`.
    pub base_rev: Option<i64>,
    pub create_only: bool,
    pub no_create: bool,
}

pub struct EditResult {
    pub page_id: i64,
    pub title: Title,
    pub new: bool,
    pub nochange: bool,
    pub rev_id: i64,
    pub old_rev: i64,
    pub ts: i64,
    pub size: i64,
    /// True when an out-of-date `base_rev` was reconciled by a clean three-way merge.
    pub merged: bool,
}

fn insert_revision(c: &Connection, page_id: i64, parent: i64, actor: &Actor, text: &str, summary: &str, minor: bool, ts: i64) -> Res<i64> {
    c.prepare_cached(
        "INSERT INTO revision(page,parent,user_id,user_name,ts,comment,minor,size,sha256,model,text)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'wikitext',?10)",
    )?
    .execute(params![page_id, parent, actor.user_id, actor.name, ts, summary, minor as i64, text.len() as i64, sha256_hex(text.as_bytes()), text])?;
    Ok(c.last_insert_rowid())
}

impl Wiki {
    pub fn clean_text(&self, raw: &str) -> Res<String> {
        if raw.len() > self.cfg.max_page_bytes {
            return Err(ApiError::new(
                axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                "contenttoobig",
                format!("content exceeds the {} byte limit", self.cfg.max_page_bytes),
            ));
        }
        if raw.contains('\0') {
            return Err(ApiError::bad("badcontent", "content must not contain NUL bytes"));
        }
        Ok(raw.replace("\r\n", "\n").replace('\r', "\n").trim_end().to_string())
    }

    fn check_edit_protection(&self, actor: &Actor, p: &PageRow) -> Res<()> {
        if actor.meets_level(&p.protect_edit) {
            return Ok(());
        }
        Err(ApiError::forbidden(
            "protectedpage",
            format!("this page is protected (edit={}); you cannot edit it", p.protect_edit),
        ))
    }

    pub fn edit(&self, c: &Connection, actor: &Actor, req: EditReq) -> Res<EditResult> {
        actor.require("edit")?;
        let mut text = self.clean_text(&req.text)?;
        let mut merged = false;
        let summary = truncate_chars(req.summary.trim(), 500);
        let existing = find_page(c, &req.title)?;

        match &existing {
            Some(p) => {
                if req.create_only {
                    return Err(ApiError::conflict("articleexists", "the page already exists"));
                }
                self.check_edit_protection(actor, p)?;
                if let Some(base) = req.base_rev {
                    if base != p.latest {
                        // Someone else saved in between: try a three-way merge before giving up.
                        let base_text = get_rev(c, base)?.filter(|r| r.page == p.id).map(|r| rev_text(c, r.id)).transpose()?;
                        let ours = rev_text(c, p.latest)?;
                        let conflict = || {
                            ApiError::conflict(
                                "editconflict",
                                format!("the page was edited since revision {base} (latest is {}) and the changes overlap; re-fetch and retry", p.latest),
                            )
                        };
                        let Some(base_text) = base_text else { return Err(conflict()) };
                        match crate::merge::merge3(&base_text, &ours, &text) {
                            crate::merge::Merge::Clean(m) => {
                                text = self.clean_text(&m)?;
                                merged = true;
                            }
                            crate::merge::Merge::Conflict => {
                                // Hand the client everything it needs to resolve the conflict.
                                let (marked, regions) = crate::merge::with_markers(&base_text, &ours, &text);
                                return Err(conflict().with("conflict", json!({ "base_rev": base, "current_rev": p.latest, "current": ours, "merged": marked, "conflicts": regions })));
                            }
                        }
                    }
                }
                if rev_text(c, p.latest)? == text {
                    return Ok(EditResult {
                        page_id: p.id,
                        title: p.title.clone(),
                        new: false,
                        nochange: true,
                        rev_id: p.latest,
                        old_rev: p.latest,
                        ts: p.touched,
                        size: p.len,
                        merged: false,
                    });
                }
            }
            None => {
                if req.no_create {
                    return Err(ApiError::not_found("missingtitle", "the page does not exist"));
                }
                actor.require("createpage")?;
                if text.trim().is_empty() {
                    return Err(ApiError::bad("emptypage", "cannot create an empty page"));
                }
            }
        }

        let ts = now();
        let (page_id, parent, was_redirect) = match &existing {
            Some(p) => (p.id, p.latest, p.is_redirect),
            None => {
                c.execute(
                    "INSERT INTO page(ns,title,touched,created) VALUES(?1,?2,?3,?3)",
                    params![req.title.ns, req.title.name, ts],
                )?;
                (c.last_insert_rowid(), 0, false)
            }
        };
        let rev_id = insert_revision(c, page_id, parent, actor, &text, &summary, req.minor, ts)?;
        c.execute("UPDATE page SET latest=?1, touched=?2 WHERE id=?3", params![rev_id, ts, page_id])?;
        let r = self.reindex(c, page_id, &req.title, &text)?;
        if let Some(uid) = actor.user_id {
            c.execute("UPDATE user SET editcount=editcount+1 WHERE id=?1", params![uid])?;
        }
        if existing.is_none() || was_redirect != r.redirect.is_some() || req.title.ns == NS_TEMPLATE {
            self.mark_dirty();
        }
        self.refresh_dependents(c, &req.title)?;

        Ok(EditResult { page_id, title: req.title, new: existing.is_none(), nochange: false, rev_id, old_rev: parent, ts, size: text.len() as i64, merged })
    }

    pub fn move_page(&self, c: &Connection, actor: &Actor, from: &Title, to: &Title, reason: &str, noredirect: bool) -> Res<PageRow> {
        actor.require("move")?;
        let p = find_page(c, from)?.ok_or_else(|| ApiError::not_found("missingtitle", "the page to move does not exist"))?;
        if !actor.meets_level(&p.protect_move) {
            return Err(ApiError::forbidden("protectedpage", format!("this page is move-protected ({})", p.protect_move)));
        }
        if from == to {
            return Err(ApiError::bad("selfmove", "cannot move a page onto itself"));
        }
        if find_page(c, to)?.is_some() {
            return Err(ApiError::conflict("articleexists", "a page with the target title already exists"));
        }
        if noredirect {
            actor.require("suppressredirect")?;
        }
        let ts = now();
        let reason = truncate_chars(reason.trim(), 500);
        c.execute("UPDATE page SET ns=?1, title=?2, touched=?3 WHERE id=?4", params![to.ns, to.name, ts, p.id])?;
        let text = rev_text(c, p.latest)?;
        self.reindex(c, p.id, to, &text)?;
        log_event(c, "move", "move", &actor.name, Some(from), &reason, json!({"target_ns": to.ns, "target_title": to.name, "redirect": !noredirect}))?;
        if !noredirect {
            c.execute("INSERT INTO page(ns,title,touched,created) VALUES(?1,?2,?3,?3)", params![from.ns, from.name, ts])?;
            let rid = c.last_insert_rowid();
            let body = format!("#REDIRECT [[{}]]", to.full());
            let comment = format!("Moved page [[{}]] to [[{}]]", from.full(), to.full());
            let rev = insert_revision(c, rid, 0, actor, &body, &comment, false, ts)?;
            c.execute("UPDATE page SET latest=?1 WHERE id=?2", params![rev, rid])?;
            self.reindex(c, rid, from, &body)?;
        }
        self.mark_dirty();
        self.refresh_dependents(c, from)?;
        self.refresh_dependents(c, to)?;
        find_page(c, to)?.ok_or_else(|| ApiError::internal("moved page vanished"))
    }

    pub fn delete_page(&self, c: &Connection, actor: &Actor, t: &Title, reason: &str) -> Res<i64> {
        actor.require("delete")?;
        let p = find_page(c, t)?.ok_or_else(|| ApiError::not_found("missingtitle", "the page does not exist"))?;
        c.execute("UPDATE page SET deleted=1, touched=?1 WHERE id=?2", params![now(), p.id])?;
        c.execute("DELETE FROM search WHERE rowid=?1", params![p.id])?;
        for tbl in ["pagelink", "templatelink", "categorylink"] {
            c.execute(&format!("DELETE FROM {tbl} WHERE from_page=?1"), params![p.id])?;
        }
        if t.ns == crate::title::NS_FILE {
            crate::files::set_deleted(c, &t.name, true)?;
        }
        log_event(c, "delete", "delete", &actor.name, Some(t), truncate_chars(reason.trim(), 500).as_str(), json!({"revisions": count_revs(c, p.id)?}))?;
        self.mark_dirty();
        self.refresh_dependents(c, t)?;
        Ok(p.id)
    }

    pub fn undelete_page(&self, c: &Connection, actor: &Actor, t: &Title, reason: &str) -> Res<PageRow> {
        actor.require("undelete")?;
        if find_page(c, t)?.is_some() {
            return Err(ApiError::conflict("undeleteconflict", "a live page with this title already exists"));
        }
        let id: i64 = c
            .query_row("SELECT id FROM page WHERE ns=?1 AND title=?2 AND deleted=1 ORDER BY id DESC LIMIT 1", params![t.ns, t.name], |r| r.get(0))
            .optional()?
            .ok_or_else(|| ApiError::not_found("nothingtoundelete", "there is no deleted page with this title"))?;
        c.execute("UPDATE page SET deleted=0, touched=?1 WHERE id=?2", params![now(), id])?;
        let p = find_page_by_id(c, id)?.ok_or_else(|| ApiError::internal("undeleted page vanished"))?;
        let text = rev_text(c, p.latest)?;
        self.reindex(c, id, &p.title, &text)?;
        if t.ns == crate::title::NS_FILE {
            crate::files::set_deleted(c, &t.name, false)?;
        }
        log_event(c, "delete", "restore", &actor.name, Some(t), truncate_chars(reason.trim(), 500).as_str(), json!({}))?;
        self.mark_dirty();
        self.refresh_dependents(c, t)?;
        find_page_by_id(c, id)?.ok_or_else(|| ApiError::internal("undeleted page vanished"))
    }

    pub fn protect(&self, c: &Connection, actor: &Actor, t: &Title, edit: Option<&str>, mv: Option<&str>, reason: &str) -> Res<PageRow> {
        actor.require("protect")?;
        let p = find_page(c, t)?.ok_or_else(|| ApiError::not_found("missingtitle", "the page does not exist"))?;
        let norm = |s: &str| -> Res<String> {
            match s {
                "" | "all" | "none" => Ok(String::new()),
                "user" | "autoconfirmed" => Ok("user".into()),
                "sysop" => Ok("sysop".into()),
                other => Err(ApiError::bad("badprotectlevel", format!("unknown protection level '{other}' (use all, user or sysop)"))),
            }
        };
        let e = edit.map(norm).transpose()?.unwrap_or(p.protect_edit.clone());
        let m = mv.map(norm).transpose()?.unwrap_or(p.protect_move.clone());
        c.execute("UPDATE page SET protect_edit=?1, protect_move=?2 WHERE id=?3", params![e, m, p.id])?;
        log_event(c, "protect", "protect", &actor.name, Some(t), truncate_chars(reason.trim(), 500).as_str(), json!({"edit": e, "move": m}))?;
        find_page(c, t)?.ok_or_else(|| ApiError::internal("page vanished"))
    }

    /// Revert the latest run of edits by the last editor to the version before them.
    pub fn rollback(&self, c: &Connection, actor: &Actor, t: &Title, user: Option<&str>, summary: &str) -> Res<(EditResult, String)> {
        actor.require("rollback")?;
        let p = find_page(c, t)?.ok_or_else(|| ApiError::not_found("missingtitle", "the page does not exist"))?;
        let latest = get_rev(c, p.latest)?.ok_or_else(|| ApiError::internal("missing latest revision"))?;
        if let Some(u) = user {
            if !u.trim().replace('_', " ").eq_ignore_ascii_case(&latest.user_name) {
                return Err(ApiError::conflict("alreadyrolled", format!("the last edit was made by {}, not {u}", latest.user_name)));
            }
        }
        let target: Option<i64> = c
            .query_row(
                "SELECT id FROM revision WHERE page=?1 AND id<?2 AND user_name<>?3 ORDER BY id DESC LIMIT 1",
                params![p.id, latest.id, latest.user_name],
                |r| r.get(0),
            )
            .optional()?;
        let target = target.ok_or_else(|| ApiError::bad("onlyauthor", "the page has no earlier revision by a different user to roll back to"))?;
        let text = rev_text(c, target)?;
        let summary = if summary.trim().is_empty() { format!("Reverted edits by {} to revision {target}", latest.user_name) } else { summary.to_string() };
        let res = self.edit(c, actor, EditReq { title: t.clone(), text, summary, minor: false, base_rev: Some(latest.id), create_only: false, no_create: true })?;
        Ok((res, latest.user_name))
    }
}

impl Wiki {
    /// Undo `undo_rev` (and everything after `after`, default: just that one) by three-way
    /// merging its inverse into the current text. Fails with `undofailure` if later edits overlap.
    pub fn undo(&self, c: &Connection, actor: &Actor, t: &Title, undo_rev: i64, after: Option<i64>, summary: &str) -> Res<EditResult> {
        let p = find_page(c, t)?.ok_or_else(|| ApiError::not_found("missingtitle", "the page does not exist"))?;
        let rev = get_rev(c, undo_rev)?.filter(|r| r.page == p.id).ok_or_else(|| ApiError::bad("nosuchrevid", "that revision does not belong to this page"))?;
        let before = match after {
            Some(a) => get_rev(c, a)?.filter(|r| r.page == p.id && r.id < rev.id).ok_or_else(|| ApiError::bad("badundoafter", "undoafter must be an earlier revision of this page"))?.id,
            None => rev.parent,
        };
        if before == 0 {
            return Err(ApiError::bad("undocreation", "the creation of a page cannot be undone; delete it instead"));
        }
        let (base, ours, theirs) = (rev_text(c, rev.id)?, rev_text(c, p.latest)?, rev_text(c, before)?);
        let new_text = match crate::merge::merge3(&base, &ours, &theirs) {
            crate::merge::Merge::Clean(m) => m,
            crate::merge::Merge::Conflict => {
                let (marked, regions) = crate::merge::with_markers(&base, &ours, &theirs);
                return Err(ApiError::conflict("undofailure", "later edits overlap with this revision; the undo cannot be applied automatically")
                    .with("conflict", json!({ "current_rev": p.latest, "current": ours, "merged": marked, "conflicts": regions })));
            }
        };
        let summary = if summary.trim().is_empty() { format!("Undo revision {} by {}", rev.id, rev.user_name) } else { summary.to_string() };
        self.edit(c, actor, EditReq { title: t.clone(), text: new_text, summary, minor: false, base_rev: Some(p.latest), create_only: false, no_create: true })
    }

    /// Make an old revision the current one again.
    pub fn restore(&self, c: &Connection, actor: &Actor, t: &Title, rev_id: i64, summary: &str) -> Res<EditResult> {
        let p = find_page(c, t)?.ok_or_else(|| ApiError::not_found("missingtitle", "the page does not exist"))?;
        let rev = get_rev(c, rev_id)?.filter(|r| r.page == p.id).ok_or_else(|| ApiError::bad("nosuchrevid", "that revision does not belong to this page"))?;
        let summary = if summary.trim().is_empty() { format!("Restored revision {}", rev.id) } else { summary.to_string() };
        self.edit(c, actor, EditReq { title: t.clone(), text: rev_text(c, rev.id)?, summary, minor: false, base_rev: None, create_only: false, no_create: true })
    }
}

fn count_revs(c: &Connection, page: i64) -> Res<i64> {
    Ok(c.query_row("SELECT count(*) FROM revision WHERE page=?1", params![page], |r| r.get(0))?)
}

#[cfg(test)]
pub mod testutil {
    use super::*;
    use crate::users;

    pub struct TestWiki {
        pub wiki: Wiki,
        pub admin: Actor,
        pub anon: Actor,
        _dir: std::path::PathBuf,
    }

    impl Drop for TestWiki {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self._dir);
        }
    }

    pub fn make(anon_edit: bool) -> TestWiki {
        let dir = std::env::temp_dir().join(format!("hoepedia-test-{}-{}", std::process::id(), crate::util::new_token()));
        let mut cfg = Config::for_tests(dir.join("w.db").to_str().unwrap());
        cfg.anon_edit = anon_edit;
        cfg.anon_read = true;
        let db = Db::open(&cfg.db_path, 2).unwrap();
        let (admin, _) = {
            let g = db.writer();
            users::create_user(&g, "Admin", &["sysop".to_string()]).unwrap()
        };
        let anon = Actor::anon("127.0.0.1".parse().unwrap(), &cfg);
        let admin = Actor::from_user(&admin);
        TestWiki { wiki: Wiki::new(Arc::new(cfg), db), admin, anon, _dir: dir }
    }

    pub fn req(title: &str, text: &str) -> EditReq {
        EditReq { title: Title::parse(title).unwrap().title, text: text.into(), summary: "t".into(), minor: false, base_rev: None, create_only: false, no_create: false }
    }
}

#[cfg(test)]
mod tests {
    use super::{testutil::*, *};

    #[test]
    fn create_edit_conflict_nochange() {
        let t = make(false);
        let c = t.wiki.db.writer();
        let r1 = t.wiki.edit(&c, &t.admin, req("Foo", "Hello [[Bar]]")).unwrap();
        assert!(r1.new && r1.old_rev == 0);
        let same = t.wiki.edit(&c, &t.admin, req("Foo", "Hello [[Bar]]\n")).unwrap();
        assert!(same.nochange);
        let mut r = req("Foo", "Changed");
        r.base_rev = Some(999);
        assert_eq!(t.wiki.edit(&c, &t.admin, r).err().unwrap().code, "editconflict");
        let mut r = req("Foo", "Changed");
        r.base_rev = Some(r1.rev_id);
        let r2 = t.wiki.edit(&c, &t.admin, r).unwrap();
        assert_eq!(r2.old_rev, r1.rev_id);
        let mut r = req("Foo", "x");
        r.create_only = true;
        assert_eq!(t.wiki.edit(&c, &t.admin, r).err().unwrap().code, "articleexists");
    }

    #[test]
    fn anon_cannot_edit_unless_enabled() {
        let t = make(false);
        let c = t.wiki.db.writer();
        assert_eq!(t.wiki.edit(&c, &t.anon, req("Foo", "x")).err().unwrap().status, 401);
        let t2 = make(true);
        let c2 = t2.wiki.db.writer();
        assert!(t2.wiki.edit(&c2, &t2.anon, req("Foo", "x")).is_ok());
    }

    #[test]
    fn links_categories_templates_tracked_and_refreshed() {
        let t = make(false);
        let c = t.wiki.db.writer();
        t.wiki.edit(&c, &t.admin, req("Template:Cat", "[[Category:Things]]")).unwrap();
        let r = t.wiki.edit(&c, &t.admin, req("Page A", "{{Cat}} see [[Page B]]")).unwrap();
        let cats: i64 = c.query_row("SELECT count(*) FROM categorylink WHERE cat='Things' AND from_page=?1", [r.page_id], |r| r.get(0)).unwrap();
        assert_eq!(cats, 1);
        // Editing the template removes the category from the transcluding page.
        t.wiki.edit(&c, &t.admin, req("Template:Cat", "nothing")).unwrap();
        let cats: i64 = c.query_row("SELECT count(*) FROM categorylink WHERE cat='Things'", [], |r| r.get(0)).unwrap();
        assert_eq!(cats, 0);
        let links: i64 = c.query_row("SELECT count(*) FROM pagelink WHERE to_title='Page B'", [], |r| r.get(0)).unwrap();
        assert_eq!(links, 1);
    }

    #[test]
    fn move_delete_undelete_protect_rollback() {
        let t = make(true);
        let c = t.wiki.db.writer();
        let (a, b) = (Title::parse("Old").unwrap().title, Title::parse("New").unwrap().title);
        t.wiki.edit(&c, &t.admin, req("Old", "content")).unwrap();
        t.wiki.move_page(&c, &t.admin, &a, &b, "rename", false).unwrap();
        assert!(find_page(&c, &a).unwrap().unwrap().is_redirect);
        assert_eq!(find_page(&c, &b).unwrap().unwrap().title, b);
        assert_eq!(t.wiki.move_page(&c, &t.admin, &a, &b, "", false).err().unwrap().code, "articleexists");

        t.wiki.delete_page(&c, &t.admin, &b, "gone").unwrap();
        assert!(find_page(&c, &b).unwrap().is_none());
        // re-create the same title while the old one is deleted, then undelete must conflict
        t.wiki.edit(&c, &t.admin, req("New", "fresh")).unwrap();
        assert_eq!(t.wiki.undelete_page(&c, &t.admin, &b, "").err().unwrap().code, "undeleteconflict");
        t.wiki.delete_page(&c, &t.admin, &b, "again").unwrap();
        let p = t.wiki.undelete_page(&c, &t.admin, &b, "back").unwrap();
        assert!(p.latest > 0);

        t.wiki.protect(&c, &t.admin, &b, Some("sysop"), None, "vandalism").unwrap();
        assert_eq!(t.wiki.edit(&c, &t.anon, req("New", "hacked")).err().unwrap().code, "protectedpage");

        // rollback: anon vandalises the unprotected page
        t.wiki.edit(&c, &t.admin, req("Page", "good")).unwrap();
        t.wiki.edit(&c, &t.anon, req("Page", "bad")).unwrap();
        let (res, undone) = t.wiki.rollback(&c, &t.admin, &Title::parse("Page").unwrap().title, Some("127.0.0.1"), "").unwrap();
        assert_eq!(undone, "127.0.0.1");
        assert_eq!(rev_text(&c, res.rev_id).unwrap(), "good");
    }

    #[test]
    fn conflict_merges_and_undo() {
        let t = make(false);
        let c = t.wiki.db.writer();
        let doc = Title::parse("Doc").unwrap().title;
        let r1 = t.wiki.edit(&c, &t.admin, req("Doc", "one\ntwo\nthree\nfour\n")).unwrap();
        let r2 = t.wiki.edit(&c, &t.admin, req("Doc", "one\ntwo\nthree\nFOUR\n")).unwrap();
        let mut alice = req("Doc", "one\nTWO\nthree\nfour\n");
        alice.base_rev = Some(r1.rev_id);
        let m = t.wiki.edit(&c, &t.admin, alice).unwrap();
        assert!(m.merged);
        assert_eq!(rev_text(&c, m.rev_id).unwrap(), "one\nTWO\nthree\nFOUR");
        let mut clash = req("Doc", "one\nX\nthree\nFOUR\n");
        clash.base_rev = Some(r1.rev_id);
        assert_eq!(t.wiki.edit(&c, &t.admin, clash).err().unwrap().code, "editconflict");
        let u = t.wiki.undo(&c, &t.admin, &doc, r2.rev_id, None, "").unwrap();
        assert_eq!(rev_text(&c, u.rev_id).unwrap(), "one\nTWO\nthree\nfour");
        let r = t.wiki.restore(&c, &t.admin, &doc, r1.rev_id, "").unwrap();
        assert_eq!(rev_text(&c, r.rev_id).unwrap(), "one\ntwo\nthree\nfour");
        assert_eq!(t.wiki.undo(&c, &t.admin, &doc, r1.rev_id, None, "").err().unwrap().code, "undocreation");
    }

    #[test]
    fn interwiki_admin_rules_and_readonly_flag() {
        let t = make(false);
        let c = t.wiki.db.writer();
        assert_eq!(t.wiki.set_interwiki(&c, &t.anon, "wp", Some("https://x/$1"), "", false, false).err().unwrap().status, 401);
        assert_eq!(t.wiki.set_interwiki(&c, &t.admin, "wp", Some("javascript:alert($1)"), "", false, false).err().unwrap().code, "badurl");
        assert_eq!(t.wiki.set_interwiki(&c, &t.admin, "wp", Some("https://x/nodollar"), "", false, false).err().unwrap().code, "badurl");
        assert_eq!(t.wiki.set_interwiki(&c, &t.admin, "template", Some("https://x/$1"), "", false, false).err().unwrap().code, "badprefix");
        t.wiki.set_interwiki(&c, &t.admin, "WP", Some("https://en.example.org/wiki/$1"), "", false, false).unwrap();
        let r = t.wiki.edit(&c, &t.admin, req("Page", "see [[wp:Rust]]")).unwrap();
        let html = t.wiki.render_text(&c, &Title::parse("Page").unwrap().title, &rev_text(&c, r.rev_id).unwrap()).html;
        assert!(html.contains("https://en.example.org/wiki/Rust"), "{html}");
        t.wiki.set_interwiki(&c, &t.admin, "wp", None, "", false, false).unwrap();
        assert_eq!(t.wiki.set_interwiki(&c, &t.admin, "wp", None, "", false, false).err().unwrap().code, "nosuchprefix");
    }

    #[test]
    fn derived_tables_and_interwiki_api_rules() {
        let t = make(false);
        let c = t.wiki.db.writer();
        assert_eq!(t.wiki.set_interwiki(&c, &t.admin, "o", Some("https://o.example/wiki/$1"), "ftp://o.example/api.php", false, false).err().unwrap().code, "badurl");
        assert_eq!(t.wiki.set_interwiki(&c, &t.admin, "o", Some("https://o.example/wiki/$1"), "", true, false).err().unwrap().code, "missingparam");
        t.wiki.set_interwiki(&c, &t.admin, "o", Some("https://o.example/wiki/$1"), "https://o.example/api.php", true, false).unwrap();
        assert_eq!(Wiki::search_targets(&c).unwrap(), vec![("o".to_string(), "https://o.example/wiki/$1".to_string(), "https://o.example/api.php".to_string())]);
        let r = t.wiki.edit(&c, &t.admin, req("Doc", "{{SHORTDESC:A doc}}{{DISPLAYTITLE:doc}} [https://example.org/x site] [[o:Remote page]] [[File:Pic.png]]")).unwrap();
        let count = |sql: &str| -> i64 { c.query_row(sql, [r.page_id], |x| x.get(0)).unwrap() };
        assert_eq!(count("SELECT count(*) FROM extlink WHERE from_page=?1 AND url='https://example.org/x'"), 1);
        assert_eq!(count("SELECT count(*) FROM iwlink WHERE from_page=?1 AND prefix='o' AND title='Remote page'"), 1);
        assert_eq!(count("SELECT count(*) FROM pageprop WHERE page=?1 AND name='description' AND value='A doc'"), 1);
        assert_eq!(count("SELECT count(*) FROM pagelink WHERE from_page=?1 AND to_ns=6"), 1);
        // a full reindex produces the same rows
        assert_eq!(t.wiki.reindex_all(&c).unwrap(), 1);
        assert_eq!(count("SELECT count(*) FROM extlink WHERE from_page=?1"), 1);
        // anonymous identity modes
        let ip: std::net::IpAddr = "203.0.113.77".parse().unwrap();
        assert_eq!(crate::users::anon_name(ip, "ip"), "203.0.113.77");
        assert_eq!(crate::users::anon_name(ip, "masked"), "203.0.113.x");
        assert_eq!(crate::users::anon_name("2001:db8:85a3::8a2e:370:7334".parse().unwrap(), "masked"), "2001:db8:85a3::x");
        let h = crate::users::anon_name(ip, "hashed");
        assert!(h.starts_with("Anon-") && h.len() == 15 && h == crate::users::anon_name(ip, "hashed") && h != crate::users::anon_name("203.0.113.78".parse().unwrap(), "hashed"), "{h}");
    }

    #[test]
    fn html_wikitext_roundtrip_is_stable() {
        let t = make(false);
        let c = t.wiki.db.writer();
        t.wiki.edit(&c, &t.admin, req("Exists", "x")).unwrap();
        let src = "Intro with '''bold''', ''italic'' and [[Exists]], [[Missing page|a label]], https://example.com and [https://e.com/x site].\n\n\
== Heading ==\nPara two<br>line.\n\n* a\n** b\n* c\n\n# one\n# two\n\n; term : def\n\n\
{| class=\"wikitable\"\n|+ Cap\n! A !! B\n|-\n| 1 || colspan=\"2\" | 2\n|}\n\n----\n=== Sub ===\n<u>under</u> and [[#Heading]] [[File:Cat.png|A cat]]\n\n\
<div class=\"box\" style=\"color:red\">\nInside the box\n</div>\n\n[[File:Cat.png|thumb|300px|left|Caption ''here'']]\n\n<span class=\"hl\">hi</span> [[File:Cat.png|120px]] text\n\n<blockquote>\nQuoted\n</blockquote>";
        let page = Title::parse("Doc").unwrap().title;
        let h1 = t.wiki.render_text(&c, &page, src).html;
        let cx = crate::htmlwt::Ctx { article_path: &t.wiki.cfg.article_path, file_path: &t.wiki.cfg.file_path };
        let back = crate::htmlwt::html_to_wikitext(&h1, &cx);
        let h2 = t.wiki.render_text(&c, &page, &back).html;
        assert_eq!(h1, h2, "round trip changed the page\n--- html1 ---\n{h1}\n--- html2 ---\n{h2}\n--- wikitext ---\n{back}");
    }

    #[test]
    fn size_limits_and_nul() {
        let t = make(false);
        let c = t.wiki.db.writer();
        assert_eq!(t.wiki.edit(&c, &t.admin, req("X", "a\0b")).err().unwrap().code, "badcontent");
        assert_eq!(t.wiki.edit(&c, &t.admin, req("X", "   ")).err().unwrap().code, "emptypage");
    }
}
