//! Users, groups, API tokens and the permission model.
//!
//! Authentication is by bearer token (`Authorization: Bearer hp_...`), so there are no
//! cookies, no sessions and no CSRF surface. Tokens are stored as SHA-256 hashes.

use std::net::IpAddr;

use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::{
    config::Config,
    error::{ApiError, Res},
    util::{collapse_ws, new_token, now, sha256_hex},
};

/// Groups that can be granted explicitly. Every registered user is implicitly in `user`.
pub const ASSIGNABLE_GROUPS: &[&str] = &["sysop"];

const USER_RIGHTS: &[&str] = &["read", "edit", "createpage", "move", "upload"];
const SYSOP_RIGHTS: &[&str] = &[
    "editprotected", "delete", "undelete", "protect", "rollback", "block", "userrights", "createuser", "suppressredirect",
    "viewdeleted", "siteadmin", "deletefile",
];
const EDIT_RIGHTS: &[&str] = &["edit", "createpage", "move", "upload"];

#[derive(Clone, Debug)]
pub struct UserRow {
    pub id: i64,
    pub name: String,
    pub groups: Vec<String>,
    pub blocked: bool,
    pub block_reason: String,
    pub registered: i64,
    pub editcount: i64,
}

const UCOLS: &str = "id,name,groups,blocked,block_reason,registered,editcount";

fn user_row(r: &Row) -> rusqlite::Result<UserRow> {
    let groups: String = r.get(2)?;
    Ok(UserRow {
        id: r.get(0)?,
        name: r.get(1)?,
        groups: groups.split(',').filter(|g| !g.is_empty()).map(String::from).collect(),
        blocked: r.get::<_, i64>(3)? != 0,
        block_reason: r.get(4)?,
        registered: r.get(5)?,
        editcount: r.get(6)?,
    })
}

/// Who is making the request, with their effective rights resolved once.
#[derive(Clone, Debug)]
pub struct Actor {
    pub user_id: Option<i64>,
    /// Registered user name, or the client IP for anonymous requests.
    pub name: String,
    pub groups: Vec<String>,
    pub blocked: bool,
    pub block_reason: String,
    rights: Vec<&'static str>,
}

/// Per-wiki secret for `ANON_IDENTITY=hashed` (set once at startup from the database).
pub static ANON_SALT: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The name recorded for an anonymous editor.
pub fn anon_name(ip: IpAddr, mode: &str) -> String {
    match mode {
        "masked" => match ip {
            IpAddr::V4(v4) => {
                let o = v4.octets();
                format!("{}.{}.{}.x", o[0], o[1], o[2])
            }
            IpAddr::V6(v6) => {
                let s = v6.segments();
                format!("{:x}:{:x}:{:x}::x", s[0], s[1], s[2])
            }
        },
        "hashed" => {
            use sha2::{Digest, Sha256};
            let salt = ANON_SALT.get().map(String::as_str).unwrap_or("");
            let d = Sha256::digest(format!("{salt}|{ip}").as_bytes());
            format!("Anon-{}", crate::util::hex(&d[..5]))
        }
        _ => ip.to_string(),
    }
}

impl Actor {
    pub fn anon(ip: IpAddr, cfg: &Config) -> Actor {
        let mut rights = Vec::new();
        if cfg.anon_read {
            rights.push("read");
        }
        if cfg.anon_edit {
            rights.extend(["edit", "createpage"]);
        }
        Actor { user_id: None, name: anon_name(ip, &cfg.anon_identity), groups: vec!["*".into()], blocked: false, block_reason: String::new(), rights }
    }

    pub fn from_user(u: &UserRow) -> Actor {
        let mut rights: Vec<&'static str> = USER_RIGHTS.to_vec();
        if u.groups.iter().any(|g| g == "sysop") {
            rights.extend(SYSOP_RIGHTS);
        }
        if u.blocked {
            rights.retain(|r| !EDIT_RIGHTS.contains(r));
        }
        let mut groups = vec!["*".to_string(), "user".to_string()];
        groups.extend(u.groups.iter().cloned());
        Actor {
            user_id: Some(u.id),
            name: u.name.clone(),
            groups,
            blocked: u.blocked,
            block_reason: u.block_reason.clone(),
            rights,
        }
    }

    pub fn is_anon(&self) -> bool {
        self.user_id.is_none()
    }
    pub fn can(&self, right: &str) -> bool {
        self.rights.contains(&right)
    }
    pub fn rights(&self) -> &[&'static str] {
        &self.rights
    }

    pub fn require(&self, right: &str) -> Res<()> {
        if self.can(right) {
            return Ok(());
        }
        if self.blocked && EDIT_RIGHTS.contains(&right) {
            let why = if self.block_reason.is_empty() { "no reason given".to_string() } else { self.block_reason.clone() };
            return Err(ApiError::forbidden("blocked", format!("Your account is blocked ({why})")));
        }
        if self.is_anon() {
            return Err(ApiError::unauthorized(format!("The '{right}' right requires an API token (Authorization: Bearer ...)")));
        }
        Err(ApiError::forbidden("permissiondenied", format!("You need the '{right}' right to do this")))
    }

    /// Protection levels: "" (none), "user" (registered users), "sysop".
    pub fn meets_level(&self, level: &str) -> bool {
        match level {
            "" => true,
            "user" => !self.is_anon(),
            _ => self.can("editprotected"),
        }
    }
}

pub fn clean_username(raw: &str) -> Res<String> {
    let n = collapse_ws(&raw.replace('_', " "));
    if n.is_empty() || n.chars().count() > 60 {
        return Err(ApiError::bad("invalidusername", "user names must be 1-60 characters"));
    }
    if n.chars().any(|c| c.is_control() || "#<>[]|{}:/@\u{E000}\u{E001}".contains(c)) || n.parse::<IpAddr>().is_ok() {
        return Err(ApiError::bad("invalidusername", "user name contains illegal characters"));
    }
    let mut cs = n.chars();
    Ok(cs.next().map(|f| f.to_uppercase().collect::<String>() + cs.as_str()).unwrap_or_default())
}

fn check_groups(groups: &[String]) -> Res<()> {
    for g in groups {
        if !ASSIGNABLE_GROUPS.contains(&g.as_str()) {
            return Err(ApiError::bad("badgroup", format!("unknown group '{g}' (assignable: {})", ASSIGNABLE_GROUPS.join(", "))));
        }
    }
    Ok(())
}

pub fn user_by_token(c: &Connection, token: &str) -> Res<Option<UserRow>> {
    let h = sha256_hex(token.as_bytes());
    Ok(c.prepare_cached(&format!("SELECT {UCOLS} FROM user WHERE token_hash=?1"))?.query_row(params![h], user_row).optional()?)
}

pub fn user_by_name(c: &Connection, name: &str) -> Res<Option<UserRow>> {
    let n = clean_username(name)?;
    Ok(c.prepare_cached(&format!("SELECT {UCOLS} FROM user WHERE name=?1"))?.query_row(params![n], user_row).optional()?)
}

fn need_user(c: &Connection, name: &str) -> Res<UserRow> {
    user_by_name(c, name)?.ok_or_else(|| ApiError::not_found("nosuchuser", format!("user '{name}' does not exist")))
}

/// Returns the user and the *plaintext token (shown once)*.
pub fn create_user(c: &Connection, name: &str, groups: &[String]) -> Res<(UserRow, String)> {
    let name = clean_username(name)?;
    check_groups(groups)?;
    let token = new_token();
    let r = c.execute(
        "INSERT INTO user(name,token_hash,groups,registered) VALUES(?1,?2,?3,?4)",
        params![name, sha256_hex(token.as_bytes()), groups.join(","), now()],
    );
    match r {
        Ok(_) => Ok((need_user(c, &name)?, token)),
        Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::ConstraintViolation => {
            Err(ApiError::conflict("userexists", format!("user '{name}' already exists")))
        }
        Err(e) => Err(e.into()),
    }
}

pub fn set_groups(c: &Connection, name: &str, add: &[String], remove: &[String]) -> Res<UserRow> {
    check_groups(add)?;
    check_groups(remove)?;
    let u = need_user(c, name)?;
    let mut g = u.groups.clone();
    g.retain(|x| !remove.contains(x));
    for a in add {
        if !g.contains(a) {
            g.push(a.clone());
        }
    }
    if u.groups.contains(&"sysop".to_string()) && !g.contains(&"sysop".to_string()) {
        let admins: i64 = c.query_row("SELECT count(*) FROM user WHERE (',' || groups || ',') LIKE '%,sysop,%'", [], |r| r.get(0))?;
        if admins <= 1 {
            return Err(ApiError::bad("lastadmin", "cannot remove the last sysop"));
        }
    }
    c.execute("UPDATE user SET groups=?1 WHERE id=?2", params![g.join(","), u.id])?;
    need_user(c, name)
}

pub fn set_block(c: &Connection, name: &str, blocked: bool, reason: &str) -> Res<UserRow> {
    let u = need_user(c, name)?;
    if blocked && u.groups.iter().any(|g| g == "sysop") {
        return Err(ApiError::bad("cantblock", "sysops cannot be blocked; remove the group first"));
    }
    c.execute("UPDATE user SET blocked=?1, block_reason=?2 WHERE id=?3", params![blocked as i64, reason, u.id])?;
    need_user(c, name)
}

pub fn rotate_token(c: &Connection, user_id: i64) -> Res<String> {
    let token = new_token();
    c.execute("UPDATE user SET token_hash=?1 WHERE id=?2", params![sha256_hex(token.as_bytes()), user_id])?;
    Ok(token)
}

pub fn list_users(c: &Connection, prefix: &str, group: Option<&str>, limit: i64, offset: i64) -> Res<Vec<UserRow>> {
    let like = format!("{}%", prefix.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
    let mut st = c.prepare(&format!(
        "SELECT {UCOLS} FROM user WHERE name LIKE ?1 ESCAPE '\\' AND (?2 IS NULL OR (',' || groups || ',') LIKE '%,' || ?2 || ',%') ORDER BY name LIMIT ?3 OFFSET ?4"
    ))?;
    let rows = st.query_map(params![like, group, limit, offset], user_row)?.collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Makes sure at least one sysop exists. Returns a freshly generated token (to log once) if
/// one had to be created.
pub fn ensure_admin(c: &Connection, cfg: &Config) -> Res<Option<String>> {
    if let Some(t) = &cfg.admin_token {
        if t.len() < 16 {
            tracing::warn!("ADMIN_TOKEN is shorter than 16 characters; use a long random value");
        }
        c.execute(
            "INSERT INTO user(name,token_hash,groups,registered) VALUES('Admin',?1,'sysop',?2)
             ON CONFLICT(name) DO UPDATE SET token_hash=excluded.token_hash, groups='sysop', blocked=0",
            params![sha256_hex(t.as_bytes()), now()],
        )?;
        return Ok(None);
    }
    let admins: i64 = c.query_row("SELECT count(*) FROM user WHERE (',' || groups || ',') LIKE '%,sysop,%'", [], |r| r.get(0))?;
    if admins > 0 {
        return Ok(None);
    }
    let (_, token) = create_user(c, "Admin", &["sysop".to_string()])
        .or_else(|_| create_user(c, "Admin2", &["sysop".to_string()]))?;
    Ok(Some(token))
}
