//! Talking to other wikis: interwiki transclusion (`{{prefix:Page}}`) and federated search.
//!
//! Remote wikis are reached through the MediaWiki Action API protocol, which both other
//! hoepedia instances and MediaWiki sites speak. Only prefixes an administrator configured
//! with an `api` URL are ever contacted. Requests are short-lived, never follow redirects,
//! are size-capped and cached; whatever comes back is treated as untrusted wikitext.

use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    io::Read,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use moka::sync::Cache;
use serde_json::Value;

const MAX_BODY: u64 = 2 * 1024 * 1024;
/// Background fetches running at once.
const MAX_INFLIGHT: usize = 8;

thread_local! {
    static OFFLINE: Cell<bool> = const { Cell::new(false) };
}

/// Run `f` without network access to other wikis: cached text is used, anything missing or
/// stale is fetched in the background. Used while the database writer is held, so a slow
/// remote can never hold up other writes.
pub fn offline<T>(f: impl FnOnce() -> T) -> T {
    let before = OFFLINE.with(|o| o.replace(true));
    let r = f();
    OFFLINE.with(|o| o.set(before));
    r
}

pub fn is_offline() -> bool {
    OFFLINE.with(|o| o.get())
}

struct Entry {
    /// `None` = the page does not exist there (or could not be fetched yet).
    text: Option<Arc<String>>,
    fetched: Instant,
}

type Key = (String, String);
type OnChange = Box<dyn Fn(&str, &str) + Send + Sync>;

pub struct Remote {
    /// For a render that is waiting: gives up quickly.
    agent: ureq::Agent,
    /// For background refreshes and polling: nobody waits, so slow wikis get time.
    patient: ureq::Agent,
    entries: Cache<Key, Arc<Entry>>,
    ttl: Duration,
    inflight: Mutex<HashSet<Key>>,
    on_change: OnceLock<OnChange>,
}

fn enc(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

/// Titles compare like MediaWiki's: underscores are spaces, the first letter is case-insensitive.
pub fn same_title(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        let s = s.replace('_', " ");
        let s = s.trim();
        let mut cs = s.chars();
        cs.next().map(|f| f.to_uppercase().collect::<String>() + cs.as_str()).unwrap_or_default()
    };
    norm(a) == norm(b)
}

impl Remote {
    pub fn new(ttl_secs: u64) -> Remote {
        let build = |connect: u64, total: u64| {
            ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(connect))
                .timeout(Duration::from_secs(total))
                .redirects(0)
                .user_agent(concat!("hoepedia-api/", env!("CARGO_PKG_VERSION"), " (interwiki)"))
                .build()
        };
        Remote {
            agent: build(2, 3),
            patient: build(5, 20),
            // Entries are kept past their freshness (served stale while refreshing) and only
            // dropped when unused for a week or when space runs out.
            entries: Cache::builder()
                .max_capacity(32 * 1024 * 1024)
                .weigher(|_k, v: &Arc<Entry>| (v.text.as_ref().map_or(0, |t| t.len()) as u32).saturating_add(128))
                .time_to_idle(Duration::from_secs(7 * 86_400))
                .build(),
            ttl: Duration::from_secs(ttl_secs),
            inflight: Mutex::new(HashSet::new()),
            on_change: OnceLock::new(),
        }
    }

    /// Called (from a background thread) when a remote page's text changed: `(api, title)`.
    pub fn set_on_change(&self, f: OnChange) {
        let _ = self.on_change.set(f);
    }

    fn get_json(&self, url: &str) -> Result<Value, String> {
        self.get_json_with(&self.agent, url)
    }

    fn get_json_with(&self, agent: &ureq::Agent, url: &str) -> Result<Value, String> {
        let resp = agent.get(url).set("Accept", "application/json").call().map_err(|e| match e {
            ureq::Error::Status(code, _) => format!("HTTP {code}"),
            other => other.to_string(),
        })?;
        let mut body = Vec::new();
        resp.into_reader().take(MAX_BODY + 1).read_to_end(&mut body).map_err(|e| e.to_string())?;
        if body.len() as u64 > MAX_BODY {
            return Err("response too large".into());
        }
        serde_json::from_slice(&body).map_err(|e| format!("invalid JSON: {e}"))
    }

    /// Current wikitext of `title` on the wiki whose Action API lives at `api`.
    /// Fetch the current text: `Ok(None)` = no such page there, `Err` = the wiki could not be asked.
    fn fetch(&self, api: &str, title: &str, patient: bool) -> Result<Option<String>, String> {
        let sep = if api.contains('?') { '&' } else { '?' };
        let url = format!("{api}{sep}action=query&format=json&formatversion=2&redirects=1&prop=revisions&rvprop=content&rvslots=main&titles={}", enc(title));
        let v = self.get_json_with(if patient { &self.patient } else { &self.agent }, &url)?;
        if v.get("query").is_none() {
            return Err(v["error"]["info"].as_str().unwrap_or("unexpected answer").chars().take(200).collect());
        }
        let rev = &v["query"]["pages"][0]["revisions"][0];
        // formatversion=2 with slots, or the older flat shape
        Ok(rev["slots"]["main"]["content"].as_str().or_else(|| rev["content"].as_str()).map(str::to_string))
    }

    fn store(&self, key: Key, text: Option<String>, fetched: Instant) -> bool {
        let new = text.map(Arc::new);
        let changed = self.entries.get(&key).is_none_or(|old| old.text.as_deref() != new.as_deref());
        self.entries.insert(key, Arc::new(Entry { text: new, fetched }));
        changed
    }

    /// Text of `title` on the wiki whose Action API is `api`.
    ///
    /// * cached and fresh: returned;
    /// * cached but stale: returned, and refreshed in the background;
    /// * not cached: fetched now, unless `offline` is in force or `may_wait` is false, in
    ///   which case it is fetched in the background and `None` is returned for now.
    pub fn wikitext(self: &Arc<Self>, api: &str, title: &str, may_wait: bool) -> Option<Arc<String>> {
        let key = (api.to_string(), title.to_string());
        if let Some(e) = self.entries.get(&key) {
            if e.fetched.elapsed() > self.ttl {
                self.refresh_in_background(key);
            }
            return e.text.clone();
        }
        if is_offline() || !may_wait {
            self.refresh_in_background(key);
            return None;
        }
        match self.fetch(api, title, false) {
            Ok(t) => {
                self.store(key, t, Instant::now());
            }
            Err(e) => {
                // Too slow or unreachable for a waiting reader: keep trying in the background.
                tracing::info!(api, title, error = %e, "interwiki fetch did not finish in time; continuing in the background");
                self.refresh_in_background(key);
                return None;
            }
        }
        self.entries.get(&(api.to_string(), title.to_string())).and_then(|e| e.text.clone())
    }

    fn refresh_in_background(self: &Arc<Self>, key: Key) {
        {
            let mut inflight = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
            if inflight.len() >= MAX_INFLIGHT || !inflight.insert(key.clone()) {
                return;
            }
        }
        let me = Arc::clone(self);
        let spawned = std::thread::Builder::new().name("interwiki".into()).spawn(move || {
            me.refresh_now(&key);
            me.inflight.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
        });
        if spawned.is_err() {
            self.inflight.lock().unwrap_or_else(|e| e.into_inner()).clear();
        }
    }

    /// Fetch again; tell the wiki if the text changed. Failures keep the old text.
    fn refresh_now(&self, key: &Key) {
        match self.fetch(&key.0, &key.1, true) {
            Ok(t) => {
                if self.store(key.clone(), t, Instant::now()) {
                    if let Some(f) = self.on_change.get() {
                        f(&key.0, &key.1);
                    }
                }
            }
            Err(e) => {
                tracing::warn!(api = %key.0, title = %key.1, error = %e, "interwiki refresh failed; keeping the cached copy");
                // Keep what we had (or remember "not available") and try again in a minute.
                let old = self.entries.get(key).and_then(|e| e.text.clone());
                let retry = Instant::now().checked_sub(self.ttl.saturating_sub(Duration::from_secs(60))).unwrap_or_else(Instant::now);
                self.entries.insert(key.clone(), Arc::new(Entry { text: old, fetched: retry }));
            }
        }
    }

    /// Ask every remote wiki in the cache which of its pages changed since the last poll and
    /// refresh the cached ones. `last` remembers each wiki's position between calls.
    pub fn poll_changes(&self, last: &mut HashMap<String, i64>) {
        let mut by_api: HashMap<String, Vec<String>> = HashMap::new();
        for (k, _) in self.entries.iter() {
            by_api.entry(k.0.clone()).or_default().push(k.1.clone());
        }
        let now = crate::util::now();
        for (api, titles) in by_api {
            let Some(since) = last.get(&api).copied() else {
                last.insert(api, now);
                continue;
            };
            let sep = if api.contains('?') { '&' } else { '?' };
            let url = format!(
                "{api}{sep}action=query&format=json&formatversion=2&list=recentchanges&rcprop=title|timestamp&rclimit=500&rcdir=newer&rcstart={}",
                enc(&crate::util::iso(since - 5))
            );
            match self.get_json_with(&self.patient, &url) {
                Ok(v) => {
                    let list = v["query"]["recentchanges"].as_array().cloned().unwrap_or_default();
                    for ch in &list {
                        let Some(t) = ch["title"].as_str() else { continue };
                        for cached in titles.iter().filter(|c| same_title(c, t)) {
                            self.refresh_now(&(api.clone(), cached.clone()));
                        }
                    }
                    last.insert(api, now);
                }
                Err(e) => tracing::debug!(api, error = %e, "could not poll a remote wiki for changes"),
            }
        }
    }

    /// Full-text search on a remote wiki: (total hits, [(title, snippet)]).
    pub fn search(&self, api: &str, query: &str, limit: i64) -> Result<(i64, Vec<(String, String)>), String> {
        let sep = if api.contains('?') { '&' } else { '?' };
        let url = format!("{api}{sep}action=query&format=json&formatversion=2&list=search&srlimit={limit}&srsearch={}", enc(query));
        let v = self.get_json(&url)?;
        if let Some(e) = v["error"]["info"].as_str() {
            return Err(e.chars().take(200).collect());
        }
        let hits: Vec<(String, String)> = v["query"]["search"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|h| Some((h["title"].as_str()?.to_string(), h["snippet"].as_str().unwrap_or("").to_string())))
                    .take(limit as usize)
                    .collect()
            })
            .unwrap_or_default();
        let total = v["query"]["searchinfo"]["totalhits"].as_i64().unwrap_or(hits.len() as i64);
        Ok((total, hits))
    }

    pub fn purge(&self) {
        self.entries.invalidate_all();
    }
}
