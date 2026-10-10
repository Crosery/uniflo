//! Engine side of usage and cost: the background ledger builder, follow-ups, persistence,
//! price refresh / sync, and the queries the gateway and CLI serve.
//!
//! Startup never waits for it: `index()` stays head+tail, then `run()` restores cached
//! ledgers, follows what changed since, and reads every remaining source completely on a
//! few background threads (progress in [`UsageProgress`]). Afterwards each source change the
//! engine applies is followed from the ledger's own cursor, and the touched sessions'
//! `usage` totals go out as `session` envelopes.

use super::{Engine, Pending};
use crate::adapter::{HistoryQuery, MetaPatch, Record};
use crate::pricing::{Pricing, sync};
use crate::usage::{Ledger, Names, Phase, SourceLedger, report, store};
use crate::util::now_ms;
use anyhow::{Result, anyhow};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use uniflo_schema::{ModelUsage, PricingStatus, Session, SessionUsageDetail, UsageProgress, UsageReport, session_key};

/// Periodic catalog sync of a daemon: first run `delay` after start, then every `every`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PriceSync {
    pub delay: Duration,
    pub every: Duration,
}

impl Default for PriceSync {
    fn default() -> Self {
        PriceSync { delay: Duration::from_secs(60), every: Duration::from_secs(6 * 3600) }
    }
}

pub(super) enum UsageMsg {
    Changed(PathBuf),
    Removed(PathBuf),
}

const SAVE_EVERY: Duration = Duration::from_secs(300);

enum Follow {
    Done,
    Rebuild,
}

impl Engine {
    pub fn pricing(&self) -> &Pricing {
        &self.pricing
    }

    pub(super) fn usage_touch(&self, src: &Path) {
        if let Some(tx) = &*self.usage_tx.lock().unwrap() {
            let _ = tx.send(UsageMsg::Changed(src.to_path_buf()));
        }
    }

    pub(super) fn usage_gone(&self, src: &Path) {
        if let Some(tx) = &*self.usage_tx.lock().unwrap() {
            let _ = tx.send(UsageMsg::Removed(src.to_path_buf()));
        }
    }

    // ---------------------------------------------------------------- queries

    pub fn usage_progress(&self) -> UsageProgress {
        self.usage.read().unwrap().progress()
    }

    /// Aggregate `sessions` (filtered by the caller) per `q`.
    pub fn usage_report(&self, sessions: &[&Session], q: &report::Query) -> UsageReport {
        self.pricing.refresh();
        let p = self.pricing.current();
        report::report(&self.usage.read().unwrap(), sessions, q, &p, now_ms())
    }

    /// Model names seen in `sessions` with their catalog match and totals.
    pub fn models(&self, sessions: &[&Session]) -> Vec<ModelUsage> {
        self.pricing.refresh();
        let p = self.pricing.current();
        self.usage.read().unwrap().models(sessions.iter().map(|s| s.key.as_str()), &p)
    }

    pub fn pricing_status(&self) -> PricingStatus {
        self.pricing.refresh();
        self.pricing.status(now_ms())
    }

    /// Per-step usage of one session. Blocking: before its ledger is built the session's full
    /// history is read once through the adapter.
    pub fn session_usage(&self, key: &str) -> Result<SessionUsageDetail> {
        self.pricing.refresh();
        let p = self.pricing.current();
        let (src, model) = {
            let st = self.state.read().unwrap();
            let e = st.sessions.get(key).ok_or_else(|| anyhow!("unknown session {key}"))?;
            (e.src.clone(), e.session.model.clone())
        };
        {
            let u = self.usage.read().unwrap();
            let ready = u.source(&src).is_some_and(|s| s.phase == Phase::Ready);
            if let Some(l) = u.view(key).filter(|_| ready) {
                return Ok(l.detail(key, &p));
            }
        }
        let evs = self.history(key, &HistoryQuery { before: None, limit: usize::MAX / 2 })?;
        let mut l = Ledger::default();
        let mut names = Names::default();
        l.apply(&Record::Meta(MetaPatch { model, ..Default::default() }), &mut names, &p);
        for e in evs {
            l.apply(&Record::Event(e), &mut names, &p);
        }
        Ok(l.detail(key, &p))
    }

    // ---------------------------------------------------------------- building

    fn usage_tag(&self) -> String {
        format!("{}/usage{}", self.tag, crate::usage::LEDGER_VERSION)
    }

    fn usage_cache_path(&self) -> Option<PathBuf> {
        self.opts.cache_path.as_deref().map(store::path_for)
    }

    /// Load cached ledgers for known sources; everything else is queued for a full read
    /// (most recently active first). Returns the queue.
    fn usage_restore(&self) -> Vec<PathBuf> {
        let mut cached = match self.usage_cache_path() {
            Some(p) => store::load(&p, &self.usage_tag()),
            None => Default::default(),
        };
        let p = self.pricing.current();
        let generation = self.pricing.generation();
        let mut queue: Vec<(i64, PathBuf)> = Vec::new();
        let mut restored: Vec<String> = Vec::new();
        let archived = self.archived_ledgers();
        {
            let st = self.state.read().unwrap();
            let mut u = self.usage.write().unwrap();
            for (path, s) in &st.sources {
                let harness = self.adapters[s.adapter].info().id.to_owned();
                match cached.remove(path).filter(|c| c.0 == harness) {
                    Some((h, cursor, mut ledgers)) => {
                        for l in ledgers.values_mut() {
                            l.reprice(&p, generation);
                        }
                        restored.extend(ledgers.keys().cloned());
                        store::install(&mut u, path.clone(), s.adapter, h, cursor, ledgers);
                    }
                    None => {
                        let recent =
                            s.keys.iter().filter_map(|k| st.sessions.get(k)).map(|e| e.session.updated_at).max();
                        queue.push((recent.unwrap_or(0), path.clone()));
                        u.put_source(path.clone(), SourceLedger::queued(s.adapter, harness));
                    }
                }
            }
            for (path, ledger, key) in archived {
                u.put_source(path, ledger);
                restored.push(key);
            }
            u.started = true;
        }
        self.refresh_sessions(&restored, false);
        queue.sort_by_key(|a| std::cmp::Reverse(a.0));
        queue.into_iter().map(|(_, p)| p).collect()
    }

    /// Full read of one source into fresh ledgers, swapped in at the end.
    fn build_source(&self, path: &Path, publish: bool) {
        let known = {
            let st = self.state.read().unwrap();
            st.sources.get(path).map(|s| {
                let harness = self.adapters[s.adapter].info().id;
                let ids: Vec<String> =
                    s.keys.iter().filter_map(|k| st.sessions.get(k)).map(|e| e.session.id.clone()).collect();
                (s.adapter, harness, ids)
            })
        };
        let Some((adapter, harness, ids)) = known else {
            self.usage.write().unwrap().remove_source(path);
            return;
        };
        let generation = self.pricing.generation();
        let p = self.pricing.current();
        let mut ledgers: HashMap<String, Ledger> = HashMap::new();
        let mut names = Names::default();
        let res = self.adapters[adapter].read_all(path, &ids, &mut |id, rec| {
            ledgers.entry(session_key(harness, id)).or_default().apply(&rec, &mut names, &p);
        });
        let cursor = match res {
            Ok(c) => Some(c),
            Err(err) => {
                self.note_error(&format!("usage {}: {err:#}", path.display()));
                None
            }
        };
        let mut keys: Vec<String> = ledgers.keys().cloned().collect();
        keys.sort();
        for l in ledgers.values_mut() {
            l.priced_at = generation;
        }
        {
            let st = self.state.read().unwrap();
            if !st.sources.contains_key(path) {
                // Cleaned up while this read ran: the archive's ledger holds the session now.
                return;
            }
            let mut u = self.usage.write().unwrap();
            // Keys this source no longer has must drop their totals too.
            if let Some(old) = u.source(path) {
                keys.extend(old.ledgers.keys().filter(|k| !ledgers.contains_key(*k)).cloned().collect::<Vec<_>>());
            }
            let s = SourceLedger { adapter, harness: harness.to_owned(), cursor, phase: Phase::Ready, ledgers };
            u.put_source(path.to_path_buf(), s);
            u.dirty = true;
        }
        if self.pricing.generation() != generation {
            self.reprice(Some(&keys));
        }
        self.refresh_sessions(&keys, publish);
    }

    /// Full reads on a few background threads; each finished path is reported on `done`.
    fn spawn_builds(self: &Arc<Self>, queue: Vec<PathBuf>, done: mpsc::UnboundedSender<PathBuf>) {
        if queue.is_empty() {
            return;
        }
        let me = self.clone();
        std::thread::spawn(move || me.run_builds(queue, Some(done)));
    }

    fn run_builds(&self, queue: Vec<PathBuf>, done: Option<mpsc::UnboundedSender<PathBuf>>) {
        let threads = (self.opts.threads / 2).clamp(1, 8);
        let next = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..threads.min(queue.len()) {
                let (queue, next, done) = (&queue, &next, done.clone());
                s.spawn(move || {
                    while let Some(p) = queue.get(next.fetch_add(1, Ordering::Relaxed)) {
                        let publish = self.usage_progress().ready;
                        self.build_source(p, publish);
                        if let Some(tx) = &done {
                            let _ = tx.send(p.clone());
                        }
                    }
                });
            }
        });
    }

    /// Read what was appended since the ledger cursor (blocking).
    fn follow_source(&self, path: &Path) -> Follow {
        let known = {
            let u = self.usage.read().unwrap();
            match u.source(path) {
                Some(s) if s.phase == Phase::Queued => return Follow::Done,
                Some(s) => Some((s.adapter, s.harness.clone(), s.cursor.clone())),
                None => None,
            }
        };
        let Some((adapter, harness, Some(cursor))) = known else {
            return if self.state.read().unwrap().sources.contains_key(path) { Follow::Rebuild } else { Follow::Done };
        };
        let a = &self.adapters[adapter];
        if !path.exists() || !a.changed(path, &cursor) {
            return Follow::Done;
        }
        let out = match a.read(path, Some(&cursor)) {
            Ok(o) => o,
            Err(err) => {
                self.note_error(&format!("usage {}: {err:#}", path.display()));
                return Follow::Done;
            }
        };
        if out.summary || out.reset {
            return Follow::Rebuild;
        }
        let p = self.pricing.current();
        let generation = self.pricing.generation();
        let mut names = Names::default();
        let mut touched: HashSet<String> = HashSet::new();
        {
            let mut u = self.usage.write().unwrap();
            for (id, rec) in &out.batch.items {
                let key = session_key(&harness, id);
                let Some(l) = u.ledger_mut(path, &key, generation) else { break };
                if l.apply(rec, &mut names, &p) {
                    touched.insert(key);
                }
            }
            if let Some(s) = u.source_mut(path) {
                s.cursor = Some(out.cursor);
            }
            u.dirty |= !touched.is_empty();
        }
        let keys: Vec<String> = touched.into_iter().collect();
        self.refresh_sessions(&keys, true);
        Follow::Done
    }

    fn requeue(&self, path: &Path) -> bool {
        let st = self.state.read().unwrap();
        let Some(s) = st.sources.get(path) else { return false };
        let mut u = self.usage.write().unwrap();
        match u.source_mut(path) {
            Some(e) => e.phase = Phase::Queued,
            None => {
                let harness = self.adapters[s.adapter].info().id.to_owned();
                u.put_source(path.to_path_buf(), SourceLedger::queued(s.adapter, harness));
            }
        }
        true
    }

    fn usage_drop(&self, path: &Path) {
        let mut u = self.usage.write().unwrap();
        if u.remove_source(path).is_some() {
            u.dirty = true;
        }
    }

    /// Recompute step costs priced with an older catalog (all ledgers, or `keys`).
    fn reprice(&self, keys: Option<&[String]>) -> Vec<String> {
        let p = self.pricing.current();
        let generation = self.pricing.generation();
        let mut changed = Vec::new();
        let mut u = self.usage.write().unwrap();
        for (k, l) in u.ledgers_mut() {
            if l.priced_at != generation && keys.is_none_or(|ks| ks.contains(k)) {
                l.reprice(&p, generation);
                changed.push(k.clone());
            }
        }
        changed.sort();
        changed.dedup();
        changed
    }

    /// Write ledger totals into the session snapshots; `publish` sends `session` envelopes.
    pub(super) fn refresh_sessions(&self, keys: &[String], publish: bool) {
        if keys.is_empty() {
            return;
        }
        let p = self.pricing.current();
        let sums: Vec<(&String, _)> = {
            let u = self.usage.read().unwrap();
            keys.iter().map(|k| (k, u.view(k).and_then(|l| l.summary(&p)).map(Box::new))).collect()
        };
        let mut out = Vec::new();
        {
            let mut st = self.state.write().unwrap();
            for (k, sum) in sums {
                if let Some(e) = st.sessions.get_mut(k)
                    && e.session.usage != sum
                {
                    e.session.usage = sum;
                    if publish {
                        out.push(Pending::Session(e.session.clone()));
                    }
                }
            }
        }
        self.dirty.store(true, Ordering::Relaxed);
        self.publish(out);
    }

    /// Persist ledgers next to the index cache (no-op without a cache path).
    pub fn save_usage_cache(&self) -> Result<()> {
        let Some(path) = self.usage_cache_path() else { return Ok(()) };
        let u = self.usage.read().unwrap();
        store::save(&path, &self.usage_tag(), &u)?;
        drop(u);
        self.usage.write().unwrap().dirty = false;
        Ok(())
    }

    /// Blocking complete pass for one-shot callers (CLI without a daemon): restore, read
    /// every remaining source, follow the rest, persist.
    pub fn index_usage(&self) -> UsageProgress {
        if !self.opts.usage {
            return self.usage_progress();
        }
        let queue = self.usage_restore();
        self.run_builds(queue, None);
        let ready: Vec<PathBuf> = self.usage.read().unwrap().sources().map(|(p, _)| p.clone()).collect();
        let mut again = Vec::new();
        for p in ready {
            if let Follow::Rebuild = self.follow_source(&p)
                && self.requeue(&p)
            {
                again.push(p);
            }
        }
        self.run_builds(again, None);
        let _ = self.save_usage_cache();
        self.usage_progress()
    }

    // ---------------------------------------------------------------- loop

    /// Runs inside [`Engine::run`]: ledger builds and follow-ups, price refresh and sync.
    pub(super) async fn usage_loop(self: Arc<Self>) {
        let (tx, mut rx) = mpsc::unbounded_channel::<UsageMsg>();
        let (built_tx, mut built_rx) = mpsc::unbounded_channel::<PathBuf>();
        let mut pending: Vec<PathBuf> = Vec::new();
        if self.opts.usage {
            *self.usage_tx.lock().unwrap() = Some(tx);
            let me = self.clone();
            let queue = tokio::task::spawn_blocking(move || me.usage_restore()).await.unwrap_or_default();
            pending.extend(
                self.usage.read().unwrap().sources().filter(|(_, s)| s.phase == Phase::Ready).map(|(p, _)| p.clone()),
            );
            self.spawn_builds(queue, built_tx.clone());
        } else {
            drop(tx);
        }
        let mut generation = self.pricing.generation();
        let mut next_sync = self.opts.price_sync.map(|s| Instant::now() + s.delay);
        let syncing = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut saved = Instant::now();
        let mut was_ready = self.usage_progress().ready;
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            if !pending.is_empty() {
                let mut seen = HashSet::new();
                let batch: Vec<PathBuf> = pending.drain(..).filter(|p| seen.insert(p.clone())).collect();
                let me = self.clone();
                let rebuild = tokio::task::spawn_blocking(move || {
                    batch
                        .into_iter()
                        .filter(|p| matches!(me.follow_source(p), Follow::Rebuild) && me.requeue(p))
                        .collect()
                })
                .await
                .unwrap_or_default();
                self.spawn_builds(rebuild, built_tx.clone());
            }
            tokio::select! {
                m = rx.recv(), if self.opts.usage => match m {
                    Some(UsageMsg::Changed(p)) => pending.push(p),
                    Some(UsageMsg::Removed(p)) => self.usage_drop(&p),
                    None => {}
                },
                Some(p) = built_rx.recv() => pending.push(p),
                _ = tick.tick() => {
                    let me = self.clone();
                    let _ = tokio::task::spawn_blocking(move || me.pricing.refresh()).await;
                    let g = self.pricing.generation();
                    if g != generation {
                        generation = g;
                        let me = self.clone();
                        let _ = tokio::task::spawn_blocking(move || {
                            let keys = me.reprice(None);
                            me.refresh_sessions(&keys, false);
                        })
                        .await;
                    }
                    if let (Some(at), Some(every)) = (next_sync, self.opts.price_sync.map(|s| s.every))
                        && Instant::now() >= at
                        && !syncing.swap(true, Ordering::AcqRel)
                    {
                        next_sync = Some(Instant::now() + every);
                        let (me, flag) = (self.clone(), syncing.clone());
                        tokio::task::spawn_blocking(move || {
                            let r = me.pricing.sync_now(&sync::Sources::from_env());
                            if let Some(err) = r.error {
                                tracing::warn!("price sync: {err}");
                            }
                            flag.store(false, Ordering::Release);
                        });
                    }
                    let ready = self.usage_progress().ready;
                    let dirty = self.usage.read().unwrap().dirty;
                    if dirty && ((ready && !was_ready) || saved.elapsed() >= SAVE_EVERY) {
                        saved = Instant::now();
                        let me = self.clone();
                        let _ = tokio::task::spawn_blocking(move || me.save_usage_cache()).await;
                    }
                    was_ready = ready;
                }
            }
        }
    }
}
