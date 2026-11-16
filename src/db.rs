//! SQLite storage: one writer connection + a few read-only connections (WAL mode).

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex, MutexGuard,
};

use rusqlite::{Connection, OpenFlags};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS user (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE COLLATE NOCASE,
    token_hash  TEXT UNIQUE,
    groups      TEXT NOT NULL DEFAULT '',
    blocked     INTEGER NOT NULL DEFAULT 0,
    block_reason TEXT NOT NULL DEFAULT '',
    registered  INTEGER NOT NULL,
    editcount   INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS page (
    id             INTEGER PRIMARY KEY,
    ns             INTEGER NOT NULL,
    title          TEXT NOT NULL,
    latest         INTEGER NOT NULL DEFAULT 0,
    is_redirect    INTEGER NOT NULL DEFAULT 0,
    redirect_ns    INTEGER,
    redirect_title TEXT,
    protect_edit   TEXT NOT NULL DEFAULT '',
    protect_move   TEXT NOT NULL DEFAULT '',
    len            INTEGER NOT NULL DEFAULT 0,
    words          INTEGER NOT NULL DEFAULT 0,
    touched        INTEGER NOT NULL,
    created        INTEGER NOT NULL,
    deleted        INTEGER NOT NULL DEFAULT 0
);
-- A deleted page keeps its row (and history); a new page may reuse the title.
CREATE UNIQUE INDEX IF NOT EXISTS page_name ON page(ns, title) WHERE deleted = 0;
CREATE INDEX IF NOT EXISTS page_deleted_name ON page(ns, title) WHERE deleted = 1;

CREATE TABLE IF NOT EXISTS revision (
    id        INTEGER PRIMARY KEY,
    page      INTEGER NOT NULL REFERENCES page(id),
    parent    INTEGER NOT NULL DEFAULT 0,
    user_id   INTEGER,
    user_name TEXT NOT NULL,
    ts        INTEGER NOT NULL,
    comment   TEXT NOT NULL DEFAULT '',
    minor     INTEGER NOT NULL DEFAULT 0,
    size      INTEGER NOT NULL,
    sha256    TEXT NOT NULL,
    model     TEXT NOT NULL DEFAULT 'wikitext',
    text      TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS rev_page ON revision(page, id);
CREATE INDEX IF NOT EXISTS rev_ts ON revision(ts, id);
CREATE INDEX IF NOT EXISTS rev_user ON revision(user_name, id);

CREATE TABLE IF NOT EXISTS pagelink (from_page INTEGER NOT NULL, to_ns INTEGER NOT NULL, to_title TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS pagelink_from ON pagelink(from_page);
CREATE INDEX IF NOT EXISTS pagelink_to ON pagelink(to_ns, to_title);

CREATE TABLE IF NOT EXISTS templatelink (from_page INTEGER NOT NULL, to_ns INTEGER NOT NULL, to_title TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS tpllink_from ON templatelink(from_page);
CREATE INDEX IF NOT EXISTS tpllink_to ON templatelink(to_ns, to_title);

CREATE TABLE IF NOT EXISTS categorylink (from_page INTEGER NOT NULL, cat TEXT NOT NULL, sortkey TEXT NOT NULL DEFAULT '');
CREATE INDEX IF NOT EXISTS catlink_from ON categorylink(from_page);
CREATE INDEX IF NOT EXISTS catlink_cat ON categorylink(cat, sortkey);

CREATE TABLE IF NOT EXISTS log (
    id        INTEGER PRIMARY KEY,
    ts        INTEGER NOT NULL,
    type      TEXT NOT NULL,
    action    TEXT NOT NULL,
    user_name TEXT NOT NULL,
    ns        INTEGER NOT NULL DEFAULT 0,
    title     TEXT NOT NULL DEFAULT '',
    comment   TEXT NOT NULL DEFAULT '',
    params    TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS log_ts ON log(ts, id);
CREATE INDEX IF NOT EXISTS log_type ON log(type, id);

CREATE TABLE IF NOT EXISTS interwiki (prefix TEXT PRIMARY KEY, url TEXT NOT NULL);

CREATE TABLE IF NOT EXISTS extlink (from_page INTEGER NOT NULL, url TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS extlink_from ON extlink(from_page);
CREATE INDEX IF NOT EXISTS extlink_url ON extlink(url);

CREATE TABLE IF NOT EXISTS iwlink (from_page INTEGER NOT NULL, prefix TEXT NOT NULL, title TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS iwlink_from ON iwlink(from_page);
CREATE INDEX IF NOT EXISTS iwlink_to ON iwlink(prefix, title);

CREATE TABLE IF NOT EXISTS langlink (from_page INTEGER NOT NULL, lang TEXT NOT NULL, title TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS langlink_from ON langlink(from_page);
CREATE INDEX IF NOT EXISTS langlink_to ON langlink(lang, title);

CREATE TABLE IF NOT EXISTS watchlist (
    user  INTEGER NOT NULL,
    ns    INTEGER NOT NULL,
    title TEXT NOT NULL,
    added INTEGER NOT NULL,
    PRIMARY KEY(user, ns, title)
);
CREATE INDEX IF NOT EXISTS watchlist_page ON watchlist(ns, title);

CREATE TABLE IF NOT EXISTS pageprop (page INTEGER NOT NULL, name TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY(page, name));
CREATE INDEX IF NOT EXISTS pageprop_name ON pageprop(name, page);

-- Replicated settings (db_id, anon_salt).
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
-- Per-instance state that must never be replicated (replica position, pending maintenance).
CREATE TABLE IF NOT EXISTS local_state (key TEXT PRIMARY KEY, value TEXT NOT NULL);

-- Replication index: one row per changed (table, key) holding the sequence number of its
-- latest change. Replicas ask for everything with seq > N and receive the current rows.
CREATE TABLE IF NOT EXISTS repl_log (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    tbl TEXT NOT NULL,
    k   NOT NULL,
    UNIQUE(tbl, k)
);

CREATE TABLE IF NOT EXISTS file (
    id      INTEGER PRIMARY KEY,
    name    TEXT NOT NULL UNIQUE,
    latest  INTEGER NOT NULL DEFAULT 0,
    deleted INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS file_version (
    id        INTEGER PRIMARY KEY,
    file      INTEGER NOT NULL REFERENCES file(id),
    ts        INTEGER NOT NULL,
    user_id   INTEGER,
    user_name TEXT NOT NULL,
    comment   TEXT NOT NULL DEFAULT '',
    mime      TEXT NOT NULL,
    size      INTEGER NOT NULL,
    width     INTEGER NOT NULL DEFAULT 0,
    height    INTEGER NOT NULL DEFAULT 0,
    sha256    TEXT NOT NULL,
    data      BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS file_version_file ON file_version(file, id);
CREATE INDEX IF NOT EXISTS file_version_sha ON file_version(sha256);

CREATE VIRTUAL TABLE IF NOT EXISTS search USING fts5(
    title, body,
    tokenize = 'unicode61 remove_diacritics 2',
    prefix = '2 3'
);
"#;

/// Tables copied to replicas, with the column that groups the rows of one logical object.
pub const REPLICATED: &[(&str, &str)] = &[
    ("meta", "key"),
    ("user", "id"),
    ("page", "id"),
    ("revision", "id"),
    ("pagelink", "from_page"),
    ("templatelink", "from_page"),
    ("categorylink", "from_page"),
    ("extlink", "from_page"),
    ("iwlink", "from_page"),
    ("langlink", "from_page"),
    ("watchlist", "user"),
    ("pageprop", "page"),
    ("log", "id"),
    ("interwiki", "prefix"),
    ("file", "id"),
    ("file_version", "id"),
];

/// Bumped when derived tables gain new content that old pages lack (forces a one-off reindex).
const INDEX_VERSION: i64 = 4;

pub fn get_state(c: &Connection, table: &str, key: &str) -> rusqlite::Result<Option<String>> {
    use rusqlite::OptionalExtension;
    c.query_row(&format!("SELECT value FROM {table} WHERE key=?1"), [key], |r| r.get(0)).optional()
}

pub fn set_state(c: &Connection, table: &str, key: &str, value: &str) -> rusqlite::Result<()> {
    c.execute(&format!("INSERT INTO {table}(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value"), [key, value])?;
    Ok(())
}

fn has_column(c: &Connection, table: &str, col: &str) -> rusqlite::Result<bool> {
    let mut st = c.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = st.query_map([], |r| r.get::<_, String>(1))?.collect::<Result<Vec<_>, _>>()?;
    Ok(names.iter().any(|n| n == col))
}

/// Idempotent upgrades of databases created by older versions.
fn migrate(c: &Connection, replica: bool) -> rusqlite::Result<()> {
    // Media metadata (filled in by ffprobe / pdfinfo after upload).
    for (col, decl) in [("duration", "REAL NOT NULL DEFAULT 0"), ("pages", "INTEGER NOT NULL DEFAULT 0"), ("probed", "INTEGER NOT NULL DEFAULT 0")] {
        if !has_column(c, "file_version", col)? {
            c.execute_batch(&format!("ALTER TABLE file_version ADD COLUMN {col} {decl}"))?;
        }
    }
    for (col, decl) in [("api", "TEXT NOT NULL DEFAULT ''"), ("trans", "INTEGER NOT NULL DEFAULT 0"), ("lang", "INTEGER NOT NULL DEFAULT 0")] {
        if !has_column(c, "interwiki", col)? {
            c.execute_batch(&format!("ALTER TABLE interwiki ADD COLUMN {col} {decl}"))?;
        }
    }
    if get_state(c, "local_state", "node_id")?.is_none() {
        use rand::RngCore;
        let mut b = [0u8; 8];
        rand::thread_rng().fill_bytes(&mut b);
        set_state(c, "local_state", "node_id", &crate::util::hex(&b))?;
    }
    if replica {
        // Derived tables arrive complete from the primary; nothing to rebuild if this
        // instance is later promoted.
        set_state(c, "local_state", "index_version", &INDEX_VERSION.to_string())?;
        return Ok(());
    }
    ensure_identity(c)?;
    let pages: i64 = c.query_row("SELECT count(*) FROM page", [], |r| r.get(0))?;
    let ver: i64 = get_state(c, "local_state", "index_version")?.and_then(|v| v.parse().ok()).unwrap_or(if pages == 0 { INDEX_VERSION } else { 1 });
    if ver < INDEX_VERSION {
        set_state(c, "local_state", "needs_reindex", "1")?;
    }
    set_state(c, "local_state", "index_version", &INDEX_VERSION.to_string())?;
    // First start with replication support: index every existing row so replicas receive it.
    // Index every existing row of a table the replication log has never seen (first start
    // with replication support, or a table added by an upgrade) so replicas receive it.
    for (t, k) in REPLICATED {
        let seen: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM repl_log WHERE tbl=?1)", [t], |r| r.get(0))?;
        if !seen {
            c.execute_batch(&format!("INSERT OR REPLACE INTO repl_log(tbl,k) SELECT DISTINCT '{t}', {k} FROM {t}"))?;
        }
    }
    Ok(())
}

/// Connection-local triggers that record every change to a replicated table in `repl_log`.
/// They live in the temp schema of the writer connection: the database file stays free of
/// triggers, and a replica (which applies changes verbatim) simply does not install them.
pub fn install_change_triggers(c: &Connection) -> rusqlite::Result<()> {
    let mut sql = String::new();
    for (t, k) in REPLICATED {
        // DELETE + INSERT (rather than INSERT OR REPLACE) so the bump also happens when the
        // triggering statement carries its own conflict clause, which would override ours.
        let bump = |who: &str| format!("DELETE FROM repl_log WHERE tbl='{t}' AND k IS {who}.{k}; INSERT INTO repl_log(tbl,k) VALUES('{t}', {who}.{k});");
        sql.push_str(&format!(
            "CREATE TEMP TRIGGER IF NOT EXISTS repl_{t}_i AFTER INSERT ON main.{t} BEGIN {n} END;
             CREATE TEMP TRIGGER IF NOT EXISTS repl_{t}_u AFTER UPDATE ON main.{t} BEGIN {o} {n} END;
             CREATE TEMP TRIGGER IF NOT EXISTS repl_{t}_d AFTER DELETE ON main.{t} BEGIN {o} END;\n",
            n = bump("NEW"),
            o = bump("OLD"),
        ));
    }
    c.execute_batch(&sql)
}

/// Stop recording changes (the node became a replica: it now mirrors the primary's log).
pub fn remove_change_triggers(c: &Connection) -> rusqlite::Result<()> {
    let mut sql = String::new();
    for (t, _) in REPLICATED {
        for op in ["i", "u", "d"] {
            sql.push_str(&format!("DROP TRIGGER IF EXISTS temp.repl_{t}_{op};"));
        }
    }
    c.execute_batch(&sql)
}

/// Identity every primary needs (also called when a replica is promoted).
pub fn ensure_identity(c: &Connection) -> rusqlite::Result<()> {
    let random = || {
        use rand::RngCore;
        let mut b = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut b);
        crate::util::hex(&b)
    };
    for key in ["db_id", "anon_salt"] {
        if get_state(c, "meta", key)?.is_none() {
            set_state(c, "meta", key, &random())?;
        }
    }
    Ok(())
}

pub struct Db {
    writer: Mutex<Connection>,
    readers: Vec<Mutex<Connection>>,
    next: AtomicUsize,
}

fn tune(c: &Connection) -> rusqlite::Result<()> {
    c.busy_timeout(std::time::Duration::from_secs(5))?;
    c.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=NORMAL; PRAGMA temp_store=MEMORY;")
}

impl Db {
    #[cfg(test)]
    pub fn open(path: &str, n_readers: usize) -> rusqlite::Result<Db> {
        Self::open_as(path, n_readers, false)
    }

    /// `replica = true`: no change triggers and no locally generated identity; the follower
    /// (see `repl.rs`) is the only writer.
    pub fn open_as(path: &str, n_readers: usize, replica: bool) -> rusqlite::Result<Db> {
        if let Some(dir) = std::path::Path::new(path).parent() {
            if !dir.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(dir);
            }
        }
        let w = Connection::open(path)?;
        w.pragma_update(None, "journal_mode", "WAL")?;
        tune(&w)?;
        w.execute_batch(SCHEMA)?;
        migrate(&w, replica)?;
        if !replica {
            install_change_triggers(&w)?;
        }
        let mut readers = Vec::new();
        for _ in 0..n_readers.max(1) {
            let r = Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            tune(&r)?;
            r.pragma_update(None, "query_only", true)?;
            readers.push(Mutex::new(r));
        }
        Ok(Db { writer: Mutex::new(w), readers, next: AtomicUsize::new(0) })
    }

    pub fn reader(&self) -> MutexGuard<'_, Connection> {
        let i = self.next.fetch_add(1, Ordering::Relaxed) % self.readers.len();
        self.readers[i].lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn writer(&self) -> MutexGuard<'_, Connection> {
        self.writer.lock().unwrap_or_else(|e| e.into_inner())
    }
}
