//! The wikitext engine: preprocessing (comments, nowiki, include tags), template
//! expansion with parser functions, block/inline rendering to *sanitised* HTML, and
//! extraction of links / categories / sections / redirects.
//!
//! Safety model: every character of source text is HTML-escaped on output. Only a small
//! whitelist of attribute-less inline tags is re-enabled, and the few places that accept
//! attributes (tables) go through `safe_attrs`.

use std::collections::{HashMap, HashSet};

use crate::title::{Title, NS_CATEGORY, NS_FILE, NS_TEMPLATE};
use crate::util::{civil, collapse_ws, html_escape, path_encode, strip_html};

const PH_OPEN: char = '\u{E000}';
const PH_CLOSE: char = '\u{E001}';
const MAX_DEPTH: usize = 24;
const MAX_CALLS: usize = 4_000;
const MAX_EXPANSION_BYTES: usize = 4_000_000;
/// Remote (interwiki) templates fetched per render.
const MAX_REMOTE_CALLS: usize = 40;

pub trait Resolver {
    fn template_source(&mut self, t: &Title) -> Option<String>;
    fn page_exists(&mut self, t: &Title) -> bool;
    /// URL template (containing `$1`) for an interwiki prefix such as `wikipedia`.
    fn interwiki(&mut self, _prefix: &str) -> Option<String> {
        None
    }
    /// Content type of the current version of an uploaded file (`None` = no such file).
    fn file_mime(&mut self, _name: &str) -> Option<String> {
        None
    }
    /// True if the prefix is a language link prefix: `[[fr:Titre]]` then records a language
    /// link instead of rendering an inline link.
    fn interwiki_is_language(&mut self, _prefix: &str) -> bool {
        false
    }
    /// Site statistics for modules: `what` is pages/articles/files/edits/users/admins/
    /// ns (arg = namespace id) or cat_all/cat_pages/cat_files/cat_subcats (arg = category).
    fn stat(&mut self, _what: &str, _arg: &str) -> i64 {
        0
    }
    /// Wikitext of `title` on the wiki behind an interwiki prefix that allows transclusion
    /// (`{{prefix:Template name}}`). `None` = not a transcludable prefix or not available.
    fn remote_template(&mut self, _prefix: &str, _title: &str) -> Option<String> {
        None
    }
    /// True if `{{prefix:...}}` transcludes from another wiki (even when the text is not
    /// available right now).
    fn interwiki_transcludes(&mut self, _prefix: &str) -> bool {
        false
    }
}

pub struct Opts<'a> {
    pub page: &'a Title,
    pub site_name: &'a str,
    /// Where internal links point, `$1` is replaced by the encoded title. e.g. `/wiki/$1`
    pub article_path: &'a str,
    /// Where `[[File:x]]` images are served from, `$1` = encoded file name.
    pub file_path: &'a str,
    pub now: i64,
    /// IANA zone for `{{#timel}}` and `{{LOCAL...}}` variables.
    pub tz: &'a str,
    /// Content language code (reported to modules).
    pub lang: &'a str,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Section {
    pub toclevel: usize,
    pub level: u8,
    pub line: String,
    pub number: String,
    pub index: usize,
    pub anchor: String,
}

#[derive(Default)]
pub struct Rendered {
    pub html: String,
    /// Plain text of the whole page (used for the search index).
    pub text: String,
    /// Plain text of the first non-empty paragraph.
    pub extract: String,
    pub extract_html: String,
    pub links: Vec<Title>,
    pub templates: Vec<Title>,
    /// External URLs linked from the page.
    pub extlinks: Vec<String>,
    /// (interwiki prefix, remote title)
    pub iwlinks: Vec<(String, String)>,
    /// (language prefix, title in that language)
    pub langlinks: Vec<(String, String)>,
    /// (category name, sort key)
    pub categories: Vec<(String, String)>,
    pub sections: Vec<Section>,
    pub redirect: Option<(Title, Option<String>)>,
    /// From `{{SHORTDESC:...}}`.
    pub description: Option<String>,
    pub display_title: Option<String>,
}

pub fn render(raw: &str, o: &Opts, res: &mut dyn Resolver) -> Rendered {
    if o.page.ns == crate::title::NS_MODULE {
        let mut r = Rendered::default();
        r.html = format!("<pre class=\"code\" data-lang=\"lua\"><code>{}</code></pre>", html_escape(raw));
        r.text = collapse_ws(raw);
        return r;
    }
    let mut p = P::new(o, res);
    if let Some((target, frag)) = detect_redirect(raw) {
        let mut r = Rendered::default();
        let href = format!("{}{}", p.article_url(&target), frag.as_ref().map(|f| format!("#{}", anchor_id(f))).unwrap_or_default());
        r.html = format!(
            "<div class=\"redirectMsg\"><p>Redirect to:</p><ul class=\"redirectText\"><li><a href=\"{}\" title=\"{}\">{}</a></li></ul></div>",
            html_escape(&href),
            html_escape(&target.full()),
            html_escape(&target.full())
        );
        r.text = String::new();
        r.links.push(target.clone());
        r.redirect = Some((target, frag));
        return r;
    }
    let text = p.prep(raw, false);
    let text = p.expand(&text, &HashMap::new(), 0);
    let text = strip_magic(&text);
    let text = p.extract_refs(&text);
    let html = p.blocks(&text);
    p.finish(html)
}

/// Expand templates/parser functions/variables and return wikitext (what `action=expandtemplates` shows).
/// `<nowiki>`/`<pre>` bodies come back HTML-escaped.
pub fn expand_only(raw: &str, o: &Opts, res: &mut dyn Resolver) -> String {
    let mut p = P::new(o, res);
    let t = p.prep(raw, false);
    let t = p.expand(&t, &HashMap::new(), 0);
    p.restore(&strip_magic(&t), 0)
}

// ---------------------------------------------------------------------------------------
// redirects & raw sections
// ---------------------------------------------------------------------------------------

pub fn detect_redirect(raw: &str) -> Option<(Title, Option<String>)> {
    let t = raw.trim_start();
    if t.len() < 9 || !t.is_char_boundary(9) || !t[..9].eq_ignore_ascii_case("#redirect") {
        return None;
    }
    let rest = t[9..].trim_start_matches(|c: char| c == ':' || c.is_whitespace());
    let rest = rest.strip_prefix("[[")?;
    let end = rest.find("]]")?;
    let inner = rest[..end].split('|').next()?;
    let parsed = Title::parse(inner.trim_start_matches(':')).ok()?;
    Some((parsed.title, parsed.fragment))
}

pub struct RawSection {
    pub start: usize,
    pub end: usize,
}

/// Headings of the *source* text (for section editing). Section N is `result[N-1]`.
pub fn raw_sections(raw: &str) -> Vec<RawSection> {
    let mut heads: Vec<(usize, u8, String)> = Vec::new();
    let mut pos = 0usize;
    let mut in_block = false;
    for line in raw.split_inclusive('\n') {
        let l = line.to_ascii_lowercase();
        let opens = l.matches("<nowiki").count() + l.matches("<pre").count();
        let closes = l.matches("</nowiki").count() + l.matches("</pre").count();
        if !in_block {
            if let Some((lvl, t)) = parse_heading(line.trim_end_matches('\n')) {
                heads.push((pos, lvl, t.to_string()));
            }
        }
        if opens > closes {
            in_block = true;
        } else if closes > opens {
            in_block = false;
        }
        pos += line.len();
    }
    let mut out = Vec::with_capacity(heads.len());
    for (i, (start, level, _)) in heads.iter().enumerate() {
        let end = heads[i + 1..].iter().find(|h| h.1 <= *level).map(|h| h.0).unwrap_or(raw.len());
        out.push(RawSection { start: *start, end });
    }
    out
}

/// Byte range of the lead section (before the first heading).
pub fn lead_range(raw: &str) -> (usize, usize) {
    (0, raw_sections(raw).first().map(|s| s.start).unwrap_or(raw.len()))
}

// ---------------------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------------------

pub fn anchor_id(s: &str) -> String {
    let plain = strip_html(s);
    let mut out = String::new();
    for c in plain.trim().chars() {
        if c.is_whitespace() {
            out.push('_');
        } else if c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | ':' | '(' | ')' | ',') {
            out.push(c);
        }
    }
    out
}

fn parse_heading(line: &str) -> Option<(u8, &str)> {
    let t = line.trim_end();
    let lead = t.bytes().take_while(|&c| c == b'=').count();
    if lead == 0 || lead >= t.len() {
        return None;
    }
    let trail = t.bytes().rev().take_while(|&c| c == b'=').count();
    if trail == 0 {
        return None;
    }
    let level = lead.min(trail).min(6);
    if t.len() < level * 2 {
        return None;
    }
    let inner = t[level..t.len() - level].trim();
    if inner.is_empty() {
        return None;
    }
    Some((level as u8, inner))
}

fn find_ci(hay_lower: &str, needle: &str, from: usize) -> Option<usize> {
    hay_lower.get(from..)?.find(needle).map(|p| p + from)
}

fn strip_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find("<!--") {
        out.push_str(&rest[..p]);
        match rest[p + 4..].find("-->") {
            Some(e) => rest = &rest[p + 4 + e + 3..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// `<noinclude>` / `<includeonly>` handling. `transcluding` = being pulled in by `{{...}}`.
fn include_tags(s: &str, transcluding: bool) -> String {
    let (drop, keep) = if transcluding { ("noinclude", "includeonly") } else { ("includeonly", "noinclude") };
    let s = remove_blocks_ci(s, drop);
    let s = strip_tags_ci(&s, keep);
    strip_tags_ci(&s, "onlyinclude")
}

fn remove_blocks_ci(s: &str, tag: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while let Some(p) = find_ci(&lower, &open, i) {
        out.push_str(&s[i..p]);
        match find_ci(&lower, &close, p) {
            Some(e) => i = e + close.len(),
            None => {
                i = s.len();
                break;
            }
        }
    }
    out.push_str(&s[i..]);
    out
}

fn strip_tags_ci(s: &str, tag: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let a = find_ci(&lower, &open, i);
        let b = find_ci(&lower, &close, i);
        let (p, len) = match (a, b) {
            (Some(a), Some(b)) if a < b => (a, open.len()),
            (Some(_), Some(b)) => (b, close.len()),
            (Some(a), None) => (a, open.len()),
            (None, Some(b)) => (b, close.len()),
            (None, None) => break,
        };
        out.push_str(&s[i..p]);
        i = p + len;
    }
    out.push_str(&s[i..]);
    out
}

fn strip_magic(s: &str) -> String {
    let mut out = s.to_string();
    for w in ["__NOTOC__", "__TOC__", "__FORCETOC__", "__NOEDITSECTION__", "__NOINDEX__", "__INDEX__"] {
        out = out.replace(w, "");
    }
    out
}

/// Finds the end of a `{{...}}` / `{{{...}}}` starting at `i`. Returns (end_exclusive, opener_len).
fn find_close(s: &str, i: usize) -> Option<(usize, usize)> {
    let b = s.as_bytes();
    let opener = |j: usize| if b[j..].starts_with(b"{{{") && !b[j..].starts_with(b"{{{{") { 3 } else { 2 };
    let open = opener(i);
    let mut stack = vec![open];
    let mut j = i + open;
    while j < b.len() {
        if b[j] == b'{' && b[j..].starts_with(b"{{") {
            let o = opener(j);
            stack.push(o);
            j += o;
        } else if b[j] == b'}' && b[j..].starts_with(b"}}") {
            let top = *stack.last()?;
            let take = if top == 3 && b[j..].starts_with(b"}}}") { 3 } else { 2 };
            stack.pop();
            j += take;
            if stack.is_empty() {
                return if j >= i + 2 * open { Some((j, open)) } else { None };
            }
        } else {
            j += 1;
        }
    }
    None
}

/// Split on top-level `sep` (ignores separators inside `{...}` and `[[...]]`).
fn split_top(s: &str, sep: u8) -> Vec<&str> {
    let b = s.as_bytes();
    let (mut parts, mut start, mut br, mut sq, mut j) = (Vec::new(), 0usize, 0i32, 0i32, 0usize);
    while j < b.len() {
        match b[j] {
            b'{' => br += 1,
            b'}' => br -= 1,
            b'[' if b.get(j + 1) == Some(&b'[') => {
                sq += 1;
                j += 1;
            }
            b']' if b.get(j + 1) == Some(&b']') => {
                sq -= 1;
                j += 1;
            }
            c if c == sep && br <= 0 && sq <= 0 => {
                parts.push(&s[start..j]);
                start = j + 1;
            }
            _ => {}
        }
        j += 1;
    }
    parts.push(&s[start..]);
    parts
}

fn split_eq(s: &str) -> Option<(&str, &str)> {
    let parts = split_top(s, b'=');
    if parts.len() < 2 {
        return None;
    }
    let k = parts[0].len();
    Some((&s[..k], &s[k + 1..]))
}

fn ucfirst(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

fn lcfirst(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_lowercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Valid `&name;` / `&#123;` entities are preserved, everything else is escaped.
fn esc_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    let b = s.as_bytes();
    for (i, c) in s.char_indices() {
        match c {
            '&' => {
                let tail = &s[i + 1..s.len().min(i + 12)];
                let valid = tail.find(';').is_some_and(|e| {
                    let n = &tail[..e];
                    !n.is_empty()
                        && (n.bytes().all(|x| x.is_ascii_alphanumeric())
                            || (n.starts_with('#') && n.len() > 1 && n[1..].bytes().all(|x| x.is_ascii_hexdigit() || x == b'x')))
                });
                if valid && b.get(i).is_some() {
                    out.push('&');
                } else {
                    out.push_str("&amp;");
                }
            }
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

const CSS_PROPS: &[&str] = &[
    "color", "background-color", "background", "font-weight", "font-style", "font-size", "font-family", "font-variant", "text-align",
    "text-decoration", "text-transform", "text-indent", "vertical-align", "white-space", "line-height", "letter-spacing", "word-break",
    "width", "min-width", "max-width", "height", "min-height", "max-height", "margin", "padding", "border", "border-collapse",
    "border-spacing", "border-radius", "float", "clear", "display", "list-style-type", "overflow", "opacity", "column-count",
    "box-sizing", "table-layout", "caption-side", "empty-cells",
];

/// Keeps only whitelisted CSS declarations with harmless values (no `url()`, no escapes,
/// no positioning), so inline styles cannot load resources or overlay the host page.
fn safe_style(v: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for decl in v.split(';').take(40) {
        let Some((prop, val)) = decl.split_once(':') else { continue };
        let prop = prop.trim().to_ascii_lowercase();
        let val = val.trim();
        let known = CSS_PROPS.contains(&prop.as_str())
            || ["margin-", "padding-", "border-"].iter().any(|p| prop.starts_with(p) && prop.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'));
        if !known || val.is_empty() || val.len() > 120 {
            continue;
        }
        let lower = val.to_ascii_lowercase();
        let chars_ok = val.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '#' | '%' | '.' | ',' | '(' | ')' | '+' | '-' | '!' | '/'));
        if !chars_ok || ["url", "expression", "var(", "attr(", "image", "element("].iter().any(|bad| lower.contains(bad)) {
            continue;
        }
        out.push(format!("{prop}:{val}"));
    }
    out.join(";")
}

/// Parses `name="value"` pairs, keeping only whitelisted names with validated values.
fn safe_attrs(s: &str, allowed: &[&str]) -> String {
    let mut out = String::new();
    let mut rest = s.trim();
    let mut seen: Vec<String> = Vec::new();
    while !rest.is_empty() {
        let Some(eq) = rest.find('=') else { break };
        let name = rest[..eq].trim().to_ascii_lowercase();
        let after = rest[eq + 1..].trim_start();
        let (val, next) = if let Some(q) = after.chars().next().filter(|c| *c == '"' || *c == '\'') {
            match after[1..].find(q) {
                Some(e) => (&after[1..1 + e], &after[e + 2..]),
                None => (&after[1..], ""),
            }
        } else {
            let e = after.find(char::is_whitespace).unwrap_or(after.len());
            (&after[..e], &after[e..])
        };
        let val = crate::util::decode_entities(val);
        let val = val.as_str();
        let clean: Option<String> = match name.as_str() {
            "class" => val.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_')).then(|| val.to_string()),
            "colspan" | "rowspan" => (!val.is_empty() && val.len() <= 3 && val.bytes().all(|b| b.is_ascii_digit())).then(|| val.to_string()),
            "scope" => matches!(val, "row" | "col" | "rowgroup" | "colgroup").then(|| val.to_string()),
            "dir" => matches!(val, "ltr" | "rtl" | "auto").then(|| val.to_string()),
            "lang" => (!val.is_empty() && val.len() <= 20 && val.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')).then(|| val.to_string()),
            "title" => (val.len() <= 300 && !val.chars().any(|c| c == PH_OPEN || c == PH_CLOSE)).then(|| val.to_string()),
            "style" => Some(safe_style(val)).filter(|v| !v.is_empty()),
            _ => None,
        };
        if let Some(v) = clean {
            if allowed.contains(&name.as_str()) && !seen.contains(&name) {
                out.push_str(&format!(" {}=\"{}\"", name, html_escape(&v)));
                seen.push(name);
            }
        }
        rest = next.trim_start();
    }
    out
}

const INLINE_TAGS: &[&str] = &[
    "b", "i", "u", "s", "em", "strong", "sub", "sup", "small", "big", "code", "tt", "kbd", "var", "samp", "del", "ins", "strike", "span",
    "abbr", "cite", "q", "mark", "dfn",
];
/// Block-level HTML that may be written directly (and is what `mw.html` produces). Every tag
/// opened is closed again by the renderer, so markup can never leak out of its container.
const BLOCK_TAGS: &[&str] =
    &["div", "blockquote", "p", "table", "thead", "tbody", "tfoot", "tr", "td", "th", "caption", "ul", "ol", "li", "dl", "dt", "dd"];
const COMMON_ATTRS: &[&str] = &["class", "style", "title", "lang", "dir"];
const CELL_ATTRS: &[&str] = &["class", "style", "title", "lang", "dir", "colspan", "rowspan", "scope"];

#[derive(Clone, Copy, PartialEq)]
enum TagKind {
    Inline,
    Block,
    Void,
}

struct HtmlTag {
    name: &'static str,
    /// Sanitised attributes (empty for closing tags).
    attrs: String,
    adv: usize,
    closing: bool,
    kind: TagKind,
}

/// A whitelisted HTML tag at the start of `rest`, with its attributes sanitised.
fn html_tag(rest: &str) -> Option<HtmlTag> {
    let bytes = rest.as_bytes();
    if bytes.first() != Some(&b'<') {
        return None;
    }
    let gt = bytes[..bytes.len().min(600)].iter().position(|b| *b == b'>')?;
    let inner = &rest[1..gt];
    if inner.contains('<') {
        return None;
    }
    let (closing, body) = match inner.strip_prefix('/') {
        Some(b) => (true, b),
        None => (false, inner),
    };
    let body = body.trim_end().trim_end_matches('/');
    let name_len = body.find(|c: char| !c.is_ascii_alphanumeric()).unwrap_or(body.len());
    let lname = body[..name_len].to_ascii_lowercase();
    let attr_src = &body[name_len..];
    if !attr_src.is_empty() && !attr_src.starts_with(char::is_whitespace) {
        return None;
    }
    let (name, kind): (&'static str, TagKind) = if lname == "br" {
        ("br", TagKind::Void)
    } else if lname == "hr" {
        ("hr", TagKind::Void)
    } else if let Some(t) = INLINE_TAGS.iter().find(|t| **t == lname) {
        (*t, TagKind::Inline)
    } else {
        let t = BLOCK_TAGS.iter().find(|t| **t == lname)?;
        (*t, TagKind::Block)
    };
    if kind == TagKind::Void && closing {
        return None;
    }
    let attrs = if closing || kind == TagKind::Void { String::new() } else { safe_attrs(attr_src, if matches!(name, "td" | "th") { CELL_ATTRS } else { COMMON_ATTRS }) };
    Some(HtmlTag { name, attrs, adv: gt + 1, closing, kind })
}

// ---------------------------------------------------------------------------------------
// the parser
// ---------------------------------------------------------------------------------------

enum Tok {
    Text(String),
    Html(String),
}

struct RefEntry {
    name: Option<String>,
    html: String,
    uses: usize,
}

type Args = HashMap<String, String>;

struct P<'a> {
    o: &'a Opts<'a>,
    res: &'a mut dyn Resolver,
    ph: Vec<String>,
    block_ph: HashSet<usize>,
    out: Rendered,
    seen_links: HashSet<Title>,
    seen_tpl: HashSet<Title>,
    seen_cats: HashSet<String>,
    seen_ext: HashSet<String>,
    seen_iw: HashSet<(String, String)>,
    remote_calls: usize,
    /// Set while expanding text fetched from another wiki: templates and modules it uses
    /// are then looked up on that wiki first.
    remote_ctx: Option<String>,
    exists: HashMap<Title, bool>,
    stack: Vec<Title>,
    calls: usize,
    invokes: usize,
    iw: HashMap<String, Option<String>>,
    bytes: usize,
    refs: Vec<RefEntry>,
    refs_ph: Vec<usize>,
    ext_counter: usize,
    anchors: HashMap<String, usize>,
    toc_stack: Vec<u8>,
    toc_nums: Vec<usize>,
}

impl<'a> P<'a> {
    fn new(o: &'a Opts<'a>, res: &'a mut dyn Resolver) -> Self {
        P {
            o,
            res,
            ph: Vec::new(),
            block_ph: HashSet::new(),
            out: Rendered::default(),
            seen_links: HashSet::new(),
            seen_tpl: HashSet::new(),
            seen_cats: HashSet::new(),
            seen_ext: HashSet::new(),
            seen_iw: HashSet::new(),
            remote_calls: 0,
            remote_ctx: None,
            exists: HashMap::new(),
            stack: Vec::new(),
            calls: 0,
            invokes: 0,
            iw: HashMap::new(),
            bytes: 0,
            refs: Vec::new(),
            refs_ph: Vec::new(),
            ext_counter: 0,
            anchors: HashMap::new(),
            toc_stack: Vec::new(),
            toc_nums: Vec::new(),
        }
    }

    fn err_span(&mut self, msg: &str) -> String {
        self.put(format!("<strong class=\"error\">{}</strong>", html_escape(msg)), false)
    }

    fn article_url(&self, t: &Title) -> String {
        self.o.article_path.replace("$1", &path_encode(&t.full()))
    }

    fn put(&mut self, html: String, block: bool) -> String {
        self.ph.push(html);
        if block {
            self.block_ph.insert(self.ph.len() - 1);
        }
        format!("{PH_OPEN}{}{PH_CLOSE}", self.ph.len() - 1)
    }

    fn restore(&self, s: &str, depth: usize) -> String {
        if depth > 6 || !s.contains(PH_OPEN) {
            return s.to_string();
        }
        let mut out = String::with_capacity(s.len());
        let mut rest = s;
        while let Some(p) = rest.find(PH_OPEN) {
            out.push_str(&rest[..p]);
            let after = &rest[p + PH_OPEN.len_utf8()..];
            match after.find(PH_CLOSE) {
                Some(e) => {
                    if let Some(h) = after[..e].parse::<usize>().ok().and_then(|n| self.ph.get(n)) {
                        out.push_str(&self.restore(h, depth + 1));
                    }
                    rest = &after[e + PH_CLOSE.len_utf8()..];
                }
                None => {
                    rest = after;
                }
            }
        }
        out.push_str(rest);
        out
    }

    fn is_exists(&mut self, t: &Title) -> bool {
        if let Some(v) = self.exists.get(t) {
            return *v;
        }
        let v = self.res.page_exists(t);
        self.exists.insert(t.clone(), v);
        v
    }

    // ----- preprocessing --------------------------------------------------------------

    fn prep(&mut self, s: &str, transcluding: bool) -> String {
        let s: String = s.chars().filter(|c| *c != PH_OPEN && *c != PH_CLOSE).collect();
        let s = strip_comments(&s);
        let s = include_tags(&s, transcluding);
        self.protect_blocks(&s)
    }

    /// `<nowiki>`, `<pre>`, `<syntaxhighlight>`, `<source>` become opaque placeholders.
    fn protect_blocks(&mut self, s: &str) -> String {
        const TAGS: [&str; 4] = ["nowiki", "pre", "syntaxhighlight", "source"];
        let lower = s.to_ascii_lowercase();
        let mut out = String::with_capacity(s.len());
        let mut i = 0;
        'outer: while i < s.len() {
            let mut best: Option<(usize, &str)> = None;
            for t in TAGS {
                let mut from = i;
                while let Some(p) = find_ci(&lower, &format!("<{t}"), from) {
                    let nc = lower.as_bytes().get(p + t.len() + 1).copied();
                    if matches!(nc, Some(b'>') | Some(b' ') | Some(b'/') | Some(b'\n') | Some(b'\t')) {
                        if best.is_none_or(|(bp, _)| p < bp) {
                            best = Some((p, t));
                        }
                        break;
                    }
                    from = p + 1;
                }
            }
            let Some((p, tag)) = best else { break 'outer };
            out.push_str(&s[i..p]);
            let Some(gt_rel) = s[p..].find('>') else {
                out.push_str(&s[p..p + 1]);
                i = p + 1;
                continue;
            };
            let gt = p + gt_rel;
            let open_tag = &s[p + 1..gt];
            let self_closing = open_tag.trim_end().ends_with('/');
            let (content, next) = if self_closing {
                ("", gt + 1)
            } else {
                let close = format!("</{tag}>");
                match find_ci(&lower, &close, gt + 1) {
                    Some(e) => (&s[gt + 1..e], e + close.len()),
                    None => {
                        out.push_str(&s[p..gt + 1]);
                        i = gt + 1;
                        continue;
                    }
                }
            };
            let html = match tag {
                "nowiki" => esc_text_nowiki(content),
                "pre" => format!("<pre>{}</pre>", html_escape(content.trim_start_matches('\n'))),
                _ => {
                    let lang: String = open_tag
                        .split("lang=")
                        .nth(1)
                        .map(|v| v.trim_start_matches(['"', '\'']).chars().take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '_')).collect())
                        .unwrap_or_default();
                    let cls = if lang.is_empty() { String::new() } else { format!(" data-lang=\"{lang}\"") };
                    format!("<pre class=\"code\"{cls}><code>{}</code></pre>", html_escape(content.trim_matches('\n')))
                }
            };
            let block = tag != "nowiki";
            let ph = self.put(html, block);
            out.push_str(&ph);
            i = next;
        }
        out.push_str(&s[i..]);
        out
    }

    // ----- template expansion ---------------------------------------------------------

    fn expand(&mut self, s: &str, args: &Args, depth: usize) -> String {
        if depth > MAX_DEPTH {
            return String::new();
        }
        let mut out = String::with_capacity(s.len());
        let mut i = 0;
        while let Some(p) = s[i..].find("{{") {
            out.push_str(&s[i..i + p]);
            i += p;
            match find_close(s, i) {
                Some((end, open)) => {
                    let inner = &s[i + open..end - open];
                    let rep = if open == 3 { self.expand_param(inner, args, depth) } else { self.expand_call(inner, args, depth) };
                    out.push_str(&rep);
                    i = end;
                }
                None => {
                    out.push_str("{{");
                    i += 2;
                }
            }
        }
        out.push_str(&s[i..]);
        out
    }

    fn expand_param(&mut self, inner: &str, args: &Args, depth: usize) -> String {
        let parts = split_top(inner, b'|');
        let name = self.expand(parts[0], args, depth + 1).trim().to_string();
        if let Some(v) = args.get(&name) {
            return v.clone();
        }
        if parts.len() > 1 {
            return self.expand(parts[1], args, depth + 1);
        }
        format!("{{{{{{{name}}}}}}}")
    }

    fn variable(&self, name: &str) -> Option<String> {
        let (y, mo, d, h, mi, s) = civil(self.o.now);
        const MONTHS: [&str; 12] =
            ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
        Some(match name {
            "!" => "|".into(),
            "=" => "=".into(),
            "PAGENAME" => self.o.page.name.clone(),
            "PAGENAMEE" => path_encode(&self.o.page.name),
            "FULLPAGENAME" => self.o.page.full(),
            "FULLPAGENAMEE" => path_encode(&self.o.page.full()),
            "NAMESPACE" => crate::title::ns_name(self.o.page.ns).to_string(),
            "SITENAME" => self.o.site_name.to_string(),
            "CURRENTYEAR" => y.to_string(),
            "CURRENTMONTH" => format!("{mo:02}"),
            "CURRENTMONTHNAME" => MONTHS[(mo - 1) as usize].to_string(),
            "CURRENTDAY" => d.to_string(),
            "CURRENTTIME" => format!("{h:02}:{mi:02}"),
            "CURRENTTIMESTAMP" => format!("{y:04}{mo:02}{d:02}{h:02}{mi:02}{s:02}"),
            "CURRENTHOUR" => format!("{h:02}"),
            "CURRENTDAY2" => format!("{d:02}"),
            "CURRENTDAYNAME" => crate::timefmt::format("l", self.o.now),
            "CURRENTDOW" => crate::timefmt::format("w", self.o.now),
            "CURRENTWEEK" => crate::timefmt::format("W", self.o.now).trim_start_matches('0').to_string(),
            "CONTENTLANGUAGE" | "CONTENTLANG" => self.o.lang.to_string(),
            _ if name.starts_with("LOCAL") => {
                let code = match name {
                    "LOCALYEAR" => "Y",
                    "LOCALMONTH" => "m",
                    "LOCALMONTHNAME" => "F",
                    "LOCALDAY" => "j",
                    "LOCALDAY2" => "d",
                    "LOCALDAYNAME" => "l",
                    "LOCALDOW" => "w",
                    "LOCALHOUR" => "H",
                    "LOCALTIME" => "H:i",
                    "LOCALTIMESTAMP" => "YmdHis",
                    _ => return None,
                };
                crate::timefmt::format_in(code, self.o.now, &crate::timefmt::local(self.o.tz, self.o.now))
            }
            _ => return None,
        })
    }

    fn expand_call(&mut self, inner: &str, args: &Args, depth: usize) -> String {
        self.calls += 1;
        if self.calls > MAX_CALLS || self.bytes > MAX_EXPANSION_BYTES {
            return String::new();
        }
        let parts = split_top(inner, b'|');
        let head = parts[0];

        if let Some((fname, first)) = head.split_once(':') {
            let f = fname.trim().to_lowercase();
            if matches!(
                f.as_str(),
                "#if" | "#ifeq" | "#ifexist" | "#switch" | "#time" | "#timel" | "#expr" | "#ifexpr" | "#iferror" | "#invoke" | "padleft" | "padright" | "plural" | "formatnum" | "anchorencode" | "#len" | "#sub" | "#pos" | "#replace" | "lc" | "uc" | "lcfirst" | "ucfirst" | "urlencode" | "fullurl" | "localurl" | "shortdesc" | "displaytitle"
            ) {
                let r = self.parser_fn(&f, first, &parts[1..], args, depth);
                self.bytes += r.len();
                return r;
            }
        }
        if let Some(v) = self.variable(head.trim()) {
            return v;
        }

        let name = self.expand(head, args, depth + 1);
        let new_args = self.call_args(&parts[1..], args, depth);
        // {{prefix:Title}} on an interwiki prefix that allows transclusion
        if let Some((pfx, remote)) = name.trim().split_once(':') {
            let key = pfx.trim().to_lowercase().replace(' ', "_");
            let remote = collapse_ws(&remote.replace('_', " "));
            if !key.is_empty() && !remote.is_empty() && crate::title::ns_by_name(pfx).is_none() && self.remote_calls < MAX_REMOTE_CALLS && self.res.interwiki_transcludes(&key) {
                // The dependency is recorded even while the text is still being fetched, so
                // the page is re-rendered when it arrives or changes.
                if self.seen_iw.insert((key.clone(), remote.clone())) {
                    self.out.iwlinks.push((key.clone(), remote.clone()));
                }
                self.remote_calls += 1;
                let Some(source) = self.res.remote_template(&key, &remote) else {
                    // not available yet: a link to it for now
                    return format!("[[{key}:{remote}]]");
                };
                {
                    // Remote wikitext is untrusted: it goes through the same expansion and
                    // sanitising as local text. A pseudo-title guards against loops.
                    let guard = Title::new(-1, &format!("{key}:{remote}"));
                    if self.stack.contains(&guard) {
                        return format!("[Template loop detected: {key}:{remote}]");
                    }
                    let body = self.prep(&source, true);
                    self.stack.push(guard);
                    let outer = self.remote_ctx.replace(key);
                    let r = self.expand(&body, &new_args, depth + 1);
                    self.remote_ctx = outer;
                    self.stack.pop();
                    self.bytes += r.len();
                    return r;
                }
            }
        }
        let Ok(parsed) = Title::parse_in(name.trim(), NS_TEMPLATE) else {
            return format!("{{{{{}}}}}", name);
        };
        match self.transclude(parsed.title, new_args, depth) {
            Ok(r) => r,
            Err(fallback) => fallback,
        }
    }

    /// Positional and named arguments of a call, expanded in the caller's frame.
    fn call_args(&mut self, parts: &[&str], args: &Args, depth: usize) -> Args {
        let mut new_args = Args::new();
        let mut pos = 1;
        for part in parts {
            match split_eq(part) {
                Some((k, v)) => {
                    let k = self.expand(k, args, depth + 1).trim().to_string();
                    let v = self.expand(v, args, depth + 1).trim().to_string();
                    new_args.insert(k, v);
                }
                None => {
                    let v = self.expand(part, args, depth + 1);
                    new_args.insert(pos.to_string(), v);
                    pos += 1;
                }
            }
        }
        new_args
    }

    /// Source of a template or module: inside remotely fetched text the remote wiki is asked
    /// first (so its own templates and modules are used), then the local wiki.
    fn source_of(&mut self, t: &Title) -> Option<String> {
        if let Some(pfx) = self.remote_ctx.clone() {
            if self.remote_calls < MAX_REMOTE_CALLS {
                self.remote_calls += 1;
                let full = t.full();
                if self.seen_iw.insert((pfx.clone(), full.clone())) {
                    self.out.iwlinks.push((pfx.clone(), full.clone()));
                }
                if let Some(s) = self.res.remote_template(&pfx, &full) {
                    return Some(s);
                }
            }
        }
        self.res.template_source(t)
    }

    /// Expand page `t` as a template. `Err` carries the wikitext shown when it cannot be.
    fn transclude(&mut self, t: Title, new_args: Args, depth: usize) -> Result<String, String> {
        if self.seen_tpl.insert(t.clone()) {
            self.out.templates.push(t.clone());
        }
        if self.stack.contains(&t) {
            return Err(format!("[Template loop detected: {}]", t.full()));
        }
        let Some(source) = self.source_of(&t) else {
            return Err(format!("[[:{}]]", t.full()));
        };
        let body = self.prep(&source, true);
        self.stack.push(t);
        let r = self.expand(&body, &new_args, depth + 1);
        self.stack.pop();
        self.bytes += r.len();
        Ok(r)
    }

    fn parser_fn(&mut self, f: &str, first: &str, rest: &[&str], args: &Args, d: usize) -> String {
        let ex = |p: &mut Self, s: Option<&&str>| s.map(|s| p.expand(s, args, d + 1).trim().to_string()).unwrap_or_default();
        match f {
            "#if" => {
                let c = self.expand(first, args, d + 1);
                ex(self, if c.trim().is_empty() { rest.get(1) } else { rest.first() })
            }
            "#ifeq" => {
                let a = self.expand(first, args, d + 1).trim().to_string();
                let b = ex(self, rest.first());
                let eq = match (a.parse::<f64>(), b.parse::<f64>()) {
                    (Ok(x), Ok(y)) => x == y,
                    _ => a == b,
                };
                ex(self, if eq { rest.get(1) } else { rest.get(2) })
            }
            "#ifexist" => {
                let name = self.expand(first, args, d + 1);
                let found = Title::parse(name.trim()).map(|p| self.is_exists(&p.title)).unwrap_or(false);
                ex(self, if found { rest.first() } else { rest.get(1) })
            }
            "#switch" => {
                let val = self.expand(first, args, d + 1).trim().to_string();
                let mut matched = false;
                let mut default: Option<&str> = None;
                for (i, arg) in rest.iter().enumerate() {
                    match split_eq(arg) {
                        Some((k, v)) => {
                            let k = self.expand(k, args, d + 1).trim().to_string();
                            if matched || k == val {
                                return self.expand(v, args, d + 1).trim().to_string();
                            }
                            if k == "#default" {
                                default = Some(v);
                            }
                        }
                        None => {
                            let k = self.expand(arg, args, d + 1).trim().to_string();
                            if k == val {
                                matched = true;
                            } else if i == rest.len() - 1 {
                                default = Some(arg);
                            }
                        }
                    }
                }
                default.map(|v| self.expand(v, args, d + 1).trim().to_string()).unwrap_or_default()
            }
            "lc" => self.expand(first, args, d + 1).trim().to_lowercase(),
            "uc" => self.expand(first, args, d + 1).trim().to_uppercase(),
            "lcfirst" => lcfirst(self.expand(first, args, d + 1).trim()),
            "ucfirst" => ucfirst(self.expand(first, args, d + 1).trim()),
            "urlencode" => path_encode(self.expand(first, args, d + 1).trim()),
            "fullurl" | "localurl" => match Title::parse(self.expand(first, args, d + 1).trim()) {
                Ok(p) => self.article_url(&p.title),
                Err(_) => String::new(),
            },
            "#time" | "#timel" => {
                let fmt = self.expand(first, args, d + 1).trim().to_string();
                let date = ex(self, rest.first());
                match crate::timefmt::parse_ex(&date, self.o.now) {
                    Some((ts, wall)) if f == "#timel" => {
                        // local time: a date without a zone is wall-clock time in the wiki's zone
                        let ts = if wall { crate::timefmt::from_wall(self.o.tz, ts) } else { ts };
                        crate::timefmt::format_in(&fmt, ts, &crate::timefmt::local(self.o.tz, ts))
                    }
                    Some((ts, _)) => crate::timefmt::format(&fmt, ts),
                    None => self.err_span("Error: Invalid time."),
                }
            }
            "#expr" => {
                let e = self.expand(first, args, d + 1);
                match crate::expr::eval(e.trim()) {
                    Ok(v) => crate::expr::fmt(v),
                    Err(m) => self.err_span(&format!("Expression error: {m}")),
                }
            }
            "#ifexpr" => {
                let e = self.expand(first, args, d + 1);
                let truthy = crate::expr::eval(e.trim()).map(|v| v != 0.0);
                match truthy {
                    Ok(true) => ex(self, rest.first()),
                    Ok(false) => ex(self, rest.get(1)),
                    Err(m) => self.err_span(&format!("Expression error: {m}")),
                }
            }
            "#iferror" => {
                let v = self.expand(first, args, d + 1);
                if self.restore(&v, 0).contains("class=\"error\"") {
                    ex(self, rest.first())
                } else if rest.len() > 1 {
                    ex(self, rest.get(1))
                } else {
                    v.trim().to_string()
                }
            }
            "padleft" | "padright" => {
                let s = self.expand(first, args, d + 1).trim().to_string();
                let n: usize = ex(self, rest.first()).parse().unwrap_or(0).min(500);
                let pad = { let p = ex(self, rest.get(1)); if p.is_empty() { "0".to_string() } else { p } };
                let have = s.chars().count();
                if have >= n {
                    s
                } else {
                    let fill: String = pad.chars().cycle().take(n - have).collect();
                    if f == "padleft" { format!("{fill}{s}") } else { format!("{s}{fill}") }
                }
            }
            "plural" => {
                let n = self.expand(first, args, d + 1).trim().replace(',', "").parse::<f64>().unwrap_or(0.0);
                ex(self, if n == 1.0 { rest.first() } else { rest.get(1).or(rest.first()) })
            }
            "formatnum" => {
                let raw = self.expand(first, args, d + 1).trim().to_string();
                match raw.parse::<f64>() {
                    Ok(_) => {
                        let (int, frac) = raw.split_once('.').map_or((raw.as_str(), None), |(a, b)| (a, Some(b)));
                        let (sign, digits) = int.strip_prefix('-').map_or(("", int), |d| ("-", d));
                        let mut g = String::new();
                        for (i, ch) in digits.chars().enumerate() {
                            if i > 0 && (digits.len() - i) % 3 == 0 {
                                g.push(',');
                            }
                            g.push(ch);
                        }
                        format!("{sign}{g}{}", frac.map(|f| format!(".{f}")).unwrap_or_default())
                    }
                    Err(_) => raw,
                }
            }
            "anchorencode" => anchor_id(self.expand(first, args, d + 1).trim()),
            "#len" => self.expand(first, args, d + 1).trim().chars().count().to_string(),
            "#sub" => {
                let s: Vec<char> = self.expand(first, args, d + 1).trim().chars().collect();
                let start: i64 = ex(self, rest.first()).parse().unwrap_or(0);
                let len: Option<i64> = rest.get(1).and_then(|_| ex(self, rest.get(1)).parse().ok());
                let n = s.len() as i64;
                let from = if start < 0 { (n + start).max(0) } else { start.min(n) };
                let to = match len { None => n, Some(l) if l < 0 => (n + l).max(from), Some(l) => (from + l).min(n) };
                s[from as usize..to as usize].iter().collect()
            }
            "#pos" => {
                let hay = self.expand(first, args, d + 1).trim().to_string();
                let needle = ex(self, rest.first());
                hay.find(&needle).filter(|_| !needle.is_empty()).map(|b| hay[..b].chars().count().to_string()).unwrap_or_default()
            }
            "#replace" => {
                let hay = self.expand(first, args, d + 1).trim().to_string();
                let (from, to) = (ex(self, rest.first()), ex(self, rest.get(1)));
                if from.is_empty() { hay } else { hay.replace(&from, &to) }
            }
            "#invoke" => {
                self.invokes += 1;
                if self.invokes > 50 {
                    return self.err_span("Script error: too many #invoke calls");
                }
                let module = self.expand(first, args, d + 1).trim().to_string();
                let func = ex(self, rest.first());
                let call_args = self.call_args(rest.get(1..).unwrap_or(&[]), args, d);
                let Ok(parsed) = Title::parse_in(&module, crate::title::NS_MODULE) else {
                    return self.err_span("Script error: invalid module name");
                };
                let mt = parsed.title;
                if self.seen_tpl.insert(mt.clone()) {
                    self.out.templates.push(mt.clone());
                }
                let Some(src) = self.source_of(&mt) else {
                    return self.err_span(&format!("Script error: no such module \"{}\"", mt.full()));
                };
                if self.stack.len() > MAX_DEPTH {
                    return self.err_span("Script error: modules nested too deeply");
                }
                let (page, site, module) = (self.o.page.full(), self.o.site_name.to_string(), mt.full());
                let ctx = crate::lua::Ctx { page: &page, site: &site, module: &module };
                // The module frame sits on the stack so recursive invocations are bounded.
                self.stack.push(Title::new(-2, &format!("{}#{}", module, self.invokes)));
                let result = crate::lua::run(&src, &func, &call_args, args, &ctx, &mut LuaHost { p: self, args: &call_args, depth: d + 1 });
                self.stack.pop();
                match result {
                    // Placeholders in the output are kept: they stand for already-sanitised
                    // fragments (nowiki, error spans) that passed through the module.
                    Ok(s) => self.protect_blocks(&strip_comments(&s)),
                    Err(m) => self.err_span(&format!("Script error: {m}")),
                }
            }
            "shortdesc" => {
                let v = collapse_ws(&strip_html(self.expand(first, args, d + 1).trim()));
                if !v.is_empty() && v != "none" {
                    self.out.description = Some(v);
                }
                String::new()
            }
            "displaytitle" => {
                let v = collapse_ws(&strip_html(self.expand(first, args, d + 1).trim()));
                if !v.is_empty() {
                    self.out.display_title = Some(v);
                }
                String::new()
            }
            _ => String::new(),
        }
    }

    // ----- <ref> -----------------------------------------------------------------------

    fn extract_refs(&mut self, s: &str) -> String {
        let lower = s.to_ascii_lowercase();
        let mut out = String::with_capacity(s.len());
        let mut i = 0;
        while i < s.len() {
            let r = find_ci(&lower, "<ref", i);
            let rs = find_ci(&lower, "<references", i);
            let (p, is_refs) = match (r, rs) {
                (Some(a), Some(b)) if b <= a => (b, true),
                (Some(a), _) => (a, false),
                (None, Some(b)) => (b, true),
                (None, None) => break,
            };
            let nc = lower.as_bytes().get(p + if is_refs { 11 } else { 4 }).copied();
            if !matches!(nc, Some(b'>') | Some(b' ') | Some(b'/') | Some(b'\n')) {
                out.push_str(&s[i..p + 1]);
                i = p + 1;
                continue;
            }
            let Some(gt_rel) = s[p..].find('>') else { break };
            let gt = p + gt_rel;
            let tag = &s[p + 1..gt];
            let self_closing = tag.trim_end().ends_with('/');
            out.push_str(&s[i..p]);
            if is_refs {
                let idx = self.ph.len();
                let ph = self.put(String::new(), true);
                self.refs_ph.push(idx);
                out.push_str(&ph);
                i = gt + 1;
                continue;
            }
            let name = tag.split("name=").nth(1).map(|v| {
                let v = v.trim_start();
                match v.chars().next() {
                    Some(q @ ('"' | '\'')) => v[1..].split(q).next().unwrap_or("").to_string(),
                    _ => v.chars().take_while(|c| !c.is_whitespace() && *c != '/' && *c != '>').collect(),
                }
            });
            let (content, next) = if self_closing {
                (None, gt + 1)
            } else {
                match find_ci(&lower, "</ref>", gt + 1) {
                    Some(e) => (Some(s[gt + 1..e].to_string()), e + 6),
                    None => {
                        out.push_str(&s[p..gt + 1]);
                        i = gt + 1;
                        continue;
                    }
                }
            };
            let existing = name.as_ref().and_then(|n| self.refs.iter().position(|r| r.name.as_deref() == Some(n.as_str())));
            let idx = match (existing, content) {
                (Some(ix), c) => {
                    if let Some(c) = c {
                        if self.refs[ix].html.is_empty() {
                            self.refs[ix].html = self.inline(c.trim());
                        }
                    }
                    ix
                }
                (None, c) => {
                    let html = c.map(|c| self.inline(c.trim())).unwrap_or_default();
                    self.refs.push(RefEntry { name: name.clone(), html, uses: 0 });
                    self.refs.len() - 1
                }
            };
            self.refs[idx].uses += 1;
            let (n, k) = (idx + 1, self.refs[idx].uses);
            let id = if k == 1 { format!("cite_ref-{n}") } else { format!("cite_ref-{n}-{}", k - 1) };
            let sup = format!("<sup id=\"{id}\" class=\"reference\"><a href=\"#cite_note-{n}\">[{n}]</a></sup>");
            let ph = self.put(sup, false);
            out.push_str(&ph);
            i = next;
        }
        out.push_str(&s[i..]);
        out
    }

    fn refs_html(&self) -> String {
        let mut h = String::from("<ol class=\"references\">");
        for (i, r) in self.refs.iter().enumerate() {
            let n = i + 1;
            h.push_str(&format!(
                "<li id=\"cite_note-{n}\"><span class=\"mw-cite-backlink\"><a href=\"#cite_ref-{n}\">\u{2191}</a></span> <span class=\"reference-text\">{}</span></li>",
                r.html
            ));
        }
        h.push_str("</ol>");
        h
    }

    // ----- block level ------------------------------------------------------------------

    fn blocks(&mut self, text: &str) -> String {
        let lines: Vec<&str> = text.split('\n').collect();
        let mut out = String::new();
        let mut para = String::new();
        let mut pre = String::new();
        let mut list = String::new();
        // Raw block-level HTML (<div>, <table>, ...) opened in this container and not yet closed.
        let mut open_blocks: Vec<&'static str> = Vec::new();
        let mut i = 0;
        while i < lines.len() {
            let line = lines[i];
            i += 1;
            let tl = line.trim_start();

            if tl.starts_with("{|") {
                self.flush_para(&mut out, &mut para);
                self.flush_pre(&mut out, &mut pre);
                self.do_list(&mut out, &mut list, "", "");
                let start = i - 1;
                while i < lines.len() && !lines[i].trim_start().starts_with("|}") {
                    i += 1;
                }
                let end = i.min(lines.len() - 1);
                i = end + 1;
                let html = self.table(&lines[start..=end]);
                out.push_str(&html);
                continue;
            }
            if let Some((lvl, title)) = parse_heading(line) {
                self.flush_para(&mut out, &mut para);
                self.flush_pre(&mut out, &mut pre);
                self.do_list(&mut out, &mut list, "", "");
                out.push_str(&self.heading(lvl, title));
                continue;
            }
            if line.starts_with("----") {
                self.flush_para(&mut out, &mut para);
                self.flush_pre(&mut out, &mut pre);
                self.do_list(&mut out, &mut list, "", "");
                out.push_str("<hr>\n");
                let rest = line.trim_start_matches('-').trim();
                if !rest.is_empty() {
                    para.push_str(rest);
                    para.push('\n');
                }
                continue;
            }
            let prefix: String = line.chars().take_while(|c| matches!(c, '*' | '#' | ':' | ';')).collect();
            if !prefix.is_empty() {
                self.flush_para(&mut out, &mut para);
                self.flush_pre(&mut out, &mut pre);
                let content = line[prefix.len()..].trim();
                self.do_list(&mut out, &mut list, &prefix, content);
                continue;
            }
            self.do_list(&mut out, &mut list, "", "");
            if line.starts_with(' ') && !line.trim().is_empty() {
                self.flush_para(&mut out, &mut para);
                pre.push_str(&line[1..]);
                pre.push('\n');
                continue;
            }
            self.flush_pre(&mut out, &mut pre);
            if line.trim().is_empty() {
                self.flush_para(&mut out, &mut para);
                continue;
            }
            if self.is_block_ph(line.trim()) {
                // <pre>, <syntaxhighlight>, <references/> on their own line are block elements
                self.flush_para(&mut out, &mut para);
                out.push_str(line.trim());
                out.push('\n');
                continue;
            }
            if line.contains('<') && self.block_html_line(line, &mut out, &mut para, &mut open_blocks) {
                continue;
            }
            para.push_str(line);
            para.push('\n');
        }
        self.flush_para(&mut out, &mut para);
        self.flush_pre(&mut out, &mut pre);
        self.do_list(&mut out, &mut list, "", "");
        while let Some(t) = open_blocks.pop() {
            out.push_str(&format!("</{t}>\n"));
        }
        out
    }

    /// A line containing raw block-level HTML tags: the tags are emitted (balanced against
    /// `open`), the text between them is rendered inline. Returns false if the line has none.
    fn block_html_line(&mut self, line: &str, out: &mut String, para: &mut String, open: &mut Vec<&'static str>) -> bool {
        let mut pieces: Vec<(bool, &str, Option<HtmlTag>)> = Vec::new();
        let (mut i, mut start, mut found) = (0usize, 0usize, false);
        while let Some(p) = line[i..].find('<') {
            let at = i + p;
            match html_tag(&line[at..]).filter(|t| t.kind == TagKind::Block) {
                Some(t) => {
                    found = true;
                    if at > start {
                        pieces.push((false, &line[start..at], None));
                    }
                    i = at + t.adv;
                    start = i;
                    pieces.push((true, "", Some(t)));
                }
                None => i = at + 1,
            }
        }
        if !found {
            return false;
        }
        if start < line.len() {
            pieces.push((false, &line[start..], None));
        }
        self.flush_para(out, para);
        for (is_tag, text, tag) in pieces {
            if !is_tag {
                if !text.trim().is_empty() {
                    out.push_str(&self.inline_line(text.trim(), true));
                }
                continue;
            }
            let Some(t) = tag else { continue };
            if !t.closing {
                if open.len() < 200 {
                    open.push(t.name);
                    out.push_str(&format!("<{}{}>", t.name, t.attrs));
                }
            } else if let Some(pos) = open.iter().rposition(|n| *n == t.name) {
                while open.len() > pos {
                    out.push_str(&format!("</{}>", open.pop().unwrap_or("div")));
                }
            }
        }
        out.push('\n');
        true
    }

    fn is_block_ph(&self, text: &str) -> bool {
        text.starts_with(PH_OPEN)
            && text.ends_with(PH_CLOSE)
            && text.matches(PH_OPEN).count() == 1
            && text[PH_OPEN.len_utf8()..text.len() - PH_CLOSE.len_utf8()].parse::<usize>().is_ok_and(|n| self.block_ph.contains(&n))
    }

    fn flush_para(&mut self, out: &mut String, para: &mut String) {
        let text = para.trim().to_string();
        para.clear();
        if text.is_empty() {
            return;
        }
        if self.is_block_ph(&text) {
            out.push_str(&text);
            out.push('\n');
            return;
        }
        let html = self.inline(&text);
        if html.trim().is_empty() {
            // a paragraph that held only categories or language links renders as nothing
            return;
        }
        if self.is_block_ph(html.trim()) {
            // e.g. a `[[File:...|thumb]]` figure on its own line
            out.push_str(html.trim());
            out.push('\n');
            return;
        }
        out.push_str("<p>");
        out.push_str(&html);
        out.push_str("</p>\n");
    }

    fn flush_pre(&mut self, out: &mut String, pre: &mut String) {
        if pre.is_empty() {
            return;
        }
        let text = pre.trim_end().to_string();
        pre.clear();
        if self.is_block_ph(text.trim()) {
            out.push_str(text.trim());
            out.push('\n');
            return;
        }
        out.push_str("<pre>");
        out.push_str(&self.inline(&text));
        out.push_str("</pre>\n");
    }

    fn heading(&mut self, level: u8, raw_title: &str) -> String {
        let html = self.inline_line(raw_title, true);
        let plain = collapse_ws(&strip_html(&self.restore(&html, 0)));
        let base = anchor_id(&plain);
        let n = self.anchors.entry(base.clone()).or_insert(0);
        *n += 1;
        let anchor = if *n == 1 { base } else { format!("{base}_{n}") };

        while self.toc_stack.last().is_some_and(|&t| t >= level) {
            self.toc_stack.pop();
        }
        let toclevel = self.toc_stack.len() + 1;
        self.toc_stack.push(level);
        self.toc_nums.truncate(toclevel);
        while self.toc_nums.len() < toclevel {
            self.toc_nums.push(0);
        }
        self.toc_nums[toclevel - 1] += 1;
        let number = self.toc_nums.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(".");
        self.out.sections.push(Section {
            toclevel,
            level,
            line: plain,
            number,
            index: self.out.sections.len() + 1,
            anchor: anchor.clone(),
        });
        format!("<h{level} id=\"{}\">{html}</h{level}>\n", html_escape(&anchor))
    }

    fn do_list(&mut self, out: &mut String, prev: &mut String, cur: &str, content: &str) {
        let p: Vec<char> = prev.chars().collect();
        let c: Vec<char> = cur.chars().collect();
        let same = |a: char, b: char| a == b || (matches!(a, ';' | ':') && matches!(b, ';' | ':'));
        let common = p.iter().zip(c.iter()).take_while(|(a, b)| same(**a, **b)).count();
        let list_open = |ch: char| match ch {
            '*' => "<ul>",
            '#' => "<ol>",
            _ => "<dl>",
        };
        let list_close = |ch: char| match ch {
            '*' => "</ul>\n",
            '#' => "</ol>\n",
            _ => "</dl>\n",
        };
        let item_open = |ch: char| match ch {
            '*' | '#' => "<li>",
            ';' => "<dt>",
            _ => "<dd>",
        };
        let item_close = |ch: char| match ch {
            '*' | '#' => "</li>",
            ';' => "</dt>",
            _ => "</dd>",
        };
        for k in (common..p.len()).rev() {
            out.push_str(item_close(p[k]));
            out.push_str(list_close(p[k]));
        }
        if c.is_empty() {
            prev.clear();
            return;
        }
        if c.len() == common {
            out.push_str(item_close(p[common - 1]));
            out.push('\n');
            out.push_str(item_open(c[common - 1]));
        } else {
            for k in common..c.len() {
                out.push_str(list_open(c[k]));
                out.push_str(item_open(c[k]));
            }
        }
        let mut new_prev: Vec<char> = c.clone();
        if c.last() == Some(&';') {
            // ";term : definition" on one line
            let (term, def) = split_dt(content);
            out.push_str(&self.inline_line(term, true));
            if let Some(d) = def {
                out.push_str("</dt><dd>");
                out.push_str(&self.inline_line(d, true));
                *new_prev.last_mut().unwrap() = ':';
            }
        } else {
            out.push_str(&self.inline_line(content, true));
        }
        *prev = new_prev.into_iter().collect();
    }

    fn table(&mut self, lines: &[&str]) -> String {
        struct Cell {
            header: bool,
            attrs: String,
            content: String,
        }
        let table_attrs = safe_attrs(lines[0].trim_start().trim_start_matches("{|"), COMMON_ATTRS);
        let mut caption = String::new();
        let mut rows: Vec<Vec<Cell>> = Vec::new();
        for line in &lines[1..] {
            let t = line.trim_start();
            if t.starts_with("|}") {
                break;
            }
            if let Some(c) = t.strip_prefix("|+") {
                caption = c.trim().to_string();
            } else if t.starts_with("|-") {
                if rows.last().is_some_and(|r| !r.is_empty()) {
                    rows.push(Vec::new());
                }
            } else if t.starts_with('!') || t.starts_with('|') {
                let header = t.starts_with('!');
                let seps: &[&str] = if header { &["!!", "||"] } else { &["||"] };
                if rows.is_empty() {
                    rows.push(Vec::new());
                }
                for piece in split_cells(&t[1..], seps) {
                    let (attrs, content) = split_cell_attr(piece);
                    rows.last_mut().unwrap().push(Cell {
                        header,
                        attrs: safe_attrs(attrs, CELL_ATTRS),
                        content: content.trim().to_string(),
                    });
                }
            } else if let Some(cell) = rows.last_mut().and_then(|r| r.last_mut()) {
                cell.content.push('\n');
                cell.content.push_str(line);
            }
        }
        let mut html = format!("<table{table_attrs}>\n");
        if !caption.is_empty() {
            html.push_str(&format!("<caption>{}</caption>\n", self.inline_line(&caption, true)));
        }
        for row in rows.iter().filter(|r| !r.is_empty()) {
            html.push_str("<tr>");
            for c in row {
                let tag = if c.header { "th" } else { "td" };
                let body = if c.content.contains('\n') { self.blocks(&c.content) } else { self.inline_line(&c.content, true) };
                html.push_str(&format!("<{tag}{}>{}</{tag}>", c.attrs, body));
            }
            html.push_str("</tr>\n");
        }
        html.push_str("</table>\n");
        html
    }

    // ----- inline -------------------------------------------------------------------------

    fn inline(&mut self, s: &str) -> String {
        s.split('\n').map(|l| self.inline_line(l, true)).collect::<Vec<_>>().join("\n")
    }

    fn inline_line(&mut self, s: &str, links: bool) -> String {
        let mut toks: Vec<Tok> = Vec::new();
        let mut text = String::new();
        let mut open_tags: Vec<&'static str> = Vec::new();
        let mut i = 0;
        macro_rules! flush {
            () => {
                if !text.is_empty() {
                    toks.push(Tok::Text(std::mem::take(&mut text)));
                }
            };
        }
        while i < s.len() {
            let rest = &s[i..];
            if links && rest.starts_with("[[") {
                if let Some((html, adv)) = self.wikilink(rest) {
                    flush!();
                    toks.push(Tok::Html(html));
                    i += adv;
                    continue;
                }
            } else if links && rest.starts_with('[') {
                if let Some((html, adv)) = self.extlink(rest) {
                    flush!();
                    toks.push(Tok::Html(html));
                    i += adv;
                    continue;
                }
            } else if links
                && (rest.starts_with("http://") || rest.starts_with("https://"))
                && !text.chars().last().is_some_and(|c| c.is_alphanumeric())
            {
                let end = rest.find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '[' | ']' | '"' | PH_OPEN)).unwrap_or(rest.len());
                let url = rest[..end].trim_end_matches(['.', ',', ';', ':', '!', '?', ')', '\'']);
                if url.len() > 9 {
                    flush!();
                    self.note_extlink(url);
                    toks.push(Tok::Html(format!(
                        "<a class=\"external free\" rel=\"nofollow noopener\" href=\"{0}\">{0}</a>",
                        html_escape(url)
                    )));
                    i += url.len();
                    continue;
                }
            } else if rest.starts_with('<') {
                if let Some(HtmlTag { name, attrs, adv, closing, kind }) = html_tag(rest) {
                    flush!();
                    match (kind, closing) {
                        (TagKind::Void, _) => toks.push(Tok::Html(format!("<{name}>"))),
                        (_, false) => {
                            open_tags.push(name);
                            toks.push(Tok::Html(format!("<{name}{attrs}>")));
                        }
                        (_, true) => {
                            if let Some(pos) = open_tags.iter().rposition(|t| *t == name) {
                                let mut h = String::new();
                                while open_tags.len() > pos {
                                    h.push_str(&format!("</{}>", open_tags.pop().unwrap()));
                                }
                                toks.push(Tok::Html(h));
                            }
                        }
                    }
                    i += adv;
                    continue;
                }
            }
            let c = rest.chars().next().unwrap();
            text.push(c);
            i += c.len_utf8();
        }
        flush!();
        while let Some(t) = open_tags.pop() {
            toks.push(Tok::Html(format!("</{t}>")));
        }
        assemble(toks)
    }

    fn wikilink(&mut self, rest: &str) -> Option<(String, usize)> {
        let end = rest.find("]]")?;
        let inner = &rest[2..end];
        if inner.contains("[[") {
            return None;
        }
        let (target_raw, label_raw) = match inner.split_once('|') {
            Some((t, l)) => (t.trim(), Some(l)),
            None => (inner.trim(), None),
        };
        if target_raw.is_empty() {
            return None;
        }
        let mut adv = end + 2;

        if let Some(sec) = target_raw.strip_prefix('#') {
            let label = label_raw.map(|l| self.inline_line(l, false)).unwrap_or_else(|| esc_text(sec));
            return Some((format!("<a href=\"#{}\">{label}</a>", html_escape(&anchor_id(sec))), adv));
        }
        {
            let inline_only = target_raw.starts_with(':');
            if let Some((pfx, rest)) = target_raw.trim_start_matches(':').split_once(':') {
                let key = pfx.trim().to_lowercase().replace(' ', "_");
                let rest = rest.trim();
                if !rest.is_empty() && !key.is_empty() && crate::title::ns_by_name(pfx).is_none() {
                    let tpl = match self.iw.get(&key) {
                        Some(v) => v.clone(),
                        None => {
                            let v = self.res.interwiki(&key);
                            self.iw.insert(key.clone(), v.clone());
                            v
                        }
                    };
                    if let Some(tpl) = tpl {
                        let remote = collapse_ws(&rest.replace('_', " "));
                        if !inline_only && self.res.interwiki_is_language(&key) {
                            // [[fr:Titre]]: a language link, listed with the page, not shown inline
                            if !self.out.langlinks.iter().any(|(l, _)| *l == key) {
                                self.out.langlinks.push((key, remote));
                            }
                            return Some((String::new(), adv));
                        }
                        if self.seen_iw.insert((key.clone(), remote.clone())) {
                            self.out.iwlinks.push((key.clone(), remote));
                        }
                        let url = tpl.replace("$1", &path_encode(rest));
                        let label = match label_raw.map(str::trim).filter(|l| !l.is_empty()) {
                            Some(l) => self.inline_line(l, false),
                            None => esc_text(target_raw.trim_start_matches(':')),
                        };
                        return Some((format!("<a class=\"extiw\" rel=\"nofollow noopener\" href=\"{}\" title=\"{}\">{label}</a>", html_escape(&url), html_escape(target_raw.trim_start_matches(':'))), adv));
                    }
                }
            }
        }
        let lead_colon = target_raw.starts_with(':');
        let parsed = Title::parse(target_raw).ok()?;
        let title = parsed.title;

        if !lead_colon && title.ns == NS_CATEGORY {
            if self.seen_cats.insert(title.name.clone()) {
                let key = label_raw.map(|l| l.trim().to_string()).unwrap_or_default();
                self.out.categories.push((title.name.clone(), key));
            }
            return Some((String::new(), adv));
        }
        if !lead_colon && title.ns == NS_FILE {
            return Some((self.file_embed(&title, label_raw), adv));
        }

        let exists = self.is_exists(&title);
        if self.seen_links.insert(title.clone()) {
            self.out.links.push(title.clone());
        }
        let mut label = match label_raw.map(str::trim).filter(|l| !l.is_empty()) {
            Some(l) => self.inline_line(l, false),
            None => esc_text(target_raw.trim_start_matches(':')),
        };
        if label_raw.is_none() {
            let trail: String = rest[adv..].chars().take_while(|c| c.is_ascii_lowercase()).collect();
            if !trail.is_empty() {
                label.push_str(&trail);
                adv += trail.len();
            }
        }
        let frag = parsed.fragment.map(|f| format!("#{}", anchor_id(&f))).unwrap_or_default();
        let href = format!("{}{}", self.article_url(&title), frag);
        let class = if exists { "" } else { " class=\"new\"" };
        Some((format!("<a href=\"{}\" title=\"{}\"{class}>{label}</a>", html_escape(&href), html_escape(&title.full())), adv))
    }

    /// `[[File:Name.png|thumb|200px|left|alt=...|Caption]]`
    fn file_embed(&mut self, title: &Title, opts: Option<&str>) -> String {
        let (mut frame, mut width, mut height, mut align, mut alt, mut caption, mut border) = (false, None::<u32>, None::<u32>, "", None::<&str>, None::<&str>, false);
        let (mut page, mut start) = (0u32, None::<f64>);
        for o in opts.map(|o| o.split('|').map(str::trim).collect::<Vec<_>>()).unwrap_or_default() {
            let lower = o.to_ascii_lowercase();
            match lower.as_str() {
                "thumb" | "thumbnail" | "frame" | "framed" => frame = true,
                "frameless" | "upright" => {}
                "border" => border = true,
                "left" => align = "left",
                "right" => align = "right",
                "center" | "centre" => align = "center",
                "none" => align = "none",
                _ => {
                    if let Some(px) = lower.strip_suffix("px") {
                        let (w, h) = px.split_once('x').map_or((px, ""), |(w, h)| (w, h));
                        if (w.is_empty() || w.bytes().all(|b| b.is_ascii_digit())) && (h.is_empty() || h.bytes().all(|b| b.is_ascii_digit())) && !(w.is_empty() && h.is_empty()) {
                            width = w.parse().ok().filter(|n| (1..=10_000).contains(n));
                            height = h.parse().ok().filter(|n| (1..=10_000).contains(n));
                            continue;
                        }
                    }
                    if let Some(v) = o.strip_prefix("alt=") {
                        alt = Some(v.trim());
                    } else if let Some(v) = lower.strip_prefix("page=") {
                        page = v.trim().parse().ok().filter(|p| (1..=100_000).contains(p)).unwrap_or(0);
                    } else if let Some(v) = lower.strip_prefix("thumbtime=").or_else(|| lower.strip_prefix("start=")) {
                        start = parse_media_time(v.trim());
                    } else if lower.starts_with("link=") || lower.starts_with("upright=") || lower.starts_with("class=") || lower.starts_with("lang=") {
                    } else if !o.is_empty() {
                        caption = Some(o);
                    }
                }
            }
        }
        let mime = self.res.file_mime(&title.name);
        let media = mime.as_deref().and_then(crate::media::kind);
        if (frame || media.is_some_and(|k| k != crate::media::Kind::Audio)) && width.is_none() && height.is_none() {
            width = Some(if media == Some(crate::media::Kind::Video) { 320 } else { 220 });
        }
        let exists = self.is_exists(title);
        if self.seen_links.insert(title.clone()) {
            self.out.links.push(title.clone());
        }
        let file_url = self.o.file_path.replace("$1", &path_encode(&title.name));
        if let Some(k) = media {
            let previewable = mime.as_deref().is_some_and(|m| crate::media::tools().can_preview(m));
            let sep = if file_url.contains('?') { '&' } else { '?' };
            let w = width.unwrap_or(220);
            let html = match k {
                crate::media::Kind::Video => {
                    let poster = if previewable {
                        let t = start.map(|s| format!("&time={s}")).unwrap_or_default();
                        format!(" poster=\"{}\"", html_escape(&format!("{file_url}{sep}width={w}{t}")))
                    } else {
                        String::new()
                    };
                    let frag = start.map(|s| format!("#t={s}")).unwrap_or_default();
                    format!(
                        "<video class=\"mw-file\" controls preload=\"none\" width=\"{w}\"{poster} src=\"{}\" data-file=\"{}\"></video>",
                        html_escape(&format!("{file_url}{frag}")),
                        html_escape(&title.name)
                    )
                }
                crate::media::Kind::Audio => {
                    format!("<audio class=\"mw-file\" controls preload=\"none\" src=\"{}\" data-file=\"{}\"></audio>", html_escape(&file_url), html_escape(&title.name))
                }
                crate::media::Kind::Pdf => {
                    let label = alt.or(caption).unwrap_or(&title.name);
                    if previewable {
                        let p = if page > 1 { format!("&page={page}") } else { String::new() };
                        format!(
                            "<a class=\"mw-file-link\" href=\"{}\"><img class=\"mw-file\" src=\"{}\" alt=\"{}\" width=\"{w}\" loading=\"lazy\" data-file=\"{}\"></a>",
                            html_escape(&file_url),
                            html_escape(&format!("{file_url}{sep}width={w}{p}")),
                            html_escape(label),
                            html_escape(&title.name)
                        )
                    } else {
                        format!("<a class=\"mw-file-link\" href=\"{}\" data-file=\"{}\">{}</a>", html_escape(&file_url), html_escape(&title.name), html_escape(label))
                    }
                }
            };
            if !frame {
                return if align.is_empty() || align == "none" { html } else { format!("<span class=\"mw-align-{align}\">{html}</span>") };
            }
            let cap = caption.map(|c| self.inline_line(c, true)).unwrap_or_default();
            let a = if align.is_empty() { "right" } else { align };
            return self.put(format!("<figure class=\"mw-thumb mw-align-{a}\">{html}<figcaption>{cap}</figcaption></figure>"), true);
        }
        let mut src = file_url;
        let mut dims = String::new();
        if width.is_some() || height.is_some() {
            let sep = if src.contains('?') { '&' } else { '?' };
            let q: Vec<String> = [width.map(|w| format!("width={w}")), height.map(|h| format!("height={h}"))].into_iter().flatten().collect();
            src.push(sep);
            src.push_str(&q.join("&"));
            if let Some(w) = width {
                dims.push_str(&format!(" width=\"{w}\""));
            }
        }
        let alt_text = alt.or(caption).unwrap_or(&title.name);
        let mut cls = String::from(if exists { "mw-file" } else { "mw-file new" });
        if border {
            cls.push_str(" mw-border");
        }
        let img = format!("<img class=\"{cls}\" src=\"{}\" alt=\"{}\"{dims} loading=\"lazy\" data-file=\"{}\">", html_escape(&src), html_escape(alt_text), html_escape(&title.name));
        if !frame {
            return if align.is_empty() || align == "none" { img } else { format!("<span class=\"mw-align-{align}\">{img}</span>") };
        }
        let cap = caption.map(|c| self.inline_line(c, true)).unwrap_or_default();
        let a = if align.is_empty() { "right" } else { align };
        let fig = format!("<figure class=\"mw-thumb mw-align-{a}\">{img}<figcaption>{cap}</figcaption></figure>");
        self.put(fig, true)
    }

    fn note_extlink(&mut self, url: &str) {
        if self.out.extlinks.len() < 2_000 && self.seen_ext.insert(url.to_string()) {
            self.out.extlinks.push(url.to_string());
        }
    }

    fn extlink(&mut self, rest: &str) -> Option<(String, usize)> {
        let close = rest.find(']')?;
        let inner = &rest[1..close];
        if !(inner.starts_with("http://") || inner.starts_with("https://")) {
            return None;
        }
        let (url, label) = match inner.find(char::is_whitespace) {
            Some(p) => (&inner[..p], inner[p..].trim()),
            None => (inner, ""),
        };
        if url.chars().any(|c| matches!(c, '"' | '<' | '>' | PH_OPEN)) {
            return None;
        }
        self.note_extlink(url);
        let label_html = if label.is_empty() {
            self.ext_counter += 1;
            format!("[{}]", self.ext_counter)
        } else {
            self.inline_line(label, false)
        };
        Some((format!("<a class=\"external\" rel=\"nofollow noopener\" href=\"{}\">{label_html}</a>", html_escape(url)), close + 1))
    }

    // ----- finishing -------------------------------------------------------------------------

    fn finish(mut self, mut html: String) -> Rendered {
        if !self.refs.is_empty() {
            let list = self.refs_html();
            if self.refs_ph.is_empty() {
                html.push_str(&list);
            } else {
                let first = self.refs_ph[0];
                self.ph[first] = list;
            }
        }
        let html = self.restore(&html, 0);
        let text = collapse_ws(&strip_html(&html));
        let (ex, exh) = intro(&html);
        let mut r = self.out;
        r.html = html;
        r.text = text;
        r.extract = ex;
        r.extract_html = exh;
        r
    }
}

/// Gives a running Lua module access to the page being rendered.
struct LuaHost<'p, 'a> {
    p: &'p mut P<'a>,
    /// Arguments of the `#invoke` frame (what `{{{1}}}` means inside `frame:preprocess`).
    args: &'p Args,
    depth: usize,
}

impl crate::lua::Host for LuaHost<'_, '_> {
    fn module_source(&mut self, name: &str) -> Option<String> {
        let t = Title::parse_in(name, crate::title::NS_MODULE).ok()?.title;
        if t.ns != crate::title::NS_MODULE {
            return None;
        }
        if self.p.seen_tpl.insert(t.clone()) {
            self.p.out.templates.push(t.clone());
        }
        self.p.source_of(&t)
    }
    fn stat(&mut self, what: &str, arg: &str) -> i64 {
        self.p.res.stat(what, arg)
    }
    fn tz(&self) -> String {
        self.p.o.tz.to_string()
    }
    fn lang(&self) -> String {
        self.p.o.lang.to_string()
    }
    fn preprocess(&mut self, text: &str) -> String {
        if self.p.calls > MAX_CALLS || self.p.bytes > MAX_EXPANSION_BYTES {
            return String::new();
        }
        let t = self.p.protect_blocks(&strip_comments(text));
        let r = self.p.expand(&t, self.args, self.depth);
        self.p.bytes += r.len();
        r
    }
    fn expand_template(&mut self, title: &str, args: &HashMap<String, String>) -> Result<String, String> {
        self.p.calls += 1;
        if self.p.calls > MAX_CALLS || self.p.bytes > MAX_EXPANSION_BYTES {
            return Err("expansion limit reached".into());
        }
        let t = Title::parse_in(title, NS_TEMPLATE).map_err(|e| format!("invalid title: {e}"))?.title;
        self.p.transclude(t.clone(), args.clone(), self.depth).map_err(|_| format!("template \"{}\" does not exist or loops", t.full()))
    }
    fn title_info(&mut self, text: &str, default_ns: i64) -> Option<crate::lua::TitleInfo> {
        let p = Title::parse_in(text, default_ns).ok()?;
        Some(crate::lua::TitleInfo {
            ns: p.title.ns,
            ns_text: crate::title::ns_name(p.title.ns).to_string(),
            text: p.title.name.clone(),
            prefixed: p.title.full(),
            fragment: p.fragment.unwrap_or_default(),
        })
    }
    fn page_exists(&mut self, prefixed: &str) -> bool {
        Title::parse(prefixed).map(|p| self.p.is_exists(&p.title)).unwrap_or(false)
    }
    fn page_content(&mut self, prefixed: &str) -> Option<String> {
        let t = Title::parse(prefixed).ok()?.title;
        // Reading another page makes this one depend on it, like a transclusion.
        if self.p.seen_tpl.insert(t.clone()) {
            self.p.out.templates.push(t.clone());
        }
        self.p.res.template_source(&t)
    }
    fn url_for(&mut self, prefixed: &str) -> String {
        Title::parse(prefixed).map(|p| self.p.article_url(&p.title)).unwrap_or_default()
    }
    fn now(&self) -> i64 {
        self.p.o.now
    }
}

/// `90`, `1:30`, `0:01:30.5` -> seconds.
fn parse_media_time(v: &str) -> Option<f64> {
    let mut total = 0.0;
    for part in v.split(':') {
        let n: f64 = part.parse().ok().filter(|n: &f64| n.is_finite() && *n >= 0.0)?;
        total = total * 60.0 + n;
    }
    (total <= 1e7).then_some((total * 10.0).round() / 10.0)
}

fn esc_text_nowiki(s: &str) -> String {
    html_escape(s)
}

fn split_dt(s: &str) -> (&str, Option<&str>) {
    let b = s.as_bytes();
    let mut depth = 0i32;
    for (i, &c) in b.iter().enumerate() {
        match c {
            b'[' => depth += 1,
            b']' => depth -= 1,
            b':' if depth <= 0 && !s[i..].starts_with("://") => return (s[..i].trim(), Some(s[i + 1..].trim())),
            _ => {}
        }
    }
    (s, None)
}

fn split_cells<'a>(s: &'a str, seps: &[&str]) -> Vec<&'a str> {
    let b = s.as_bytes();
    let (mut parts, mut start, mut sq, mut j) = (Vec::new(), 0usize, 0i32, 0usize);
    while j < b.len() {
        if b[j] == b'[' && b.get(j + 1) == Some(&b'[') {
            sq += 1;
            j += 2;
            continue;
        }
        if b[j] == b']' && b.get(j + 1) == Some(&b']') {
            sq -= 1;
            j += 2;
            continue;
        }
        if sq <= 0 {
            if let Some(sep) = seps.iter().find(|sp| b[j..].starts_with(sp.as_bytes())) {
                parts.push(&s[start..j]);
                j += sep.len();
                start = j;
                continue;
            }
        }
        j += 1;
    }
    parts.push(&s[start..]);
    parts
}

fn split_cell_attr(s: &str) -> (&str, &str) {
    let b = s.as_bytes();
    let mut sq = 0i32;
    for j in 0..b.len() {
        if b[j] == b'[' && b.get(j + 1) == Some(&b'[') {
            sq += 1;
        } else if b[j] == b']' && j > 0 && b[j - 1] == b']' {
            sq -= 1;
        } else if b[j] == b'|' && sq <= 0 && b.get(j + 1) != Some(&b'|') {
            let attrs = &s[..j];
            if !attrs.contains("[[") && attrs.contains('=') {
                return (attrs, &s[j + 1..]);
            }
            return ("", s);
        }
    }
    ("", s)
}

fn assemble(toks: Vec<Tok>) -> String {
    let mut out = String::new();
    let mut st: Vec<char> = Vec::new();
    for t in toks {
        match t {
            Tok::Html(h) => out.push_str(&h),
            Tok::Text(t) => quotes(&mut out, &t, &mut st),
        }
    }
    while let Some(c) = st.pop() {
        out.push_str(if c == 'b' { "</b>" } else { "</i>" });
    }
    out
}

fn tag(c: char, open: bool) -> &'static str {
    match (c, open) {
        ('b', true) => "<b>",
        ('b', false) => "</b>",
        ('i', true) => "<i>",
        _ => "</i>",
    }
}

fn toggle(out: &mut String, st: &mut Vec<char>, c: char) {
    if let Some(pos) = st.iter().rposition(|x| *x == c) {
        let above: Vec<char> = st.split_off(pos + 1);
        for a in above.iter().rev() {
            out.push_str(tag(*a, false));
        }
        st.pop();
        out.push_str(tag(c, false));
        for a in above {
            out.push_str(tag(a, true));
            st.push(a);
        }
    } else {
        st.push(c);
        out.push_str(tag(c, true));
    }
}

fn quotes(out: &mut String, text: &str, st: &mut Vec<char>) {
    let b = text.as_bytes();
    let (mut i, mut start) = (0usize, 0usize);
    while i < b.len() {
        if b[i] != b'\'' {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < b.len() && b[j] == b'\'' {
            j += 1;
        }
        let mut n = j - i;
        if n >= 2 {
            out.push_str(&esc_text(&text[start..i]));
            if n == 4 {
                out.push_str("&#39;");
                n = 3;
            } else if n > 5 {
                for _ in 0..n - 5 {
                    out.push_str("&#39;");
                }
                n = 5;
            }
            match n {
                2 => toggle(out, st, 'i'),
                3 => toggle(out, st, 'b'),
                _ => {
                    let (hb, hi) = (st.contains(&'b'), st.contains(&'i'));
                    match (hb, hi) {
                        (false, false) => {
                            toggle(out, st, 'i');
                            toggle(out, st, 'b');
                        }
                        (true, true) => {
                            let top = *st.last().unwrap();
                            toggle(out, st, top);
                            let other = if top == 'b' { 'i' } else { 'b' };
                            toggle(out, st, other);
                        }
                        (true, false) => {
                            toggle(out, st, 'b');
                            toggle(out, st, 'i');
                        }
                        (false, true) => {
                            toggle(out, st, 'i');
                            toggle(out, st, 'b');
                        }
                    }
                }
            }
            start = j;
        }
        i = j;
    }
    out.push_str(&esc_text(&text[start..]));
}

fn remove_ref_sups(html: &str) -> String {
    let mut out = String::new();
    let mut rest = html;
    while let Some(p) = rest.find("<sup id=\"cite_ref") {
        out.push_str(&rest[..p]);
        rest = match rest[p..].find("</sup>") {
            Some(e) => &rest[p + e + 6..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

fn intro(html: &str) -> (String, String) {
    let mut rest = html;
    while let Some(p) = rest.find("<p>") {
        let after = &rest[p + 3..];
        let Some(e) = after.find("</p>") else { break };
        let inner = remove_ref_sups(&after[..e]);
        let text = collapse_ws(&strip_html(&inner));
        if !text.is_empty() {
            return (text, inner);
        }
        rest = &after[e + 4..];
    }
    (String::new(), String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake(HashMap<String, String>);
    impl Resolver for Fake {
        fn template_source(&mut self, t: &Title) -> Option<String> {
            self.0.get(&t.full()).cloned()
        }
        fn page_exists(&mut self, t: &Title) -> bool {
            t.name == "Exists" || t.name == "A.png"
        }
        fn interwiki(&mut self, prefix: &str) -> Option<String> {
            match prefix {
                "wp" => Some("https://en.example.org/wiki/$1".to_string()),
                "fr" => Some("https://fr.example.org/wiki/$1".to_string()),
                _ => None,
            }
        }
        fn interwiki_is_language(&mut self, prefix: &str) -> bool {
            prefix == "fr"
        }
        fn file_mime(&mut self, name: &str) -> Option<String> {
            match name.rsplit('.').next()? {
                "png" => Some("image/png".into()),
                "pdf" => Some("application/pdf".into()),
                "mp4" => Some("video/mp4".into()),
                "mp3" => Some("audio/mpeg".into()),
                _ => None,
            }
        }
        fn stat(&mut self, what: &str, arg: &str) -> i64 {
            match (what, arg) {
                ("pages", _) => 42,
                ("cat_pages", "Greek") => 3,
                ("ns", "10") => 7,
                _ => 0,
            }
        }
        fn interwiki_transcludes(&mut self, prefix: &str) -> bool {
            prefix == "wp"
        }
        fn remote_template(&mut self, prefix: &str, title: &str) -> Option<String> {
            match (prefix, title) {
                ("wp", "Outer") => Some("outer[{{Inner|{{{1}}}}}] {{Local}} {{#invoke:RemoteMod|f}}".into()),
                ("wp", "Template:Inner") => Some("remote-inner:{{{1}}}".into()),
                ("wp", "Module:RemoteMod") => Some("local p = {} function p.f() return 'remote-lua' .. require('Module:RemoteDep').x end return p".into()),
                ("wp", "Module:RemoteDep") => Some("return { x = '+dep' }".into()),
                ("wp", "Remote box") => Some("<b>remote</b> {{{1|none}}} {{Local}} <script>x</script><noinclude>doc</noinclude>".into()),
                ("wp", "Loop") => Some("{{wp:Loop}}".into()),
                _ => None,
            }
        }
    }

    fn r(src: &str, tpls: &[(&str, &str)]) -> Rendered {
        let page = Title::new(0, "Test page");
        let o = Opts { page: &page, site_name: "TestWiki", article_path: "/wiki/$1", file_path: "/files/$1", now: 1_794_553_200, tz: "Europe/Berlin", lang: "en" };
        let mut f = Fake(tpls.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect());
        render(src, &o, &mut f)
    }

    #[test]
    fn inline_formatting() {
        let h = r("''it'' and '''bold''' and '''''both'''''", &[]).html;
        assert_eq!(h.trim(), "<p><i>it</i> and <b>bold</b> and <i><b>both</b></i></p>");
    }

    #[test]
    fn xss_is_escaped() {
        let h = r("<script>alert(1)</script> <b onclick=x>hi</b> [http://a.com\" onmouseover=\"x y]", &[]).html;
        assert!(!h.contains("<script"), "{h}");
        assert!(!h.contains("<b onclick"), "{h}");
        assert!(!h.contains("onmouseover=\"x"), "{h}");
    }

    #[test]
    fn links_and_categories() {
        let x = r("See [[Exists]], [[Missing|a label]]s and [[Category:Cats|Sort]] [[:Category:Cats]].", &[]);
        assert!(x.html.contains("<a href=\"/wiki/Exists\" title=\"Exists\">Exists</a>"), "{}", x.html);
        assert!(x.html.contains("class=\"new\">a label</a>"), "{}", x.html);
        assert_eq!(x.categories, vec![("Cats".to_string(), "Sort".to_string())]);
        assert!(x.links.iter().any(|t| t.name == "Missing"));
        assert!(x.links.iter().any(|t| t.key() == "Category:Cats"));
    }

    #[test]
    fn headings_sections_numbering() {
        let x = r("lead\n== A ==\n=== B ===\n== A ==\ntext", &[]);
        let nums: Vec<_> = x.sections.iter().map(|s| (s.number.clone(), s.anchor.clone())).collect();
        assert_eq!(nums, vec![("1".into(), "A".into()), ("1.1".into(), "B".into()), ("2".into(), "A_2".into())]);
    }

    #[test]
    fn lists_nested() {
        let h = r("* a\n** b\n* c\n# one\n# two", &[]).html;
        assert_eq!(
            h.replace('\n', ""),
            "<ul><li>a<ul><li>b</li></ul></li><li>c</li></ul><ol><li>one</li><li>two</li></ol>"
        );
    }

    #[test]
    fn definition_list() {
        let h = r("; term : def", &[]).html.replace('\n', "");
        assert_eq!(h, "<dl><dt>term</dt><dd>def</dd></dl>");
    }

    #[test]
    fn templates_params_and_functions() {
        let t = [
            ("Template:Hello", "Hi {{{1}}}, you are {{{age|unknown}}}.<noinclude>DOC</noinclude>"),
            ("Template:Pick", "{{#switch: {{{1}}} | a = Apple | b | c = BC | #default = other}}"),
            ("Template:Loop", "{{Loop}}"),
        ];
        assert_eq!(r("{{Hello|Bob|age=30}}", &t).text, "Hi Bob, you are 30.");
        assert_eq!(r("{{Hello|Bob}}", &t).text, "Hi Bob, you are unknown.");
        assert_eq!(r("{{Pick|b}} {{Pick|a}} {{Pick|z}}", &t).text, "BC Apple other");
        assert_eq!(r("{{#if: x | yes | no}} {{#if:|yes|no}} {{#ifeq: 1 | 1.0 | eq | ne}}", &t).text, "yes no eq");
        assert!(r("{{Loop}}", &t).text.contains("loop detected"));
        let x = r("{{Hello|A}} {{Nope}}", &t);
        assert_eq!(x.templates.len(), 2);
        assert!(x.html.contains("Template:Nope"));
        assert_eq!(r("{{PAGENAME}} {{CURRENTYEAR}} {{SITENAME}}", &t).text, "Test page 2026 TestWiki");
    }

    #[test]
    fn template_bomb_is_bounded() {
        let t = [("Template:B", "{{B2}}{{B2}}{{B2}}{{B2}}{{B2}}{{B2}}{{B2}}{{B2}}"), ("Template:B2", "{{B}}{{B}}{{B}}{{B}}{{B}}{{B}}{{B}}{{B}}")];
        let start = std::time::Instant::now();
        let _ = r("{{B}}", &t);
        assert!(start.elapsed().as_secs() < 5);
    }

    #[test]
    fn nowiki_pre_and_comments() {
        let x = r("a <!-- hidden --> <nowiki>''no'' [[x]] {{y}}</nowiki>\n\n <pre>\n<b>raw</b></pre>", &[]);
        assert!(x.html.contains("&#39;&#39;no&#39;&#39; [[x]] {{y}}"), "{}", x.html);
        assert!(x.html.contains("&lt;b&gt;raw&lt;/b&gt;"), "{}", x.html);
        assert!(!x.html.contains("hidden"));
    }

    #[test]
    fn tables() {
        let x = r("{| class=\"wikitable\" onclick=\"x\"\n|+ Cap\n! A !! B\n|-\n| 1 || colspan=\"2\" | 2\n|}", &[]);
        let h = x.html.replace('\n', "");
        assert_eq!(h, "<table class=\"wikitable\"><caption>Cap</caption><tr><th>A</th><th>B</th></tr><tr><td>1</td><td colspan=\"2\">2</td></tr></table>");
    }

    #[test]
    fn refs_not_wrapped_in_p() {
        let x = r("A.<ref>S</ref>\n<references/>\n[[Category:X]]", &[]);
        assert!(!x.html.contains("<p><ol"), "{}", x.html);
        assert!(!x.html.contains("</ol>\n</p>"), "{}", x.html);
    }

    #[test]
    fn refs() {
        let x = r("Fact.<ref>Source one</ref> More.<ref name=a>Src A</ref><ref name=a/>\n\n<references/>", &[]);
        assert!(x.html.contains("cite_note-2"), "{}", x.html);
        assert!(x.html.contains("cite_ref-2-1"), "{}", x.html);
        assert!(x.html.contains("Src A"), "{}", x.html);
        assert_eq!(x.extract, "Fact. More.");
    }

    #[test]
    fn redirect() {
        let x = r("#REDIRECT [[Target page#Sec]]", &[]);
        let (t, f) = x.redirect.unwrap();
        assert_eq!(t.name, "Target page");
        assert_eq!(f.as_deref(), Some("Sec"));
    }

    #[test]
    fn raw_section_ranges() {
        let raw = "lead\n== A ==\na\n=== A1 ===\nx\n== B ==\nb\n";
        let s = raw_sections(raw);
        assert_eq!(s.len(), 3);
        assert_eq!(&raw[s[0].start..s[0].end], "== A ==\na\n=== A1 ===\nx\n");
        assert_eq!(&raw[s[1].start..s[1].end], "=== A1 ===\nx\n");
        assert_eq!(&raw[s[2].start..s[2].end], "== B ==\nb\n");
    }

    #[test]
    fn autolink_and_entities() {
        let h = r("go https://example.com/x?a=1&b=2. &copy; & more", &[]).html;
        assert!(h.contains("href=\"https://example.com/x?a=1&amp;b=2\""), "{h}");
        assert!(h.contains("&copy; &amp; more"), "{h}");
    }

    #[test]
    fn expr_and_string_functions() {
        let t = [("Template:T", "{{{1}}}")];
        assert_eq!(r("{{#expr: 2*(3+4)}} {{#ifexpr: 1>2 | yes | no}} {{#iferror: {{#expr: 1/0}} | bad | good}}", &t).text, "14 no bad");
        assert!(r("{{#expr: 1/0}}", &t).text.contains("Expression error: Division by zero"));
        assert_eq!(r("{{#time: j F Y | 2024-03-05}} {{#time: Y}} {{#expr: 7.5 fmod 2}} {{#iferror: {{#time: Y | nonsense}} | bad | ok}}", &t).text, "5 March 2024 2026 1.5 bad");
        assert_eq!(r("{{padleft:7|3|0}} {{padright:ab|4|-}} {{formatnum:1234567.5}} {{#len:héllo}} {{#sub:abcdef|1|3}} {{#replace:a-b|-|+}} {{plural:1|is|are}}", &t).text, "007 ab-- 1,234,567.5 5 bcd a+b is");
    }

    #[test]
    fn invoke_runs_lua_modules() {
        let t = [
            ("Module:Greet", "local p={} function p.hi(f) return 'Hi '..(f.args[1] or '?')..' '..mw.text.trim(f.args.x or '') end return p"),
            ("Module:Bad", "local p={} function p.f() error('boom') end return p"),
            ("Template:Wrap", "{{#invoke:Greet|hi|{{{1}}}|x= y }}"),
        ];
        assert_eq!(r("{{#invoke:Greet|hi|Bob|x= there }}", &t).text, "Hi Bob there");
        assert_eq!(r("{{Wrap|Ann}}", &t).text, "Hi Ann y");
        let bad = r("{{#invoke:Bad|f}}", &t);
        assert!(bad.text.contains("Script error"), "{}", bad.text);
        assert!(bad.html.contains("class=\"error\""));
        assert!(r("{{#invoke:Nope|f}}", &t).text.contains("no such module"));
        assert!(!r("{{#invoke:Greet|hi|<script>}}", &t).html.contains("<script"));
    }

    #[test]
    fn interwiki_and_file_embeds() {
        let h = r("[[wp:Rust (language)|Rust]] [[nope:Foo]] [[:wp:Skip]] [[File:a.png|thumb|A cat]]", &[]).html;
        assert!(h.contains("href=\"https://en.example.org/wiki/Rust_(language)\" title=\"wp:Rust (language)\">Rust</a>"), "{h}");
        assert!(h.contains("class=\"new\">nope:Foo"), "{h}");
        assert!(h.contains("<figure class=\"mw-thumb mw-align-right\"><img class=\"mw-file\" src=\"/files/A.png?width=220\" alt=\"A cat\" width=\"220\""), "{h}");
        assert!(h.contains("<figcaption>A cat</figcaption></figure>"), "{h}");
        // plain embeds, explicit sizes, alignment, missing files
        let h = r("[[File:a.png]] [[File:a.png|120px|alt=Alt text]] [[File:a.png|x80px|left]] [[File:missing.png|50x60px]]", &[]).html;
        assert!(h.contains("<img class=\"mw-file\" src=\"/files/A.png\" alt=\"A.png\" loading"), "{h}");
        assert!(h.contains("src=\"/files/A.png?width=120\" alt=\"Alt text\" width=\"120\""), "{h}");
        assert!(h.contains("<span class=\"mw-align-left\"><img class=\"mw-file\" src=\"/files/A.png?height=80\""), "{h}");
        assert!(h.contains("<img class=\"mw-file new\" src=\"/files/Missing.png?width=50&amp;height=60\""), "{h}");
        // a figure alone on its line is not wrapped in a paragraph
        let h = r("Intro\n\n[[File:a.png|thumb|Caption with ''markup'']]\n\nOutro", &[]).html;
        assert!(h.contains("</p>\n<figure") && h.contains("<figcaption>Caption with <i>markup</i></figcaption></figure>\n<p>Outro"), "{h}");
    }

    #[test]
    fn raw_html_blocks_and_attributes() {
        let h = r("<div class=\"box\" style=\"color:red; position:fixed; background:url(x)\" onclick=\"evil()\" id=\"x\">\nInside ''text''\n</div>\nAfter", &[]).html;
        assert!(h.contains("<div class=\"box\" style=\"color:red\">\n<p>Inside <i>text</i></p>\n</div>\n<p>After</p>"), "{h}");
        assert!(!h.contains("onclick") && !h.contains("position") && !h.contains("url("), "{h}");
        // single-line blocks render their text without a paragraph, tables work as raw HTML too
        let h = r("<table class=\"t\"><tr><th colspan=\"2\" scope=\"col\">Head</th></tr><tr><td>[[Exists]]</td><td style=\"text-align:right\">2</td></tr></table>", &[]).html;
        assert!(h.contains("<table class=\"t\"><tr><th colspan=\"2\" scope=\"col\">Head</th></tr><tr><td><a href=\"/wiki/Exists\""), "{h}");
        assert!(h.contains("<td style=\"text-align:right\">2</td></tr></table>"), "{h}");
        // unclosed and stray tags can never leak out of the page
        let h = r("<div><div>deep\n</span></table></div></div></div>stray", &[]).html;
        assert_eq!(h.matches("<div>").count(), h.matches("</div>").count(), "{h}");
        assert!(!h.contains("</table>") && !h.contains("</span>"), "{h}");
        let h = r("<div class=\"a\">never closed\n* item <div>in list", &[]).html;
        assert_eq!(h.matches("<div").count(), h.matches("</div>").count(), "{h}");
        // inline tags keep safe attributes only
        let h = r("<span class=\"hl\" title=\"a &quot;b&quot;\" style=\"font-weight:bold;behavior:url(x)\" onmouseover=x>t</span> <abbr title=\"x\">a</abbr> <script>alert(1)</script> <img src=x onerror=y> <iframe src=x>", &[]).html;
        assert!(h.contains("<span class=\"hl\" style=\"font-weight:bold\" title=\"a &quot;b&quot;\">t</span>") || h.contains("<span class=\"hl\" title=\"a &quot;b&quot;\" style=\"font-weight:bold\">t</span>"), "{h}");
        assert!(h.contains("<abbr title=\"x\">a</abbr>") && !h.contains("<script") && !h.contains("<img") && !h.contains("<iframe") && !h.contains("onmouseover=x>"), "{h}");
        for bad in ["color:red;background-image:url(//x)", "width:expression(alert(1))", "color:\\72 ed", "top:0", "color:var(--x)", "font-family:'x'"] {
            assert_eq!(safe_style(bad).replace("color:red", ""), "", "{bad}");
        }
        assert_eq!(safe_style("COLOR : #fff ; margin-left: 1em; border-top:1px solid rgb(1,2,3)"), "color:#fff;margin-left:1em;border-top:1px solid rgb(1,2,3)");
    }

    #[test]
    fn lua_modules_use_the_wiki() {
        let t = [
            ("Template:Greet", "Hi {{{1}}}!"),
            ("Module:Util", "local u = {} function u.twice(s) return s .. s end return u"),
            ("Module:Data", "return { colour = 'red' }"),
            ("Module:M", "local p = {} local util = require('Module:Util')
                function p.tpl(f) return f:expandTemplate{ title = 'Greet', args = { f.args[1] } } end
                function p.pre(f) return f:preprocess('{{Greet|{{{1}}}}} {{#expr: 2+3}}') end
                function p.req(f) return util.twice('ab') .. mw.loadData('Module:Data').colour end
                function p.box(f) return mw.html.create('div'):addClass('infobox'):css('color', 'red'):tag('span'):wikitext(\"''x''\"):allDone() end
                function p.exists(f) return tostring(mw.title.new('Exists').exists) .. tostring(mw.title.new('Nope').exists) end
                function p.evil(f) return '<script>alert(1)</script><div onclick=\"x\">d</div>' end
                function p.loop(f) return f:preprocess('{{#invoke:M|loop}}') end
                function p.pf(f) return f:callParserFunction('#expr', '6*7') .. f:callParserFunction{ name = 'uc', args = { 'abc' } } end
                return p"),
        ];
        assert_eq!(r("{{#invoke:M|tpl|Ann}}", &t).text, "Hi Ann!");
        assert_eq!(r("{{#invoke:M|pre|Bob}}", &t).text, "Hi Bob! 5");
        assert_eq!(r("{{#invoke:M|req}}", &t).text, "ababred");
        assert_eq!(r("{{#invoke:M|exists}}", &t).text, "truefalse");
        assert_eq!(r("{{#invoke:M|pf}}", &t).text, "42ABC");
        let h = r("{{#invoke:M|box}}", &t).html;
        assert!(h.contains("<div class=\"infobox\" style=\"color:red\"><span><i>x</i></span></div>"), "{h}");
        let h = r("{{#invoke:M|evil}}", &t).html;
        assert!(!h.contains("<script") && !h.contains("onclick") && h.contains("<div>d</div>"), "{h}");
        // required modules and expanded templates are recorded as dependencies
        let deps = r("{{#invoke:M|req}}", &t).templates;
        assert!(deps.contains(&Title::new(crate::title::NS_MODULE, "Util")) && deps.contains(&Title::new(crate::title::NS_MODULE, "Data")));
        // runaway recursion through preprocess ends with an error instead of hanging
        let rec = r("{{#invoke:M|loop}}", &t);
        assert!(rec.text.contains("Script error"), "{}", rec.text);
    }

    #[test]
    fn interwiki_transclusion_and_link_tracking() {
        let t = [("Template:Local", "LOCAL")];
        let out = r("{{wp:Remote box|arg}} {{wp:Missing}} {{wp:Loop}} [[wp:Some page]] [https://example.org/a x] https://example.org/b [https://example.org/a again]", &t);
        assert!(out.html.contains("<b>remote</b> arg LOCAL &lt;script&gt;x&lt;/script&gt;"), "{}", out.html);
        assert!(!out.html.contains("doc"), "{}", out.html);
        assert!(out.text.contains("Template loop detected"), "{}", out.text);
        assert_eq!(out.extlinks, vec!["https://example.org/a".to_string(), "https://example.org/b".to_string()]);
        assert!(out.iwlinks.contains(&("wp".to_string(), "Remote box".to_string())) && out.iwlinks.contains(&("wp".to_string(), "Some page".to_string())), "{:?}", out.iwlinks);
        // no more than MAX_REMOTE_CALLS remote fetches per page
        let many = "{{wp:Remote box}}".repeat(100);
        let n = r(&many, &t).html.matches("<b>remote</b>").count();
        assert!((10..=MAX_REMOTE_CALLS).contains(&n), "{n}");
    }

    #[test]
    fn remote_context_language_links_and_local_time() {
        let t = [("Template:Local", "LOCAL"), ("Template:Inner", "local-inner")];
        // templates and modules used by remote text come from the remote wiki; others fall back to local
        assert_eq!(r("{{wp:Outer|x}} / {{Inner|y}}", &t).text, "outer[remote-inner:x] LOCAL remote-lua+dep / local-inner");
        // language links are collected, not rendered; a leading colon makes an ordinary link
        let out = r("Text [[fr:Bonjour le monde]] [[:fr:Lien visible]] [[:wp:Thing]] end", &t);
        assert_eq!(out.langlinks, vec![("fr".to_string(), "Bonjour le monde".to_string())]);
        assert!(!out.html.contains("Bonjour") && out.html.contains("href=\"https://fr.example.org/wiki/Lien_visible\" title=\"fr:Lien visible\">fr:Lien visible</a>") && out.html.contains("en.example.org/wiki/Thing"), "{}", out.html);
        // #time is UTC, #timel and LOCAL* use the wiki's zone (Europe/Berlin in tests; now = 07:00 UTC)
        assert_eq!(r("{{#time: H:i T}} {{#timel: H:i T}} {{LOCALTIME}} {{CURRENTTIME}} {{LOCALHOUR}}", &t).text, "07:00 UTC 08:00 CET 08:00 07:00 08");
        assert_eq!(r("{{#timel: Y-m-d H:i P | 2024-07-01 12:00}} {{#timel: H:i | 2024-07-01T12:00:00Z}}", &t).text, "2024-07-01 12:00 +02:00 14:00");
        // statistics reach modules
        let m = [("Module:S", "local p = {} function p.f() return mw.site.stats.pages .. ' ' .. mw.site.stats.pagesInCategory('Greek', 'pages') .. ' ' .. mw.site.stats.pagesInNamespace(10) .. ' ' .. mw.getContentLanguage():getCode() end return p")];
        assert_eq!(r("{{#invoke:S|f}}", &m).text, "42 3 7 en");
    }

    #[test]
    fn media_embeds() {
        let t = [];
        let h = r("[[File:Talk.mp4|thumb|start=1:05|The talk]] [[File:Song.mp3]] [[File:Report.pdf|page=3|180px]]", &t).html;
        assert!(h.contains("<video class=\"mw-file\" controls preload=\"none\" width=\"320\""), "{h}");
        assert!(h.contains("src=\"/files/Talk.mp4#t=65\" data-file=\"Talk.mp4\"></video><figcaption>The talk</figcaption></figure>"), "{h}");
        assert!(h.contains("<audio class=\"mw-file\" controls preload=\"none\" src=\"/files/Song.mp3\" data-file=\"Song.mp3\"></audio>"), "{h}");
        // previews depend on the tools installed where the test runs
        if crate::media::tools().can_preview("application/pdf") {
            assert!(h.contains("<a class=\"mw-file-link\" href=\"/files/Report.pdf\"><img class=\"mw-file\" src=\"/files/Report.pdf?width=180&amp;page=3\""), "{h}");
        } else {
            assert!(h.contains("<a class=\"mw-file-link\" href=\"/files/Report.pdf\" data-file=\"Report.pdf\">Report.pdf</a>"), "{h}");
        }
        if crate::media::tools().can_preview("video/mp4") {
            assert!(h.contains("poster=\"/files/Talk.mp4?width=320&amp;time=65\""), "{h}");
        }
        assert_eq!(parse_media_time("90"), Some(90.0));
        assert_eq!(parse_media_time("0:01:30.25"), Some(90.3));
        assert_eq!(parse_media_time("x"), None);
    }

    #[test]
    fn expand_only_returns_wikitext() {
        let page = Title::new(0, "P");
        let o = Opts { page: &page, site_name: "S", article_path: "/wiki/$1", file_path: "/f/$1", now: 0, tz: "UTC", lang: "en" };
        let mut f = Fake([("Template:Hi".to_string(), "Hello {{{1}}}".to_string())].into_iter().collect());
        assert_eq!(expand_only("''{{Hi|Bob}}'' {{#expr:1+1}}", &o, &mut f), "''Hello Bob'' 2");
    }

    #[test]
    fn shortdesc() {
        let x = r("{{SHORTDESC:A test}}Hello", &[]);
        assert_eq!(x.description.as_deref(), Some("A test"));
        assert_eq!(x.extract, "Hello");
    }
}
