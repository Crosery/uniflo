//! Rust-side verification of cold-tier rows: must agree with FTS5's own trigram `MATCH` exactly.
//!
//! `blk_fts` is `detail=none`, so it can only answer "which blocks contain all of these trigrams"
//! (no phrase/adjacency, no position, no row-level content). Every candidate block is decompressed
//! and each row in it is matched and scored here, in Rust, using the *same* case folding FTS5's
//! trigram tokenizer uses: SQLite's fold table does not always agree with Rust's `to_lowercase`
//! (`İ`, `ß`, fullwidth Latin, Greek sigma, …), so it is learned once at startup by probing FTS5's
//! own tokenizer rather than trusted from Rust's Unicode tables.

use super::{query, rank};
use rusqlite::Connection;
use std::collections::HashMap;
use std::sync::OnceLock;

fn fold_table() -> &'static HashMap<char, char> {
    static TABLE: OnceLock<HashMap<char, char>> = OnceLock::new();
    TABLE.get_or_init(build_fold_table)
}

/// [`fold_table`] spread over the Basic Multilingual Plane, one slot per code point (256 KB): a
/// cold query folds every candidate row, and hashing each CJK character into the map was a large
/// share of a first query's CPU.
fn bmp_folds() -> &'static [char] {
    static DENSE: OnceLock<Box<[char]>> = OnceLock::new();
    DENSE.get_or_init(|| {
        let table = fold_table();
        (0u32..0x1_0000).map(|cp| char::from_u32(cp).map_or('\0', |c| table.get(&c).copied().unwrap_or(c))).collect()
    })
}

/// Candidates: every ASCII character, plus every character that could possibly need folding —
/// either its Rust `to_lowercase` differs from itself (ordinary upper → lower, including
/// multi-character results like `İ`), or it is already "lowercase" by Rust's rules but is not the
/// canonical lowering of its own uppercase (e.g. Greek final sigma `ς`: `to_uppercase` is `Σ`, but
/// `Σ.to_lowercase()` is the *other* sigma `σ`, not `ς` — the two are fold-equivalent even though
/// neither looks "uppercase"). The mapping actually used comes from probing FTS5 below, not from
/// either of these Rust answers, since SQLite's fold table does not always agree with them.
fn fold_candidates() -> Vec<char> {
    let mut out: std::collections::HashSet<char> = (0u32..128).filter_map(char::from_u32).collect();
    for cp in 128u32..0x11_0000 {
        let Some(c) = char::from_u32(cp) else { continue };
        let mut lower = c.to_lowercase();
        let first = lower.next();
        if first != Some(c) || lower.next().is_some() {
            out.insert(c);
            continue;
        }
        let mut upper = c.to_uppercase();
        if let (Some(u), None) = (upper.next(), upper.next()) {
            let mut back = u.to_lowercase();
            if let (Some(b), None) = (back.next(), back.next())
                && b != c
            {
                out.insert(c);
            }
        }
    }
    out.into_iter().collect()
}

fn build_fold_table() -> HashMap<char, char> {
    probe(&fold_candidates())
}

/// Probe FTS5's trigram tokenizer: one row per candidate, text = the character repeated three
/// times (so the tokenizer emits exactly one trigram), then read the folded trigram back via
/// `fts5vocab`'s `instance` table (which, unlike the aggregated `row` table, keeps each document's
/// own term so it can be matched back to its candidate by rowid).
fn probe(candidates: &[char]) -> HashMap<char, char> {
    let c = Connection::open_in_memory().expect("in-memory fold probe");
    c.execute_batch(
        "CREATE VIRTUAL TABLE probe USING fts5(text, tokenize='trigram case_sensitive 0');
         CREATE VIRTUAL TABLE probe_vocab USING fts5vocab(probe, 'instance');",
    )
    .expect("create fold probe tables");
    {
        let mut ins = c.prepare_cached("INSERT INTO probe(rowid, text) VALUES (?1, ?2)").unwrap();
        for (i, ch) in candidates.iter().enumerate() {
            let text: String = std::iter::repeat_n(*ch, 3).collect();
            ins.execute(rusqlite::params![i as i64, text]).unwrap();
        }
    }
    let mut map = HashMap::new();
    let mut st = c.prepare("SELECT doc, term FROM probe_vocab").unwrap();
    let mut rows = st.query([]).unwrap();
    while let Some(r) = rows.next().unwrap() {
        let doc: i64 = r.get(0).unwrap();
        let term: String = r.get(1).unwrap();
        if let (Some(&ch), Some(folded)) = (candidates.get(doc as usize), term.chars().next())
            && folded != ch
        {
            map.insert(ch, folded);
        }
    }
    map
}

/// The character FTS5's trigram tokenizer folds `c` to under `case_sensitive 0` (identity when
/// unmapped).
pub fn fold_char(c: char) -> char {
    // FTS5 folds ASCII exactly like `to_ascii_lowercase` (`ascii_folds_like_ascii_lowercase`), so
    // the common case skips the table lookup.
    if c.is_ascii() {
        return c.to_ascii_lowercase();
    }
    match bmp_folds().get(c as usize) {
        Some(&f) => f,
        None => fold_table().get(&c).copied().unwrap_or(c),
    }
}

/// `s` with every character folded the way FTS5's trigram tokenizer would.
pub fn fold(s: &str) -> String {
    if s.is_ascii() {
        return s.to_ascii_lowercase();
    }
    s.chars().map(fold_char).collect()
}

fn quote(t: &str) -> String {
    format!("\"{}\"", t.replace('"', "\"\""))
}

/// `"abc" AND "bcd" AND ...`: every distinct overlapping trigram of an already-folded term, ANDed
/// as plain tokens. `blk_fts` is `detail=none` and cannot run phrase queries, so this only narrows
/// candidate *blocks*; [`row_matches`] confirms the real (adjacent) substring per row. `None` for
/// terms under 3 characters (not representable in a trigram index).
pub fn block_candidate_expr(folded_term: &str) -> Option<String> {
    let cs: Vec<char> = folded_term.chars().collect();
    if cs.len() < 3 {
        return None;
    }
    let mut grams: Vec<String> = cs.windows(3).map(|w| quote(&w.iter().collect::<String>())).collect();
    grams.sort();
    grams.dedup();
    Some(grams.join(" AND "))
}

/// Byte offset of the first occurrence of `needle` in `hay`, both already folded, allowing
/// overlap (so repeated counting below matches FTS5's phrase instance count on e.g. `aaaa`/`aaa`).
fn find(hay: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    memchr::memmem::find(hay.as_bytes(), needle.as_bytes())
}

/// Overlapping occurrences of `needle` in `hay` (both folded already): FTS5's phrase instance
/// count for a trigram-tokenized term is exactly this (e.g. `"aaa"` in `"aaaa"` is 2).
pub fn count_occurrences(hay: &str, needle: &str) -> i64 {
    if needle.is_empty() || needle.len() > hay.len() {
        return 0;
    }
    let finder = memchr::memmem::Finder::new(needle.as_bytes());
    let (mut n, mut start) = (0i64, 0usize);
    while let Some(i) = finder.find(&hay.as_bytes()[start..]) {
        n += 1;
        start += i + 1;
    }
    n
}

/// Positive/negative terms (already split by length) a cold row either must or must not contain,
/// built once per query and reused for every candidate row.
#[derive(Debug, Clone, Default)]
pub struct Criteria {
    /// Folded long positive terms (also the per-phrase order for [`row_frequencies`] / bm25).
    pub pos_long: Vec<String>,
    /// Folded long negative terms.
    pub neg_long: Vec<String>,
    /// Raw (unfolded) short positive terms, matched ASCII case-insensitively.
    pub like_raw: Vec<String>,
    /// Raw (unfolded) short negative terms, matched ASCII case-insensitively.
    pub not_like_raw: Vec<String>,
}

impl Criteria {
    pub fn from_plan(plan: &query::Plan) -> Criteria {
        Criteria {
            pos_long: plan.pos_long.iter().map(|t| fold(t)).collect(),
            neg_long: plan.neg_long.iter().map(|t| fold(t)).collect(),
            like_raw: plan.like_raw.clone(),
            not_like_raw: plan.not_like_raw.clone(),
        }
    }
}

/// Whether one row's text satisfies `crit`: the same semantics as FTS5 `MATCH` plus `LIKE` /
/// `NOT LIKE` for the hot tier's query plan.
pub fn row_matches(text: &str, crit: &Criteria) -> bool {
    match_text(text, crit, false).is_some()
}

/// Per-phrase term frequencies of a row satisfying `crit`, in `crit.pos_long` order (bm25's `f`);
/// `None` when the row does not match. Folds the text once for both.
pub fn row_frequencies(text: &str, crit: &Criteria) -> Option<Vec<u32>> {
    match_text(text, crit, true)
}

fn match_text(text: &str, crit: &Criteria, count: bool) -> Option<Vec<u32>> {
    if crit.like_raw.iter().any(|t| query::find_ascii_ci(text, t).is_none())
        || crit.not_like_raw.iter().any(|t| query::find_ascii_ci(text, t).is_some())
    {
        return None;
    }
    if crit.pos_long.is_empty() && crit.neg_long.is_empty() {
        return Some(Vec::new());
    }
    let folded = fold(text);
    if crit.neg_long.iter().any(|t| find(&folded, t).is_some()) {
        return None;
    }
    let mut freqs = Vec::with_capacity(if count { crit.pos_long.len() } else { 0 });
    for t in &crit.pos_long {
        if count {
            let n = count_occurrences(&folded, t);
            if n == 0 {
                return None;
            }
            freqs.push(n.min(u32::MAX as i64) as u32);
        } else if find(&folded, t).is_none() {
            return None;
        }
    }
    Some(freqs)
}

/// [`query::snippet`], or a fold-aware fallback with the same window and highlight format: used
/// for cold hits, whose match may only be found under FTS5's own (non-ASCII) case folding.
pub fn fold_snippet(text: &str, needles: &[String]) -> String {
    let folded_needles: Vec<String> = needles.iter().map(|n| fold(n)).collect();
    query::snippet(text, needles)
        .or_else(|| query::snippet_with(text, &folded_needles, find_folded))
        .unwrap_or_else(|| query::clean(&text.chars().take(120).collect::<String>()))
}

/// The first occurrence of an already-folded `needle` in `hay` under FTS5's folding, as a byte range
/// of `hay` itself: folding maps character to character, but not always to one of the same byte
/// length (`ſ` → `s`), so offsets in the folded copy are translated back.
fn find_folded(hay: &str, needle: &str) -> Option<(usize, usize)> {
    let mut folded = String::with_capacity(hay.len());
    // (offset in `folded`, offset in `hay`) at every character boundary, the end included.
    let mut bounds = Vec::with_capacity(hay.len() + 1);
    for (i, c) in hay.char_indices() {
        bounds.push((folded.len(), i));
        folded.push(fold_char(c));
    }
    bounds.push((folded.len(), hay.len()));
    let start = find(&folded, needle)?;
    let at = |f: usize| bounds.binary_search_by_key(&f, |b| b.0).ok().map(|k| bounds[k].1);
    Some((at(start)?, at(start + needle.len())?))
}

pub use rank::{bm25_multi, idf, tf_part};

#[cfg(test)]
mod tests {
    use super::*;

    fn fts5_matches(text: &str, expr: &str) -> bool {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE VIRTUAL TABLE t USING fts5(text, tokenize='trigram case_sensitive 0');").unwrap();
        c.execute("INSERT INTO t(text) VALUES (?1)", [text]).unwrap();
        c.query_row("SELECT count(*) FROM t WHERE t MATCH ?1", [expr], |r| r.get::<_, i64>(0)).unwrap() == 1
    }

    fn agrees(text: &str, term: &str) -> bool {
        let want = fts5_matches(text, &format!("\"{}\"", term.replace('"', "\"\"")));
        let got = find(&fold(text), &fold(term)).is_some();
        assert_eq!(got, want, "text={text:?} term={term:?}");
        got
    }

    #[test]
    fn fold_matches_fts5_on_tricky_scripts_and_symbols() {
        // Every term is 3+ characters: that is the only length the trigram index (and so
        // `block_candidate_expr` / `row_matches`'s long-term path) ever folds-and-matches;
        // shorter terms go through the ASCII `LIKE` path instead (tested separately).
        for (text, term) in [
            ("İstanbul", "ist"),              // İ lowers to a two-character "i̇" in Rust
            ("STRASSE", "straße"),            // ß has no simple uppercase in either table
            ("ﬃle", "ffi"),                   // ligature: one codepoint, not decomposed
            ("ÀÁÂÃÄÅ", "åäã"),                // accented Latin capitals
            ("ΣΙΓΜΑ", "σιγμα"),               // Greek capitals
            ("aςbb", "aσbb"),                 // Greek final sigma ς vs σ: fold-equivalent, neither "uppercase"
            ("ＡＢＣ", "abc"),                // fullwidth Latin
            ("MiXeD CaSe", "mixed case"),     // plain ASCII mixed case
            ("aaaa", "aaa"),                  // overlapping occurrences
            ("👍🏽 emoji", "emoji"),            // emoji + skin-tone modifier ahead of a plain match
            ("café", "cafe\u{301}"),          // NFC vs NFD: not normalized, so this does not match
            ("e\u{301}cole", "e\u{301}cole"), // combining mark, exact self-match
        ] {
            agrees(text, term);
        }
    }

    #[test]
    fn fold_table_is_idempotent_and_handles_short_and_empty_text() {
        for c in ['a', 'A', 'İ', 'ς', 'Σ', '缓', '👍'] {
            assert_eq!(fold_char(fold_char(c)), fold_char(c), "{c:?}");
        }
        assert_eq!(fold(""), "");
        assert_eq!(fold("ab"), fold("ab")); // no panic on sub-trigram text
        assert_eq!(find("", "a"), None);
        assert_eq!(find("ab", "abc"), None);
        assert!(row_matches("", &Criteria::default()));
    }

    #[test]
    fn dense_folds_agree_with_the_probed_table() {
        for (&c, &f) in fold_table() {
            assert_eq!(fold_char(c), f, "{c:?}");
        }
        for c in ['中', '文', 'ı', '\u{1F600}', 'é'] {
            assert_eq!(fold_char(c), fold_table().get(&c).copied().unwrap_or(c), "{c:?}");
        }
    }

    #[test]
    fn ascii_folds_like_ascii_lowercase() {
        // `fold_char`'s ASCII fast path skips the probed table; this pins the two together.
        for b in 0u8..128 {
            let c = b as char;
            assert_eq!(fold_table().get(&c).copied().unwrap_or(c), c.to_ascii_lowercase(), "{c:?}");
        }
    }

    #[test]
    fn fold_snippet_maps_folded_offsets_back_to_the_original_text() {
        // A character FTS5 folds to one of another byte length (e.g. `İ` → `i`): offsets taken in
        // the folded text drift from the original's.
        let (&c, &f) = fold_table()
            .iter()
            .filter(|(c, f)| c.len_utf8() != f.len_utf8() && !f.is_ascii_uppercase())
            .min()
            .expect("FTS5 folds some character to one of a different byte length");
        let text = format!("{c}é{c}xyz tail");
        let needle = format!("{f}xyz");
        assert!(query::snippet(&text, std::slice::from_ref(&needle)).is_none(), "only a fold-aware search finds it");
        assert_eq!(fold_snippet(&text, &[needle]), format!("{c}é\u{2}{c}xyz\u{3} tail"));
    }

    #[test]
    fn row_frequencies_agree_with_row_matches() {
        let crit = Criteria { pos_long: vec![fold("aaa"), fold("Deploy")], ..Default::default() };
        assert_eq!(row_frequencies("aaaa DEPLOY deploy", &crit), Some(vec![2, 2]));
        assert!(row_matches("aaaa DEPLOY deploy", &crit));
        assert_eq!(row_frequencies("aaaa only", &crit), None);
        assert!(!row_matches("aaaa only", &crit));
    }

    #[test]
    fn count_occurrences_counts_overlaps() {
        assert_eq!(count_occurrences("aaaa", "aaa"), 2);
        assert_eq!(count_occurrences("aaaa", "aaaa"), 1);
        assert_eq!(count_occurrences("ab", "abc"), 0);
        assert_eq!(count_occurrences("", "a"), 0);
        assert_eq!(count_occurrences("abc", ""), 0);
    }

    #[test]
    fn block_candidate_expr_matches_blk_fts_for_the_same_term() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE VIRTUAL TABLE blk_fts USING fts5(text, content='', contentless_delete=1, detail=none, \
             tokenize='trigram case_sensitive 0');",
        )
        .unwrap();
        c.execute("INSERT INTO blk_fts(rowid, text) VALUES (1, 'deploy script here')", []).unwrap();
        c.execute("INSERT INTO blk_fts(rowid, text) VALUES (2, 'nothing related')", []).unwrap();
        let expr = block_candidate_expr(&fold("deploy")).unwrap();
        let hit: i64 = c
            .query_row(&format!("SELECT count(*) FROM blk_fts WHERE blk_fts MATCH '{expr}'"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(hit, 1);
        assert!(block_candidate_expr("ab").is_none());
    }

    #[test]
    fn row_matches_applies_long_and_short_pos_neg_terms() {
        let crit = Criteria {
            pos_long: vec![fold("deploy")],
            neg_long: vec![fold("rollback")],
            like_raw: vec!["ok".into()],
            not_like_raw: vec!["bad".into()],
        };
        assert!(row_matches("please deploy ok now", &crit));
        assert!(!row_matches("please deploy ok now but rollback", &crit), "excluded long term");
        assert!(!row_matches("please deploy now (bad)", &crit), "excluded short term");
        assert!(!row_matches("please OK now", &crit), "missing long term");
        assert!(!row_matches("deploy now", &crit), "missing short term");
    }

    #[test]
    fn fold_snippet_finds_non_ascii_case_matches() {
        let s = fold_snippet("ÄRGER", &["ärger".into()]);
        assert!(s.contains('\u{2}') && s.to_lowercase().contains("ärger") || s.contains("ÄRGER"), "{s:?}");
    }
}
