//! The adapter contract: everything a harness integration must provide.
//!
//! An adapter turns one harness's on-disk storage into [`Record`]s. The engine owns
//! discovery scheduling, cursors, status, caching and fan-out; adapters stay pure readers.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uniflo_schema::{Event, Status};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HarnessInfo {
    /// Stable id used in session keys, e.g. `claude`.
    pub id: &'static str,
    pub name: &'static str,
}

/// Session metadata contributed by a decoder. Later values win, except `title`,
/// where the highest rank wins (e.g. user-set title beats generated title).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MetaPatch {
    /// Native id of the parent session (sub-agents, forks).
    pub parent: Option<String>,
    pub title: Option<(u8, String)>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub started_at: Option<i64>,
    /// Last activity known from metadata (sources that do not replay every event).
    pub updated_at: Option<i64>,
}

impl MetaPatch {
    pub fn is_empty(&self) -> bool {
        *self == MetaPatch::default()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    Meta(MetaPatch),
    Event(Event),
}

/// Output of one read: records tagged with the native session id they belong to.
#[derive(Debug, Default)]
pub struct Batch {
    pub items: Vec<(String, Record)>,
    /// Discriminators the decoder did not recognise (coverage telemetry).
    pub unknown: Vec<String>,
    /// Lines that were complete but not valid JSON.
    pub bad_lines: u64,
    pub bytes: u64,
}

/// Resume point for a source. Serialized into the on-disk index cache.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Cursor {
    /// Bytes consumed (JSONL) or adapter-defined position.
    pub offset: u64,
    pub size: u64,
    pub mtime_ms: i64,
    /// Adapter-private resume state.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub state: serde_json::Value,
}

#[derive(Debug, Default)]
pub struct ReadOutput {
    pub cursor: Cursor,
    pub batch: Batch,
    /// Produced by a summary scan (head + tail), not a contiguous follow: events are
    /// a sample used for status/preview, never broadcast as live.
    pub summary: bool,
    /// The source shrank or was rewritten; previous state for it is invalid.
    pub reset: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryQuery {
    /// Only events with `pos < before`.
    pub before: Option<u64>,
    /// Soft target: whole source lines are returned, and a page reaches back to where decoding
    /// can start (`LineDecoder::page_start`), so the result may exceed it.
    pub limit: usize,
}

/// A running harness process attached to a session.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveSession {
    pub id: String,
    pub pid: u32,
    /// Only set when the harness publishes a trustworthy status.
    pub status: Option<Status>,
}

pub trait Adapter: Send + Sync + 'static {
    fn info(&self) -> HarnessInfo;

    /// Existing storage roots to watch recursively.
    fn roots(&self) -> Vec<PathBuf>;

    /// Map a changed filesystem path to the source it belongs to (e.g. `x.db-wal` → `x.db`).
    fn source_for(&self, path: &Path) -> Option<PathBuf>;

    /// All current sources.
    fn discover(&self) -> Vec<PathBuf>;

    /// `cursor == None`: cheap summary scan. `Some`: continue from the cursor to the end.
    fn read(&self, src: &Path, cursor: Option<&Cursor>) -> Result<ReadOutput>;

    /// Newest events of one session (chronological order), paging backwards with `before`.
    fn history(&self, src: &Path, session_id: &str, q: &HistoryQuery) -> Result<Vec<Event>>;

    /// Every record of `src` from its very beginning, in source order, fed to `sink` with its
    /// native session id; returns a cursor [`Adapter::read`] continues from. For consumers
    /// that need complete history (the usage ledger), never the head+tail sample.
    /// `sessions` are the native ids the engine knows in `src`.
    ///
    /// The default takes a summary cursor first, then replays each session's full
    /// [`Adapter::history`]: events after the cursor arrive again on the next read with the
    /// same ids, which consumers treat as replacements.
    fn read_all(&self, src: &Path, sessions: &[String], sink: &mut dyn FnMut(&str, Record)) -> Result<Cursor> {
        let out = self.read(src, None)?;
        for (id, r) in out.batch.items {
            if let Record::Meta(m) = r {
                sink(&id, Record::Meta(m));
            }
        }
        for id in sessions {
            for e in self.history(src, id, &HistoryQuery { before: None, limit: usize::MAX / 2 })? {
                sink(id, Record::Event(e));
            }
        }
        Ok(out.cursor)
    }

    /// Cheap check whether `src` moved past `cursor` (hot polling, warm restarts).
    fn changed(&self, src: &Path, cursor: &Cursor) -> bool {
        match std::fs::metadata(src) {
            Ok(md) => md.len() != cursor.size || crate::util::file_mtime_ms(&md) != cursor.mtime_ms,
            Err(_) => true,
        }
    }

    /// Live process registry; `None` when the harness has none.
    fn live(&self) -> Option<Vec<LiveSession>> {
        None
    }

    /// Directories whose changes should trigger [`Adapter::live`].
    fn live_roots(&self) -> Vec<PathBuf> {
        Vec::new()
    }

    /// What a user-confirmed cleanup of session `id` moves to the trash: its transcript plus
    /// files only it owns (e.g. its sub-agent directory); missing paths are skipped. `None`:
    /// the harness does not support cleanup (databases, files shared by sessions). Helpers in
    /// [`crate::cleanup::targets`].
    fn cleanup_targets(&self, _src: &Path, _id: &str) -> Option<Vec<PathBuf>> {
        None
    }
}

/// Recursively collect files under `root` (bounded depth) accepted by `keep`.
pub fn walk(root: &Path, max_depth: usize, keep: &dyn Fn(&Path) -> bool, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(root) else {
        return;
    };
    for ent in rd.flatten() {
        let Ok(ft) = ent.file_type() else { continue };
        let p = ent.path();
        if ft.is_dir() {
            if max_depth > 0 {
                walk(&p, max_depth - 1, keep, out);
            }
        } else if ft.is_file() && keep(&p) {
            out.push(p);
        }
    }
}
