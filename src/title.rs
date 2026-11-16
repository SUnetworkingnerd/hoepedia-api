//! Namespaces and title normalisation (MediaWiki-style rules).

use crate::util::collapse_ws;

pub const NAMESPACES: &[(i64, &str)] = &[
    (0, ""),
    (1, "Talk"),
    (2, "User"),
    (3, "User talk"),
    (4, "Project"),
    (5, "Project talk"),
    (6, "File"),
    (7, "File talk"),
    (10, "Template"),
    (11, "Template talk"),
    (14, "Category"),
    (15, "Category talk"),
    (828, "Module"),
    (829, "Module talk"),
];

pub const NS_MAIN: i64 = 0;
pub const NS_FILE: i64 = 6;
pub const NS_TEMPLATE: i64 = 10;
pub const NS_MODULE: i64 = 828;
pub const NS_CATEGORY: i64 = 14;

pub fn ns_name(ns: i64) -> &'static str {
    NAMESPACES.iter().find(|(i, _)| *i == ns).map(|(_, n)| *n).unwrap_or("")
}

pub fn ns_by_name(name: &str) -> Option<i64> {
    let n = collapse_ws(&name.replace('_', " ")).to_lowercase();
    if n.is_empty() {
        return None;
    }
    match n.as_str() {
        "image" => return Some(6),
        "image talk" => return Some(7),
        _ => {}
    }
    NAMESPACES.iter().find(|(i, nm)| *i != 0 && nm.to_lowercase() == n).map(|(i, _)| *i)
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Title {
    pub ns: i64,
    /// Normalised name without the namespace prefix (spaces, first letter upper-case).
    pub name: String,
}

#[derive(Debug)]
pub struct Parsed {
    pub title: Title,
    pub fragment: Option<String>,
}

impl Title {
    pub fn new(ns: i64, name: &str) -> Title {
        Title { ns, name: name.to_string() }
    }

    /// "Template:Foo bar"
    pub fn full(&self) -> String {
        if self.ns == 0 {
            self.name.clone()
        } else {
            format!("{}:{}", ns_name(self.ns), self.name)
        }
    }

    /// "Template:Foo_bar" — the form used in URLs and `key` fields.
    pub fn key(&self) -> String {
        self.full().replace(' ', "_")
    }

    pub fn parse(input: &str) -> Result<Parsed, String> {
        Self::parse_in(input, NS_MAIN)
    }

    pub fn parse_in(input: &str, default_ns: i64) -> Result<Parsed, String> {
        let (base, frag) = match input.split_once('#') {
            Some((b, f)) => (b, Some(f.trim().to_string()).filter(|f| !f.is_empty())),
            None => (input, None),
        };
        let mut s = collapse_ws(&base.replace('_', " "));
        let mut ns = default_ns;
        if let Some(rest) = s.strip_prefix(':') {
            s = rest.trim().to_string();
            ns = NS_MAIN;
        }
        if let Some((p, rest)) = s.split_once(':') {
            if let Some(found) = ns_by_name(p) {
                ns = found;
                s = rest.trim().to_string();
            }
        }
        let name = capitalize(&s);
        validate(&name)?;
        Ok(Parsed { title: Title { ns, name }, fragment: frag })
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

fn validate(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("title is empty".into());
    }
    if name.len() > 255 {
        return Err("title is too long (255 bytes max)".into());
    }
    if let Some(c) = name
        .chars()
        .find(|c| c.is_control() || matches!(c, '<' | '>' | '[' | ']' | '{' | '}' | '|' | '\u{E000}' | '\u{E001}'))
    {
        return Err(format!("title contains illegal character {c:?}"));
    }
    if name == "." || name == ".." || name.starts_with("../") || name.starts_with("./") {
        return Err("title looks like a relative path".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises() {
        let p = Title::parse("  template:foo_bar  baz#Sec ").unwrap();
        assert_eq!(p.title, Title::new(10, "Foo bar baz"));
        assert_eq!(p.fragment.as_deref(), Some("Sec"));
        assert_eq!(p.title.key(), "Template:Foo_bar_baz");
    }

    #[test]
    fn colon_forces_main_and_unknown_prefix_stays() {
        assert_eq!(Title::parse_in(":Foo", 10).unwrap().title, Title::new(0, "Foo"));
        assert_eq!(Title::parse("Star Wars: Rogue").unwrap().title, Title::new(0, "Star Wars: Rogue"));
        assert_eq!(Title::parse("image:x.png").unwrap().title.ns, 6);
    }

    #[test]
    fn rejects_bad() {
        assert!(Title::parse("a|b").is_err());
        assert!(Title::parse("").is_err());
        assert!(Title::parse("Category:").is_err());
        assert!(Title::parse("..").is_err());
    }
}
