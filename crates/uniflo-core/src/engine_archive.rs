//! Engine side of session cleanup: archived sessions (listed like any other, served from
//! Uniflo's compact archive), tombstone checks on source reads, and the swap between a
//! session's source and its archive in both directions.

use super::{Engine, Entry, Pending, SourceState, State};
use crate::adapter::{Adapter, HistoryQuery, Record};
use crate::archive::{ArchiveStore, Archived, compact};
use crate::status::StatusTracker;
use crate::usage::{Ledger, Names, Phase, SourceLedger};
use crate::util::now_ms;
use anyhow::{Result, anyhow};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use uniflo_schema::{Body, Event, Session, Status};

/// What a cleanup needs to know about one session.
pub(crate) struct Item {
    pub session: Session,
    pub adapter: usize,
    pub src: PathBuf,
    pub archived: bool,
}

pub(crate) struct CleanupView {
    pub sessions: HashMap<String, Item>,
    /// Every source with the session keys it holds.
    pub sources: Vec<(PathBuf, Vec<String>)>,
}

/// State taken out by [`Engine::retire`], put back by [`Engine::unretire`].
#[derive(Default)]
pub(crate) struct Retired {
    targets: Vec<PathBuf>,
    sources: Vec<(PathBuf, SourceState)>,
    entries: Vec<Entry>,
    ledgers: Vec<(PathBuf, SourceLedger)>,
    archives: Vec<(String, PathBuf)>,
}

impl Engine {
    /// Uniflo's archive of cleaned-up sessions (`None` without a data directory).
    pub fn archive(&self) -> Option<&Arc<ArchiveStore>> {
        self.archive.as_ref()
    }

    pub fn adapters(&self) -> &[Arc<dyn Adapter>] {
        &self.adapters
    }

    pub(super) fn tombstoned(&self, src: &Path) -> bool {
        self.archive.as_ref().is_some_and(|a| a.tombstoned(src).is_some())
    }

    /// Whether `src` may be read as a source (blocking; see [`ArchiveStore::admit`]).
    pub(super) fn admit(&self, src: &Path) -> bool {
        self.archive.as_ref().is_none_or(|a| a.admit(src))
    }

    fn archived_entry(&self, store: &ArchiveStore, a: &Archived) -> Option<Entry> {
        let adapter = self.adapters.iter().position(|x| x.info().id == a.session.harness)?;
        let path = store.dir().join(&a.file);
        let mut session = a.session.clone();
        session.archived = true;
        session.source = path.display().to_string();
        session.pid = None;
        let tracker = StatusTracker {
            status: Status::Idle,
            since: session.status_since,
            reason: "archived".into(),
            last: session.updated_at,
        };
        let mut e = Entry {
            session,
            tracker,
            title_rank: u8::MAX,
            adapter,
            src: path.clone(),
            live_pid: None,
            live_status: None,
            archive: Some(path),
        };
        e.refresh(now_ms(), self.windows());
        Some(e)
    }

    /// Archived sessions that no source holds (end of `index()`).
    pub(super) fn load_archived(&self, st: &mut State) {
        let Some(store) = &self.archive else { return };
        for a in store.entries() {
            if st.sessions.contains_key(&a.session.key) {
                continue;
            }
            if let Some(e) = self.archived_entry(store, &a) {
                st.sessions.insert(a.session.key.clone(), e);
            }
        }
    }

    /// A source shows up again for an archived key (restored from the trash): the source wins.
    pub(super) fn unarchive(&self, entry: &Entry) {
        if let Some(p) = &entry.archive {
            let mut u = self.usage.write().unwrap();
            if u.remove_source(p).is_some() {
                u.dirty = true;
            }
        }
    }

    pub(super) fn archived_history(path: &Path, q: &HistoryQuery) -> Result<Vec<Event>> {
        let (_, mut evs) = compact::read(path)?;
        evs.retain(|e| q.before.is_none_or(|b| e.pos.unwrap_or(0) < b));
        let skip = evs.len().saturating_sub(q.limit.max(1));
        Ok(evs.split_off(skip))
    }

    pub(crate) fn cleanup_view(&self) -> CleanupView {
        let st = self.state.read().unwrap();
        CleanupView {
            sessions: st
                .sessions
                .iter()
                .map(|(k, e)| {
                    let it = Item {
                        session: e.session.clone(),
                        adapter: e.adapter,
                        src: e.src.clone(),
                        archived: e.archive.is_some(),
                    };
                    (k.clone(), it)
                })
                .collect(),
            sources: st.sources.iter().map(|(p, s)| (p.clone(), s.keys.clone())).collect(),
        }
    }

    /// The session snapshot and its complete, deduplicated events from a full read of its source,
    /// with each usage event's model resolved as the usage ledger resolves it (so an archive
    /// prices every step exactly like the source did). Blocking.
    pub(crate) fn archive_material(&self, key: &str) -> Result<(Session, Vec<Event>)> {
        let (adapter, src, id, session) = {
            let st = self.state.read().unwrap();
            let e = st.sessions.get(key).ok_or_else(|| anyhow!("unknown session {key}"))?;
            (self.adapters[e.adapter].clone(), e.src.clone(), e.session.id.clone(), e.session.clone())
        };
        let mut recs = Vec::new();
        adapter.read_all(&src, std::slice::from_ref(&id), &mut |sid, r| {
            if sid == id {
                recs.push(r);
            }
        })?;
        let p = self.pricing.current();
        let mut ledger = Ledger::default();
        let mut names = Names::default();
        for r in &recs {
            ledger.apply(r, &mut names, &p);
        }
        let models: HashMap<&str, Arc<str>> =
            ledger.steps.iter().filter_map(|s| Some((&*s.event, s.model.clone()?))).collect();
        let mut events = crate::jsonl::dedupe_events(recs);
        for e in &mut events {
            if let Body::Usage(u) = &mut e.body
                && u.model.is_none()
            {
                u.model = models.get(e.id.as_str()).map(|m| m.to_string());
            }
        }
        Ok((session, events))
    }

    /// Swap sources for their archives before the files move: tombstones first (so an in-flight
    /// read of a moving file is dropped, see `apply()`), then sources out, archived entries and
    /// archive-built usage ledgers in. `events` are the archived events per key.
    pub(crate) fn retire(
        &self,
        archived: &[Archived],
        events: &HashMap<String, Vec<Event>>,
        targets: &[PathBuf],
        root: &str,
    ) -> Retired {
        let Some(store) = self.archive.clone() else { return Retired::default() };
        store.bury(targets, root, now_ms());
        let mut r = Retired { targets: targets.to_vec(), ..Default::default() };
        let p = self.pricing.current();
        let generation = self.pricing.generation();
        {
            let mut st = self.state.write().unwrap();
            let gone: Vec<PathBuf> =
                st.sources.keys().filter(|s| targets.iter().any(|t| s.starts_with(t))).cloned().collect();
            for s in gone {
                if let Some(state) = st.sources.remove(&s) {
                    r.sources.push((s, state));
                }
            }
            let mut u = self.usage.write().unwrap();
            for (s, _) in &r.sources {
                if let Some(l) = u.remove_source(s) {
                    r.ledgers.push((s.clone(), l));
                }
            }
            for a in archived {
                let key = a.session.key.clone();
                let Some(entry) = self.archived_entry(&store, a) else { continue };
                let path = store.dir().join(&a.file);
                if self.opts.usage {
                    let mut l = Ledger::priced(generation);
                    let mut names = Names::default();
                    for e in events.get(&key).into_iter().flatten() {
                        l.apply(&Record::Event(e.clone()), &mut names, &p);
                    }
                    let harness = a.session.harness.clone();
                    let ledgers = HashMap::from([(key.clone(), l)]);
                    let s =
                        SourceLedger { adapter: entry.adapter, harness, cursor: None, phase: Phase::Ready, ledgers };
                    u.put_source(path.clone(), s);
                }
                if let Some(old) = st.sessions.insert(key.clone(), entry) {
                    r.entries.push(old);
                }
                r.archives.push((key, path));
            }
            u.dirty = true;
        }
        self.dirty.store(true, Ordering::Relaxed);
        let keys: Vec<String> = r.archives.iter().map(|(k, _)| k.clone()).collect();
        self.republish(&keys);
        r
    }

    /// Undo [`Engine::retire`] (nothing was moved).
    pub(crate) fn unretire(&self, r: Retired) {
        if let Some(store) = &self.archive {
            store.unbury(&r.targets);
        }
        {
            let mut st = self.state.write().unwrap();
            let mut u = self.usage.write().unwrap();
            for (k, path) in &r.archives {
                u.remove_source(path);
                st.sessions.remove(k);
            }
            for (p, l) in r.ledgers {
                u.put_source(p, l);
            }
            for (p, s) in r.sources {
                st.sources.insert(p, s);
            }
            for e in r.entries {
                st.sessions.insert(e.session.key.clone(), e);
            }
            u.dirty = true;
        }
        self.dirty.store(true, Ordering::Relaxed);
        let keys: Vec<String> = r.archives.into_iter().map(|(k, _)| k).collect();
        self.republish(&keys);
    }

    /// Only part of the targets moved: lift the tombstones of what stayed, so those files are
    /// indexed again as sources (they win over the archive).
    pub(crate) fn retire_partial(&self, stayed: &[PathBuf]) {
        if let Some(store) = &self.archive {
            store.unbury(stayed);
        }
    }

    /// Drop archived sessions whose archives were deleted (`removed` envelopes).
    pub(crate) fn forget_archived(&self, keys: &[String]) {
        let mut out = Vec::new();
        {
            let mut st = self.state.write().unwrap();
            let mut u = self.usage.write().unwrap();
            for k in keys {
                if let Some(e) = st.sessions.get(k).filter(|e| e.archive.is_some()) {
                    if let Some(p) = &e.archive {
                        u.remove_source(p);
                    }
                    st.sessions.remove(k);
                    out.push(Pending::Removed(k.clone()));
                }
            }
            u.dirty |= !out.is_empty();
        }
        self.publish(out);
    }

    /// Ledgers of archived sessions, built from their archives (usage restore at startup).
    pub(super) fn archived_ledgers(&self) -> Vec<(PathBuf, SourceLedger, String)> {
        let list: Vec<(String, usize, String, PathBuf)> = {
            let st = self.state.read().unwrap();
            st.sessions
                .values()
                .filter_map(|e| Some((e.session.key.clone(), e.adapter, e.session.harness.clone(), e.archive.clone()?)))
                .collect()
        };
        let p = self.pricing.current();
        let generation = self.pricing.generation();
        let mut out = Vec::new();
        for (key, adapter, harness, path) in list {
            let events = match compact::read(&path) {
                Ok((_, evs)) => evs,
                Err(err) => {
                    self.note_error(&format!("archive {}: {err:#}", path.display()));
                    continue;
                }
            };
            let mut l = Ledger::priced(generation);
            let mut names = Names::default();
            for e in events {
                l.apply(&Record::Event(e), &mut names, &p);
            }
            let ledgers = HashMap::from([(key.clone(), l)]);
            out.push((path, SourceLedger { adapter, harness, cursor: None, phase: Phase::Ready, ledgers }, key));
        }
        out
    }

    /// Recompute usage totals of `keys` and send their `session` envelopes.
    fn republish(&self, keys: &[String]) {
        self.refresh_sessions(keys, false);
        let st = self.state.read().unwrap();
        let out: Vec<Pending> =
            keys.iter().filter_map(|k| st.sessions.get(k)).map(|e| Pending::Session(e.session.clone())).collect();
        drop(st);
        self.publish(out);
    }
}
