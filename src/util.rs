use std::time::{SystemTime, UNIX_EPOCH};

use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use rand::RngCore;
use sha2::{Digest, Sha256};

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// (year, month, day, hour, minute, second) in UTC.
pub fn civil(ts: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400) as u32;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, secs / 3600, (secs % 3600) / 60, secs % 60)
}

/// ISO 8601, the format MediaWiki uses: 2026-11-13T07:00:00Z
pub fn iso(ts: i64) -> String {
    let (y, m, d, h, mi, s) = civil(ts);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

pub fn hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(H[(b >> 4) as usize] as char);
        s.push(H[(b & 15) as usize] as char);
    }
    s
}

pub fn new_token() -> String {
    let mut b = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut b);
    format!("hp_{}", hex(&b))
}

pub fn html_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            _ => o.push(c),
        }
    }
    o
}

const PATH_SET: &AsciiSet = &CONTROLS
    .add(b' ').add(b'"').add(b'#').add(b'%').add(b'<').add(b'>').add(b'?')
    .add(b'`').add(b'{').add(b'}').add(b'/').add(b'\\').add(b'^').add(b'[').add(b']').add(b'|');

/// Encode a title for use as a URL path segment (spaces become underscores).
pub fn path_encode(title: &str) -> String {
    utf8_percent_encode(&title.replace(' ', "_"), PATH_SET).to_string()
}

/// Flatten rendered HTML to plain text. Only block-level tags introduce whitespace, so
/// `<a>Foo</a>.` stays `Foo.`
pub fn strip_html(s: &str) -> String {
    const BLOCK: &[&str] = &[
        "p", "div", "br", "li", "ul", "ol", "dl", "dt", "dd", "tr", "td", "th", "table", "caption", "pre", "blockquote", "hr", "h1",
        "h2", "h3", "h4", "h5", "h6",
    ];
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('<') {
        out.push_str(&rest[..p]);
        match rest[p..].find('>') {
            Some(e) => {
                let name: String = rest[p + 1..p + e]
                    .trim_start_matches('/')
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .collect::<String>()
                    .to_ascii_lowercase();
                if BLOCK.contains(&name.as_str()) {
                    out.push(' ');
                }
                rest = &rest[p + e + 1..];
            }
            None => {
                out.push_str(&rest[p..]);
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    decode_entities(&out)
}

pub fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        rest = &rest[p..];
        if let Some(semi) = rest[..rest.len().min(12)].find(';') {
            let ent = &rest[1..semi];
            let rep = match ent {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" | "#39" => Some('\''),
                "nbsp" => Some(' '),
                _ => ent
                    .strip_prefix("#x")
                    .and_then(|h| u32::from_str_radix(h, 16).ok())
                    .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                    .and_then(char::from_u32),
            };
            if let Some(c) = rep {
                out.push(c);
                rest = &rest[semi + 1..];
                continue;
            }
        }
        out.push('&');
        rest = &rest[1..];
    }
    out.push_str(rest);
    out
}

pub fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_epoch_and_leap() {
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(iso(1_794_553_200), "2026-11-13T07:00:00Z");
    }

    #[test]
    fn strips_tags_and_entities() {
        let s = r#"The <span class="x">Rust</span> language &amp; its &quot;borrow checker&quot;"#;
        assert_eq!(collapse_ws(&strip_html(s)), "The Rust language & its \"borrow checker\"");
    }

    #[test]
    fn inline_tags_do_not_add_spaces() {
        assert_eq!(collapse_ws(&strip_html("<p>See <a href=\"x\">Policy</a>.</p><p>Next</p>")), "See Policy. Next");
    }

    #[test]
    fn path_encoding() {
        assert_eq!(path_encode("AC/DC rocks"), "AC%2FDC_rocks");
    }
}
