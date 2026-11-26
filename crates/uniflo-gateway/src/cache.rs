//! High-concurrency request coalescing (singleflight) and versioned zero-copy response cache.
//!
//! - [`Singleflight`] merges simultaneous in-flight requests for the exact same read key into one
//!   execution so bursts of identical queries never multiply CPU, disk or SQLite work.
//! - [`ResponseCache`] holds pre-serialized 200 OK [`Bytes`] payloads tagged with exact monotonic
//!   source generations (`state_gen`, `usage_gen`, `pricing_gen`, `fts_gen`): any state mutation
//!   instantly invalidates affected keys without timers, while concurrent hits share an `RwLock`
//!   read lock and zero-copy `Bytes::clone()`.

use axum::body::Bytes;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};
use tokio::sync::watch;

pub(crate) const NS_SESSIONS: u8 = 1;
pub(crate) const NS_USAGE: u8 = 2;
pub(crate) const NS_MODELS: u8 = 3;
pub(crate) const NS_SESSION_USAGE: u8 = 4;
pub(crate) const NS_SEARCH: u8 = 5;

const MAX_ENTRIES: usize = 128;
const MAX_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct CachedBody {
    pub content_type: &'static str,
    pub body: Bytes,
}

impl CachedBody {
    pub(crate) fn json<T: serde::Serialize>(value: &T) -> Result<Self, (StatusCode, String)> {
        let bytes = serde_json::to_vec(value)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("serialize response: {e}")))?;
        Ok(CachedBody { content_type: "application/json", body: Bytes::from(bytes) })
    }

    pub(crate) fn ndjson<T: serde::Serialize>(items: impl IntoIterator<Item = T>) -> Self {
        let mut out = Vec::new();
        for it in items {
            if serde_json::to_writer(&mut out, &it).is_ok() {
                out.push(b'\n');
            }
        }
        CachedBody { content_type: "application/x-ndjson", body: Bytes::from(out) }
    }

    pub(crate) fn into_response(self) -> Response {
        ([(header::CONTENT_TYPE, self.content_type)], self.body).into_response()
    }
}

pub(crate) type FlightResult = Result<CachedBody, (StatusCode, String)>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CacheKey {
    pub ns: u8,
    pub gen_tag: (u64, u64, u64),
    pub key: String,
}

struct CacheEntry {
    value: CachedBody,
    used: AtomicU64,
}

#[derive(Default)]
struct CacheInner {
    entries: HashMap<CacheKey, CacheEntry>,
    bytes: usize,
}

/// Bounded, generation-keyed zero-copy response cache.
/// Reads take only a shared `RwLock::read` and touch an atomic LRU clock.
pub(crate) struct ResponseCache {
    inner: RwLock<CacheInner>,
    clock: AtomicU64,
    max_entries: usize,
    max_bytes: usize,
}

impl Default for ResponseCache {
    fn default() -> Self {
        Self::new(MAX_ENTRIES, MAX_BYTES)
    }
}

impl ResponseCache {
    pub(crate) fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self { inner: RwLock::new(CacheInner::default()), clock: AtomicU64::new(0), max_entries, max_bytes }
    }

    pub(crate) fn get(&self, key: &CacheKey) -> Option<CachedBody> {
        let inner = self.inner.read().unwrap();
        let entry = inner.entries.get(key)?;
        let now = self.clock.fetch_add(1, Ordering::Relaxed) + 1;
        entry.used.store(now, Ordering::Relaxed);
        Some(entry.value.clone())
    }

    pub(crate) fn insert(&self, key: CacheKey, value: CachedBody) {
        let size = value.body.len();
        if size > self.max_bytes / 2 {
            return;
        }
        let mut inner = self.inner.write().unwrap();
        // Drop obsolete generations in the same namespace so stale versions never hog budget.
        let ns = key.ns;
        let gen_tag = key.gen_tag;
        let mut dropped = 0usize;
        inner.entries.retain(|k, e| {
            let obsolete = k.ns == ns && k.gen_tag != gen_tag;
            if obsolete {
                dropped += e.value.body.len();
            }
            !obsolete
        });
        inner.bytes = inner.bytes.saturating_sub(dropped);

        let now = self.clock.fetch_add(1, Ordering::Relaxed) + 1;
        if let Some(old) = inner.entries.insert(key, CacheEntry { value, used: AtomicU64::new(now) }) {
            inner.bytes = inner.bytes.saturating_sub(old.value.body.len());
        }
        inner.bytes += size;

        while (inner.entries.len() > self.max_entries || inner.bytes > self.max_bytes) && inner.entries.len() > 1 {
            let Some(lru) =
                inner.entries.iter().min_by_key(|(_, e)| e.used.load(Ordering::Relaxed)).map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(e) = inner.entries.remove(&lru) {
                inner.bytes = inner.bytes.saturating_sub(e.value.body.len());
            }
        }
    }
}

/// Async singleflight: coalesces concurrent identical in-flight calls into one execution.
/// Cancellation-safe: if a leader is cancelled before finishing, waiters fall back and run the work.
#[derive(Default)]
pub(crate) struct Singleflight {
    inflight: Mutex<HashMap<String, watch::Receiver<Option<FlightResult>>>>,
}

struct FlightGuard<'a> {
    inflight: &'a Mutex<HashMap<String, watch::Receiver<Option<FlightResult>>>>,
    key: Option<String>,
}

impl Drop for FlightGuard<'_> {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            self.inflight.lock().unwrap().remove(&key);
        }
    }
}

impl Singleflight {
    pub(crate) async fn run<F, Fut>(&self, key: String, f: F) -> FlightResult
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = FlightResult>,
    {
        enum Slot<'a> {
            Wait(watch::Receiver<Option<FlightResult>>),
            Lead(watch::Sender<Option<FlightResult>>, FlightGuard<'a>),
        }

        let slot = {
            let mut map = self.inflight.lock().unwrap();
            if let Some(rx) = map.get(&key).cloned() {
                Slot::Wait(rx)
            } else {
                let (tx, rx) = watch::channel(None);
                map.insert(key.clone(), rx);
                Slot::Lead(tx, FlightGuard { inflight: &self.inflight, key: Some(key) })
            }
        };

        match slot {
            Slot::Wait(mut rx) => {
                if let Ok(val) = rx.wait_for(|v| v.is_some()).await
                    && let Some(res) = val.clone()
                {
                    return res;
                }
                // Leader cancelled before producing a value: run directly so this caller still succeeds.
                f().await
            }
            Slot::Lead(tx, guard) => {
                let res = f().await;
                drop(guard);
                let _ = tx.send(Some(res.clone()));
                res
            }
        }
    }
}

/// Whether a session query string contains a relative time filter (`since:`, `after:`, `before:`, `until:`)
/// whose meaning depends on `now_ms()` rather than immutable state generations alone.
pub(crate) fn query_has_relative_time(q: &str) -> bool {
    q.split_whitespace().any(|tok| {
        let body = tok.strip_prefix('!').unwrap_or(tok);
        let Some((k, v)) = body.split_once(':') else { return false };
        matches!(k, "since" | "after" | "before" | "until") && is_relative_time(v)
    })
}

/// Relative duration like `30m`, `2h`, `7d`, `1w`, `10s` (as opposed to `YYYY-MM-DD` or epoch ms).
pub(crate) fn is_relative_time(v: &str) -> bool {
    let v = v.trim();
    if v.is_empty() || v.contains('-') || v.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    v.find(|c: char| !c.is_ascii_digit()).is_some_and(|i| i > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn singleflight_coalesces_concurrent_identical_requests() {
        let sf = Arc::new(Singleflight::default());
        let runs = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..16 {
            let sf = sf.clone();
            let runs = runs.clone();
            handles.push(tokio::spawn(async move {
                sf.run("k".into(), || async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                    Ok(CachedBody { content_type: "application/json", body: Bytes::from_static(b"42") })
                })
                .await
            }));
        }
        for h in handles {
            let body = h.await.unwrap().unwrap();
            assert_eq!(&body.body[..], b"42");
        }
        assert_eq!(runs.load(Ordering::SeqCst), 1, "all 16 concurrent callers share one execution");
    }

    #[test]
    fn response_cache_invalidates_obsolete_generations_and_respects_budget() {
        let cache = ResponseCache::new(4, 100);
        let k1 = CacheKey { ns: NS_SESSIONS, gen_tag: (1, 0, 0), key: "a".into() };
        let k2 = CacheKey { ns: NS_SESSIONS, gen_tag: (2, 0, 0), key: "b".into() };
        cache.insert(k1.clone(), CachedBody { content_type: "application/json", body: Bytes::from(vec![b'x'; 20]) });
        assert!(cache.get(&k1).is_some());
        cache.insert(k2.clone(), CachedBody { content_type: "application/json", body: Bytes::from(vec![b'y'; 20]) });
        assert!(cache.get(&k1).is_none(), "generation bump purges obsolete namespace entries");
        assert!(cache.get(&k2).is_some());
    }

    #[test]
    fn relative_time_detection_distinguishes_relative_and_absolute() {
        assert!(query_has_relative_time("h:claude since:7d"));
        assert!(query_has_relative_time("!before:2h foo"));
        assert!(!query_has_relative_time("h:claude since:2026-10-01"));
        assert!(!query_has_relative_time("s:work uniflo"));
        assert!(is_relative_time("30d"));
        assert!(!is_relative_time("2026-10-01"));
        assert!(!is_relative_time("1790000000000"));
    }
}
