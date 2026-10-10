//! Session search: fd-style structured filters + fzf-style fuzzy ranking.
//!
//! Query language (tokens separated by spaces, all must hold):
//!
//! | token | meaning |
//! |---|---|
//! | `h:claude,codex` / `harness:` | harness id in list |
//! | `s:work` / `status:idle` / `is:work` | status |
//! | `in:uniflo` / `cwd:` | cwd contains (case-insensitive) |
//! | `since:2h` / `before:7d` / `since:2026-10-01` | updated_at window (`m`,`h`,`d`,`w`) |
//! | `is:sub` / `is:root` / `is:live` | has parent / no parent / attached process |
//! | `is:archived` | cleaned up, served from Uniflo's archive |
//! | `id:01a0` | session id or key prefix |
//! | `parent:<key>` | children of a session |
//! | anything else | fzf syntax over title, preview, cwd, harness, id: `foo`, `'exact`, `^prefix`, `suffix$`, `!not` |
//!
//! Prefix a filter with `!` to negate it (`!h:codex`, `!is:sub`).
//!
//! [`fts`] is the full-text index over event bodies (`/v1/search`, `uniflo grep`); its
//! `filter` reuses this syntax.

pub mod fts;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use uniflo_schema::{Session, Status};

#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    Harness(Vec<String>),
    Status(Status),
    Cwd(String),
    Since(i64),
    Before(i64),
    Sub(bool),
    Live,
    Archived,
    Id(String),
    Parent(String),
    Not(Box<Filter>),
}

impl Filter {
    pub fn matches(&self, s: &Session) -> bool {
        match self {
            Filter::Harness(hs) => hs.iter().any(|h| h == &s.harness),
            Filter::Status(st) => s.status == *st,
            Filter::Cwd(c) => s.cwd.as_deref().is_some_and(|cwd| cwd.to_lowercase().contains(c)),
            Filter::Since(t) => s.updated_at >= *t,
            Filter::Before(t) => s.updated_at < *t,
            Filter::Sub(sub) => s.parent.is_some() == *sub,
            Filter::Live => s.pid.is_some(),
            Filter::Archived => s.archived,
            Filter::Id(p) => s.id.starts_with(p.as_str()) || s.key.starts_with(p.as_str()),
            Filter::Parent(k) => s.parent.as_deref() == Some(k.as_str()),
            Filter::Not(f) => !f.matches(s),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Query {
    pub filters: Vec<Filter>,
    /// Remaining free text, fzf syntax.
    pub text: String,
}

impl Query {
    pub fn parse(q: &str, now_ms: i64) -> Query {
        let mut filters = Vec::new();
        let mut text: Vec<&str> = Vec::new();
        for tok in q.split_whitespace() {
            let (neg, body) = match tok.strip_prefix('!') {
                Some(rest) if rest.contains(':') => (true, rest),
                _ => (false, tok),
            };
            match parse_filter(body, now_ms) {
                Some(f) => filters.push(if neg { Filter::Not(Box::new(f)) } else { f }),
                None => text.push(tok),
            }
        }
        Query { filters, text: text.join(" ") }
    }

    pub fn is_empty(&self) -> bool {
        self.filters.is_empty() && self.text.is_empty()
    }

    /// Remove the plain `since:` / `before:` filters and return them as a `[since, before)`
    /// window, for callers that filter on event time rather than session `updated_at`.
    /// The latest `since` and the earliest `before` win; negated ones stay filters.
    pub fn take_window(&mut self) -> (Option<i64>, Option<i64>) {
        let (mut since, mut before) = (None::<i64>, None::<i64>);
        self.filters.retain(|f| match f {
            Filter::Since(t) => {
                since = Some(since.map_or(*t, |s| s.max(*t)));
                false
            }
            Filter::Before(t) => {
                before = Some(before.map_or(*t, |b| b.min(*t)));
                false
            }
            _ => true,
        });
        (since, before)
    }
}

fn parse_filter(tok: &str, now: i64) -> Option<Filter> {
    let (k, v) = tok.split_once(':')?;
    if v.is_empty() {
        return None;
    }
    let lower = v.to_lowercase();
    Some(match k {
        "h" | "harness" => Filter::Harness(lower.split(',').filter(|s| !s.is_empty()).map(str::to_owned).collect()),
        "s" | "status" => Filter::Status(parse_status(&lower)?),
        "in" | "cwd" => Filter::Cwd(lower),
        "since" | "after" => Filter::Since(parse_when(&lower, now)?),
        "before" | "until" => Filter::Before(parse_when(&lower, now)?),
        "id" => Filter::Id(v.to_owned()),
        "parent" => Filter::Parent(v.to_owned()),
        "is" => match lower.as_str() {
            "sub" | "child" => Filter::Sub(true),
            "root" | "main" => Filter::Sub(false),
            "live" | "running" => Filter::Live,
            "archived" => Filter::Archived,
            other => Filter::Status(parse_status(other)?),
        },
        _ => return None,
    })
}

fn parse_status(s: &str) -> Option<Status> {
    match s {
        "work" | "working" | "busy" => Some(Status::Work),
        "idle" => Some(Status::Idle),
        _ => None,
    }
}

/// `30m`, `2h`, `7d`, `1w` relative to now, or an absolute `YYYY-MM-DD` (UTC midnight).
fn parse_when(v: &str, now: i64) -> Option<i64> {
    if let Some((y, rest)) = v.split_once('-') {
        let (m, d) = rest.split_once('-')?;
        return Some(days_from_civil(y.parse().ok()?, m.parse().ok()?, d.parse().ok()?) * 86_400_000);
    }
    let split = v.find(|c: char| !c.is_ascii_digit())?;
    let n: i64 = v[..split].parse().ok()?;
    let unit = match &v[split..] {
        "s" => 1_000,
        "m" | "min" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        "w" => 604_800_000,
        _ => return None,
    };
    Some(now - n * unit)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[derive(Debug, Clone, Copy)]
pub struct Hit<'a> {
    pub session: &'a Session,
    /// Fuzzy score (0 when the query has no free text).
    pub score: u32,
}

/// Filter + rank. Without free text, results keep the input order (callers pass newest-first).
pub fn search<'a>(sessions: &'a [Session], q: &Query, limit: usize) -> Vec<Hit<'a>> {
    let filtered = sessions.iter().filter(|s| q.filters.iter().all(|f| f.matches(s)));
    if q.text.is_empty() {
        return filtered.take(limit).map(|s| Hit { session: s, score: 0 }).collect();
    }
    let pattern = Pattern::parse(&q.text, CaseMatching::Smart, Normalization::Smart);
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut buf = Vec::new();
    let mut hay = String::new();
    let mut hits: Vec<Hit<'a>> = filtered
        .filter_map(|s| {
            haystack(s, &mut hay);
            pattern.score(Utf32Str::new(&hay, &mut buf), &mut matcher).map(|score| Hit { session: s, score })
        })
        .collect();
    hits.sort_by(|a, b| b.score.cmp(&a.score).then(b.session.updated_at.cmp(&a.session.updated_at)));
    hits.truncate(limit);
    hits
}

fn haystack(s: &Session, out: &mut String) {
    out.clear();
    for part in
        [s.title.as_deref(), s.preview.as_deref(), s.cwd.as_deref(), Some(s.harness.as_str()), Some(s.id.as_str())]
            .into_iter()
            .flatten()
    {
        out.push_str(part);
        out.push(' ');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_790_943_179_000;

    fn s(harness: &str, id: &str, title: &str, cwd: &str, status: Status, updated_ago_ms: i64) -> Session {
        Session {
            key: format!("{harness}:{id}"),
            harness: harness.into(),
            id: id.into(),
            parent: None,
            title: Some(title.into()),
            cwd: Some(cwd.into()),
            model: None,
            preview: None,
            source: String::new(),
            started_at: None,
            updated_at: NOW - updated_ago_ms,
            status,
            status_since: 0,
            status_reason: None,
            pid: None,
            usage: None,
            archived: false,
        }
    }

    fn corpus() -> Vec<Session> {
        let mut sub = s("claude", "agent-1", "Review PR", "/w/Uniflo", Status::Idle, 5_000);
        sub.parent = Some("claude:aaa".into());
        let mut live = s("omp", "01a0", "Refactor gateway", "/w/api-console", Status::Work, 1_000);
        live.pid = Some(42);
        let mut old = s("codex", "bbb", "Fix login bug", "/w/web", Status::Idle, 3 * 86_400_000);
        old.archived = true;
        vec![live, sub, s("claude", "aaa", "Build Uniflo daemon", "/w/Uniflo", Status::Work, 60_000), old]
    }

    fn keys(hits: &[Hit]) -> Vec<String> {
        hits.iter().map(|h| h.session.key.clone()).collect()
    }

    #[test]
    fn parses_filters_and_text() {
        let q = Query::parse("h:claude,codex s:work in:Uniflo since:2h daemon !is:sub", NOW);
        assert_eq!(q.text, "daemon");
        assert_eq!(q.filters.len(), 5);
        assert_eq!(q.filters[0], Filter::Harness(vec!["claude".into(), "codex".into()]));
        assert_eq!(q.filters[3], Filter::Since(NOW - 7_200_000));
        assert_eq!(q.filters[4], Filter::Not(Box::new(Filter::Sub(true))));
        let q = Query::parse("http://x weird:thing ^pre", NOW);
        assert!(q.filters.is_empty(), "unknown keys stay free text");
        assert_eq!(q.text, "http://x weird:thing ^pre");
    }

    #[test]
    fn absolute_dates() {
        let q = Query::parse("since:2026-10-02", NOW);
        assert_eq!(q.filters[0], Filter::Since(1_790_899_200_000));
    }

    #[test]
    fn window_is_lifted_out_of_the_filters() {
        let mut q = Query::parse("h:claude since:2d since:1d before:1h !since:3d foo", NOW);
        assert_eq!(q.take_window(), (Some(NOW - 86_400_000), Some(NOW - 3_600_000)));
        assert_eq!(q.filters.len(), 2, "harness and the negated since stay");
        assert_eq!(q.text, "foo");
    }

    #[test]
    fn filters_combine() {
        let c = corpus();
        let q = |s: &str| keys(&search(&c, &Query::parse(s, NOW), 10));
        assert_eq!(q("h:claude"), vec!["claude:agent-1", "claude:aaa"]);
        assert_eq!(q("s:work"), vec!["omp:01a0", "claude:aaa"]);
        assert_eq!(q("in:uniflo is:root"), vec!["claude:aaa"]);
        assert_eq!(q("since:1d"), vec!["omp:01a0", "claude:agent-1", "claude:aaa"]);
        assert_eq!(q("before:1d"), vec!["codex:bbb"]);
        assert_eq!(q("is:live"), vec!["omp:01a0"]);
        assert_eq!(q("!h:claude"), vec!["omp:01a0", "codex:bbb"]);
        assert_eq!(q("parent:claude:aaa"), vec!["claude:agent-1"]);
        assert_eq!(q("id:bb"), vec!["codex:bbb"]);
        assert_eq!(q("is:archived"), vec!["codex:bbb"]);
        assert_eq!(q("!is:archived"), vec!["omp:01a0", "claude:agent-1", "claude:aaa"]);
    }

    #[test]
    fn fuzzy_ranking_and_fzf_syntax() {
        let c = corpus();
        let q = |s: &str| keys(&search(&c, &Query::parse(s, NOW), 10));
        assert_eq!(q("unfdmn").first().map(String::as_str), Some("claude:aaa"), "subsequence match");
        assert_eq!(q("'gateway"), vec!["omp:01a0"]);
        assert_eq!(q("^fix"), vec!["codex:bbb"]);
        assert!(!q("uniflo !daemon").contains(&"claude:aaa".to_string()));
        assert!(q("zzzzqqq").is_empty());
    }

    #[test]
    fn limit_applies() {
        let c = corpus();
        assert_eq!(search(&c, &Query::default(), 2).len(), 2);
    }
}
