//! The index database: schema, format tag, per-session progress and event rows.
//!
//! `docs` holds one row per searchable event (`UNIQUE(sid, event)`, so an event id that comes
//! back replaces its row); `docs_fts` is an external-content FTS5 trigram index over
//! `docs.text`. `sess.done_upd` / `done_pos` record how far the last complete backfill pass
//! got, which is what lets a restart skip unchanged sessions.
//!
//! Row ids carry the session and kind ([`row_id`]), so ranking reads only the FTS index:
//! joining `docs` costs a page read per match (rows are text-heavy), ~0.7 s for 14k matches.
//!
//! `docs_fts` is maintained by plain single-row statements, never triggers: a trigger needs a
//! statement journal, every statement journal is a savepoint, and FTS5 flushes its pending
//! terms into a new segment at each savepoint — one tiny segment plus merge work per event.

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
"#;

/// Tool arguments are cut to this many bytes before indexing.
pub const ARGS_MAX: usize = 2 * 1024;
/// Tool output is cut to this many bytes before indexing.
pub const OUTPUT_MAX: usize = 4 * 1024;

/// Indexed kinds; the stored code is the position + 1.
pub const KINDS: [&str; 6] = ["user_message", "assistant_message", "reasoning", "tool_call", "tool_result", "system"];

pub fn kind_code(kind: &str) -> Option<i64> {
    KINDS.iter().position(|k| *k == kind).map(|i| i as i64 + 1)
}

const SEQ_BITS: u32 = 29;

/// `sid << 32 | seq << 3 | kind`: up to 2^29 rows per session, kinds 1..=6.
pub fn row_id(sid: i64, seq: i64, kind: i64) -> i64 {
    (sid << 32) | (seq << 3) | kind
}

pub fn sid_of(id: i64) -> i64 {
    id >> 32
}

pub fn kind_of(id: i64) -> i64 {
    id & 7
}

fn next_seq(c: &Connection, sid: i64) -> Result<i64> {
    let max: Option<i64> = c
        .prepare_cached("SELECT max(id) FROM docs WHERE id >= ?1 AND id < ?2")?
        .query_row(params![sid << 32, (sid + 1) << 32], |r| r.get(0))?;
    let seq = max.map_or(0, |m| ((m >> 3) & ((1 << SEQ_BITS) - 1)) + 1);
    anyhow::ensure!(seq < 1 << SEQ_BITS, "session {sid} has too many indexed events");
    Ok(seq)
}

pub fn kind_name(code: i64) -> &'static str {
    usize::try_from(code - 1).ok().and_then(|i| KINDS.get(i)).copied().unwrap_or("unknown")
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
    c.pragma_update(None, "journal_mode", "WAL")?;
    c.pragma_update(None, "synchronous", "NORMAL")?;
    // Segment merges re-read recent pages; 32 MB keeps them out of the file.
    c.pragma_update(None, "cache_size", -32_000)?;
    // A checkpointed WAL keeps its peak size unless capped.
    c.pragma_update(None, "journal_size_limit", 64 * 1024 * 1024)?;
    Ok(())
}

fn tag_of(c: &Connection) -> Result<Option<String>> {
    Ok(c.query_row("SELECT v FROM meta WHERE k = 'tag'", [], |r| r.get(0)).optional()?)
}

/// Open the writer connection. A file with another tag (format, Uniflo or schema version
/// changed) or one that cannot be read is discarded and recreated; returns whether that happened.
pub fn open(path: &Path, tag: &str) -> Result<(Connection, bool)> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let mut rebuilt = false;
    if path.exists() {
        let keep = Connection::open(path).ok().and_then(|c| tag_of(&c).ok().flatten()).is_some_and(|t| t == tag);
        if !keep {
            remove_files(path);
            rebuilt = true;
        }
    }
    let c = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
    configure(&c)?;
    c.execute_batch(SCHEMA).context("create full-text schema (needs SQLite FTS5 with the trigram tokenizer)")?;
    c.execute("INSERT OR REPLACE INTO meta (k, v) VALUES ('tag', ?1)", [tag])?;
    Ok((c, rebuilt))
}

pub fn open_reader(path: &Path) -> Result<Connection> {
    let c = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .with_context(|| format!("open {}", path.display()))?;
    c.busy_timeout(Duration::from_secs(2))?;
    Ok(c)
}

#[derive(Debug, Clone, Default)]
pub struct SessRow {
    pub sid: i64,
    pub done_upd: Option<i64>,
    pub done_pos: Option<u64>,
    pub pinned: bool,
    /// Kept for pinned sessions only: the engine may not know them.
    pub meta: Option<Session>,
}

pub fn load_sessions(c: &Connection) -> Result<Vec<(String, SessRow)>> {
    let mut st = c.prepare("SELECT sid, key, done_upd, done_pos, pinned, meta FROM sess")?;
    let rows = st.query_map([], |r| {
        let meta: Option<String> = r.get(5)?;
        Ok((
            r.get::<_, String>(1)?,
            SessRow {
                sid: r.get(0)?,
                done_upd: r.get(2)?,
                done_pos: r.get::<_, Option<i64>>(3)?.map(|p| p as u64),
                pinned: r.get::<_, i64>(4)? != 0,
                meta: meta.and_then(|m| serde_json::from_str(&m).ok()),
            },
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn count_docs(c: &Connection) -> Result<u64> {
    Ok(c.query_row("SELECT count(*) FROM docs", [], |r| r.get::<_, i64>(0))? as u64)
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

/// Drop a session and its rows. Returns the number of event rows removed.
pub fn delete_session(c: &Connection, sid: i64) -> Result<u64> {
    let rows: Vec<(i64, String)> = c
        .prepare_cached("SELECT id, text FROM docs WHERE sid = ?1")?
        .query_map([sid], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, text) in &rows {
        fts_delete(c, *id, text)?;
    }
    c.prepare_cached("DELETE FROM docs WHERE sid = ?1")?.execute([sid])?;
    c.prepare_cached("DELETE FROM sess WHERE sid = ?1")?.execute([sid])?;
    Ok(rows.len() as u64)
}

/// Insert, replace or drop the row of one event. Returns the change in row count.
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
            fts_delete(c, id, &old_text(c, id)?)?;
            c.prepare_cached("DELETE FROM docs WHERE id = ?1")?.execute([id])?;
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
            Ok(1 - removed)
        }
        Some((_, true, true)) => Ok(0),
        Some((id, true, false)) => {
            c.prepare_cached("UPDATE docs SET ts = ?2, pos = ?3 WHERE id = ?1")?.execute(params![id, e.ts, pos])?;
            Ok(0)
        }
        Some((id, false, _)) => {
            fts_delete(c, id, &old_text(c, id)?)?;
            c.prepare_cached("UPDATE docs SET ts = ?2, pos = ?3, text = ?4 WHERE id = ?1")?
                .execute(params![id, e.ts, pos, text])?;
            fts_insert(c, id, &text)?;
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
        assert_eq!(delete_session(&c, sid).unwrap(), 1);
        assert_eq!(hits("gamma"), 0);
    }
}
