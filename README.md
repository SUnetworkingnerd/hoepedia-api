# Hoepedia

Hoepedia is a wiki you talk to over HTTP. It stores pages, keeps every past version, searches them, and turns
wiki text into HTML. It has no website of its own: you build the site, app or bot, and Hoepedia is the engine
behind it.

This guide gets you from nothing to a working wiki in about ten minutes. When you want more, the
[advanced guide](ADVANCED.md) covers everything else.

**Contents**

1. [Start it](#1-start-it)
2. [Read your first page](#2-read-your-first-page)
3. [Set up your terminal](#3-set-up-your-terminal)
4. [Create a page](#4-create-a-page)
5. [Change a page](#5-change-a-page)
6. [Find pages](#6-find-pages)
7. [Wiki text in one table](#7-wiki-text-in-one-table)
8. [Upload a picture](#8-upload-a-picture)
9. [Let other people write](#9-let-other-people-write)
10. [Delete, and undo a delete](#10-delete-and-undo-a-delete)
11. [Keep your wiki safe](#11-keep-your-wiki-safe)
12. [Settings](#settings) · [When something goes wrong](#when-something-goes-wrong) · [Where to go next](#where-to-go-next)

## 1. Start it

You need **one** of these on your computer: a downloaded Hoepedia release, [Docker](https://docs.docker.com/get-docker/),
or [Rust](https://rustup.rs).

| | |
|---|---|
| **Windows** | double-click `start.bat` |
| **Linux / macOS** | run `./start.sh` in a terminal |

The first start can take a few minutes if the program has to be built. When it is ready you see:

```
  Hoepedia API 0.8.0 is running (primary)

  API         http://127.0.0.1:3000
  Data        ./data/wiki.db
  Previews    PDF, video and audio
  Admin token hp_0ab3ce1e538c...
              (new; also saved in ./data/admin-token.txt)
```

Two things to remember from that:

* **The address**, `http://localhost:3000`. Every request in this guide goes there.
* **The admin token**. It is your password for making changes. Reading is open to everyone; writing needs the token.
  If you lose it, `hoepedia-api token` prints it again (with Docker: `docker compose exec hoepedia-api hoepedia-api token`).

Leave that window open: closing it (or pressing Ctrl+C) stops the wiki. Your pages stay in the `data` folder.

**Check that it works.** Open a second terminal in the same folder and run:

```
hoepedia-api check
```

It creates a few test pages, tries the main features, removes the pages again and should end with
`All 14 checks passed.` (On Linux/macOS write `./hoepedia-api check`; if you started with Rust the program is in
`target/release/`; with Docker use `docker compose exec hoepedia-api hoepedia-api check`.)

## 2. Read your first page

A new wiki already has one page, called `Main Page`. Open this in your browser:

<http://localhost:3000/rest/v1/page/Main_Page/html>

You should see a short welcome text. That is all "reading a page" is: asking for an address.

There are two ways to read a page:

| Address | You get |
|---|---|
| `/rest/v1/page/Main_Page/html` | the page as HTML, ready to show on a website |
| `/rest/v1/page/Main_Page` | details as JSON, including `source`: the wiki text someone typed |

In page addresses, write spaces as underscores: the page "Main Page" is `Main_Page`.

## 3. Set up your terminal

The rest of the guide uses a terminal. Every step shows the command twice: once for **Linux / macOS** (bash) and once for
**Windows**. On Windows, use **PowerShell** (search for it in the Start menu), not the older Command Prompt.

Store the address and the token once, so the commands stay short. Replace `hp_...` with your own admin token.

**Linux / macOS**

```bash
H=http://localhost:3000
T="Authorization: Bearer hp_..."
```

**Windows (PowerShell)**

```powershell
$H = "http://localhost:3000"
$T = @{ Authorization = "Bearer hp_..." }
```

## 4. Create a page

You create a page by sending its text to the address it should have. Here we make a page called `Coffee`.

**Linux / macOS**

```bash
curl -X PUT $H/rest/v1/page/Coffee -H "$T" -H "Content-Type: application/json" \
  -d '{"source": "Coffee is a drink made from roasted beans. See also [[Tea]].\n\n== History ==\nCoffee was first brewed in Yemen.\n\n[[Category:Drinks]]", "comment": "First version"}'
```

**Windows (PowerShell)**

```powershell
$body = @{
  source  = "Coffee is a drink made from roasted beans. See also [[Tea]].`n`n== History ==`nCoffee was first brewed in Yemen.`n`n[[Category:Drinks]]"
  comment = "First version"
} | ConvertTo-Json
Invoke-RestMethod -Method Put -Uri "$H/rest/v1/page/Coffee" -Headers $T -ContentType "application/json" -Body $body
```

`source` is the text of the page; `comment` is a note about what you did, shown in the page's history.
The answer starts like this:

```json
{"content_model":"wikitext","created":true,"id":2,"key":"Coffee","latest":{"id":2, ...
```

`"created":true` means the page is new. (The numbers in the answers in this guide will be different on your wiki.) Now look at it: <http://localhost:3000/rest/v1/page/Coffee/html>

```html
<p>Coffee is a drink made from roasted beans. See also <a href="/wiki/Tea" title="Tea" class="new">Tea</a>.</p>
<h2 id="History">History</h2>
<p>Coffee was first brewed in Yemen.</p>
```

Hoepedia turned `[[Tea]]` into a link and `== History ==` into a heading. The link has `class="new"` because
no page called Tea exists yet, so your site can show it differently (wikis traditionally colour it red).

## 5. Change a page

Changing a page is the same request with new text. The old text is not lost.

**Linux / macOS**

```bash
curl -X PUT $H/rest/v1/page/Coffee -H "$T" -H "Content-Type: application/json" \
  -d '{"source": "Coffee is a hot drink made from roasted beans.", "comment": "Shorter"}'
```

**Windows (PowerShell)**

```powershell
$body = @{ source = "Coffee is a hot drink made from roasted beans."; comment = "Shorter" } | ConvertTo-Json
Invoke-RestMethod -Method Put -Uri "$H/rest/v1/page/Coffee" -Headers $T -ContentType "application/json" -Body $body
```

Every change is kept as a numbered **revision**. To see them, open
<http://localhost:3000/rest/v1/page/Coffee/history>. Each entry has an `id` (the revision number), the `comment`,
who made it and when. The newest is listed first:

```json
{"revisions": [
  {"id": 8, "comment": "Shorter", "user": "Admin", "timestamp": "2026-11-16T06:42:18Z", ...},
  {"id": 7, "comment": "First version", "user": "Admin", "timestamp": "2026-11-16T06:42:18Z", ...}
], ...}
```

Revision numbers count across the whole wiki, so yours will be different. To go back to an earlier version, take
its `id` from the history (here 7, the "First version") and make it current again:

**Linux / macOS**

```bash
curl -X POST $H/rest/v1/page/Coffee/restore -H "$T" -H "Content-Type: application/json" -d '{"revision": 7}'
```

**Windows (PowerShell)**

```powershell
Invoke-RestMethod -Method Post -Uri "$H/rest/v1/page/Coffee/restore" -Headers $T -ContentType "application/json" -Body '{"revision": 7}'
```

Restoring is itself a new revision, so nothing is ever lost: you can always go back again.

## 6. Find pages

| What you want | Address |
|---|---|
| Search the text of all pages | `/rest/v1/search/page?q=roasted` |
| Suggestions while someone types a title | `/rest/v1/search/title?q=Cof` |
| A list of all pages | `/rest/v1/pages` |
| The pages in a category | `/api.php?action=query&list=categorymembers&cmtitle=Category:Drinks` |

Try the first one in your browser: <http://localhost:3000/rest/v1/search/page?q=roasted>. The answer lists the
matching pages with a short excerpt in which the found word is marked.

## 7. Wiki text in one table

Pages are written in wiki text (the same markup Wikipedia uses). The most useful parts:

| You type | You get |
|---|---|
| `'''bold'''` | **bold** |
| `''italic''` | *italic* |
| `== Heading ==` | a heading (more `=` signs for smaller headings) |
| `[[Tea]]` | a link to the page Tea |
| `[[Tea\|a nice cup]]` | a link to Tea that reads "a nice cup" |
| `[https://example.org Example]` | a link to another website |
| `* item` | a bullet list item |
| `# item` | a numbered list item |
| `[[Category:Drinks]]` | puts the page in the category Drinks |
| `[[File:Cup.png\|thumb\|A cup]]` | shows an uploaded picture with a caption |
| `{{Infobox\|name=Coffee}}` | inserts the page `Template:Infobox`, a reusable piece of text |
| *an empty line* | starts a new paragraph |

Want to try markup without saving anything? Send it to the converter:

**Linux / macOS**

```bash
curl -X POST $H/rest/v1/transform/wikitext/to/html -H "Content-Type: application/json" \
  -d "{\"wikitext\": \"'''Hello''' from [[Coffee]]\"}"
```

**Windows (PowerShell)**

```powershell
Invoke-RestMethod -Method Post -Uri "$H/rest/v1/transform/wikitext/to/html" -ContentType "application/json" -Body (@{ wikitext = "'''Hello''' from [[Coffee]]" } | ConvertTo-Json)
```

## 8. Upload a picture

Send the file itself to the name it should have. This uploads `cup.png` from the current folder:

**Linux / macOS**

```bash
curl -X PUT $H/rest/v1/file/Cup.png -H "$T" --data-binary @cup.png
```

**Windows (PowerShell)**

```powershell
Invoke-RestMethod -Method Put -Uri "$H/rest/v1/file/Cup.png" -Headers $T -InFile .\cup.png
```

* The picture is now at `http://localhost:3000/rest/v1/file/Cup.png`.
* Add `?width=300` to get a smaller version: `http://localhost:3000/rest/v1/file/Cup.png?width=300`.
* Put `[[File:Cup.png|thumb|A cup of coffee]]` in a page to show it there.

Hoepedia accepts PNG, JPEG, GIF, WebP and SVG pictures, plus PDF, MP3, MP4, Ogg and WebM files, up to 10 MB each.
The ending of the name must match what the file really is.

PDFs, videos and sound files get previews too, if the programs **ffmpeg** and **Poppler** are installed (the Docker version
always has them; the window that starts the wiki shows `Previews` with what is available):

* `http://localhost:3000/rest/v1/file/Report.pdf?page=2` shows page 2 of a PDF as a picture.
* `http://localhost:3000/rest/v1/file/Talk.mp4?time=30` shows the video at 30 seconds.
* `http://localhost:3000/rest/v1/file/Song.mp3?width=400` draws the sound as a waveform.
* In a page, `[[File:Talk.mp4]]` becomes a video player and `[[File:Song.mp3]]` an audio player.

## 9. Let other people write

Give each person (or each app) their own token instead of sharing the admin token. This creates the user Maria:

**Linux / macOS**

```bash
curl -X POST $H/rest/v1/users -H "$T" -H "Content-Type: application/json" -d '{"name": "Maria"}'
```

**Windows (PowerShell)**

```powershell
Invoke-RestMethod -Method Post -Uri "$H/rest/v1/users" -Headers $T -ContentType "application/json" -Body '{"name": "Maria"}'
```

The answer contains Maria's `token`. **It is shown only this once**, so pass it on right away. Maria can create and
change pages and upload files; only an administrator can delete pages, block users or make backups.

If you would rather let anybody edit without a token, start the wiki with the setting `ANON_EDIT=true`
(see "Settings" below).

## 10. Delete, and undo a delete

Only administrators can delete. A deleted page disappears from reading and search, but nothing is thrown away:

**Linux / macOS**

```bash
curl -X DELETE $H/rest/v1/page/Coffee -H "$T"                 # delete
curl -X POST   $H/rest/v1/page/Coffee/undelete -H "$T"        # bring it back, with its history
```

**Windows (PowerShell)**

```powershell
Invoke-RestMethod -Method Delete -Uri "$H/rest/v1/page/Coffee" -Headers $T
Invoke-RestMethod -Method Post   -Uri "$H/rest/v1/page/Coffee/undelete" -Headers $T
```

## 11. Keep your wiki safe

**Make a backup** while the wiki is running:

**Linux / macOS**

```bash
curl -X POST $H/rest/v1/admin/backups -H "$T"
```

**Windows (PowerShell)**

```powershell
Invoke-RestMethod -Method Post -Uri "$H/rest/v1/admin/backups" -Headers $T
```

This writes a complete copy, for example `wiki-20261116-035634.db`, into `data/backups`. Copy that file somewhere
else now and then. To go back to a backup: stop the wiki, replace `data/wiki.db` with the backup file, start again.

**Keep the admin token secret.** Anyone who has it can change or delete everything.

**Before you put the wiki on the internet**, go through the
[security checklist](ADVANCED.md#going-public-security-checklist) in the advanced guide. Started directly, Hoepedia only answers
on your own computer, which is the safe default.

## Settings

Hoepedia runs without any settings. To change something, put a file called `.env` next to the program
(copy `.env.example`, which explains every setting), then restart. A few common ones (the
[configuration reference](ADVANCED.md#configuration-reference) lists all of them):

| Setting | What it does |
|---|---|
| `SITE_NAME=My Wiki` | the name of your wiki |
| `PORT=8080` | use another port (also possible as `hoepedia-api --port 8080`) |
| `ANON_EDIT=true` | let people edit without a token |
| `ANON_READ=false` | make the wiki private: reading needs a token too |
| `ARTICLE_PATH=https://example.org/wiki/{title}` | where links between pages should point on *your* website |
| `TIMEZONE=Europe/Berlin` | the time zone used for local dates |

## When something goes wrong

| What you see | What to do |
|---|---|
| `Cannot listen on 127.0.0.1:3000 ... Another program may be using this port` | Start with another port: `hoepedia-api --port 3001` (then use that port in every address). |
| `"code":"unauthorized"` | The request needs a token. Check the `Authorization: Bearer ...` header and that the token is complete. |
| `"code":"permissiondenied"` | The token is valid but that user may not do this (for example, only administrators delete pages). |
| `"code":"editconflict"` | Somebody changed the same words at the same time. The answer contains both versions with the differences marked; pick the right text and save again. |
| `"code":"filetypenotallowed"` or `"badextension"` | The file is not one of the accepted kinds, or its name ends in the wrong extension. |
| `"code":"nothumbnail"` on a PDF, video or sound file | Previews need ffmpeg and Poppler; the message says what to install. The file itself still works. |
| `"code":"ratelimited"` | Too many requests in a minute from one address. Wait a little, or raise `RATE_LIMIT_PER_MIN`. |
| The browser cannot open the address | Is the window with the wiki still open? Run `hoepedia-api check` to find out. |
| You lost the admin token | Run `hoepedia-api token` in the folder you start the wiki from. |

Every error answer has the same shape, so your app can rely on it:

```json
{"error": {"code": "unauthorized", "info": "The 'edit' right requires an API token (Authorization: Bearer ...)"}, "httpCode": 401}
```

## Where to go next

* **All addresses at a glance:** <http://localhost:3000/> lists them, and <http://localhost:3000/openapi.json> describes each
  one in a form that API tools (Postman, Insomnia, code generators) can load.
* **Templates and Lua modules:** [the wikitext engine](ADVANCED.md#the-wikitext-engine).
* **Thumbnails and previews:** [files](ADVANCED.md#files-thumbnails-and-previews).
* **Reacting to changes live** (bots, notifications): [feeds and watchlists](ADVANCED.md#feeds-and-watchlists).
* **Tools written for MediaWiki** can often talk to Hoepedia through `/api.php`, which uses the same parameter names
  ([details](ADVANCED.md#action-api-specifics)).
* **More than one server:** replicas, confirmed writes and automatic fail-over are in
  [running several servers](ADVANCED.md#running-several-servers).
