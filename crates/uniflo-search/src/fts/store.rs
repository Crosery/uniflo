//! The index database: schema, format tag, per-session progress, event rows and the hot/cold
//! tier split (ADR-0011).
//!
//! `docs` holds one row per hot-tier event (`UNIQUE(sid, event)`, so an event id that comes back
//! replaces its row); `docs_fts` is an external-content FTS5 trigram index over `docs.text`. Rows
//! older than `hot_days` move to `blk` / `blk_data` / `blk_fts` (see [`crate::fts::cold`]); a
//! session lives wholly in one tier (`sess.cold`). `sess.done_upd` / `done_pos` record how far the
//! last complete backfill pass got, which is what lets a restart skip unchanged sessions.
//!
//! Row ids carry the session and kind ([`row_id`]), so ranking reads only the FTS index:
//! joining `docs` costs a page read per match (rows are text-heavy), ~0.7 s for 14k matches.
//! Cold rows keep the same id scheme, so promoting a session back to hot needs no renumbering.
//!
//! `docs_fts` is maintained by plain single-row statements, never triggers: a trigger needs a
//! statement journal, every statement journal is a savepoint, and FTS5 flushes its pending
//! terms into a new segment at each savepoint — one tiny segment plus merge work per event.
//!
//! `meta` also carries the running counters (`hot_rows`, `hot_tokens`, `cold_rows`,
//! `cold_tokens`, `cold_gen`) bm25's global average document length and cache invalidation are
//! computed from, kept up to date in the same transactions as the rows they describe.

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;
use uniflo_schema::{Body, Event, Session, clip};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS sess (
    sid      INTEGER PRIMARY KEY,
    key      TEXT NOT NULL UNIQUE,
    done_upd INTEGER,
    done_pos INTEGER,
    pinned   INTEGER NOT NULL DEFAULT 0,
    cold     INTEGER NOT NULL DEFAULT 0,
    fingerprint INTEGER,
    meta     TEXT
);
CREATE TABLE IF NOT EXISTS docs (
    id    INTEGER PRIMARY KEY,
    sid   INTEGER NOT NULL,
    event TEXT NOT NULL,
    ts    INTEGER NOT NULL,
    pos   INTEGER,
    text  TEXT NOT NULL,
    UNIQUE (sid, event)
);
CREATE VIRTUAL TABLE IF NOT EXISTS docs_fts USING fts5(
    text, content='docs', content_rowid='id', tokenize='trigram case_sensitive 0'
);
CREATE TABLE IF NOT EXISTS blk (
    bid   INTEGER PRIMARY KEY AUTOINCREMENT,
    sid   INTEGER NOT NULL,
    first INTEGER NOT NULL,
    last  INTEGER NOT NULL,
    rows  INTEGER NOT NULL,
    kinds INTEGER NOT NULL,
    tokens INTEGER NOT NULL,
    bytes INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS blk_sid ON blk (sid);
CREATE TABLE IF NOT EXISTS blk_data (bid INTEGER PRIMARY KEY, z BLOB NOT NULL);
CREATE VIRTUAL TABLE IF NOT EXISTS blk_fts USING fts5(
    text, content='', contentless_delete=1, detail=none, tokenize='trigram case_sensitive 0'
);
"#;

/// Tool arguments are cut to this many bytes before indexing.
pub const ARGS_MAX: usize = 2 * 1024;
/// Tool output is cut to this many bytes before indexing.
pub const OUTPUT_MAX: usize = 4 * 1024;
/// A cold block holds about this many bytes of text before it is cut (ADR-0011).
pub const COLD_BLOCK_MAX: usize = 64 * 1024;
pub const DAY_MS: i64 = 86_400_000;

/// Indexed kinds; the stored code is the position + 1.
pub const KINDS: [&str; 6] = ["user_message", "assistant_message", "reasoning", "tool_call", "tool_result", "system"];

pub fn kind_code(kind: &str) -> Option<i64> {
    KINDS.iter().position(|k| *k == kind).map(|i| i as i64 + 1)
}

const SEQ_BITS: u32 = 29;

/// `sid << 32 | seq << 3 | kind`: up to 2^29 rows per session, kinds 1..=6. Shared by both tiers
/// so promoting a session back to hot restores its rows under the same ids.
pub fn row_id(sid: i64, seq: i64, kind: i64) -> i64 {
    (sid << 32) | (seq << 3) | kind
}

pub fn sid_of(id: i64) -> i64 {
    id >> 32
}

/// Row ids of one session: `sid << 32 ..`, in insertion order.
pub fn id_range(sid: i64) -> std::ops::Range<i64> {
    sid << 32..(sid + 1) << 32
}

/// Width of a row-id range covering `rows` consecutive rows.
pub fn id_span(rows: i64) -> i64 {
    rows << 3
}

pub fn kind_of(id: i64) -> i64 {
    id & 7
}

/// Bit `kind` sets in a [`blk.kinds`] bitmask.
pub fn kind_bit(kind: i64) -> i64 {
    1 << kind
}

fn next_seq(c: &Connection, sid: i64) -> Result<i64> {
    let hot: Option<i64> = c
        .prepare_cached("SELECT max(id) FROM docs WHERE id >= ?1 AND id < ?2")?
        .query_row(params![sid << 32, (sid + 1) << 32], |r| r.get(0))?;
    let cold: Option<i64> =
        c.prepare_cached("SELECT max(last) FROM blk WHERE sid = ?1")?.query_row([sid], |r| r.get(0))?;
    let max = hot.into_iter().chain(cold).max();
    let seq = max.map_or(0, |m| ((m >> 3) & ((1 << SEQ_BITS) - 1)) + 1);
    anyhow::ensure!(seq < 1 << SEQ_BITS, "session {sid} has too many indexed events");
    Ok(seq)
}

pub fn kind_name(code: i64) -> &'static str {
    usize::try_from(code - 1).ok().and_then(|i| KINDS.get(i)).copied().unwrap_or("unknown")
}

/// Trigram token count for a text of `chars` characters (characters minus 2, floored at 0): the
/// same accounting FTS5's `docsize` uses for `detail=full`, kept for bm25's average document
/// length across both tiers.
pub fn token_count(text: &str) -> i64 {
    (text.chars().count() as i64 - 2).max(0)
}

/// `(kind code, text)` indexed for an event; `None` for kinds without searchable text.
/// The text may be empty (e.g. a `[redacted]` reasoning block): an existing row is then dropped.
pub fn doc(e: &Event) -> Option<(i64, String)> {
    let mut text = match &e.body {
        Body::UserMessage { text, .. } | Body::AssistantMessage { text, .. } => text.clone(),
        Body::Reasoning { text } if text.trim() == "[redacted]" => String::new(),
        Body::Reasoning { text } => text.clone(),
        Body::ToolCall { name, input, .. } => {
            let mut args = String::new();
            args_text(input, &mut args);
            clip(&mut args, ARGS_MAX);
            if args.is_empty() { name.clone() } else { format!("{name}\n{args}") }
        }
        Body::ToolResult { output, .. } => {
            let mut s = output.clone();
            clip(&mut s, OUTPUT_MAX);
            s
        }
        Body::System { subtype, .. } => subtype.clone(),
        Body::TurnStart {} | Body::TurnEnd { .. } | Body::Usage(_) => return None,
    };
    // The snippet markers must only ever come from the index.
    if text.contains(['\u{2}', '\u{3}']) {
        text = text.replace(['\u{2}', '\u{3}'], " ");
    }
    Some((kind_code(e.body.kind())?, text))
}

/// Argument values as plain text (keys and JSON syntax dropped), stopping past [`ARGS_MAX`].
fn args_text(v: &Value, out: &mut String) {
    if out.len() > ARGS_MAX {
        return;
    }
    let push = |out: &mut String, s: &str| {
        if !s.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(s);
        }
    };
    match v {
        Value::String(s) => push(out, s),
        Value::Array(a) => a.iter().for_each(|x| args_text(x, out)),
        Value::Object(o) => o.values().for_each(|x| args_text(x, out)),
        Value::Null => {}
        other => push(out, &other.to_string()),
    }
}

pub fn wal_paths(path: &Path) -> [PathBuf; 2] {
    let with = |suffix: &str| {
        let mut s = path.as_os_str().to_owned();
        s.push(suffix);
        PathBuf::from(s)
    };
    [with("-wal"), with("-shm")]
}

fn remove_files(path: &Path) {
    let _ = std::fs::remove_file(path);
    for p in wal_paths(path) {
        let _ = std::fs::remove_file(p);
    }
}

fn configure(c: &Connection) -> Result<()> {
    c.busy_timeout(Duration::from_secs(5))?;
    // Only takes effect on an empty database (before the first CREATE TABLE) or after a VACUUM;
    // harmless to re-issue on a database that already has it set. Must run before switching to
    // WAL below: SQLite silently ignores this pragma once WAL mode is already active, even on an
    // otherwise-empty database.
    c.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
    c.pragma_update(None, "journal_mode", "WAL")?;
    c.pragma_update(None, "synchronous", "NORMAL")?;
    // Segment merges re-read recent pages; 32 MB keeps them out of the file.
    c.pragma_update(None, "cache_size", -32_000)?;
    // A checkpointed WAL keeps its peak size unless capped.
    c.pragma_update(None, "journal_size_limit", 64 * 1024 * 1024)?;
    Ok(())
}

fn tag_of(c: &Connection) -> Result<Option<String>> {
    meta_get(c, "tag")
}

/// How [`open`] found the file: built from scratch (`discarded` tells apart "there was no file"
/// from "an existing one was thrown away", for [`OpenState::rebuilt`]'s pre-ADR-0011 meaning), the
/// same format but a different Uniflo version (the index is kept; see [`crate::fts::Fts`]'s
/// version refresh), or nothing changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenState {
    Fresh { discarded: bool },
    VersionChanged,
    Unchanged,
}

impl OpenState {
    /// Whether an *existing* file was discarded (bad tag or unreadable); `false` for a file that
    /// never existed — the `FtsStatus.rebuilt` contract predates tiering and only ever meant the
    /// former.
    pub fn rebuilt(self) -> bool {
        matches!(self, OpenState::Fresh { discarded: true })
    }

    pub fn is_fresh(self) -> bool {
        matches!(self, OpenState::Fresh { .. })
    }
}

/// Open the writer connection. A file with another tag (format or schema version changed) or one
/// that cannot be read is discarded and recreated. The Uniflo version lives outside the tag
/// (`meta.version`): a version-only change keeps the index (see [`OpenState::VersionChanged`]).
pub fn open(path: &Path, tag: &str) -> Result<(Connection, OpenState)> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let existed = path.exists();
    let mut discarded = false;
    if existed {
        let keep = Connection::open(path).ok().and_then(|c| tag_of(&c).ok().flatten()).is_some_and(|t| t == tag);
        if !keep {
            remove_files(path);
            discarded = true;
        }
    }
    let c = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
    configure(&c)?;
    c.execute_batch(SCHEMA).context("create full-text schema (needs SQLite FTS5 with the trigram tokenizer)")?;
    let fresh = !existed || discarded;
    let prev_version = if fresh { None } else { meta_get(&c, "version")? };
    meta_set(&c, "tag", tag)?;
    let version = env!("CARGO_PKG_VERSION");
    meta_set(&c, "version", version)?;
    let state = if fresh {
        OpenState::Fresh { discarded }
    } else if prev_version.as_deref() != Some(version) {
        OpenState::VersionChanged
    } else {
        OpenState::Unchanged
    };
    Ok((c, state))
}

pub fn open_reader(path: &Path) -> Result<Connection> {
    let c = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .with_context(|| format!("open {}", path.display()))?;
    c.busy_timeout(Duration::from_secs(2))?;
    // Multi-phrase bm25() reads its doclists twice (IDF pass, then the rows); the default 2 MB
    // cache drops them in between.
    c.pragma_update(None, "cache_size", -32_000)?;
    super::rank::register(&c)?;
    Ok(c)
}

/// Whether more than a quarter of the index's leaf pages lie outside its largest segment. A query
/// reads each segment's share of a term's postings separately, so on a cold cache every trigram
/// costs a seek per segment; merging into one segment makes the reads few and sequential.
pub fn scattered(c: &Connection) -> Result<bool> {
    // `pgno` is the leaf number shifted left once (the low bit flags a doclist index).
    let mut st = c.prepare("SELECT (max(pgno) >> 1) - (min(pgno) >> 1) + 1 FROM docs_fts_idx GROUP BY segid")?;
    let leaves: Vec<i64> = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    let (total, largest) = (leaves.iter().sum::<i64>(), leaves.iter().copied().max().unwrap_or(0));
    Ok(total - largest > total / 4)
}

/// One step of merging every segment into one, about `pages` leaf pages of work. The first step
/// (`start`) gathers all segments into one merge; later ones continue it. Returns whether the
/// step did any work (FTS5's documented `total_changes` test).
pub fn merge_step(c: &Connection, start: bool, pages: i64) -> Result<bool> {
    let before = c.total_changes();
    let n = if start { -pages } else { pages };
    c.prepare_cached("INSERT INTO docs_fts (docs_fts, rank) VALUES ('merge', ?1)")?.execute([n])?;
    Ok(c.total_changes() - before >= 2)
}

/// Fraction of the file's pages that are free (eligible for `incremental_vacuum`).
pub fn free_ratio(c: &Connection) -> Result<f64> {
    let free: i64 = c.pragma_query_value(None, "freelist_count", |r| r.get(0))?;
    let total: i64 = c.pragma_query_value(None, "page_count", |r| r.get(0))?;
    Ok(if total == 0 { 0.0 } else { free as f64 / total as f64 })
}

/// Reclaim up to `pages` free pages (`PRAGMA auto_vacuum=INCREMENTAL` must already be set).
pub fn incremental_vacuum(c: &Connection, pages: i64) -> Result<()> {
    c.pragma_update(None, "incremental_vacuum", pages)?;
    Ok(())
}

pub fn meta_get(c: &Connection, k: &str) -> Result<Option<String>> {
    Ok(c.query_row("SELECT v FROM meta WHERE k = ?1", [k], |r| r.get(0)).optional()?)
}

pub fn meta_set(c: &Connection, k: &str, v: &str) -> Result<()> {
    c.execute("INSERT OR REPLACE INTO meta (k, v) VALUES (?1, ?2)", params![k, v])?;
    Ok(())
}

fn meta_i64(c: &Connection, k: &str) -> Result<i64> {
    Ok(meta_get(c, k)?.and_then(|v| v.parse().ok()).unwrap_or(0))
}

fn bump_meta_i64(c: &Connection, k: &str, delta: i64) -> Result<()> {
    if delta == 0 {
        return Ok(());
    }
    let v = meta_i64(c, k)? + delta;
    meta_set(c, k, &v.to_string())
}

/// Row and token counts behind bm25's global average document length (`avgdl = (hot_tokens +
/// cold_tokens) / (hot_rows + cold_rows)`), maintained in the same transactions as the data.
#[derive(Debug, Clone, Copy, Default)]
pub struct Counters {
    pub hot_rows: i64,
    pub hot_tokens: i64,
    pub cold_rows: i64,
    pub cold_tokens: i64,
    /// Bumped whenever cold content changes; invalidates the cold verification cache.
    pub cold_gen: i64,
}

impl Counters {
    pub fn avgdl(&self) -> f64 {
        let rows = self.hot_rows + self.cold_rows;
        if rows == 0 { 1.0 } else { (self.hot_tokens + self.cold_tokens) as f64 / rows as f64 }
    }
}

pub fn counters(c: &Connection) -> Result<Counters> {
    Ok(Counters {
        hot_rows: meta_i64(c, "hot_rows")?,
        hot_tokens: meta_i64(c, "hot_tokens")?,
        cold_rows: meta_i64(c, "cold_rows")?,
        cold_tokens: meta_i64(c, "cold_tokens")?,
        cold_gen: meta_i64(c, "cold_gen")?,
    })
}

pub fn bump_hot(c: &Connection, d_rows: i64, d_tokens: i64) -> Result<()> {
    bump_meta_i64(c, "hot_rows", d_rows)?;
    bump_meta_i64(c, "hot_tokens", d_tokens)
}

pub fn bump_cold(c: &Connection, d_rows: i64, d_tokens: i64) -> Result<()> {
    bump_meta_i64(c, "cold_rows", d_rows)?;
    bump_meta_i64(c, "cold_tokens", d_tokens)?;
    bump_meta_i64(c, "cold_gen", 1)
}

#[derive(Debug, Clone, Default)]
pub struct SessRow {
    pub sid: i64,
    pub done_upd: Option<i64>,
    pub done_pos: Option<u64>,
    pub pinned: bool,
    pub cold: bool,
    /// Kept for pinned sessions only: the engine may not know them.
    pub meta: Option<Session>,
}

pub fn load_sessions(c: &Connection) -> Result<Vec<(String, SessRow)>> {
    let mut st = c.prepare("SELECT sid, key, done_upd, done_pos, pinned, cold, meta FROM sess")?;
    let rows = st.query_map([], |r| {
        let meta: Option<String> = r.get(6)?;
        Ok((
            r.get::<_, String>(1)?,
            SessRow {
                sid: r.get(0)?,
                done_upd: r.get(2)?,
                done_pos: r.get::<_, Option<i64>>(3)?.map(|p| p as u64),
                pinned: r.get::<_, i64>(4)? != 0,
                cold: r.get::<_, i64>(5)? != 0,
                meta: meta.and_then(|m| serde_json::from_str(&m).ok()),
            },
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Hot + cold event rows, from the running counters (cheap: no table scan).
pub fn count_docs(c: &Connection) -> Result<u64> {
    let Counters { hot_rows, cold_rows, .. } = counters(c)?;
    Ok((hot_rows + cold_rows).max(0) as u64)
}

pub fn insert_session(c: &Connection, key: &str) -> Result<i64> {
    c.prepare_cached("INSERT INTO sess (key) VALUES (?1) ON CONFLICT (key) DO NOTHING")?.execute([key])?;
    Ok(c.prepare_cached("SELECT sid FROM sess WHERE key = ?1")?.query_row([key], |r| r.get(0))?)
}

pub fn finish_pass(c: &Connection, sid: i64, upd: i64, pos: Option<u64>) -> Result<()> {
    c.prepare_cached("UPDATE sess SET done_upd = ?2, done_pos = ?3 WHERE sid = ?1")?.execute(params![
        sid,
        upd,
        pos.map(|p| p as i64)
    ])?;
    Ok(())
}

pub fn pin(c: &Connection, sid: i64, s: &Session) -> Result<()> {
    c.prepare_cached("UPDATE sess SET pinned = 1, meta = ?2, done_upd = ?3 WHERE sid = ?1")?.execute(params![
        sid,
        serde_json::to_string(s)?,
        s.updated_at
    ])?;
    Ok(())
}

/// Clear backfill progress for every non-pinned session, so the writer's reconcile pass treats
/// them as changed and re-reads them in the background: used on a Uniflo version change (the
/// index is kept) and once after migrating from `fts-v1.sqlite`. Pinned sessions (indexed outside
/// the engine, e.g. archives) are left alone: there is nothing live to re-verify them against.
pub fn clear_progress_non_pinned(c: &Connection) -> Result<()> {
    c.execute("UPDATE sess SET done_upd = NULL, done_pos = NULL WHERE pinned = 0", [])?;
    Ok(())
}

/// Mark a session pinned with raw (already-serialized) metadata, without otherwise touching its
/// progress: used by migration, which carries a session's prior `done_upd` / `done_pos` forward
/// instead of resetting them (see [`pin`] for the normal, engine-driven path).
pub fn pin_raw(c: &Connection, sid: i64, meta_json: Option<&str>) -> Result<()> {
    c.prepare_cached("UPDATE sess SET pinned = 1, meta = ?2 WHERE sid = ?1")?.execute(params![sid, meta_json])?;
    Ok(())
}

pub fn is_cold(c: &Connection, sid: i64) -> Result<bool> {
    Ok(c.query_row("SELECT cold FROM sess WHERE sid = ?1", [sid], |r| r.get::<_, i64>(0))? != 0)
}

pub fn set_cold(c: &Connection, sid: i64, cold: bool) -> Result<()> {
    c.prepare_cached("UPDATE sess SET cold = ?2 WHERE sid = ?1")?.execute(params![sid, cold as i64])?;
    Ok(())
}

pub fn fingerprint_of(c: &Connection, sid: i64) -> Result<Option<i64>> {
    Ok(c.query_row("SELECT fingerprint FROM sess WHERE sid = ?1", [sid], |r| r.get(0)).optional()?.flatten())
}

pub fn set_fingerprint(c: &Connection, sid: i64, fp: i64) -> Result<()> {
    c.prepare_cached("UPDATE sess SET fingerprint = ?2 WHERE sid = ?1")?.execute(params![sid, fp])?;
    Ok(())
}

fn fts_insert(c: &Connection, id: i64, text: &str) -> Result<()> {
    c.prepare_cached("INSERT INTO docs_fts (rowid, text) VALUES (?1, ?2)")?.execute(params![id, text])?;
    Ok(())
}

/// External content: FTS5 needs the exact old text to remove its terms.
fn fts_delete(c: &Connection, id: i64, old: &str) -> Result<()> {
    c.prepare_cached("INSERT INTO docs_fts (docs_fts, rowid, text) VALUES ('delete', ?1, ?2)")?
        .execute(params![id, old])?;
    Ok(())
}

fn old_text(c: &Connection, id: i64) -> Result<String> {
    Ok(c.prepare_cached("SELECT text FROM docs WHERE id = ?1")?.query_row([id], |r| r.get(0))?)
}

/// One hot event row, as read back for cold-ification or promotion.
#[derive(Debug, Clone)]
pub struct Row {
    pub id: i64,
    pub event: String,
    pub ts: i64,
    pub pos: Option<i64>,
    pub text: String,
}

/// Remove a session's hot rows (docs + docs_fts), bump `hot_rows` / `hot_tokens` and return what
/// was removed, newest-insertion-order (by id). Does not touch `sess` or the cold tier.
pub fn drop_hot(c: &Connection, sid: i64) -> Result<Vec<Row>> {
    let rows: Vec<Row> = c
        .prepare_cached("SELECT id, event, ts, pos, text FROM docs WHERE sid = ?1 ORDER BY id")?
        .query_map([sid], |r| {
            Ok(Row { id: r.get(0)?, event: r.get(1)?, ts: r.get(2)?, pos: r.get(3)?, text: r.get(4)? })
        })?
        .collect::<rusqlite::Result<_>>()?;
    if rows.is_empty() {
        return Ok(rows);
    }
    let tokens: i64 = rows.iter().map(|r| token_count(&r.text)).sum();
    for r in &rows {
        fts_delete(c, r.id, &r.text)?;
    }
    c.prepare_cached("DELETE FROM docs WHERE sid = ?1")?.execute([sid])?;
    bump_hot(c, -(rows.len() as i64), -tokens)?;
    Ok(rows)
}

/// Insert a hot row under an id that already exists (promotion from cold): the id, event and
/// position are fixed, only the FTS postings are (re)built.
pub fn insert_hot_row(c: &Connection, sid: i64, r: &Row) -> Result<()> {
    c.prepare_cached("INSERT INTO docs (id, sid, event, ts, pos, text) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")?
        .execute(params![r.id, sid, r.event, r.ts, r.pos, r.text])?;
    fts_insert(c, r.id, &r.text)?;
    Ok(())
}

/// Drop a session's `sess` row (its hot and cold rows must already be gone).
pub fn drop_session_row(c: &Connection, sid: i64) -> Result<()> {
    c.prepare_cached("DELETE FROM sess WHERE sid = ?1")?.execute([sid])?;
    Ok(())
}

/// Insert, replace or drop the row of one event. Returns the change in row count. Only valid for
/// a session already in the hot tier (callers must promote a cold session first).
pub fn upsert(c: &Connection, sid: i64, e: &Event) -> Result<i64> {
    let Some((kind, text)) = doc(e) else { return Ok(0) };
    let pos = e.pos.map(|p| p as i64);
    let found: Option<(i64, bool, bool)> = c
        .prepare_cached("SELECT id, text = ?3, ts = ?4 AND pos IS ?5 FROM docs WHERE sid = ?1 AND event = ?2")?
        .query_row(params![sid, e.id, text, e.ts, pos], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?;
    // The kind lives in the row id: a changed kind is a delete plus an insert.
    let (found, removed) = match found {
        Some((id, ..)) if text.is_empty() || kind_of(id) != kind => {
            let old = old_text(c, id)?;
            fts_delete(c, id, &old)?;
            c.prepare_cached("DELETE FROM docs WHERE id = ?1")?.execute([id])?;
            bump_hot(c, -1, -token_count(&old))?;
            (None, 1)
        }
        other => (other, 0),
    };
    match found {
        None if text.is_empty() => Ok(-removed),
        None => {
            let id = row_id(sid, next_seq(c, sid)?, kind);
            c.prepare_cached("INSERT INTO docs (id, sid, event, ts, pos, text) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")?
                .execute(params![id, sid, e.id, e.ts, pos, text])?;
            fts_insert(c, id, &text)?;
            bump_hot(c, 1, token_count(&text))?;
            Ok(1 - removed)
        }
        Some((_, true, true)) => Ok(0),
        Some((id, true, false)) => {
            c.prepare_cached("UPDATE docs SET ts = ?2, pos = ?3 WHERE id = ?1")?.execute(params![id, e.ts, pos])?;
            Ok(0)
        }
        Some((id, false, _)) => {
            let old = old_text(c, id)?;
            fts_delete(c, id, &old)?;
            c.prepare_cached("UPDATE docs SET ts = ?2, pos = ?3, text = ?4 WHERE id = ?1")?
                .execute(params![id, e.ts, pos, text])?;
            fts_insert(c, id, &text)?;
            bump_hot(c, 0, token_count(&text) - token_count(&old))?;
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(id: &str, body: Body) -> Event {
        Event { id: id.into(), session: "t:s".into(), ts: 1, pos: Some(1), partial: false, truncated: false, body }
    }

    #[test]
    fn bundled_sqlite_has_fts5_trigram() {
        let c = Connection::open_in_memory().unwrap();
        let v: String = c.query_row("SELECT sqlite_version()", [], |r| r.get(0)).unwrap();
        let n: Vec<u32> = v.split('.').map(|x| x.parse().unwrap()).collect();
        assert!((n[0], n[1]) >= (3, 34), "trigram tokenizer needs SQLite 3.34+, bundled {v}");
        c.execute_batch(SCHEMA).unwrap();
        c.execute("INSERT INTO docs (id, sid, event, ts, text) VALUES (1, 1, 'e', 0, '修复缓存击穿问题')", []).unwrap();
        fts_insert(&c, 1, "修复缓存击穿问题").unwrap();
        let n: i64 =
            c.query_row("SELECT count(*) FROM docs_fts WHERE docs_fts MATCH '\"缓存击穿\"'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn indexed_text_per_kind() {
        let text = |b: Body| doc(&ev("e", b)).map(|(k, t)| (kind_name(k), t));
        assert_eq!(text(Body::Reasoning { text: "[redacted]".into() }), Some(("reasoning", String::new())));
        assert_eq!(
            text(Body::System { subtype: "compact".into(), text: "long".into() }),
            Some(("system", "compact".into()))
        );
        assert_eq!(text(Body::TurnEnd { reason: None }), None);
        let call = text(Body::ToolCall {
            call_id: "c".into(),
            name: "Edit".into(),
            input: json!({"file_path": "/x.rs", "new_string": "fn parse_releases(\"a\")", "n": 3, "args": ["-v"]}),
        })
        .unwrap();
        assert_eq!(call.0, "tool_call");
        let mut lines: Vec<&str> = call.1.lines().collect();
        assert_eq!(lines.remove(0), "Edit");
        lines.sort();
        assert_eq!(lines, vec!["-v", "/x.rs", "3", "fn parse_releases(\"a\")"], "values only, no JSON syntax");
        let call =
            text(Body::ToolCall { call_id: "c".into(), name: "Write".into(), input: json!({"c": "é".repeat(4000)}) });
        assert!(call.unwrap().1.len() <= "Write\n".len() + ARGS_MAX);
        let out =
            text(Body::ToolResult { call_id: "c".into(), name: None, output: "a\u{2}b".repeat(3000), is_error: false });
        let out = out.unwrap().1;
        assert!(out.len() <= OUTPUT_MAX && !out.contains('\u{2}'));
    }

    #[test]
    fn scattered_segments_merge_into_one() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(SCHEMA).unwrap();
        let sid = insert_session(&c, "t:s").unwrap();
        let segments =
            || -> i64 { c.query_row("SELECT count(DISTINCT segid) FROM docs_fts_idx", [], |r| r.get(0)).unwrap() };
        assert!(!scattered(&c).unwrap(), "empty index");
        // Each commit flushes a segment; three equal ones stay below FTS5's automerge.
        for i in 0..3 {
            let msg = Body::AssistantMessage { text: format!("segment {i} talks about deploy"), model: None };
            upsert(&c, sid, &ev(&format!("e{i}"), msg)).unwrap();
        }
        assert_eq!(segments(), 3);
        assert!(scattered(&c).unwrap());
        let mut start = true;
        while merge_step(&c, start, 1).unwrap() {
            start = false;
        }
        assert!(!start, "the first step did work");
        assert_eq!(segments(), 1);
        assert!(!scattered(&c).unwrap());
        let n: i64 =
            c.query_row("SELECT count(*) FROM docs_fts WHERE docs_fts MATCH '\"deploy\"'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 3);
    }

    #[test]
    fn upsert_replaces_by_id_and_drops_emptied_rows() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(SCHEMA).unwrap();
        let sid = insert_session(&c, "t:s").unwrap();
        assert_eq!(insert_session(&c, "t:s").unwrap(), sid);
        let msg = |t: &str| ev("p", Body::AssistantMessage { text: t.into(), model: None });
        let hits = |q: &str| -> i64 {
            c.query_row("SELECT count(*) FROM docs_fts WHERE docs_fts MATCH ?1", [format!("\"{q}\"")], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(upsert(&c, sid, &msg("alpha")).unwrap(), 1);
        let id: i64 = c.query_row("SELECT id FROM docs", [], |r| r.get(0)).unwrap();
        assert_eq!((sid_of(id), kind_name(kind_of(id))), (sid, "assistant_message"));
        assert_eq!(upsert(&c, sid, &msg("alphabet")).unwrap(), 0);
        assert_eq!(upsert(&c, sid, &msg("alphabet")).unwrap(), 0);
        assert_eq!((hits("alpha"), hits("alphabet")), (1, 1));
        assert_eq!(count_docs(&c).unwrap(), 1);
        assert_eq!(upsert(&c, sid, &ev("p", Body::Reasoning { text: "[redacted]".into() })).unwrap(), -1);
        assert_eq!((hits("alpha"), count_docs(&c).unwrap()), (0, 0));
        upsert(&c, sid, &msg("gamma ray")).unwrap();
        // Same id, new kind: a fresh row id with the new kind, the old terms gone.
        assert_eq!(upsert(&c, sid, &ev("p", Body::Reasoning { text: "gamma burst".into() })).unwrap(), 0);
        let id: i64 = c.query_row("SELECT id FROM docs", [], |r| r.get(0)).unwrap();
        assert_eq!(kind_name(kind_of(id)), "reasoning");
        assert_eq!((hits("ray"), hits("burst")), (0, 1));
        drop_hot(&c, sid).unwrap();
        drop_session_row(&c, sid).unwrap();
        assert_eq!(hits("gamma"), 0);
    }

    #[test]
    fn counters_track_hot_rows_and_tokens() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(SCHEMA).unwrap();
        let sid = insert_session(&c, "t:s").unwrap();
        let msg = |t: &str| ev("p", Body::AssistantMessage { text: t.into(), model: None });
        assert_eq!(counters(&c).unwrap().hot_rows, 0);
        upsert(&c, sid, &msg("hello world")).unwrap(); // 11 chars -> 9 tokens
        let ct = counters(&c).unwrap();
        assert_eq!((ct.hot_rows, ct.hot_tokens), (1, 9));
        upsert(&c, sid, &msg("hi")).unwrap(); // 2 chars -> 0 tokens
        let ct = counters(&c).unwrap();
        assert_eq!((ct.hot_rows, ct.hot_tokens), (1, 0));
        let rows = drop_hot(&c, sid).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(counters(&c).unwrap().hot_rows, 0);
    }

    #[test]
    fn token_count_matches_trigram_accounting() {
        assert_eq!(token_count(""), 0);
        assert_eq!(token_count("a"), 0);
        assert_eq!(token_count("ab"), 0);
        assert_eq!(token_count("abc"), 1);
        assert_eq!(token_count("缓存击穿"), 2);
    }

    #[test]
    fn version_change_is_reported_without_discarding_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fts-v2.sqlite");
        let (c, state) = open(&path, "uniflo-fts/format2/schema1").unwrap();
        assert_eq!(state, OpenState::Fresh { discarded: false }, "no prior file: fresh, but not `rebuilt`");
        assert!(!state.rebuilt());
        meta_set(&c, "version", "0.0.1-fake").unwrap();
        drop(c);
        let (_, state) = open(&path, "uniflo-fts/format2/schema1").unwrap();
        assert_eq!(state, OpenState::VersionChanged);
        assert!(!state.rebuilt() && !state.is_fresh());
        let (_, state) = open(&path, "uniflo-fts/format2/schema1").unwrap();
        assert_eq!(state, OpenState::Unchanged);
        let (_, state) = open(&path, "uniflo-fts/format3/schema1").unwrap();
        assert_eq!(state, OpenState::Fresh { discarded: true }, "format change discards the old file");
        assert!(state.rebuilt());
    }
}
