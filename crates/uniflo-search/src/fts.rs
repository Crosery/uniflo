//! Full-text index over event bodies: SQLite, tiered by session age (ADR-0011). Sessions active
//! within `hot_days` live in the ADR-0008 trigram-with-position format (hot); older sessions are
//! cut into zstd-compressed blocks with a block-level trigram index (cold, [`cold`]), verified and
//! scored in Rust ([`verify`]) against candidate blocks.
//!
//! One background thread owns the write connection. It
//! - migrates a leftover `fts-v1.sqlite` once, before anything else, while queries already read
//!   every session it has converted ([`migrate`]);
//! - backfills every session through [`Engine::history`] pages, newest sessions first and
//!   throttled, recording per-session progress so a restart only catches up what changed; a
//!   session already older than `hot_days` is backfilled straight into the cold tier;
//! - follows the engine broadcast, so new and overwritten events are searchable within ~100 ms,
//!   promoting a cold session back to hot first if needed;
//! - reconciles against the engine's session list periodically, because summary reads are never
//!   broadcast and a lagging subscriber loses envelopes, and drops sessions the engine removed;
//! - merges the index into one segment, in throttled steps, when a backlog leaves it spread over
//!   many ([`store::scattered`]);
//! - compacts hot sessions past `hot_days` into the cold tier on idle turns, bounded per commit;
//! - runs bounded `incremental_vacuum` steps on idle turns once free pages pass 5%.
//!
//! A second thread then reads the index into the OS page cache ([`FtsStatus::warming`]).
//!
//! Queries run on a pool of read-only connections and never wait for the writer (WAL). Relevance
//! queries score the hot tier fully, then verify cold candidate sessions newest first on several
//! threads, stopping once no older session's score upper bound can still enter the page
//! ([`Fts::rank_cold`]).
//!
//! Embedding: `engine.index()` first, then [`Fts::start`]; the gateway serves it as `/v1/search`.

pub mod cache;
pub mod cold;
pub mod migrate;
pub mod query;
pub mod rank;
pub mod store;
pub mod verify;

use crate::{Query as SessionQuery, search as filter_sessions};
use anyhow::Result;
use rusqlite::{Connection, params_from_iter, types::Value as Sql};
use std::cell::Cell;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use store::SessRow;
use tokio::sync::broadcast::{self, error::TryRecvError};
use uniflo_core::util::now_ms;
use uniflo_core::{Engine, HistoryQuery};
use uniflo_schema::search::{FtsStatus, IndexProgress, SearchHit, SearchOrder, SearchResponse, SearchSession};
use uniflo_schema::{Envelope, Event, SCHEMA_VERSION, Session};

/// Bump when what or how events are indexed changes: the next start rebuilds the index. The tag
/// also carries the storage format (`format2` = the tiered layout of this ADR); the Uniflo
/// version lives outside the tag, in `meta.version` (see [`store::OpenState::VersionChanged`]).
pub const FTS_FORMAT: u32 = 2;
/// Sessions active within this many days live in the hot tier by default.
pub const DEFAULT_HOT_DAYS: u32 = 7;
const DEFAULT_BLOCK_CACHE_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_TOTAL_BUDGET: Duration = Duration::from_millis(250);
/// bm25's tf-part never exceeds `k1 + 1`; used for the cold tier's score upper bound.
const K1_PLUS_1: f64 = 2.2;

const IDLE: Duration = Duration::from_millis(100);
/// Writes accumulate in one transaction for at most this long (see [`Writer::transaction`]).
const COMMIT_EVERY: Duration = Duration::from_millis(500);
const HITS_PER_SESSION: usize = 3;
/// Snippet window in trigram tokens (≈ characters).
const SNIPPET_TOKENS: u32 = 40;
/// Wall-clock budget of a short-term (`LIKE`) scan; past it the query answers with what it found.
pub const LIKE_BUDGET: Duration = Duration::from_secs(2);
/// Rows per `LIKE` statement, so the budget is checked inside large sessions too.
const LIKE_CHUNK: i64 = 1024;
/// Leaf pages of work per step of the post-backlog segment merge (≈ 8 MB written per commit).
const MERGE_PAGES: i64 = 2000;
/// Bytes of free space to reclaim per `incremental_vacuum` idle step.
const VACUUM_PAGES: i64 = 2000;
const DAY_MS: f64 = 86_400_000.0;

/// A clock, injectable in tests so a session can be made deterministically "old" without faking
/// file mtimes: production uses [`now_ms`].
pub type NowFn = Arc<dyn Fn() -> i64 + Send + Sync>;

#[derive(Clone)]
pub struct FtsOptions {
    pub path: PathBuf,
    /// Index format; a different value (like a different schema version) discards the index at
    /// start. Does not include the Uniflo version (see [`store::OpenState`]).
    pub format: u32,
    /// Interval of the full reconciliation against the engine's session list.
    pub reconcile: Duration,
    /// Events per history page while backfilling.
    pub page: usize,
    /// Backfill throttle: after each page the writer sleeps `pause ×` the time the page took.
    pub pause: f32,
    /// See [`LIKE_BUDGET`].
    pub like_budget: Duration,
    /// Sessions whose `updated_at` is at least this many days old live in the cold tier.
    pub hot_days: u32,
    /// Decompressed cold blocks kept in memory (LRU).
    pub block_cache_bytes: usize,
    /// Wall-clock budget, after a relevance page is settled, to keep checking remaining cold
    /// candidates so `total` is exact rather than a lower bound.
    pub total_budget: Duration,
    /// Threads verifying cold blocks within one query; 0 means one per core, at most 8.
    pub verify_threads: usize,
    /// The clock [`Writer`] uses to decide a session's age; overridable in tests.
    pub now: NowFn,
}

impl std::fmt::Debug for FtsOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FtsOptions")
            .field("path", &self.path)
            .field("format", &self.format)
            .field("reconcile", &self.reconcile)
            .field("page", &self.page)
            .field("pause", &self.pause)
            .field("like_budget", &self.like_budget)
            .field("hot_days", &self.hot_days)
            .field("block_cache_bytes", &self.block_cache_bytes)
            .field("total_budget", &self.total_budget)
            .field("verify_threads", &self.verify_threads)
            .finish_non_exhaustive()
    }
}

impl Default for FtsOptions {
    fn default() -> Self {
        FtsOptions {
            path: default_path(),
            format: FTS_FORMAT,
            reconcile: Duration::from_secs(30),
            page: 500,
            pause: 0.5,
            like_budget: LIKE_BUDGET,
            hot_days: DEFAULT_HOT_DAYS,
            block_cache_bytes: DEFAULT_BLOCK_CACHE_BYTES,
            total_budget: DEFAULT_TOTAL_BUDGET,
            verify_threads: 0,
            now: Arc::new(now_ms),
        }
    }
}

/// `fts-v2.sqlite` in Uniflo's cache directory (follows `UNIFLO_HOME`).
pub fn default_path() -> PathBuf {
    uniflo_core::cache::dir().join("fts-v2.sqlite")
}

#[derive(Debug, Clone, Default)]
pub struct SearchParams {
    pub q: String,
    /// Session filter in the `uniflo ls` syntax, e.g. `h:claude in:work since:7d`.
    pub filter: Option<String>,
    /// Only these event kinds.
    pub kinds: Option<Vec<String>>,
    /// Sessions per page (clamped to 1..=200, default 20 when 0).
    pub limit: usize,
    pub offset: usize,
}

#[derive(Debug)]
pub enum SearchError {
    /// The query cannot run (e.g. only exclusions): the caller's fault.
    Query(String),
    Index(anyhow::Error),
}

impl std::fmt::Display for SearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SearchError::Query(m) => f.write_str(m),
            SearchError::Index(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for SearchError {}

impl From<anyhow::Error> for SearchError {
    fn from(e: anyhow::Error) -> Self {
        SearchError::Index(e)
    }
}

impl From<rusqlite::Error> for SearchError {
    fn from(e: rusqlite::Error) -> Self {
        SearchError::Index(e.into())
    }
}

enum Cmd {
    Index(Box<Session>, Vec<Event>),
    Remove(String),
}

#[derive(Default)]
struct Shared {
    sessions: HashMap<String, SessRow>,
    keys: HashMap<i64, String>,
    progress: IndexProgress,
    indexing: bool,
    /// Until the writer has decided on, or finished, the post-backlog segment merge.
    merging: bool,
    warming: bool,
    /// Converting a leftover `fts-v1.sqlite` (see [`migrate`]).
    migrating: bool,
    reclaimed_bytes: u64,
    build_ms: Option<u64>,
    errors: u64,
    last_error: Option<String>,
}

impl Shared {
    fn remember(&mut self, key: &str, row: SessRow) {
        self.keys.insert(row.sid, key.to_owned());
        self.sessions.insert(key.to_owned(), row);
    }

    fn forget(&mut self, key: &str) {
        if let Some(r) = self.sessions.remove(key) {
            self.keys.remove(&r.sid);
        }
    }

    fn sync_progress(&mut self, expected: &[Session]) {
        let known: HashSet<&str> = expected.iter().map(|s| s.key.as_str()).collect();
        let pinned = self.sessions.iter().filter(|(key, row)| row.pinned && !known.contains(key.as_str())).count();
        let done = expected
            .iter()
            .filter(|s| {
                self.sessions
                    .get(&s.key)
                    .is_some_and(|row| row.pinned || row.done_upd.is_some_and(|t| t >= s.updated_at))
            })
            .count();
        self.progress.total = expected.len() + pinned;
        self.progress.done = done + pinned;
        self.indexing = self.progress.done < self.progress.total;
    }
}

struct Inner {
    engine: Arc<Engine>,
    opts: FtsOptions,
    rebuilt: bool,
    shared: Mutex<Shared>,
    readers: Mutex<Vec<Connection>>,
    cmds: Mutex<Vec<Cmd>>,
    pending_commands: AtomicUsize,
    park_when_idle: AtomicBool,
    stop: AtomicBool,
    blocks: cache::BlockCache,
    cold_cache: cache::ColdCache,
    dir: Mutex<Option<Arc<cache::BlockDir>>>,
    /// Cold sessions verified since start, for tests (and future diagnostics).
    cold_verified: AtomicUsize,
}

impl Inner {
    fn note_error(&self, msg: String) {
        tracing::debug!("full-text index: {msg}");
        let mut sh = self.shared.lock().unwrap();
        sh.errors += 1;
        sh.last_error = Some(msg);
    }

    fn now(&self) -> i64 {
        (self.opts.now)()
    }

    fn cutoff(&self) -> i64 {
        self.now() - self.opts.hot_days as i64 * store::DAY_MS
    }
}

/// Handle to a running index. Dropping it stops the writer thread.
pub struct Fts {
    inner: Arc<Inner>,
    writer: Option<std::thread::JoinHandle<()>>,
    warmer: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Fts {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::Relaxed);
        for h in [self.writer.take(), self.warmer.take()].into_iter().flatten() {
            let _ = h.join();
        }
    }
}

fn open_writer(path: &Path, tag: &str) -> Result<(Connection, store::OpenState, bool)> {
    let (conn, state) = store::open(path, tag)?;
    let migrating = migrate::pending(path);
    if state == store::OpenState::VersionChanged && !migrating {
        store::clear_progress_non_pinned(&conn)?;
    }
    Ok((conn, state, migrating))
}

impl Fts {
    /// Open (or rebuild) the index and start its writer thread. Call after [`Engine::index`];
    /// returns once the backlog is known, so [`Fts::status`] is meaningful immediately.
    ///
    /// A `fts-v1.sqlite` next to `opts.path` is converted by the writer thread before its normal
    /// loop ([`migrate`]); meanwhile `status().migrating` is set and queries already see every
    /// session converted so far.
    pub fn start(engine: Arc<Engine>, opts: FtsOptions) -> Result<Fts> {
        let tag = format!("uniflo-fts/format{}/schema{}", opts.format, SCHEMA_VERSION);
        let lease = store::try_writer(&opts.path)?;
        let (conn, open_state, migrating) = if lease.is_some() {
            open_writer(&opts.path, &tag)?
        } else {
            let until = Instant::now() + Duration::from_secs(2);
            loop {
                let opened = store::open_reader(&opts.path).and_then(|c| {
                    let current = store::meta_get(&c, "tag")?;
                    if let Some(current) = current {
                        anyhow::ensure!(current == tag, "active full-text index has another format");
                        Ok(Some(c))
                    } else {
                        Ok(None)
                    }
                });
                match opened {
                    Ok(Some(c)) => break (c, store::OpenState::Unchanged, false),
                    Ok(None) => {}
                    Err(e) => {
                        let initializing = matches!(e.downcast_ref::<rusqlite::Error>(),
                            Some(rusqlite::Error::SqliteFailure(code, msg))
                                if matches!(code.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
                                    || (code.code == rusqlite::ErrorCode::CannotOpen && !opts.path.exists())
                                    || msg.as_deref() == Some("no such table: meta"));
                        if !initializing {
                            return Err(e);
                        }
                    }
                }
                anyhow::ensure!(Instant::now() < until, "full-text index writer is still initializing");
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        let reclaimed_bytes = store::meta_get(&conn, "reclaimed_bytes")?.and_then(|v| v.parse().ok()).unwrap_or(0);
        let mut shared = Shared {
            merging: lease.is_some(),
            warming: true,
            migrating,
            indexing: migrating,
            reclaimed_bytes,
            ..Default::default()
        };
        for (k, r) in store::load_sessions(&conn)? {
            shared.remember(&k, r);
        }
        if lease.is_none() {
            shared.sync_progress(&engine.sessions());
        }
        let fresh = shared.sessions.is_empty();
        let rx = engine.subscribe(None).1;
        let inner = Arc::new(Inner {
            engine,
            opts: opts.clone(),
            rebuilt: open_state.rebuilt(),
            shared: Mutex::new(shared),
            readers: Mutex::new(Vec::new()),
            cmds: Mutex::new(Vec::new()),
            pending_commands: AtomicUsize::new(0),
            park_when_idle: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            blocks: cache::BlockCache::new(opts.block_cache_bytes),
            cold_cache: cache::ColdCache::default(),
            dir: Mutex::new(None),
            cold_verified: AtomicUsize::new(0),
        });
        let mut w = Writer {
            inner: inner.clone(),
            conn,
            lease,
            rx,
            queue: VecDeque::new(),
            pass: None,
            next_reconcile: Instant::now(),
            reader_version: 0,
            lagged: Cell::new(false),
            tx_since: Cell::new(None),
            build: None,
            merge_check: true,
            merge: None,
            compact_check: true,
            vacuum_check: true,
        };
        if w.lease.is_some() && !migrating {
            w.reconcile();
            w.commit(true);
            if fresh && !w.queue.is_empty() {
                w.build = Some(Instant::now());
            }
        }
        let writer = std::thread::Builder::new().name("uniflo-fts".into()).spawn(move || w.run())?;
        let i = inner.clone();
        let warmer = std::thread::Builder::new().name("uniflo-fts-warm".into()).spawn(move || {
            let t0 = Instant::now();
            match warm(&i) {
                Ok(true) => tracing::info!("full-text index warmed in {} ms", t0.elapsed().as_millis()),
                Ok(false) => {}
                Err(e) => i.note_error(format!("warm: {e:#}")),
            }
            i.shared.lock().unwrap().warming = false;
        })?;
        Ok(Fts { inner, writer: Some(writer), warmer: Some(warmer) })
    }

    pub fn path(&self) -> &Path {
        &self.inner.opts.path
    }

    pub fn status(&self) -> FtsStatus {
        let path = &self.inner.opts.path;
        let bytes = std::iter::once(path.clone())
            .chain(store::wal_paths(path))
            .filter_map(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .sum();
        // SQL before taking `shared`: the writer takes that lock between its steps.
        let (ct, blocks) = match self.reader() {
            Ok(c) => {
                (store::counters(&c).ok(), c.query_row("SELECT count(*) FROM blk", [], |r| r.get::<_, i64>(0)).ok())
            }
            Err(_) => (None, None),
        };
        let sh = self.inner.shared.lock().unwrap();
        FtsStatus {
            indexing: sh.indexing,
            progress: sh.progress,
            path: path.display().to_string(),
            bytes,
            rebuilt: self.inner.rebuilt,
            warming: sh.warming,
            build_ms: sh.build_ms,
            errors: sh.errors,
            last_error: sh.last_error.clone(),
            hot_days: Some(self.inner.opts.hot_days),
            hot_events: ct.map(|c| c.hot_rows.max(0) as u64),
            cold_events: ct.map(|c| c.cold_rows.max(0) as u64),
            cold_blocks: blocks.map(|n| n.max(0) as u64),
            migrating: sh.migrating,
            reclaimed_bytes: sh.reclaimed_bytes,
        }
    }

    /// Block until the backlog and queued [`Fts::index_session`] / [`Fts::remove_session`] calls
    /// are done. Returns false on timeout.
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let until = Instant::now() + timeout;
        loop {
            let idle =
                !self.inner.shared.lock().unwrap().indexing && self.inner.pending_commands.load(Ordering::Acquire) == 0;
            if idle {
                // Let the writer finish the loop turn that drained the last command.
                std::thread::sleep(IDLE * 2);
                if !self.inner.shared.lock().unwrap().indexing
                    && self.inner.pending_commands.load(Ordering::Acquire) == 0
                {
                    return true;
                }
            }
            if Instant::now() >= until {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Index a session the engine does not (or will no longer) serve, e.g. an archived transcript.
    /// Such a session is pinned: reconciliation and `removed` envelopes leave it alone; drop it
    /// with [`Fts::remove_session`]. Applied asynchronously by the writer.
    pub fn index_session(&self, session: Session, events: Vec<Event>) {
        let mut cmds = self.inner.cmds.lock().unwrap();
        self.inner.pending_commands.fetch_add(1, Ordering::Release);
        cmds.push(Cmd::Index(Box::new(session), events));
    }

    /// Delete a session's rows (pinned or not), from whichever tier(s) hold it. Applied
    /// asynchronously by the writer.
    pub fn remove_session(&self, key: &str) {
        let mut cmds = self.inner.cmds.lock().unwrap();
        self.inner.pending_commands.fetch_add(1, Ordering::Release);
        cmds.push(Cmd::Remove(key.to_owned()));
    }

    /// Release a one-shot build's writer lease once caught up, while still serving searches.
    /// Explicit queued writes can acquire it again; a daemon should keep following instead.
    pub fn release_writer_when_idle(&self) {
        self.inner.park_when_idle.store(true, Ordering::Release);
    }

    /// Blocking: run a query (see `docs/search.md#全文检索`).
    pub fn search(&self, p: &SearchParams) -> Result<SearchResponse, SearchError> {
        let terms = query::parse(&p.q);
        let plan = query::plan(&terms)
            .ok_or_else(|| SearchError::Query("q needs at least one term that is not an exclusion".into()))?;
        let limit = if p.limit == 0 { 20 } else { p.limit.min(200) };
        let filter = p.filter.as_deref().map(str::trim).filter(|f| !f.is_empty());
        let now = now_ms();
        let allowed = filter.map(|f| self.allowed(f, now));
        let order = if plan.recent() { SearchOrder::Recent } else { SearchOrder::Relevance };

        // Read before scanning: the writer commits before it clears `indexing`, so a scan that
        // starts after `indexing == false` sees everything that flag promises.
        let (indexing, progress) = {
            let sh = self.inner.shared.lock().unwrap();
            (sh.indexing, sh.progress)
        };
        let pooled = self.reader()?;
        // One read transaction for the scan and every snippet: a consistent snapshot, and a single
        // WAL read-lock round trip. Each round trip takes SQLite's process-wide VFS mutex, which the
        // engine's SQLite adapters hold while opening their databases.
        let conn = pooled.unchecked_transaction()?;
        let dir = self.block_dir(&conn)?;
        let kinds: Option<HashSet<i64>> =
            p.kinds.as_ref().map(|ks| ks.iter().filter_map(|k| store::kind_code(k.trim())).collect());
        let mut groups: HashMap<i64, Vec<Cand>> = HashMap::new();
        let end: ScanEnd = match &plan.fts {
            Some(_) if !plan.recent() => {
                self.rank_hot(&conn, &plan, kinds.as_ref(), allowed.as_ref(), &mut groups)?;
                self.rank_cold(
                    &conn,
                    &dir,
                    &plan,
                    kinds.as_ref(),
                    allowed.as_ref(),
                    now,
                    p.offset + limit,
                    &mut groups,
                )?
            }
            _ => {
                let need = p.offset + limit;
                self.scan_recent(&conn, &dir, &plan, kinds.as_ref(), allowed.as_ref(), need, &mut groups)?
            }
        };
        let mut ranked: Vec<(Session, f64, Vec<Cand>)> = Vec::with_capacity(groups.len());
        for (sid, mut hits) in groups {
            // Rows of a session the engine just dropped linger until the writer catches up.
            let Some(meta) = self.session_meta(sid) else { continue };
            hits.sort_by(Cand::best_first);
            hits.truncate(HITS_PER_SESSION);
            let score = match order {
                SearchOrder::Relevance => hits[0].key * recency(now, meta.updated_at),
                SearchOrder::Recent => hits[0].key,
            };
            ranked.push((meta, score, hits));
        }
        // A total order: equal scores must not page differently from one call to the next.
        ranked
            .sort_by(|a, b| b.1.total_cmp(&a.1).then(b.0.updated_at.cmp(&a.0.updated_at)).then(a.0.key.cmp(&b.0.key)));
        let total = end.total.unwrap_or(ranked.len());

        let mut results = Vec::new();
        for (meta, score, hits) in ranked.into_iter().skip(p.offset).take(limit) {
            let mut out = Vec::new();
            for c in &hits {
                let (event, ts, snippet) = self.hit(&conn, &dir, &plan, c)?;
                out.push(SearchHit { event, kind: store::kind_name(store::kind_of(c.id)).to_owned(), ts, snippet });
            }
            results.push(SearchSession {
                session: meta.key,
                harness: meta.harness,
                title: meta.title.or(meta.preview),
                cwd: meta.cwd,
                updated_at: Some(meta.updated_at),
                score: if order == SearchOrder::Relevance { score } else { 0.0 },
                hits: out,
            });
        }
        Ok(SearchResponse {
            q: p.q.clone(),
            filter: filter.map(str::to_owned),
            order,
            total,
            offset: p.offset,
            limit,
            indexing,
            progress,
            results,
            partial: end.partial,
            scanned_until: end.scanned_until,
        })
    }

    /// Hot-tier relevance scoring: `uniflo_bm25n` with the global avgdl and per-phrase IDF
    /// estimated from the hot tier, so hot and cold scores are directly comparable. One phrase or
    /// several are the same code path (the old single-phrase fast path's only advantage — skipping
    /// the built-in's `xQueryPhrase` IDF pass — comes for free here too, since IDF is supplied).
    fn rank_hot(
        &self,
        conn: &Connection,
        plan: &query::Plan,
        kinds: Option<&HashSet<i64>>,
        allowed: Option<&HashSet<i64>>,
        groups: &mut HashMap<i64, Vec<Cand>>,
    ) -> Result<()> {
        let idfs = self.hot_idfs(conn, plan)?;
        let avgdl = store::counters(conn)?.avgdl();
        let idf_json = rank::idf_json(&idfs);
        let expr = plan.fts.as_deref().unwrap_or_default();
        let sql = "SELECT rowid, uniflo_bm25n(docs_fts, ?2, ?3) FROM docs_fts WHERE docs_fts MATCH ?1";
        let mut st = conn.prepare_cached(sql)?;
        let mut rows = st.query(rusqlite::params![expr, avgdl, idf_json])?;
        while let Some(r) = rows.next()? {
            let id: i64 = r.get(0)?;
            if kinds.is_some_and(|k| !k.contains(&store::kind_of(id)))
                || allowed.is_some_and(|a| !a.contains(&store::sid_of(id)))
            {
                continue;
            }
            push_hit(groups, Cand { id, key: r.get(1)? });
        }
        Ok(())
    }

    /// Per-phrase IDF, estimated from the hot tier alone (`1.0` if it is empty): shared by the hot
    /// scoring above, the cold tier's Rust-side scoring, and the cold upper-bound stop test.
    fn hot_idfs(&self, conn: &Connection, plan: &query::Plan) -> Result<Vec<f64>> {
        let hot_rows: i64 = conn.query_row("SELECT count(*) FROM docs", [], |r| r.get(0))?;
        let mut idfs = Vec::with_capacity(plan.pos_long.len());
        for term in &plan.pos_long {
            let expr = fts_quote(term);
            let hits: i64 =
                conn.query_row("SELECT count(*) FROM docs_fts WHERE docs_fts MATCH ?1", [&expr], |r| r.get(0))?;
            idfs.push(rank::idf(hot_rows, hits));
        }
        Ok(idfs)
    }

    /// Cold-tier relevance scoring: candidate sessions newest first, verified in parallel batches,
    /// stopping once no older session's score upper bound (`Σidf · (k1+1) · recency`) can beat the
    /// current `need`-th best score, then spending `total_budget` confirming at least one hit in
    /// the rest so `total` is exact when the budget allows it. Only candidate blocks are
    /// decompressed: a matching row lies within one block, which then holds every trigram.
    #[allow(clippy::too_many_arguments)]
    fn rank_cold(
        &self,
        conn: &Connection,
        dir: &cache::BlockDir,
        plan: &query::Plan,
        kinds: Option<&HashSet<i64>>,
        allowed: Option<&HashSet<i64>>,
        now: i64,
        need: usize,
        groups: &mut HashMap<i64, Vec<Cand>>,
    ) -> Result<ScanEnd> {
        let blocks = self.cold_blocks(conn, dir, plan)?;
        let candidates = self.cold_candidates(dir, blocks.as_ref(), allowed);
        if candidates.is_empty() {
            return Ok(ScanEnd { total: Some(groups.len()), ..Default::default() });
        }
        let idfs = self.hot_idfs(conn, plan)?;
        let avgdl = store::counters(conn)?.avgdl();
        let crit = verify::Criteria::from_plan(plan);
        let cache_key = cold_cache_key(plan, kinds);
        let cached = self.inner.cold_cache.get(&cache_key, dir.generation).unwrap_or_default();
        let mut fresh: HashMap<i64, cache::SessionVerify> = HashMap::new();
        let idf_sum: f64 = idfs.iter().sum();
        let mut sofar: BinaryHeap<Reverse<OrdF64>> = BinaryHeap::new();
        let push_score = |sofar: &mut BinaryHeap<Reverse<OrdF64>>, v: f64| {
            sofar.push(Reverse(OrdF64(v)));
            if sofar.len() > need {
                sofar.pop();
            }
        };
        // Seed the page threshold with the hot tier's results already in `groups`.
        for hits in groups.values() {
            if let Some(best) =
                hits.iter().map(|c| c.key).fold(None, |m: Option<f64>, k| Some(m.map_or(k, |m| m.max(k))))
            {
                push_score(&mut sofar, best);
            }
        }
        let take = |v: &cache::SessionVerify,
                    upd: i64,
                    groups: &mut HashMap<i64, Vec<Cand>>,
                    sofar: &mut BinaryHeap<Reverse<OrdF64>>| {
            let Some(facts) = v.facts.as_deref() else { return };
            let hits = score_session(facts, &idfs, avgdl);
            if let Some(&(_, best)) = hits.first() {
                push_score(sofar, best * recency(now, upd));
            }
            for (id, key) in hits {
                push_hit(groups, Cand { id, key });
            }
        };
        // One session per thread per batch: a batch may verify up to `threads - 1` sessions past the
        // stop point, harmless (they rank below the page) but wasted work.
        let batch_size = self.verify_threads();
        let mut idx = 0usize;
        let mut settled = false;
        while !settled && idx < candidates.len() {
            let mut batch = Vec::new();
            while idx < candidates.len() && batch.len() < batch_size {
                let (sid, upd) = candidates[idx];
                if sofar.len() >= need && idf_sum * K1_PLUS_1 * recency(now, upd) < kth(&sofar) {
                    settled = true;
                    break;
                }
                idx += 1;
                match cached.get(&sid) {
                    // Paging or repeating this query: rescore what an earlier run verified, as long
                    // as nothing cold has changed since (`cold_gen`).
                    Some(v) if v.facts.is_some() || !v.matched => take(v, upd, groups, &mut sofar),
                    _ => batch.push((sid, upd)),
                }
            }
            if batch.is_empty() {
                continue;
            }
            let jobs = self.jobs(conn, dir, blocks.as_ref(), &batch)?;
            for (&(sid, upd), v) in batch.iter().zip(self.verify_all(jobs, &crit, kinds, Check::Rows)?) {
                take(&v, upd, groups, &mut sofar);
                fresh.insert(sid, v);
            }
        }
        // The page is settled; spend the remaining budget confirming at least one hit in the rest
        // so `total` is exact rather than a lower bound. These sessions are guaranteed to rank
        // below the page (the stop test above already proved it), so they are counted but not
        // added to `groups` / scored.
        let t0 = Instant::now();
        let mut scanned_until = None;
        let mut rest_matched = 0usize;
        while idx < candidates.len() {
            if t0.elapsed() >= self.inner.opts.total_budget {
                scanned_until = Some(candidates[idx].1);
                break;
            }
            let mut batch = Vec::new();
            while idx < candidates.len() && batch.len() < batch_size * 4 {
                let (sid, upd) = candidates[idx];
                idx += 1;
                match cached.get(&sid) {
                    Some(v) => rest_matched += usize::from(v.matched),
                    None => batch.push((sid, upd)),
                }
            }
            if batch.is_empty() {
                continue;
            }
            let jobs = self.jobs(conn, dir, blocks.as_ref(), &batch)?;
            for (&(sid, _), v) in batch.iter().zip(self.verify_all(jobs, &crit, kinds, Check::Exists)?) {
                rest_matched += usize::from(v.matched);
                fresh.insert(sid, v);
            }
        }
        if !fresh.is_empty() {
            for v in fresh.values_mut() {
                if v.facts.as_ref().is_some_and(|f| f.len() > cache::MAX_SESSION_ROWS) {
                    v.facts = None;
                }
            }
            self.inner.cold_cache.merge(&cache_key, dir.generation, fresh);
        }
        Ok(ScanEnd { partial: false, scanned_until, total: Some(groups.len() + rest_matched) })
    }

    /// The cold block directory as of `conn`'s snapshot, cached until `cold_gen` moves.
    fn block_dir(&self, conn: &Connection) -> Result<Arc<cache::BlockDir>> {
        let generation = store::counters(conn)?.cold_gen;
        if let Some(d) = self.inner.dir.lock().unwrap().as_ref().filter(|d| d.generation == generation) {
            return Ok(d.clone());
        }
        let mut dir = cache::BlockDir { generation, ..Default::default() };
        let mut st = conn.prepare_cached("SELECT bid, sid, first, last FROM blk ORDER BY sid, first")?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let (bid, sid): (i64, i64) = (r.get(0)?, r.get(1)?);
            dir.blocks.insert(bid, (sid, r.get(2)?, r.get(3)?));
            dir.by_sid.entry(sid).or_default().push(bid);
        }
        let dir = Arc::new(dir);
        *self.inner.dir.lock().unwrap() = Some(dir.clone());
        Ok(dir)
    }

    /// Each cold session's candidate blocks (block-level trigram AND over the folded positive long
    /// terms), in block order; `None` when no term narrows them, so every block is a candidate.
    fn cold_blocks(
        &self,
        conn: &Connection,
        dir: &cache::BlockDir,
        plan: &query::Plan,
    ) -> Result<Option<HashMap<i64, Vec<i64>>>> {
        let mut bids: Option<HashSet<i64>> = None;
        let mut st = conn.prepare_cached("SELECT rowid FROM blk_fts WHERE blk_fts MATCH ?1")?;
        for term in &plan.pos_long {
            let Some(expr) = verify::block_candidate_expr(&verify::fold(term)) else { continue };
            let mut rows = st.query([&expr])?;
            let mut hit = HashSet::new();
            while let Some(r) = rows.next()? {
                let bid: i64 = r.get(0)?;
                if bids.as_ref().is_none_or(|b| b.contains(&bid)) {
                    hit.insert(bid);
                }
            }
            bids = Some(hit);
        }
        let Some(bids) = bids else { return Ok(None) };
        let mut by: HashMap<i64, Vec<i64>> = HashMap::new();
        for bid in bids {
            if let Some(&(sid, ..)) = dir.blocks.get(&bid) {
                by.entry(sid).or_default().push(bid);
            }
        }
        for v in by.values_mut() {
            v.sort_by_key(|b| dir.blocks[b].1);
        }
        Ok(Some(by))
    }

    /// Cold sessions with candidate blocks, restricted by `allowed`, newest first by the
    /// `updated_at` the final ranking uses.
    fn cold_candidates(
        &self,
        dir: &cache::BlockDir,
        blocks: Option<&HashMap<i64, Vec<i64>>>,
        allowed: Option<&HashSet<i64>>,
    ) -> Vec<(i64, i64)> {
        let has = |sid: &i64| blocks.map_or_else(|| dir.by_sid.contains_key(sid), |b| b.contains_key(sid));
        self.by_recency(allowed).into_iter().filter(|(sid, _)| has(sid)).collect()
    }

    /// The candidate blocks of `batch`'s sessions: decoded already (block cache) or compressed,
    /// read here from the query's snapshot so the verifying threads need no connection.
    fn jobs(
        &self,
        conn: &Connection,
        dir: &cache::BlockDir,
        blocks: Option<&HashMap<i64, Vec<i64>>>,
        batch: &[(i64, i64)],
    ) -> Result<Vec<Job>> {
        let mut st = conn.prepare_cached("SELECT z FROM blk_data WHERE bid = ?1")?;
        let mut out = Vec::with_capacity(batch.len());
        for &(sid, _) in batch {
            let bids =
                blocks.map_or_else(|| dir.by_sid.get(&sid), |b| b.get(&sid)).map(Vec::as_slice).unwrap_or_default();
            let mut job = Job { blocks: Vec::with_capacity(bids.len()) };
            for &bid in bids {
                let block = match self.inner.blocks.get(bid) {
                    Some(rows) => Block::Ready(rows),
                    None => Block::Packed(st.query_row([bid], |r| r.get(0))?),
                };
                job.blocks.push((bid, block));
            }
            out.push(job);
        }
        Ok(out)
    }

    /// Verify `jobs` on up to [`FtsOptions::verify_threads`] threads; results in `jobs` order.
    fn verify_all(
        &self,
        jobs: Vec<Job>,
        crit: &verify::Criteria,
        kinds: Option<&HashSet<i64>>,
        check: Check,
    ) -> Result<Vec<cache::SessionVerify>> {
        self.inner.cold_verified.fetch_add(jobs.len(), Ordering::Relaxed);
        let blocks = &self.inner.blocks;
        let threads = self.verify_threads().min(jobs.len());
        if threads <= 1 {
            return jobs.into_iter().map(|j| verify_job(blocks, j, crit, kinds, check)).collect();
        }
        let slots: Vec<Mutex<Option<Job>>> = jobs.into_iter().map(|j| Mutex::new(Some(j))).collect();
        let done: Vec<Mutex<Option<Result<cache::SessionVerify>>>> = slots.iter().map(|_| Mutex::new(None)).collect();
        let next = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(slot) = slots.get(i) else { break };
                        let job = slot.lock().unwrap().take().expect("each job is taken once");
                        *done[i].lock().unwrap() = Some(verify_job(blocks, job, crit, kinds, check));
                    }
                });
            }
        });
        done.into_iter().map(|d| d.into_inner().unwrap().expect("every job ran")).collect()
    }

    fn verify_threads(&self) -> usize {
        match self.inner.opts.verify_threads {
            0 => std::thread::available_parallelism().map_or(1, |n| n.get()).min(8),
            n => n,
        }
    }

    /// A cold block's rows, through the block cache.
    fn block_rows(&self, conn: &Connection, bid: i64) -> Result<Arc<Vec<store::Row>>> {
        self.inner.blocks.get_or_insert(bid, || {
            let z: Vec<u8> =
                conn.prepare_cached("SELECT z FROM blk_data WHERE bid = ?1")?.query_row([bid], |r| r.get(0))?;
            cold::decode_compressed(&z)
        })
    }

    /// A short term (no trigram narrows it): `LIKE` over each session's rows, the most recently
    /// active session first; with longer terms too, only over the rows the FTS index matched. A
    /// session's `updated_at` bounds its events' times, so the scan stops once `need` sessions are
    /// found and the next session cannot hold a newer hit than the `need`-th, or when the time
    /// budget runs out. Cold sessions are decompressed and matched in Rust with the same criteria.
    #[allow(clippy::too_many_arguments)]
    fn scan_recent(
        &self,
        conn: &Connection,
        dir: &cache::BlockDir,
        plan: &query::Plan,
        kinds: Option<&HashSet<i64>>,
        allowed: Option<&HashSet<i64>>,
        need: usize,
        groups: &mut HashMap<i64, Vec<Cand>>,
    ) -> Result<ScanEnd> {
        let t0 = Instant::now();
        let budget = self.inner.opts.like_budget;
        let crit = verify::Criteria::from_plan(plan);
        let (cond, pats) = like_clauses(plan);
        // Row ids only (no bm25, no text): cheap even for common long terms.
        let fts_rows: Option<HashMap<i64, Vec<i64>>> = match &plan.fts {
            Some(expr) => {
                let mut by: HashMap<i64, Vec<i64>> = HashMap::new();
                let mut st = conn.prepare_cached("SELECT rowid FROM docs_fts WHERE docs_fts MATCH ?1")?;
                let mut rows = st.query([expr])?;
                while let Some(r) = rows.next()? {
                    let id: i64 = r.get(0)?;
                    if kinds.is_none_or(|k| k.contains(&store::kind_of(id))) {
                        by.entry(store::sid_of(id)).or_default().push(id);
                    }
                }
                Some(by)
            }
            None => None,
        };
        let mut one = conn.prepare_cached(&format!("SELECT d.id, d.ts FROM docs d WHERE d.id = ?{cond}"))?;
        let mut span =
            conn.prepare_cached(&format!("SELECT d.id, d.ts FROM docs d WHERE d.id >= ? AND d.id < ?{cond}"))?;
        let mut last = conn.prepare_cached("SELECT max(id) FROM docs WHERE id >= ?1 AND id < ?2")?;
        // Newest hit of each of the best `need` sessions so far; the smallest is the page's cut-off.
        let mut best: BinaryHeap<Reverse<i64>> = BinaryHeap::new();
        // Cold candidate blocks, looked up when the scan first reaches a cold session.
        let mut narrowed: Option<Option<HashMap<i64, Vec<i64>>>> = None;
        for (sid, upd) in self.by_recency(allowed) {
            // Strictly newer: a session tied with the cut-off could still win on the key tie-break.
            if best.len() >= need && best.peek().is_some_and(|Reverse(ts)| *ts > upd) {
                return Ok(ScanEnd { partial: false, scanned_until: Some(upd), total: None });
            }
            if t0.elapsed() >= budget {
                return Ok(ScanEnd { partial: true, scanned_until: Some(upd), total: None });
            }
            let newest = if let Some(all) = dir.by_sid.get(&sid) {
                let narrowed = match &mut narrowed {
                    Some(n) => n,
                    slot => slot.insert(self.cold_blocks(conn, dir, plan)?),
                };
                let bids = match narrowed {
                    Some(by) => by.get(&sid).map(Vec::as_slice).unwrap_or_default(),
                    None => all.as_slice(),
                };
                self.scan_cold_session(conn, bids, &crit, kinds, groups)?
            } else {
                self.scan_hot_session(sid, &fts_rows, &mut one, &mut span, &mut last, &pats, kinds, groups)?
            };
            if let Some(ts) = newest {
                best.push(Reverse(ts));
                if best.len() > need {
                    best.pop();
                }
            }
        }
        Ok(ScanEnd::default())
    }

    /// A cold session's part of [`Fts::scan_recent`], over its candidate blocks `bids`.
    fn scan_cold_session(
        &self,
        conn: &Connection,
        bids: &[i64],
        crit: &verify::Criteria,
        kinds: Option<&HashSet<i64>>,
        groups: &mut HashMap<i64, Vec<Cand>>,
    ) -> Result<Option<i64>> {
        if bids.is_empty() {
            return Ok(None);
        }
        self.inner.cold_verified.fetch_add(1, Ordering::Relaxed);
        let mut newest = None;
        for &bid in bids {
            let rows = self.block_rows(conn, bid)?;
            for r in rows.iter() {
                if kinds.is_some_and(|k| !k.contains(&store::kind_of(r.id))) || !verify::row_matches(&r.text, crit) {
                    continue;
                }
                newest = newest.max(Some(r.ts));
                push_hit(groups, Cand { id: r.id, key: r.ts as f64 });
            }
        }
        Ok(newest)
    }

    #[allow(clippy::too_many_arguments)]
    fn scan_hot_session(
        &self,
        sid: i64,
        fts_rows: &Option<HashMap<i64, Vec<i64>>>,
        one: &mut rusqlite::CachedStatement<'_>,
        span: &mut rusqlite::CachedStatement<'_>,
        last: &mut rusqlite::CachedStatement<'_>,
        pats: &[Sql],
        kinds: Option<&HashSet<i64>>,
        groups: &mut HashMap<i64, Vec<Cand>>,
    ) -> Result<Option<i64>> {
        let steps: Vec<(i64, i64)> = match fts_rows {
            Some(by) => match by.get(&sid) {
                Some(ids) => ids.iter().map(|&id| (id, id + 1)).collect(),
                None => return Ok(None),
            },
            None => {
                let ids = store::id_range(sid);
                let Some(max) = last.query_row([ids.start, ids.end], |r| r.get::<_, Option<i64>>(0))? else {
                    return Ok(None);
                };
                let chunk = store::id_span(LIKE_CHUNK);
                (ids.start..=max).step_by(chunk as usize).map(|lo| (lo, (lo + chunk).min(max + 1))).collect()
            }
        };
        let mut newest: Option<i64> = None;
        for (lo, hi) in steps {
            let mut rows = if hi == lo + 1 {
                one.query(params_from_iter(std::iter::once(Sql::Integer(lo)).chain(pats.iter().cloned())))?
            } else {
                span.query(params_from_iter(
                    [Sql::Integer(lo), Sql::Integer(hi)].into_iter().chain(pats.iter().cloned()),
                ))?
            };
            while let Some(r) = rows.next()? {
                let id: i64 = r.get(0)?;
                if kinds.is_some_and(|k| !k.contains(&store::kind_of(id))) {
                    continue;
                }
                let ts: i64 = r.get(1)?;
                newest = newest.max(Some(ts));
                push_hit(groups, Cand { id, key: ts as f64 });
            }
        }
        Ok(newest)
    }

    /// Indexed sessions as `(sid, updated_at)`, most recently active first.
    fn by_recency(&self, allowed: Option<&HashSet<i64>>) -> Vec<(i64, i64)> {
        let live: HashMap<String, i64> =
            self.inner.engine.sessions().into_iter().map(|s| (s.key, s.updated_at)).collect();
        let sh = self.inner.shared.lock().unwrap();
        let mut out: Vec<(i64, i64)> = sh
            .sessions
            .iter()
            .filter(|(_, r)| allowed.is_none_or(|a| a.contains(&r.sid)))
            .filter_map(|(k, r)| {
                let pinned = r.meta.as_ref().filter(|_| r.pinned).map(|m| m.updated_at);
                Some((r.sid, live.get(k).copied().or(pinned)?))
            })
            .collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.cmp(&a.0)));
        out
    }

    /// Session ids passing a `uniflo ls`-syntax filter, through the same matcher as `/v1/sessions`.
    fn allowed(&self, filter: &str, now: i64) -> HashSet<i64> {
        let mut list = self.inner.engine.sessions();
        let sh = self.inner.shared.lock().unwrap();
        let known: HashSet<String> = list.iter().map(|s| s.key.clone()).collect();
        list.extend(
            sh.sessions
                .values()
                .filter(|r| r.pinned)
                .filter_map(|r| r.meta.clone())
                .filter(|m| !known.contains(&m.key)),
        );
        let q = SessionQuery::parse(filter, now);
        filter_sessions(&list, &q, usize::MAX)
            .into_iter()
            .filter_map(|h| sh.sessions.get(&h.session.key).map(|r| r.sid))
            .collect()
    }

    /// Current metadata: the engine's snapshot, else what a pinned session was indexed with.
    fn session_meta(&self, sid: i64) -> Option<Session> {
        let (key, pinned) = {
            let sh = self.inner.shared.lock().unwrap();
            let key = sh.keys.get(&sid)?.clone();
            let pinned = sh.sessions.get(&key).filter(|r| r.pinned).and_then(|r| r.meta.clone());
            (key, pinned)
        };
        self.inner.engine.session(&key).or(pinned)
    }

    /// `(event id, ts, snippet)` of one hit. The snippet is cut from the stored text: FTS5's
    /// `snippet()` would re-run the whole query for every hit. Cold hits are read back from their
    /// block (through the cache) rather than a `docs` row.
    fn hit(
        &self,
        conn: &Connection,
        dir: &cache::BlockDir,
        plan: &query::Plan,
        c: &Cand,
    ) -> Result<(String, i64, String)> {
        if let Some(bids) = dir.by_sid.get(&store::sid_of(c.id)) {
            // The block whose id range covers the row; every block as a fallback.
            let covering = bids.iter().filter(|b| {
                let (_, first, last) = dir.blocks[*b];
                (first..=last).contains(&c.id)
            });
            for &bid in covering.chain(bids) {
                let rows = self.block_rows(conn, bid)?;
                if let Some(row) = rows.iter().find(|r| r.id == c.id) {
                    return Ok((row.event.clone(), row.ts, verify::fold_snippet(&row.text, &plan.marks)));
                }
            }
            anyhow::bail!("cold row vanished");
        }
        let (event, ts, text): (String, i64, String) = conn
            .prepare_cached("SELECT event, ts, text FROM docs WHERE id = ?1")?
            .query_row([c.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        if let Some(s) = query::snippet(&text, &plan.marks) {
            return Ok((event, ts, s));
        }
        let snippet = match &plan.fts {
            Some(expr) => {
                let sql = format!(
                    "SELECT snippet(docs_fts, 0, char(2), char(3), '…', {SNIPPET_TOKENS}) \
                     FROM docs_fts WHERE docs_fts MATCH ?1 AND rowid = ?2"
                );
                let s: String = conn.prepare_cached(&sql)?.query_row(rusqlite::params![expr, c.id], |r| r.get(0))?;
                query::clean(&s)
            }
            None => query::like_snippet(&text, &plan.marks),
        };
        Ok((event, ts, snippet))
    }

    fn reader(&self) -> Result<Pooled<'_>> {
        let conn = self.inner.readers.lock().unwrap().pop();
        let conn = match conn {
            Some(c) => c,
            None => store::open_reader(&self.inner.opts.path)?,
        };
        Ok(Pooled { conn: Some(conn), pool: &self.inner.readers })
    }

    /// Cold blocks decompressed and matched since start (tests: early termination).
    #[doc(hidden)]
    pub fn cold_verified_count(&self) -> usize {
        self.inner.cold_verified.load(Ordering::Relaxed)
    }
}

struct Pooled<'a> {
    conn: Option<Connection>,
    pool: &'a Mutex<Vec<Connection>>,
}

impl std::ops::Deref for Pooled<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn.as_ref().expect("pooled connection")
    }
}

impl Drop for Pooled<'_> {
    fn drop(&mut self) {
        if let Some(c) = self.conn.take() {
            self.pool.lock().unwrap().push(c);
        }
    }
}

/// Once the backlog has drained and the segments are merged, read the two tables every query
/// touches into the OS page cache: the term index, and the document lengths bm25 looks up for
/// each matched row (`docs_fts_docsize`, under 1% of the index). Cold, those lookups cost a
/// term's first query up to a second; the postings are read per term, sequentially once merged.
/// Also pre-reads the cold tier's block directory and trigram index. False when stopped first.
fn warm(inner: &Inner) -> Result<bool> {
    while {
        let sh = inner.shared.lock().unwrap();
        sh.indexing || sh.merging || sh.migrating
    } {
        if inner.stop.load(Ordering::Relaxed) {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let conn = store::open_reader(&inner.opts.path)?;
    // count(*) visits every page of the b-tree.
    for table in ["docs_fts_idx", "docs_fts_docsize", "blk", "blk_fts_idx"] {
        let _ = conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |_| Ok(()));
    }
    Ok(true)
}

/// Ranking weight `1 + 1/(1 + age_days/30)`: ×2 today, ×1.5 a month ago, →1 for old sessions.
fn recency(now: i64, t: i64) -> f64 {
    1.0 + 1.0 / (1.0 + (now - t).max(0) as f64 / DAY_MS / 30.0)
}

/// `f64` ordered by `total_cmp`, so it can live in a `BinaryHeap` (scores are never NaN, but
/// `f64` has no blanket `Ord` regardless).
#[derive(Debug, Clone, Copy, PartialEq)]
struct OrdF64(f64);

impl Eq for OrdF64 {}

impl PartialOrd for OrdF64 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrdF64 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}

fn fts_quote(t: &str) -> String {
    format!("\"{}\"", t.replace('"', "\"\""))
}

/// Identifies a relevance query's cold-tier verification for [`cache::ColdCache`]: the terms that
/// decide matching and scoring, and the kinds filter. Not the session filter: that only narrows
/// which sessions are looked at, not what verifying one of them finds.
fn cold_cache_key(plan: &query::Plan, kinds: Option<&HashSet<i64>>) -> String {
    let kinds = kinds.map(|k| {
        let mut v: Vec<i64> = k.iter().copied().collect();
        v.sort_unstable();
        v
    });
    format!(
        "{:?}\u{0}{:?}\u{0}{:?}\u{0}{:?}\u{0}{kinds:?}",
        plan.pos_long, plan.neg_long, plan.like_raw, plan.not_like_raw
    )
}

/// The `need`-th best score so far (the heap keeps exactly the best `need`).
fn kth(sofar: &BinaryHeap<Reverse<OrdF64>>) -> f64 {
    sofar.peek().map(|Reverse(OrdF64(v))| *v).unwrap_or(f64::MIN)
}

/// One cold session's candidate blocks, for [`verify_job`].
struct Job {
    blocks: Vec<(i64, Block)>,
}

enum Block {
    Ready(Arc<Vec<store::Row>>),
    Packed(Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Check {
    /// Every matching row's scoring inputs.
    Rows,
    /// Only whether some row matches (the exact-`total` pass).
    Exists,
}

/// Decode a session's candidate blocks (through the block cache) and verify their rows of the
/// requested kinds.
fn verify_job(
    blocks: &cache::BlockCache,
    job: Job,
    crit: &verify::Criteria,
    kinds: Option<&HashSet<i64>>,
    check: Check,
) -> Result<cache::SessionVerify> {
    let mut facts = cache::Facts::default();
    for (bid, block) in job.blocks {
        let rows = match block {
            Block::Ready(rows) => rows,
            Block::Packed(z) => {
                let (bytes, rows) = cold::decode_compressed(&z)?;
                blocks.insert(bid, bytes, rows)
            }
        };
        for r in rows.iter().filter(|r| kinds.is_none_or(|k| k.contains(&store::kind_of(r.id)))) {
            match check {
                Check::Exists => {
                    if verify::row_matches(&r.text, crit) {
                        return Ok(cache::SessionVerify { matched: true, facts: None });
                    }
                }
                Check::Rows => {
                    if let Some(f) = verify::row_frequencies(&r.text, crit) {
                        facts.ids.push(r.id);
                        facts.tokens.push(store::token_count(&r.text).max(0) as u32);
                        facts.freqs.extend(f);
                    }
                }
            }
        }
    }
    Ok(match check {
        Check::Exists => cache::SessionVerify { matched: false, facts: None },
        Check::Rows => cache::SessionVerify { matched: !facts.is_empty(), facts: Some(Arc::new(facts)) },
    })
}

/// A cold session's best rows by bm25 under the current statistics, best first (ties as
/// [`Cand::best_first`]).
fn score_session(facts: &cache::Facts, idfs: &[f64], avgdl: f64) -> Vec<(i64, f64)> {
    let phrases = idfs.len();
    let mut scored: Vec<(i64, f64)> = (0..facts.len())
        .map(|i| (facts.ids[i], rank::bm25_counts(facts.row(i, phrases), idfs, facts.tokens[i] as f64, avgdl)))
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(b.0.cmp(&a.0)));
    scored.truncate(HITS_PER_SESSION);
    scored
}

/// Where a scan stopped: a short-term scan's [`SearchResponse::scanned_until`] /`partial`, or a
/// relevance scan's exact-or-lower-bound `total` (see [`Fts::rank_cold`]).
#[derive(Debug, Default)]
struct ScanEnd {
    partial: bool,
    scanned_until: Option<i64>,
    total: Option<usize>,
}

/// `AND d.text [NOT] LIKE ?` for the plan's short terms, and their patterns.
fn like_clauses(plan: &query::Plan) -> (String, Vec<Sql>) {
    let mut cond = String::new();
    let mut args = Vec::new();
    for (pats, op) in [(&plan.like, "LIKE"), (&plan.not_like, "NOT LIKE")] {
        for pat in pats {
            cond.push_str(&format!(" AND d.text {op} ? ESCAPE '\\'"));
            args.push(Sql::Text(pat.clone()));
        }
    }
    (cond, args)
}

/// Keep a session's candidates bounded while scanning; the best ones are picked at the end.
fn push_hit(groups: &mut HashMap<i64, Vec<Cand>>, c: Cand) {
    let hits = groups.entry(store::sid_of(c.id)).or_default();
    hits.push(c);
    if hits.len() > HITS_PER_SESSION * 4 {
        hits.sort_by(Cand::best_first);
        hits.truncate(HITS_PER_SESSION);
    }
}

#[derive(Debug, Clone, Copy)]
struct Cand {
    id: i64,
    /// Higher is better: a weighted bm25 score, or the event time when ordering by recency.
    key: f64,
}

impl Cand {
    fn best_first(a: &Cand, b: &Cand) -> std::cmp::Ordering {
        b.key.total_cmp(&a.key).then(b.id.cmp(&a.id))
    }
}

/// How one backfill pass writes a session's rows.
enum PassMode {
    /// Incremental, per page, straight into the hot tier (today's ADR-0008 behaviour).
    Hot,
    /// The session is (or will stay) older than `hot_days`: collect every row in memory across
    /// pages, then write it as cold blocks once, comparing against the existing fingerprint so an
    /// unchanged session is not rewritten.
    Cold { collected: Vec<Event> },
}

/// One backfill pass over a session: history pages from the newest down to `stop`.
struct Pass {
    session: Session,
    mode: PassMode,
    /// `done_pos` of the previous complete pass: a catch-up ends once a page reaches it.
    stop: Option<u64>,
    before: Option<u64>,
    /// Event ids already written by this pass. Pages go newest first, so the first sighting of
    /// an id is its latest version; older duplicates (earlier streaming states) are skipped.
    seen: HashSet<String>,
    max_pos: Option<u64>,
}

struct Writer {
    inner: Arc<Inner>,
    conn: Connection,
    lease: Option<Connection>,
    rx: broadcast::Receiver<Arc<Envelope>>,
    queue: VecDeque<Session>,
    pass: Option<Pass>,
    next_reconcile: Instant,
    reader_version: i64,
    /// Reconcile at the next turn: the broadcast lagged or a write was rolled back.
    lagged: Cell<bool>,
    /// Start of the open write transaction.
    tx_since: Cell<Option<Instant>>,
    build: Option<Instant>,
    /// Look at the segments at the next idle turn: set at start and whenever a backlog drains.
    merge_check: bool,
    /// The running segment merge: its start, and whether its first step ran.
    merge: Option<(Instant, bool)>,
    /// Look for a hot session to compact at the next idle turn.
    compact_check: bool,
    /// Run `incremental_vacuum` at the next idle turn.
    vacuum_check: bool,
}

impl Writer {
    fn run(mut self) {
        if self.lease.is_some() && self.inner.shared.lock().unwrap().migrating {
            self.migrate();
        }
        match store::count_docs(&self.conn) {
            Ok(n) => self.inner.shared.lock().unwrap().progress.events = n,
            Err(e) => self.inner.note_error(format!("count: {e:#}")),
        }
        while !self.inner.stop.load(Ordering::Relaxed) {
            if self.lease.is_none() {
                let needs_writer = !self.inner.park_when_idle.load(Ordering::Acquire)
                    || self.inner.pending_commands.load(Ordering::Acquire) != 0
                    || self.inner.shared.lock().unwrap().indexing;
                if needs_writer && Instant::now() >= self.next_reconcile {
                    self.next_reconcile = Instant::now() + Duration::from_secs(1);
                    if let Err(e) = self.claim_writer() {
                        self.inner.note_error(format!("writer lease: {e:#}"));
                    }
                }
                if self.lease.is_none() {
                    if let Ok(version) = self.conn.pragma_query_value(None, "data_version", |r| r.get::<_, i64>(0))
                        && version != self.reader_version
                    {
                        self.reload();
                        self.reader_version = version;
                    }
                    self.nap(IDLE);
                    continue;
                }
            }
            let busy = self.follow() | self.commands();
            if self.lagged.get() {
                self.pass = None;
            }
            if self.lagged.get() || Instant::now() >= self.next_reconcile {
                self.reconcile();
            }
            if self.pass.is_none() {
                self.pass = self.next_pass();
            }
            match self.pass.take() {
                Some(mut p) => {
                    let t0 = Instant::now();
                    if self.step(&mut p) {
                        let mut sh = self.inner.shared.lock().unwrap();
                        sh.progress.done = (sh.progress.done + 1).min(sh.progress.total);
                    } else {
                        self.pass = Some(p);
                    }
                    self.commit(false);
                    self.nap(t0.elapsed().mul_f32(self.inner.opts.pause));
                }
                None => {
                    self.commit(true);
                    self.settle();
                    if self.inner.park_when_idle.load(Ordering::Acquire)
                        && self.inner.pending_commands.load(Ordering::Acquire) == 0
                        && self.tx_since.get().is_none()
                    {
                        match store::open_reader(&self.inner.opts.path) {
                            Ok(reader) => {
                                self.conn = reader;
                                self.lease = None;
                                self.merge = None;
                                self.inner.shared.lock().unwrap().merging = false;
                                continue;
                            }
                            Err(e) => self.inner.note_error(format!("release writer: {e:#}")),
                        }
                    }
                    let did_idle_work = self.merge_step() || self.compact_step() || self.vacuum_step();
                    if !did_idle_work && !busy {
                        self.nap(IDLE);
                    }
                }
            }
        }
        self.commit(true);
    }

    fn claim_writer(&mut self) -> Result<bool> {
        let Some(lease) = store::try_writer(&self.inner.opts.path)? else { return Ok(false) };
        let tag = format!("uniflo-fts/format{}/schema{}", self.inner.opts.format, SCHEMA_VERSION);
        let (conn, _, migrating) = open_writer(&self.inner.opts.path, &tag)?;
        self.conn = conn;
        self.lease = Some(lease);
        self.inner.shared.lock().unwrap().migrating = migrating;
        self.inner.shared.lock().unwrap().merging = true;
        self.reload();
        if migrating {
            self.migrate();
        } else {
            self.reconcile();
            self.commit(true);
        }
        Ok(true)
    }

    /// Convert a leftover `fts-v1.sqlite` ([`migrate`]); queries see each converted session at
    /// once. Stopped part-way, the old file stays for the next start to resume. Once complete,
    /// every session is refreshed from its source, since an older Uniflo wrote those rows; an
    /// unchanged session then costs a comparison, not a rewrite.
    fn migrate(&mut self) {
        let inner = self.inner.clone();
        let t0 = Instant::now();
        let mut on_session = |key: &str, row: SessRow, rows: usize, total: usize| {
            let mut sh = inner.shared.lock().unwrap();
            sh.remember(key, row);
            sh.progress.total = total;
            sh.progress.done = (sh.progress.done + 1).min(total);
            sh.progress.events += rows as u64;
            !inner.stop.load(Ordering::Relaxed)
        };
        match migrate::migrate(&self.conn, &inner.opts.path, inner.opts.hot_days, inner.now(), &mut on_session) {
            Ok(Some(r)) if r.complete => {
                let saved = store::clear_progress_non_pinned(&self.conn)
                    .and_then(|()| store::meta_set(&self.conn, "reclaimed_bytes", &r.reclaimed_bytes.to_string()));
                if let Err(e) = saved {
                    inner.note_error(format!("migrate: {e:#}"));
                }
                inner.shared.lock().unwrap().reclaimed_bytes = r.reclaimed_bytes;
                tracing::info!(
                    "full-text index: migrated {} sessions from fts-v1.sqlite in {} ms, reclaimed {} bytes",
                    r.sessions,
                    t0.elapsed().as_millis(),
                    r.reclaimed_bytes
                );
            }
            Ok(_) => {}
            Err(e) => inner.note_error(format!("migrate: {e:#}")),
        }
        // The progress reset above (or a failed session's rollback) changed `sess`.
        self.reload();
        {
            let mut sh = inner.shared.lock().unwrap();
            sh.migrating = false;
            sh.progress.done = 0;
        }
        if !inner.stop.load(Ordering::Relaxed) {
            self.reconcile();
        }
    }

    /// Reload the in-memory session map and row count from the database.
    fn reload(&self) {
        if let Ok(rows) = store::load_sessions(&self.conn) {
            let expected = self.lease.is_none().then(|| self.inner.engine.sessions());
            let mut sh = self.inner.shared.lock().unwrap();
            sh.sessions.clear();
            sh.keys.clear();
            for (k, r) in rows {
                sh.remember(&k, r);
            }
            if let Some(expected) = expected {
                sh.sync_progress(&expected);
            }
        }
        if let Ok(n) = store::count_docs(&self.conn) {
            self.inner.shared.lock().unwrap().progress.events = n;
        }
    }

    fn nap(&self, d: Duration) {
        let until = Instant::now() + d;
        while !self.inner.stop.load(Ordering::Relaxed) {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            std::thread::sleep(left.min(Duration::from_millis(50)));
        }
    }

    fn settle(&mut self) {
        let mut sh = self.inner.shared.lock().unwrap();
        if !sh.indexing {
            return;
        }
        sh.indexing = false;
        sh.progress.done = sh.progress.total;
        self.merge_check = true;
        self.compact_check = true;
        if let Some(t0) = self.build.take() {
            let ms = t0.elapsed().as_millis() as u64;
            sh.build_ms = Some(ms);
            tracing::info!(
                "full-text index built: {} events from {} sessions in {ms} ms",
                sh.progress.events,
                sh.progress.total
            );
        }
    }

    /// One step of merging the index into one segment, run on idle turns once a backlog leaves it
    /// [`store::scattered`]: each step commits on its own (bounded WAL) and is throttled like the
    /// backfill. Returns whether a step ran.
    fn merge_step(&mut self) -> bool {
        if self.merge.is_none() && std::mem::take(&mut self.merge_check) {
            match store::scattered(&self.conn) {
                Ok(true) => self.merge = Some((Instant::now(), false)),
                Ok(false) => {}
                Err(e) => self.inner.note_error(format!("merge: {e:#}")),
            }
        }
        let Some((since, started)) = self.merge else {
            self.inner.shared.lock().unwrap().merging = false;
            return false;
        };
        let t0 = Instant::now();
        let mut more = false;
        let res = self.transaction(|_, tx| {
            more = store::merge_step(tx, !started, MERGE_PAGES)?;
            Ok(0)
        });
        self.commit(true);
        self.merge = (res.is_ok() && more).then_some((since, true));
        if self.merge.is_none() {
            self.inner.shared.lock().unwrap().merging = false;
            if res.is_ok() {
                tracing::info!("full-text index merged into one segment in {} ms", since.elapsed().as_millis());
            }
        }
        self.nap(t0.elapsed().mul_f32(self.inner.opts.pause));
        true
    }

    /// Move one hot, non-pinned session whose last activity is past `hot_days` into the cold
    /// tier, bounded per commit and throttled like the backfill. A session already queued for this
    /// loop's reconcile pass is skipped (it is mid-flight, not idle). Returns whether work ran.
    fn compact_step(&mut self) -> bool {
        if !std::mem::take(&mut self.compact_check) {
            return false;
        }
        self.compact_check = true; // re-check every idle turn, not just once
        let Some(sid) = self.compaction_candidate() else { return false };
        let t0 = Instant::now();
        let res = self.transaction(|_, tx| {
            let rows = store::drop_hot(tx, sid)?;
            let delta = -(rows.len() as i64);
            cold_ify(tx, sid, rows)?;
            Ok(delta)
        });
        // Two separate locks, deliberately not nested in one condition: holding the guard from
        // the first across the second would self-deadlock (`Mutex` is not reentrant).
        let key = self.inner.shared.lock().unwrap().keys.get(&sid).cloned();
        if let Some(key) = key
            && let Some(r) = self.inner.shared.lock().unwrap().sessions.get_mut(&key)
        {
            r.cold = true;
        }
        self.commit(true);
        self.nap(t0.elapsed().mul_f32(self.inner.opts.pause));
        res.is_ok()
    }

    fn compaction_candidate(&self) -> Option<i64> {
        let cutoff = self.inner.cutoff();
        let live: HashMap<String, i64> =
            self.inner.engine.sessions().into_iter().map(|s| (s.key, s.updated_at)).collect();
        let current = self.pass.as_ref().map(|p| p.session.key.clone());
        let in_queue: HashSet<&str> = self.queue.iter().map(|s| s.key.as_str()).collect();
        let sh = self.inner.shared.lock().unwrap();
        sh.sessions
            .iter()
            .filter(|(k, r)| {
                !r.pinned && !r.cold && current.as_deref() != Some(k.as_str()) && !in_queue.contains(k.as_str())
            })
            .filter_map(|(k, r)| {
                let upd = live.get(k).copied().or(r.done_upd)?;
                (upd < cutoff).then_some(r.sid)
            })
            .next()
    }

    /// Reclaim free pages once they exceed 5% of the file, bounded per idle turn.
    fn vacuum_step(&mut self) -> bool {
        if !std::mem::take(&mut self.vacuum_check) {
            return false;
        }
        self.vacuum_check = true;
        match store::free_ratio(&self.conn) {
            Ok(r) if r > 0.05 => {
                let t0 = Instant::now();
                if let Err(e) = store::incremental_vacuum(&self.conn, VACUUM_PAGES) {
                    self.inner.note_error(format!("incremental_vacuum: {e:#}"));
                    return false;
                }
                self.nap(t0.elapsed().mul_f32(self.inner.opts.pause));
                true
            }
            Ok(_) => false,
            Err(e) => {
                self.inner.note_error(format!("free_ratio: {e:#}"));
                false
            }
        }
    }

    /// Diff the engine's sessions against recorded progress: queue what changed, drop what is gone.
    fn reconcile(&mut self) {
        self.lagged.set(false);
        self.next_reconcile = Instant::now() + self.inner.opts.reconcile;
        let sessions = self.inner.engine.sessions();
        let current = self.pass.as_ref().map(|p| p.session.key.clone());
        let (gone, pinned_only) = {
            let sh = self.inner.shared.lock().unwrap();
            let present: HashSet<&str> = sessions.iter().map(|s| s.key.as_str()).collect();
            let gone: Vec<(String, i64)> = sh
                .sessions
                .iter()
                .filter(|(k, r)| !r.pinned && !present.contains(k.as_str()))
                .map(|(k, r)| (k.clone(), r.sid))
                .collect();
            let pinned_only = sh.sessions.iter().filter(|(k, r)| r.pinned && !present.contains(k.as_str())).count();
            (gone, pinned_only)
        };
        if !gone.is_empty() {
            let _ = self.transaction(|w, tx| {
                let mut removed = 0;
                for (key, sid) in &gone {
                    removed += remove_everywhere(tx, *sid)? as i64;
                    w.inner.shared.lock().unwrap().forget(key);
                }
                Ok(-removed)
            });
        }
        let total = sessions.len() + pinned_only;
        {
            let sh = self.inner.shared.lock().unwrap();
            self.queue = sessions
                .into_iter()
                .filter(|s| current.as_deref() != Some(s.key.as_str()))
                .filter(|s| sh.sessions.get(&s.key).is_none_or(|r| !r.pinned && r.done_upd != Some(s.updated_at)))
                .collect();
        }
        let pending = self.queue.len() + usize::from(self.pass.is_some());
        let mut sh = self.inner.shared.lock().unwrap();
        sh.progress.total = total;
        sh.progress.done = total.saturating_sub(pending);
        sh.indexing = pending > 0;
    }

    fn next_pass(&mut self) -> Option<Pass> {
        while let Some(session) = self.queue.pop_front() {
            let (row, cutoff) = {
                let sh = self.inner.shared.lock().unwrap();
                (sh.sessions.get(&session.key).cloned(), self.inner.cutoff())
            };
            if row.as_ref().is_some_and(|r| r.pinned) {
                continue;
            }
            let old = session.updated_at < cutoff;
            let mode = match &row {
                // Existing cold session: promote if it is active again, else stay cold (and
                // compare against its existing blocks rather than blindly rewriting them).
                Some(r) if r.cold => {
                    if old {
                        PassMode::Cold { collected: Vec::new() }
                    } else {
                        if let Err(e) = self.transaction(|_, tx| {
                            cold::promote(tx, r.sid)?;
                            Ok(0)
                        }) {
                            self.inner.note_error(format!("promote {}: {e:#}", session.key));
                        }
                        if let Some(row) = self.inner.shared.lock().unwrap().sessions.get_mut(&session.key) {
                            row.cold = false;
                        }
                        PassMode::Hot
                    }
                }
                // Existing hot session: keep writing incrementally regardless of age — it only
                // has pending content because it changed, so it is (or was just) active; aging
                // out while idle is compaction's job, not the reconcile pass's.
                Some(_) => PassMode::Hot,
                // Brand new session: if it is already old, go straight to cold so a first backfill
                // never inflates the hot tier (ADR-0011's "全新构建").
                None if old => PassMode::Cold { collected: Vec::new() },
                None => PassMode::Hot,
            };
            let stop = row.filter(|r| r.done_upd.is_some() && !r.cold).and_then(|r| r.done_pos);
            return Some(Pass { session, mode, stop, before: None, seen: HashSet::new(), max_pos: None });
        }
        None
    }

    /// Index one history page of the pass. Returns true when the pass is complete.
    fn step(&mut self, p: &mut Pass) -> bool {
        let key = p.session.key.clone();
        let q = HistoryQuery { before: p.before, limit: self.inner.opts.page.max(1) };
        let page = match self.inner.engine.history(&key, &q) {
            Ok(page) => page,
            Err(err) => {
                if self.inner.engine.session(&key).is_some() {
                    // Still there but unreadable: record it as done so it is retried only once it changes.
                    self.inner.note_error(format!("{key}: {err:#}"));
                    let upd = p.session.updated_at;
                    let _ = self.transaction(|w, tx| {
                        let sid = w.sid(tx, &key)?;
                        store::finish_pass(tx, sid, upd, p.max_pos)?;
                        w.inner
                            .shared
                            .lock()
                            .unwrap()
                            .sessions
                            .entry(key.clone())
                            .and_modify(|r| r.done_upd = Some(upd));
                        Ok(0)
                    });
                }
                return true;
            }
        };
        let first = page.first().and_then(|e| e.pos);
        let done = match first {
            None => true,
            Some(f) => p.before.is_some_and(|b| f >= b) || p.stop.is_some_and(|s| f <= s),
        };
        match &mut p.mode {
            PassMode::Cold { collected } => {
                for e in page.iter().rev() {
                    if let Some(pos) = e.pos {
                        p.max_pos = Some(p.max_pos.map_or(pos, |m| m.max(pos)));
                    }
                    if p.seen.insert(e.id.clone()) {
                        collected.push(e.clone());
                    }
                }
                if done {
                    // Give up on error the same way the hot branch below does (`rollback` already
                    // logged it): there is no partial state worth retrying within this pass.
                    let _ = self.transaction(|w, tx| {
                        let sid = w.sid(tx, &key)?;
                        let delta = finalize_cold_pass(tx, sid, std::mem::take(collected))?;
                        let pos = p.max_pos.max(p.stop);
                        store::finish_pass(tx, sid, p.session.updated_at, pos)?;
                        if let Some(r) = w.inner.shared.lock().unwrap().sessions.get_mut(&key) {
                            r.done_upd = Some(p.session.updated_at);
                            r.done_pos = pos;
                            r.cold = true;
                        }
                        Ok(delta)
                    });
                    return true;
                }
                p.before = first;
                false
            }
            PassMode::Hot => {
                let res = self.transaction(|w, tx| {
                    let sid = w.sid(tx, &key)?;
                    let mut delta = 0;
                    for e in page.iter().rev() {
                        if let Some(pos) = e.pos {
                            p.max_pos = Some(p.max_pos.map_or(pos, |m| m.max(pos)));
                        }
                        if p.seen.insert(e.id.clone()) {
                            delta += store::upsert(tx, sid, e)?;
                        }
                    }
                    if done {
                        let pos = p.max_pos.max(p.stop);
                        store::finish_pass(tx, sid, p.session.updated_at, pos)?;
                        if let Some(r) = w.inner.shared.lock().unwrap().sessions.get_mut(&key) {
                            r.done_upd = Some(p.session.updated_at);
                            r.done_pos = pos;
                        }
                    }
                    Ok(delta)
                });
                match res {
                    Ok(_) if !done => {
                        p.before = first;
                        false
                    }
                    _ => true,
                }
            }
        }
    }

    /// Apply live envelopes from the engine. Returns whether there were any.
    fn follow(&mut self) -> bool {
        let mut batch: Vec<Arc<Envelope>> = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(env) => {
                    if matches!(&*env, Envelope::Event { .. } | Envelope::Removed { .. }) {
                        batch.push(env);
                    }
                }
                Err(TryRecvError::Lagged(_)) => self.lagged.set(true),
                Err(TryRecvError::Empty | TryRecvError::Closed) => break,
            }
        }
        if batch.is_empty() {
            return false;
        }
        let _ = self.transaction(|w, tx| {
            let mut delta = 0;
            for env in &batch {
                match &**env {
                    Envelope::Event { event, .. } => {
                        let sid = w.sid(tx, &event.session)?;
                        w.ensure_hot(tx, sid)?;
                        delta += store::upsert(tx, sid, event)?;
                    }
                    Envelope::Removed { key, .. } => {
                        let sid = w.inner.shared.lock().unwrap().sessions.get(key).filter(|r| !r.pinned).map(|r| r.sid);
                        if let Some(sid) = sid {
                            delta -= remove_everywhere(tx, sid)? as i64;
                            w.inner.shared.lock().unwrap().forget(key);
                        }
                    }
                    _ => {}
                }
            }
            Ok(delta)
        });
        true
    }

    fn commands(&mut self) -> bool {
        let cmds = std::mem::take(&mut *self.inner.cmds.lock().unwrap());
        if cmds.is_empty() {
            return false;
        }
        let cutoff = self.inner.cutoff();
        if !self.commit(true) {
            self.requeue_commands(cmds);
            return true;
        }
        let result = self.transaction(|w, tx| {
            let mut delta = 0;
            for cmd in &cmds {
                match cmd {
                    Cmd::Index(session, events) => {
                        let existing = w.inner.shared.lock().unwrap().sessions.get(&session.key).cloned();
                        let mut seen = HashSet::new();
                        let fresh: Vec<&Event> = events.iter().rev().filter(|e| seen.insert(e.id.as_str())).collect();
                        match existing {
                            // New, already-old pinned session: write straight to cold.
                            None if session.updated_at < cutoff => {
                                let sid = w.sid(tx, &session.key)?;
                                let rows: Vec<Event> = fresh.into_iter().cloned().collect();
                                delta += finalize_cold_pass(tx, sid, rows)?;
                                store::pin(tx, sid, session)?;
                                if let Some(r) = w.inner.shared.lock().unwrap().sessions.get_mut(&session.key) {
                                    r.pinned = true;
                                    r.cold = true;
                                    r.done_upd = Some(session.updated_at);
                                    r.meta = Some((**session).clone());
                                }
                            }
                            other => {
                                let sid = w.sid(tx, &session.key)?;
                                if other.is_some_and(|r| r.cold) {
                                    w.ensure_hot(tx, sid)?;
                                }
                                for e in fresh {
                                    delta += store::upsert(tx, sid, e)?;
                                }
                                store::pin(tx, sid, session)?;
                                if let Some(r) = w.inner.shared.lock().unwrap().sessions.get_mut(&session.key) {
                                    r.pinned = true;
                                    r.cold = false;
                                    r.done_upd = Some(session.updated_at);
                                    r.meta = Some((**session).clone());
                                }
                            }
                        }
                    }
                    Cmd::Remove(key) => {
                        let sid = w.inner.shared.lock().unwrap().sessions.get(key).map(|r| r.sid);
                        if let Some(sid) = sid {
                            delta -= remove_everywhere(tx, sid)? as i64;
                            w.inner.shared.lock().unwrap().forget(key);
                        }
                    }
                }
            }
            Ok(delta)
        });
        if result.is_ok() && self.commit(true) {
            self.inner.pending_commands.fetch_sub(cmds.len(), Ordering::Release);
        } else {
            self.requeue_commands(cmds);
        }
        true
    }

    fn requeue_commands(&self, mut cmds: Vec<Cmd>) {
        let mut pending = self.inner.cmds.lock().unwrap();
        cmds.append(&mut pending);
        *pending = cmds;
        drop(pending);
        self.nap(Duration::from_secs(1));
    }

    /// Row id of a session, created on first sight.
    fn sid(&self, tx: &Connection, key: &str) -> Result<i64> {
        if let Some(r) = self.inner.shared.lock().unwrap().sessions.get(key) {
            return Ok(r.sid);
        }
        let sid = store::insert_session(tx, key)?;
        self.inner.shared.lock().unwrap().remember(key, SessRow { sid, ..Default::default() });
        Ok(sid)
    }

    /// Promote a cold session back to hot before writing to it (live events, or an indexed/pinned
    /// session that is receiving new content). No-op if it is already hot.
    fn ensure_hot(&self, tx: &Connection, sid: i64) -> Result<()> {
        if store::is_cold(tx, sid)? {
            cold::promote(tx, sid)?;
            // Two separate locks: holding the first guard into the second would self-deadlock.
            let key = self.inner.shared.lock().unwrap().keys.get(&sid).cloned();
            if let Some(key) = key
                && let Some(r) = self.inner.shared.lock().unwrap().sessions.get_mut(&key)
            {
                r.cold = false;
            }
        }
        Ok(())
    }

    /// Run `f` inside the rolling write transaction; `f` returns the change in event rows.
    ///
    /// Writes are batched across pages and sessions and committed by [`Writer::commit`] at most
    /// every [`COMMIT_EVERY`]: each commit makes FTS5 flush its pending terms into a new segment
    /// (merge work later), and each BEGIN/COMMIT takes SQLite's process-wide shm lock, which
    /// adapters reading their own SQLite stores in this process contend for.
    fn transaction(&self, f: impl FnOnce(&Self, &Connection) -> Result<i64>) -> Result<()> {
        let res = (|| {
            if self.tx_since.get().is_none() {
                self.conn.execute_batch("BEGIN IMMEDIATE")?;
                self.tx_since.set(Some(Instant::now()));
            }
            f(self, &self.conn)
        })();
        match res {
            Ok(delta) => {
                let mut sh = self.inner.shared.lock().unwrap();
                sh.progress.events = sh.progress.events.saturating_add_signed(delta);
                Ok(())
            }
            Err(err) => {
                self.rollback(&err);
                Err(err)
            }
        }
    }

    fn commit(&self, force: bool) -> bool {
        let Some(t0) = self.tx_since.get() else { return true };
        if !force && t0.elapsed() < COMMIT_EVERY {
            return true;
        }
        match self.conn.execute_batch("COMMIT") {
            Ok(()) => {
                self.tx_since.set(None);
                true
            }
            Err(err) => {
                self.rollback(&err.into());
                false
            }
        }
    }

    /// Drop the open transaction. The in-memory session map and row count may describe writes
    /// that never landed, so both are reloaded and the next turn reconciles again.
    fn rollback(&self, err: &anyhow::Error) {
        self.inner.note_error(format!("write: {err:#}"));
        if self.tx_since.take().is_some() {
            let _ = self.conn.execute_batch("ROLLBACK");
        }
        self.reload();
        self.lagged.set(true);
    }
}

/// Write freshly-collected rows (from a `PassMode::Cold` pass, or a brand-new pinned session) as
/// cold blocks, comparing against the existing fingerprint so an unchanged session is not
/// rewritten. Returns the change in row count.
fn finalize_cold_pass(tx: &Connection, sid: i64, events: Vec<Event>) -> Result<i64> {
    let rows: Vec<store::Row> = events
        .into_iter()
        .enumerate()
        .filter_map(|(seq, e)| {
            let (kind, text) = store::doc(&e)?;
            if text.is_empty() {
                return None;
            }
            Some(store::Row {
                id: store::row_id(sid, seq as i64, kind),
                event: e.id,
                ts: e.ts,
                pos: e.pos.map(|p| p as i64),
                text,
            })
        })
        .collect();
    let fp = cold::fingerprint(&rows);
    if store::fingerprint_of(tx, sid)? == Some(fp) {
        return Ok(0); // unchanged: skip rewriting (e.g. a version-bump refresh, or a repeat index_session).
    }
    let (old_rows, old_tokens) = cold::delete_blocks(tx, sid)?;
    store::bump_cold(tx, -old_rows, -old_tokens)?;
    let stats = cold::write_blocks(tx, sid, &rows)?;
    store::bump_cold(tx, stats.rows, stats.tokens)?;
    store::set_cold(tx, sid, true)?;
    store::set_fingerprint(tx, sid, fp)?;
    Ok(stats.rows - old_rows)
}

/// Compaction's write half: `rows` are hot rows already removed by [`store::drop_hot`].
fn cold_ify(tx: &Connection, sid: i64, rows: Vec<store::Row>) -> Result<()> {
    let stats = cold::write_blocks(tx, sid, &rows)?;
    store::bump_cold(tx, stats.rows, stats.tokens)?;
    store::set_cold(tx, sid, true)?;
    store::set_fingerprint(tx, sid, cold::fingerprint(&rows))?;
    Ok(())
}

/// Delete a session from whichever tier(s) hold it, plus its `sess` row. Returns the number of
/// event rows removed (hot + cold).
fn remove_everywhere(tx: &Connection, sid: i64) -> Result<u64> {
    let hot = store::drop_hot(tx, sid)?.len() as u64;
    let (cold_rows, cold_tokens) = cold::delete_blocks(tx, sid)?;
    if cold_rows != 0 {
        store::bump_cold(tx, -cold_rows, -cold_tokens)?;
    }
    store::drop_session_row(tx, sid)?;
    Ok(hot + cold_rows as u64)
}
