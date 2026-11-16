//! HTML → wikitext. A small tolerant HTML parser plus a converter for the subset this engine
//! renders (and common pasted HTML). Scripts/styles are dropped; unknown tags keep their text.

use crate::util::decode_entities;

#[derive(Debug)]
enum Node {
    Text(String),
    El { name: String, attrs: Vec<(String, String)>, kids: Vec<Node> },
}

const VOID: &[&str] = &["br", "hr", "img", "meta", "link", "input", "wbr", "col"];
const MAX_DEPTH: usize = 64;

struct Open {
    name: String,
    attrs: Vec<(String, String)>,
    kids: Vec<Node>,
}

fn parse(html: &str) -> Vec<Node> {
    let mut stack: Vec<Open> = vec![Open { name: String::new(), attrs: vec![], kids: vec![] }];
    let b = html.as_bytes();
    let mut i = 0;
    let close_top = |stack: &mut Vec<Open>| {
        let o = stack.pop().unwrap();
        stack.last_mut().unwrap().kids.push(Node::El { name: o.name, attrs: o.attrs, kids: o.kids });
    };
    while i < b.len() {
        if b[i] != b'<' {
            let e = html[i..].find('<').map_or(b.len(), |p| i + p);
            stack.last_mut().unwrap().kids.push(Node::Text(html[i..e].to_string()));
            i = e;
            continue;
        }
        let rest = &html[i..];
        if rest.starts_with("<!--") {
            i = rest.find("-->").map_or(b.len(), |p| i + p + 3);
            continue;
        }
        if rest.starts_with("<!") || rest.starts_with("<?") {
            i = rest.find('>').map_or(b.len(), |p| i + p + 1);
            continue;
        }
        let closing = rest.starts_with("</");
        let name_start = if closing { 2 } else { 1 };
        let name: String = rest[name_start..].chars().take_while(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase();
        if name.is_empty() {
            stack.last_mut().unwrap().kids.push(Node::Text("<".into()));
            i += 1;
            continue;
        }
        // find the end of the tag, honouring quotes
        let mut j = i + name_start + name.len();
        let mut q: Option<u8> = None;
        while j < b.len() {
            match (q, b[j]) {
                (None, b'>') => break,
                (None, c @ (b'"' | b'\'')) => q = Some(c),
                (Some(c), x) if x == c => q = None,
                _ => {}
            }
            j += 1;
        }
        let inner = &html[i + name_start + name.len()..j.min(b.len())];
        i = (j + 1).min(b.len());
        if closing {
            if let Some(pos) = stack.iter().rposition(|o| o.name == name) {
                if pos > 0 {
                    while stack.len() > pos {
                        close_top(&mut stack);
                    }
                }
            }
            continue;
        }
        let attrs = parse_attrs(inner.trim_end_matches('/'));
        if matches!(name.as_str(), "script" | "style") {
            let close = format!("</{name}");
            let lower = html[i..].to_ascii_lowercase();
            i = lower.find(&close).and_then(|p| html[i + p..].find('>').map(|g| i + p + g + 1)).unwrap_or(b.len());
            continue;
        }
        // implied end tags
        let implied: &[&str] = match name.as_str() {
            "li" => &["li"],
            "p" => &["p"],
            "td" | "th" => &["td", "th"],
            "tr" => &["td", "th", "tr"],
            "dt" | "dd" => &["dt", "dd"],
            _ => &[],
        };
        if !implied.is_empty() {
            let boundary: &[&str] = match name.as_str() {
                "li" => &["ul", "ol"],
                "td" | "th" => &["tr", "table"],
                "tr" => &["table"],
                "dt" | "dd" => &["dl"],
                _ => &["div", "td", "th", "li", "blockquote", "table"],
            };
            if let Some(pos) = stack.iter().rposition(|o| implied.contains(&o.name.as_str())) {
                if !stack[pos + 1..].iter().any(|o| boundary.contains(&o.name.as_str())) && pos > 0 {
                    while stack.len() > pos {
                        close_top(&mut stack);
                    }
                }
            }
        }
        if VOID.contains(&name.as_str()) || inner.trim_end().ends_with('/') {
            stack.last_mut().unwrap().kids.push(Node::El { name, attrs, kids: vec![] });
        } else if stack.len() < MAX_DEPTH {
            stack.push(Open { name, attrs, kids: vec![] });
        }
    }
    while stack.len() > 1 {
        close_top(&mut stack);
    }
    stack.pop().unwrap().kids
}

fn parse_attrs(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = s.trim();
    while !rest.is_empty() {
        let name_end = rest.find(|c: char| c == '=' || c.is_whitespace()).unwrap_or(rest.len());
        let name = rest[..name_end].to_ascii_lowercase();
        rest = rest[name_end..].trim_start();
        let mut val = String::new();
        if let Some(r) = rest.strip_prefix('=') {
            let r = r.trim_start();
            if let Some(q) = r.chars().next().filter(|c| *c == '"' || *c == '\'') {
                let end = r[1..].find(q).map_or(r.len(), |p| p + 1);
                val = r[1..end].to_string();
                rest = r.get(end + 1..).unwrap_or("").trim_start();
            } else {
                let end = r.find(char::is_whitespace).unwrap_or(r.len());
                val = r[..end].to_string();
                rest = r[end..].trim_start();
            }
        }
        if !name.is_empty() {
            out.push((name, decode_entities(&val)));
        }
    }
    out
}

fn attr<'a>(attrs: &'a [(String, String)], k: &str) -> Option<&'a str> {
    attrs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
}

pub struct Ctx<'a> {
    /// Same template as `ARTICLE_PATH` / `FILE_PATH` (`$1` = title) so links convert back to `[[...]]`.
    pub article_path: &'a str,
    pub file_path: &'a str,
}

fn from_template(href: &str, tpl: &str) -> Option<String> {
    let (pre, suf) = tpl.split_once("$1")?;
    let r = href.strip_prefix(pre)?;
    let r = if suf.is_empty() { r } else { r.strip_suffix(suf)? };
    Some(percent_encoding::percent_decode_str(r).decode_utf8_lossy().replace('_', " "))
}

fn needs_nowiki(t: &str) -> bool {
    ["[[", "]]", "{{", "}}", "''", "~~~", "<", "__", "{|", "|}"].iter().any(|p| t.contains(p))
}

fn escape_text(t: &str) -> String {
    let t = t.replace('\u{a0}', " ");
    if needs_nowiki(&t) { format!("<nowiki>{t}</nowiki>") } else { t }
}

fn collapse(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut sp = false;
    for c in s.chars() {
        if c.is_whitespace() {
            sp = true;
        } else {
            if sp {
                out.push(' ');
            }
            sp = false;
            out.push(c);
        }
    }
    if sp {
        out.push(' ');
    }
    out
}

fn plain(nodes: &[Node]) -> String {
    nodes
        .iter()
        .map(|n| match n {
            Node::Text(t) => decode_entities(t),
            Node::El { kids, .. } => plain(kids),
        })
        .collect()
}

fn inline(nodes: &[Node], cx: &Ctx) -> String {
    let mut out = String::new();
    for n in nodes {
        match n {
            Node::Text(t) => out.push_str(&escape_text(&collapse(&decode_entities(t)))),
            Node::El { name, attrs, kids } => match name.as_str() {
                "b" | "strong" => out.push_str(&wrap("'''", &inline(kids, cx))),
                "i" | "em" | "cite" => out.push_str(&wrap("''", &inline(kids, cx))),
                "u" | "s" | "sub" | "sup" | "small" | "big" | "code" | "tt" | "kbd" | "var" | "samp" | "del" | "ins" | "strike" => {
                    let tag = if name == "strike" { "s" } else { name.as_str() };
                    out.push_str(&format!("<{tag}>{}</{tag}>", inline(kids, cx)));
                }
                "br" => out.push_str("<br>"),
                // a linked PDF preview: <a href=file><img data-file=...></a>
                "a" if kids.len() == 1 && matches!(&kids[0], Node::El { name, attrs: ia, .. } if name == "img" && attr(ia, "data-file").is_some()) => {
                    if let Node::El { attrs: ia, .. } = &kids[0] {
                        out.push_str(&file_embed(ia, cx, &[], None).unwrap_or_default());
                    }
                }
                "a" if attr(attrs, "data-file").is_some() => {
                    out.push_str(&format!("[[File:{}]]", attr(attrs, "data-file").unwrap_or_default()));
                }
                "a" => out.push_str(&link(attrs, kids, cx)),
                "video" | "audio" if attr(attrs, "data-file").is_some() => if let Some(e) = file_embed(attrs, cx, &[], None) { out.push_str(&e) },
                "img" => match file_embed(attrs, cx, &[], None) {
                    Some(e) => out.push_str(&e),
                    None => out.push_str(&escape_text(attr(attrs, "alt").unwrap_or(""))),
                },
                // <span class="mw-align-left"><img></span> is how an aligned plain image renders
                "span" if align_of(attrs).is_some() && kids.len() == 1 && matches!(&kids[0], Node::El { name, .. } if name == "img") => {
                    if let Node::El { attrs: ia, .. } = &kids[0] {
                        match file_embed(ia, cx, &[align_of(attrs).unwrap_or("")], None) {
                            Some(e) => out.push_str(&e),
                            None => out.push_str(&inline(kids, cx)),
                        }
                    }
                }
                "span" | "abbr" | "q" | "mark" | "dfn" => {
                    let a = safe_attr_str(attrs);
                    if a.is_empty() && name == "span" {
                        out.push_str(&inline(kids, cx));
                    } else {
                        out.push_str(&format!("<{name}{a}>{}</{name}>", inline(kids, cx)));
                    }
                }
                _ => out.push_str(&inline(kids, cx)),
            },
        }
    }
    out
}

/// `class`, `style`, `title`, `lang` and `dir` as wikitext tag attributes (the parser
/// validates them again when rendering).
fn safe_attr_str(attrs: &[(String, String)]) -> String {
    let mut out = String::new();
    for k in ["class", "style", "title", "lang", "dir"] {
        if let Some(v) = attr(attrs, k).map(str::trim).filter(|v| !v.is_empty()) {
            out.push_str(&format!(" {k}=\"{}\"", decode_entities(v).replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;").replace('>', "&gt;")));
        }
    }
    out
}

fn align_of(attrs: &[(String, String)]) -> Option<&'static str> {
    let cls = attr(attrs, "class")?;
    ["left", "right", "center", "none"].into_iter().find(|a| cls.split_whitespace().any(|c| c.strip_prefix("mw-align-") == Some(a)))
}

/// `<img>` rendered by the engine back to `[[File:Name|opts|caption]]`.
fn file_embed(attrs: &[(String, String)], cx: &Ctx, extra: &[&str], caption: Option<&str>) -> Option<String> {
    let src = attr(attrs, "src").unwrap_or("");
    // the rendition parameters the engine appends are not part of the file name
    let cut = ["?width=", "&width=", "?height=", "&height="].iter().filter_map(|m| src.find(m)).min().unwrap_or(src.len());
    let name = attr(attrs, "data-file").map(str::to_string).filter(|n| !n.is_empty()).or_else(|| from_template(&src[..cut], cx.file_path))?;
    let q = &src[cut..];
    let num_q = |key: &str| q.split(['?', '&']).find_map(|kv| kv.strip_prefix(key)).and_then(|v| v.parse::<u32>().ok());
    // a <video> carries its size as an attribute (320 is the default the renderer uses)
    let video_w = attr(attrs, "poster").and(attr(attrs, "width")).or_else(|| attr(attrs, "width").filter(|_| src.contains("#t=") || attr(attrs, "controls").is_some()));
    let num = |key: &str| match (key, video_w) {
        ("width=", Some(w)) => w.parse::<u32>().ok().filter(|w| *w != 320),
        ("height=", Some(_)) => None,
        _ => num_q(key),
    };
    // <video src="...#t=65"> keeps its start time
    let start = src.split_once("#t=").map(|(_, t)| t.to_string());
    let mut parts: Vec<String> = vec![format!("File:{name}")];
    let thumb = extra.contains(&"thumb");
    if thumb {
        parts.push("thumb".into());
    }
    match (num("width="), num("height=")) {
        (Some(220), None) if thumb => {}
        (Some(w), Some(h)) => parts.push(format!("{w}x{h}px")),
        (Some(w), None) => parts.push(format!("{w}px")),
        (None, Some(h)) => parts.push(format!("x{h}px")),
        (None, None) => {}
    }
    if let Some(p) = num("page=").filter(|p| *p > 1) {
        parts.push(format!("page={p}"));
    }
    if let Some(s) = start {
        parts.push(format!("start={s}"));
    }
    if attr(attrs, "class").is_some_and(|c| c.split_whitespace().any(|x| x == "mw-border")) {
        parts.push("border".into());
    }
    for e in extra.iter().filter(|e| !e.is_empty() && **e != "thumb" && !(thumb && **e == "right")) {
        parts.push(e.to_string());
    }
    let alt = attr(attrs, "alt").map(decode_entities).unwrap_or_default();
    match caption {
        Some(c) => {
            if !alt.is_empty() && alt != name && alt != c.replace('\'', "") {
                parts.push(format!("alt={alt}"));
            }
            if !c.is_empty() {
                parts.push(c.to_string());
            }
        }
        None if !alt.is_empty() && alt != name => parts.push(alt),
        None => {}
    }
    Some(format!("[[{}]]", parts.join("|")))
}

fn wrap(mark: &str, inner: &str) -> String {
    let t = inner.trim();
    if t.is_empty() {
        return inner.to_string();
    }
    let lead = &inner[..inner.len() - inner.trim_start().len()];
    let trail = &inner[inner.trim_end().len()..];
    format!("{lead}{mark}{t}{mark}{trail}")
}

fn link(attrs: &[(String, String)], kids: &[Node], cx: &Ctx) -> String {
    let text = inline(kids, cx);
    let label_plain = plain(kids);
    let Some(href) = attr(attrs, "href") else { return text };
    if let Some(frag) = href.strip_prefix('#') {
        let f = frag.replace('_', " ");
        return if label_plain.trim() == f { format!("[[#{f}]]") } else { format!("[[#{f}|{}]]", text.trim()) };
    }
    let (path, frag) = href.split_once('#').map_or((href, None), |(p, f)| (p, Some(f)));
    if let Some(title) = from_template(path, cx.article_path) {
        let full = match frag {
            Some(f) if !f.is_empty() => format!("{title}#{}", f.replace('_', " ")),
            _ => title.clone(),
        };
        let lp = label_plain.trim();
        return if lp == full || lp == title { format!("[[{full}]]") } else { format!("[[{full}|{}]]", text.trim()) };
    }
    if href.starts_with("http://") || href.starts_with("https://") {
        let t = text.trim();
        if t == href && !href.contains(|c: char| c.is_whitespace() || matches!(c, '[' | ']' | '<' | '>' | '"')) {
            return href.to_string(); // bare URLs are autolinked by the engine
        }
        return if t.is_empty() { format!("[{href}]") } else { format!("[{href} {t}]") };
    }
    text
}

fn is_block(name: &str) -> bool {
    matches!(
        name,
        "p" | "div" | "ul" | "ol" | "dl" | "table" | "pre" | "hr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "blockquote" | "section" | "article" | "body" | "html" | "main" | "header" | "footer" | "figure"
    )
}

fn blocks(nodes: &[Node], cx: &Ctx, out: &mut Vec<String>) {
    let mut run: Vec<&Node> = Vec::new();
    let flush = |run: &mut Vec<&Node>, out: &mut Vec<String>| {
        if run.is_empty() {
            return;
        }
        let owned: Vec<Node> = Vec::new();
        let _ = owned;
        let s: String = run
            .iter()
            .map(|n| match n {
                Node::Text(t) => escape_text(&collapse(&decode_entities(t))),
                Node::El { .. } => inline(std::slice::from_ref(*n), cx),
            })
            .collect();
        let s = s.trim().to_string();
        if !s.is_empty() {
            out.push(line_safe(&s));
        }
        run.clear();
    };
    for n in nodes {
        match n {
            Node::El { name, attrs, kids } if is_block(name) || name == "head" || name == "title" => {
                flush(&mut run, out);
                match name.as_str() {
                    "head" | "title" => {}
                    "figure" => {
                        // <figure class="mw-thumb mw-align-right"><img><figcaption>..</figcaption></figure>
                        // the picture: an <img>, a <video>/<audio>, or a linked PDF preview
                        let img = kids.iter().find_map(|k| match k {
                            Node::El { name, attrs, .. } if matches!(name.as_str(), "img" | "video" | "audio") => Some(attrs),
                            Node::El { name, kids, .. } if name == "a" => kids.iter().find_map(|k| match k {
                                Node::El { name, attrs, .. } if name == "img" => Some(attrs),
                                _ => None,
                            }),
                            _ => None,
                        });
                        let cap = kids.iter().find_map(|k| match k {
                            Node::El { name, kids, .. } if name == "figcaption" => Some(inline(kids, cx).trim().to_string()),
                            _ => None,
                        });
                        match img.and_then(|a| file_embed(a, cx, &["thumb", align_of(attrs).unwrap_or("")], Some(cap.as_deref().unwrap_or("")))) {
                            Some(e) => out.push(e),
                            None => blocks(kids, cx, out),
                        }
                    }
                    "div" | "blockquote" if name == "blockquote" || !safe_attr_str(attrs).is_empty() => {
                        // kept as raw HTML, which the parser accepts with these attributes
                        let mut inner = Vec::new();
                        blocks(kids, cx, &mut inner);
                        out.push(format!("<{name}{}>", safe_attr_str(attrs)));
                        out.extend(inner);
                        out.push(format!("</{name}>"));
                    }
                    "p" => {
                        let s = inline(kids, cx);
                        let s = s.trim();
                        if !s.is_empty() {
                            out.push(line_safe(s));
                        }
                    }
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                        let lvl = name[1..].parse::<usize>().unwrap_or(2).max(1);
                        let eq = "=".repeat(lvl);
                        let t = inline(kids, cx);
                        if !t.trim().is_empty() {
                            out.push(format!("{eq} {} {eq}", t.trim()));
                        }
                    }
                    "hr" => out.push("----".into()),
                    "pre" => {
                        let t = plain(kids);
                        let t = t.trim_matches('\n');
                        if !t.is_empty() {
                            out.push(format!("<pre>{}</pre>", t.replace("</pre", "&lt;/pre")));
                        }
                    }
                    "ul" | "ol" | "dl" => {
                        let mut lines = Vec::new();
                        list(name, kids, "", cx, &mut lines);
                        if !lines.is_empty() {
                            out.push(lines.join("\n"));
                        }
                    }
                    "table" => {
                        let t = table(attrs, kids, cx);
                        if !t.is_empty() {
                            out.push(t);
                        }
                    }
                    _ => blocks(kids, cx, out),
                }
            }
            Node::El { name, kids, .. } if matches!(name.as_str(), "span" | "font" | "section") && kids.iter().any(|k| matches!(k, Node::El { name, .. } if is_block(name))) => {
                flush(&mut run, out);
                blocks(kids, cx, out);
            }
            other => run.push(other),
        }
    }
    flush(&mut run, out);
}

/// A paragraph line must not begin with characters that wikitext treats as list/heading/pre markup.
fn line_safe(s: &str) -> String {
    s.split('\n')
        .map(|l| match l.chars().next() {
            Some('*' | '#' | ':' | ';' | '=' | ' ' | '-') => format!("<nowiki/>{l}"),
            _ => l.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn list(kind: &str, items: &[Node], prefix: &str, cx: &Ctx, out: &mut Vec<String>) {
    let els: Vec<&Node> = items.iter().filter(|n| matches!(n, Node::El { .. })).collect();
    let mut skip_next = false;
    for (idx, n) in els.iter().enumerate() {
        if skip_next {
            skip_next = false;
            continue;
        }
        let Node::El { name, kids, .. } = n else { continue };
        let marker = match (kind, name.as_str()) {
            ("ul", "li") => "*",
            ("ol", "li") => "#",
            ("dl", "dt") => ";",
            ("dl", "dd") => ":",
            _ => continue,
        };
        let (mut inl, mut nested): (Vec<&Node>, Vec<&Node>) = (Vec::new(), Vec::new());
        for k in kids {
            match k {
                Node::El { name, .. } if matches!(name.as_str(), "ul" | "ol" | "dl") => nested.push(k),
                _ => inl.push(k),
            }
        }
        let text: String = inl
            .iter()
            .map(|k| match k {
                Node::Text(t) => escape_text(&collapse(&decode_entities(t))),
                Node::El { .. } => inline(std::slice::from_ref(*k), cx),
            })
            .collect();
        let p = format!("{prefix}{marker}");
        // `; term : definition` on one line, the way it is normally written
        if marker == ";" {
            if let Some(Node::El { name: n2, kids: k2, .. }) = els.get(idx + 1) {
                if n2 == "dd" && !k2.iter().any(|k| matches!(k, Node::El { name, .. } if matches!(name.as_str(), "ul" | "ol" | "dl"))) {
                    let d: String = k2
                        .iter()
                        .map(|k| match k {
                            Node::Text(t) => escape_text(&collapse(&decode_entities(t))),
                            Node::El { .. } => inline(std::slice::from_ref(k), cx),
                        })
                        .collect();
                    out.push(format!("{p} {} : {}", text.trim(), d.trim()));
                    skip_next = true;
                    continue;
                }
            }
        }
        out.push(format!("{p} {}", text.trim()).trim_end().to_string());
        for nl in nested {
            if let Node::El { name, kids, .. } = nl {
                list(name, kids, &p, cx, out);
            }
        }
    }
}

fn table(attrs: &[(String, String)], kids: &[Node], cx: &Ctx) -> String {
    let mut out = String::from("{|");
    if let Some(c) = attr(attrs, "class").filter(|c| c.chars().all(|ch| ch.is_ascii_alphanumeric() || " -_".contains(ch))) {
        out.push_str(&format!(" class=\"{c}\""));
    }
    out.push('\n');
    fn rows<'a>(nodes: &'a [Node], acc: &mut Vec<&'a Node>, caption: &mut Option<&'a [Node]>) {
        for n in nodes {
            if let Node::El { name, kids, .. } = n {
                match name.as_str() {
                    "tr" => acc.push(n),
                    "caption" => *caption = Some(kids),
                    "thead" | "tbody" | "tfoot" => rows(kids, acc, caption),
                    _ => {}
                }
            }
        }
    }
    let (mut trs, mut caption) = (Vec::new(), None);
    rows(kids, &mut trs, &mut caption);
    if let Some(c) = caption {
        out.push_str(&format!("|+ {}\n", inline(c, cx).trim()));
    }
    for (i, tr) in trs.iter().enumerate() {
        let Node::El { kids, .. } = tr else { continue };
        if i > 0 {
            out.push_str("|-\n");
        }
        for cell in kids {
            let Node::El { name, attrs, kids } = cell else { continue };
            if name != "td" && name != "th" {
                continue;
            }
            let mut a = String::new();
            for k in ["colspan", "rowspan"] {
                if let Some(v) = attr(attrs, k).filter(|v| !v.is_empty() && v.len() <= 3 && v.bytes().all(|b| b.is_ascii_digit())) {
                    a.push_str(&format!(" {k}=\"{v}\""));
                }
            }
            let mut b = Vec::new();
            blocks(kids, cx, &mut b);
            let content = if b.len() <= 1 && !b.iter().any(|x| x.contains('\n')) { b.join("") } else { b.join("\n\n") };
            let lead = if name == "th" { "!" } else { "|" };
            if a.is_empty() {
                out.push_str(&format!("{lead} {content}\n"));
            } else {
                out.push_str(&format!("{lead}{a} | {content}\n"));
            }
        }
    }
    out.push_str("|}");
    out
}

pub fn html_to_wikitext(html: &str, cx: &Ctx) -> String {
    let tree = parse(html);
    let mut out = Vec::new();
    blocks(&tree, cx, &mut out);
    out.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const CX: Ctx = Ctx { article_path: "/wiki/$1", file_path: "/files/$1" };

    fn w(h: &str) -> String {
        html_to_wikitext(h, &CX)
    }

    #[test]
    fn figures_blocks_and_spans() {
        assert_eq!(
            w("<figure class=\"mw-thumb mw-align-right\"><img class=\"mw-file\" src=\"/files/Cat.png?width=220\" alt=\"A cat\" width=\"220\" data-file=\"Cat.png\"><figcaption>A <i>cat</i></figcaption></figure>"),
            "[[File:Cat.png|thumb|A ''cat'']]"
        );
        assert_eq!(
            w("<figure class=\"mw-thumb mw-align-left\"><img src=\"/files/Cat.png?width=300\" alt=\"Other alt\" data-file=\"Cat.png\"><figcaption>Cap</figcaption></figure>"),
            "[[File:Cat.png|thumb|300px|left|alt=Other alt|Cap]]"
        );
        assert_eq!(w("<p><img src=\"/files/My_cat.png?width=120&amp;height=80\" alt=\"My cat.png\"> <span class=\"mw-align-left\"><img src=\"/files/A.png?height=50\" alt=\"A.png\" data-file=\"A.png\"></span></p>"), "[[File:My cat.png|120x80px]] [[File:A.png|x50px|left]]");
        assert_eq!(w("<div class=\"box\" style=\"color:red\" onclick=\"x()\"><p>Inside</p></div><p>After</p>"), "<div class=\"box\" style=\"color:red\">\n\nInside\n\n</div>\n\nAfter");
        assert_eq!(w("<div><p>plain wrapper</p></div>"), "plain wrapper");
        assert_eq!(w("<blockquote><p>Quoted</p></blockquote>"), "<blockquote>\n\nQuoted\n\n</blockquote>");
        assert_eq!(w("<p>a <span class=\"hl\" title=\"say &quot;hi&quot;\">b</span> <span>c</span> <abbr title=\"x\">y</abbr></p>"), "a <span class=\"hl\" title=\"say &quot;hi&quot;\">b</span> c <abbr title=\"x\">y</abbr>");
    }

    #[test]
    fn basics() {
        assert_eq!(w("<p>Hello <b>bold</b> and <i>it</i>.</p><p>Two</p>"), "Hello '''bold''' and ''it''.\n\nTwo");
        assert_eq!(w("<h2>Title</h2><hr><h3>Sub</h3>"), "== Title ==\n\n----\n\n=== Sub ===");
        assert_eq!(w("<ul><li>a<ul><li>b</li></ul></li><li>c</li></ul><ol><li>x<li>y</ol>"), "* a\n** b\n* c\n\n# x\n# y");
        assert_eq!(w("<dl><dt>t</dt><dd>d</dd></dl>"), "; t : d");
    }

    #[test]
    fn links_images_tables() {
        assert_eq!(w(r##"<p><a href="/wiki/Rust_(language)">Rust (language)</a> <a href="/wiki/Foo#Bar" title="x">label</a> <a href="https://e.com/x?a=1&amp;b=2">site</a> <a href="https://e.com">https://e.com</a> <a href="#Sec">Sec</a></p>"##),
            "[[Rust (language)]] [[Foo#Bar|label]] [https://e.com/x?a=1&b=2 site] https://e.com [[#Sec]]");
        assert_eq!(w(r#"<img src="/files/Cat.png" alt="A cat">"#), "[[File:Cat.png|A cat]]");
        let t = w("<table class=\"wikitable\"><caption>C</caption><tr><th>A</th><th>B</th></tr><tr><td>1</td><td colspan=\"2\">2</td></tr></table>");
        assert_eq!(t, "{| class=\"wikitable\"\n|+ C\n! A\n! B\n|-\n| 1\n| colspan=\"2\" | 2\n|}");
    }

    #[test]
    fn dangerous_and_odd_input() {
        assert_eq!(w("<script>alert(1)</script><style>p{}</style><p>ok</p>"), "ok");
        assert_eq!(w("<p>[[not a link]] and {{tpl}} and '' x</p>"), "<nowiki>[[not a link]] and {{tpl}} and '' x</nowiki>");
        assert_eq!(w("<p>* not a list</p>"), "<nowiki/>* not a list");
        assert_eq!(w("<p onclick=\"x\">unclosed <b>bold"), "unclosed '''bold'''");
        assert_eq!(w(&"<div>".repeat(5000)), "");
        assert_eq!(w("<pre>a\n  b</pre>"), "<pre>a\n  b</pre>");
        assert_eq!(w("just text &amp; more"), "just text & more");
    }
}

