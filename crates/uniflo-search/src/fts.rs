//! Full-text index over event bodies: SQLite FTS5 (trigram tokenizer) in the cache directory.
//!
//! One background thread owns the write connection. It
//! - backfills every session through [`Engine::history`] pages, newest sessions first and
//!   throttled, recording per-session progress so a restart only catches up what changed;
//! - follows the engine broadcast, so new and overwritten events are searchable within ~100 ms;
//! - reconciles against the engine's session list periodically, because summary reads are never
//!   broadcast and a lagging subscriber loses envelopes, and drops sessions the engine removed;
//! - merges the index into one segment, in throttled steps, when a backlog leaves it spread over
//!   many ([`store::scattered`]).
//!
//! A second thread then reads the index into the OS page cache ([`FtsStatus::warming`]).
//!
//! Queries run on a pool of read-only connections and never wait for the writer (WAL).
//! Embedding: `engine.index()` first, then [`Fts::start`]; the gateway serves it as `/v1/search`.

pub mod query;
pub mod rank;
pub mod store;

use crate::{Query as SessionQuery, search as filter_sessions};
use anyhow::Result;
use rusqlite::{Connection, params_from_iter, types::Value as Sql};
use std::cell::Cell;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use store::SessRow;
use tokio::sync::broadcast::{self, error::TryRecvError};
use uniflo_core::util::now_ms;
use uniflo_core::{Engine, HistoryQuery};
use uniflo_schema::search::{FtsStatus, IndexProgress, SearchHit, SearchOrder, SearchResponse, SearchSession};
use uniflo_schema::{Envelope, Event, SCHEMA_VERSION, Session};

/// Bump when what or how events are indexed changes: the next start rebuilds the index.
pub const FTS_FORMAT: u32 = 1;

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
const DAY_MS: f64 = 86_400_000.0;

#[derive(Debug, Clone)]
pub struct FtsOptions {
    pub path: PathBuf,
    /// Index format; a different value (like a different Uniflo or schema version) discards
    /// the index at start.
    pub format: u32,
    /// Interval of the full reconciliation against the engine's session list.
    pub reconcile: Duration,
    /// Events per history page while backfilling.
    pub page: usize,
    /// Backfill throttle: after each page the writer sleeps `pause ×` the time the page took.
    pub pause: f32,
    /// See [`LIKE_BUDGET`].
    pub like_budget: Duration,
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
        }
    }
}

/// `fts-v1.sqlite` in Uniflo's cache directory (follows `UNIFLO_HOME`).
pub fn default_path() -> PathBuf {
    uniflo_core::cache::dir().join("fts-v1.sqlite")
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
}

struct Inner {
    engine: Arc<Engine>,
    opts: FtsOptions,
    rebuilt: bool,
    shared: Mutex<Shared>,
    readers: Mutex<Vec<Connection>>,
    cmds: Mutex<Vec<Cmd>>,
    stop: AtomicBool,
}

impl Inner {
    fn note_error(&self, msg: String) {
        tracing::debug!("full-text index: {msg}");
        let mut sh = self.shared.lock().unwrap();
        sh.errors += 1;
        sh.last_error = Some(msg);
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

impl Fts {
    /// Open (or rebuild) the index and start its writer thread. Call after [`Engine::index`];
    /// returns once the backlog is known, so [`Fts::status`] is meaningful immediately.
    pub fn start(engine: Arc<Engine>, opts: FtsOptions) -> Result<Fts> {
        let tag =
            format!("uniflo-fts/format{}/uniflo{}/schema{}", opts.format, env!("CARGO_PKG_VERSION"), SCHEMA_VERSION);
        let (conn, rebuilt) = store::open(&opts.path, &tag)?;
        let mut shared = Shared { merging: true, warming: true, ..Default::default() };
        for (k, r) in store::load_sessions(&conn)? {
            shared.remember(&k, r);
        }
        let fresh = shared.sessions.is_empty();
        let rx = engine.subscribe(None).1;
        let inner = Arc::new(Inner {
            engine,
            opts,
            rebuilt,
            shared: Mutex::new(shared),
            readers: Mutex::new(Vec::new()),
            cmds: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
        });
        let mut w = Writer {
            inner: inner.clone(),
            conn,
            rx,
            queue: VecDeque::new(),
            pass: None,
            next_reconcile: Instant::now(),
            lagged: Cell::new(false),
            tx_since: Cell::new(None),
            build: None,
            merge_check: true,
            merge: None,
        };
        w.reconcile();
        w.commit(true);
        if fresh && !w.queue.is_empty() {
            w.build = Some(Instant::now());
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
        }
    }

    /// Block until the backlog and queued [`Fts::index_session`] / [`Fts::remove_session`] calls
    /// are done. Returns false on timeout.
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let until = Instant::now() + timeout;
        loop {
            let idle = !self.inner.shared.lock().unwrap().indexing && self.inner.cmds.lock().unwrap().is_empty();
            if idle {
                // Let the writer finish the loop turn that drained the last command.
                std::thread::sleep(IDLE * 2);
                if !self.inner.shared.lock().unwrap().indexing {
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
        self.inner.cmds.lock().unwrap().push(Cmd::Index(Box::new(session), events));
    }

    /// Delete a session's rows (pinned or not). Applied asynchronously by the writer.
    pub fn remove_session(&self, key: &str) {
        self.inner.cmds.lock().unwrap().push(Cmd::Remove(key.to_owned()));
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
        let kinds: Option<HashSet<i64>> =
            p.kinds.as_ref().map(|ks| ks.iter().filter_map(|k| store::kind_code(k.trim())).collect());
        let mut groups: HashMap<i64, Vec<Cand>> = HashMap::new();
        let mut end = ScanEnd::default();
        match &plan.fts {
            Some(expr) if !plan.recent() => {
                self.rank_fts(&conn, &plan, expr, kinds.as_ref(), allowed.as_ref(), &mut groups)?
            }
            _ => {
                let need = p.offset + limit;
                end = self.scan_recent(&conn, &plan, kinds.as_ref(), allowed.as_ref(), need, &mut groups)?;
            }
        }
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
        let total = ranked.len();

        let mut results = Vec::new();
        for (meta, score, hits) in ranked.into_iter().skip(p.offset).take(limit) {
            let mut out = Vec::new();
            for c in &hits {
                let (event, ts, snippet) = self.hit(&conn, &plan, c.id)?;
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

    /// Relevance: bm25 over the FTS index alone (session and kind are in the row id). A single
    /// phrase skips bm25()'s IDF pass (see [`rank`]); the IDF is applied once the rows are counted.
    fn rank_fts(
        &self,
        conn: &Connection,
        plan: &query::Plan,
        expr: &str,
        kinds: Option<&HashSet<i64>>,
        allowed: Option<&HashSet<i64>>,
        groups: &mut HashMap<i64, Vec<Cand>>,
    ) -> Result<()> {
        let one = plan.phrases == 1;
        let sql = if one {
            "SELECT rowid, uniflo_bm25(docs_fts), uniflo_rows(docs_fts) FROM docs_fts WHERE docs_fts MATCH ?1"
        } else {
            "SELECT rowid, -bm25(docs_fts), 0 FROM docs_fts WHERE docs_fts MATCH ?1"
        };
        let mut st = conn.prepare_cached(sql)?;
        let mut rows = st.query([expr])?;
        let (mut matched, mut table_rows) = (0i64, 0i64);
        while let Some(r) = rows.next()? {
            matched += 1;
            table_rows = r.get(2)?;
            let id: i64 = r.get(0)?;
            if kinds.is_some_and(|k| !k.contains(&store::kind_of(id)))
                || allowed.is_some_and(|a| !a.contains(&store::sid_of(id)))
            {
                continue;
            }
            push_hit(groups, Cand { id, key: r.get(1)? });
        }
        if one {
            let idf = rank::idf(table_rows, matched);
            groups.values_mut().flatten().for_each(|c| c.key *= idf);
        }
        Ok(())
    }

    /// A short term (no trigram narrows it): `LIKE` over each session's rows, the most recently
    /// active session first; with longer terms too, only over the rows the FTS index matched. A
    /// session's `updated_at` bounds its events' times, so the scan stops once `need` sessions are
    /// found and the next session cannot hold a newer hit than the `need`-th, or when the time
    /// budget runs out.
    fn scan_recent(
        &self,
        conn: &Connection,
        plan: &query::Plan,
        kinds: Option<&HashSet<i64>>,
        allowed: Option<&HashSet<i64>>,
        need: usize,
        groups: &mut HashMap<i64, Vec<Cand>>,
    ) -> Result<ScanEnd> {
        let t0 = Instant::now();
        let budget = self.inner.opts.like_budget;
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
        for (sid, upd) in self.by_recency(allowed) {
            // Strictly newer: a session tied with the cut-off could still win on the key tie-break.
            if best.len() >= need && best.peek().is_some_and(|Reverse(ts)| *ts > upd) {
                return Ok(ScanEnd { partial: false, scanned_until: Some(upd) });
            }
            // One statement per candidate row, or per LIKE_CHUNK rows of the session's id range.
            let steps: Vec<(i64, i64)> = match &fts_rows {
                Some(by) => match by.get(&sid) {
                    Some(ids) => ids.iter().map(|&id| (id, id + 1)).collect(),
                    None => continue,
                },
                None => {
                    let ids = store::id_range(sid);
                    let Some(max) = last.query_row([ids.start, ids.end], |r| r.get::<_, Option<i64>>(0))? else {
                        continue;
                    };
                    let chunk = store::id_span(LIKE_CHUNK);
                    (ids.start..=max).step_by(chunk as usize).map(|lo| (lo, (lo + chunk).min(max + 1))).collect()
                }
            };
            let mut newest: Option<i64> = None;
            for (lo, hi) in steps {
                if t0.elapsed() >= budget {
                    return Ok(ScanEnd { partial: true, scanned_until: Some(upd) });
                }
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
            if let Some(ts) = newest {
                best.push(Reverse(ts));
                if best.len() > need {
                    best.pop();
                }
            }
        }
        Ok(ScanEnd::default())
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
    /// `snippet()` would re-run the whole query for every hit.
    fn hit(&self, conn: &Connection, plan: &query::Plan, id: i64) -> Result<(String, i64, String)> {
        let (event, ts, text): (String, i64, String) = conn
            .prepare_cached("SELECT event, ts, text FROM docs WHERE id = ?1")?
            .query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        if let Some(s) = query::snippet(&text, &plan.marks) {
            return Ok((event, ts, s));
        }
        let snippet = match &plan.fts {
            Some(expr) => {
                let sql = format!(
                    "SELECT snippet(docs_fts, 0, char(2), char(3), '…', {SNIPPET_TOKENS}) \
                     FROM docs_fts WHERE docs_fts MATCH ?1 AND rowid = ?2"
                );
                let s: String = conn.prepare_cached(&sql)?.query_row(rusqlite::params![expr, id], |r| r.get(0))?;
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
/// False when stopped first.
fn warm(inner: &Inner) -> Result<bool> {
    while {
        let sh = inner.shared.lock().unwrap();
        sh.indexing || sh.merging
    } {
        if inner.stop.load(Ordering::Relaxed) {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let conn = store::open_reader(&inner.opts.path)?;
    // count(*) visits every page of the b-tree.
    for table in ["docs_fts_idx", "docs_fts_docsize"] {
        conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |_| Ok(()))?;
    }
    Ok(true)
}

/// Ranking weight `1 + 1/(1 + age_days/30)`: ×2 today, ×1.5 a month ago, →1 for old sessions.
fn recency(now: i64, t: i64) -> f64 {
    1.0 + 1.0 / (1.0 + (now - t).max(0) as f64 / DAY_MS / 30.0)
}

/// Where a short-term scan stopped (see [`SearchResponse::scanned_until`]).
#[derive(Debug, Default)]
struct ScanEnd {
    partial: bool,
    scanned_until: Option<i64>,
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
    /// Higher is better: `-bm25`, or the event time when ordering by recency.
    key: f64,
}

impl Cand {
    fn best_first(a: &Cand, b: &Cand) -> std::cmp::Ordering {
        b.key.total_cmp(&a.key).then(b.id.cmp(&a.id))
    }
}

/// One backfill pass over a session: history pages from the newest down to `stop`.
struct Pass {
    session: Session,
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
    rx: broadcast::Receiver<Arc<Envelope>>,
    queue: VecDeque<Session>,
    pass: Option<Pass>,
    next_reconcile: Instant,
    /// Reconcile at the next turn: the broadcast lagged or a write was rolled back.
    lagged: Cell<bool>,
    /// Start of the open write transaction.
    tx_since: Cell<Option<Instant>>,
    build: Option<Instant>,
    /// Look at the segments at the next idle turn: set at start and whenever a backlog drains.
    merge_check: bool,
    /// The running segment merge: its start, and whether its first step ran.
    merge: Option<(Instant, bool)>,
}

impl Writer {
    fn run(mut self) {
        match store::count_docs(&self.conn) {
            Ok(n) => self.inner.shared.lock().unwrap().progress.events = n,
            Err(e) => self.inner.note_error(format!("count: {e:#}")),
        }
        while !self.inner.stop.load(Ordering::Relaxed) {
            let busy = self.follow() | self.commands();
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
                    if !self.merge_step() && !busy {
                        self.nap(IDLE);
                    }
                }
            }
        }
        self.commit(true);
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
                    removed += store::delete_session(tx, *sid)? as i64;
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
            let sh = self.inner.shared.lock().unwrap();
            let row = sh.sessions.get(&session.key);
            if row.is_some_and(|r| r.pinned) {
                continue;
            }
            let stop = row.filter(|r| r.done_upd.is_some()).and_then(|r| r.done_pos);
            return Some(Pass { session, stop, before: None, seen: HashSet::new(), max_pos: None });
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
                        delta += store::upsert(tx, sid, event)?;
                    }
                    Envelope::Removed { key, .. } => {
                        let sid = w.inner.shared.lock().unwrap().sessions.get(key).filter(|r| !r.pinned).map(|r| r.sid);
                        if let Some(sid) = sid {
                            delta -= store::delete_session(tx, sid)? as i64;
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
        let _ = self.transaction(|w, tx| {
            let mut delta = 0;
            for cmd in &cmds {
                match cmd {
                    Cmd::Index(session, events) => {
                        let sid = w.sid(tx, &session.key)?;
                        let mut seen = HashSet::new();
                        for e in events.iter().rev() {
                            if seen.insert(e.id.as_str()) {
                                delta += store::upsert(tx, sid, e)?;
                            }
                        }
                        store::pin(tx, sid, session)?;
                        if let Some(r) = w.inner.shared.lock().unwrap().sessions.get_mut(&session.key) {
                            r.pinned = true;
                            r.done_upd = Some(session.updated_at);
                            r.meta = Some((**session).clone());
                        }
                    }
                    Cmd::Remove(key) => {
                        let sid = w.inner.shared.lock().unwrap().sessions.get(key).map(|r| r.sid);
                        if let Some(sid) = sid {
                            delta -= store::delete_session(tx, sid)? as i64;
                            w.inner.shared.lock().unwrap().forget(key);
                        }
                    }
                }
            }
            Ok(delta)
        });
        true
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

    /// Run `f` inside the rolling write transaction; `f` returns the change in event rows.
    ///
    /// Writes are batched across pages and sessions and committed by [`Writer::commit`] at most
    /// every [`COMMIT_EVERY`]: each commit makes FTS5 flush its pending terms into a new segment
    /// (merge work later), and each BEGIN/COMMIT takes SQLite's process-wide shm lock, which
    /// adapters reading their own SQLite stores in this process contend for.
    fn transaction(&self, f: impl FnOnce(&Self, &Connection) -> Result<i64>) -> Result<()> {
        let res = (|| {
            if self.tx_since.get().is_none() {
                self.conn.execute_batch("BEGIN")?;
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

    fn commit(&self, force: bool) {
        let Some(t0) = self.tx_since.get() else { return };
        if !force && t0.elapsed() < COMMIT_EVERY {
            return;
        }
        match self.conn.execute_batch("COMMIT") {
            Ok(()) => self.tx_since.set(None),
            Err(err) => self.rollback(&err.into()),
        }
    }

    /// Drop the open transaction. The in-memory session map and row count may describe writes
    /// that never landed, so both are reloaded and the next turn reconciles again.
    fn rollback(&self, err: &anyhow::Error) {
        self.inner.note_error(format!("write: {err:#}"));
        if self.tx_since.take().is_some() {
            let _ = self.conn.execute_batch("ROLLBACK");
        }
        if let Ok(rows) = store::load_sessions(&self.conn) {
            let mut sh = self.inner.shared.lock().unwrap();
            sh.sessions.clear();
            sh.keys.clear();
            for (k, r) in rows {
                sh.remember(&k, r);
            }
        }
        if let Ok(n) = store::count_docs(&self.conn) {
            self.inner.shared.lock().unwrap().progress.events = n;
        }
        self.lagged.set(true);
    }
}
