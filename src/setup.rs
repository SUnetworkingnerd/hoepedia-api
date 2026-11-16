//! Everything that makes the server easy to start: `.env` loading, command-line options,
//! the automatically created admin token, the start-up banner and the built-in self-test
//! (`hoepedia-api check`). All of it works the same on Linux, macOS and Windows.

use std::{
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::Value;

pub const HELP: &str = "\
hoepedia-api - a wiki engine that is only an API

USAGE
  hoepedia-api [OPTIONS]          start the server
  hoepedia-api check [URL]        test a running server (default: the one on this computer)
  hoepedia-api token              show the admin token and where it is stored
  hoepedia-api --help | --version

OPTIONS
  --port N        port to listen on (default 3000; same as PORT)
  --bind ADDR     address to listen on (default 127.0.0.1 = this computer only;
                  use 0.0.0.0 to accept connections from other machines)
  --data DIR      folder for the database, backups and thumbnails (default ./data)

No configuration is needed: on the first start an admin token is created and saved in
<data>/admin-token.txt. Settings can be given as environment variables or in a file named
.env in the folder you start the server from (see .env.example for all of them).
";

/// Read `KEY=VALUE` lines from `.env` in the working directory. Real environment variables
/// win. Lines starting with `#` and blank lines are ignored; quotes around values are removed.
pub fn load_dotenv() -> Option<PathBuf> {
    let path = Path::new(".env");
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else { continue };
        let (k, mut v) = (k.trim(), v.trim());
        if k.is_empty() || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        if v.len() >= 2 && ((v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\''))) {
            v = &v[1..v.len() - 1];
        }
        if std::env::var_os(k).is_none() {
            std::env::set_var(k, v);
        }
    }
    Some(path.to_path_buf())
}

pub enum Command {
    Serve,
    Check(Option<String>),
    Token,
}

/// Apply command-line options (they become the matching environment variables).
/// Prints help/version and exits for those flags; exits with an error for unknown ones.
pub fn parse_args(args: &[String]) -> Command {
    let mut cmd = Command::Serve;
    let mut i = 0;
    let need = |i: usize, name: &str| -> String {
        args.get(i + 1).cloned().unwrap_or_else(|| {
            eprintln!("{name} needs a value\n\n{HELP}");
            std::process::exit(2)
        })
    };
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" | "help" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            "-V" | "--version" | "version" => {
                println!("hoepedia-api {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--port" | "-p" => {
                std::env::set_var("PORT", need(i, "--port"));
                i += 1;
            }
            "--bind" => {
                std::env::set_var("BIND_ADDR", need(i, "--bind"));
                i += 1;
            }
            "--data" => {
                let dir = PathBuf::from(need(i, "--data"));
                std::env::set_var("DATABASE_PATH", dir.join("wiki.db"));
                if std::env::var_os("BACKUP_DIR").is_none() {
                    std::env::set_var("BACKUP_DIR", dir.join("backups"));
                }
                i += 1;
            }
            "check" => {
                cmd = Command::Check(args.get(i + 1).filter(|a| !a.starts_with('-')).cloned());
                if matches!(&cmd, Command::Check(Some(_))) {
                    i += 1;
                }
            }
            "token" => cmd = Command::Token,
            "serve" | "start" | "run" => {}
            other => {
                eprintln!("unknown option '{other}'\n\n{HELP}");
                std::process::exit(2);
            }
        }
        i += 1;
    }
    cmd
}

pub fn data_dir() -> PathBuf {
    let db = std::env::var("DATABASE_PATH").ok().filter(|p| !p.trim().is_empty()).unwrap_or_else(|| "./data/wiki.db".to_string());
    Path::new(&db).parent().map(Path::to_path_buf).filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| PathBuf::from("."))
}

pub fn token_file() -> PathBuf {
    data_dir().join("admin-token.txt")
}

fn read_token_file() -> Option<String> {
    read_token_at(&token_file())
}

fn read_token_at(path: &Path) -> Option<String> {
    let t = std::fs::read_to_string(path).ok()?;
    let t = t.trim().to_string();
    (t.len() >= 16).then_some(t)
}

/// Where the admin token comes from, for the banner.
pub enum TokenSource {
    Env,
    File,
    /// Just created: shown once.
    Created(String),
    /// A replica gets its users (and so its admin) from the primary.
    None,
    /// A wiki created by an older version: its admin keeps the token shown back then.
    Existing,
}

/// Make sure an admin token exists without the operator having to invent one: if
/// `ADMIN_TOKEN` is not set, use the one saved in the data folder, creating it on first start.
pub fn ensure_admin_token() -> TokenSource {
    // The placeholder of old example files must never become a real password.
    if std::env::var("ADMIN_TOKEN").is_ok_and(|t| t.trim().starts_with("change-me")) {
        eprintln!("ADMIN_TOKEN still has its placeholder value; ignoring it.");
        std::env::remove_var("ADMIN_TOKEN");
    }
    let explicit = std::env::var("ADMIN_TOKEN").is_ok_and(|t| !t.trim().is_empty());
    let replica = std::env::var("REPLICA_OF").is_ok_and(|u| !u.trim().is_empty());
    let db = std::env::var("DATABASE_PATH").ok().filter(|p| !p.trim().is_empty()).unwrap_or_else(|| "./data/wiki.db".to_string());
    let existing = std::fs::metadata(&db).is_ok_and(|m| m.len() > 0);
    let (source, token) = admin_token_at(&token_file(), explicit, replica, existing);
    if let Some(t) = token {
        std::env::set_var("ADMIN_TOKEN", t);
    }
    source
}

/// The decision behind `ensure_admin_token`: (where the token comes from, a token to adopt).
fn admin_token_at(path: &Path, explicit: bool, replica: bool, existing_db: bool) -> (TokenSource, Option<String>) {
    if explicit {
        return (TokenSource::Env, None);
    }
    if let Some(t) = read_token_at(path) {
        return (TokenSource::File, Some(t));
    }
    if replica {
        return (TokenSource::None, None);
    }
    if existing_db {
        // Never replace the admin token of a wiki that already has one.
        return (TokenSource::Existing, None);
    }
    let token = crate::util::new_token();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match std::fs::write(path, format!("{token}\n")) {
        Ok(()) => {
            // Only the account running the server may read it.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
            }
        }
        Err(e) => eprintln!("warning: could not save the admin token to {}: {e}", path.display()),
    }
    (TokenSource::Created(token.clone()), Some(token))
}

fn local_url(cfg: &crate::config::Config) -> String {
    let host = if cfg.addr.ip().is_unspecified() { "localhost".to_string() } else { cfg.addr.ip().to_string() };
    format!("http://{host}:{}", cfg.addr.port())
}

/// A short, plain summary printed once the server is listening.
pub fn banner(cfg: &crate::config::Config, source: &TokenSource, role: &str, previews: &str) {
    let url = local_url(cfg);
    let mut lines = vec![
        String::new(),
        format!("  Hoepedia API {} is running ({role})", env!("CARGO_PKG_VERSION")),
        String::new(),
        format!("  API         {url}"),
        format!("  Endpoints   {url}/           (full description: {url}/openapi.json)"),
        format!("  Data        {}", cfg.db_path),
        format!("  Previews    {previews}"),
    ];
    match source {
        TokenSource::Created(t) => {
            lines.push(format!("  Admin token {t}"));
            lines.push(format!("              (new; also saved in {})", token_file().display()));
        }
        TokenSource::File => lines.push(format!("  Admin token saved in {}", token_file().display())),
        TokenSource::Env => lines.push("  Admin token from ADMIN_TOKEN".to_string()),
        TokenSource::None => lines.push("  Admin token the primary's tokens work here".to_string()),
        TokenSource::Existing => lines.push("  Admin token the one shown when this wiki was created (to replace it, set ADMIN_TOKEN)".to_string()),
    }
    lines.push(String::new());
    lines.push("  Try it:".to_string());
    lines.push(format!("    curl {url}/rest/v1/page/Main_Page/html"));
    lines.push("    hoepedia-api check        (tests this server)".to_string());
    lines.push(String::new());
    lines.push("  Stop with Ctrl+C.".to_string());
    lines.push(String::new());
    println!("{}", lines.join("\n"));
}

pub const WELCOME_PAGE: &str = "\
'''Welcome to your wiki.''' This page was created automatically; edit or replace it.

== First steps ==
* Read a page: <code>GET /rest/v1/page/Main_Page</code> (source) or <code>/rest/v1/page/Main_Page/html</code>
* Create or change a page: <code>PUT /rest/v1/page/My_page</code> with <code>{\"source\": \"text\", \"comment\": \"why\"}</code> and the header <code>Authorization: Bearer YOUR_TOKEN</code>
* Search: <code>GET /rest/v1/search/page?q=words</code>
* Upload a file: <code>PUT /rest/v1/file/Picture.png</code> with the file as the request body
* Everything else: <code>GET /openapi.json</code>

[[Category:Help]]
";

pub fn token_command() -> i32 {
    match std::env::var("ADMIN_TOKEN").ok().filter(|t| !t.trim().is_empty()) {
        Some(t) => {
            println!("{t}");
            eprintln!("(from the ADMIN_TOKEN setting)");
            0
        }
        None => match read_token_file() {
            Some(t) => {
                println!("{t}");
                eprintln!("(saved in {})", token_file().display());
                0
            }
            None => {
                eprintln!("No admin token yet: it is created the first time the server starts (looked in {}).", token_file().display());
                1
            }
        },
    }
}

// ---------------------------------------------------------------------------------------
// hoepedia-api check
// ---------------------------------------------------------------------------------------

struct Client {
    agent: ureq::Agent,
    base: String,
    token: String,
}

impl Client {
    /// (status, body)
    fn send(&self, method: &str, path: &str, body: Option<(&str, &[u8])>) -> Result<(u16, Vec<u8>), String> {
        let mut r = self.agent.request(method, &format!("{}{path}", self.base));
        if !self.token.is_empty() {
            r = r.set("Authorization", &format!("Bearer {}", self.token));
        }
        let resp = match body {
            Some((ct, bytes)) => r.set("Content-Type", ct).send_bytes(bytes),
            None => r.call(),
        };
        let resp = match resp {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r,
            Err(e) => return Err(e.to_string()),
        };
        let status = resp.status();
        let mut out = Vec::new();
        resp.into_reader().take(64 << 20).read_to_end(&mut out).map_err(|e| e.to_string())?;
        Ok((status, out))
    }
    fn json(&self, method: &str, path: &str, body: Option<Value>) -> Result<(u16, Value), String> {
        let bytes = body.map(|b| b.to_string().into_bytes());
        let (s, out) = self.send(method, path, bytes.as_deref().map(|b| ("application/json", b)))?;
        Ok((s, serde_json::from_slice(&out).unwrap_or(Value::Null)))
    }
    fn text(&self, path: &str) -> Result<(u16, String), String> {
        let (s, out) = self.send("GET", path, None)?;
        Ok((s, String::from_utf8_lossy(&out).into_owned()))
    }
}

fn tiny_png() -> Vec<u8> {
    let img = image::RgbImage::from_fn(240, 120, |x, y| image::Rgb([(x % 256) as u8, (y * 2 % 256) as u8, 150]));
    let mut out = std::io::Cursor::new(Vec::new());
    let _ = image::DynamicImage::ImageRgb8(img).write_to(&mut out, image::ImageFormat::Png);
    out.into_inner()
}

/// A quick end-to-end test of a running server. Creates a few `Selfcheck ...` pages and a
/// file, verifies the main features and removes them again. Returns the process exit code.
pub fn check(url: Option<String>) -> i32 {
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(3000);
    let base = url.unwrap_or_else(|| format!("http://127.0.0.1:{port}")).trim_end_matches('/').to_string();
    let token = std::env::var("ADMIN_TOKEN").ok().filter(|t| !t.trim().is_empty()).or_else(read_token_file).unwrap_or_default();
    let c = Client { agent: ureq::AgentBuilder::new().timeout(Duration::from_secs(20)).build(), base: base.clone(), token };
    println!("Checking {base}");
    match c.text("/healthz") {
        Ok((200, _)) => {}
        Ok((s, _)) => {
            println!("  The server answered with status {s} on /healthz. Is this a Hoepedia server?");
            return 2;
        }
        Err(e) => {
            println!("  Cannot reach the server: {e}\n  Start it first (run hoepedia-api in another window), or give its address: hoepedia-api check http://host:port");
            return 2;
        }
    }
    if c.token.is_empty() {
        println!("  No admin token found. Set ADMIN_TOKEN, or run this from the folder the server was started in.");
        return 2;
    }
    let who = c.json("GET", "/api.php?action=query&meta=userinfo", None).map(|(_, v)| v).unwrap_or(Value::Null);
    if !who.to_string().contains("\"siteadmin\"") {
        println!("  The admin token was not accepted by this server (is it the token of another wiki?).");
        return 2;
    }
    let (mut ok, mut failed) = (0, 0);
    let mut step = |name: &str, pass: bool, detail: String| {
        if pass {
            ok += 1;
            println!("  ok    {name}");
        } else {
            failed += 1;
            println!("  FAIL  {name}  ->  {}", detail.chars().take(200).collect::<String>());
        }
    };
    let show = |r: &Result<(u16, Value), String>| match r {
        Ok((s, v)) => format!("{s} {v}"),
        Err(e) => e.clone(),
    };
    let has = |r: &Result<(u16, Value), String>, needle: &str| r.as_ref().is_ok_and(|(s, v)| *s < 300 && v.to_string().contains(needle));

    let r = c.json("PUT", "/rest/v1/page/Selfcheck_page", Some(serde_json::json!({ "source": "Selfcheck page about '''quetzals'''. {{Selfcheck hi|there}} [[Category:Selfcheck]]", "comment": "selfcheck" })));
    step("create a page", has(&r, "\"id\""), show(&r));
    let r = c.json("PUT", "/rest/v1/page/Template:Selfcheck_hi", Some(serde_json::json!({ "source": "Hello {{{1}}}! {{#expr: 6 * 7}}", "comment": "selfcheck" })));
    step("create a template", has(&r, "\"id\""), show(&r));
    let r = c.text("/rest/v1/page/Selfcheck_page/html");
    step("render it (bold text, template, parser function)", r.as_ref().is_ok_and(|(s, b)| *s == 200 && b.contains("<b>quetzals</b>") && b.contains("Hello there! 42")), format!("{r:?}"));
    let r = c.json("GET", "/rest/v1/search/page?q=quetzals", None);
    step("full-text search finds it", has(&r, "Selfcheck_page"), show(&r));
    let r = c.json("PUT", "/rest/v1/page/Selfcheck_page", Some(serde_json::json!({ "source": "Selfcheck page, second version. [[Category:Selfcheck]]", "comment": "selfcheck 2" })));
    step("edit it", has(&r, "\"id\""), show(&r));
    let r = c.json("GET", "/rest/v1/page/Selfcheck_page/history", None);
    step("history has both versions", r.as_ref().is_ok_and(|(_, v)| v["revisions"].as_array().is_some_and(|a| a.len() == 2)), show(&r));
    let r = c.json("GET", "/api.php?action=query&list=categorymembers&cmtitle=Category:Selfcheck", None);
    step("Action API lists the category", has(&r, "Selfcheck page"), show(&r));
    let r = c.json("PUT", "/rest/v1/page/Module:Selfcheck", Some(serde_json::json!({ "source": "local p = {} function p.f(frame) return 'Lua says ' .. mw.ustring.upper(frame.args[1]) end return p", "comment": "selfcheck" })));
    step("create a Lua module", has(&r, "\"id\""), show(&r));
    let r = c.json("POST", "/rest/v1/transform/wikitext/to/html", Some(serde_json::json!({ "wikitext": "{{#invoke:Selfcheck|f|hi}}" })));
    step("run the Lua module", has(&r, "Lua says HI"), show(&r));
    let png = tiny_png();
    let r = c.send("PUT", "/rest/v1/file/Selfcheck.png", Some(("image/png", &png))).map(|(s, b)| (s, serde_json::from_slice(&b).unwrap_or(Value::Null)));
    step("upload an image", has(&r, "image/png"), show(&r));
    let r = c.send("GET", "/rest/v1/file/Selfcheck.png?width=100", None);
    step("make a thumbnail", r.as_ref().is_ok_and(|(s, b)| *s == 200 && b.starts_with(b"\x89PNG") && b.len() < png.len()), r.as_ref().map(|(s, b)| format!("{s}, {} bytes", b.len())).unwrap_or_else(|e| e.clone()));
    let r = c.json("POST", "/rest/v1/admin/backups", Some(serde_json::json!({})));
    let backup = r.as_ref().ok().and_then(|(_, v)| v["name"].as_str().map(String::from));
    step("make a backup", backup.is_some(), show(&r));
    if let Some(b) = backup {
        let _ = c.send("DELETE", &format!("/rest/v1/admin/backups/{b}"), None);
    }
    let r = c.json("GET", "/rest/v1/replication/status", None);
    step("replication status", has(&r, "\"role\""), show(&r));
    // clean up
    for p in ["Selfcheck_page", "Template:Selfcheck_hi", "Module:Selfcheck", "File:Selfcheck.png"] {
        let _ = c.send("DELETE", &format!("/rest/v1/page/{p}"), None);
    }
    let _ = c.send("DELETE", "/rest/v1/file/Selfcheck.png", None);
    let r = c.text("/rest/v1/page/Selfcheck_page");
    step("clean up", r.as_ref().is_ok_and(|(s, _)| *s == 404), format!("{r:?}"));

    println!();
    if failed == 0 {
        println!("All {ok} checks passed. The server works.");
        0
    } else {
        println!("{failed} of {} checks failed.", ok + failed);
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_token_is_created_once_and_reused() {
        let dir = std::env::temp_dir().join(format!("hoepedia-setup-{}", crate::util::new_token()));
        let path = dir.join("data").join("admin-token.txt");
        // a replica without a saved token gets none (its users come from the primary)
        assert!(matches!(admin_token_at(&path, false, true, false), (TokenSource::None, None)));
        // first start: created and saved
        assert!(matches!(admin_token_at(&path, false, false, true), (TokenSource::Existing, None)), "an existing wiki keeps its token");
        assert!(!path.exists());
        let (src, tok) = admin_token_at(&path, false, false, false);
        let created = tok.unwrap();
        assert!(matches!(src, TokenSource::Created(ref t) if *t == created) && created.len() >= 32);
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), created);
        // later starts (also as a replica) reuse it; an explicit ADMIN_TOKEN wins
        assert!(matches!(admin_token_at(&path, false, false, true), (TokenSource::File, Some(ref t)) if *t == created));
        assert!(matches!(admin_token_at(&path, false, true, true), (TokenSource::File, Some(_))));
        assert!(matches!(admin_token_at(&path, true, false, false), (TokenSource::Env, None)));
        // a damaged file is replaced
        std::fs::write(&path, "short").unwrap();
        assert!(matches!(admin_token_at(&path, false, false, false), (TokenSource::Created(_), Some(_))));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn arguments() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(matches!(parse_args(&a(&[])), Command::Serve));
        assert!(matches!(parse_args(&a(&["check"])), Command::Check(None)));
        assert!(matches!(parse_args(&a(&["check", "http://x:1"])), Command::Check(Some(ref u)) if u == "http://x:1"));
        assert!(matches!(parse_args(&a(&["token"])), Command::Token));
    }
}
