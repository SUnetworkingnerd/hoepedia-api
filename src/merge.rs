//! Three-way merge (diff3 style) used for edit conflicts and `undo`.
//!
//! Lines are merged first. If two edits touch the same lines, the merge is retried on words,
//! so two people changing different words of one paragraph still merge cleanly. Only edits
//! that overlap at word level are reported as a conflict.

use std::time::{Duration, Instant};

use similar::{capture_diff_slices_deadline, Algorithm, DiffOp};

pub enum Merge {
    Clean(String),
    Conflict,
}

struct Hunk<'a> {
    start: usize,
    end: usize,
    items: &'a [&'a str],
    side: u8,
}

fn hunks<'a>(base: &[&'a str], other: &'a [&'a str], side: u8, deadline: Instant) -> Vec<Hunk<'a>> {
    let mut out = Vec::new();
    for op in capture_diff_slices_deadline(Algorithm::Myers, base, other, Some(deadline)) {
        let (start, end, ns, nl) = match op {
            DiffOp::Equal { .. } => continue,
            DiffOp::Delete { old_index, old_len, new_index } => (old_index, old_index + old_len, new_index, 0),
            DiffOp::Insert { old_index, new_index, new_len } => (old_index, old_index, new_index, new_len),
            DiffOp::Replace { old_index, old_len, new_index, new_len } => (old_index, old_index + old_len, new_index, new_len),
        };
        out.push(Hunk { start, end, items: &other[ns..ns + nl], side });
    }
    out
}

/// Merge two sequences derived from `base`. `None` = the changes overlap.
fn merge_seq(base: &[&str], ours: &[&str], theirs: &[&str], cap: usize) -> Option<String> {
    let deadline = Instant::now() + Duration::from_millis(400);
    let mut all = hunks(base, ours, 0, deadline);
    all.extend(hunks(base, theirs, 1, deadline));
    all.sort_by_key(|h| (h.start, h.end, h.side));

    let mut out = String::with_capacity(cap);
    let mut pos = 0usize;
    let mut prev: Option<&Hunk> = None;
    for h in &all {
        if let Some(p) = prev {
            if h.start <= p.end && h.side != p.side {
                if h.start == p.start && h.end == p.end && h.items == p.items {
                    continue;
                }
                return None;
            }
        }
        for l in &base[pos.min(h.start)..h.start] {
            out.push_str(l);
        }
        for l in h.items {
            out.push_str(l);
        }
        pos = h.end;
        if prev.is_none_or(|p| h.end >= p.end) {
            prev = Some(h);
        }
    }
    for l in &base[pos.min(base.len())..] {
        out.push_str(l);
    }
    Some(out)
}

/// The line-level merge with conflicting regions marked diff3-style, so a client can show
/// both versions and let a person choose:
///
/// ```text
/// <<<<<<< current
/// (lines as they are on the wiki now)
/// =======
/// (lines of the incoming edit)
/// >>>>>>> yours
/// ```
/// Returns the text and the number of conflicting regions.
pub fn with_markers(base: &str, ours: &str, theirs: &str) -> (String, usize) {
    let lines = |s| -> Vec<&str> { str::split_inclusive(s, '\n').collect() };
    let (b, o, t) = (lines(base), lines(ours), lines(theirs));
    let deadline = Instant::now() + Duration::from_millis(400);
    let mut all = hunks(&b, &o, 0, deadline);
    all.extend(hunks(&b, &t, 1, deadline));
    all.sort_by_key(|h| (h.start, h.end, h.side));
    let mut out = String::with_capacity(ours.len() + theirs.len());
    let push = |out: &mut String, items: &[&str]| {
        for l in items {
            out.push_str(l);
        }
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
    };
    let (mut pos, mut conflicts, mut i) = (0usize, 0usize, 0usize);
    while i < all.len() {
        // a region = a maximal run of overlapping hunks (or insertions at the same point)
        let (start, mut end, mut j) = (all[i].start, all[i].end, i + 1);
        while j < all.len() && (all[j].start < end || (all[j].start == end && start == end && all[j].end == all[j].start)) {
            end = end.max(all[j].end);
            j += 1;
        }
        let region = &all[i..j];
        for l in &b[pos..start] {
            out.push_str(l);
        }
        // what each side made of base[start..end]
        let side = |s: u8| -> Vec<&str> {
            let mut v = Vec::new();
            let mut p = start;
            for h in region.iter().filter(|h| h.side == s) {
                v.extend_from_slice(&b[p..h.start]);
                v.extend_from_slice(h.items);
                p = h.end;
            }
            v.extend_from_slice(&b[p..end]);
            v
        };
        let (mine, yours) = (side(0), side(1));
        let both = region.iter().any(|h| h.side == 0) && region.iter().any(|h| h.side == 1);
        if !both || mine == yours {
            for l in if region.iter().any(|h| h.side == 0) { &mine } else { &yours } {
                out.push_str(l);
            }
        } else {
            conflicts += 1;
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("<<<<<<< current\n");
            push(&mut out, &mine);
            out.push_str("=======\n");
            push(&mut out, &yours);
            out.push_str(">>>>>>> yours\n");
        }
        pos = end;
        i = j;
    }
    for l in &b[pos.min(b.len())..] {
        out.push_str(l);
    }
    (out, conflicts)
}

/// Words, whitespace runs and single punctuation characters. Concatenating the tokens gives
/// back the input exactly.
fn tokens(s: &str) -> Vec<&str> {
    #[derive(PartialEq, Clone, Copy)]
    enum K {
        Word,
        Space,
        Newline,
        Other,
    }
    let kind = |c: char| {
        if c == '\n' {
            K::Newline
        } else if c.is_whitespace() {
            K::Space
        } else if c.is_alphanumeric() || c == '_' {
            K::Word
        } else {
            K::Other
        }
    };
    let mut out = Vec::new();
    let mut start = 0;
    let mut prev: Option<K> = None;
    for (i, c) in s.char_indices() {
        let k = kind(c);
        // Punctuation and newlines are always tokens of their own.
        if let Some(pk) = prev {
            if pk != k || matches!(k, K::Other | K::Newline) {
                out.push(&s[start..i]);
                start = i;
            }
        }
        prev = Some(k);
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// Above this size the word-level pass is skipped (the line-level result stands).
const MAX_WORD_MERGE_BYTES: usize = 1_500_000;

/// Merge `ours` (current) and `theirs` (incoming), both derived from `base`.
pub fn merge3(base: &str, ours: &str, theirs: &str) -> Merge {
    if ours == theirs {
        return Merge::Clean(ours.to_string());
    }
    if base == ours {
        return Merge::Clean(theirs.to_string());
    }
    if base == theirs {
        return Merge::Clean(ours.to_string());
    }
    let cap = base.len().max(ours.len()).max(theirs.len());
    let lines = |s| -> Vec<&str> { str::split_inclusive(s, '\n').collect() };
    let (b, o, t) = (lines(base), lines(ours), lines(theirs));
    if let Some(m) = merge_seq(&b, &o, &t, cap) {
        return Merge::Clean(m);
    }
    if cap > MAX_WORD_MERGE_BYTES {
        return Merge::Conflict;
    }
    let (b, o, t) = (tokens(base), tokens(ours), tokens(theirs));
    match merge_seq(&b, &o, &t, cap) {
        Some(m) => Merge::Clean(m),
        None => Merge::Conflict,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(m: Merge) -> String {
        match m {
            Merge::Clean(s) => s,
            Merge::Conflict => panic!("conflict"),
        }
    }

    #[test]
    fn disjoint_changes_merge() {
        assert_eq!(clean(merge3("a\nb\nc\nd\n", "a\nB\nc\nd\n", "a\nb\nc\nD\n")), "a\nB\nc\nD\n");
    }

    #[test]
    fn inserts_and_deletes_merge() {
        assert_eq!(clean(merge3("1\n2\n3\n4\n5\n", "0\n1\n2\n3\n4\n5\n", "1\n2\n4\n5\n6\n")), "0\n1\n2\n4\n5\n6\n");
    }

    #[test]
    fn same_line_conflicts() {
        assert!(matches!(merge3("a\nb\nc\n", "a\nX\nc\n", "a\nY\nc\n"), Merge::Conflict));
    }

    #[test]
    fn different_words_on_one_line_merge() {
        let base = "The quick brown fox jumps over the lazy dog.\nSecond line.\n";
        let ours = "The quick red fox jumps over the lazy dog.\nSecond line.\n";
        let theirs = "The quick brown fox jumps over the sleepy dog.\nSecond line.\n";
        assert_eq!(clean(merge3(base, ours, theirs)), "The quick red fox jumps over the sleepy dog.\nSecond line.\n");
        // one side rewords a sentence, the other appends to the same paragraph
        assert_eq!(clean(merge3("Alpha beta gamma.", "Alpha BETA gamma.", "Alpha beta gamma. Delta.")), "Alpha BETA gamma. Delta.");
    }

    #[test]
    fn same_word_conflicts() {
        assert!(matches!(merge3("one two three", "one TWO three", "one deux three"), Merge::Conflict));
        // adjacent line edits merge at word level
        assert_eq!(clean(merge3("a\nb\nc\n", "a\nB\nc\n", "a\nb\nC\n")), "a\nB\nC\n");
    }

    #[test]
    fn conflict_markers() {
        let (m, n) = with_markers("a\nb\nc\nd\n", "a\nB1\nc\nd\nmine\n", "a\nB2\nc\nD\n");
        assert_eq!(n, 1);
        assert_eq!(m, "a\n<<<<<<< current\nB1\n=======\nB2\n>>>>>>> yours\nc\nD\nmine\n");
        // identical changes and one-sided changes are not conflicts
        assert_eq!(with_markers("a\nb\n", "a\nX\n", "a\nX\n"), ("a\nX\n".to_string(), 0));
        assert_eq!(with_markers("a\nb\n", "a\nb\n", "a\nY\n"), ("a\nY\n".to_string(), 0));
        // no trailing newline
        let (m, n) = with_markers("one", "uno", "eins");
        assert_eq!((m.as_str(), n), ("<<<<<<< current\nuno\n=======\neins\n>>>>>>> yours\n", 1));
    }

    #[test]
    fn tokens_round_trip() {
        let s = "Héllo, wörld!\n\n  [[Link|x]] {{t|a=1}}\t end";
        assert_eq!(tokens(s).concat(), s);
    }

    #[test]
    fn identical_change_is_not_a_conflict() {
        assert_eq!(clean(merge3("a\nb\n", "a\nX\n", "a\nX\n")), "a\nX\n");
    }

    #[test]
    fn same_point_inserts_conflict() {
        assert!(matches!(merge3("a\nb\n", "a\nX\nb\n", "a\nY\nb\n"), Merge::Conflict));
    }
}
