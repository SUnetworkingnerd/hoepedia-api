//! OpenAPI 3.0 description of the REST API and the Action-style endpoint, served at `/openapi.json`.

use serde_json::{json, Map, Value};

use crate::config::Config;

/// (method, path, tag, summary, auth: "" | "user" | "sysop", has JSON body)
const ROUTES: &[(&str, &str, &str, &str, &str, bool)] = &[
    ("get", "/rest/v1/", "meta", "Endpoint index", "", false),
    ("get", "/rest/v1/site", "meta", "Site info, namespaces, statistics", "", false),
    ("get", "/rest/v1/me", "meta", "Who am I (groups, rights)", "", false),
    ("post", "/rest/v1/me/token", "meta", "Rotate your own API token", "user", false),
    ("get", "/rest/v1/page/{title}", "pages", "Wikitext source and metadata", "", false),
    ("put", "/rest/v1/page/{title}", "pages", "Create or edit a page (auto-merges edits made against an older `latest.id`)", "edit", true),
    ("post", "/rest/v1/page/{title}", "pages", "Create a page (409 if it exists)", "edit", true),
    ("delete", "/rest/v1/page/{title}", "pages", "Delete a page (sysop)", "sysop", false),
    ("get", "/rest/v1/page/{title}/bare", "pages", "Same as GET page", "", false),
    ("get", "/rest/v1/page/{title}/html", "pages", "Rendered, sanitised HTML (ETag)", "", false),
    ("get", "/rest/v1/page/{title}/with_html", "pages", "Source plus rendered HTML", "", false),
    ("get", "/rest/v1/page/{title}/summary", "pages", "Short summary (extract, description)", "", false),
    ("get", "/rest/v1/page/{title}/sections", "pages", "Section tree", "", false),
    ("get", "/rest/v1/page/{title}/history", "revisions", "Revision history (cursor paged)", "", false),
    ("get", "/rest/v1/page/{title}/history/counts", "revisions", "Revision counts", "", false),
    ("get", "/rest/v1/page/{title}/links", "pages", "Outgoing links", "", false),
    ("get", "/rest/v1/page/{title}/linkshere", "pages", "Backlinks / transclusions", "", false),
    ("get", "/rest/v1/page/{title}/categories", "pages", "Categories of a page", "", false),
    ("get", "/rest/v1/page/{title}/templates", "pages", "Templates and modules used", "", false),
    ("post", "/rest/v1/page/{title}/move", "moderation", "Move a page", "user", true),
    ("post", "/rest/v1/page/{title}/undelete", "moderation", "Restore a deleted page", "sysop", true),
    ("post", "/rest/v1/page/{title}/protect", "moderation", "Set edit/move protection", "sysop", true),
    ("post", "/rest/v1/page/{title}/rollback", "moderation", "Revert the last editor's run of edits", "sysop", true),
    ("post", "/rest/v1/page/{title}/undo", "revisions", "Undo a specific revision via three-way merge", "edit", true),
    ("post", "/rest/v1/page/{title}/restore", "revisions", "Make an old revision current again", "edit", true),
    ("get", "/rest/v1/revision/{id}", "revisions", "A revision with its source", "", false),
    ("get", "/rest/v1/revision/{id}/html", "revisions", "A revision rendered to HTML", "", false),
    ("get", "/rest/v1/revision/{id}/compare/{to}", "revisions", "Line diff between two revisions", "", false),
    ("get", "/rest/v1/pages", "lists", "List pages (namespace, prefix, from, redirects)", "", false),
    ("get", "/rest/v1/random", "lists", "Random page summary", "", false),
    ("get", "/rest/v1/search/page", "search", "Full-text search with snippets (q, namespace)", "", false),
    ("get", "/rest/v1/search/title", "search", "Title autocomplete", "", false),
    ("get", "/rest/v1/category/{name}/members", "lists", "Category members", "", false),
    ("get", "/rest/v1/recentchanges", "lists", "Recent changes (edits, new pages, log)", "", false),
    ("get", "/rest/v1/logs", "lists", "Audit log", "", false),
    ("get", "/rest/v1/feed/changes", "feeds", "Resumable change feed (cursor R-L)", "", false),
    ("get", "/rest/v1/stream/changes", "feeds", "Server-Sent Events change stream (Last-Event-ID resumes)", "", false),
    ("get", "/rest/v1/file/{name}", "files", "Download a file (content-sniffed type, CSP sandbox, ETag)", "", false),
    ("put", "/rest/v1/file/{name}", "files", "Upload a file or a new version (raw body; ?comment=&description=)", "upload", false),
    ("post", "/rest/v1/file/{name}", "files", "Upload a new file (409 if it exists)", "upload", false),
    ("delete", "/rest/v1/file/{name}", "files", "Delete a file (sysop)", "sysop", false),
    ("get", "/rest/v1/file/{name}/info", "files", "File metadata (mime, size, dimensions, sha256)", "", false),
    ("get", "/rest/v1/file/{name}/history", "files", "File versions", "", false),
    ("get", "/rest/v1/files", "files", "List files (prefix, mime, from)", "", false),
    ("get", "/rest/v1/users", "users", "List users", "", false),
    ("post", "/rest/v1/users", "users", "Create a user; returns the token once (sysop)", "sysop", true),
    ("get", "/rest/v1/users/{name}/contributions", "users", "A user's edits", "", false),
    ("put", "/rest/v1/users/{name}/groups", "users", "Add/remove groups (sysop)", "sysop", true),
    ("post", "/rest/v1/users/{name}/block", "users", "Block a user (sysop)", "sysop", true),
    ("post", "/rest/v1/users/{name}/unblock", "users", "Unblock a user (sysop)", "sysop", false),
    ("post", "/rest/v1/transform/wikitext/to/html", "transform", "Render wikitext to sanitised HTML", "", true),
    ("post", "/rest/v1/transform/wikitext/expand", "transform", "Expand templates/parser functions/Lua, return wikitext", "", true),
    ("post", "/rest/v1/transform/html/to/wikitext", "transform", "Convert HTML to wikitext", "", true),
    ("post", "/rest/v1/admin/reindex", "admin", "Re-render every page and rebuild links, props and the search index (sysop)", "sysop", false),
    ("get", "/rest/v1/replication/status", "admin", "Replication role, sequence numbers and replica lag (sysop)", "sysop", false),
    ("get", "/rest/v1/replication/changes", "admin", "Rows changed after ?since=N (long-polls with ?wait=S); used by replicas (sysop)", "sysop", false),
    ("post", "/rest/v1/replication/vote", "admin", "Election: another cluster node asks for this node's vote (sysop)", "sysop", false),
    ("post", "/rest/v1/replication/promote", "admin", "On a replica: become the primary now (manual fail-over) (sysop)", "sysop", false),
    ("get", "/rest/v1/watchlist", "watchlist", "Recent changes to the pages you watch (?raw=1: the watched titles)", "user", false),
    ("put", "/rest/v1/watchlist/{title}", "watchlist", "Watch a page", "user", false),
    ("delete", "/rest/v1/watchlist/{title}", "watchlist", "Stop watching a page", "user", false),
    ("post", "/rest/v1/interwiki/{prefix}/purge", "site", "Forget cached remote templates (sysop)", "sysop", false),
    ("post", "/rest/v1/replication/resync", "admin", "On a replica: discard the local copy and copy the primary again (sysop)", "sysop", false),
    ("get", "/rest/v1/interwiki", "site", "Interwiki prefixes", "", false),
    ("put", "/rest/v1/interwiki/{prefix}", "site", "Add or change an interwiki prefix (sysop)", "sysop", true),
    ("delete", "/rest/v1/interwiki/{prefix}", "site", "Remove an interwiki prefix (sysop)", "sysop", false),
    ("get", "/rest/v1/admin/backups", "admin", "List backups (sysop)", "sysop", false),
    ("post", "/rest/v1/admin/backups", "admin", "Create a consistent online backup via VACUUM INTO (sysop)", "sysop", false),
    ("get", "/rest/v1/admin/backups/{name}", "admin", "Download a backup (sysop)", "sysop", false),
    ("delete", "/rest/v1/admin/backups/{name}", "admin", "Delete a backup (sysop)", "sysop", false),
    ("get", "/api/rest_v1/page/summary/{title}", "wikimedia-compat", "Wikimedia REST: page summary", "", false),
    ("get", "/api/rest_v1/page/html/{title}", "wikimedia-compat", "Wikimedia REST: page HTML", "", false),
    ("get", "/api/rest_v1/page/title/{title}", "wikimedia-compat", "Wikimedia REST: page history", "", false),
    ("get", "/api/rest_v1/page/random/summary", "wikimedia-compat", "Wikimedia REST: random summary", "", false),
    ("get", "/api/rest_v1/page/revision/{id}", "wikimedia-compat", "Wikimedia REST: a revision", "", false),
    ("get", "/healthz", "ops", "Liveness probe", "", false),
    ("get", "/metrics", "ops", "Prometheus metrics", "", false),
];

const ACTIONS: &[&str] = &[
    "help", "query", "parse", "expandtemplates", "opensearch", "compare", "edit", "upload", "move", "delete", "undelete", "protect", "rollback",
    "createuser", "userrights", "block", "unblock", "rotatetoken",
];

pub fn spec(cfg: &Config) -> Value {
    let mut paths: Map<String, Value> = Map::new();
    for (method, path, tag, summary, auth, body) in ROUTES {
        let mut op = json!({
            "tags": [tag],
            "summary": summary,
            "responses": {
                "200": { "description": "Success" },
                "default": { "description": "Error", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } } },
            },
        });
        let params: Vec<Value> = path
            .split('/')
            .filter_map(|seg| seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')))
            .map(|n| json!({ "name": n, "in": "path", "required": true, "schema": { "type": "string" } }))
            .collect();
        if !params.is_empty() {
            op["parameters"] = Value::Array(params);
        }
        if *body {
            op["requestBody"] = json!({ "content": { "application/json": { "schema": { "type": "object" } } } });
        }
        if !auth.is_empty() {
            op["security"] = json!([{ "bearer": [] }]);
            op["description"] = json!(format!("Requires the `{}` capability.", if *auth == "user" { "user" } else { auth }));
        }
        if path.starts_with("/rest/v1/file/") && matches!(*method, "put" | "post") && !path.ends_with("/info") {
            op["requestBody"] = json!({ "required": true, "content": { "application/octet-stream": { "schema": { "type": "string", "format": "binary" } } } });
        }
        paths.entry(path.to_string()).or_insert_with(|| json!({})).as_object_mut().unwrap().insert(method.to_string(), op);
    }
    let action_op = |m: &str| {
        json!({
            "tags": ["action"],
            "summary": format!("MediaWiki-style Action API ({m}); write actions require POST"),
            "description": "Parameter names follow the MediaWiki Action API. `action=help` lists modules. POST accepts form, JSON or multipart (for `action=upload`).",
            "parameters": [{ "name": "action", "in": "query", "schema": { "type": "string", "enum": ACTIONS } }],
            "responses": { "200": { "description": "JSON in formatversion=2 style" } },
        })
    };
    let mut action = Map::new();
    action.insert("get".into(), action_op("read"));
    let mut post = action_op("write");
    post["requestBody"] = json!({ "content": {
        "application/x-www-form-urlencoded": { "schema": { "type": "object" } },
        "application/json": { "schema": { "type": "object" } },
        "multipart/form-data": { "schema": { "type": "object" } },
    } });
    action.insert("post".into(), post);
    paths.insert("/api.php".into(), Value::Object(action));

    let tags: Vec<Value> = ["meta", "pages", "revisions", "moderation", "lists", "search", "feeds", "files", "users", "transform", "site", "admin", "action", "wikimedia-compat", "ops"]
        .iter()
        .map(|t| json!({ "name": t }))
        .collect();
    json!({
        "openapi": "3.0.3",
        "info": {
            "title": format!("{} (hoepedia-api)", cfg.site_name),
            "version": env!("CARGO_PKG_VERSION"),
            "description": "A self-hosted wiki engine exposed purely as an API. Authenticate with `Authorization: Bearer <token>`.",
        },
        "servers": [{ "url": "/" }],
        "tags": tags,
        "paths": paths,
        "components": {
            "securitySchemes": { "bearer": { "type": "http", "scheme": "bearer" } },
            "schemas": { "Error": { "type": "object", "properties": {
                "error": { "type": "object", "properties": { "code": { "type": "string" }, "info": { "type": "string" } } },
                "httpCode": { "type": "integer" } } } },
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_is_wellformed() {
        let s = spec(&Config::for_tests("x.db"));
        assert_eq!(s["openapi"], "3.0.3");
        assert!(s["paths"]["/rest/v1/page/{title}"]["put"]["requestBody"].is_object());
        assert!(s["paths"]["/api.php"]["post"].is_object());
        assert_eq!(s["paths"]["/rest/v1/page/{title}"]["get"]["parameters"][0]["name"], "title");
        assert!(s["paths"].as_object().unwrap().len() > 40);
    }
}
