# Hoepedia: advanced guide

The complete reference: every API surface, the wikitext engine, files, running several servers, and every setting.
New here? Start with the [beginner guide](README.md).

Hoepedia is a self-hosted **wiki engine that is only an API**. There is no website and no upstream wiki: it stores its own pages,
revisions and files, parses its own wikitext (templates, parser functions, Lua modules), runs its own search index and enforces
its own permissions. You build the front end, bot or app on top.

**Contents**

1. [Overview](#overview)
2. [Quick start](#quick-start)
3. [Authentication and permissions](#authentication-and-permissions)
4. [Editing, history and merging](#editing-history-and-merging)
5. [Files, thumbnails and previews](#files-thumbnails-and-previews)
6. [The wikitext engine](#the-wikitext-engine)
7. [Interwiki and federation](#interwiki-and-federation)
8. [Action API specifics](#action-api-specifics)
9. [Feeds and watchlists](#feeds-and-watchlists)
10. [Backups and maintenance](#backups-and-maintenance)
11. [Running several servers](#running-several-servers)
12. [Going public: security checklist](#going-public-security-checklist)
13. [Configuration reference](#configuration-reference)
14. [Development](#development)
15. [What changed in 0.8](#what-changed-in-08)

## Overview

| Surface | Mounted at | Style |
|---|---|---|
| **Action-style API** | `/api.php` (alias `/w/api.php`) | MediaWiki Action API parameter names and `formatversion=2` JSON: `action=query/parse/edit/upload/...`, `generator=`, `prop=imageinfo` |
| **REST API** | `/rest/v1/...` | Modelled on MediaWiki core REST, plus pages, files, history, diffs, search, feeds, moderation, backups |
| **Wikimedia REST aliases** | `/api/rest_v1/...` | `page/summary/{t}`, `page/html/{t}`, `page/title/{t}`, `page/random/summary` |
| **OpenAPI 3** | `/openapi.json` | Machine-readable description of all of the above |

What is inside the single program:

* SQLite in WAL mode with FTS5 full-text search, Lua 5.4, image scaling, SVG rendering and animated WebP, all compiled in.
* Nothing else is needed to run it. If **ffmpeg** and **Poppler** are installed (the Docker image has both), PDFs, videos and
  sound files get previews too.
* Replicas follow a primary live, accept writes on its behalf, and elect a new primary by majority vote when it fails.

`GET /` lists the endpoints, `GET /api.php?action=help` lists the actions, `GET /openapi.json` is the full spec.

## Quick start

There is nothing to configure. Pick whichever of these fits your machine:

| You have | Linux / macOS | Windows |
|---|---|---|
| **A downloaded release** (program and scripts in one folder) | `./start.sh` | double-click `start.bat` |
| **Docker** | `./start.sh` | double-click `start.bat` (with Docker Desktop running) |
| **Rust** ([rustup.rs](https://rustup.rs)) | `./start.sh --native` | `start.bat --native` |

The start script uses the first of those it finds and prints where the API is and what the admin token is:

```
  Hoepedia API 0.8.0 is running (primary)

  API         http://127.0.0.1:3000
  Data        ./data/wiki.db
  Previews    PDF, video and audio
  Admin token hp_3f9a...          (new; also saved in ./data/admin-token.txt)
```

Then, in another terminal, let the server test itself:

```
hoepedia-api check          # or: docker compose exec hoepedia-api hoepedia-api check
```

Things worth knowing:

* **The admin token** is created on the first start and kept in `data/admin-token.txt`; `hoepedia-api token` prints it. Send it as
  `Authorization: Bearer <token>` to write. To choose your own, set `ADMIN_TOKEN`.
* **Your data** is the `data` folder (or the `hoepedia-data` Docker volume).
* **Settings** are optional. They can go in a `.env` file in the folder the server starts in (copy `.env.example`), in environment
  variables, or on the command line: `hoepedia-api --port 8080 --data D:\wiki --bind 0.0.0.0`. `hoepedia-api --help` lists the
  options. Empty values fall back to the defaults.
* **Network:** run directly, the server only listens on this computer (`127.0.0.1`). `--bind 0.0.0.0` opens it to your network.
  The Docker setup publishes it on `127.0.0.1:3000` (`HOST_BIND`, `HOST_PORT`). Read the
  [security checklist](#going-public-security-checklist) before exposing it to the internet.
* A new wiki starts with one page, `Main Page`, that lists the first steps.

### Building from source

You need Rust and a C compiler: on Debian/Ubuntu `sudo apt install build-essential`; on Windows the Visual Studio C++ Build Tools,
which the Rust installer offers to install. Then `cargo build --release`; the program is `target/release/hoepedia-api`.

Ready-made downloads for Linux, Windows and macOS are produced by the included GitHub Actions workflow whenever a version tag
(`v0.8.0`) is pushed.

### First requests

**Linux / macOS**

```bash
H=http://localhost:3000; T="Authorization: Bearer $(./hoepedia-api token 2>/dev/null)"

curl -X PUT -H "$T" -H "Content-Type: application/json" $H/rest/v1/page/Ada_Lovelace \
  -d '{"source":"{{SHORTDESC:Mathematician}}\n'"'''Ada'''"' wrote about [[Analytical Engine]].\n[[Category:People]]","comment":"create"}'
curl $H/rest/v1/page/Ada_Lovelace/summary
curl -X PUT -H "$T" $H/rest/v1/file/Ada.png --data-binary @ada.png                       # upload
curl "$H/api.php?action=query&generator=categorymembers&gcmtitle=Category:People&prop=extracts|imageinfo&exintro&explaintext"
curl -N $H/rest/v1/stream/changes                                                        # live change stream (SSE)
```

**Windows (PowerShell)**

```powershell
$H = "http://localhost:3000"
$T = @{ Authorization = "Bearer $(.\hoepedia-api.exe token 2>$null)" }

Invoke-RestMethod -Method Put -Uri "$H/rest/v1/page/Ada_Lovelace" -Headers $T -ContentType "application/json" `
  -Body '{"source":"Ada wrote about [[Analytical Engine]]. [[Category:People]]","comment":"create"}'
Invoke-RestMethod "$H/rest/v1/page/Ada_Lovelace/summary"
Invoke-RestMethod -Method Put -Uri "$H/rest/v1/file/Ada.png" -Headers $T -InFile .\ada.png
Invoke-RestMethod "$H/rest/v1/search/page?q=analytical"
```

With Docker, replace `./hoepedia-api token` by `docker compose exec hoepedia-api hoepedia-api token`.

## Authentication and permissions

Bearer tokens only (`Authorization: Bearer hp_...` or `X-Api-Token: hp_...`): no cookies, sessions or CSRF tokens.
Tokens are stored as SHA-256 hashes and shown once.

* The administrator account **Admin** gets its token from `ADMIN_TOKEN` if that is set, otherwise from `data/admin-token.txt`,
  which is created on the first start (`hoepedia-api token` prints it). A wiki created before 0.7 without `ADMIN_TOKEN` keeps the
  token it showed back then.
* Sysops create users (`POST /rest/v1/users` or `action=createuser`); `POST /rest/v1/me/token` rotates your own token.

| Group | Rights |
|---|---|
| anonymous | `read`. With `ANON_EDIT=true` also edit and create (never upload). `ANON_READ=false` makes the wiki private. History records the IP, a masked IP or a pseudonym (`ANON_IDENTITY=ip\|masked\|hashed`). |
| `user` | read, edit, create, move, **upload** |
| `sysop` | everything a user can, plus delete/undelete, protect, edit protected pages, rollback, block, user rights, create users, delete files, interwiki, backups, reindex and replication (`siteadmin`) |

## Editing, history and merging

| Request | What it does |
|---|---|
| `PUT /rest/v1/page/{title}` `{"source","comment"}` | creates or changes a page (`action=edit` does the same) |
| `... "latest":{"id":N}` | says which revision your edit is based on, so concurrent edits are merged (below) |
| `POST /rest/v1/page/{title}/undo` `{"revision":N}` | undoes revision N (with `"after":M`, every revision after M up to N), even if later edits exist, as long as they don't overlap (`409 undofailure` otherwise); also `action=edit&undo=N&undoafter=M` |
| `POST /rest/v1/page/{title}/restore` `{"revision":N}` | makes an old revision current again (as a new revision) |
| `GET /rest/v1/page/{title}/history` | the revisions, newest first |

Section edits (`section=N|new`), `appendtext` and `prependtext` work through `action=edit`. Rollback, move (with redirect),
protect, delete/undelete, block and an audit log are included.

**Merging.** When an edit names its base revision and the page has moved on since, the engine runs a **three-way merge**: first
on lines and, where two edits touch the same lines, on **words**. Two people changing different words of one sentence merge
automatically (`"merged": true`). Only edits to the same words return **409 `editconflict`**, and that answer carries everything
needed to resolve it: `error.conflict = {current_rev, current, merged, conflicts}`, where `merged` is the text with
`<<<<<<< current` / `=======` / `>>>>>>> yours` markers around each conflicting part. Through the Action API, pass
`baserevid=N`.

## Files, thumbnails and previews

### Uploading

* `PUT /rest/v1/file/{name}` (raw body) uploads a file or adds a new version; `POST` only creates. `action=upload` accepts
  multipart (`file=`) or base64 (`filedata=`). The limit is `UPLOAD_MAX_BYTES` (10 MB).
* The client's `Content-Type` is **never trusted**. The content is sniffed (PNG, JPEG, GIF, WebP, SVG, PDF, Ogg, WebM, MP4, MP3),
  the file name's extension must match it, and anything else is rejected.
* **SVG** uploads are parsed and rewritten from scratch (`src/svg.rs`): scripts, event handlers, `foreignObject`, animation,
  external references, entities and editor metadata are removed, and only the cleaned file is stored. Malformed files are refused
  (`badsvg`). `ALLOW_SVG=false` refuses SVG altogether.
* Files keep every version, identical uploads are recognised by SHA-256, deleting is reversible, and each file has a `File:`
  description page. Listing: `GET /rest/v1/files`, `list=allimages|imageusage`, `prop=imageinfo|images|fileusage|duplicatefiles`.

### Downloading

`GET /rest/v1/file/{name}` sends the sniffed type, `X-Content-Type-Options: nosniff`, a sandboxing `Content-Security-Policy` and an
`ETag`. `?version=N` fetches an old version, cached forever.

### Thumbnails

`?width=300` (and/or `height=`) returns a scaled copy:

| Original | Thumbnail |
|---|---|
| PNG, JPEG, still GIF or WebP | PNG or JPEG |
| animated GIF | animated GIF |
| animated WebP | animated WebP (full colour and transparency) |
| SVG | PNG, any size up to 2048 px wide; text drawn with an embedded DejaVu font |
| PDF, video, audio | see [previews](#pdf-video-and-audio-previews) |

* Widths are rounded up to a fixed set of steps, pictures are never scaled up, and EXIF orientation is applied.
* Thumbnails have their own `ETag` and are cached in memory (`THUMB_CACHE_BYTES`) and on disk (`THUMB_DIR`, trimmed to
  `THUMB_DISK_BYTES`), so they survive restarts.
* Decoding is bounded: at most `THUMB_MAX_MEGAPIXELS` (25) per picture and two pictures at a time (`503 busy` beyond that).
* In pages, `[[File:x.png|thumb|200px|left|alt=...|Caption]]` renders a `<figure>` whose image URL carries `?width=`.
  `prop=imageinfo&iiurlwidth=` and `prop=pageimages` return thumbnail URLs.

### PDF, video and audio previews

These need `pdftoppm`/`pdfinfo` (Poppler) and `ffmpeg`/`ffprobe`. The Docker image includes them; elsewhere, install them and
they are found in `PATH` (or set `FFMPEG_PATH`, `FFPROBE_PATH`, `PDFTOPPM_PATH`, `PDFINFO_PATH`). The start-up banner shows which
previews are available; `MEDIA_PREVIEWS=off` turns them off.

| Request | Picture |
|---|---|
| `Report.pdf?width=300` | the first page; `&page=3` for another one |
| `Talk.mp4?width=300` | a frame a tenth into the video; `&time=12.5` for another moment |
| `Song.mp3?width=300` | a waveform |
| `?page=` or `?time=` without a width | the same at 640 px |

* After an upload, duration, page count and frame size are read in the background. They appear in the file's info (`duration`,
  `pages`, `width`, `height`, `mediatype`) and in `prop=imageinfo`, which also returns `thumburl`.
* In pages, `[[File:talk.mp4|thumb|start=1:05|Caption]]` renders a `<video controls>` with a poster frame, `[[File:song.mp3]]` an
  `<audio controls>`, and `[[File:report.pdf|page=2|200px]]` a linked picture of that page.
* **Safety:** each run works on a private copy in a temporary folder and has a time limit and (on Linux/macOS) CPU, memory and
  file-size limits. ffmpeg is told the container format and may read only that one local file, so a crafted upload cannot make
  it fetch URLs or read other files.
* Without the programs, these files are stored and served as usual and `?width=` answers `nothumbnail`, naming what to install.

## The wikitext engine

Written from scratch (`src/wikitext.rs`). It handles inline and block markup, headings and the section tree, nested lists,
tables, `[[links]]` (with red-link marking), categories, redirects, `<ref>`/`<references/>`, `<nowiki>`/`<pre>`/`<syntaxhighlight>`,
`<noinclude>`/`<includeonly>`, and **templates** (`{{T|a|k=v}}`, `{{{1|default}}}`).

### Parser functions and variables

* **Conditions:** `#if #ifeq #ifexist #switch #ifexpr #iferror`.
* **Arithmetic:** `#expr` (comparison, boolean, `round`, `mod`, `fmod`, `div`, math functions).
* **Dates:** `#time` (UTC) and `#timel` (the wiki's `TIMEZONE`; any IANA zone, DST-aware) with PHP `date()` codes. Input can be ISO,
  `5 March 2024`, `@unix`, `+2 weeks`, `yesterday` and similar. `{{LOCALTIME}}`, `{{LOCALYEAR}}` ... sit alongside the `CURRENT*`
  variables.
* **Text:** `lc uc lcfirst ucfirst urlencode fullurl localurl padleft padright plural formatnum anchorencode`, `#len #sub #pos #replace`.
* **Page:** `SHORTDESC`, `DISPLAYTITLE`, and variables such as `PAGENAME`, `CURRENTYEAR`, `SITENAME`, `{{!}}`.

### Lua modules

`{{#invoke:Module|function|args}}` runs `Module:Name` pages in a sandboxed Lua 5.4: no io, debug or package, no bytecode, 16 MiB
of memory and 1.5 s per call, at most 50 invokes per page. Errors render as `Script error: ...`. The `mw` library (`src/mw.lua`)
covers what templates commonly use:

| Part | Contents |
|---|---|
| loading | `require('Module:X')`, `mw.loadData`, `mw.loadJsonData` |
| frames | `args`, `getParent`, `getTitle`, `preprocess`, `expandTemplate`, `callParserFunction`, `extensionTag`, `newChild`, `getArgument` |
| `mw.text` | split, gsplit, trim, nowiki, encode, decode, tag, listToText, truncate, jsonEncode/jsonDecode ... |
| `mw.ustring` | **Unicode-aware** `find/match/gmatch/gsub`: patterns work on characters and `%a %l %u %w` follow Unicode |
| `mw.html` | the HTML builder |
| `mw.title` | `new`, `makeTitle`, `getCurrentTitle`; `.exists`, `:getContent()` ... |
| `mw.language` | `mw.getContentLanguage()`, formatDate, formatNum, lc/uc, plural ... |
| others | `mw.uri`, `mw.site` (with live `stats`, `pagesInCategory`, `pagesInNamespace`), `mw.hash` (md5, sha1, sha2), `mw.message` (raw messages with `$1` parameters), `mw.clone`, `os.time/date/clock` |

Not available: `mw.wikibase` (Wikidata) and `mw.ext.*` (MediaWiki extensions). Interface messages are English whatever `SITE_LANG`
says, and the `mw.ustring` normalisation functions (`toNFC` ...) return their input. Heavily Wikipedia-specific modules may need edits.

### Raw HTML and safety

`<div>`, `<span>`, `<blockquote>`, `<table>/<tr>/<td>/<th>`, lists and the usual inline tags may be written directly (this is also
what `mw.html` emits). Attributes are limited to `class`, `title`, `lang`, `dir`, `colspan`/`rowspan`/`scope` and a safe subset of
`style` (no `url()`, no positioning); every opened tag is closed by the renderer.

**Output is safe by construction:** all source text is escaped and only whitelisted tags with checked attributes are emitted, so
nothing a page, template, module or remote wiki produces can add scripts, event handlers or external resources. Template expansion
is bounded in depth, calls and size.

### Conversions

* `action=expandtemplates` or `POST /rest/v1/transform/wikitext/expand` returns expanded wikitext.
* `POST /rest/v1/transform/wikitext/to/html` renders wikitext without saving it.
* `POST /rest/v1/transform/html/to/wikitext` converts what the engine renders back (round-trip tested: wikitext → HTML →
  wikitext → HTML is identical) and common pasted HTML; scripts and styles are dropped.
* Editing a template or module re-indexes the pages that use it (up to `REFRESH_LINKS_LIMIT` at once).

## Interwiki and federation

A sysop registers other wikis under a prefix:

```bash
curl -X PUT -H "$T" -H "Content-Type: application/json" $H/rest/v1/interwiki/wp \
  -d '{"url":"https://en.wikipedia.org/wiki/$1", "api":"https://en.wikipedia.org/w/api.php", "trans":true}'
```

| Field | Effect |
|---|---|
| `url` | `[[wp:Rust]]` becomes a link to that wiki |
| `api` | the other wiki's Action API (another Hoepedia or a MediaWiki): enables **federated search** with `GET /rest/v1/search/page?q=...&interwiki=1` or `list=search&srinterwiki=1` |
| `trans` | enables **interwiki transclusion**: `{{wp:Template:Infobox\|a=b}}` fetches that page's wikitext and expands it with your arguments |
| `language` | makes it a language link: `[[fr:Titre]]` is not shown in the text but returned by `prop=langlinks`; `[[:fr:Titre]]` (leading colon) stays an ordinary link |

Templates and modules that a remote page uses are fetched **from the same remote wiki** (falling back to yours), so a remote infobox
works without copying its dependencies. Remote text is untrusted and goes through the same sanitiser. Only prefixes a sysop
registered are ever contacted, and requests never follow redirects.

**Other wikis can never slow this one down:**

* **Edits and other writes never wait for another wiki.** While the database is being written, only cached copies are used;
  anything missing is fetched in the background, and the pages that use it are re-rendered as soon as it arrives (seconds).
* **Readers wait at most 3 s, and only for a page this wiki has never fetched.** Older copies (`INTERWIKI_CACHE_SECS`, default an
  hour) are served while a fresh one is fetched in the background; if the other wiki is down, the last good copy stays.
* **Changes on the other wiki arrive by themselves:** every `INTERWIKI_POLL_SECS` (60) each wiki is asked for its recent changes;
  changed pages are fetched again and the pages using them re-rendered.
* `POST /rest/v1/interwiki/{prefix}/purge` forgets everything cached (for all prefixes).

## Action API specifics

`action=query`:

* `prop=` `info revisions categories links templates linkshere transcludedin redirects images fileusage duplicatefiles extlinks
  iwlinks langlinks contributors categoryinfo pageimages extracts pageprops description imageinfo`
* `list=` `search prefixsearch allpages allcategories allredirects alllinks alltransclusions allfileusages allrevisions
  alldeletedrevisions (sysop) categorymembers backlinks embeddedin imageusage exturlusage iwbacklinks pageswithprop querypage
  recentchanges logevents watchlist watchlistraw random allusers users blocks usercontribs allimages`
* `list=querypage&qppage=` `Wantedpages Wantedcategories Wantedtemplates Lonelypages Uncategorizedpages Deadendpages Longpages
  Shortpages Ancientpages Mostlinked Mostrevisions Mostcategories Unusedtemplates Unusedimages Unusedcategories BrokenRedirects
  DoubleRedirects Listredirects Withoutinterwiki`
* `meta=` `siteinfo(general,namespaces,statistics,usergroups,interwikimap) userinfo tokens`
* **`generator=`** any list that yields pages (`allpages allcategories allimages allredirects alllinks alltransclusions
  allfileusages allrevisions alldeletedrevisions categorymembers search prefixsearch backlinks embeddedin imageusage exturlusage
  iwbacklinks pageswithprop querypage random recentchanges usercontribs logevents watchlist watchlistraw`), or, starting from
  `titles=`/`pageids=`, `links templates categories images linkshere transcludedin redirects fileusage`. Generator parameters are
  prefixed with `g`, and continuation uses `g...` keys.

Other actions: `parse`, `expandtemplates`, `edit`, `upload`, `move`, `delete`, `undelete`, `protect`, `rollback`, `watch`,
`opensearch`, `compare`, `createuser`, `userrights`, `block`, `unblock`, `rotatetoken`. Write actions require POST.

Differences from MediaWiki: JSON only, and no `login` or CSRF tokens (`meta=tokens` returns the dummy `+\`); send the bearer token
instead.

## Feeds and watchlists

* `GET /rest/v1/feed/changes?cursor=R-L` returns edits, new pages and log events, oldest first, resumable and without gaps. Call it
  without `cursor` to get the current position ("start from now"), and store the `cursor` each answer returns.
* `GET /rest/v1/stream/changes` is a **Server-Sent Events** stream (`event: change`, `id: <cursor>`). Reconnecting with
  `Last-Event-ID` replays what you missed. At most 200 streams at a time.
* `action=query&list=recentchanges` supports `rcstart`, `rcend` and `rcdir`.
* **Watchlists** (per user): `PUT` / `DELETE /rest/v1/watchlist/{title}`; `GET /rest/v1/watchlist` gives recent changes to
  watched pages (`?raw=1` for just the titles); also `action=watch&titles=A|B[&unwatch=1]`, `list=watchlist`, `list=watchlistraw`.

## Backups and maintenance

* **Backups:** `POST /rest/v1/admin/backups` (sysop) writes a consistent, compacted copy (`VACUUM INTO`) into `BACKUP_DIR` while the
  wiki keeps serving; writers pause for the copy. `GET /rest/v1/admin/backups` lists them, `GET .../{name}` downloads one,
  `DELETE .../{name}` removes it. To restore: stop the server, replace the database file with the backup, start again.
* **Read-only mode:** `READ_ONLY=true` rejects every write with `503 readonly`; reads, feeds and backups keep working.
* **Reindex:** `POST /rest/v1/admin/reindex` (sysop) re-renders every page and rebuilds links, page properties and the search
  index. Upgrading from an older version does this once automatically.
* **Monitoring:** `GET /healthz` for health checks, `GET /metrics` for Prometheus (no token needed, so keep it off the public
  internet).
* **Errors** always have the shape `{"error":{"code":"...","info":"..."},"httpCode":N}`.

## Running several servers

### Replicas

Start another instance with `REPLICA_OF=<the primary's URL>` and `REPLICA_TOKEN=<a sysop token of the primary>`, or on one host
`docker compose --profile replica up -d`. The replica copies the whole wiki, then follows the primary by long-polling: changes arrive
within milliseconds. Each batch is applied in one transaction, so a replica only ever shows states the primary really had.

* **Reads:** replicas answer every read (pages, search, files, thumbnails, feeds) and accept the same tokens.
* **Writes through any node:** a replica passes write requests on to the primary and answers only once it has applied the change
  itself, so a client behind a load balancer can read its own write from the node it wrote to. `REPLICA_FORWARD_WRITES=false`
  makes replicas answer `503 readonly` instead.
* **Confirmed writes:** `REPL_MIN_ACKS=1` on the primary makes every write wait until that many replicas have applied it (usually a
  few milliseconds). Without automatic fail-over, a write nobody confirms within `REPL_ACK_TIMEOUT_MS` still stands and a warning is
  logged.
* `GET /rest/v1/replication/status` shows the role, term, lag and (on the primary) each replica's position.
  `POST /rest/v1/replication/resync` on a replica rebuilds its copy. Upgrade replicas together with the primary.

### Automatic fail-over

Fail-over is decided by **majority vote**, so it needs **three or more** nodes. Each node gets the same settings, plus `REPLICA_OF`
on all but the first. An example with three machines:

```ini
# on every node (in .env or as environment variables)
ADMIN_TOKEN=<one long random string, the same everywhere>
REPLICA_TOKEN=<the same string>
PEERS=https://wiki-a.example,https://wiki-b.example,https://wiki-c.example
FAILOVER_AFTER_SECS=10

# only on wiki-b and wiki-c
REPLICA_OF=https://wiki-a.example
```

What then happens:

* **Writes:** a write succeeds only once a **majority** of the nodes has it (the primary plus enough replicas; `REPL_MIN_ACKS` can
  ask for more). If the majority does not confirm in time, the answer is `503 notconfirmed`: the change is kept if this primary stays
  in charge and may be lost otherwise, so check before retrying.
* **A primary on its own stops:** a primary that has not heard from a majority for half of `FAILOVER_AFTER_SECS` stops accepting
  changes (`503 noquorum`) before anyone else is allowed to take over, so there are never two primaries accepting writes.
* **Election:** when the primary has been unreachable for `FAILOVER_AFTER_SECS`, a replica asks the others for their votes and
  becomes the primary only with a majority. A node votes once per election, refuses while it still hears from a primary, and
  refuses a candidate that lacks changes it has. So a replica that is merely cut off from a healthy primary cannot take over, and the
  winner always has every confirmed write. The other replicas find and follow the winner.
* **The old primary returns:** once it sees the new primary, it saves its data as `pre-demotion-*.db` in `BACKUP_DIR` and becomes a
  replica. Changes it took that the majority never confirmed (and that were therefore never reported as successful) exist only in
  that backup.
* Roles survive restarts. `POST /rest/v1/replication/promote` promotes a node by hand; `FAILOVER_AFTER_SECS=0` (the default) leaves
  fail-over manual.

With two nodes a majority is both, so a two-node cluster with automatic fail-over stops accepting changes when either node is down.
Use three; the third can be a small machine that only votes and holds a copy.

## Going public: security checklist

Before the wiki is reachable from the internet:

* **Put a TLS reverse proxy in front** (Caddy, nginx, Traefik) and set `TRUST_PROXY=true` so rate limits and history see the real
  client address. Leave `TRUST_PROXY=false` when there is no proxy, or clients could fake their address.
* **Keep the admin token secret**, and give every person or app their own token (`POST /rest/v1/users`).
* **Block `/metrics`** at the proxy; it needs no token.
* **Decide who may edit:** `ANON_EDIT` is off by default; `ANON_READ=false` makes the wiki private.
* **Restrict `CORS_ORIGINS`** to your own sites if you like. The default `*` is safe because authentication is an explicit header,
  never a cookie, but a list is tidier.
* **Keep `RATE_LIMIT_PER_MIN`** on (300 per address by default).
* **Back up** regularly and copy the backups to another machine.

The Docker container already runs as a non-root user with a read-only root file system and all capabilities dropped; only `/data`
is writable.

## Configuration reference

Every setting is optional. `.env.example` contains all of them with comments.

**Server**

| Setting | Default | Meaning |
|---|---|---|
| `BIND_ADDR` | `127.0.0.1` | address to listen on; `0.0.0.0` = all networks (`--bind`) |
| `PORT` | `3000` | port (`--port`) |
| `DATABASE_PATH` | `./data/wiki.db` | the database; its folder holds everything else (`--data` sets the folder) |
| `HOST_BIND`, `HOST_PORT` | `127.0.0.1`, `3000` | Docker only: where the container is published on the host |
| `RUST_LOG` | `info` | log detail (`warn`, `info`, `debug`) |

**Site and access**

| Setting | Default | Meaning |
|---|---|---|
| `SITE_NAME` | `Hoepedia` | the wiki's name |
| `ADMIN_TOKEN` | generated | token of the `Admin` account |
| `ARTICLE_PATH` | `/wiki/{title}` | where links between pages point on your front end (`$1` works too) |
| `FILE_PATH` | `/rest/v1/file/{name}` | where file embeds point, e.g. a CDN |
| `ANON_READ` | `true` | `false` = reading needs a token |
| `ANON_EDIT` | `false` | `true` = anyone may edit and create pages |
| `ANON_IDENTITY` | `ip` | what history records for anonymous edits: `ip`, `masked` or `hashed` |
| `CORS_ORIGINS` | `*` | browser origins allowed to call the API, comma-separated |
| `TIMEZONE` | `UTC` | IANA zone for `{{#timel:}}`, `{{LOCALTIME}}` and similar |
| `SITE_LANG` | `en` | content language reported by the API and to Lua |

**Limits and performance**

| Setting | Default | Meaning |
|---|---|---|
| `MAX_PAGE_BYTES` | `2000000` | largest page text |
| `RATE_LIMIT_PER_MIN` | `300` | requests per minute per address; `0` = off |
| `TRUST_PROXY` | `false` | read the client address from `X-Forwarded-For` (only behind your own proxy) |
| `REQUEST_TIMEOUT_SECS` | `25` | longest a request may take |
| `READ_CONNECTIONS` | `4` | database connections for reads (1 to 32) |
| `RENDER_CACHE_ENTRIES` | `5000` | rendered pages kept in memory |
| `REFRESH_LINKS_LIMIT` | `500` | pages re-indexed at once when a template changes |
| `CACHE_MAX_AGE_SECS` | `0` | `Cache-Control` max-age on reads; `0` = always revalidate with the `ETag` |

**Files**

| Setting | Default | Meaning |
|---|---|---|
| `UPLOAD_MAX_BYTES` | `10000000` | largest upload |
| `ALLOW_SVG` | `true` | accept (cleaned) SVG uploads |
| `THUMB_MAX_MEGAPIXELS` | `25` | largest picture that is decoded for thumbnails |
| `THUMB_CACHE_BYTES` | `64000000` | memory for cached thumbnails |
| `THUMB_DIR` | `thumbs` next to the database | disk cache for thumbnails; `off` = memory only |
| `THUMB_DISK_BYTES` | `512000000` | size of the disk cache |
| `MEDIA_PREVIEWS` | `on` | `off` = no PDF, video and audio previews |
| `FFMPEG_PATH`, `FFPROBE_PATH`, `PDFTOPPM_PATH`, `PDFINFO_PATH` | found in `PATH` | where the preview programs are |

**Interwiki**

| Setting | Default | Meaning |
|---|---|---|
| `INTERWIKI_CACHE_SECS` | `3600` | how long a fetched remote page counts as fresh (at least 10) |
| `INTERWIKI_POLL_SECS` | `60` | how often other wikis are asked for changes; `0` = never |

**Backups, replicas and fail-over**

| Setting | Default | Meaning |
|---|---|---|
| `BACKUP_DIR` | `./data/backups` | where backups go |
| `READ_ONLY` | `false` | reject every write |
| `REPLICA_OF` | empty | the primary's URL; makes this node a replica |
| `REPLICA_TOKEN` | empty | a sysop token of the primary |
| `REPLICA_FORWARD_WRITES` | `true` | replicas pass writes on to the primary |
| `REPL_MIN_ACKS` | `0` | replicas that must confirm each write (with automatic fail-over, never fewer than a majority needs) |
| `REPL_ACK_TIMEOUT_MS` | `2000` | how long a write waits for confirmations |
| `PEERS` | empty | URLs of all nodes, including this one, comma-separated |
| `FAILOVER_AFTER_SECS` | `0` | how long the primary must be unreachable before an election; `0` = manual fail-over only |
| `REPLICA_HOST_PORT` | `3001` | Docker only: host port of the optional same-host replica |

## Development

```bash
cargo test                                  # 104 unit tests: parser, Lua sandbox and mw library, Unicode patterns, #time and
                                            # zones, expr, merge, SVG cleaner, thumbnails, media previews (skipped without
                                            # ffmpeg/Poppler), replication, elections, queries, feeds, HTML round trip
hoepedia-api check                          # built-in self-test of a running server (any platform)
./test-hoepedia.sh http://localhost:3000    # the longer smoke test (bash, curl, python3; needs ADMIN_TOKEN)
```

Source layout: `src/wikitext.rs` (parser and renderer), `src/mw.lua` (Lua `mw` library), `src/svg.rs` (SVG cleaner),
`src/media.rs` (previews), `src/files.rs` (uploads and thumbnails), `src/remote.rs` (interwiki), `src/repl.rs` (replication and
elections), `src/action.rs` and `src/rest.rs` (the two APIs), `src/openapi.rs` (the spec).

If a build fails with an error about `moka` and `sync`: the `moka` dependency needs `default-features = false, features = ["sync"]`,
because releases since 0.12.12 no longer enable `sync` by default.

## What changed in 0.8

| Before | Now |
|---|---|
| No previews of PDF, video and audio | Page, frame and waveform previews via Poppler and ffmpeg (in the Docker image), file metadata, `<video>`/`<audio>` embeds |
| Animated WebP thumbnails became 256-colour GIFs | Animated WebP stays animated WebP (libwebp, compiled in) |
| A slow remote wiki could delay renders by 8 s and, during edits, other writes | Writes never wait; readers wait at most 3 s and only for never-fetched pages; remote changes are polled |
| Best-effort fail-over: a cut-off replica could promote itself and writes could be lost | Majority votes, a primary that stops itself without a majority, writes confirmed by a majority |
