//! Model price catalog and per-step cost.
//!
//! Three layers, first match wins: `<data>/pricing/overrides.json` (hand-edited, never
//! written by Uniflo) → `<data>/pricing/catalog.json` (written by [`sync`]) → the snapshot
//! embedded at build time (`data/pricing-snapshot.json`, from `scripts/build-pricing.mjs`).
//! A running daemon picks up file changes (e.g. `uniflo pricing sync` from another process)
//! within a second of the next lookup.

pub mod catalog;
pub mod sync;

use catalog::{Catalog, Tokens, match_key, segment_at, step_cost};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};
use uniflo_schema::{CatalogEntry, CostSource, MatchKind, PricingStatus};

const SNAPSHOT_JSON: &str = include_str!("../../data/pricing-snapshot.json");
const RECHECK: Duration = Duration::from_secs(1);

/// The embedded catalog, built from the filtered upstream files in the snapshot.
pub struct Snapshot {
    pub generated_at: i64,
    pub entries: Vec<CatalogEntry>,
}

pub fn snapshot() -> &'static Snapshot {
    static S: OnceLock<Snapshot> = OnceLock::new();
    S.get_or_init(|| {
        let v: Value = serde_json::from_str(SNAPSHOT_JSON).unwrap_or_default();
        Snapshot {
            generated_at: v.get("generated_at").and_then(Value::as_i64).unwrap_or(0),
            entries: from_upstream(&v),
        }
    })
}

/// `{sources: {"models.dev": <api.json subset>, "litellm": <subset>}}` → entries.
pub fn from_upstream(v: &Value) -> Vec<CatalogEntry> {
    let null = Value::Null;
    let md = catalog::parse_models_dev(v.pointer("/sources/models.dev").unwrap_or(&null));
    let ll = catalog::parse_litellm(v.pointer("/sources/litellm").unwrap_or(&null));
    let mut all = catalog::merge(md, ll);
    catalog::fill_missing(&mut all);
    all
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OverridesFile {
    List(Vec<CatalogEntry>),
    Wrapped { models: Vec<CatalogEntry> },
}

fn load_overrides(dir: &Path) -> (Vec<CatalogEntry>, Option<String>) {
    let p = dir.join(sync::OVERRIDES_FILE);
    let Ok(bytes) = std::fs::read(&p) else { return (Vec::new(), None) };
    match serde_json::from_slice::<OverridesFile>(&bytes) {
        Ok(OverridesFile::List(v) | OverridesFile::Wrapped { models: v }) => {
            let mut v: Vec<CatalogEntry> = v.into_iter().filter(|e| !e.id.is_empty() && !e.prices.is_empty()).collect();
            for e in &mut v {
                e.source = "override".into();
                e.prices.sort_by_key(|s| s.from);
            }
            (v, None)
        }
        Err(e) => (Vec::new(), Some(format!("{}: {e}", sync::OVERRIDES_FILE))),
    }
}

/// A resolved model name: how it matched and the catalog entry, if any.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub kind: MatchKind,
    pub entry: Option<Arc<CatalogEntry>>,
}

/// One consistent view of all layers.
pub struct Loaded {
    catalog: Catalog,
    shared: Vec<Arc<CatalogEntry>>,
    pub overrides: usize,
    /// `(source label, fetched_at)` of `catalog.json`.
    pub synced: Option<(String, i64)>,
    pub state: sync::SyncState,
    pub problem: Option<String>,
    memo: Mutex<HashMap<String, (MatchKind, Option<usize>)>>,
}

impl Loaded {
    fn build(dir: Option<&Path>) -> Loaded {
        let snap = snapshot();
        let (overrides, problem, synced, state) = match dir {
            Some(d) => {
                let (o, p) = load_overrides(d);
                (o, p, sync::load_catalog(d), sync::load_state(d))
            }
            None => (Vec::new(), None, None, sync::SyncState::default()),
        };
        let n_over = overrides.len();
        let mut seen: HashMap<String, ()> = HashMap::new();
        let mut all: Vec<CatalogEntry> = Vec::new();
        let synced_meta = synced.as_ref().map(|c| (c.source.clone(), c.fetched_at));
        let layers = [overrides, synced.map(|c| c.models).unwrap_or_default(), snap.entries.clone()];
        for layer in layers {
            for e in layer {
                if seen.insert(match_key(&e.id), ()).is_none() {
                    all.push(e);
                }
            }
        }
        let catalog = Catalog::new(all);
        let shared = catalog.entries.iter().cloned().map(Arc::new).collect();
        Loaded {
            catalog,
            shared,
            overrides: n_over,
            synced: synced_meta,
            state,
            problem,
            memo: Mutex::new(HashMap::new()),
        }
    }

    pub fn entries(&self) -> &[Arc<CatalogEntry>] {
        &self.shared
    }

    pub fn resolve(&self, model: &str) -> Resolved {
        let hit = {
            let mut memo = self.memo.lock().unwrap();
            match memo.get(model) {
                Some(r) => *r,
                None => *memo.entry(model.to_owned()).or_insert_with(|| self.catalog.resolve(model)),
            }
        };
        Resolved { kind: hit.0, entry: hit.1.map(|i| self.shared[i].clone()) }
    }

    /// Catalog price of one step at its event time; `None` when the model is unknown.
    pub fn price(&self, model: &str, ts: i64, t: Tokens) -> Option<(f64, CostSource)> {
        let r = self.resolve(model);
        let seg = segment_at(r.entry.as_deref()?, ts)?;
        let src = if r.kind == MatchKind::Exact { CostSource::Catalog } else { CostSource::Approx };
        Some((step_cost(seg, t), src))
    }

    /// `(fetched_at, stale)`: last successful sync, else the snapshot's build time.
    pub fn freshness(&self, now: i64) -> (Option<i64>, bool) {
        let at = self.synced.as_ref().map(|s| s.1).or(Some(snapshot().generated_at)).filter(|t| *t > 0);
        (at, at.is_none_or(|t| now - t > sync::STALE_AFTER_MS))
    }
}

/// The live price view shared by the engine and its readers.
pub struct Pricing {
    dir: Option<PathBuf>,
    loaded: RwLock<Arc<Loaded>>,
    generation: AtomicU64,
    stamps: Mutex<(Option<Instant>, Stamps)>,
    sync_enabled: AtomicBool,
}

impl Pricing {
    /// `dir` is `<data>/pricing`; `None` = embedded snapshot only, nothing read or written.
    pub fn new(dir: Option<PathBuf>) -> Pricing {
        let stamps = dir.as_deref().map(file_stamps).unwrap_or_default();
        Pricing {
            loaded: RwLock::new(Arc::new(Loaded::build(dir.as_deref()))),
            dir,
            generation: AtomicU64::new(1),
            stamps: Mutex::new((Some(Instant::now()), stamps)),
            sync_enabled: AtomicBool::new(false),
        }
    }

    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    pub fn set_sync_enabled(&self, on: bool) {
        self.sync_enabled.store(on, Ordering::Relaxed);
    }

    /// Bumped whenever the effective prices may have changed.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub fn current(&self) -> Arc<Loaded> {
        self.loaded.read().unwrap().clone()
    }

    /// Re-read the files when any of them changed (checked at most once a second).
    /// Returns whether prices were reloaded.
    pub fn refresh(&self) -> bool {
        let Some(dir) = &self.dir else { return false };
        let mut st = self.stamps.lock().unwrap();
        if st.0.is_some_and(|t| t.elapsed() < RECHECK) {
            return false;
        }
        st.0 = Some(Instant::now());
        let now = file_stamps(dir);
        if now == st.1 {
            return false;
        }
        st.1 = now;
        drop(st);
        self.reload();
        true
    }

    /// Unconditional re-read (after an in-process sync).
    pub fn reload(&self) {
        let next = Arc::new(Loaded::build(self.dir.as_deref()));
        *self.loaded.write().unwrap() = next;
        if let Some(d) = &self.dir {
            *self.stamps.lock().unwrap() = (Some(Instant::now()), file_stamps(d));
        }
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    pub fn status(&self, now: i64) -> PricingStatus {
        let l = self.current();
        let (fetched_at, stale) = l.freshness(now);
        let error = match (&l.state.error, &l.problem) {
            (Some(a), Some(b)) => Some(format!("{a}; {b}")),
            (a, b) => a.clone().or_else(|| b.clone()),
        };
        PricingStatus {
            source: l.synced.as_ref().map_or_else(|| "snapshot".to_owned(), |s| s.0.clone()),
            fetched_at,
            stale,
            error,
            models: l.catalog.len() as u64,
            overrides: l.overrides as u64,
            last_attempt: l.state.last_attempt,
            pending: l.state.pending.len() as u64,
            sync_enabled: self.sync_enabled.load(Ordering::Relaxed),
        }
    }

    /// Sync now (blocking: network), then reload.
    pub fn sync_now(&self, sources: &sync::Sources) -> sync::Report {
        let Some(dir) = &self.dir else {
            return sync::Report { error: Some("no data directory".into()), ..Default::default() };
        };
        let report = sync::run(dir, sources, crate::util::now_ms());
        self.reload();
        report
    }
}

/// `(size, mtime ms)` of each pricing file, `None` when absent.
type Stamps = Vec<Option<(u64, i64)>>;

fn file_stamps(dir: &Path) -> Stamps {
    [sync::CATALOG_FILE, sync::OVERRIDES_FILE, sync::STATE_FILE]
        .iter()
        .map(|f| std::fs::metadata(dir.join(f)).ok().map(|m| (m.len(), crate::util::file_mtime_ms(&m))))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use uniflo_schema::PriceSegment;

    #[test]
    fn embedded_snapshot_prices_known_models() {
        let s = snapshot();
        assert!(s.generated_at > 0);
        assert!(s.entries.len() > 100, "snapshot has {} entries", s.entries.len());
        assert!(s.entries.iter().all(|e| e.prices.iter().all(|p| p.cache_read.is_finite())));
        let l = Loaded::build(None);
        for m in ["claude-sonnet-4-5-20250929", "gpt-5", "gemini-2.5-pro"] {
            assert_eq!(l.resolve(m).kind, MatchKind::Exact, "{m}");
        }
    }

    #[test]
    fn overrides_win_and_reload_on_change() {
        let dir = tempfile::tempdir().unwrap();
        let p = Pricing::new(Some(dir.path().to_path_buf()));
        let t = Tokens { input: 1_000_000, ..Default::default() };
        let base = p.current().price("gpt-5", 0, t).unwrap().0;
        let ov = CatalogEntry {
            id: "gpt-5".into(),
            prices: vec![PriceSegment { input: 42.0, output: 1.0, cache_read: 1.0, ..Default::default() }],
            ..Default::default()
        };
        std::fs::write(dir.path().join("overrides.json"), serde_json::to_vec(&vec![ov]).unwrap()).unwrap();
        let g = p.generation();
        p.reload();
        assert!(p.generation() > g);
        assert_ne!(base, 42.0);
        assert_eq!(p.current().price("gpt-5", 0, t), Some((42.0, CostSource::Catalog)));
        assert_eq!(p.status(0).overrides, 1);
    }
}
