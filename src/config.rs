use std::{env, net::SocketAddr, str::FromStr, time::Duration};

#[derive(Clone, Debug)]
pub struct Config {
    pub addr: SocketAddr,
    pub db_path: String,
    pub site_name: String,
    /// Where rendered internal links point; `$1` is the encoded title.
    pub article_path: String,
    /// Allow unauthenticated reads (false = private wiki, every request needs a token).
    pub anon_read: bool,
    /// Allow unauthenticated edits.
    pub anon_edit: bool,
    /// Optional: bootstrap an `Admin` sysop with this token on startup.
    pub admin_token: Option<String>,
    pub max_page_bytes: usize,
    pub render_cache_entries: u64,
    pub read_connections: usize,
    /// 0 disables rate limiting.
    pub rate_limit_per_min: u32,
    pub trust_proxy: bool,
    pub request_timeout: Duration,
    /// Max pages re-indexed synchronously when a template changes.
    pub refresh_limit: usize,
    pub cache_max_age: u64,
    /// Where `[[File:x]]` embeds point; `$1` = encoded file name.
    pub file_path: String,
    pub upload_max_bytes: usize,
    /// Reject every write (replica / maintenance mode). Reads keep working.
    pub read_only: bool,
    pub backup_dir: String,
    /// Base URL of the primary (e.g. `https://wiki.example.org`). Makes this instance a
    /// read-only replica that continuously copies the primary's data.
    pub replica_of: Option<String>,
    /// A sysop token on the primary, used by the replica to fetch changes.
    pub replica_token: String,
    /// Base URLs of the other nodes of this cluster (enables automatic fail-over handling).
    pub peers: Vec<String>,
    /// A replica promotes itself after the primary has been unreachable this long (0 = never).
    pub failover_after_secs: u64,
    /// Replicas pass write requests on to the primary instead of refusing them.
    pub forward_writes: bool,
    /// A write waits until this many replicas have confirmed it (0 = asynchronous).
    pub repl_min_acks: usize,
    pub repl_ack_timeout_ms: u64,
    /// `ip` (MediaWiki style), `masked` (203.0.113.x) or `hashed` (stable pseudonym, no address stored).
    pub anon_identity: String,
    /// `*` or a comma-separated list of allowed browser origins.
    pub cors_origins: String,
    pub allow_svg: bool,
    /// Largest image (in megapixels) the server will decode to make a thumbnail.
    pub thumb_max_megapixels: u64,
    pub thumb_cache_bytes: u64,
    /// How long remote (interwiki) templates and failed fetches are cached.
    pub interwiki_cache_secs: u64,
    /// How often other wikis are asked for changes to transcluded pages (0 = never).
    pub interwiki_poll_secs: u64,
    /// Directory for the persistent thumbnail cache (empty = memory only).
    pub thumb_dir: String,
    pub thumb_disk_bytes: u64,
    /// IANA time zone for `{{#timel}}` / `{{LOCAL...}}` (default UTC).
    pub timezone: String,
    /// Content language code reported by the API and to modules.
    pub site_lang: String,
}

fn var<T: FromStr>(key: &str, default: T) -> T {
    // An empty value (e.g. `FOO=` from docker-compose) means "use the default".
    env::var(key).ok().filter(|v| !v.is_empty()).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn flag(key: &str, default: bool) -> bool {
    match env::var(key).ok().as_deref().map(str::to_ascii_lowercase).as_deref() {
        Some("1" | "true" | "yes" | "on") => true,
        Some("0" | "false" | "no" | "off") => false,
        _ => default,
    }
}

impl Config {
    pub fn from_env() -> Self {
        // Local only unless asked otherwise (the Docker image sets 0.0.0.0).
        let bind: String = var("BIND_ADDR", "127.0.0.1".to_string());
        let port: u16 = var("PORT", 3000);
        let addr: SocketAddr = format!("{bind}:{port}").parse().expect("invalid BIND_ADDR / PORT");
        // `{title}` is accepted as a docker-compose-friendly alias for `$1`.
        let mut article_path: String = var("ARTICLE_PATH", "/wiki/$1".to_string()).replace("{title}", "$1");
        if !article_path.contains("$1") {
            article_path.push_str("$1");
        }
        Self {
            addr,
            db_path: var("DATABASE_PATH", "./data/wiki.db".to_string()),
            site_name: var("SITE_NAME", "Hoepedia".to_string()),
            article_path,
            anon_read: flag("ANON_READ", true),
            anon_edit: flag("ANON_EDIT", false),
            admin_token: env::var("ADMIN_TOKEN").ok().filter(|t| !t.is_empty()),
            max_page_bytes: var("MAX_PAGE_BYTES", 2_000_000),
            render_cache_entries: var("RENDER_CACHE_ENTRIES", 5_000),
            read_connections: var("READ_CONNECTIONS", 4usize).clamp(1, 32),
            rate_limit_per_min: var("RATE_LIMIT_PER_MIN", 300),
            trust_proxy: flag("TRUST_PROXY", false),
            request_timeout: Duration::from_secs(var("REQUEST_TIMEOUT_SECS", 25)),
            refresh_limit: var("REFRESH_LINKS_LIMIT", 500),
            cache_max_age: var("CACHE_MAX_AGE_SECS", 0),
            file_path: var("FILE_PATH", "/rest/v1/file/$1".to_string()).replace("{name}", "$1"),
            upload_max_bytes: var("UPLOAD_MAX_BYTES", 10_000_000),
            read_only: flag("READ_ONLY", false),
            backup_dir: var("BACKUP_DIR", "./data/backups".to_string()),
            replica_of: env::var("REPLICA_OF").ok().map(|u| u.trim().trim_end_matches('/').to_string()).filter(|u| !u.is_empty()),
            replica_token: var("REPLICA_TOKEN", String::new()),
            peers: var("PEERS", String::new()).split(',').map(|p| p.trim().trim_end_matches('/').to_string()).filter(|p| p.starts_with("http://") || p.starts_with("https://")).collect(),
            failover_after_secs: var("FAILOVER_AFTER_SECS", 0u64),
            forward_writes: flag("REPLICA_FORWARD_WRITES", true),
            repl_min_acks: var("REPL_MIN_ACKS", 0usize).min(16),
            repl_ack_timeout_ms: var("REPL_ACK_TIMEOUT_MS", 2000u64).clamp(50, 20_000),
            anon_identity: match var("ANON_IDENTITY", "ip".to_string()).to_ascii_lowercase().as_str() {
                "masked" => "masked".into(),
                "hashed" => "hashed".into(),
                _ => "ip".into(),
            },
            cors_origins: var("CORS_ORIGINS", "*".to_string()),
            allow_svg: flag("ALLOW_SVG", true),
            thumb_max_megapixels: var("THUMB_MAX_MEGAPIXELS", 25u64).clamp(1, 400),
            thumb_cache_bytes: var("THUMB_CACHE_BYTES", 64_000_000),
            interwiki_cache_secs: var("INTERWIKI_CACHE_SECS", 3600u64).max(10),
            interwiki_poll_secs: var("INTERWIKI_POLL_SECS", 60u64),
            thumb_dir: match env::var("THUMB_DIR") {
                Ok(d) if d.trim().eq_ignore_ascii_case("off") => String::new(),
                Ok(d) if !d.trim().is_empty() => d.trim().to_string(),
                // default: next to the database
                _ => std::path::Path::new(&var("DATABASE_PATH", "./data/wiki.db".to_string())).parent().map(|p| p.join("thumbs").to_string_lossy().into_owned()).unwrap_or_default(),
            },
            thumb_disk_bytes: var("THUMB_DISK_BYTES", 512_000_000u64),
            timezone: {
                let tz: String = var("TIMEZONE", "UTC".to_string());
                if crate::timefmt::valid_zone(&tz) {
                    tz
                } else {
                    eprintln!("TIMEZONE '{tz}' is not a known IANA zone; using UTC");
                    "UTC".to_string()
                }
            },
            site_lang: {
                let l: String = var("SITE_LANG", "en".to_string());
                if !l.is_empty() && l.len() <= 16 && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') { l.to_ascii_lowercase() } else { "en".to_string() }
            },
        }
    }

    #[cfg(test)]
    pub fn for_tests(db_path: &str) -> Self {
        let mut c = Self::from_env();
        c.db_path = db_path.to_string();
        c.backup_dir = std::path::Path::new(db_path).parent().map(|p| p.join("backups").to_string_lossy().into_owned()).unwrap_or_default();
        c.thumb_dir = std::path::Path::new(db_path).parent().map(|p| p.join("thumbs").to_string_lossy().into_owned()).unwrap_or_default();
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_env_values_fall_back_to_defaults() {
        env::set_var("HP_TEST_STR", "");
        env::set_var("HP_TEST_NUM", "");
        assert_eq!(var("HP_TEST_STR", "dflt".to_string()), "dflt");
        assert_eq!(var("HP_TEST_NUM", 7u32), 7);
        env::set_var("HP_TEST_STR", "x");
        assert_eq!(var("HP_TEST_STR", "dflt".to_string()), "x");
    }
}
