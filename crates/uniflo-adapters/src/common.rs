//! Helpers shared by several adapters.

use std::path::{Path, PathBuf};

pub fn under_any(p: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|r| p.starts_with(r))
}

/// Provider stop reasons that end the agent's turn (as opposed to `tool_use`).
pub fn ends_turn_reason(r: &str) -> bool {
    matches!(r, "end_turn" | "stop_sequence" | "stop" | "length" | "refusal")
}

/// `2026-08-29T15-54-05-510Z_<id>` → `<id>` (Pi-style file stems).
pub fn after_ts_prefix(stem: &str) -> Option<&str> {
    let (ts, id) = stem.split_once('_')?;
    (ts.len() >= 20 && ts.as_bytes()[4] == b'-' && ts.contains('T') && !id.is_empty()).then_some(id)
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
    fn ts_prefixed_stems() {
        assert_eq!(after_ts_prefix("2026-08-29T15-54-05-510Z_01a04e3a-6746"), Some("01a04e3a-6746"));
        assert_eq!(after_ts_prefix("2026-09-06T06-41-13-422Z_pad_mtpfre9k_w2uu8q"), Some("pad_mtpfre9k_w2uu8q"));
        assert_eq!(after_ts_prefix("SpecAxis"), None);
        assert_eq!(after_ts_prefix("agent_x"), None);
    }
}
