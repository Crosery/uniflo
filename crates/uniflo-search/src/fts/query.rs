//! Full-text query syntax: whitespace-separated terms ANDed, `"…"` phrases, `-term` exclusions.
//!
//! Terms of 3+ characters go to the FTS5 trigram index (each quoted, so it is a plain substring);
//! shorter ones cannot be indexed by trigrams and fall back to `LIKE '%term%'`.

use std::cmp::Reverse;
use uniflo_schema::search::{HIGHLIGHT_END, HIGHLIGHT_START};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Term {
    pub text: String,
    pub neg: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// FTS5 MATCH expression over the long positive terms.
    pub fts: Option<String>,
    /// `LIKE` patterns (already escaped for `ESCAPE '\'`) that must match.
    pub like: Vec<String>,
    /// `LIKE` patterns that must not match.
    pub not_like: Vec<String>,
    /// Positive terms in query order: what a snippet highlights.
    pub marks: Vec<String>,
}

impl Plan {
    /// A short positive term means substring scanning: results go newest first.
    pub fn recent(&self) -> bool {
        !self.like.is_empty()
    }
}

pub fn parse(q: &str) -> Vec<Term> {
    let mut out = Vec::new();
    let mut it = q.chars().peekable();
    while let Some(&c) = it.peek() {
        if c.is_whitespace() {
            it.next();
            continue;
        }
        let mut neg = false;
        if c == '-' {
            it.next();
            match it.peek() {
                Some(n) if !n.is_whitespace() => neg = true,
                _ => {
                    out.push(Term { text: "-".into(), neg: false });
                    continue;
                }
            }
        }
        let mut text = String::new();
        if it.peek() == Some(&'"') {
            it.next();
            for ch in it.by_ref() {
                if ch == '"' {
                    break;
                }
                text.push(ch);
            }
        } else {
            while let Some(&ch) = it.peek() {
                if ch.is_whitespace() {
                    break;
                }
                text.push(ch);
                it.next();
            }
        }
        if !text.trim().is_empty() {
            out.push(Term { text, neg });
        }
    }
    out
}

fn is_long(t: &str) -> bool {
    t.chars().count() >= 3
}

fn quote(t: &str) -> String {
    format!("\"{}\"", t.replace('"', "\"\""))
}

pub fn like_pattern(t: &str) -> String {
    let mut s = String::with_capacity(t.len() + 2);
    s.push('%');
    for c in t.chars() {
        if matches!(c, '\\' | '%' | '_') {
            s.push('\\');
        }
        s.push(c);
    }
    s.push('%');
    s
}

/// `None` when the query has no positive term.
pub fn plan(terms: &[Term]) -> Option<Plan> {
    let pos_long: Vec<&str> = terms.iter().filter(|t| !t.neg && is_long(&t.text)).map(|t| t.text.as_str()).collect();
    let short: Vec<&str> = terms.iter().filter(|t| !t.neg && !is_long(&t.text)).map(|t| t.text.as_str()).collect();
    if pos_long.is_empty() && short.is_empty() {
        return None;
    }
    let mut not_like = Vec::new();
    let fts = (!pos_long.is_empty()).then(|| {
        let mut e = format!("({})", pos_long.iter().map(|t| quote(t)).collect::<Vec<_>>().join(" AND "));
        for t in terms.iter().filter(|t| t.neg) {
            if is_long(&t.text) {
                e.push_str(" NOT ");
                e.push_str(&quote(&t.text));
            }
        }
        e
    });
    for t in terms.iter().filter(|t| t.neg && (fts.is_none() || !is_long(&t.text))) {
        not_like.push(like_pattern(&t.text));
    }
    let marks = terms.iter().filter(|t| !t.neg).map(|t| t.text.clone()).collect();
    Some(Plan { fts, like: short.iter().map(|t| like_pattern(t)).collect(), not_like, marks })
}

/// Byte offset of the first ASCII-case-insensitive occurrence of `needle` (what SQLite `LIKE`
/// matches). Never lowercases: case mapping changes byte lengths (`İ`, `K`), so offsets taken
/// from a lowered copy would not be valid in `hay`.
fn find_ascii_ci(hay: &str, needle: &str) -> Option<usize> {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    if n.is_empty() || n.len() > h.len() {
        return None;
    }
    (0..=h.len() - n.len()).find(|&i| hay.is_char_boundary(i) && h[i..i + n.len()].eq_ignore_ascii_case(n))
}

/// The text around the first occurrence of any `needles` (40 characters before, 80 after), with
/// every occurrence inside that window highlighted. `None` when no needle occurs under ASCII case
/// folding; the FTS5 index also folds non-ASCII case, so its matches can miss here.
pub fn snippet(text: &str, needles: &[String]) -> Option<String> {
    let (s, e) = needles.iter().filter_map(|n| find_ascii_ci(text, n).map(|i| (i, i + n.len()))).min()?;
    let start = text[..s].char_indices().rev().nth(39).map_or(0, |(i, _)| i);
    let end = text[e..].char_indices().nth(80).map_or(text.len(), |(i, _)| e + i);
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    let mut at = start;
    // Earliest occurrence first, the longest needle when several start there.
    while let Some((i, Reverse(len))) =
        needles.iter().filter_map(|n| find_ascii_ci(&text[at..end], n).map(|i| (at + i, Reverse(n.len())))).min()
    {
        out.push_str(&text[at..i]);
        out.push(HIGHLIGHT_START);
        out.push_str(&text[i..i + len]);
        out.push(HIGHLIGHT_END);
        at = i + len;
    }
    out.push_str(&text[at..end]);
    if end < text.len() {
        out.push('…');
    }
    Some(clean(&out))
}

/// [`snippet`], or the start of the text when no needle occurs.
pub fn like_snippet(text: &str, needles: &[String]) -> String {
    snippet(text, needles).unwrap_or_else(|| clean(&text.chars().take(120).collect::<String>()))
}

/// One line: newlines and tabs become spaces.
pub fn clean(s: &str) -> String {
    s.chars().map(|c| if matches!(c, '\n' | '\r' | '\t') { ' ' } else { c }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(text: &str, neg: bool) -> Term {
        Term { text: text.into(), neg }
    }

    #[test]
    fn terms_phrases_and_exclusions() {
        assert_eq!(
            parse(r#"deploy "deploy script" -rollback -"hot fix" - 缓存"#),
            vec![
                t("deploy", false),
                t("deploy script", false),
                t("rollback", true),
                t("hot fix", true),
                t("-", false),
                t("缓存", false),
            ]
        );
        assert_eq!(parse(r#"  "unterminated phrase"#), vec![t("unterminated phrase", false)]);
        assert!(parse(r#" "" "#).is_empty());
    }

    #[test]
    fn long_terms_use_fts_short_terms_use_like() {
        let p = plan(&parse(r#"deploy "deploy script" -rollback"#)).unwrap();
        assert_eq!(p.fts.as_deref(), Some(r#"("deploy" AND "deploy script") NOT "rollback""#));
        assert!(p.like.is_empty() && p.not_like.is_empty() && !p.recent());

        let p = plan(&parse("缓存 parse_rel -ab")).unwrap();
        assert_eq!(p.fts.as_deref(), Some(r#"("parse_rel")"#));
        assert_eq!(p.like, vec!["%缓存%"]);
        assert_eq!(p.not_like, vec!["%ab%"]);
        assert!(p.recent());

        let p = plan(&parse("5% _b -rollback")).unwrap();
        assert_eq!(p.fts, None);
        assert_eq!(p.like, vec![r"%5\%%", r"%\_b%"]);
        assert_eq!(p.not_like, vec!["%rollback%"], "no FTS expression: long exclusions become NOT LIKE");

        assert_eq!(plan(&parse(r#"a"b"c"#)).unwrap().fts.as_deref(), Some(r#"("a""b""c")"#));
        assert!(plan(&parse("-only -excluded")).is_none());
    }

    #[test]
    fn like_snippet_is_unicode_safe() {
        // Case mapping changes byte lengths for these; the search must not slice by lowered offsets.
        let text = format!("{}İK Ω ab{}", "x".repeat(60), "y".repeat(100));
        let s = like_snippet(&text, &["AB".into()]);
        assert!(s.contains("\u{2}ab\u{3}"), "{s:?}");
        assert!(s.starts_with('…') && s.ends_with('…'));
        assert_eq!(like_snippet("ΩİKΩ", &["İK".into()]), "Ω\u{2}İK\u{3}Ω");
        assert_eq!(like_snippet("no match\nhere", &["zz".into()]), "no match here");
        assert_eq!(like_snippet("缓存\n击穿", &["缓存".into()]), "\u{2}缓存\u{3} 击穿");
    }

    #[test]
    fn snippet_highlights_every_term_in_the_window() {
        let marks = vec!["deploy".to_owned(), "script".to_owned(), "dep".to_owned()];
        assert_eq!(
            snippet("Deploy the script, then deploy again", &marks).unwrap(),
            "\u{2}Deploy\u{3} the \u{2}script\u{3}, then \u{2}deploy\u{3} again"
        );
        let far = format!("deploy{}deploy", "x".repeat(100));
        assert_eq!(snippet(&far, &marks).unwrap().matches('\u{2}').count(), 1, "outside the window");
        assert_eq!(snippet("ÄRGER", &["ärger".into()]), None, "non-ASCII case: caller falls back to FTS5");
        assert_eq!(plan(&parse("缓存 parse_rel -ab")).unwrap().marks, vec!["缓存", "parse_rel"]);
    }
}
