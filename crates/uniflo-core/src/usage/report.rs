//! `GET /v1/usage`: group steps and prompts of a session set by one dimension.

use super::tz::Tz;
use super::{Ledger, UsageIndex};
use crate::pricing::Loaded;
use crate::pricing::catalog::base_name;
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use uniflo_schema::{MatchKind, PricingBrief, Session, UsageReport, UsageRow};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GroupBy {
    #[default]
    Harness,
    Model,
    Project,
    Cwd,
    Dir,
    Day,
    Hour,
    Weekday,
    Session,
}

impl GroupBy {
    pub const ALL: &[&str] = &["harness", "model", "project", "cwd", "dir", "day", "hour", "weekday", "session"];

    pub fn parse(s: &str) -> Result<GroupBy, String> {
        Ok(match s {
            "harness" | "h" => GroupBy::Harness,
            "model" | "m" => GroupBy::Model,
            "project" | "repo" => GroupBy::Project,
            "cwd" => GroupBy::Cwd,
            "dir" => GroupBy::Dir,
            "day" | "date" => GroupBy::Day,
            "hour" => GroupBy::Hour,
            "weekday" | "dow" => GroupBy::Weekday,
            "session" => GroupBy::Session,
            other => return Err(format!("unknown group_by {other:?}; one of {}", GroupBy::ALL.join(", "))),
        })
    }

    pub fn as_str(self) -> &'static str {
        GroupBy::ALL[self as usize]
    }

    fn by_time(self) -> bool {
        matches!(self, GroupBy::Day | GroupBy::Hour | GroupBy::Weekday)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sort {
    /// Cost for dimensions, key for time buckets.
    #[default]
    Auto,
    Cost,
    Tokens,
    Steps,
    Sessions,
    Prompts,
    Key,
}

impl Sort {
    pub fn parse(s: &str) -> Result<Sort, String> {
        Ok(match s {
            "" | "auto" => Sort::Auto,
            "cost" => Sort::Cost,
            "tokens" => Sort::Tokens,
            "steps" => Sort::Steps,
            "sessions" => Sort::Sessions,
            "prompts" => Sort::Prompts,
            "key" | "name" => Sort::Key,
            other => return Err(format!("unknown sort {other:?}; one of cost, tokens, steps, sessions, prompts, key")),
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct Query {
    pub group_by: GroupBy,
    /// Event-time window `[since, until)`, epoch ms.
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub tz: Tz,
    /// Only sessions whose cwd is this directory or below it; the base of `dir` grouping.
    pub under: Option<String>,
    /// Path components below the base for `dir` grouping (default 1).
    pub depth: Option<usize>,
    /// Keep this many rows; the rest fold into one `(other)` row so totals still add up.
    pub limit: Option<usize>,
    pub sort: Sort,
}

pub const OTHER_KEY: &str = "(other)";

#[derive(Default)]
struct Acc {
    label: String,
    sessions: Vec<u32>,
    steps: u64,
    prompts: u64,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    reasoning: u64,
    cost: f64,
    priced: u64,
    unpriced: u64,
}

impl Acc {
    fn session(&mut self, si: u32) {
        if self.sessions.last() != Some(&si) {
            self.sessions.push(si);
        }
    }

    fn absorb(&mut self, o: Acc) {
        let mut s = std::mem::take(&mut self.sessions);
        s.extend(o.sessions);
        s.sort_unstable();
        s.dedup();
        self.sessions = s;
        self.steps += o.steps;
        self.prompts += o.prompts;
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
        self.reasoning += o.reasoning;
        self.cost += o.cost;
        self.priced += o.priced;
        self.unpriced += o.unpriced;
    }

    fn row(&self, key: String) -> UsageRow {
        UsageRow {
            key,
            label: self.label.clone(),
            sessions: self.sessions.len() as u64,
            steps: self.steps,
            prompts: self.prompts,
            input: self.input,
            output: self.output,
            cache_read: self.cache_read,
            cache_write: self.cache_write,
            reasoning: self.reasoning,
            cost_usd: (self.priced > 0).then_some(self.cost),
            unpriced_steps: self.unpriced,
        }
    }
}

/// Git root above `cwd` (only `stat`s, memoized per process), else `cwd` itself.
pub fn project_of(cwd: &str) -> String {
    static MEMO: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let memo = MEMO.get_or_init(Default::default);
    if let Some(r) = memo.lock().unwrap().get(cwd) {
        return r.clone();
    }
    let root = Path::new(cwd)
        .ancestors()
        .find(|d| d.join(".git").exists())
        .map_or_else(|| cwd.to_owned(), |d| d.display().to_string());
    memo.lock().unwrap().insert(cwd.to_owned(), root.clone());
    root
}

fn trim_dir(s: &str) -> &str {
    let t = s.trim_end_matches('/');
    if t.is_empty() && s.starts_with('/') { "/" } else { t }
}

/// `cwd` relative to `base` as components; `None` when it is not under it.
fn below<'a>(cwd: &'a str, base: &str) -> Option<Vec<&'a str>> {
    let cwd = trim_dir(cwd);
    let rest = if base == "/" {
        cwd.strip_prefix('/')?
    } else if cwd == base {
        ""
    } else {
        cwd.strip_prefix(base)?.strip_prefix('/')?
    };
    Some(rest.split('/').filter(|c| !c.is_empty()).collect())
}

fn join(base: &str, parts: &[&str]) -> String {
    if parts.is_empty() {
        return base.to_owned();
    }
    let sep = if base.ends_with('/') { "" } else { "/" };
    format!("{base}{sep}{}", parts.join("/"))
}

fn common_root<'a>(cwds: impl Iterator<Item = &'a str>) -> Option<String> {
    let mut root: Option<Vec<&str>> = None;
    for c in cwds {
        let parts: Vec<&str> = trim_dir(c).split('/').collect();
        root = Some(match root {
            None => parts,
            Some(r) => r.iter().zip(&parts).take_while(|(a, b)| a == b).map(|(a, _)| *a).collect(),
        });
    }
    root.map(|r| {
        let j = r.join("/");
        if j.is_empty() { "/".to_owned() } else { j }
    })
}

const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// Aggregate `sessions` (already filtered by the caller's `q`) by `q.group_by`.
pub fn report(index: &UsageIndex, sessions: &[&Session], q: &Query, p: &Loaded, now: i64) -> UsageReport {
    let in_window = |ts: i64| q.since.is_none_or(|s| ts >= s) && q.until.is_none_or(|u| ts < u);
    let under = q.under.as_deref().map(trim_dir);
    let picked: Vec<(&Session, Cow<Ledger>)> = sessions
        .iter()
        .filter(|s| under.is_none_or(|u| s.cwd.as_deref().is_some_and(|c| below(c, u).is_some())))
        .filter_map(|s| index.view(&s.key).map(|l| (*s, l)))
        .collect();
    let dir_base: Option<String> = match (q.group_by, under) {
        (GroupBy::Dir, Some(u)) => Some(u.to_owned()),
        (GroupBy::Dir, None) => common_root(picked.iter().filter_map(|(s, _)| s.cwd.as_deref())),
        _ => None,
    };
    let depth = q.depth.unwrap_or(1).max(1);
    let mut model_keys: HashMap<String, (String, String)> = HashMap::new();
    let mut model_key = |m: Option<&str>| -> (String, String) {
        let m = m.unwrap_or("");
        if let Some(k) = model_keys.get(m) {
            return k.clone();
        }
        model_keys
            .entry(m.to_owned())
            .or_insert_with(|| {
                if m.is_empty() {
                    return (String::new(), "(unknown)".into());
                }
                let r = p.resolve(m);
                match (r.kind, r.entry) {
                    (MatchKind::Exact, Some(e)) => (e.id.clone(), e.id.clone()),
                    _ => {
                        let b = base_name(m).to_owned();
                        (b.clone(), b)
                    }
                }
            })
            .clone()
    };
    let time_key = |ts: i64| -> (String, String) {
        if ts <= 0 {
            return (String::new(), "(unknown time)".into());
        }
        let c = q.tz.civil(ts);
        match q.group_by {
            GroupBy::Day => {
                let k = format!("{:04}-{:02}-{:02}", c.year, c.month, c.day);
                (k.clone(), k)
            }
            GroupBy::Hour => (format!("{:02}", c.hour), format!("{:02}:00", c.hour)),
            _ => (c.weekday.to_string(), WEEKDAYS[(c.weekday as usize).saturating_sub(1) % 7].into()),
        }
    };
    let session_key = |s: &Session| -> (String, String) {
        match q.group_by {
            GroupBy::Harness => (s.harness.clone(), s.harness.clone()),
            GroupBy::Cwd => match &s.cwd {
                Some(c) => (c.clone(), c.clone()),
                None => (String::new(), "(no cwd)".into()),
            },
            GroupBy::Project => match &s.cwd {
                Some(c) => {
                    let r = project_of(c);
                    (r.clone(), r)
                }
                None => (String::new(), "(no cwd)".into()),
            },
            GroupBy::Dir => match (s.cwd.as_deref(), dir_base.as_deref()) {
                (Some(c), Some(base)) => match below(c, base) {
                    Some(parts) => {
                        let k = join(base, &parts[..parts.len().min(depth)]);
                        let label =
                            if parts.is_empty() { ".".to_owned() } else { parts[..parts.len().min(depth)].join("/") };
                        (k, label)
                    }
                    None => (c.to_owned(), c.to_owned()),
                },
                _ => (String::new(), "(no cwd)".into()),
            },
            _ => {
                let label = s.title.clone().or_else(|| s.preview.clone()).unwrap_or_else(|| s.key.clone());
                (s.key.clone(), label)
            }
        }
    };

    let mut rows: HashMap<String, Acc> = HashMap::new();
    let mut all_sessions = 0u64;
    for (si, (s, l)) in picked.iter().enumerate() {
        let si = si as u32;
        let fixed = (!q.group_by.by_time() && q.group_by != GroupBy::Model).then(|| session_key(s));
        let mut any = false;
        let mut turn_model: HashMap<u32, Option<&str>> = HashMap::new();
        for st in &l.steps {
            turn_model.entry(st.turn).or_insert(st.model.as_deref());
            if !in_window(st.ts) {
                continue;
            }
            any = true;
            let (k, label) = match &fixed {
                Some(f) => f.clone(),
                None if q.group_by == GroupBy::Model => model_key(st.model.as_deref()),
                None => time_key(st.ts),
            };
            let a = rows.entry(k).or_insert_with(|| Acc { label, ..Default::default() });
            a.session(si);
            a.steps += 1;
            a.input += st.input;
            a.output += st.output;
            a.cache_read += st.cache_read;
            a.cache_write += st.cache_write;
            a.reasoning += st.reasoning;
            match st.cost {
                Some(c) => {
                    a.cost += c;
                    a.priced += 1;
                }
                None => a.unpriced += 1,
            }
        }
        for pr in &l.prompts {
            if !in_window(pr.ts) {
                continue;
            }
            any = true;
            let (k, label) = match &fixed {
                Some(f) => f.clone(),
                None if q.group_by == GroupBy::Model => {
                    model_key(turn_model.get(&pr.turn).copied().flatten().or(l.model.as_deref()))
                }
                None => time_key(pr.ts),
            };
            let a = rows.entry(k).or_insert_with(|| Acc { label, ..Default::default() });
            a.session(si);
            a.prompts += 1;
        }
        all_sessions += u64::from(any);
    }

    let mut list: Vec<(String, Acc)> = rows.into_iter().collect();
    let sort = match q.sort {
        Sort::Auto if q.group_by.by_time() => Sort::Key,
        Sort::Auto => Sort::Cost,
        s => s,
    };
    let tokens = |a: &Acc| a.input + a.output + a.cache_read + a.cache_write;
    list.sort_by(|(ka, a), (kb, b)| {
        let ord = match sort {
            Sort::Key | Sort::Auto => std::cmp::Ordering::Equal,
            Sort::Cost => b.cost.total_cmp(&a.cost).then(tokens(b).cmp(&tokens(a))),
            Sort::Tokens => tokens(b).cmp(&tokens(a)),
            Sort::Steps => b.steps.cmp(&a.steps),
            Sort::Sessions => b.sessions.len().cmp(&a.sessions.len()),
            Sort::Prompts => b.prompts.cmp(&a.prompts),
        };
        ord.then_with(|| ka.cmp(kb))
    });
    if let Some(n) = q.limit
        && list.len() > n
    {
        let mut other = Acc::default();
        let folded = list.split_off(n);
        let count = folded.len();
        for (_, a) in folded {
            other.absorb(a);
        }
        other.label = format!("{count} more");
        list.push((OTHER_KEY.to_owned(), other));
    }
    let rows: Vec<UsageRow> = list.iter().map(|(k, a)| a.row(k.clone())).collect();
    let mut totals =
        UsageRow { key: "total".into(), label: "total".into(), sessions: all_sessions, ..Default::default() };
    for r in &rows {
        totals.steps += r.steps;
        totals.prompts += r.prompts;
        totals.input += r.input;
        totals.output += r.output;
        totals.cache_read += r.cache_read;
        totals.cache_write += r.cache_write;
        totals.reasoning += r.reasoning;
        totals.unpriced_steps += r.unpriced_steps;
        if let Some(c) = r.cost_usd {
            totals.cost_usd = Some(totals.cost_usd.unwrap_or(0.0) + c);
        }
    }
    let (fetched_at, stale) = p.freshness(now);
    UsageReport {
        group_by: q.group_by.as_str().to_owned(),
        since: q.since,
        until: q.until,
        tz: q.tz.name(),
        under: dir_base.filter(|_| q.group_by == GroupBy::Dir).or_else(|| under.map(str::to_owned)),
        rows,
        totals,
        pricing: PricingBrief { fetched_at, stale },
        indexing: index.progress(),
    }
}

/// `since` / `until` values: `30m` `2h` `7d` `1w` ago, `YYYY-MM-DD` (midnight in `tz`) or
/// epoch milliseconds.
pub fn parse_time(v: &str, now: i64, tz: &Tz) -> Result<i64, String> {
    let v = v.trim();
    let bad = || format!("bad time {v:?}; use 30m, 2h, 7d, 1w, YYYY-MM-DD or epoch ms");
    if !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) {
        return v.parse().map_err(|_| bad());
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(v, "%Y-%m-%d") {
        let utc = d.and_hms_opt(0, 0, 0).ok_or_else(bad)?.and_utc().timestamp_millis();
        // The zone's offset near that midnight; exact except within hours of a DST switch.
        return Ok(utc - tz.offset_at(utc) as i64 * 1000);
    }
    let split = v.find(|c: char| !c.is_ascii_digit()).filter(|i| *i > 0).ok_or_else(bad)?;
    let n: i64 = v[..split].parse().map_err(|_| bad())?;
    let unit = match &v[split..] {
        "s" => 1_000,
        "m" | "min" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        "w" => 604_800_000,
        _ => return Err(bad()),
    };
    Ok(now - n * unit)
}

/// `in:~/x` / `cwd:~/x` → the home-expanded path (search filters compare raw cwd text).
pub fn expand_home(q: &str, home: &str) -> String {
    q.split_whitespace()
        .map(|tok| {
            let (neg, body) = tok.strip_prefix('!').map_or(("", tok), |b| ("!", b));
            for k in ["in:", "cwd:"] {
                if let Some(rest) = body.strip_prefix(k)
                    && (rest == "~" || rest.starts_with("~/"))
                {
                    return format!("{neg}{k}{home}{}", &rest[1..]);
                }
            }
            tok.to_owned()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_parse_relative_dates_in_zone_and_epoch() {
        let now = 1_790_943_179_000;
        let utc = Tz::Fixed(0);
        assert_eq!(parse_time("2h", now, &utc), Ok(now - 7_200_000));
        assert_eq!(parse_time("1790000000000", now, &utc), Ok(1_790_000_000_000));
        assert_eq!(parse_time("2026-10-02", now, &utc), Ok(1_790_899_200_000));
        assert_eq!(parse_time("2026-10-02", now, &Tz::Fixed(8 * 3600)), Ok(1_790_899_200_000 - 8 * 3_600_000));
        assert!(parse_time("soon", now, &utc).is_err());
        assert!(parse_time("d", now, &utc).is_err());
    }

    #[test]
    fn dir_helpers() {
        assert_eq!(below("/w/repo/a/b", "/w/repo"), Some(vec!["a", "b"]));
        assert_eq!(below("/w/repo", "/w/repo"), Some(vec![]));
        assert_eq!(below("/w/repo2", "/w/repo"), None);
        assert_eq!(below("/x", "/"), Some(vec!["x"]));
        assert_eq!(common_root(["/w/a/b", "/w/a/c", "/w/a"].into_iter()).as_deref(), Some("/w/a"));
        assert_eq!(common_root(["/a", "/b"].into_iter()).as_deref(), Some("/"));
        assert_eq!(join("/w", &["a", "b"]), "/w/a/b");
        assert_eq!(join("/", &["a"]), "/a");
    }

    #[test]
    fn home_expansion_only_touches_cwd_filters() {
        assert_eq!(expand_home("h:claude in:~/work !cwd:~ ~/x", "/u"), "h:claude in:/u/work !cwd:/u ~/x");
    }
}
