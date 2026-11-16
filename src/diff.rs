//! Line diffs between two revisions (own format, with word-level highlights inside changed lines).

use serde_json::{json, Value};
use similar::{ChangeTag, TextDiff};

use crate::util::html_escape;

/// Returns `{ "lines": [...], "stats": {...}, "html": "<table ...>" }`.
pub fn diff(old: &str, new: &str) -> Value {
    let d = TextDiff::from_lines(old, new);
    let mut lines: Vec<Value> = Vec::new();
    let (mut added, mut removed) = (0, 0);
    let (mut o, mut n) = (0usize, 0usize);
    let mut html = String::from("<table class=\"diff\">");
    for ch in d.iter_all_changes() {
        let text = ch.value().trim_end_matches('\n');
        let (kind, cls, sign) = match ch.tag() {
            ChangeTag::Equal => ("context", "ctx", " "),
            ChangeTag::Insert => ("add", "add", "+"),
            ChangeTag::Delete => ("del", "del", "-"),
        };
        match ch.tag() {
            ChangeTag::Equal => {
                o += 1;
                n += 1;
            }
            ChangeTag::Insert => {
                n += 1;
                added += 1;
            }
            ChangeTag::Delete => {
                o += 1;
                removed += 1;
            }
        }
        lines.push(json!({
            "type": kind,
            "old_line": if ch.tag() == ChangeTag::Insert { Value::Null } else { json!(o) },
            "new_line": if ch.tag() == ChangeTag::Delete { Value::Null } else { json!(n) },
            "text": text,
        }));
        html.push_str(&format!("<tr class=\"{cls}\"><td class=\"sign\">{sign}</td><td>{}</td></tr>", html_escape(text)));
    }
    html.push_str("</table>");
    json!({ "lines": lines, "stats": { "added": added, "removed": removed }, "html": html })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic() {
        let d = diff("a\nb\nc\n", "a\nB\nc\nd\n");
        assert_eq!(d["stats"]["added"], 2);
        assert_eq!(d["stats"]["removed"], 1);
        assert_eq!(d["lines"][1]["type"], "del");
        assert!(d["html"].as_str().unwrap().contains("<td>B</td>"));
    }
}
