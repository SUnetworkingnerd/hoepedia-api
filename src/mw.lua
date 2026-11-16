-- The `mw` library available to Module: pages. Loaded into every sandbox before the module runs.
-- `__h` holds the host functions implemented in lua.rs; it is hidden from modules afterwards.
local h = __h
__h = nil
local PAGE, SITE, VERSION, LANG = __PAGE, __SITE, __VERSION, __LANG
local rawload = load
load, loadstring, dofile, loadfile, collectgarbage, print = nil, nil, nil, nil, nil, nil

mw = {}

-- ---------------------------------------------------------------- utilities
function mw.clone(v, seen)
  if type(v) ~= "table" then return v end
  seen = seen or {}
  if seen[v] then return seen[v] end
  local c = {}
  seen[v] = c
  for k, x in pairs(v) do c[mw.clone(k, seen)] = mw.clone(x, seen) end
  return setmetatable(c, getmetatable(v))
end
function mw.log() end
function mw.logObject() end
function mw.allToString(...)
  local t = { ... }
  for i = 1, select("#", ...) do t[i] = tostring(t[i]) end
  return table.concat(t, "\t")
end
function mw.dumpObject(o) return h.jsonEncode(o) end
function mw.addWarning() end
function mw.isSubsting() return false end

-- ---------------------------------------------------------------- require / loadData
local loaded, data, loading, nloaded = {}, {}, {}, 0
local builtin = {
  strict = function() return true end,
  libraryUtil = function()
    local function check(name, argIdx, arg, expect, nilOk)
      if arg == nil and nilOk then return end
      if type(arg) ~= expect then
        error(string.format("bad argument #%d to '%s' (%s expected, got %s)", argIdx, name, expect, type(arg)), 3)
      end
    end
    return {
      checkType = check,
      checkTypeMulti = function() end,
      checkTypeForIndex = function() end,
      checkTypeForNamedArg = function(name, argName, arg, expect, nilOk) check(name, 1, arg, expect, nilOk) end,
      makeCheckSelfFunction = function() return function() end end,
    }
  end,
}
local function loadModule(name)
  if type(name) ~= "string" then error("module name must be a string", 3) end
  local src = h.moduleSource(name)
  if not src then error("module '" .. name .. "' not found", 3) end
  nloaded = nloaded + 1
  if nloaded > 60 then error("too many modules loaded", 3) end
  if loading[name] then error("loop or previous error loading module '" .. name .. "'", 3) end
  loading[name] = true
  local fn, err = rawload(src, "=" .. name, "t")
  if not fn then error(err, 0) end
  local res = fn()
  loading[name] = nil
  return res
end
function require(name)
  if builtin[name] then
    if loaded[name] == nil then loaded[name] = builtin[name]() end
    return loaded[name]
  end
  if loaded[name] == nil then
    local res = loadModule(name)
    if res == nil then res = true end
    loaded[name] = res
  end
  return loaded[name]
end
function mw.loadData(name)
  if data[name] == nil then
    local res = loadModule(name)
    if type(res) ~= "table" then error("Module '" .. name .. "' for mw.loadData must return a table", 2) end
    data[name] = res
  end
  return data[name]
end
function mw.loadJsonData(name)
  local t = h.content(name)
  if not t then error("page '" .. tostring(name) .. "' not found", 2) end
  return h.jsonDecode(t)
end

-- ---------------------------------------------------------------- mw.text
local text = {}
mw.text = text
function text.trim(s, charset)
  s = tostring(s)
  if charset then
    return (s:gsub("^[" .. charset .. "]+", ""):gsub("[" .. charset .. "]+$", ""))
  end
  return (s:gsub("^%s+", ""):gsub("%s+$", ""))
end
function text.gsplit(s, pattern, plain)
  s = tostring(s)
  pattern = pattern or ","
  local pos, done = 1, false
  return function()
    if done then return nil end
    if pattern == "" then
      if pos > #s then done = true return nil end
      local e = utf8.offset(s, 2, pos) or (#s + 1)
      local piece = s:sub(pos, e - 1)
      pos = e
      return piece
    end
    local a, b = s:find(pattern, pos, plain)
    if not a or b < a then
      done = true
      return s:sub(pos)
    end
    local piece = s:sub(pos, a - 1)
    pos = b + 1
    return piece
  end
end
function text.split(s, pattern, plain)
  local t = {}
  for piece in text.gsplit(s, pattern, plain) do t[#t + 1] = piece end
  return t
end
function text.nowiki(s)
  return (tostring(s):gsub("[%[%]{}|<>'=*#:;&\"]", function(c) return "&#" .. c:byte() .. ";" end))
end
local ents = { ["&"] = "&amp;", ["<"] = "&lt;", [">"] = "&gt;", ['"'] = "&quot;", ["\194\160"] = "&nbsp;" }
function text.encode(s, charset)
  return (tostring(s):gsub("[" .. (charset or '<>&"\194\160') .. "]", function(c) return ents[c] or ("&#" .. utf8.codepoint(c) .. ";") end))
end
local named = { amp = "&", lt = "<", gt = ">", quot = '"', nbsp = "\194\160", apos = "'" }
function text.decode(s)
  return (tostring(s):gsub("&(#?x?)(%w+);", function(kind, body)
    if kind == "" then return named[body] end
    local n = tonumber(body, kind == "#x" and 16 or 10)
    if n and n > 0 and n < 0x110000 then return utf8.char(n) end
  end))
end
function text.tag(name, attrs, content)
  if type(name) == "table" then name, attrs, content = name.name, name.attrs, name.content end
  local out = { "<", name }
  if attrs then
    local keys = {}
    for k in pairs(attrs) do keys[#keys + 1] = tostring(k) end
    table.sort(keys)
    for _, k in ipairs(keys) do
      local v = attrs[k]
      if v == true then out[#out + 1] = " " .. k
      elseif v ~= false and v ~= nil then out[#out + 1] = " " .. k .. '="' .. text.encode(tostring(v)) .. '"' end
    end
  end
  if content == nil then out[#out + 1] = ">"
  elseif content == false then out[#out + 1] = " />"
  else out[#out + 1] = ">" .. tostring(content) .. "</" .. name .. ">" end
  return table.concat(out)
end
function text.listToText(list, sep, conj)
  sep, conj = sep or ", ", conj or " and "
  local n = #list
  if n == 0 then return "" end
  if n == 1 then return tostring(list[1]) end
  local t = {}
  for i = 1, n - 1 do t[i] = tostring(list[i]) end
  return table.concat(t, sep) .. conj .. tostring(list[n])
end
function text.truncate(s, len, ellipsis, adjust)
  s = tostring(s)
  ellipsis = ellipsis or "..."
  local n = utf8.len(s) or #s
  if math.abs(len) >= n then return s end
  if len >= 0 then return h.usub(s, 1, len) .. ellipsis end
  return ellipsis .. h.usub(s, n + len + 1, n)
end
function text.unstrip(s) return s end
function text.unstripNoWiki(s) return s end
function text.killMarkers(s) return s end
function text.stripTags(s) return (tostring(s):gsub("<[^>]*>", "")) end
text.jsonEncode = function(v) return h.jsonEncode(v) end
text.jsonDecode = function(s) return h.jsonDecode(tostring(s)) end
text.JSON_PRESERVE_KEYS, text.JSON_TRY_FIXING, text.JSON_PRETTY = 1, 2, 4

-- ---------------------------------------------------------------- mw.ustring
local ustring = {}
mw.ustring = ustring
ustring.len = function(s) return utf8.len(tostring(s)) end
ustring.sub = function(s, i, j) return h.usub(tostring(s), i or 1, j or -1) end
ustring.upper = function(s) return h.upper(tostring(s)) end
ustring.lower = function(s) return h.lower(tostring(s)) end
ustring.char, ustring.codepoint, ustring.byteoffset = utf8.char, utf8.codepoint, utf8.offset
ustring.format, ustring.rep, ustring.byte = string.format, string.rep, string.byte
-- Pattern functions work on characters and use Unicode character classes (see lpat.rs).
function ustring.find(s, pattern, init, plain)
  return h.ufind(tostring(s), tostring(pattern), init or 1, plain and true or false)
end
function ustring.match(s, pattern, init)
  s = tostring(s)
  local r = table.pack(h.ufind(s, tostring(pattern), init or 1, false))
  if r[1] == nil then return nil end
  if r.n > 2 then return table.unpack(r, 3, r.n) end
  return h.usub(s, r[1], r[2])
end
function ustring.gmatch(s, pattern)
  s = tostring(s)
  local ms, i = h.umatches(s, tostring(pattern)), 0
  return function()
    i = i + 1
    local m = ms[i]
    if not m then return nil end
    if #m > 2 then return table.unpack(m, 3) end
    return s:sub(m[1], m[2])
  end
end
function ustring.gsub(s, pattern, repl, n)
  s = tostring(s)
  local kind = type(repl)
  if kind == "number" then repl, kind = tostring(repl), "string" end
  if kind ~= "string" and kind ~= "table" and kind ~= "function" then
    error("bad argument #3 to 'gsub' (string/function/table expected)", 2)
  end
  local ms = h.umatches(s, tostring(pattern), n)
  local out, pos = {}, 1
  for _, m in ipairs(ms) do
    out[#out + 1] = s:sub(pos, m[1] - 1)
    local whole = s:sub(m[1], m[2])
    local r
    if kind == "string" then
      r = repl:gsub("%%(.)", function(c)
        if c == "0" then return whole end
        if c:match("%d") then
          local v = m[2 + tonumber(c)]
          if v == nil then
            if c == "1" then return whole end
            error("invalid capture index %" .. c .. " in replacement string", 3)
          end
          return tostring(v)
        end
        return c
      end)
    elseif kind == "table" then
      r = repl[m[3] ~= nil and m[3] or whole]
    elseif m[3] ~= nil then
      r = repl(table.unpack(m, 3))
    else
      r = repl(whole)
    end
    if r == nil or r == false then
      r = whole
    elseif type(r) ~= "string" and type(r) ~= "number" then
      error("invalid replacement value (a " .. type(r) .. ")", 2)
    end
    out[#out + 1] = tostring(r)
    pos = m[2] + 1
  end
  out[#out + 1] = s:sub(pos)
  return table.concat(out), #ms
end
ustring.gcodepoint = function(s)
  local it, st, i = utf8.codes(tostring(s))
  return function()
    local c
    i, c = it(st, i)
    return c
  end
end
ustring.isutf8 = function(s) return utf8.len(tostring(s)) ~= nil end
ustring.toNFC = function(s) return s end
ustring.toNFD = function(s) return s end
ustring.toNFKC = function(s) return s end
ustring.toNFKD = function(s) return s end
ustring.maxPatternLength, ustring.maxStringLength = 10000, 2097152

-- ---------------------------------------------------------------- mw.uri
mw.uri = {}
function mw.uri.encode(s, enctype)
  s = tostring(s)
  local sp = (enctype == "PATH" and "%%20") or (enctype == "WIKI" and "_") or "+"
  return (s:gsub("[^%w%-_%.~ ]", function(c) return string.format("%%%02X", c:byte()) end):gsub(" ", sp))
end
function mw.uri.decode(s, enctype)
  s = tostring(s)
  if enctype ~= "PATH" then s = s:gsub(enctype == "WIKI" and "_" or "%+", " ") end
  return (s:gsub("%%(%x%x)", function(x) return string.char(tonumber(x, 16)) end))
end
function mw.uri.anchorEncode(s) return (tostring(s):gsub("^%s+", ""):gsub("%s+$", ""):gsub("%s+", "_")) end
function mw.uri.localUrl(page, query) return h.url(tostring(page)) .. (query and ("?" .. mw.uri.buildQueryString(query)) or "") end
mw.uri.fullUrl = mw.uri.localUrl
mw.uri.canonicalUrl = mw.uri.localUrl
function mw.uri.buildQueryString(t)
  if type(t) ~= "table" then return tostring(t) end
  local keys, out = {}, {}
  for k in pairs(t) do keys[#keys + 1] = tostring(k) end
  table.sort(keys)
  for _, k in ipairs(keys) do out[#out + 1] = mw.uri.encode(k) .. "=" .. mw.uri.encode(tostring(t[k])) end
  return table.concat(out, "&")
end

-- ---------------------------------------------------------------- mw.title
local titleMT = {}
local function wrapTitle(info)
  if not info then return nil end
  return setmetatable(info, titleMT)
end
local titleMethods = {}
function titleMethods:getContent() return h.content(self.prefixedText) end
function titleMethods:fullUrl(q) return mw.uri.localUrl(self.prefixedText, q) end
titleMethods.localUrl, titleMethods.canonicalUrl = titleMethods.fullUrl, titleMethods.fullUrl
function titleMethods:subPageTitle(t) return mw.title.new(self.prefixedText .. "/" .. t) end
function titleMethods:inNamespace(ns) return self.namespace == ns or self.nsText == ns end
function titleMethods:isSubpageOf(other) return self.prefixedText:sub(1, #other.prefixedText + 1) == other.prefixedText .. "/" end
function titleMethods:partialUrl() return (self.text:gsub(" ", "_")) end
titleMT.__index = function(t, k)
  if titleMethods[k] then return titleMethods[k] end
  if k == "exists" then
    local v = h.exists(t.prefixedText)
    rawset(t, "exists", v)
    return v
  elseif k == "isRedirect" then
    local c = h.content(t.prefixedText)
    return c ~= nil and c:lower():match("^%s*#redirect") ~= nil
  elseif k == "id" then
    return h.exists(t.prefixedText) and 1 or 0
  elseif k == "contentModel" then
    return t.namespace == 828 and "Scribunto" or "wikitext"
  elseif k == "talkPageTitle" or k == "basePageTitle" or k == "rootPageTitle" then
    return nil
  end
end
titleMT.__tostring = function(t) return t.prefixedText end
titleMT.__eq = function(a, b) return a.prefixedText == b.prefixedText end
titleMT.__lt = function(a, b) return a.prefixedText < b.prefixedText end
mw.title = {}
function mw.title.new(t, ns)
  if type(t) == "number" then return nil end
  return wrapTitle(h.titleInfo(tostring(t), type(ns) == "number" and ns or 0, type(ns) == "string" and ns or nil))
end
function mw.title.makeTitle(ns, t, fragment)
  local info = h.titleInfo(tostring(t), type(ns) == "number" and ns or 0, type(ns) == "string" and ns or nil, true)
  if info and fragment then info.fragment = fragment end
  return wrapTitle(info)
end
function mw.title.getCurrentTitle() return mw.title.new(PAGE) end
function mw.title.equals(a, b) return a.prefixedText == b.prefixedText end
function mw.title.compare(a, b)
  if a.prefixedText == b.prefixedText then return 0 end
  return a.prefixedText < b.prefixedText and -1 or 1
end

-- ---------------------------------------------------------------- mw.site
mw.site = { siteName = SITE, server = "", scriptPath = "", currentVersion = VERSION, namespaces = h.namespaces() }
-- Statistics are read from the wiki when first used.
mw.site.stats = setmetatable({
  pagesInCategory = function(cat, which)
    local t = { all = h.stat("cat_all", cat), pages = h.stat("cat_pages", cat), files = h.stat("cat_files", cat), subcats = h.stat("cat_subcats", cat) }
    if which == "*" then return t end
    return t[which or "all"]
  end,
  pagesInNamespace = function(ns) return h.stat("ns", tostring(ns)) end,
  usersInGroup = function(group) return h.stat("group", tostring(group)) end,
  interwikiMap = function() return {} end,
}, { __index = function(t, k)
  local key = ({ pages = "pages", articles = "articles", files = "files", edits = "edits", users = "users", activeUsers = "activeusers", admins = "admins" })[k]
  if not key then return nil end
  local v = h.stat(key)
  rawset(t, k, v)
  return v
end })
for _, ns in pairs(mw.site.namespaces) do mw.site.namespaces[ns.name] = ns end

-- ---------------------------------------------------------------- mw.language
local lang = {}
lang.__index = lang
function lang:getCode() return LANG end
function lang:toBcp47Code() return LANG end
function lang:isRTL() return false end
function lang:getDir() return "ltr" end
function lang:lc(s) return h.lower(tostring(s)) end
function lang:uc(s) return h.upper(tostring(s)) end
function lang:lcfirst(s) s = tostring(s) return h.lower(h.usub(s, 1, 1)) .. h.usub(s, 2, -1) end
function lang:ucfirst(s) s = tostring(s) return h.upper(h.usub(s, 1, 1)) .. h.usub(s, 2, -1) end
function lang:caseFold(s) return h.lower(tostring(s)) end
function lang:formatNum(n, opts)
  local s = tostring(n)
  if opts and opts.noCommafy then return s end
  local sign, int, frac = s:match("^(-?)(%d+)(.*)$")
  if not int then return s end
  int = int:reverse():gsub("(%d%d%d)", "%1,"):reverse():gsub("^,", "")
  return sign .. int .. frac
end
function lang:parseFormattedNumber(s) return tonumber((tostring(s):gsub(",", ""))) end
function lang:formatDate(format, timestamp, localTime)
  local r = h.formatDate(tostring(format), timestamp and tostring(timestamp) or "", localTime and true or false)
  if not r then error("bad argument #2 to 'formatDate': invalid timestamp '" .. tostring(timestamp) .. "'", 2) end
  return r
end
function lang:formatDuration(seconds)
  local parts = {}
  for _, u in ipairs({ { "day", 86400 }, { "hour", 3600 }, { "minute", 60 }, { "second", 1 } }) do
    local n = math.floor(seconds / u[2])
    if n > 0 then
      parts[#parts + 1] = n .. " " .. u[1] .. (n == 1 and "" or "s")
      seconds = seconds - n * u[2]
    end
  end
  return text.listToText(parts)
end
function lang:plural(n, ...)
  local forms = { ... }
  if type(forms[1]) == "table" then forms = forms[1] end
  return (tonumber(n) == 1) and forms[1] or (forms[2] or forms[1])
end
lang.convertPlural = lang.plural
function lang:gender(_, ...) local f = { ... } if type(f[1]) == "table" then f = f[1] end return f[3] or f[1] end
function lang:getFallbackLanguages() return {} end
mw.language = {}
function mw.language.new() return setmetatable({}, lang) end
mw.language.getContentLanguage = mw.language.new
mw.getContentLanguage = mw.language.new
mw.getLanguage = mw.language.new
function mw.language.getContentLanguage() return setmetatable({}, lang) end
function mw.language.fetchLanguageName(code) return code == "en" and "English" or "" end
function mw.language.isKnownLanguageTag(code) return code == "en" or code == LANG end
function mw.language.isSupportedLanguage(code) return code == "en" or code == LANG end
function mw.language.isValidCode(code) return type(code) == "string" and code:match("^[%a-]+$") ~= nil end

-- ---------------------------------------------------------------- mw.hash
mw.hash = {}
function mw.hash.hashValue(algo, value) return h.hash(tostring(algo), tostring(value)) end
function mw.hash.listAlgorithms() return { "md5", "sha1", "sha224", "sha256", "sha384", "sha512" } end

-- ---------------------------------------------------------------- mw.message
-- There is no interface-message store: named messages render as their key, raw messages
-- get their $1, $2 ... parameters substituted.
local message = {}
message.__index = message
local function newMessage(text, key, params)
  return setmetatable({ text = text, key = key, _params = params or {} }, message)
end
function message:params(...)
  local p = { ... }
  if type(p[1]) == "table" and #p == 1 then p = p[1] end
  for _, v in ipairs(p) do
    if type(v) == "table" then v = v.raw or v.num or "" end
    self._params[#self._params + 1] = tostring(v)
  end
  return self
end
message.rawParams, message.numParams = message.params, message.params
function message:inLanguage() return self end
function message:useDatabase() return self end
function message:plain()
  return (self.text:gsub("%$(%d+)", function(n) return self._params[tonumber(n)] end))
end
message.__tostring = message.plain
function message:exists() return self.key == nil end
function message:isBlank() return self:plain() == "" end
function message:isDisabled() local t = self:plain() return t == "" or t == "-" end
mw.message = {}
function mw.message.new(key, ...) return newMessage("\226\167\188" .. tostring(key) .. "\226\167\189", key):params(...) end
function mw.message.newFallbackSequence(...) return mw.message.new((...)) end
function mw.message.newRawMessage(text, ...) return newMessage(tostring(text)):params(...) end
function mw.message.rawParam(v) return { raw = tostring(v) } end
function mw.message.numParam(v) return { num = tostring(v) } end
function mw.message.getDefaultLanguage() return mw.language.new() end

-- ---------------------------------------------------------------- mw.html
mw.html = {}
local html = {}
html.__index = html
html.__tostring = function(self)
  local out = {}
  self:_build(out)
  return table.concat(out)
end
local void = { br = true, hr = true, img = true, wbr = true }
function html:_build(out)
  if self.tagName then
    out[#out + 1] = "<" .. self.tagName
    for _, a in ipairs(self.attrs) do out[#out + 1] = " " .. a[1] .. '="' .. text.encode(tostring(a[2])) .. '"' end
    if #self.styles > 0 then
      local css = {}
      for _, st in ipairs(self.styles) do css[#css + 1] = type(st) == "string" and st or (st[1] .. ":" .. tostring(st[2])) end
      out[#out + 1] = ' style="' .. text.encode(table.concat(css, ";")) .. '"'
    end
    if self.selfClosing or void[self.tagName] then out[#out + 1] = " />" return end
    out[#out + 1] = ">"
  end
  for _, n in ipairs(self.nodes) do
    if type(n) == "table" then n:_build(out) else out[#out + 1] = tostring(n) end
  end
  if self.tagName then out[#out + 1] = "</" .. self.tagName .. ">" end
end
function mw.html.create(tagName, args)
  if tagName ~= nil and (type(tagName) ~= "string" or not tagName:match("^[%a][%w]*$")) then error("invalid tag name", 2) end
  return setmetatable({ tagName = tagName ~= "" and tagName or nil, attrs = {}, styles = {}, nodes = {},
    selfClosing = args and args.selfClosing, parent = args and args.parent }, html)
end
function html:node(n) if n ~= nil then self.nodes[#self.nodes + 1] = n end return self end
function html:wikitext(...)
  for i = 1, select("#", ...) do
    local v = select(i, ...)
    if v == nil then break end
    self.nodes[#self.nodes + 1] = tostring(v)
  end
  return self
end
function html:newline() self.nodes[#self.nodes + 1] = "\n" return self end
function html:tag(name, args)
  local child = mw.html.create(name, { selfClosing = args and args.selfClosing, parent = self })
  self.nodes[#self.nodes + 1] = child
  return child
end
function html:getAttr(name)
  for _, a in ipairs(self.attrs) do if a[1] == name then return a[2] end end
end
function html:attr(name, value)
  if type(name) == "table" then
    for k, v in pairs(name) do self:attr(k, v) end
    return self
  end
  if type(name) ~= "string" or not name:match("^[%a_:][%w_:%.%-]*$") then error("invalid attribute name", 2) end
  for i, a in ipairs(self.attrs) do
    if a[1] == name then
      if value == nil then table.remove(self.attrs, i) else a[2] = value end
      return self
    end
  end
  if value ~= nil then self.attrs[#self.attrs + 1] = { name, value } end
  return self
end
function html:addClass(c)
  if c == nil then return self end
  local cur = self:getAttr("class")
  return self:attr("class", cur and (cur .. " " .. tostring(c)) or tostring(c))
end
function html:css(name, value)
  if type(name) == "table" then
    for k, v in pairs(name) do self:css(k, v) end
    return self
  end
  if value ~= nil then self.styles[#self.styles + 1] = { name, value } end
  return self
end
function html:cssText(css) if css ~= nil then self.styles[#self.styles + 1] = tostring(css) end return self end
function html:done() return self.parent or self end
function html:allDone()
  local n = self
  while n.parent do n = n.parent end
  return n
end

-- ---------------------------------------------------------------- os (time only)
os = {
  time = function(t)
    if type(t) == "table" then
      return h.parseDate(string.format("%04d-%02d-%02d %02d:%02d:%02d", t.year, t.month, t.day, t.hour or 12, t.min or 0, t.sec or 0))
    end
    return h.now()
  end,
  clock = function() return h.clock() end,
  difftime = function(a, b) return a - (b or 0) end,
  date = function(fmt, t)
    fmt = fmt or "%c"
    t = t or h.now()
    if fmt:sub(1, 1) == "!" then fmt = fmt:sub(2) end
    if fmt:sub(1, 2) == "*t" then
      local f = function(c) return tonumber(h.formatDate(c, "@" .. t)) end
      return { year = f("Y"), month = f("n"), day = f("j"), hour = f("G"), min = tonumber(h.formatDate("i", "@" .. t)),
        sec = tonumber(h.formatDate("s", "@" .. t)), wday = f("w") + 1, yday = f("z") + 1, isdst = false }
    end
    local map = { Y = "Y", m = "m", d = "d", H = "H", M = "i", S = "s", y = "y", B = "F", b = "M", A = "l", a = "D", j = "z", p = "A",
      c = "D M j H:i:s Y", x = "m/d/y", X = "H:i:s", F = "Y-m-d", T = "H:i:s", e = "j", I = "h", s = "U" }
    return (fmt:gsub("%%(.)", function(c)
      if c == "%" then return "%" end
      if map[c] then return h.formatDate(map[c], "@" .. t) end
      return "%" .. c
    end))
  end,
}

-- ---------------------------------------------------------------- frames
local frameMT = {}
frameMT.__index = frameMT
local function strArgs(args)
  local out = {}
  for k, v in pairs(args or {}) do
    if type(v) == "boolean" then v = v and "1" or "" end
    out[tostring(k)] = tostring(v)
  end
  return out
end
local function makeFrame(args, parent, title)
  return setmetatable({ args = args, __parent = parent, __title = title }, frameMT)
end
__makeFrame = makeFrame
function frameMT:getParent() return self.__parent end
function frameMT:getTitle() return self.__title end
function frameMT:preprocess(t)
  if type(t) == "table" then t = t.text end
  return h.preprocess(tostring(t))
end
function frameMT:expandTemplate(opt)
  if type(opt) ~= "table" or not opt.title then error("frame:expandTemplate: a title is required", 2) end
  local r, err = h.expandTemplate(tostring(opt.title), strArgs(opt.args))
  if not r then error("frame:expandTemplate: " .. tostring(err), 2) end
  return r
end
function frameMT:callParserFunction(name, ...)
  local args
  if type(name) == "table" then name, args = name.name, name.args else args = { ... } end
  if type(args) ~= "table" then args = { args } end
  if #args == 1 and type(args[1]) == "table" then args = args[1] end
  local parts, first = {}, ""
  if args[1] ~= nil then first = tostring(args[1]) end
  for i = 2, #args do parts[#parts + 1] = tostring(args[i]) end
  for k, v in pairs(args) do
    if type(k) ~= "number" then parts[#parts + 1] = tostring(k) .. "=" .. tostring(v) end
  end
  local s = "{{" .. name .. ":" .. first
  if #parts > 0 then s = s .. "|" .. table.concat(parts, "|") end
  return h.preprocess(s .. "}}")
end
function frameMT:extensionTag(name, content, args)
  if type(name) == "table" then name, content, args = name.name, name.content, name.args end
  return h.preprocess(text.tag(name, type(args) == "table" and args or nil, content or ""))
end
function frameMT:newChild(opt)
  opt = opt or {}
  return makeFrame(strArgs(opt.args), self, opt.title or self.__title)
end
function frameMT:getArgument(name)
  if type(name) == "table" then name = name.name end
  local v = self.args[name]
  if v == nil then v = self.args[tonumber(name)] end
  if v == nil then return nil end
  return { expand = function() return v end }
end
function frameMT:argumentPairs() return pairs(self.args) end
function frameMT:newParserValue(t) return { expand = function() return h.preprocess(tostring(type(t) == "table" and t.text or t)) end } end
function frameMT:newTemplateParserValue(opt) local s = self return { expand = function() return s:expandTemplate(opt) end } end
local currentFrame
function mw.getCurrentFrame() return currentFrame end
function __setFrame(f) currentFrame = f end
