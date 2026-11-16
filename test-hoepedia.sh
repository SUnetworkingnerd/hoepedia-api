#!/usr/bin/env bash
# hoepedia-api smoke test: exercises every feature area against a running instance.
#
#   ADMIN_TOKEN=your_token ./test-hoepedia.sh [http://localhost:3000]
#
# Needs curl and python3. It creates pages and files whose names start with "Smoke"
# and removes them again at the end. It works against a primary or (with write
# forwarding, the default) against a replica.
set -u
H="${1:-http://localhost:3000}"; H="${H%/}"
T="${ADMIN_TOKEN:?set ADMIN_TOKEN to a sysop token}"
A="Authorization: Bearer $T"
CT="Content-Type: application/json"
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
OK=0; FAIL=0

check() { # name, expected substring, actual
  if printf '%s' "$3" | grep -qF -- "$2"; then OK=$((OK+1)); echo "  ok    $1"
  else FAIL=$((FAIL+1)); echo "  FAIL  $1"; echo "        expected to contain: $2"; echo "        got: $(printf '%s' "$3" | head -c 300)"; fi
}
absent() { # name, forbidden substring, actual
  if printf '%s' "$3" | grep -qF -- "$2"; then FAIL=$((FAIL+1)); echo "  FAIL  $1 (found: $2)"; else OK=$((OK+1)); echo "  ok    $1"; fi
}
put()  { curl -s -X PUT  -H "$A" -H "$CT" "$H$1" -d "$2"; }
post() { curl -s -X POST -H "$A" -H "$CT" "$H$1" -d "$2"; }
get()  { curl -s -H "$A" "$H$1"; }
code() { curl -s -o /dev/null -w '%{http_code}' "$@"; }
field() { python3 -c "import sys,json; d=json.load(sys.stdin); print($1)" 2>/dev/null; }
html() { post /rest/v1/transform/wikitext/to/html "$(python3 -c 'import json,sys; print(json.dumps({"wikitext": sys.argv[1]}))' "$1")"; }

echo "hoepedia-api smoke test -> $H"
if [ "$(code "$H/healthz")" != "200" ]; then echo "cannot reach $H/healthz"; exit 2; fi
if ! get "/api.php?action=query&meta=userinfo" | grep -q '"siteadmin"'; then echo "ADMIN_TOKEN is not a sysop token on this wiki"; exit 2; fi

echo "Basics"
check "healthz"                  'ok'              "$(curl -s $H/healthz)"
check "openapi"                  '"openapi"'       "$(curl -s $H/openapi.json)"
check "siteinfo"                 '"sitename"'      "$(curl -s "$H/api.php?action=query&meta=siteinfo")"
check "unknown route is JSON 404" '"httpCode":404'  "$(curl -s $H/nope)"
check "bad token rejected"       '401'             "$(code -H 'Authorization: Bearer wrong' $H/rest/v1/page/Smoke)"

echo "Pages"
check "create"                   '"created":true'  "$(put /rest/v1/page/Smoke_page '{"source":"{{SHORTDESC:Smoke test}}Smoke page about xylophones. [https://example.org/smoke link] [[Category:Smoke]]","comment":"smoke"}')"
check "read source"              'xylophones'      "$(get /rest/v1/page/Smoke_page)"
check "html"                     '<p>Smoke page'   "$(get /rest/v1/page/Smoke_page/html)"
check "summary"                  '"description":"Smoke test"' "$(get /rest/v1/page/Smoke_page/summary)"
check "action=parse"             '"parse"'         "$(get "/api.php?action=parse&page=Smoke_page")"
check "search"                   '"key":"Smoke_page"' "$(get "/rest/v1/search/page?q=xylophones")"
check "autocomplete"             'Smoke page'      "$(get "/rest/v1/search/title?q=Smoke")"
check "history"                  '"revisions"'     "$(get /rest/v1/page/Smoke_page/history)"

echo "Editing: merge, undo, restore"
R1=$(put /rest/v1/page/Smoke_merge '{"source":"The quick brown fox jumps over the lazy dog.","comment":"base"}' | field "d['latest']['id']")
R2=$(put /rest/v1/page/Smoke_merge "{\"source\":\"The quick red fox jumps over the lazy dog.\",\"comment\":\"a\",\"latest\":{\"id\":$R1}}" | field "d['latest']['id']")
check "stale edit merges by word" '"merged":true'  "$(put /rest/v1/page/Smoke_merge "{\"source\":\"The quick brown fox jumps over the sleepy dog.\",\"comment\":\"b\",\"latest\":{\"id\":$R1}}")"
check "merged text"              'quick red fox jumps over the sleepy dog' "$(get /rest/v1/page/Smoke_merge)"
CONFLICT=$(put /rest/v1/page/Smoke_merge "{\"source\":\"The quick blue fox jumps over the lazy dog.\",\"comment\":\"c\",\"latest\":{\"id\":$R1}}")
check "true conflict is 409"     'editconflict'    "$CONFLICT"
check "409 carries marked merge" '<<<<<<< current' "$CONFLICT"
check "undo one revision"        '"id"'            "$(post /rest/v1/page/Smoke_merge/undo "{\"revision\":$R2}")"
check "undo result"              'quick brown fox jumps over the sleepy dog' "$(get /rest/v1/page/Smoke_merge)"
check "restore old revision"     '"id"'            "$(post /rest/v1/page/Smoke_merge/restore "{\"revision\":$R1}")"
check "diff"                     '"diff"'          "$(get "/rest/v1/revision/$R1/compare/$R2")"

echo "Templates, parser functions, Lua"
put /rest/v1/page/Template:Smoke_hi '{"source":"Hello {{{1|world}}}!","comment":"smoke"}' >/dev/null
put /rest/v1/page/Module:Smoke_util '{"source":"return { twice = function(s) return s .. s end }","comment":"smoke"}' >/dev/null
put /rest/v1/page/Module:Smoke '{"source":"local p = {}\nlocal util = require(\"Module:Smoke util\")\nfunction p.hello(f) return \"Lua says \" .. util.twice(f.args[1] or \"x\") end\nfunction p.box(f) return tostring(mw.html.create(\"div\"):addClass(\"smoke-box\"):wikitext(f:expandTemplate{title=\"Smoke hi\", args={\"Lua\"}})) end\nfunction p.date(f) return mw.getContentLanguage():formatDate(\"j F Y\", \"2024-03-05\") end\nreturn p","comment":"smoke"}' >/dev/null
check "template"                 'Hello Ada!'      "$(html '{{Smoke hi|Ada}}')"
check "#expr / #if / #time"      '14 yes 5 March 2024' "$(html '{{#expr: 2 + 3 * 4}} {{#if: x | yes | no}} {{#time: j F Y | 2024-03-05}}')"
check "expandtemplates"          'Hello Bob!'      "$(get "/api.php?action=expandtemplates&text=%7B%7BSmoke_hi%7CBob%7D%7D")"
check "#invoke + require"        'Lua says abab'   "$(html '{{#invoke:Smoke|hello|ab}}')"
check "mw.html + expandTemplate" 'smoke-box\">Hello Lua!</div>' "$(html '{{#invoke:Smoke|box}}')"
check "mw.language"              '5 March 2024'    "$(html '{{#invoke:Smoke|date}}')"
put /rest/v1/page/Module:Smoke_uni '{"source":"local p = {} function p.f() return (mw.ustring.gsub(\"na\u00efve caf\u00e9\", \"%a+\", mw.ustring.upper)) .. \" \" .. mw.hash.hashValue(\"md5\", \"hello\") .. \" \" .. tostring(mw.site.stats.pages > 0) end return p","comment":"smoke"}' >/dev/null
check "unicode patterns, hash, stats" 'NAÏVE CAFÉ 5d41402abc4b2a76b9719d911017c592 true' "$(html '{{#invoke:Smoke uni|f}}')"
check "#timel"                   '2024'            "$(html '{{#timel: Y T | 2024-06-01}}')"
check "script errors are contained" 'Script error' "$(html '{{#invoke:Smoke|missing}}')"
X=$(html '<div class="ok" onclick="evil()" style="color:red;position:fixed">x</div><script>alert(1)</script><img src=x onerror=alert(1)>')
check "raw html kept when safe"  'class=\"ok\" style=\"color:red\"' "$X"
absent "no event handlers"       'onclick=\"'      "$X"
absent "no script tags"          '<script'         "$X"
check "html to wikitext"         "'''Bold'''"      "$(post /rest/v1/transform/html/to/wikitext '{"html":"<p><b>Bold</b> text</p>"}')"

echo "Files"
python3 - "$TMP" <<'PY'
import sys, struct, zlib
d = sys.argv[1]; w, h = 600, 300
raw = b''.join(b'\x00' + b''.join(bytes([x * 255 // w, y * 255 // h, 120]) for x in range(w)) for y in range(h))
ch = lambda t, b: struct.pack('>I', len(b)) + t + b + struct.pack('>I', zlib.crc32(t + b) & 0xffffffff)
open(d + '/smoke.png', 'wb').write(b'\x89PNG\r\n\x1a\n' + ch(b'IHDR', struct.pack('>IIBBBBB', w, h, 8, 2, 0, 0, 0)) + ch(b'IDAT', zlib.compress(raw)) + ch(b'IEND', b''))
open(d + '/smoke.svg', 'w').write('<svg xmlns="http://www.w3.org/2000/svg" width="64" height="48" onload="alert(1)"><script>alert(2)</script><rect width="64" height="48" fill="teal"/></svg>')
open(d + '/evil.png', 'w').write('<html><script>alert(1)</script></html>')
PY
check "upload png"               '"width":600'     "$(curl -s -X PUT -H "$A" --data-binary @$TMP/smoke.png $H/rest/v1/file/Smoke.png)"
check "file info"                '"mime":"image/png"' "$(get /rest/v1/file/Smoke.png/info)"
curl -s -H "$A" -o $TMP/thumb.png "$H/rest/v1/file/Smoke.png?width=150"
check "thumbnail is smaller"     'yes'             "$(python3 -c "import os,struct; d=open('$TMP/thumb.png','rb').read(); w,h=struct.unpack('>II',d[16:24]); print('yes' if d[:4]==b'\x89PNG' and (w,h)==(160,80) and len(d)<os.path.getsize('$TMP/smoke.png') else (w,h,len(d)))")"
check "imageinfo thumburl"       '"thumbwidth":300' "$(get "/api.php?action=query&titles=File:Smoke.png&prop=imageinfo&iiurlwidth=300")"
check "fake image rejected"      'filetypenotallowed' "$(curl -s -X PUT -H "$A" --data-binary @$TMP/evil.png $H/rest/v1/file/Smoke_evil.png)"
SVG_UP=$(curl -s -X PUT -H "$A" --data-binary @$TMP/smoke.svg $H/rest/v1/file/Smoke.svg)
if printf '%s' "$SVG_UP" | grep -q 'SVG uploads are disabled'; then echo "  skip  SVG (ALLOW_SVG=false)"; else
  check "upload svg"             '"mime":"image/svg+xml"' "$SVG_UP"
  SVG=$(get /rest/v1/file/Smoke.svg)
  check "svg drawing kept"       '<rect'           "$SVG"
  absent "svg script removed"    'script'          "$SVG"
  absent "svg handler removed"   'onload'          "$SVG"
fi
if ! printf '%s' "$SVG_UP" | grep -q 'SVG uploads are disabled'; then
  curl -s -H "$A" -o $TMP/svg.png "$H/rest/v1/file/Smoke.svg?width=200"
  check "svg rasterised to png"  'yes'             "$(python3 -c "import struct; d=open('$TMP/svg.png','rb').read(); print('yes' if d[:4]==b'\x89PNG' and struct.unpack('>II',d[16:24])==(200,150) else d[:20])")"
fi
put /rest/v1/page/Smoke_gallery '{"source":"[[File:Smoke.png|thumb|200px|A caption]]","comment":"smoke"}' >/dev/null
check "thumb embed renders figure" '<figure class="mw-thumb' "$(get /rest/v1/page/Smoke_gallery/html)"
check "list files"               'Smoke.png'       "$(get "/rest/v1/files?prefix=Smoke")"

echo "Action API: lists, props, generators"
check "categorymembers"          '"title":"Smoke page"' "$(get "/api.php?action=query&list=categorymembers&cmtitle=Category:Smoke")"
check "extlinks + contributors"  'https://example.org/smoke' "$(get "/api.php?action=query&titles=Smoke_page&prop=extlinks|contributors|info")"
check "exturlusage"              '"title":"Smoke page"' "$(get "/api.php?action=query&list=exturlusage&euquery=example.org/smoke")"
check "images + pageimages"      '"thumbnail"'     "$(get "/api.php?action=query&titles=Smoke_gallery&prop=images|pageimages")"
check "imageusage"               '"title":"Smoke gallery"' "$(get "/api.php?action=query&list=imageusage&iutitle=File:Smoke.png")"
check "pageswithprop"            '"value":"Smoke test"' "$(get "/api.php?action=query&list=pageswithprop&pwppropname=description&pwplimit=500")"
check "generator=categorymembers" '"extract"'      "$(get "/api.php?action=query&generator=categorymembers&gcmtitle=Category:Smoke&prop=extracts&exintro=1&explaintext=1")"
check "generator=images"         '"imageinfo"'     "$(get "/api.php?action=query&generator=images&titles=Smoke_gallery&prop=imageinfo")"
check "generator=search"         '"pages"'         "$(get "/api.php?action=query&generator=search&gsrsearch=xylophones&prop=info")"
check "opensearch"               'Smoke'           "$(get "/api.php?action=opensearch&search=Smoke")"
check "allimages"                '"allimages"'     "$(get "/api.php?action=query&list=allimages&aiprefix=Smoke")"
check "querypage"                '"results"'       "$(get "/api.php?action=query&list=querypage&qppage=Uncategorizedpages")"
check "allrevisions"             '"revisions"'     "$(get "/api.php?action=query&list=allrevisions&arvlimit=2")"
check "watch"                    '"watched":"Smoke page"' "$(curl -s -X PUT -H "$A" $H/rest/v1/watchlist/Smoke_page)"
check "watchlist"                '"title":"Smoke page"' "$(get /rest/v1/watchlist)"
check "list=watchlistraw"        '"title":"Smoke page"' "$(get "/api.php?action=query&list=watchlistraw&wrlimit=500")"
check "unwatch"                  '"unwatched"'     "$(curl -s -X DELETE -H "$A" $H/rest/v1/watchlist/Smoke_page)"
check "action=edit"              '"result":"Success"' "$(curl -s -X POST -H "$A" "$H/api.php" --data-urlencode action=edit --data-urlencode title=Smoke_action --data-urlencode text="via action api" --data-urlencode summary=smoke)"

echo "Interwiki"
check "set prefix"               '"prefix":"smokewiki"' "$(put /rest/v1/interwiki/smokewiki '{"url":"https://smoke.example/wiki/$1"}')"
put /rest/v1/interwiki/smokelang '{"url":"https://lang.smoke.example/wiki/$1","language":true}' >/dev/null
put /rest/v1/page/Smoke_lang '{"source":"Text [[smokelang:Autre page]]","comment":"smoke"}' >/dev/null
check "language links"           '"lang":"smokelang","title":"Autre page"' "$(get "/api.php?action=query&titles=Smoke_lang&prop=langlinks")"
curl -s -o /dev/null -X DELETE -H "$A" $H/rest/v1/interwiki/smokelang
check "interwiki link"           'https://smoke.example/wiki/Thing' "$(html '[[smokewiki:Thing]]')"
check "interwikimap"             '"smokewiki"'     "$(get "/api.php?action=query&meta=siteinfo&siprop=interwikimap")"
check "federated search answers" '"interwiki":'    "$(get "/rest/v1/search/page?q=xylophones&interwiki=1")"
check "remove prefix"            '"deleted"'       "$(curl -s -X DELETE -H "$A" $H/rest/v1/interwiki/smokewiki)"

echo "Feeds, backups, replication, maintenance"
CUR=$(get /rest/v1/feed/changes | field "d['cursor']")
put /rest/v1/page/Smoke_feed '{"source":"feed","comment":"smoke"}' >/dev/null
check "change feed resumes"      'Smoke feed'      "$(get "/rest/v1/feed/changes?cursor=$CUR")"
check "sse stream"               'event:'          "$(curl -s -N --max-time 2 -H "$A" -H 'Last-Event-ID: 0-0' "$H/rest/v1/stream/changes" 2>/dev/null | head -c 2000)"
BK=$(post /rest/v1/admin/backups '{}')
check "backup created"           '"name":"wiki-'   "$BK"
BKN=$(printf '%s' "$BK" | field "d['name']")
check "backup listed"            "$BKN"            "$(get /rest/v1/admin/backups)"
check "backup downloads as sqlite" 'SQLite format 3' "$(get "/rest/v1/admin/backups/$BKN" | head -c 15 | tr -d '\0')"
curl -s -X DELETE -H "$A" "$H/rest/v1/admin/backups/$BKN" >/dev/null
check "replication status"       '"role":'         "$(get /rest/v1/replication/status)"
check "replication changes"      '"changes":['     "$(get "/rest/v1/replication/changes?since=999999999")"
check "replication needs sysop"  '401'             "$(code "$H/rest/v1/replication/changes?since=0")"
check "metrics"                  'hoepedia'        "$(curl -s $H/metrics)"

echo "Cleanup"
for p in Smoke_page Smoke_merge Smoke_lang Template:Smoke_hi Module:Smoke Module:Smoke_util Module:Smoke_uni Smoke_gallery Smoke_action Smoke_feed File:Smoke.png File:Smoke.svg; do
  curl -s -o /dev/null -X DELETE -H "$A" "$H/rest/v1/page/$p"
done
curl -s -o /dev/null -X DELETE -H "$A" "$H/rest/v1/file/Smoke.png"; curl -s -o /dev/null -X DELETE -H "$A" "$H/rest/v1/file/Smoke.svg"
check "pages removed"            '404'             "$(code -H "$A" $H/rest/v1/page/Smoke_page)"

echo
echo "Result: $OK passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
