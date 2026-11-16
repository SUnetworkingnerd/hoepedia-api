//! Sandboxed Lua 5.4 for `{{#invoke:Module|function|args}}` (Scribunto-style modules).
//!
//! Only string/table/math/utf8 are loaded: no io/debug/package, no dofile, no bytecode.
//! `require` and `mw.loadData` load other `Module:` pages through the host. Memory (16 MiB)
//! and wall-clock time (1.5 s) are bounded. The `mw` library is implemented in `PRELUDE`
//! on top of a handful of host functions.

use std::{
    cell::RefCell,
    collections::HashMap,
    time::{Duration, Instant},
};

use mlua::{ChunkMode, Function, HookTriggers, Lua, LuaOptions, StdLib, Table, Value, VmState};

/// What a module can ask of the wiki it runs in.
pub trait Host {
    /// Source of `Module:<name>` (the `Module:` prefix is optional).
    fn module_source(&mut self, name: &str) -> Option<String>;
    /// Expand wikitext (templates, parser functions) in the context of the invoking frame.
    fn preprocess(&mut self, text: &str) -> String;
    fn expand_template(&mut self, title: &str, args: &HashMap<String, String>) -> Result<String, String>;
    /// Normalised title parts, or `None` for an invalid title.
    fn title_info(&mut self, text: &str, default_ns: i64) -> Option<TitleInfo>;
    fn page_exists(&mut self, prefixed: &str) -> bool;
    fn page_content(&mut self, prefixed: &str) -> Option<String>;
    fn url_for(&mut self, prefixed: &str) -> String;
    fn now(&self) -> i64;
    /// Site statistics (see `Resolver::stat`).
    fn stat(&mut self, _what: &str, _arg: &str) -> i64 {
        0
    }
    /// IANA time zone of the wiki.
    fn tz(&self) -> String {
        "UTC".into()
    }
    fn lang(&self) -> String {
        "en".into()
    }
}

pub struct TitleInfo {
    pub ns: i64,
    pub ns_text: String,
    pub text: String,
    pub prefixed: String,
    pub fragment: String,
}

pub struct Ctx<'a> {
    pub page: &'a str,
    pub site: &'a str,
    /// Title of the invoked module (`Module:Foo`), for `frame:getTitle()`.
    pub module: &'a str,
}

const PRELUDE: &str = include_str!("mw.lua");

fn err(e: mlua::Error) -> String {
    let s = e.to_string();
    let first = s.lines().find(|l| !l.trim().is_empty()).unwrap_or("error");
    first.trim_start_matches("runtime error: ").chars().take(300).collect()
}

fn args_table(lua: &Lua, args: &HashMap<String, String>) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    for (k, v) in args {
        match k.parse::<i64>() {
            Ok(n) => t.set(n, v.as_str())?,
            Err(_) => t.set(k.as_str(), v.as_str())?,
        }
    }
    Ok(t)
}

fn json_to_lua(lua: &Lua, v: &serde_json::Value, depth: usize) -> mlua::Result<Value> {
    if depth > 64 {
        return Err(mlua::Error::runtime("JSON nested too deeply"));
    }
    Ok(match v {
        serde_json::Value::Null => Value::Nil,
        serde_json::Value::Bool(b) => Value::Boolean(*b),
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) => Value::Integer(i),
            None => Value::Number(n.as_f64().unwrap_or(0.0)),
        },
        serde_json::Value::String(s) => Value::String(lua.create_string(s)?),
        serde_json::Value::Array(a) => {
            let t = lua.create_table()?;
            for (i, x) in a.iter().enumerate() {
                t.set(i + 1, json_to_lua(lua, x, depth + 1)?)?;
            }
            Value::Table(t)
        }
        serde_json::Value::Object(o) => {
            let t = lua.create_table()?;
            for (k, x) in o {
                // Numeric keys come back as numbers, as Scribunto does.
                match k.parse::<i64>() {
                    Ok(n) => t.set(n, json_to_lua(lua, x, depth + 1)?)?,
                    Err(_) => t.set(k.as_str(), json_to_lua(lua, x, depth + 1)?)?,
                }
            }
            Value::Table(t)
        }
    })
}

fn lua_to_json(v: &Value, depth: usize) -> mlua::Result<serde_json::Value> {
    use serde_json::Value as J;
    if depth > 64 {
        return Err(mlua::Error::runtime("table nested too deeply"));
    }
    Ok(match v {
        Value::Nil => J::Null,
        Value::Boolean(b) => J::Bool(*b),
        Value::Integer(i) => J::from(*i),
        Value::Number(n) => serde_json::Number::from_f64(*n).map_or(J::Null, J::Number),
        Value::String(s) => J::String(s.to_string_lossy()),
        Value::Table(t) => {
            let len = t.raw_len();
            let mut count = 0usize;
            let mut map = serde_json::Map::new();
            for pair in t.clone().pairs::<Value, Value>() {
                let (k, x) = pair?;
                count += 1;
                let key = match &k {
                    Value::String(s) => s.to_string_lossy(),
                    Value::Integer(i) => i.to_string(),
                    Value::Number(n) => n.to_string(),
                    _ => continue,
                };
                map.insert(key, lua_to_json(&x, depth + 1)?);
            }
            if len > 0 && count == len {
                let mut arr = Vec::with_capacity(len);
                for i in 1..=len {
                    arr.push(lua_to_json(&t.raw_get::<Value>(i)?, depth + 1)?);
                }
                J::Array(arr)
            } else {
                J::Object(map)
            }
        }
        _ => return Err(mlua::Error::runtime("value cannot be encoded as JSON")),
    })
}

fn char_sub(s: &str, i: i64, j: i64) -> String {
    let n = s.chars().count() as i64;
    let norm = |x: i64| if x < 0 { (n + x + 1).max(0) } else { x };
    let (a, b) = (norm(i).max(1), norm(j).min(n));
    if a > b {
        return String::new();
    }
    s.chars().skip((a - 1) as usize).take((b - a + 1) as usize).collect()
}

fn hash(algo: &str, data: &[u8]) -> Result<String, String> {
    use sha2::Digest;
    Ok(match algo.to_ascii_lowercase().as_str() {
        "md5" => crate::util::hex(&md5::Md5::digest(data)),
        "sha1" => crate::util::hex(&sha1::Sha1::digest(data)),
        "sha224" => crate::util::hex(&sha2::Sha224::digest(data)),
        "sha256" => crate::util::hex(&sha2::Sha256::digest(data)),
        "sha384" => crate::util::hex(&sha2::Sha384::digest(data)),
        "sha512" => crate::util::hex(&sha2::Sha512::digest(data)),
        other => return Err(format!("unknown hashing algorithm: {other}")),
    })
}

/// Lua-facing wrapper of the Unicode pattern matcher. All positions are 1-based.
struct Subject {
    chars: Vec<char>,
    /// Byte offset of every character, plus the total length.
    bytes: Vec<usize>,
}

impl Subject {
    fn new(s: &str) -> Subject {
        let mut bytes: Vec<usize> = s.char_indices().map(|(i, _)| i).collect();
        bytes.push(s.len());
        Subject { chars: s.chars().collect(), bytes }
    }
    fn cap(&self, lua: &Lua, c: &crate::lpat::Cap) -> mlua::Result<Value> {
        Ok(match c {
            crate::lpat::Cap::Pos(i) => Value::Integer(*i as i64 + 1),
            crate::lpat::Cap::Str(a, b) => Value::String(lua.create_string(self.chars[*a..*b].iter().collect::<String>())?),
        })
    }
}

/// `ufind(s, pattern, init, plain)` -> start, end, captures... (character positions) or nil.
fn ufind(lua: &Lua, (s, pat, init, plain): (String, String, Option<i64>, Option<bool>)) -> mlua::Result<mlua::MultiValue> {
    let sub = Subject::new(&s);
    let n = sub.chars.len() as i64;
    let init = match init.unwrap_or(1) {
        i if i < 0 => (n + i + 1).max(1),
        0 => 1,
        i => i,
    };
    let mut out = Vec::new();
    if init > n + 1 {
        return Ok(mlua::MultiValue::from_vec(vec![Value::Nil]));
    }
    let start = (init - 1) as usize;
    let pc: Vec<char> = pat.chars().collect();
    if plain == Some(true) {
        let found = if pc.is_empty() { Some(start) } else { sub.chars[start..].windows(pc.len()).position(|w| w == pc.as_slice()).map(|i| i + start) };
        return Ok(mlua::MultiValue::from_vec(match found {
            Some(i) => vec![Value::Integer(i as i64 + 1), Value::Integer((i + pc.len()) as i64)],
            None => vec![Value::Nil],
        }));
    }
    match crate::lpat::Matcher::new(&sub.chars, &pc).find(start).map_err(mlua::Error::runtime)? {
        Some(m) => {
            out.push(Value::Integer(m.start as i64 + 1));
            out.push(Value::Integer(m.end as i64));
            for c in &m.caps {
                out.push(sub.cap(lua, c)?);
            }
        }
        None => out.push(Value::Nil),
    }
    Ok(mlua::MultiValue::from_vec(out))
}

/// `umatches(s, pattern, max)` -> { {byte_start, byte_end, captures...}, ... } for gsub/gmatch.
fn umatches(lua: &Lua, (s, pat, max): (String, String, Option<f64>)) -> mlua::Result<Table> {
    let sub = Subject::new(&s);
    let pc: Vec<char> = pat.chars().collect();
    let max = max.map_or(1_000_000, |m| m.clamp(0.0, 1e6) as usize);
    let all = crate::lpat::Matcher::new(&sub.chars, &pc).all(max).map_err(mlua::Error::runtime)?;
    let out = lua.create_table_with_capacity(all.len(), 0)?;
    for (i, m) in all.iter().enumerate() {
        let t = lua.create_table_with_capacity(2 + m.caps.len(), 0)?;
        t.raw_set(1, sub.bytes[m.start] as i64 + 1)?;
        t.raw_set(2, sub.bytes[m.end] as i64)?;
        for (j, c) in m.caps.iter().enumerate() {
            t.raw_set(j as i64 + 3, sub.cap(lua, c)?)?;
        }
        out.raw_set(i as i64 + 1, t)?;
    }
    Ok(out)
}

fn table_to_map(t: &Table) -> mlua::Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    for pair in t.clone().pairs::<Value, Value>() {
        let (k, v) = pair?;
        let key = match k {
            Value::String(s) => s.to_string_lossy(),
            Value::Integer(i) => i.to_string(),
            _ => continue,
        };
        let val = match v {
            Value::String(s) => s.to_string_lossy(),
            Value::Integer(i) => i.to_string(),
            Value::Number(n) => crate::expr::fmt(n),
            _ => continue,
        };
        out.insert(key, val);
    }
    Ok(out)
}

pub fn run(
    source: &str,
    func: &str,
    args: &HashMap<String, String>,
    parent: &HashMap<String, String>,
    ctx: &Ctx,
    host: &mut dyn Host,
) -> Result<String, String> {
    let lua = Lua::new_with(StdLib::STRING | StdLib::TABLE | StdLib::MATH | StdLib::UTF8, LuaOptions::new()).map_err(err)?;
    lua.set_memory_limit(16 * 1024 * 1024).map_err(err)?;
    let start = Instant::now();
    lua.set_hook(HookTriggers::new().every_nth_instruction(5_000), move |_, _| {
        if start.elapsed() > Duration::from_millis(1500) {
            Err(mlua::Error::runtime("time limit exceeded"))
        } else {
            Ok(VmState::Continue)
        }
    });
    let now = host.now();
    let (tz, lang) = (host.tz(), host.lang());
    let host = RefCell::new(host);
    let go = || -> mlua::Result<String> {
        lua.scope(|scope| {
            let g = lua.globals();
            let h = lua.create_table()?;
            h.set("moduleSource", scope.create_function(|_, name: String| Ok(host.borrow_mut().module_source(&name)))?)?;
            h.set("preprocess", scope.create_function(|_, text: String| Ok(host.borrow_mut().preprocess(&text)))?)?;
            h.set(
                "expandTemplate",
                scope.create_function(|_, (title, args): (String, Table)| {
                    let map = table_to_map(&args)?;
                    Ok(match host.borrow_mut().expand_template(&title, &map) {
                        Ok(s) => (Some(s), None),
                        Err(e) => (None, Some(e)),
                    })
                })?,
            )?;
            h.set("exists", scope.create_function(|_, t: String| Ok(host.borrow_mut().page_exists(&t)))?)?;
            h.set("content", scope.create_function(|_, t: String| Ok(host.borrow_mut().page_content(&t)))?)?;
            h.set("url", scope.create_function(|_, t: String| Ok(host.borrow_mut().url_for(&t)))?)?;
            h.set(
                "titleInfo",
                scope.create_function(|lua, (text, ns, ns_name, _exact): (String, i64, Option<String>, Option<bool>)| {
                    let ns = ns_name.and_then(|n| crate::title::ns_by_name(&n)).unwrap_or(ns);
                    let Some(i) = host.borrow_mut().title_info(&text, ns) else { return Ok(Value::Nil) };
                    let t = lua.create_table()?;
                    let root = i.text.split('/').next().unwrap_or(&i.text).to_string();
                    let (base, sub) = match i.text.rsplit_once('/') {
                        Some((b, s)) => (b.to_string(), s.to_string()),
                        None => (i.text.clone(), i.text.clone()),
                    };
                    t.set("namespace", i.ns)?;
                    t.set("nsText", i.ns_text.as_str())?;
                    t.set("text", i.text.as_str())?;
                    t.set("prefixedText", i.prefixed.as_str())?;
                    t.set("fullText", if i.fragment.is_empty() { i.prefixed.clone() } else { format!("{}#{}", i.prefixed, i.fragment) })?;
                    t.set("fragment", i.fragment.as_str())?;
                    t.set("rootText", root)?;
                    t.set("isSubpage", i.text.contains('/'))?;
                    t.set("baseText", base)?;
                    t.set("subpageText", sub)?;
                    t.set("isTalkPage", i.ns % 2 == 1)?;
                    t.set("isContentPage", i.ns == 0)?;
                    t.set("isExternal", false)?;
                    t.set("interwiki", "")?;
                    Ok(Value::Table(t))
                })?,
            )?;
            h.set(
                "namespaces",
                lua.create_function(|lua, ()| {
                    let t = lua.create_table()?;
                    for (id, name) in crate::title::NAMESPACES {
                        let n = lua.create_table()?;
                        n.set("id", *id)?;
                        n.set("name", *name)?;
                        n.set("canonicalName", *name)?;
                        n.set("isTalk", id % 2 == 1)?;
                        n.set("isContent", *id == 0)?;
                        n.set("isSubject", id % 2 == 0)?;
                        t.set(*id, n)?;
                    }
                    Ok(t)
                })?,
            )?;
            h.set("usub", lua.create_function(|_, (s, i, j): (String, i64, i64)| Ok(char_sub(&s, i, j)))?)?;
            h.set("upper", lua.create_function(|_, s: String| Ok(s.to_uppercase()))?)?;
            h.set("lower", lua.create_function(|_, s: String| Ok(s.to_lowercase()))?)?;
            h.set("now", lua.create_function(move |_, ()| Ok(now))?)?;
            h.set("clock", lua.create_function(move |_, ()| Ok(start.elapsed().as_secs_f64()))?)?;
            h.set("parseDate", lua.create_function(move |_, s: String| Ok(crate::timefmt::parse(&s, now)))?)?;
            h.set(
                "formatDate",
                lua.create_function(move |_, (fmt, date, local): (String, String, Option<bool>)| {
                    Ok(crate::timefmt::parse_ex(&date, now).map(|(ts, wall)| {
                        if local == Some(true) {
                            let ts = if wall { crate::timefmt::from_wall(&tz, ts) } else { ts };
                            crate::timefmt::format_in(&fmt, ts, &crate::timefmt::local(&tz, ts))
                        } else {
                            crate::timefmt::format(&fmt, ts)
                        }
                    }))
                })?,
            )?;
            h.set("stat", scope.create_function(|_, (what, arg): (String, Option<String>)| Ok(host.borrow_mut().stat(&what, arg.as_deref().unwrap_or(""))))?)?;
            h.set("ufind", lua.create_function(ufind)?)?;
            h.set("umatches", lua.create_function(umatches)?)?;
            h.set("hash", lua.create_function(|_, (algo, data): (String, mlua::String)| hash(&algo, &data.as_bytes()).map_err(mlua::Error::runtime))?)?;
            g.set("__LANG", lang.as_str())?;
            h.set(
                "jsonEncode",
                lua.create_function(|_, v: Value| serde_json::to_string(&lua_to_json(&v, 0)?).map_err(|e| mlua::Error::runtime(e.to_string())))?,
            )?;
            h.set(
                "jsonDecode",
                lua.create_function(|lua, s: String| {
                    let v: serde_json::Value = serde_json::from_str(&s).map_err(|e| mlua::Error::runtime(format!("mw.text.jsonDecode: {e}")))?;
                    json_to_lua(lua, &v, 0)
                })?,
            )?;
            g.set("__h", h)?;
            g.set("__PAGE", ctx.page)?;
            g.set("__SITE", ctx.site)?;
            g.set("__VERSION", concat!("hoepedia-api ", env!("CARGO_PKG_VERSION")))?;
            lua.load(PRELUDE).set_name("=mw").set_mode(ChunkMode::Text).exec()?;
            let make_frame: Function = g.get("__makeFrame")?;
            let set_frame: Function = g.get("__setFrame")?;
            for name in ["__makeFrame", "__setFrame", "__PAGE", "__SITE", "__VERSION", "__LANG"] {
                g.set(name, Value::Nil)?;
            }

            // Text mode only: precompiled chunks could bypass the sandbox.
            let module: Value = lua.load(source).set_name(format!("={}", ctx.module)).set_mode(ChunkMode::Text).eval()?;
            let Value::Table(module) = module else {
                return Err(mlua::Error::runtime("the module did not return a table"));
            };
            let f: Value = module.get(func)?;
            let Value::Function(f) = f else {
                return Err(mlua::Error::runtime(format!("no function '{func}' in the module")));
            };
            let parent_frame: Table = make_frame.call((args_table(&lua, parent)?, Value::Nil, ctx.page))?;
            let frame: Table = make_frame.call((args_table(&lua, args)?, parent_frame, ctx.module))?;
            set_frame.call::<()>(frame.clone())?;

            match f.call::<Value>(frame)? {
                Value::Nil => Ok(String::new()),
                Value::String(s) => Ok(s.to_str()?.to_string()),
                Value::Integer(i) => Ok(i.to_string()),
                Value::Number(n) => Ok(crate::expr::fmt(n)),
                Value::Boolean(b) => Ok(if b { "true".into() } else { "false".into() }),
                Value::Table(t) => {
                    // Objects with __tostring (e.g. mw.html builders) are returned as their string form.
                    let ts: Function = lua.globals().get("tostring")?;
                    let s: mlua::String = ts.call(t)?;
                    Ok(s.to_str()?.to_string())
                }
                _ => Err(mlua::Error::runtime("the function must return a string")),
            }
        })
    };
    go().map_err(err)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestHost(HashMap<String, String>);
    impl Host for TestHost {
        fn module_source(&mut self, name: &str) -> Option<String> {
            let n = name.strip_prefix("Module:").unwrap_or(name);
            self.0.get(&format!("Module:{n}")).cloned()
        }
        fn preprocess(&mut self, text: &str) -> String {
            text.replace("{{X}}", "expanded")
        }
        fn expand_template(&mut self, title: &str, args: &HashMap<String, String>) -> Result<String, String> {
            match self.0.get(&format!("Template:{title}")) {
                Some(t) => Ok(t.replace("{{{1}}}", args.get("1").map_or("", String::as_str))),
                None => Err(format!("no such template '{title}'")),
            }
        }
        fn title_info(&mut self, text: &str, default_ns: i64) -> Option<TitleInfo> {
            let p = crate::title::Title::parse_in(text, default_ns).ok()?;
            Some(TitleInfo { ns: p.title.ns, ns_text: crate::title::ns_name(p.title.ns).to_string(), text: p.title.name.clone(), prefixed: p.title.full(), fragment: p.fragment.unwrap_or_default() })
        }
        fn page_exists(&mut self, t: &str) -> bool {
            self.0.contains_key(t)
        }
        fn page_content(&mut self, t: &str) -> Option<String> {
            self.0.get(t).cloned()
        }
        fn url_for(&mut self, t: &str) -> String {
            format!("/wiki/{}", t.replace(' ', "_"))
        }
        fn now(&self) -> i64 {
            1_794_553_200
        }
    }

    fn go_with(pages: &[(&str, &str)], src: &str, f: &str, a: &[(&str, &str)]) -> Result<String, String> {
        let args = a.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let mut host = TestHost(pages.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect());
        let parent: HashMap<String, String> = [("outer".to_string(), "P".to_string())].into_iter().collect();
        run(src, f, &args, &parent, &Ctx { page: "Some page", site: "Wiki", module: "Module:Test" }, &mut host)
    }

    fn go(src: &str, f: &str, a: &[(&str, &str)]) -> Result<String, String> {
        go_with(&[], src, f, a)
    }

    /// Wrap an expression as a module function `f`.
    fn ev(expr: &str) -> String {
        go(&format!("local p = {{}} function p.f(frame) return {expr} end return p"), "f", &[("1", "Bob"), ("list", "a,b,c")]).unwrap_or_else(|e| format!("ERR {e}"))
    }

    #[test]
    fn basic_module() {
        let src = "local p = {} function p.hello(frame) return 'Hello, ' .. (frame.args[1] or 'world') .. ' from ' .. mw.title.getCurrentTitle().text end return p";
        assert_eq!(go(src, "hello", &[("1", "Bob")]).unwrap(), "Hello, Bob from Some page");
        assert_eq!(go(src, "hello", &[]).unwrap(), "Hello, world from Some page");
    }

    #[test]
    fn frames() {
        assert_eq!(ev("frame:getParent().args.outer .. frame:getTitle() .. tostring(mw.getCurrentFrame() == frame)"), "PModule:Testtrue");
        assert_eq!(ev("frame:preprocess('a {{X}} b')"), "a expanded b");
        assert_eq!(ev("frame:callParserFunction('X', 'y')"), "{{X:y}}");
        assert_eq!(ev("frame:newChild{args={5}}.args['1']"), "5");
        assert_eq!(ev("frame:getArgument(1):expand()"), "Bob");
        let pages = [("Template:Greet", "Hi {{{1}}}!")];
        let src = "local p={} function p.f(frame) return frame:expandTemplate{title='Greet', args={'Ann'}} end function p.g(frame) return frame:expandTemplate{title='Nope'} end return p";
        assert_eq!(go_with(&pages, src, "f", &[]).unwrap(), "Hi Ann!");
        assert!(go_with(&pages, src, "g", &[]).unwrap_err().contains("no such template"));
    }

    #[test]
    fn text_and_ustring() {
        assert_eq!(ev("#mw.text.split(frame.args.list, ',') .. ':' .. mw.text.trim('  x ')"), "3:x");
        assert_eq!(ev("table.concat(mw.text.split('a1b22c', '%d+'), '-')"), "a-b-c");
        assert_eq!(ev("table.concat(mw.text.split('a.b', '.', true), '-')"), "a-b");
        assert_eq!(ev("mw.text.listToText({'a','b','c'})"), "a, b and c");
        assert_eq!(ev("mw.text.truncate('héllo wörld', 5)"), "héllo...");
        assert_eq!(ev("mw.text.encode('<a & b>') .. mw.text.decode('&lt;&#233;&amp;')"), "&lt;a &amp; b&gt;<é&");
        assert_eq!(ev("mw.text.tag('span', {class='x'}, 'hi')"), "<span class=\"x\">hi</span>");
        assert_eq!(ev("mw.ustring.len('héllo') .. mw.ustring.upper('é') .. mw.ustring.sub('héllo', 2, 3) .. mw.ustring.sub('héllo', -2)"), "5Ééllo");
        assert_eq!(ev("mw.text.jsonEncode(mw.text.jsonDecode('{\"a\":[1,2,{\"b\":true}]}'))"), "{\"a\":[1,2,{\"b\":true}]}");
        assert_eq!(ev("mw.text.jsonDecode('[10,20]')[2]"), "20");
    }

    #[test]
    fn language_uri_and_dates() {
        assert_eq!(ev("mw.getContentLanguage():formatNum(1234567.5)"), "1,234,567.5");
        assert_eq!(ev("mw.language.getContentLanguage():formatDate('j F Y', '2024-03-05')"), "5 March 2024");
        assert_eq!(ev("mw.getContentLanguage():ucfirst('élan') .. mw.getContentLanguage():plural(2, 'page', 'pages')"), "Élanpages");
        assert_eq!(ev("mw.uri.encode('a b&c') .. ' ' .. mw.uri.encode('a b', 'PATH') .. ' ' .. mw.uri.decode('a%20b+c')"), "a+b%26c a%20b a b c");
        assert_eq!(ev("os.date('%Y-%m-%d', 86400) .. ' ' .. os.time() .. ' ' .. os.date('*t', 0).year"), "1970-01-02 1794553200 1970");
        assert!(ev("mw.getContentLanguage():formatDate('Y', 'garbage')").starts_with("ERR"));
    }

    #[test]
    fn unicode_patterns_hash_and_messages() {
        assert_eq!(ev("mw.ustring.match('naïve café', '%a+')"), "naïve");
        assert_eq!(ev("table.concat({mw.ustring.find('añb', 'ñ')}, ',')"), "2,2");
        assert_eq!(ev("table.concat({mw.ustring.find('a.b', '.', 2, true)}, ',')"), "2,2");
        assert_eq!(ev("table.concat({mw.ustring.match('clé = valeur', '(%a+)%s*=%s*(%a+)')}, '|')"), "clé|valeur");
        assert_eq!(ev("(mw.ustring.gsub('héllo wörld', '%a+', string.upper))"), "HéLLO WöRLD");
        assert_eq!(ev("(mw.ustring.gsub('héllo wörld', '%a+', mw.ustring.upper))"), "HÉLLO WÖRLD");
        assert_eq!(ev("(mw.ustring.gsub('ééé', 'é', 'e', 2))"), "eeé");
        assert_eq!(ev("(mw.ustring.gsub('a b', '(%a) (%a)', '%2 %1 %0'))"), "b a a b");
        assert_eq!(ev("(mw.ustring.gsub('x=1, y=2', '(%a)=(%d)', {x='X'}))"), "X, y=2");
        assert_eq!(ev("select(2, mw.ustring.gsub('日本語', '.', ''))"), "3");
        assert_eq!(ev("(function() local t = {} for w in mw.ustring.gmatch('uno dós trés', '%a+') do t[#t+1] = w end return table.concat(t, '/') end)()"), "uno/dós/trés");
        assert_eq!(ev("(function() local t = {} for k, v in mw.ustring.gmatch('a=1,b=2', '(%a)=(%d)') do t[#t+1] = k .. v end return table.concat(t) end)()"), "a1b2");
        assert!(ev("mw.ustring.find('x', '[a')").starts_with("ERR"));
        assert!(ev("mw.ustring.find(string.rep('a', 40), 'a*a*a*a*a*a*a*a*a*a*a*b')").contains("too complex"));
        assert_eq!(ev("mw.hash.hashValue('md5', 'hello')"), "5d41402abc4b2a76b9719d911017c592");
        assert_eq!(ev("mw.hash.hashValue('sha1', 'hello')"), "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d");
        assert_eq!(ev("mw.hash.hashValue('sha256', '')"), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert!(ev("mw.hash.hashValue('nope', 'x')").starts_with("ERR"));
        assert_eq!(ev("mw.message.newRawMessage('Hello $1, you have $2 items', 'Ann', 3):plain()"), "Hello Ann, you have 3 items");
        assert_eq!(ev("tostring(mw.message.new('some-key'):exists())"), "false");
    }

    #[test]
    fn titles() {
        let pages = [("Main page", "hello"), ("Redir", "#REDIRECT [[Main page]]")];
        let f = |expr: &str| go_with(&pages, &format!("local p = {{}} function p.f(frame) return {expr} end return p"), "f", &[]).unwrap_or_else(|e| format!("ERR {e}"));
        assert_eq!(f("mw.title.new('template:foo bar/sub#frag').prefixedText"), "Template:Foo bar/sub");
        assert_eq!(f("mw.title.new('template:foo bar/sub').subpageText .. '|' .. mw.title.new('a/b/c').baseText .. '|' .. mw.title.new('a/b/c').rootText"), "sub|A/b|A");
        assert_eq!(f("tostring(mw.title.new('Main page').exists) .. tostring(mw.title.new('Nope').exists) .. tostring(mw.title.new('Redir').isRedirect)"), "truefalsetrue");
        assert_eq!(f("mw.title.new('Main page'):getContent() .. mw.title.makeTitle('Template', 'X').namespace .. mw.title.new('Main page'):fullUrl()"), "hello10/wiki/Main_page");
        assert_eq!(f("tostring(mw.title.new('a|b'))"), "nil");
        assert_eq!(f("mw.site.namespaces[10].name .. mw.site.namespaces.Module.id .. mw.site.siteName"), "Template828Wiki");
    }

    #[test]
    fn require_and_load_data() {
        let pages = [
            ("Module:Util", "local u = {} function u.twice(s) return s .. s end return u"),
            ("Module:Data", "return { colour = 'red', list = { 1, 2, 3 } }"),
            ("Module:Loop", "return require('Module:Loop')"),
            ("Module:Cfg.json", "{\"k\": 7}"),
        ];
        let f = |expr: &str| go_with(&pages, &format!("local p = {{}} function p.f(frame) return {expr} end return p"), "f", &[]).unwrap_or_else(|e| format!("ERR {e}"));
        assert_eq!(f("require('Module:Util').twice('ab') .. require('Util').twice('c')"), "ababcc");
        assert_eq!(f("mw.loadData('Module:Data').colour .. #mw.loadData('Module:Data').list"), "red3");
        assert_eq!(f("tostring(require('Module:Util') == require('Module:Util'))"), "true");
        assert_eq!(f("mw.loadJsonData('Module:Cfg.json').k"), "7");
        assert!(f("require('Module:Missing')").contains("not found"));
        assert!(f("require('Module:Loop')").starts_with("ERR"));
        assert_eq!(f("tostring(require('strict')) .. type(require('libraryUtil').checkType)"), "truefunction");
    }

    #[test]
    fn html_builder() {
        assert_eq!(
            ev("mw.html.create('div'):addClass('box'):css('color', 'red'):tag('span'):wikitext('a', 'b'):done():tag('br'):allDone()"),
            "<div class=\"box\" style=\"color:red\"><span>ab</span><br /></div>"
        );
        assert_eq!(ev("tostring(mw.html.create('td'):attr('colspan', 2):wikitext('x <y>'))"), "<td colspan=\"2\">x <y></td>");
        assert_eq!(ev("tostring(mw.html.create():wikitext('plain'))"), "plain");
        assert!(ev("mw.html.create('a b')").starts_with("ERR"));
    }

    #[test]
    fn sandbox_blocks_dangerous_things() {
        for src in [
            "local p={} function p.f() return os.getenv('HOME') end return p",
            "local p={} function p.f() return os.execute('id') end return p",
            "local p={} function p.f() return io.open('/etc/passwd') end return p",
            "local p={} function p.f() return require('os').getenv('HOME') end return p",
            "local p={} function p.f() return load('return 1')() end return p",
            "local p={} function p.f() return debug.getinfo(1) end return p",
            "local p={} function p.f() return __h.content('X') end return p",
            "local p={} function p.f() return string.dump(p.f) and loadstring('x') end return p",
        ] {
            assert!(go(src, "f", &[]).is_err(), "{src}");
        }
        // precompiled bytecode is refused, both as the module and through require
        assert!(go("\x1bLua\x54\x00", "f", &[]).is_err());
        assert!(go_with(&[("Module:Bin", "\x1bLua\x54\x00")], "local p={} function p.f() return require('Module:Bin') end return p", "f", &[]).is_err());
    }

    #[test]
    fn runaway_code_is_stopped() {
        let t = Instant::now();
        assert!(go("local p={} function p.f() while true do end end return p", "f", &[]).unwrap_err().contains("time limit"));
        assert!(t.elapsed().as_secs() < 5);
        assert!(go("local p={} function p.f() return #string.rep('x', 1e10) end return p", "f", &[]).is_err());
        assert!(go("local p={} function p.f() local t={} for i=1,1e9 do t[i]=i end return 1 end return p", "f", &[]).is_err());
    }

    #[test]
    fn bad_module_reports_error() {
        assert!(go("return 5", "f", &[]).unwrap_err().contains("table"));
        assert!(go("local p={} return p", "nope", &[]).unwrap_err().contains("no function"));
        assert!(go("this is not lua", "f", &[]).is_err());
        assert!(go("local p={} function p.f() error('boom') end return p", "f", &[]).unwrap_err().contains("boom"));
    }
}
