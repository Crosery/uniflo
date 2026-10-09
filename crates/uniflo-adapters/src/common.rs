//! Helpers shared by several adapters.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use uniflo_core::util::file_mtime_ms;
use uniflo_core::{
    Adapter, Cursor, HarnessInfo, HistoryQuery, JsonlAdapter, LineDecoder, LiveSession, MetaPatch, ReadOutput, Record,
};
use uniflo_schema::Event;

pub fn under_any(p: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|r| p.starts_with(r))
}

/// Provider stop reasons that end the agent's turn (as opposed to `tool_use`).
pub fn ends_turn_reason(r: &str) -> bool {
    matches!(r, "end_turn" | "stop_sequence" | "stop" | "length" | "refusal")
}

/// A harness-reported step amount. Only a positive finite number counts: subscription and
/// free channels write 0, which says nothing about the API-equivalent cost.
pub fn reported_cost(v: Option<&serde_json::Value>) -> Option<f64> {
    v.and_then(serde_json::Value::as_f64).filter(|c| c.is_finite() && *c > 0.0)
}

/// `output` per the usage contract includes thinking. A harness whose `output` excludes it
/// shows `reasoning > output` (impossible when included), so it is added back then.
pub fn output_with_reasoning(output: u64, reasoning: u64) -> u64 {
    if reasoning > output { output + reasoning } else { output }
}

/// `2026-08-29T15-54-05-510Z_<id>` → `<id>` (Pi-style file stems).
pub fn after_ts_prefix(stem: &str) -> Option<&str> {
    let (ts, id) = stem.split_once('_')?;
    (ts.len() >= 20 && ts.as_bytes()[4] == b'-' && ts.contains('T') && !id.is_empty()).then_some(id)
}

/// `(mtime ms, size)` per file, `(0, 0)` when missing: a cheap change signature.
pub type FileSig = Vec<(i64, u64)>;

pub fn file_sig(paths: &[PathBuf]) -> FileSig {
    paths.iter().map(|p| std::fs::metadata(p).map_or((0, 0), |m| (file_mtime_ms(&m), m.len()))).collect()
}

/// A JSONL transcript whose session metadata lives in sibling files rewritten in place
/// (title, cwd, per-turn usage). [`WithSidecars`] re-reads them after every read, so a
/// change to a sibling alone still lands.
pub trait Sidecar: LineDecoder {
    /// The transcript a changed sibling file belongs to.
    fn transcript_of(&self, path: &Path) -> Option<PathBuf>;
    /// Siblings of `src` whose change means `src` must be read again.
    fn sidecars(&self, src: &Path) -> Vec<PathBuf>;
    /// Records derived from the siblings after a read of `src`. Called only once the session
    /// has events: a shell without conversation stays unlisted.
    fn sidecar(&self, src: &Path, state: &mut Self::State) -> Vec<Record>;
}

pub struct WithSidecars<D: Sidecar> {
    pub inner: JsonlAdapter<D>,
}

/// Cursor state of [`WithSidecars`]: the decoder's own state plus the sibling signature.
#[derive(Serialize, Deserialize)]
struct SideState {
    d: Value,
    #[serde(default)]
    sig: FileSig,
    /// The session has produced events (it is listed).
    #[serde(default)]
    content: bool,
    /// Metadata read while there were no events yet, sent with the first ones.
    #[serde(default)]
    held: Held,
}

/// [`MetaPatch`] merged the way the engine applies patches.
#[derive(Default, Serialize, Deserialize)]
struct Held {
    parent: Option<String>,
    title: Option<(u8, String)>,
    cwd: Option<String>,
    model: Option<String>,
    started_at: Option<i64>,
    updated_at: Option<i64>,
}

impl Held {
    fn add(&mut self, p: &MetaPatch) {
        if p.parent.is_some() {
            self.parent.clone_from(&p.parent);
        }
        if let Some((rank, t)) = &p.title
            && self.title.as_ref().is_none_or(|(r, _)| rank >= r)
        {
            self.title = Some((*rank, t.clone()));
        }
        if p.cwd.is_some() {
            self.cwd.clone_from(&p.cwd);
        }
        if p.model.is_some() {
            self.model.clone_from(&p.model);
        }
        if let Some(t) = p.started_at.filter(|t| *t > 0) {
            self.started_at = Some(self.started_at.map_or(t, |x| x.min(t)));
        }
        if let Some(t) = p.updated_at {
            self.updated_at = Some(self.updated_at.map_or(t, |x| x.max(t)));
        }
    }

    fn into_patch(self) -> MetaPatch {
        let Held { parent, title, cwd, model, started_at, updated_at } = self;
        MetaPatch { parent, title, cwd, model, started_at, updated_at }
    }
}

impl<D: Sidecar> WithSidecars<D> {
    pub fn new(decoder: D) -> Self {
        WithSidecars { inner: JsonlAdapter::new(decoder) }
    }

    fn split(c: &Cursor) -> (Cursor, SideState) {
        let side = serde_json::from_value::<SideState>(c.state.clone()).unwrap_or(SideState {
            d: Value::Null,
            sig: Vec::new(),
            content: false,
            held: Held::default(),
        });
        (Cursor { state: side.d.clone(), ..c.clone() }, side)
    }

    /// Append sidecar records for `id` when they may differ from what was sent, then wrap the
    /// inner cursor.
    fn finish(
        &self,
        src: &Path,
        cursor: &mut Cursor,
        content: bool,
        held: Held,
        emit: Option<&[(i64, u64)]>,
        push: &mut dyn FnMut(&str, Record),
    ) {
        let sig = file_sig(&self.inner.decoder.sidecars(src));
        let mut st: D::State = serde_json::from_value(cursor.state.clone()).unwrap_or_default();
        if content && emit.is_none_or(|prev| prev != sig.as_slice()) {
            let id = self.inner.decoder.identify(src).map(|s| s.id).unwrap_or_default();
            for r in self.inner.decoder.sidecar(src, &mut st) {
                push(&id, r);
            }
        }
        let d = serde_json::to_value(&st).unwrap_or(Value::Null);
        cursor.state = serde_json::to_value(SideState { d, sig, content, held }).unwrap_or(Value::Null);
    }
}

impl<D: Sidecar> Adapter for WithSidecars<D> {
    fn info(&self) -> HarnessInfo {
        self.inner.info()
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.inner.roots()
    }

    fn source_for(&self, path: &Path) -> Option<PathBuf> {
        self.inner.source_for(path).or_else(|| self.inner.decoder.transcript_of(path))
    }

    fn discover(&self) -> Vec<PathBuf> {
        self.inner.discover()
    }

    fn read(&self, src: &Path, cursor: Option<&Cursor>) -> Result<ReadOutput> {
        let mut split = cursor.map(Self::split);
        let mut out = self.inner.read(src, split.as_ref().map(|(c, _)| c))?;
        let had = split.as_ref().is_some_and(|(_, s)| s.content) && !out.reset;
        let content = had || out.batch.items.iter().any(|(_, r)| matches!(r, Record::Event(_)));
        let mut held =
            split.as_mut().filter(|_| !out.reset).map(|(_, s)| std::mem::take(&mut s.held)).unwrap_or_default();
        if !content {
            // Metadata alone (headers, config records) would list an empty session; keep it
            // for when the conversation starts.
            for (_, r) in out.batch.items.drain(..) {
                if let Record::Meta(m) = r {
                    held.add(&m);
                }
            }
        } else if !had {
            let early = std::mem::take(&mut held).into_patch();
            if !early.is_empty()
                && let Some(id) = self.inner.decoder.identify(src).map(|s| s.id)
            {
                out.batch.items.insert(0, (id, Record::Meta(early)));
            }
        }
        // A plain follow re-sends sidecar records only when a sibling changed.
        let prev = split.as_ref().filter(|_| had && !out.summary).map(|(_, s)| s.sig.as_slice());
        let mut extra = Vec::new();
        self.finish(src, &mut out.cursor, content, held, prev, &mut |id, r| extra.push((id.to_owned(), r)));
        out.batch.items.extend(extra);
        Ok(out)
    }

    fn history(&self, src: &Path, session_id: &str, q: &HistoryQuery) -> Result<Vec<Event>> {
        self.inner.history(src, session_id, q)
    }

    fn read_all(&self, src: &Path, sessions: &[String], sink: &mut dyn FnMut(&str, Record)) -> Result<Cursor> {
        let mut content = false;
        let mut held: Vec<(String, Record)> = Vec::new();
        let mut cursor = self.inner.read_all(src, sessions, &mut |id, r| {
            if !content && matches!(r, Record::Meta(_)) {
                held.push((id.to_owned(), r));
                return;
            }
            if !content {
                content = true;
                for (hid, h) in held.drain(..) {
                    sink(&hid, h);
                }
            }
            sink(id, r)
        })?;
        self.finish(src, &mut cursor, content, Held::default(), None, sink);
        Ok(cursor)
    }

    fn changed(&self, src: &Path, cursor: &Cursor) -> bool {
        let (inner, side) = Self::split(cursor);
        self.inner.changed(src, &inner) || side.sig != file_sig(&self.inner.decoder.sidecars(src))
    }

    fn live(&self) -> Option<Vec<LiveSession>> {
        self.inner.live()
    }

    fn live_roots(&self) -> Vec<PathBuf> {
        self.inner.live_roots()
    }
}

#[cfg(test)]
pub mod testkit {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use uniflo_core::status::StatusTracker;
    use uniflo_core::util::preview;
    use uniflo_core::{Adapter, Record};
    use uniflo_schema::{Body, Event, Status};

    pub struct Fixture {
        dir: tempfile::TempDir,
    }

    #[derive(Debug, Default, Clone)]
    pub struct Meta {
        pub parent: Option<String>,
        pub title: Option<String>,
        pub title_rank: u8,
        pub cwd: Option<String>,
        pub model: Option<String>,
        pub started_at: Option<i64>,
    }

    /// One session's view of an adapter read, merged like the engine does.
    #[derive(Debug, Default, Clone)]
    pub struct Indexed {
        pub id: String,
        pub events: Vec<Event>,
        pub meta: Meta,
        pub preview: Option<String>,
        pub unknown: Vec<String>,
        pub bad_lines: u64,
    }

    impl Indexed {
        pub fn status(&self) -> Status {
            let mut t = StatusTracker::default();
            for e in &self.events {
                t.observe(e);
            }
            t.status
        }
    }

    impl Fixture {
        pub fn new() -> Self {
            Fixture { dir: tempfile::tempdir().unwrap() }
        }

        pub fn root(&self) -> &Path {
            self.dir.path()
        }

        pub fn write(&self, rel: &str, content: &str) -> PathBuf {
            let p = self.dir.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, content).unwrap();
            p
        }

        pub fn append(&self, p: &Path, content: &str) {
            use std::io::Write;
            std::fs::OpenOptions::new().append(true).open(p).unwrap().write_all(content.as_bytes()).unwrap();
        }

        /// Summary-read one single-session source.
        pub fn index(&self, a: &dyn Adapter, p: &Path) -> Indexed {
            let mut all = self.index_all(a, p);
            assert_eq!(all.len(), 1, "expected one session, got {:?}", all.keys().collect::<Vec<_>>());
            all.pop_first().unwrap().1
        }

        /// Summary-read a source, grouped by native session id.
        pub fn index_all(&self, a: &dyn Adapter, p: &Path) -> BTreeMap<String, Indexed> {
            let out = a.read(p, None).expect("read");
            group(out.batch)
        }
    }

    pub fn group(batch: uniflo_core::adapter::Batch) -> BTreeMap<String, Indexed> {
        let mut m: BTreeMap<String, Indexed> = BTreeMap::new();
        for (id, rec) in batch.items {
            let ix = m.entry(id.clone()).or_insert_with(|| Indexed { id, ..Default::default() });
            match rec {
                Record::Meta(p) => {
                    if p.parent.is_some() {
                        ix.meta.parent = p.parent;
                    }
                    if let Some((rank, t)) = p.title
                        && rank >= ix.meta.title_rank
                    {
                        ix.meta.title_rank = rank;
                        ix.meta.title = Some(t);
                    }
                    if p.cwd.is_some() {
                        ix.meta.cwd = p.cwd;
                    }
                    if p.model.is_some() {
                        ix.meta.model = p.model;
                    }
                    if p.started_at.is_some() {
                        ix.meta.started_at = p.started_at;
                    }
                }
                Record::Event(e) => {
                    if ix.preview.is_none()
                        && let Body::UserMessage { text, synthetic: false } = &e.body
                    {
                        ix.preview = Some(preview(text, 160));
                    }
                    ix.events.push(e);
                }
            }
        }
        if let Some(first) = m.values_mut().next() {
            first.unknown = batch.unknown;
            first.bad_lines = batch.bad_lines;
        }
        m
    }

    pub fn kinds(evs: &[Event]) -> Vec<&'static str> {
        evs.iter().map(|e| e.body.kind()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_helpers() {
        use serde_json::json;
        assert_eq!(reported_cost(Some(&json!(0.25))), Some(0.25));
        assert_eq!(reported_cost(Some(&json!(0))), None, "0 = not reported");
        assert_eq!(reported_cost(Some(&json!("1"))), None);
        assert_eq!(reported_cost(None), None);
        assert_eq!(output_with_reasoning(100, 40), 100, "already included");
        assert_eq!(output_with_reasoning(10, 40), 50, "provably excluded");
    }

    #[test]
    fn ts_prefixed_stems() {
        assert_eq!(after_ts_prefix("2026-08-29T15-54-05-510Z_01a04e3a-6746"), Some("01a04e3a-6746"));
        assert_eq!(after_ts_prefix("2026-09-06T06-41-13-422Z_pad_mtpfre9k_w2uu8q"), Some("pad_mtpfre9k_w2uu8q"));
        assert_eq!(after_ts_prefix("SpecAxis"), None);
        assert_eq!(after_ts_prefix("agent_x"), None);
    }
}
