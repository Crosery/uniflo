//! Caches from the ADR-0011 cache table: a block LRU (decompressed rows, keyed by block id — ids
//! are never reused, so eviction never risks serving stale content) and a per-query cold
//! verification cache, so paging or repeating a query reuses already-verified sessions instead of
//! decompressing them again. The verification cache is invalidated by `cold_gen`, not by time.

use super::store;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

struct Entry {
    rows: Arc<Vec<store::Row>>,
    bytes: usize,
    used: u64,
}

struct Inner {
    entries: HashMap<i64, Entry>,
    clock: u64,
    bytes: usize,
}

/// Decompressed cold blocks, LRU-evicted once their total size passes a byte budget.
pub struct BlockCache {
    inner: Mutex<Inner>,
    budget: usize,
}

impl BlockCache {
    pub fn new(budget_bytes: usize) -> BlockCache {
        BlockCache { inner: Mutex::new(Inner { entries: HashMap::new(), clock: 0, bytes: 0 }), budget: budget_bytes }
    }

    /// The block's rows, from cache or computed by `load` (which also reports its decompressed
    /// byte size, for the budget).
    pub fn get_or_insert(
        &self,
        bid: i64,
        load: impl FnOnce() -> anyhow::Result<(usize, Vec<store::Row>)>,
    ) -> anyhow::Result<Arc<Vec<store::Row>>> {
        if let Some(rows) = self.get(bid) {
            return Ok(rows);
        }
        let (bytes, rows) = load()?;
        Ok(self.insert(bid, bytes, rows))
    }

    pub fn get(&self, bid: i64) -> Option<Arc<Vec<store::Row>>> {
        let mut inner = self.inner.lock().unwrap();
        inner.clock += 1;
        let now = inner.clock;
        inner.entries.get_mut(&bid).map(|e| {
            e.used = now;
            e.rows.clone()
        })
    }

    /// Cache a decoded block (`bytes`: its decompressed size); returns the cached rows (an earlier
    /// insert of the same block by another thread wins, so callers share one copy).
    pub fn insert(&self, bid: i64, bytes: usize, rows: Vec<store::Row>) -> Arc<Vec<store::Row>> {
        let rows = Arc::new(rows);
        let mut inner = self.inner.lock().unwrap();
        if !inner.entries.contains_key(&bid) {
            inner.clock += 1;
            let now = inner.clock;
            inner.bytes += bytes;
            inner.entries.insert(bid, Entry { rows: rows.clone(), bytes, used: now });
            evict(&mut inner, self.budget);
        }
        inner.entries.get(&bid).map(|e| e.rows.clone()).unwrap_or(rows)
    }

    #[cfg(test)]
    pub fn contains(&self, bid: i64) -> bool {
        self.inner.lock().unwrap().entries.contains_key(&bid)
    }

    #[cfg(test)]
    #[allow(clippy::len_without_is_empty)] // test-only introspection, not a collection API
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().entries.len()
    }
}

fn evict(inner: &mut Inner, budget: usize) {
    while inner.bytes > budget && inner.entries.len() > 1 {
        let Some(&lru) = inner.entries.iter().min_by_key(|(_, e)| e.used).map(|(k, _)| k) else { break };
        if let Some(e) = inner.entries.remove(&lru) {
            inner.bytes -= e.bytes;
        }
    }
}

/// The cold tier's block directory (`blk` without payloads) as of one `cold_gen`: candidate lookup
/// maps the trigram index's block ids to sessions through it, and hit retrieval finds the block
/// holding a row. Reloaded whenever `cold_gen` moves.
#[derive(Debug, Default)]
pub struct BlockDir {
    pub generation: i64,
    /// Block id → `(sid, first row id, last row id)`.
    pub blocks: HashMap<i64, (i64, i64, i64)>,
    /// Session id → its block ids, in block order.
    pub by_sid: HashMap<i64, Vec<i64>>,
}

/// What verifying one cold session against a query found, reusable until the next cold write.
#[derive(Debug, Clone)]
pub struct SessionVerify {
    /// Whether any row (of the query's kinds) matched.
    pub matched: bool,
    /// Every matching row's scoring inputs, so a reuse rescores under the current IDF and avgdl:
    /// hot-tier statistics move with every write, so cached *scores* would drift from what a fresh
    /// verification computes. `None` when only existence was checked (the exact-`total` pass) or
    /// the session matched in more rows than [`MAX_SESSION_ROWS`].
    pub facts: Option<Arc<Facts>>,
}

/// Matching rows of one session: ids, token counts, and per-phrase occurrence counts flattened
/// row-major (`freqs.len() == ids.len() * phrases`).
#[derive(Debug, Default)]
pub struct Facts {
    pub ids: Vec<i64>,
    pub tokens: Vec<u32>,
    pub freqs: Vec<u32>,
}

impl Facts {
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Row `i`'s per-phrase counts.
    pub fn row(&self, i: usize, phrases: usize) -> &[u32] {
        &self.freqs[i * phrases..(i + 1) * phrases]
    }
}

/// A session matching in more rows than this is re-verified rather than cached.
pub const MAX_SESSION_ROWS: usize = 16 * 1024;
/// Matching rows kept across every cached query, about 4 MB at two phrases.
const MAX_ROWS: usize = 256 * 1024;

struct ColdEntry {
    key: String,
    generation: i64,
    rows: usize,
    sessions: HashMap<i64, SessionVerify>,
}

/// Up to 16 recent queries' already-verified cold sessions, so paging (or repeating) a query does
/// not re-decompress what it already checked. A `cold_gen` bump (any cold write) drops everything:
/// cheaper than tracking which sessions it touched, and cold writes are comparatively rare.
pub struct ColdCache {
    entries: Mutex<VecDeque<ColdEntry>>,
    capacity: usize,
    max_rows: usize,
}

impl Default for ColdCache {
    fn default() -> Self {
        ColdCache::new(16)
    }
}

fn facts_rows(v: &SessionVerify) -> usize {
    v.facts.as_ref().map_or(0, |f| f.len())
}

impl ColdCache {
    pub fn new(capacity: usize) -> ColdCache {
        ColdCache { entries: Mutex::new(VecDeque::new()), capacity, max_rows: MAX_ROWS }
    }

    /// Already-verified sessions for `key` at `generation`; `None` if nothing is cached (new query, or a
    /// stale generation).
    pub fn get(&self, key: &str, generation: i64) -> Option<HashMap<i64, SessionVerify>> {
        let entries = self.entries.lock().unwrap();
        entries.iter().find(|e| e.key == key && e.generation == generation).map(|e| e.sessions.clone())
    }

    /// Record more verified sessions for `key` at `generation`, creating or extending its entry and
    /// marking it most-recently-used; drops any entry for `key` at an older generation. An
    /// existence-only result never replaces one that kept its rows.
    pub fn merge(&self, key: &str, generation: i64, additions: HashMap<i64, SessionVerify>) {
        let mut entries = self.entries.lock().unwrap();
        entries.retain(|e| !(e.key == key && e.generation != generation));
        let mut e = match entries.iter().position(|e| e.key == key && e.generation == generation) {
            Some(pos) => entries.remove(pos).unwrap(),
            None => ColdEntry { key: key.to_owned(), generation, rows: 0, sessions: HashMap::new() },
        };
        for (sid, v) in additions {
            match e.sessions.get(&sid) {
                Some(old) if old.facts.is_some() && v.facts.is_none() => {}
                old => {
                    e.rows = e.rows - old.map_or(0, facts_rows) + facts_rows(&v);
                    e.sessions.insert(sid, v);
                }
            }
        }
        entries.push_back(e);
        while entries.len() > self.capacity
            || (entries.len() > 1 && entries.iter().map(|e| e.rows).sum::<usize>() > self.max_rows)
        {
            entries.pop_front();
        }
    }

    #[cfg(test)]
    #[allow(clippy::len_without_is_empty)] // test-only introspection, not a collection API
    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(n: usize) -> Facts {
        Facts { ids: (0..n as i64).collect(), tokens: vec![1; n], freqs: vec![1; n] }
    }

    fn rows(n: usize) -> Vec<store::Row> {
        (0..n)
            .map(|i| store::Row { id: i as i64, event: format!("e{i}"), ts: 0, pos: None, text: "x".into() })
            .collect()
    }

    #[test]
    fn block_cache_hits_without_recomputing() {
        let cache = BlockCache::new(1_000_000);
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let load = || {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok((64, rows(3)))
        };
        let a = cache.get_or_insert(1, load).unwrap();
        assert_eq!(a.len(), 3);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let b = cache.get_or_insert(1, || panic!("should not recompute")).unwrap();
        assert_eq!(b.len(), 3);
        assert!(cache.contains(1));
    }

    #[test]
    fn block_cache_evicts_least_recently_used_past_budget() {
        let cache = BlockCache::new(150);
        for bid in 0..3 {
            cache.get_or_insert(bid, || Ok((60, rows(1)))).unwrap();
        }
        // bid 0 is now the least recently used.
        assert!(!cache.contains(0), "evicted: {}", cache.len());
        assert!(cache.contains(1) && cache.contains(2));
        // Touching bid 1 keeps it alive over a freshly inserted bid 3.
        cache.get_or_insert(1, || panic!("cached")).unwrap();
        cache.get_or_insert(3, || Ok((60, rows(1)))).unwrap();
        assert!(cache.contains(1) && cache.contains(3));
    }

    #[test]
    fn cold_cache_merges_and_invalidates_on_generation_change() {
        let cache = ColdCache::new(2);
        let mut a = HashMap::new();
        a.insert(10, SessionVerify { matched: true, facts: Some(Arc::new(facts(1))) });
        cache.merge("q1", 5, a);
        assert!(cache.get("q1", 5).is_some());
        assert!(cache.get("q1", 6).is_none(), "generation bump invalidates");
        let mut b = HashMap::new();
        b.insert(11, SessionVerify { matched: false, facts: None });
        cache.merge("q1", 5, b);
        let merged = cache.get("q1", 5).unwrap();
        assert_eq!(merged.len(), 2, "merge extends rather than replaces");
    }

    #[test]
    fn cold_cache_keeps_rows_over_existence_and_caps_total_rows() {
        let mut cache = ColdCache::new(16);
        cache.max_rows = 10;
        cache.merge("q", 1, HashMap::from([(1, SessionVerify { matched: true, facts: Some(Arc::new(facts(4))) })]));
        cache.merge("q", 1, HashMap::from([(1, SessionVerify { matched: true, facts: None })]));
        assert!(cache.get("q", 1).unwrap()[&1].facts.is_some(), "existence-only must not drop kept rows");
        cache.merge("r", 1, HashMap::from([(2, SessionVerify { matched: true, facts: Some(Arc::new(facts(8))) })]));
        assert!(cache.get("q", 1).is_none(), "oldest entry evicted past the row budget");
        assert!(cache.get("r", 1).is_some());
    }

    #[test]
    fn cold_cache_evicts_oldest_past_capacity() {
        let cache = ColdCache::new(2);
        for i in 0..3 {
            cache.merge(&format!("q{i}"), 1, HashMap::new());
        }
        assert_eq!(cache.len(), 2);
        assert!(cache.get("q0", 1).is_none());
        assert!(cache.get("q2", 1).is_some());
    }
}
