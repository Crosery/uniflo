//! Price catalog sync: models.dev (primary) + LiteLLM (fills what models.dev lacks), fetched
//! with the system `curl` like [`crate::update`], folded into the catalog as time segments.
//!
//! Safeguards (ADR-0010):
//! - a changed price opens a new segment **from the observation time**; earlier usage keeps
//!   the old segment, so re-pricing history never happens;
//! - a component moving by more than 50% needs two consecutive identical reads;
//! - a missing or zero upstream value is never a price drop;
//! - a failed or empty source changes nothing it owns, the error is recorded;
//! - `overrides.json` is never written; `catalog.json` and the state are replaced atomically.

use super::catalog::{self, LITELLM, MODELS_DEV, match_key};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use uniflo_schema::{CatalogEntry, PriceSegment};

pub const PRIMARY_URL: &str = "https://models.dev/api.json";
pub const FALLBACK_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
/// No successful sync for this long marks the catalog `stale`.
pub const STALE_AFTER_MS: i64 = 24 * 3600 * 1000;
const CONFIRM_ABOVE: f64 = 0.5;

pub const CATALOG_FILE: &str = "catalog.json";
pub const OVERRIDES_FILE: &str = "overrides.json";
pub const STATE_FILE: &str = "sync-state.json";

/// Upstream URLs; `UNIFLO_PRICING_URL` / `UNIFLO_PRICING_FALLBACK_URL` override them (an
/// empty fallback disables it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sources {
    pub primary: String,
    pub fallback: Option<String>,
}

impl Sources {
    pub fn from_env() -> Sources {
        let primary = std::env::var("UNIFLO_PRICING_URL").ok().filter(|s| !s.is_empty());
        let fallback = match std::env::var("UNIFLO_PRICING_FALLBACK_URL") {
            Ok(s) if s.is_empty() => None,
            Ok(s) => Some(s),
            Err(_) => Some(FALLBACK_URL.to_owned()),
        };
        Sources { primary: primary.unwrap_or_else(|| PRIMARY_URL.to_owned()), fallback }
    }
}

/// `<data>/pricing/catalog.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CatalogFile {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub fetched_at: i64,
    #[serde(default)]
    pub models: Vec<CatalogEntry>,
}

/// A change seen once that waits for a second identical read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pending {
    pub prices: PriceSegment,
    pub first_seen: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SourceState {
    pub ok: bool,
    pub at: i64,
    #[serde(default)]
    pub entries: u64,
    #[serde(default)]
    pub error: Option<String>,
}

/// `<data>/pricing/sync-state.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SyncState {
    #[serde(default)]
    pub last_attempt: Option<i64>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub pending: BTreeMap<String, Pending>,
    #[serde(default)]
    pub sources: BTreeMap<String, SourceState>,
}

/// What one [`apply`] did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Applied {
    pub models: Vec<CatalogEntry>,
    /// Ids that got a new price segment.
    pub changed: Vec<String>,
    /// Ids whose large change waits for confirmation.
    pub pending: Vec<String>,
    pub added: Vec<String>,
}

fn same(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
}

fn same_opt(a: Option<f64>, b: Option<f64>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => same(a, b),
        (None, None) => true,
        _ => false,
    }
}

fn same_rates(a: &PriceSegment, b: &PriceSegment) -> bool {
    same(a.input, b.input)
        && same(a.output, b.output)
        && same(a.cache_read, b.cache_read)
        && same_opt(a.cache_write, b.cache_write)
        && a.tiers.len() == b.tiers.len()
        && a.tiers.iter().zip(&b.tiers).all(|(x, y)| {
            x.above == y.above
                && same(x.input, y.input)
                && same(x.output, y.output)
                && same(x.cache_read, y.cache_read)
                && same_opt(x.cache_write, y.cache_write)
        })
}

/// Observed rates laid over the current ones; a missing / zero observation keeps the current value.
fn overlay(cur: &PriceSegment, obs: &PriceSegment, now: i64) -> PriceSegment {
    let pick = |o: f64, c: f64| if o.is_finite() && o > 0.0 { o } else { c };
    let mut tiers = if obs.tiers.is_empty() { cur.tiers.clone() } else { obs.tiers.clone() };
    for t in &mut tiers {
        if !t.cache_read.is_finite() {
            let base = pick(obs.cache_read, cur.cache_read);
            let input = pick(obs.input, cur.input);
            t.cache_read = if input > 0.0 { t.input * base / input } else { t.input };
        }
    }
    PriceSegment {
        from: now,
        until: None,
        input: pick(obs.input, cur.input),
        output: pick(obs.output, cur.output),
        cache_read: pick(obs.cache_read, cur.cache_read),
        cache_write: obs.cache_write.filter(|v| v.is_finite() && *v > 0.0).or(cur.cache_write),
        tiers,
    }
}

/// Largest relative move of a base component; moving away from 0 counts as unbounded.
fn biggest_change(cur: &PriceSegment, next: &PriceSegment) -> f64 {
    let pairs = [
        (cur.input, next.input),
        (cur.output, next.output),
        (cur.cache_read, next.cache_read),
        (cur.cache_write.unwrap_or(0.0), next.cache_write.unwrap_or(0.0)),
    ];
    pairs
        .iter()
        .filter(|(a, b)| !same(*a, *b))
        .map(|(a, b)| if *a > 0.0 { (b - a).abs() / a } else { f64::INFINITY })
        .fold(0.0, f64::max)
}

/// Fold one round of reads into `base`. `None` = that source failed this round: entries it
/// owns stay untouched (the other source may not take them over either).
pub fn apply(
    base: Vec<CatalogEntry>,
    state: &mut SyncState,
    primary: Option<Vec<CatalogEntry>>,
    fallback: Option<Vec<CatalogEntry>>,
    now: i64,
) -> Applied {
    let mut out = Applied { models: base, ..Default::default() };
    let mut index: HashMap<String, usize> = out.models.iter().enumerate().map(|(i, e)| (match_key(&e.id), i)).collect();
    let primary_ok = primary.is_some();
    let mut observed: Vec<CatalogEntry> = primary.unwrap_or_default();
    let mut listed: HashMap<String, ()> = observed.iter().map(|e| (match_key(&e.id), ())).collect();
    for e in fallback.unwrap_or_default() {
        let k = match_key(&e.id);
        let owned_by_primary = index.get(&k).is_some_and(|&i| out.models[i].source == MODELS_DEV);
        if listed.contains_key(&k) || (!primary_ok && owned_by_primary) {
            continue;
        }
        listed.insert(k, ());
        observed.push(e);
    }
    for obs in observed {
        let Some(seg) = obs.prices.last().cloned() else { continue };
        let k = match_key(&obs.id);
        let Some(&i) = index.get(&k) else {
            let mut e = obs;
            e.prices = vec![PriceSegment { from: 0, until: None, ..seg }];
            catalog::fill_missing(std::slice::from_mut(&mut e));
            out.added.push(e.id.clone());
            index.insert(k, out.models.len());
            out.models.push(e);
            continue;
        };
        let entry = &mut out.models[i];
        entry.context_limit = obs.context_limit.or(entry.context_limit);
        entry.output_limit = obs.output_limit.or(entry.output_limit);
        entry.prices.sort_by_key(|s| s.from);
        let Some(cur) = entry.prices.last().cloned() else { continue };
        let next = overlay(&cur, &seg, now);
        if same_rates(&cur, &next) {
            state.pending.remove(&entry.id);
            continue;
        }
        let confirmed = state.pending.get(&entry.id).is_some_and(|p| same_rates(&p.prices, &next));
        if biggest_change(&cur, &next) > CONFIRM_ABOVE && !confirmed {
            state.pending.insert(entry.id.clone(), Pending { prices: next, first_seen: now });
            out.pending.push(entry.id.clone());
            continue;
        }
        state.pending.remove(&entry.id);
        if let Some(last) = entry.prices.last_mut() {
            if last.from >= now {
                // Same instant as the previous change: replace instead of stacking empty segments.
                *last = PriceSegment { from: last.from, ..next };
            } else {
                last.until = Some(now);
                entry.prices.push(next);
            }
        }
        out.changed.push(entry.id.clone());
    }
    out
}

/// GET `url` with the system curl (`-fsS --max-time 20`) and parse JSON.
pub fn fetch(url: &str) -> Result<Value, String> {
    #[cfg(windows)]
    let bin = "curl.exe";
    #[cfg(not(windows))]
    let bin = "curl";
    let out = std::process::Command::new(bin)
        .args([
            "-fsS",
            "--max-time",
            "20",
            "--user-agent",
            &format!("uniflo/{}", crate::update::current_version()),
            url,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("{bin} unavailable: {e}"))?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr).trim().chars().take(200).collect::<String>();
        return Err(format!("fetch failed (exit {:?}): {msg}", out.status.code()));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("not JSON: {e}"))
}

/// Outcome of one [`run`].
#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    pub fetched_at: Option<i64>,
    pub error: Option<String>,
    pub changed: Vec<String>,
    pub pending: Vec<String>,
    pub added: usize,
    pub models: usize,
}

pub fn load_catalog(dir: &Path) -> Option<CatalogFile> {
    let bytes = std::fs::read(dir.join(CATALOG_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn load_state(dir: &Path) -> SyncState {
    std::fs::read(dir.join(STATE_FILE)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// Write `bytes` to `path` through a temp file in the same directory + rename.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = path.with_file_name(format!(".{name}.tmp.{}", std::process::id()));
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

fn read_source(url: &str, parse: fn(&Value) -> Vec<CatalogEntry>) -> Result<Vec<CatalogEntry>, String> {
    let entries = parse(&fetch(url)?);
    if entries.is_empty() { Err("no first-party prices in response".into()) } else { Ok(entries) }
}

/// One sync round into `dir` (`<data>/pricing`). Never panics; failures land in the report
/// and in `sync-state.json`.
pub fn run(dir: &Path, sources: &Sources, now: i64) -> Report {
    let base = load_catalog(dir).map(|c| c.models).unwrap_or_else(|| super::snapshot().entries.clone());
    let mut state = load_state(dir);
    let primary = read_source(&sources.primary, catalog::parse_models_dev);
    let fallback = sources.fallback.as_deref().map(|u| read_source(u, catalog::parse_litellm));
    let mut errors = Vec::new();
    let mut ok = Vec::new();
    for (name, res) in [(MODELS_DEV, Some(&primary)), (LITELLM, fallback.as_ref())] {
        let Some(res) = res else { continue };
        let st = match res {
            Ok(e) => {
                ok.push(name);
                SourceState { ok: true, at: now, entries: e.len() as u64, error: None }
            }
            Err(err) => {
                errors.push(format!("{name}: {err}"));
                let prev = state.sources.get(name).cloned().unwrap_or_default();
                SourceState { ok: false, at: prev.at, entries: prev.entries, error: Some(err.clone()) }
            }
        };
        state.sources.insert(name.to_owned(), st);
    }
    state.last_attempt = Some(now);
    state.error = (!errors.is_empty()).then(|| errors.join("; "));
    let mut report = Report { error: state.error.clone(), ..Default::default() };
    if !ok.is_empty() {
        let applied = apply(base, &mut state, primary.ok(), fallback.and_then(Result::ok), now);
        let file = CatalogFile { version: 1, source: ok.join("+"), fetched_at: now, models: applied.models };
        match serde_json::to_vec_pretty(&file)
            .map_err(std::io::Error::other)
            .and_then(|b| write_atomic(&dir.join(CATALOG_FILE), &b))
        {
            Ok(()) => {
                report.fetched_at = Some(now);
                report.models = file.models.len();
                report.changed = applied.changed;
                report.pending = applied.pending;
                report.added = applied.added.len();
            }
            Err(e) => {
                let msg = format!("write {}: {e}", CATALOG_FILE);
                state.error = Some(state.error.map_or(msg.clone(), |x| format!("{x}; {msg}")));
                report.error = state.error.clone();
            }
        }
    }
    if let Ok(b) = serde_json::to_vec_pretty(&state) {
        let _ = write_atomic(&dir.join(STATE_FILE), &b);
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, input: f64, output: f64) -> CatalogEntry {
        CatalogEntry {
            id: id.into(),
            source: MODELS_DEV.into(),
            prices: vec![PriceSegment { input, output, cache_read: input / 10.0, ..Default::default() }],
            ..Default::default()
        }
    }

    #[test]
    fn small_change_opens_a_segment_at_observation_time() {
        let mut st = SyncState::default();
        let a = apply(vec![entry("m", 3.0, 15.0)], &mut st, Some(vec![entry("m", 3.0, 16.5)]), None, 1_000);
        assert_eq!(a.changed, ["m"]);
        let p = &a.models[0].prices;
        assert_eq!(p.len(), 2);
        assert_eq!((p[0].until, p[1].from, p[1].output), (Some(1_000), 1_000, 16.5));
    }

    #[test]
    fn big_change_needs_two_identical_reads_and_zero_is_not_a_drop() {
        let mut st = SyncState::default();
        let base = vec![entry("m", 3.0, 15.0)];
        let big = || Some(vec![entry("m", 3.0, 27.0)]);
        let a = apply(base.clone(), &mut st, big(), None, 1);
        assert_eq!((a.changed.len(), a.pending.as_slice()), (0, ["m".to_string()].as_slice()));
        // Back to the old price: the pending change is dropped.
        let a = apply(a.models, &mut st, Some(vec![entry("m", 3.0, 15.0)]), None, 2);
        assert!(a.changed.is_empty() && st.pending.is_empty());
        let a = apply(a.models, &mut st, big(), None, 3);
        assert!(a.changed.is_empty());
        let a = apply(a.models, &mut st, big(), None, 4);
        assert_eq!(a.changed, ["m"]);
        assert_eq!(a.models[0].prices.last().unwrap().from, 4);
        let a = apply(a.models, &mut st, Some(vec![entry("m", 0.0, 0.0)]), None, 5);
        assert!(a.changed.is_empty() && a.pending.is_empty(), "missing/zero is never a drop");
    }

    #[test]
    fn failed_primary_keeps_its_entries_from_the_fallback() {
        let mut st = SyncState::default();
        let mut ll = entry("m", 9.0, 9.0);
        ll.source = LITELLM.into();
        let mut other = entry("n", 1.0, 1.0);
        other.source = LITELLM.into();
        let a = apply(vec![entry("m", 3.0, 15.0)], &mut st, None, Some(vec![ll, other]), 1);
        assert_eq!(a.models[0].prices.len(), 1, "models.dev-owned entry untouched");
        assert_eq!(a.added, ["n"]);
    }
}
