//! SVG sanitiser. An uploaded SVG is parsed and re-serialised from scratch; only drawing
//! elements and harmless attributes survive. Scripts, event handlers, `foreignObject`,
//! animation, external references, entity declarations and editor metadata are removed, so
//! the stored file cannot execute code or fetch anything, however it is later displayed.
//! (Files are additionally served with a sandboxing Content-Security-Policy.)

const ELEMENTS: &[&str] = &[
    "svg", "g", "path", "rect", "circle", "ellipse", "line", "polyline", "polygon", "text", "tspan", "textPath", "defs", "use", "symbol",
    "linearGradient", "radialGradient", "stop", "clipPath", "mask", "pattern", "marker", "title", "desc", "image", "style", "filter",
    "feBlend", "feColorMatrix", "feComponentTransfer", "feComposite", "feConvolveMatrix", "feDiffuseLighting", "feDisplacementMap",
    "feDistantLight", "feDropShadow", "feFlood", "feFuncA", "feFuncB", "feFuncG", "feFuncR", "feGaussianBlur", "feMerge", "feMergeNode",
    "feMorphology", "feOffset", "fePointLight", "feSpecularLighting", "feSpotLight", "feTile", "feTurbulence", "switch", "a", "view",
];

const MAX_DEPTH: usize = 256;
const MAX_NODES: usize = 300_000;

fn esc(s: &str, attr: bool) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' if attr => o.push_str("&quot;"),
            c if (c as u32) < 0x20 && !matches!(c, '\n' | '\t' | '\r') => {}
            _ => o.push(c),
        }
    }
    o
}

/// Decode the predefined and numeric entities. Any other entity means the file relies on a
/// DTD, which is refused.
fn unescape(s: &str) -> Result<String, String> {
    if !s.contains('&') {
        return Ok(s.to_string());
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        rest = &rest[p + 1..];
        let semi = rest.find(';').filter(|i| *i <= 10).ok_or("malformed entity reference")?;
        let ent = &rest[..semi];
        let c = match ent {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            _ => {
                let n = if let Some(h) = ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")) {
                    u32::from_str_radix(h, 16).ok()
                } else if let Some(d) = ent.strip_prefix('#') {
                    d.parse().ok()
                } else {
                    return Err(format!("entity &{ent}; is not allowed"));
                };
                n.and_then(char::from_u32).ok_or("invalid character reference")?
            }
        };
        out.push(c);
        rest = &rest[semi + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Lower-cased with whitespace, control characters and CSS escapes/comments made visible,
/// so obfuscated `u\72l(` or `java\nscript:` cannot slip past the checks below.
fn squash(v: &str) -> Option<String> {
    if v.contains('\\') || v.contains("/*") {
        return None;
    }
    Some(v.chars().filter(|c| !c.is_whitespace() && !c.is_control()).collect::<String>().to_ascii_lowercase())
}

/// Every `url(...)` must point inside the document (`url(#id)`).
fn value_is_local(v: &str) -> bool {
    let Some(s) = squash(v) else { return false };
    if s.contains("javascript:") || s.contains("vbscript:") || s.contains("data:") || s.contains("@import") || s.contains("expression(") || s.contains("image-set(") || s.contains("-moz-binding") || s.contains("behavior:") {
        return false;
    }
    let mut rest = s.as_str();
    while let Some(p) = rest.find("url(") {
        let after = rest[p + 4..].trim_start_matches(['"', '\'']);
        if !after.starts_with('#') {
            return false;
        }
        rest = &rest[p + 4..];
    }
    true
}

fn href_ok(elem: &str, v: &str) -> bool {
    let t = v.trim();
    if t.starts_with('#') {
        return true;
    }
    // Embedded raster images are fine; nothing else may be referenced.
    elem == "image" && ["data:image/png;base64,", "data:image/jpeg;base64,", "data:image/gif;base64,", "data:image/webp;base64,"].iter().any(|p| t.starts_with(p)) && t.bytes().all(|b| b.is_ascii_graphic())
}

fn name_ok(n: &str) -> bool {
    let mut cs = n.chars();
    cs.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && cs.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
}

/// Parse `name="value"` pairs of a start tag.
fn attributes(src: &str) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    let mut rest = src.trim_start();
    while !rest.is_empty() {
        let eq = rest.find('=').ok_or("attribute without a value")?;
        let name = rest[..eq].trim();
        if !name_ok(name) {
            return Err(format!("bad attribute name '{}'", name.chars().take(30).collect::<String>()));
        }
        let after = rest[eq + 1..].trim_start();
        let q = after.chars().next().filter(|c| *c == '"' || *c == '\'').ok_or("attribute values must be quoted")?;
        let end = after[1..].find(q).ok_or("unterminated attribute value")?;
        let raw = &after[1..1 + end];
        if raw.contains('<') {
            return Err("'<' in attribute value".into());
        }
        out.push((name.to_string(), unescape(raw)?));
        rest = after[end + 2..].trim_start();
    }
    Ok(out)
}

fn clean_attrs(elem: &str, attrs: Vec<(String, String)>, root: bool) -> String {
    let mut out = String::new();
    let mut seen: Vec<String> = Vec::new();
    if root {
        out.push_str(" xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\"");
    }
    for (name, value) in attrs {
        let lower = name.to_ascii_lowercase();
        if lower.starts_with("on") || lower.starts_with("xmlns") || seen.contains(&lower) {
            continue;
        }
        let keep = match lower.as_str() {
            "href" | "xlink:href" => href_ok(elem, &value),
            "xml:space" | "xml:lang" => true,
            _ if lower.contains(':') => false,
            // attributes that only exist to point elsewhere or to script the document
            "src" | "action" | "formaction" | "srcdoc" | "ping" | "background" | "poster" | "data" | "requiredextensions" | "systemlanguage" => false,
            _ => value_is_local(&value) || (matches!(lower.as_str(), "id" | "class" | "d" | "points" | "transform" | "viewbox") && !value.contains("url(")),
        };
        if keep {
            out.push_str(&format!(" {}=\"{}\"", name, esc(&value, true)));
            seen.push(lower);
        }
    }
    out
}

fn length(v: &str) -> Option<f64> {
    let t = v.trim().trim_end_matches("px").trim();
    t.parse::<f64>().ok().filter(|n| n.is_finite() && *n > 0.0)
}

pub struct Clean {
    pub svg: String,
    pub width: i64,
    pub height: i64,
}

pub fn looks_like_svg(d: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&d[..d.len().min(2048)]).to_ascii_lowercase();
    let t = head.trim_start_matches('\u{feff}').trim_start();
    (t.starts_with("<?xml") || t.starts_with("<svg") || t.starts_with("<!--") || t.starts_with("<!doctype svg")) && head.contains("<svg")
}

pub fn sanitize(input: &[u8]) -> Result<Clean, String> {
    let text = std::str::from_utf8(input).map_err(|_| "SVG files must be UTF-8")?;
    let s = text.trim_start_matches('\u{feff}');
    let lower_head = s[..s.char_indices().nth(400).map_or(s.len(), |(i, _)| i)].to_ascii_lowercase();
    if lower_head.contains("encoding=") && !(lower_head.contains("encoding=\"utf-8\"") || lower_head.contains("encoding='utf-8'")) {
        return Err("SVG files must be UTF-8".into());
    }
    let mut out = String::with_capacity(s.len());
    // Names of the open elements; `skip` > 0 while inside a removed subtree.
    let mut stack: Vec<String> = Vec::new();
    let (mut skip, mut nodes, mut i) = (0usize, 0usize, 0usize);
    let (mut width, mut height) = (0i64, 0i64);
    let mut root_seen = false;
    let mut root_closed = false;
    let mut style_buf: Option<String> = None;

    while i < s.len() {
        let rest = &s[i..];
        if !rest.starts_with('<') {
            let end = rest.find('<').unwrap_or(rest.len());
            let raw = &rest[..end];
            i += end;
            if skip == 0 && !stack.is_empty() {
                let t = unescape(raw)?;
                match style_buf.as_mut() {
                    Some(b) => b.push_str(&t),
                    None => out.push_str(&esc(&t, false)),
                }
            } else if stack.is_empty() && !raw.trim().is_empty() {
                return Err("text outside the root element".into());
            }
            continue;
        }
        nodes += 1;
        if nodes > MAX_NODES {
            return Err("the SVG is too complex".into());
        }
        if rest.starts_with("<!--") {
            let end = rest.find("-->").ok_or("unterminated comment")?;
            i += end + 3;
        } else if rest.starts_with("<![CDATA[") {
            let end = rest.find("]]>").ok_or("unterminated CDATA section")?;
            let t = &rest[9..end];
            i += end + 3;
            if skip == 0 && !stack.is_empty() {
                match style_buf.as_mut() {
                    Some(b) => b.push_str(t),
                    None => out.push_str(&esc(t, false)),
                }
            }
        } else if rest.starts_with("<?") {
            let end = rest.find("?>").ok_or("unterminated processing instruction")?;
            i += end + 2;
        } else if rest.starts_with("<!") {
            // DOCTYPE: allowed only without an internal subset (which could declare entities).
            let end = rest.find('>').ok_or("unterminated declaration")?;
            let decl = &rest[..end];
            if decl.contains('[') || decl.to_ascii_uppercase().contains("<!ENTITY") || root_seen {
                return Err("DTD subsets and entity declarations are not allowed".into());
            }
            i += end + 1;
        } else if let Some(body) = rest.strip_prefix("</") {
            let end = body.find('>').ok_or("unterminated end tag")?;
            let name = body[..end].trim();
            i += end + 3;
            let top = stack.pop().ok_or("unexpected end tag")?;
            if top != name {
                return Err(format!("mismatched end tag </{}>", name.chars().take(30).collect::<String>()));
            }
            if skip > 0 {
                skip -= 1;
                continue;
            }
            if let Some(css) = style_buf.take() {
                if value_is_local(&css) && !css.to_ascii_lowercase().contains("</") {
                    out.push_str(&esc(&css, false));
                }
            }
            out.push_str(&format!("</{name}>"));
            if stack.is_empty() {
                root_closed = true;
            }
        } else {
            // start tag: find the closing '>' outside quotes
            let bytes = rest.as_bytes();
            let (mut j, mut quote) = (1usize, 0u8);
            while j < bytes.len() {
                let b = bytes[j];
                if quote != 0 {
                    if b == quote {
                        quote = 0;
                    }
                } else if b == b'"' || b == b'\'' {
                    quote = b;
                } else if b == b'>' {
                    break;
                }
                j += 1;
            }
            if j >= bytes.len() {
                return Err("unterminated start tag".into());
            }
            let inner = &rest[1..j];
            i += j + 1;
            let (inner, empty) = match inner.strip_suffix('/') {
                Some(x) => (x, true),
                None => (inner, false),
            };
            let name_end = inner.find(|c: char| c.is_whitespace()).unwrap_or(inner.len());
            let name = &inner[..name_end];
            if !name_ok(name) {
                return Err("malformed tag".into());
            }
            if root_closed {
                return Err("more than one root element".into());
            }
            let is_root = stack.is_empty();
            if is_root && name != "svg" {
                return Err("the root element must be <svg>".into());
            }
            if stack.len() >= MAX_DEPTH {
                return Err("the SVG is nested too deeply".into());
            }
            let allowed = ELEMENTS.contains(&name) && style_buf.is_none();
            if skip > 0 || !allowed {
                // Removed element: its whole subtree goes with it.
                if !empty {
                    stack.push(name.to_string());
                    skip += 1;
                }
                continue;
            }
            let attrs = attributes(&inner[name_end..])?;
            if is_root {
                root_seen = true;
                let get = |k: &str| attrs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
                let vb: Vec<f64> = get("viewBox").map(|v| v.split([' ', ',']).filter(|p| !p.is_empty()).filter_map(|p| p.parse().ok()).collect()).unwrap_or_default();
                let w = get("width").and_then(length).or_else(|| vb.get(2).copied());
                let h = get("height").and_then(length).or_else(|| vb.get(3).copied());
                width = w.map_or(0, |v| v.round().clamp(0.0, 1e7) as i64);
                height = h.map_or(0, |v| v.round().clamp(0.0, 1e7) as i64);
            }
            out.push_str(&format!("<{name}{}", clean_attrs(name, attrs, is_root)));
            if empty {
                out.push_str("/>");
                if is_root {
                    root_closed = true;
                }
            } else {
                out.push('>');
                stack.push(name.to_string());
                if name == "style" {
                    style_buf = Some(String::new());
                }
            }
        }
    }
    if !stack.is_empty() || !root_closed {
        return Err("the SVG is not well-formed (unclosed elements)".into());
    }
    Ok(Clean { svg: out, width, height })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(s: &str) -> String {
        sanitize(s.as_bytes()).unwrap_or_else(|e| panic!("rejected: {e}")).svg
    }

    #[test]
    fn keeps_drawings() {
        let c = sanitize(br##"<?xml version="1.0" encoding="UTF-8"?>
<!-- made by hand -->
<svg xmlns="http://www.w3.org/2000/svg" width="120px" height="80" viewBox="0 0 120 80">
  <defs><linearGradient id="g"><stop offset="0" stop-color="#f00"/><stop offset="1" stop-color="blue"/></linearGradient></defs>
  <rect x="1" y="1" width="100" height="50" fill="url(#g)" style="stroke: #000; stroke-width: 2"/>
  <text x="5" y="20">A &amp; B &lt;3 &#233;</text>
  <use href="#g"/>
</svg>"##)
        .unwrap();
        assert_eq!((c.width, c.height), (120, 80));
        assert!(c.svg.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" width=\"120px\""), "{}", c.svg);
        assert!(c.svg.contains("fill=\"url(#g)\" style=\"stroke: #000; stroke-width: 2\"/>"), "{}", c.svg);
        assert!(c.svg.contains("<text x=\"5\" y=\"20\">A &amp; B &lt;3 é</text>") && c.svg.contains("<use href=\"#g\"/>"), "{}", c.svg);
        assert!(!c.svg.contains("made by hand") && !c.svg.contains("<?xml"));
        // sanitising is idempotent
        assert_eq!(clean(&c.svg), c.svg);
        // dimensions fall back to the viewBox
        assert_eq!(sanitize(b"<svg viewBox='0 0 300 150'/>").map(|c| (c.width, c.height)).unwrap(), (300, 150));
    }

    #[test]
    fn strips_active_content() {
        let c = clean(
            r##"<svg xmlns="http://www.w3.org/2000/svg" onload="alert(1)" xmlns:ev="http://www.w3.org/2001/xml-events">
<script>alert(1)</script>
<script xlink:href="https://evil.example/x.js"/>
<foreignObject><body xmlns="http://www.w3.org/1999/xhtml"><iframe src="javascript:alert(1)"></iframe></body></foreignObject>
<a href="javascript:alert(1)"><circle r="5" onclick="alert(2)" ONMOUSEOVER="x"/></a>
<a xlink:href="https://evil.example/"><rect width="1" height="1"/></a>
<image href="https://evil.example/track.png"/><image href="data:image/svg+xml;base64,PHN2Zz4="/>
<image href="data:image/png;base64,iVBORw0KGgo="/>
<use href="https://evil.example/x.svg#a"/>
<animate attributeName="href" values="javascript:alert(1)"/><set attributeName="onclick" to="alert(1)"/>
<rect fill="url(https://evil.example/f)" filter="url( 'http://x/y#z' )" style="background: u\72l(https://evil.example)"/>
<rect style="fill: url(#ok)" stroke="url(#ok2)"/>
<style>@import url(https://evil.example/x.css); rect { fill: red }</style>
<style>rect { fill: green } /* c */</style>
<style><![CDATA[ circle { stroke: blue; fill: url(#g) } ]]></style>
<metadata><rdf:RDF xmlns:rdf="x"><cc:Work/></rdf:RDF></metadata>
<sodipodi:namedview inkscape:zoom="1"/>
<g inkscape:label="Layer" id="layer1"><path d="M0 0L1 1" inkscape:connector-curvature="0"/></g>
</svg>"##,
        );
        for bad in ["script", "alert", "onload", "onclick", "ONMOUSEOVER", "foreignObject", "iframe", "javascript", "evil.example", "animate", "<set", "@import", "metadata", "rdf", "sodipodi", "inkscape", "svg+xml", "u\\72l", "/* c */"] {
            assert!(!c.contains(bad), "{bad} survived:\n{c}");
        }
        assert!(c.contains("<circle r=\"5\"/>") && c.contains("<image href=\"data:image/png;base64,iVBORw0KGgo=\"/>"), "{c}");
        assert!(c.contains("<rect style=\"fill: url(#ok)\" stroke=\"url(#ok2)\"/>"), "{c}");
        assert!(c.contains("<style> circle { stroke: blue; fill: url(#g) } </style>"), "{c}");
        assert!(c.contains("<g id=\"layer1\"><path d=\"M0 0L1 1\"/></g>"), "{c}");
    }

    #[test]
    fn rejects_malformed_and_entity_tricks() {
        for bad in [
            &b"<!DOCTYPE svg [<!ENTITY xxe SYSTEM \"file:///etc/passwd\">]><svg>&xxe;</svg>"[..],
            b"<!DOCTYPE svg [<!ENTITY a \"aaaaaaaaaa\"><!ENTITY b \"&a;&a;&a;&a;\">]><svg>&b;</svg>",
            b"<svg>&custom;</svg>",
            b"<html><body><script>alert(1)</script></body></html>",
            b"<svg><g></svg>",
            b"<svg></svg><svg></svg>",
            b"<svg><rect width=5></svg>",
            b"<svg",
            b"\xff\xfe<\x00s\x00v\x00g\x00>\x00",
            b"<?xml version=\"1.0\" encoding=\"UTF-16\"?><svg/>",
            b"plain text",
            b"",
        ] {
            assert!(sanitize(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
        // a DOCTYPE without an internal subset is simply dropped
        assert_eq!(clean("<!DOCTYPE svg PUBLIC \"-//W3C//DTD SVG 1.1//EN\" \"http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd\"><svg/>"), "<svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\"/>");
        // markup hidden in CDATA or entities comes out as inert text
        let c = clean("<svg><text><![CDATA[<script>alert(1)</script>]]>&#60;script&#62;</text></svg>");
        assert!(c.contains("&lt;script&gt;alert(1)&lt;/script&gt;&lt;script&gt;") && !c.contains("<script"), "{c}");
        let c = clean("<svg><style><![CDATA[</style><script>alert(1)</script>]]></style></svg>");
        assert!(!c.contains("<script"), "{c}");
        assert!(sanitize(&[b'<'; 10]).is_err());
        let deep = format!("<svg>{}{}</svg>", "<g>".repeat(400), "</g>".repeat(400));
        assert!(sanitize(deep.as_bytes()).is_err());
    }

    #[test]
    fn detection() {
        assert!(looks_like_svg(b"<?xml version='1.0'?>\n<svg xmlns='http://www.w3.org/2000/svg'/>"));
        assert!(looks_like_svg(b"  <svg/>"));
        assert!(!looks_like_svg(b"<html><svg/></html>") && !looks_like_svg(b"\x89PNG"));
    }
}
