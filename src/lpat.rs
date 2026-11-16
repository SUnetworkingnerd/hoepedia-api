//! Lua patterns over Unicode code points, for `mw.ustring` (`find`, `match`, `gmatch`, `gsub`).
//! Same syntax and semantics as Lua 5.4's string library, except that items match whole
//! characters and `%a %l %u %w %d %s %p` follow Unicode. Work is bounded so a hostile
//! pattern cannot stall a render.

const MAX_CAPTURES: usize = 32;
const MAX_DEPTH: u32 = 220;
const MAX_STEPS: u32 = 3_000_000;
const UNFINISHED: isize = -1;
const POSITION: isize = -2;

#[derive(Debug, Clone, PartialEq)]
pub enum Cap {
    Str(usize, usize),
    /// `()` position capture (0-based char index).
    Pos(usize),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Match {
    pub start: usize,
    pub end: usize,
    pub caps: Vec<Cap>,
}

pub struct Matcher<'a> {
    src: &'a [char],
    pat: &'a [char],
    level: usize,
    cap: [(usize, isize); MAX_CAPTURES],
    steps: u32,
    depth: u32,
}

fn class(c: char, cl: char) -> bool {
    let res = match cl.to_ascii_lowercase() {
        'a' => c.is_alphabetic(),
        'c' => c.is_control(),
        'd' => c.is_numeric(),
        'g' => !c.is_whitespace() && !c.is_control(),
        'l' => c.is_lowercase(),
        'p' => c.is_ascii_punctuation() || (!c.is_ascii() && !c.is_alphanumeric() && !c.is_whitespace() && !c.is_control()),
        's' => c.is_whitespace(),
        'u' => c.is_uppercase(),
        'w' => c.is_alphanumeric(),
        'x' => c.is_ascii_hexdigit(),
        _ => return cl == c,
    };
    if cl.is_ascii_uppercase() {
        !res
    } else {
        res
    }
}

type R<T> = Result<T, String>;

impl<'a> Matcher<'a> {
    pub fn new(src: &'a [char], pat: &'a [char]) -> Matcher<'a> {
        Matcher { src, pat, level: 0, cap: [(0, 0); MAX_CAPTURES], steps: 0, depth: 0 }
    }

    pub fn anchored(&self) -> bool {
        self.pat.first() == Some(&'^')
    }

    /// Try to match the pattern with the match starting exactly at `s`.
    pub fn at(&mut self, s: usize) -> R<Option<Match>> {
        self.level = 0;
        self.depth = 0;
        let p0 = self.anchored() as usize;
        let Some(e) = self.do_match(s, p0)? else { return Ok(None) };
        let mut caps = Vec::with_capacity(self.level);
        for i in 0..self.level {
            let (init, len) = self.cap[i];
            caps.push(match len {
                POSITION => Cap::Pos(init),
                UNFINISHED => return Err("unfinished capture".into()),
                l => Cap::Str(init, init + l as usize),
            });
        }
        Ok(Some(Match { start: s, end: e, caps }))
    }

    /// First match at or after `init`.
    pub fn find(&mut self, init: usize) -> R<Option<Match>> {
        let mut s = init;
        loop {
            if let Some(m) = self.at(s)? {
                return Ok(Some(m));
            }
            s += 1;
            if s > self.src.len() || self.anchored() {
                return Ok(None);
            }
        }
    }

    /// All non-overlapping matches, as `gsub`/`gmatch` iterate them.
    pub fn all(&mut self, max: usize) -> R<Vec<Match>> {
        let mut out = Vec::new();
        let (mut s, mut last) = (0usize, usize::MAX);
        while out.len() < max {
            match self.at(s)? {
                Some(m) if m.end != last => {
                    s = m.end;
                    last = m.end;
                    out.push(m);
                }
                _ if s < self.src.len() => s += 1,
                _ => break,
            }
            if self.anchored() {
                break;
            }
        }
        Ok(out)
    }

    fn class_end(&self, p: usize) -> R<usize> {
        let n = self.pat.len();
        let c = self.pat[p];
        let mut q = p + 1;
        if c == '%' {
            if q >= n {
                return Err("malformed pattern (ends with '%')".into());
            }
            return Ok(q + 1);
        }
        if c == '[' {
            if self.pat.get(q) == Some(&'^') {
                q += 1;
            }
            loop {
                if q >= n {
                    return Err("malformed pattern (missing ']')".into());
                }
                let cc = self.pat[q];
                q += 1;
                if cc == '%' && q < n {
                    q += 1;
                }
                if q >= n {
                    return Err("malformed pattern (missing ']')".into());
                }
                if self.pat[q] == ']' {
                    return Ok(q + 1);
                }
            }
        }
        Ok(q)
    }

    /// `p` is at '[', `ec` at the closing ']'.
    fn bracket(&self, c: char, p: usize, ec: usize) -> bool {
        let mut sig = true;
        let mut p = p + 1;
        if self.pat[p] == '^' {
            sig = false;
            p += 1;
        }
        while p < ec {
            if self.pat[p] == '%' {
                p += 1;
                if p < ec && class(c, self.pat[p]) {
                    return sig;
                }
            } else if p + 2 < ec && self.pat[p + 1] == '-' {
                if self.pat[p] <= c && c <= self.pat[p + 2] {
                    return sig;
                }
                p += 2;
            } else if self.pat[p] == c {
                return sig;
            }
            p += 1;
        }
        !sig
    }

    fn single(&self, s: usize, p: usize, ep: usize) -> bool {
        let Some(&c) = self.src.get(s) else { return false };
        match self.pat[p] {
            '.' => true,
            '%' => class(c, self.pat[p + 1]),
            '[' => self.bracket(c, p, ep - 1),
            x => x == c,
        }
    }

    fn do_match(&mut self, s: usize, p: usize) -> R<Option<usize>> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err("pattern too complex".into());
        }
        let r = self.match_inner(s, p);
        self.depth -= 1;
        r
    }

    fn match_inner(&mut self, mut s: usize, mut p: usize) -> R<Option<usize>> {
        let n = self.pat.len();
        loop {
            self.steps += 1;
            if self.steps > MAX_STEPS {
                return Err("pattern too complex".into());
            }
            if p >= n {
                return Ok(Some(s));
            }
            let next = self.pat.get(p + 1).copied();
            match self.pat[p] {
                '(' => {
                    return if next == Some(')') { self.start_capture(s, p + 2, POSITION) } else { self.start_capture(s, p + 1, UNFINISHED) };
                }
                ')' => return self.end_capture(s, p + 1),
                '$' if p + 1 == n => return Ok((s == self.src.len()).then_some(s)),
                '%' if next == Some('b') => {
                    if p + 3 >= n {
                        return Err("malformed pattern (missing arguments to '%b')".into());
                    }
                    let (b, e) = (self.pat[p + 2], self.pat[p + 3]);
                    if self.src.get(s) != Some(&b) {
                        return Ok(None);
                    }
                    let (mut cont, mut i, mut found) = (1, s + 1, None);
                    while i < self.src.len() {
                        let c = self.src[i];
                        if c == e {
                            cont -= 1;
                            if cont == 0 {
                                found = Some(i + 1);
                                break;
                            }
                        } else if c == b {
                            cont += 1;
                        }
                        i += 1;
                    }
                    match found {
                        Some(ns) => {
                            s = ns;
                            p += 4;
                            continue;
                        }
                        None => return Ok(None),
                    }
                }
                '%' if next == Some('f') => {
                    p += 2;
                    if self.pat.get(p) != Some(&'[') {
                        return Err("missing '[' after '%f' in pattern".into());
                    }
                    let ep = self.class_end(p)?;
                    let prev = if s == 0 { '\0' } else { self.src[s - 1] };
                    let cur = self.src.get(s).copied().unwrap_or('\0');
                    if !self.bracket(prev, p, ep - 1) && self.bracket(cur, p, ep - 1) {
                        p = ep;
                        continue;
                    }
                    return Ok(None);
                }
                '%' if next.is_some_and(|c| c.is_ascii_digit()) => {
                    let l = next.unwrap() as isize - '1' as isize;
                    if l < 0 || l as usize >= self.level || self.cap[l as usize].1 < 0 {
                        return Err(format!("invalid capture index %{}", l + 1));
                    }
                    let (init, len) = (self.cap[l as usize].0, self.cap[l as usize].1 as usize);
                    if self.src.len() - s >= len && self.src[init..init + len] == self.src[s..s + len] {
                        s += len;
                        p += 2;
                        continue;
                    }
                    return Ok(None);
                }
                _ => {}
            }
            let ep = self.class_end(p)?;
            match self.pat.get(ep) {
                Some('?') => {
                    if self.single(s, p, ep) {
                        if let Some(r) = self.do_match(s + 1, ep + 1)? {
                            return Ok(Some(r));
                        }
                    }
                    p = ep + 1;
                }
                Some('+') => return if self.single(s, p, ep) { self.max_expand(s + 1, p, ep) } else { Ok(None) },
                Some('*') => return self.max_expand(s, p, ep),
                Some('-') => {
                    loop {
                        if let Some(r) = self.do_match(s, ep + 1)? {
                            return Ok(Some(r));
                        }
                        if self.single(s, p, ep) {
                            s += 1;
                        } else {
                            return Ok(None);
                        }
                    }
                }
                _ => {
                    if !self.single(s, p, ep) {
                        return Ok(None);
                    }
                    s += 1;
                    p = ep;
                }
            }
        }
    }

    fn max_expand(&mut self, s: usize, p: usize, ep: usize) -> R<Option<usize>> {
        let mut i = 0usize;
        while self.single(s + i, p, ep) {
            i += 1;
        }
        loop {
            if let Some(r) = self.do_match(s + i, ep + 1)? {
                return Ok(Some(r));
            }
            if i == 0 {
                return Ok(None);
            }
            i -= 1;
        }
    }

    fn start_capture(&mut self, s: usize, p: usize, what: isize) -> R<Option<usize>> {
        if self.level >= MAX_CAPTURES {
            return Err("too many captures".into());
        }
        self.cap[self.level] = (s, what);
        self.level += 1;
        let r = self.do_match(s, p)?;
        if r.is_none() {
            self.level -= 1;
        }
        Ok(r)
    }

    fn end_capture(&mut self, s: usize, p: usize) -> R<Option<usize>> {
        let l = (0..self.level).rev().find(|i| self.cap[*i].1 == UNFINISHED).ok_or("invalid pattern capture")?;
        self.cap[l].1 = (s - self.cap[l].0) as isize;
        let r = self.do_match(s, p)?;
        if r.is_none() {
            self.cap[l].1 = UNFINISHED;
        }
        Ok(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(s: &str, p: &str) -> Option<(usize, usize, Vec<String>)> {
        let (sc, pc): (Vec<char>, Vec<char>) = (s.chars().collect(), p.chars().collect());
        let m = Matcher::new(&sc, &pc).find(0).unwrap()?;
        let caps = m.caps.iter().map(|c| match c {
            Cap::Str(a, b) => sc[*a..*b].iter().collect(),
            Cap::Pos(i) => format!("@{i}"),
        });
        Some((m.start, m.end, caps.collect()))
    }

    fn whole(s: &str, p: &str) -> Option<String> {
        find(s, p).map(|(a, b, _)| s.chars().skip(a).take(b - a).collect())
    }

    #[test]
    fn matches_like_lua() {
        assert_eq!(whole("hello world", "o w"), Some("o w".into()));
        assert_eq!(whole("hello world", "l+"), Some("ll".into()));
        assert_eq!(whole("key = value", "%a+"), Some("key".into()));
        assert_eq!(find("key = value", "(%w+)%s*=%s*(%w+)").unwrap().2, vec!["key", "value"]);
        assert_eq!(whole("abc123", "%d+$"), Some("123".into()));
        assert_eq!(whole("abc", "^b"), None);
        assert_eq!(whole("a.b", "%."), Some(".".into()));
        assert_eq!(whole("xaaay", "a-y"), Some("aaay".into()));
        assert_eq!(whole("xaaay", "a*"), Some("".into()));
        assert_eq!(whole("f(a(b)c) d", "%b()"), Some("(a(b)c)".into()));
        assert_eq!(whole("THE (quick) fox", "%f[%a]%a+%f[%A]"), Some("THE".into()));
        assert_eq!(find("hello hello", "(h%a+) %1").unwrap().2, vec!["hello"]);
        assert_eq!(find("abc", "()b()").unwrap().2, vec!["@1", "@2"]);
        assert_eq!(whole("a-b", "[a%-]+"), Some("a-".into()));
        assert_eq!(whole("x]y", "[]]"), Some("]".into()));
        assert_eq!(whole("abc", "[^a-b]"), Some("c".into()));
        assert_eq!(whole("color colour", "colou?r$"), Some("colour".into()));
    }

    #[test]
    fn unicode_aware() {
        assert_eq!(whole("héllo wörld", "%a+"), Some("héllo".into()));
        assert_eq!(find("naïve café", "(%a+) (%a+)").unwrap().2, vec!["naïve", "café"]);
        assert_eq!(whole("ÉCOLE normale", "%u+"), Some("ÉCOLE".into()));
        assert_eq!(whole("日本語のテキスト", "..."), Some("日本語".into()));
        assert_eq!(whole("ééé!", "é+"), Some("ééé".into()));
        assert_eq!(whole("aéb", "[éè]"), Some("é".into()));
        assert_eq!(find("añb", "ñ").map(|m| (m.0, m.1)), Some((1, 2)), "positions are in characters");
        assert_eq!(whole("α-ω", "[α-ω]+"), Some("α".into()));
    }

    #[test]
    fn iterates_like_gsub() {
        let s: Vec<char> = "one two  three".chars().collect();
        let p: Vec<char> = "%a+".chars().collect();
        let all = Matcher::new(&s, &p).all(100).unwrap();
        assert_eq!(all.iter().map(|m| (m.start, m.end)).collect::<Vec<_>>(), vec![(0, 3), (4, 7), (9, 14)]);
        // empty matches: "abc" with "x*" matches at 0,1,2,3
        let s: Vec<char> = "abc".chars().collect();
        let p: Vec<char> = "x*".chars().collect();
        assert_eq!(Matcher::new(&s, &p).all(100).unwrap().len(), 4);
        assert_eq!(Matcher::new(&s, &p).all(2).unwrap().len(), 2);
    }

    #[test]
    fn errors_and_limits() {
        let run = |s: &str, p: &str| {
            let (sc, pc): (Vec<char>, Vec<char>) = (s.chars().collect(), p.chars().collect());
            Matcher::new(&sc, &pc).find(0)
        };
        for bad in ["%", "[a", "(a", "a)", "%b", "%f", "%2", "[%"] {
            assert!(run("aaa", bad).is_err(), "{bad}");
        }
        // catastrophic backtracking is cut off, not hung
        let s = "a".repeat(40);
        let t = std::time::Instant::now();
        assert!(run(&s, "a*a*a*a*a*a*a*a*a*a*a*b").is_err());
        assert!(t.elapsed().as_secs() < 5);
        assert!(run(&"(".repeat(300), &"(".repeat(300)).is_err());
    }
}
