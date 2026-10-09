//! The live index: owns every session snapshot, applies adapter output, derives
//! status and fans changes out as [`Envelope`]s.
//!
//! Concurrency model: one async loop is the only writer of source cursors (reads are
//! serialized per change batch); readers (gateway) take short read locks and never
//! touch disk except for `history`, which delegates to the adapter.

use crate::adapter::{Adapter, Cursor, HistoryQuery, LiveSession, MetaPatch, ReadOutput, Record};
use crate::archive::ArchiveStore;
use crate::cache::{self, CacheFile, CachedSession, CachedSource};
use crate::pricing::Pricing;
use crate::status::{StatusTracker, Windows, effective};
use crate::usage::UsageIndex;
use crate::util::{now_ms, preview};
use crate::watch;
use anyhow::{Result, anyhow};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};
use uniflo_schema::{Body, Envelope, Event, Harness, SCHEMA_VERSION, Session, Status, UsageProgress, session_key};

#[path = "engine_usage.rs"]
mod usage_glue;
pub use usage_glue::PriceSync;
#[path = "engine_archive.rs"]
mod archive_glue;
pub(crate) use archive_glue::CleanupView;

#[derive(Debug, Clone)]
pub struct EngineOptions {
    /// `work` with no activity for this long and no live process → `idle`.
    pub stale_after: Duration,
    /// Shorter window when the last activity was assistant text (no explicit turn end).
    pub settle_after: Duration,
    pub cache_path: Option<PathBuf>,
    /// Envelopes kept for `since=` replay.
    pub replay: usize,
    pub hot_poll: Duration,
    pub live_poll: Duration,
    pub rescan: Duration,
    pub threads: usize,
    /// Periodic crates.io update check (daemon opt-in); `None` disables it.
    pub update_check: Option<Duration>,
    /// Build per-step usage ledgers in the background after indexing.
    pub usage: bool,
    /// Uniflo's own data directory (`pricing/` lives here); `None` = embedded prices only.
    pub data_dir: Option<PathBuf>,
    /// Periodic price catalog sync (daemon opt-in); `None` never touches the network.
    pub price_sync: Option<PriceSync>,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            stale_after: Duration::from_secs(10 * 60),
            settle_after: Duration::from_secs(90),
            cache_path: Some(cache::default_path()),
            replay: 8192,
            hot_poll: Duration::from_millis(200),
            live_poll: Duration::from_secs(1),
            rescan: Duration::from_secs(30),
            threads: std::thread::available_parallelism().map_or(4, |n| n.get()),
            update_check: None,
            usage: true,
            data_dir: Some(crate::paths::data_dir()),
            price_sync: None,
        }
    }
}

const UNKNOWN_CAP: usize = 512;
const PREVIEW_CHARS: usize = 160;
const TITLE_CHARS: usize = 200;

struct Entry {
    session: Session,
    tracker: StatusTracker,
    title_rank: u8,
    adapter: usize,
    src: PathBuf,
    live_pid: Option<u32>,
    live_status: Option<Status>,
    /// Archive file of a cleaned-up session (then `src` is that file too).
    archive: Option<PathBuf>,
}

impl Entry {
    fn new(key: String, harness: &str, id: &str, adapter: usize, src: &Path) -> Self {
        Entry {
            session: Session {
                key,
                harness: harness.to_owned(),
                id: id.to_owned(),
                parent: None,
                title: None,
                cwd: None,
                model: None,
                preview: None,
                source: src.display().to_string(),
                started_at: None,
                updated_at: 0,
                status: Status::Idle,
                status_since: 0,
                status_reason: None,
                pid: None,
                usage: None,
                archived: false,
            },
            tracker: StatusTracker::default(),
            title_rank: 0,
            adapter,
            src: src.to_path_buf(),
            live_pid: None,
            live_status: None,
            archive: None,
        }
    }

    fn apply_meta(&mut self, p: MetaPatch, harness: &str) {
        let s = &mut self.session;
        if let Some(par) = p.parent {
            s.parent = Some(session_key(harness, &par));
        }
        if let Some((rank, t)) = p.title {
            let t = preview(&t, TITLE_CHARS);
            if rank >= self.title_rank && !t.is_empty() {
                self.title_rank = rank;
                s.title = Some(t);
            }
        }
        if p.cwd.is_some() {
            s.cwd = p.cwd;
        }
        if p.model.is_some() {
            s.model = p.model;
        }
        if let Some(t) = p.started_at.filter(|t| *t > 0) {
            s.started_at = Some(s.started_at.map_or(t, |x| x.min(t)));
        }
        if let Some(t) = p.updated_at {
            s.updated_at = s.updated_at.max(t);
        }
    }

    fn observe(&mut self, e: &Event) {
        self.tracker.observe(e);
        if e.ts > 0 {
            self.session.updated_at = self.session.updated_at.max(e.ts);
            if self.session.started_at.is_none() {
                self.session.started_at = Some(e.ts);
            }
        }
        if self.session.preview.is_none()
            && let Body::UserMessage { text, synthetic: false } = &e.body
        {
            let p = preview(text, PREVIEW_CHARS);
            if !p.is_empty() {
                self.session.preview = Some(p);
            }
        }
    }

    /// Recompute the published status fields. Returns whether the significant view changed.
    fn refresh(&mut self, now: i64, w: Windows) -> bool {
        let before = sig(&self.session);
        let eff = effective(&self.tracker, self.live_pid.is_some(), self.live_status, now, w);
        self.session.status = eff.status;
        self.session.status_since = eff.since;
        self.session.status_reason = (!eff.reason.is_empty()).then_some(eff.reason);
        self.session.pid = self.live_pid;
        before != sig(&self.session)
    }
}

/// Fields whose change is worth a `session` envelope (status_reason / updated_at are too chatty).
fn sig(s: &Session) -> impl PartialEq + use<> {
    (
        s.parent.clone(),
        s.title.clone(),
        s.cwd.clone(),
        s.model.clone(),
        s.preview.clone(),
        s.started_at,
        s.status,
        s.pid,
    )
}

struct SourceState {
    adapter: usize,
    cursor: Cursor,
    keys: Vec<String>,
}

#[derive(Default)]
struct State {
    sessions: HashMap<String, Entry>,
    sources: HashMap<PathBuf, SourceState>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Stats {
    pub uptime_ms: i64,
    pub seq: u64,
    pub sources: usize,
    pub sessions: usize,
    pub working: usize,
    pub subscribers: usize,
    pub index_ms: u64,
    pub indexed_files: usize,
    pub restored_from_cache: usize,
    pub reads: u64,
    pub bytes: u64,
    pub bad_lines: u64,
    pub read_errors: u64,
    pub last_error: Option<String>,
    /// `harness:discriminator` → count of records no decoder rule covered.
    pub unknown: BTreeMap<String, u64>,
    /// Last crates.io update check (daemon background task); `None` until it lands.
    pub update: Option<crate::update::UpdateInfo>,
    /// Background usage ledger progress; totals are partial until `ready`.
    pub usage: Option<UsageProgress>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexReport {
    pub ms: u64,
    pub files: usize,
    pub read: usize,
    pub restored: usize,
    pub sessions: usize,
    pub errors: usize,
}

enum Pending {
    Session(Session),
    Event(Event),
    Removed(String),
}

enum Msg {
    Changed(PathBuf),
    Live(usize),
    /// Result of a live probe, applied on the loop so writes stay single-threaded.
    Probed(usize, Vec<LiveSession>),
    Rescan,
    UpdateTick,
    /// Result of the crates.io fetch (blocking curl), applied on the loop.
    UpdateApplied(crate::update::UpdateInfo),
}

pub struct Engine {
    adapters: Vec<Arc<dyn Adapter>>,
    opts: EngineOptions,
    tag: String,
    state: RwLock<State>,
    seq: AtomicU64,
    tx: broadcast::Sender<Arc<Envelope>>,
    replay: Mutex<VecDeque<Arc<Envelope>>>,
    stats: Mutex<Stats>,
    dirty: AtomicBool,
    /// One in-flight live probe per adapter (probes may shell out to `ps`/`lsof`).
    probing: Vec<AtomicBool>,
    /// One in-flight crates.io fetch.
    updating: AtomicBool,
    started: i64,
    pricing: Pricing,
    usage: RwLock<UsageIndex>,
    usage_tx: Mutex<Option<mpsc::UnboundedSender<usage_glue::UsageMsg>>>,
    archive: Option<Arc<ArchiveStore>>,
}

impl Engine {
    pub fn new(adapters: Vec<Arc<dyn Adapter>>, opts: EngineOptions) -> Arc<Self> {
        let ids: Vec<&str> = adapters.iter().map(|a| a.info().id).collect();
        let tag = format!("uniflo-core/{}/schema{}/{}", env!("CARGO_PKG_VERSION"), SCHEMA_VERSION, ids.join(","));
        let (tx, _) = broadcast::channel(opts.replay.max(1024));
        let probing = adapters.iter().map(|_| AtomicBool::new(false)).collect();
        let pricing = Pricing::new(opts.data_dir.as_ref().map(|d| d.join("pricing")));
        pricing.set_sync_enabled(opts.price_sync.is_some());
        let archive = opts.data_dir.as_ref().map(|d| Arc::new(ArchiveStore::open(d.join("archive"))));
        Arc::new(Engine {
            probing,
            adapters,
            opts,
            tag,
            state: RwLock::new(State::default()),
            seq: AtomicU64::new(0),
            tx,
            replay: Mutex::new(VecDeque::new()),
            stats: Mutex::new(Stats::default()),
            dirty: AtomicBool::new(false),
            updating: AtomicBool::new(false),
            started: now_ms(),
            pricing,
            usage: RwLock::new(UsageIndex::default()),
            usage_tx: Mutex::new(None),
            archive,
        })
    }

    fn windows(&self) -> Windows {
        Windows {
            stale_ms: self.opts.stale_after.as_millis() as i64,
            settle_ms: self.opts.settle_after.as_millis() as i64,
        }
    }

    // ---------------------------------------------------------------- queries

    pub fn sessions(&self) -> Vec<Session> {
        let st = self.state.read().unwrap();
        let mut v: Vec<Session> = st.sessions.values().map(|e| e.session.clone()).collect();
        v.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then_with(|| a.key.cmp(&b.key)));
        v
    }

    pub fn session(&self, key: &str) -> Option<Session> {
        self.state.read().unwrap().sessions.get(key).map(|e| e.session.clone())
    }

    /// Blocking: reads the transcript through the owning adapter.
    pub fn history(&self, key: &str, q: &HistoryQuery) -> Result<Vec<Event>> {
        let (adapter, src, id, archived) = {
            let st = self.state.read().unwrap();
            let e = st.sessions.get(key).ok_or_else(|| anyhow!("unknown session {key}"))?;
            (self.adapters[e.adapter].clone(), e.src.clone(), e.session.id.clone(), e.archive.is_some())
        };
        if archived {
            return Self::archived_history(&src, q);
        }
        adapter.history(&src, &id, q)
    }

    pub fn harnesses(&self) -> Vec<Harness> {
        let st = self.state.read().unwrap();
        let mut counts: HashMap<&str, (usize, usize)> = HashMap::new();
        for e in st.sessions.values() {
            let c = counts.entry(self.adapters[e.adapter].info().id).or_default();
            c.0 += 1;
            c.1 += usize::from(e.session.status == Status::Work);
        }
        self.adapters
            .iter()
            .map(|a| {
                let info = a.info();
                let (sessions, working) = counts.get(info.id).copied().unwrap_or_default();
                Harness {
                    id: info.id.into(),
                    name: info.name.into(),
                    roots: a.roots().iter().map(|p| p.display().to_string()).collect(),
                    sessions,
                    working,
                }
            })
            .collect()
    }

    pub fn stats(&self) -> Stats {
        let mut s = self.stats.lock().unwrap().clone();
        let st = self.state.read().unwrap();
        s.uptime_ms = now_ms() - self.started;
        s.seq = self.seq.load(Ordering::SeqCst);
        s.sources = st.sources.len();
        s.sessions = st.sessions.len();
        s.working = st.sessions.values().filter(|e| e.session.status == Status::Work).count();
        s.subscribers = self.tx.receiver_count();
        drop(st);
        s.usage = self.opts.usage.then(|| self.usage_progress());
        s
    }

    pub fn seq(&self) -> u64 {
        self.seq.load(Ordering::SeqCst)
    }

    /// Envelopes after `since` still in the replay ring, plus a live receiver.
    /// The bool is false when `since` is older than the ring (the client must re-sync).
    pub fn subscribe(&self, since: Option<u64>) -> (Vec<Arc<Envelope>>, broadcast::Receiver<Arc<Envelope>>, bool) {
        let ring = self.replay.lock().unwrap();
        let rx = self.tx.subscribe();
        let Some(since) = since else { return (Vec::new(), rx, true) };
        let complete = ring.front().is_none_or(|f| f.seq() <= since + 1) || since >= self.seq();
        let backlog = ring.iter().filter(|e| e.seq() > since).cloned().collect();
        (backlog, rx, complete)
    }

    pub fn hello(&self) -> Envelope {
        Envelope::Hello {
            seq: self.seq(),
            version: SCHEMA_VERSION,
            server: format!("uniflo/{}", env!("CARGO_PKG_VERSION")),
        }
    }

    fn publish(&self, items: Vec<Pending>) {
        if items.is_empty() {
            return;
        }
        let mut ring = self.replay.lock().unwrap();
        for it in items {
            let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
            let env = Arc::new(match it {
                Pending::Session(session) => Envelope::Session { seq, session },
                Pending::Event(event) => Envelope::Event { seq, event },
                Pending::Removed(key) => Envelope::Removed { seq, key },
            });
            if ring.len() >= self.opts.replay {
                ring.pop_front();
            }
            ring.push_back(env.clone());
            let _ = self.tx.send(env);
        }
    }

    // ---------------------------------------------------------------- apply

    fn apply(&self, st: &mut State, adapter: usize, src: &Path, out: ReadOutput, live: bool, now: i64) -> Vec<Pending> {
        // A read that raced a cleanup: the file is in the trash, its session in the archive.
        if self.tombstoned(src) {
            return Vec::new();
        }
        let harness = self.adapters[adapter].info().id;
        let mut pending = Vec::new();
        {
            let mut stats = self.stats.lock().unwrap();
            stats.reads += 1;
            stats.bytes += out.batch.bytes;
            stats.bad_lines += out.batch.bad_lines;
            for u in &out.batch.unknown {
                let k = format!("{harness}:{u}");
                if stats.unknown.len() < UNKNOWN_CAP || stats.unknown.contains_key(&k) {
                    *stats.unknown.entry(k).or_default() += 1;
                }
            }
        }
        let source = st.sources.entry(src.to_path_buf()).or_insert_with(|| SourceState {
            adapter,
            cursor: Cursor::default(),
            keys: Vec::new(),
        });
        if out.reset {
            for k in &source.keys {
                if let Some(e) = st.sessions.get_mut(k) {
                    e.tracker = StatusTracker::default();
                }
            }
        }
        source.cursor = out.cursor.clone();
        let mut before: HashMap<String, Option<Session>> = HashMap::new();
        for (id, rec) in out.batch.items {
            let key = session_key(harness, &id);
            let source = st.sources.get_mut(src).unwrap();
            if !source.keys.contains(&key) {
                source.keys.push(key.clone());
            }
            if let Some(old) = st.sessions.get(&key).filter(|e| e.archive.is_some()) {
                self.unarchive(old);
                st.sessions.remove(&key);
            }
            let entry =
                st.sessions.entry(key.clone()).or_insert_with(|| Entry::new(key.clone(), harness, &id, adapter, src));
            before.entry(key).or_insert_with(|| {
                (entry.session.updated_at > 0 || entry.tracker.since > 0).then(|| entry.session.clone())
            });
            match rec {
                Record::Meta(p) => entry.apply_meta(p, harness),
                Record::Event(e) => {
                    entry.observe(&e);
                    if live && !out.summary {
                        pending.push(Pending::Event(e));
                    }
                }
            }
        }
        let stale = self.windows();
        for (key, prev) in before {
            let Some(entry) = st.sessions.get_mut(&key) else { continue };
            if entry.session.updated_at == 0 {
                entry.session.updated_at = out.cursor.mtime_ms;
            }
            let changed = entry.refresh(now, stale);
            let is_new = prev.is_none();
            if live && (is_new || changed || prev.is_some_and(|p| sig(&p) != sig(&entry.session))) {
                pending.push(Pending::Session(entry.session.clone()));
            }
        }
        self.dirty.store(true, Ordering::Relaxed);
        // Session snapshots first so clients learn a session before its events.
        pending.sort_by_key(|p| !matches!(p, Pending::Session(_)));
        pending
    }

    fn remove_source(&self, st: &mut State, src: &Path) -> Vec<Pending> {
        let Some(s) = st.sources.remove(src) else { return Vec::new() };
        self.dirty.store(true, Ordering::Relaxed);
        self.usage_gone(src);
        let mut out = Vec::new();
        for k in s.keys {
            if st.sessions.get(&k).is_some_and(|e| e.src == src) {
                st.sessions.remove(&k);
                out.push(Pending::Removed(k));
            }
        }
        out
    }

    fn apply_live(&self, adapter: usize, list: Vec<LiveSession>, now: i64) -> Vec<Pending> {
        let harness = self.adapters[adapter].info().id;
        let by_key: HashMap<String, LiveSession> = list.into_iter().map(|l| (session_key(harness, &l.id), l)).collect();
        let stale = self.windows();
        let mut st = self.state.write().unwrap();
        let mut pending = Vec::new();
        for e in st.sessions.values_mut().filter(|e| e.adapter == adapter && e.archive.is_none()) {
            let next = by_key.get(&e.session.key);
            let (pid, status) = (next.map(|l| l.pid), next.and_then(|l| l.status));
            if pid == e.live_pid && status == e.live_status {
                continue;
            }
            if e.live_pid.is_some() && pid.is_none() && e.tracker.status == Status::Work {
                e.tracker.force(Status::Idle, "exited", now);
            }
            e.live_pid = pid;
            e.live_status = status;
            if e.refresh(now, stale) {
                pending.push(Pending::Session(e.session.clone()));
            }
        }
        pending
    }

    fn tick(&self, now: i64) -> Vec<Pending> {
        let stale = self.windows();
        let mut st = self.state.write().unwrap();
        st.sessions
            .values_mut()
            .filter(|e| e.session.status == Status::Work || e.tracker.status == Status::Work)
            .filter_map(|e| e.refresh(now, stale).then(|| Pending::Session(e.session.clone())))
            .collect()
    }

    // ---------------------------------------------------------------- index

    /// Blocking initial index: restore cache, read new/changed sources in parallel.
    pub fn index(&self) -> IndexReport {
        let t0 = Instant::now();
        let now = now_ms();
        let mut cached = self.opts.cache_path.as_deref().map(|p| cache::load(p, &self.tag)).unwrap_or_default();

        let discovered: Vec<(usize, PathBuf)> = std::thread::scope(|s| {
            let hs: Vec<_> = self
                .adapters
                .iter()
                .enumerate()
                .map(|(i, a)| s.spawn(move || a.discover().into_iter().map(|p| (i, p)).collect::<Vec<_>>()))
                .collect();
            hs.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
        });
        let discovered: Vec<(usize, PathBuf)> = discovered.into_iter().filter(|(_, p)| self.admit(p)).collect();

        let mut jobs: Vec<(usize, PathBuf, Option<Cursor>)> = Vec::new();
        let mut restored = 0;
        {
            let mut st = self.state.write().unwrap();
            for (i, path) in &discovered {
                let harness = self.adapters[*i].info().id;
                match cached.remove(path).filter(|c| c.harness == harness) {
                    Some(c) => {
                        let changed = self.adapters[*i].changed(path, &c.cursor);
                        self.restore(&mut st, *i, c.clone());
                        if changed {
                            jobs.push((*i, path.clone(), Some(c.cursor)));
                        } else {
                            restored += 1;
                        }
                    }
                    None => jobs.push((*i, path.clone(), None)),
                }
            }
        }

        let errors = AtomicUsize::new(0);
        let next = AtomicUsize::new(0);
        let (rtx, rrx) = std::sync::mpsc::channel::<(usize, PathBuf, ReadOutput)>();
        let threads = self.opts.threads.clamp(1, 64);
        std::thread::scope(|s| {
            for _ in 0..threads {
                let rtx = rtx.clone();
                let (jobs, next, errors) = (&jobs, &next, &errors);
                s.spawn(move || {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some((a, p, c)) = jobs.get(i) else { break };
                        match self.adapters[*a].read(p, c.as_ref()) {
                            Ok(out) => {
                                let _ = rtx.send((*a, p.clone(), out));
                            }
                            Err(err) => {
                                errors.fetch_add(1, Ordering::Relaxed);
                                self.note_error(&format!("{}: {err:#}", p.display()));
                            }
                        }
                    }
                });
            }
            drop(rtx);
            for (a, p, out) in rrx {
                let mut st = self.state.write().unwrap();
                self.apply(&mut st, a, &p, out, false, now);
            }
        });

        self.load_archived(&mut self.state.write().unwrap());
        for i in 0..self.adapters.len() {
            if let Some(list) = self.adapters[i].live() {
                self.apply_live(i, list, now_ms());
            }
        }
        self.tick(now_ms());
        let report = IndexReport {
            ms: t0.elapsed().as_millis() as u64,
            files: discovered.len(),
            read: jobs.len(),
            restored,
            sessions: self.state.read().unwrap().sessions.len(),
            errors: errors.load(Ordering::Relaxed),
        };
        {
            let mut s = self.stats.lock().unwrap();
            s.index_ms = report.ms;
            s.indexed_files = report.files;
            s.restored_from_cache = report.restored;
        }
        self.dirty.store(true, Ordering::Relaxed);
        report
    }

    fn restore(&self, st: &mut State, adapter: usize, c: CachedSource) {
        let mut keys = Vec::new();
        for cs in c.sessions {
            let key = cs.session.key.clone();
            keys.push(key.clone());
            st.sessions.insert(
                key,
                Entry {
                    session: cs.session,
                    tracker: cs.tracker,
                    title_rank: cs.title_rank,
                    adapter,
                    src: c.path.clone(),
                    live_pid: None,
                    live_status: None,
                    archive: None,
                },
            );
        }
        st.sources.insert(c.path, SourceState { adapter, cursor: c.cursor, keys });
    }

    fn note_error(&self, msg: &str) {
        tracing::debug!("read error: {msg}");
        let mut s = self.stats.lock().unwrap();
        s.read_errors += 1;
        s.last_error = Some(msg.to_owned());
    }

    pub fn save_cache(&self) -> Result<()> {
        let Some(path) = &self.opts.cache_path else { return Ok(()) };
        let file = {
            let st = self.state.read().unwrap();
            let sources = st
                .sources
                .iter()
                .map(|(p, s)| CachedSource {
                    harness: self.adapters[s.adapter].info().id.to_owned(),
                    path: p.clone(),
                    cursor: s.cursor.clone(),
                    sessions: s
                        .keys
                        .iter()
                        .filter_map(|k| st.sessions.get(k))
                        .map(|e| CachedSession {
                            session: Session { pid: None, ..e.session.clone() },
                            tracker: e.tracker.clone(),
                            title_rank: e.title_rank,
                        })
                        .collect(),
                })
                .collect();
            CacheFile { tag: self.tag.clone(), sources }
        };
        cache::save(path, &file)?;
        self.dirty.store(false, Ordering::Relaxed);
        Ok(())
    }

    // ---------------------------------------------------------------- live loop

    /// Run forever: filesystem watch + hot polling + live probes + staleness ticks.
    /// Call [`Engine::index`] first.
    pub async fn run(self: Arc<Self>) -> Result<()> {
        let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
        let _watcher = self.start_watcher(tx.clone());
        // Close the window between `index()` and the watcher becoming active.
        let _ = tx.send(Msg::Rescan);

        spawn_ticker(tx.clone(), self.opts.rescan, || Msg::Rescan);
        tokio::spawn(self.clone().usage_loop());
        if let Some(every) = self.opts.update_check {
            // Check once at start, then periodically; the fetch itself runs blocking.
            let _ = tx.send(Msg::UpdateTick);
            spawn_ticker(tx.clone(), every, || Msg::UpdateTick);
        }
        let me = self.clone();
        let probed = tokio::task::spawn_blocking(move || {
            (0..me.adapters.len())
                .filter(|&i| !me.adapters[i].live_roots().is_empty() || me.adapters[i].live().is_some())
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
        for i in probed {
            spawn_ticker(tx.clone(), self.opts.live_poll, move || Msg::Live(i));
        }
        let mut hot = tokio::time::interval(self.opts.hot_poll);
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut save = tokio::time::interval(Duration::from_secs(30));
        hot.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                Some(msg) = rx.recv() => {
                    let mut changed: Vec<PathBuf> = Vec::new();
                    let mut seen = HashSet::new();
                    let mut live: HashSet<usize> = HashSet::new();
                    let mut probed: Vec<(usize, Vec<LiveSession>)> = Vec::new();
                    let mut rescan = false;
                    let mut update = false;
                    let mut push = |m: Msg| match m {
                        Msg::Changed(p) => { if seen.insert(p.clone()) { changed.push(p) } }
                        Msg::Live(i) => { live.insert(i); }
                        Msg::Probed(i, list) => probed.push((i, list)),
                        Msg::Rescan => rescan = true,
                        Msg::UpdateTick => update = true,
                        Msg::UpdateApplied(info) => self.set_update(info),
                    };
                    push(msg);
                    while let Ok(m) = rx.try_recv() { push(m) }
                    if rescan {
                        for p in self.clone().rescan().await {
                            if seen.insert(p.clone()) { changed.push(p) }
                        }
                    }
                    for p in changed { self.clone().process(p).await; }
                    for (i, list) in probed {
                        let p = self.apply_live(i, list, now_ms());
                        self.publish(p);
                    }
                    for i in live { self.spawn_probe(i, tx.clone()); }
                    if update { self.spawn_update_check(tx.clone()); }
                }
                _ = hot.tick() => {
                    for p in self.hot_sources() { self.clone().process(p).await; }
                }
                _ = tick.tick() => {
                    let p = self.tick(now_ms());
                    self.publish(p);
                }
                _ = save.tick() => {
                    if self.dirty.load(Ordering::Relaxed) {
                        let me = self.clone();
                        let _ = tokio::task::spawn_blocking(move || me.save_cache()).await;
                    }
                }
            }
        }
    }

    fn start_watcher(self: &Arc<Self>, tx: mpsc::UnboundedSender<Msg>) -> Option<watch::Watcher> {
        let mut roots: Vec<PathBuf> = Vec::new();
        let mut live_roots: Vec<(usize, PathBuf)> = Vec::new();
        for (i, a) in self.adapters.iter().enumerate() {
            roots.extend(a.roots());
            for r in a.live_roots() {
                roots.push(r.clone());
                live_roots.push((i, r));
            }
        }
        let me = Arc::downgrade(self);
        let res = watch::watch(&roots, move |ev| {
            let Some(me) = me.upgrade() else { return };
            match ev {
                watch::Change::Path(p) => {
                    for (i, r) in &live_roots {
                        if p.starts_with(r) {
                            let _ = tx.send(Msg::Live(*i));
                        }
                    }
                    for a in &me.adapters {
                        if let Some(src) = a.source_for(&p) {
                            let _ = tx.send(Msg::Changed(src));
                            break;
                        }
                    }
                    // A cleaned-up directory moved back from the trash: find its transcripts now.
                    if me.tombstoned(&p) {
                        let _ = tx.send(Msg::Rescan);
                    }
                }
                watch::Change::Rescan => {
                    let _ = tx.send(Msg::Rescan);
                }
            }
        });
        match res {
            Ok(w) => Some(w),
            Err(err) => {
                tracing::warn!("filesystem watch unavailable, relying on polling: {err:#}");
                None
            }
        }
    }

    /// Sources likely to change soon: working sessions and anything touched in the last minute.
    fn hot_sources(&self) -> Vec<PathBuf> {
        let now = now_ms();
        let st = self.state.read().unwrap();
        let mut out = Vec::new();
        for (p, s) in &st.sources {
            let hot = s.keys.iter().filter_map(|k| st.sessions.get(k)).any(|e| {
                e.session.status == Status::Work || e.live_pid.is_some() || now - e.session.updated_at < 60_000
            });
            if hot && self.adapters[s.adapter].changed(p, &s.cursor) {
                out.push(p.clone());
            }
        }
        out
    }

    async fn rescan(self: Arc<Self>) -> Vec<PathBuf> {
        let me = self.clone();
        let found = tokio::task::spawn_blocking(move || {
            me.adapters.iter().flat_map(|a| a.discover()).collect::<HashSet<PathBuf>>()
        })
        .await
        .unwrap_or_default();
        let (new, gone): (Vec<PathBuf>, Vec<PathBuf>) = {
            let st = self.state.read().unwrap();
            // New sources, plus known ones that moved without a notification (dropped events).
            let new = found
                .iter()
                .filter(|p| st.sources.get(*p).is_none_or(|s| self.adapters[s.adapter].changed(p, &s.cursor)))
                .cloned()
                .collect();
            let gone = st.sources.keys().filter(|p| !found.contains(*p) && !p.exists()).cloned().collect();
            (new, gone)
        };
        if !gone.is_empty() {
            let mut st = self.state.write().unwrap();
            let pending: Vec<Pending> = gone.iter().flat_map(|p| self.remove_source(&mut st, p)).collect();
            drop(st);
            self.publish(pending);
        }
        new
    }

    async fn process(self: Arc<Self>, src: PathBuf) {
        if !src.exists() {
            let pending = {
                let mut st = self.state.write().unwrap();
                self.remove_source(&mut st, &src)
            };
            self.publish(pending);
            return;
        }
        if self.tombstoned(&src) {
            let (me, p) = (self.clone(), src.clone());
            if !tokio::task::spawn_blocking(move || me.admit(&p)).await.unwrap_or(false) {
                return;
            }
        }
        let known = {
            let st = self.state.read().unwrap();
            st.sources.get(&src).map(|s| (s.adapter, s.cursor.clone()))
        };
        let (adapter, cursor) = match known {
            Some((a, c)) => (a, Some(c)),
            // A source born while running is new content: follow it from the start so its
            // first records are broadcast (adapters fall back to a summary when it is huge).
            None => match self.adapters.iter().position(|a| a.source_for(&src).as_deref() == Some(src.as_path())) {
                Some(a) => (a, Some(Cursor::default())),
                None => return,
            },
        };
        let a = self.adapters[adapter].clone();
        let path = src.clone();
        let res = tokio::task::spawn_blocking(move || a.read(&path, cursor.as_ref())).await;
        match res {
            Ok(Ok(out)) => {
                let pending = {
                    let mut st = self.state.write().unwrap();
                    self.apply(&mut st, adapter, &src, out, true, now_ms())
                };
                self.publish(pending);
                self.usage_touch(&src);
            }
            Ok(Err(err)) => self.note_error(&format!("{}: {err:#}", src.display())),
            Err(err) => self.note_error(&format!("{}: {err}", src.display())),
        }
    }

    /// Probe liveness off the loop; the result comes back as [`Msg::Probed`].
    fn spawn_probe(self: &Arc<Self>, adapter: usize, tx: mpsc::UnboundedSender<Msg>) {
        if self.probing[adapter].swap(true, Ordering::AcqRel) {
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            let a = me.adapters[adapter].clone();
            let res = tokio::task::spawn_blocking(move || a.live()).await;
            me.probing[adapter].store(false, Ordering::Release);
            if let Ok(Some(list)) = res {
                let _ = tx.send(Msg::Probed(adapter, list));
            }
        });
    }
    /// Fetch the crates.io index off the loop; the result comes back as [`Msg::UpdateApplied`].
    fn spawn_update_check(self: &Arc<Self>, tx: mpsc::UnboundedSender<Msg>) {
        if self.updating.swap(true, Ordering::AcqRel) {
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            let res = tokio::task::spawn_blocking(crate::update::check).await;
            me.updating.store(false, Ordering::Release);
            if let Ok(info) = res {
                let _ = tx.send(Msg::UpdateApplied(info));
            }
        });
    }

    fn set_update(&self, info: crate::update::UpdateInfo) {
        if info.available {
            tracing::info!(
                "uniflo {} 可用（当前 {}），运行 `uniflo update` 升级",
                info.latest.as_deref().unwrap_or("?"),
                info.current
            );
        } else if let Some(err) = &info.error {
            tracing::debug!("update check failed: {err}");
        }
        if let Some(pre) = &info.latest_prerelease {
            tracing::info!("检测到预发布 {pre}（不保证稳定），如需试用：`uniflo update --pre`");
        }
        self.stats.lock().unwrap().update = Some(info);
    }

    /// Feed a source change directly (tests, external triggers).
    pub async fn notify_path(self: &Arc<Self>, path: PathBuf) {
        let src = self.adapters.iter().find_map(|a| a.source_for(&path)).unwrap_or(path);
        self.clone().process(src).await;
    }
}

fn spawn_ticker(tx: mpsc::UnboundedSender<Msg>, every: Duration, make: impl Fn() -> Msg + Send + 'static) {
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(every);
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        iv.tick().await;
        loop {
            iv.tick().await;
            if tx.send(make()).is_err() {
                break;
            }
        }
    });
}
