//! MiniMax Code app SQLite runtime store (`~/.minimax/v2/sqlite/runtime-state.sqlite`, WAL).
//!
//! Sessions come from `local_runtime_sessions`; messages from `local_runtime_message_rows`
//! (user / assistant rows with optional thinking, tool calls and usage; role-less `msg_type 3`
//! rows are execution diagnostics, mined only for the model id). Turn boundaries are explicit:
//! `local_runtime_turn_ingress` rows yield `turn_start` (accepted) and `turn_end`
//! (completed / failed / aborted), so a turn accepted but not finished reads as `work`.
//!
//! Assistant rows are rewritten **in place** while their turn is open (tool results fill into
//! `tool_calls`, text and usage finalize). `MAX(id)` does not move on those updates, so follow
//! mode keeps `open`: the rowids of rows in still-accepted turns plus a payload hash; each read
//! re-fetches them by primary key and re-expands the ones whose payload changed. Re-expanded
//! events reuse the stable `m:<msg_id>` ids, so consumers upsert them; when a turn closes, its
//! rows are swept once more and the final updates always precede the `turn_end` in `pos` order.
//!
//! `pos` = `ts_ms * 1024 + slot` (start 0, messages 1..=1021, end 1023) so messages and turn
//! events share one chronological paging space.

use crate::sqlite::{col, columns, int, nonempty, open_ro, text, wal_sig};
use anyhow::Result;
use rusqlite::{Connection, Row};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use uniflo_core::adapter::Batch;
use uniflo_core::procs::ProcCache;
use uniflo_core::util::{file_mtime_ms, home, now_ms, text_of};
use uniflo_core::{Adapter, Cursor, HarnessInfo, HistoryQuery, LiveSession, MetaPatch, ReadOutput, Record};
use uniflo_schema::{Body, Event, Status, Usage, session_key};

const ID: &str = "minimax";
const DB_NAME: &str = "runtime-state.sqlite";
const HOT_WINDOW: i64 = 50;
const COLD_WINDOW: i64 = 8;
const HOT_TURNS: i64 = 25;
const COLD_TURNS: i64 = 4;
const HOT_MS: i64 = 24 * 3600 * 1000;
const MAX_SESSIONS: i64 = 5000;
const OPEN_CAP: usize = 256;
const SLOT_START: i64 = 0;
const SLOT_END: i64 = 1023;
const PROC_TTL: Duration = Duration::from_secs(2);
/// Accepted turns older than this are crash residue; `live()` stops trusting them.
const CRASH_CAP_MS: i64 = 24 * 3600 * 1000;

/// Any process of the Electron app (main, helpers, GPU): argv carries the install path.
#[cfg(not(target_os = "windows"))]
fn is_minimax(args: &str) -> bool {
    args.contains("MiniMax Code.app/Contents/")
}

#[cfg(target_os = "windows")]
fn is_minimax(args: &str) -> bool {
    args.to_ascii_lowercase().contains("minimax code")
}

fn is_runtime_db(p: &Path) -> bool {
    p.file_name().is_some_and(|n| n == DB_NAME)
}

pub fn adapters() -> Vec<Arc<dyn Adapter>> {
    vec![Arc::new(MiniMax {
        db: home().join(".minimax/v2/sqlite").join(DB_NAME),
        procs: ProcCache::new(PROC_TTL, is_minimax).with_files(is_runtime_db),
    })]
}

pub struct MiniMax {
    db: PathBuf,
    procs: ProcCache,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    /// `MAX(id)` of message rows.
    row_id: i64,
    /// `MAX(rowid)` of turn rows; turn rows are inserted at accept time, so rowid pages
    /// starts. Completions are caught by id, never by a `MAX(completed_at_ms)` window:
    /// one skewed timestamp must not swallow every later `turn_end`.
    turn_row: i64,
    sess_ms: i64,
    wal_size: u64,
    wal_mtime: i64,
    /// Assistant rows of still-open turns, replayed on follow when their payload changes.
    #[serde(default)]
    open: Vec<OpenRow>,
    /// Ids of turns accepted at the last read; their closure emits the pending `turn_end`.
    #[serde(default)]
    turns: Vec<String>,
}

/// An assistant row of a still-open turn: its last emission was `partial`, so a
/// tracked row always gets one final sweep once its turn closes.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenRow {
    id: i64,
    hash: u64,
}

/// FNV-1a: stable across restarts so the restored cache can detect in-place rewrites.
fn payload_hash(s: &str) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in s.as_bytes() {
        h = (h ^ *b as u64).wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

struct SessRow {
    id: String,
    patch: MetaPatch,
    hot: bool,
}

struct Schema {
    sess_select: String,
}

impl Schema {
    fn load(c: &Connection) -> Result<Schema> {
        let s = columns(c, "local_runtime_sessions")?;
        Ok(Schema {
            sess_select: format!(
                "SELECT session_id, {}, {}, {}, {}, {} FROM local_runtime_sessions",
                col(&s, "title"),
                col(&s, "workspace_dir"),
                col(&s, "created_at_ms"),
                col(&s, "parent_session_id"),
                col(&s, "updated_at_ms"),
            ),
        })
    }
}

fn sess_row(r: &Row) -> SessRow {
    let id = text(r, 0).unwrap_or_default();
    let started = int(r, 3).filter(|t| *t > 0);
    let updated = int(r, 5).unwrap_or(0);
    SessRow {
        patch: MetaPatch {
            parent: nonempty(r, 4).filter(|p| *p != id),
            title: nonempty(r, 1).map(|t| (2, t)),
            cwd: nonempty(r, 2),
            model: None,
            started_at: started,
            updated_at: Some(updated).filter(|t| *t > 0),
        },
        hot: updated.max(started.unwrap_or(0)) > now_ms() - HOT_MS,
        id,
    }
}

fn sessions(c: &Connection, sc: &Schema, filter: &str, args: &[&dyn rusqlite::ToSql]) -> Result<Vec<SessRow>> {
    let mut st = c.prepare(&format!("{} {filter}", sc.sess_select))?;
    let rows = st.query_map(args, |r| Ok(sess_row(r)))?.flatten().collect();
    Ok(rows)
}

struct Pending {
    sid: String,
    pos: i64,
    ts: i64,
    id: String,
    body: Body,
    partial: bool,
}

impl Pending {
    fn new(sid: &str, ts: i64, slot: i64, id: String, body: Body) -> Pending {
        Pending { sid: sid.to_owned(), pos: ts * 1024 + slot, ts, id, body, partial: false }
    }

    /// Part of an open turn: superseded by the same id once the turn closes.
    fn stream(mut self) -> Pending {
        self.partial = true;
        self
    }

    fn into_event(self) -> (String, Event) {
        let session = session_key(ID, &self.sid);
        let ev = Event {
            id: self.id,
            session,
            ts: self.ts,
            pos: Some(self.pos.max(0) as u64),
            partial: self.partial,
            truncated: false,
            body: self.body,
        };
        (self.sid, ev)
    }
}

const MSG_SELECT: &str =
    "SELECT id, session_id, msg_id, role, turn_id, created_at_ms, data_json FROM local_runtime_message_rows";

#[derive(Clone)]
struct Msg {
    id: i64,
    sid: String,
    msg_id: String,
    role: Option<String>,
    turn: String,
    ts: i64,
    data: Option<String>,
}

fn msg(r: &Row) -> Msg {
    Msg {
        id: int(r, 0).unwrap_or(0),
        sid: text(r, 1).unwrap_or_default(),
        msg_id: text(r, 2).unwrap_or_default(),
        role: nonempty(r, 3),
        turn: nonempty(r, 4).unwrap_or_default(),
        ts: int(r, 5).unwrap_or(0),
        data: text(r, 6),
    }
}

fn messages(c: &Connection, filter: &str, args: &[&dyn rusqlite::ToSql]) -> Result<Vec<Msg>> {
    let mut st = c.prepare(&format!("{MSG_SELECT} {filter}"))?;
    let rows = st.query_map(args, |r| Ok(msg(r)))?.flatten().collect();
    Ok(rows)
}

fn hash_of(m: &Msg) -> u64 {
    payload_hash(m.data.as_deref().unwrap_or(""))
}

/// Rows of open (accepted) turns of one session, newest `cap`, chronological.
/// Uses the `(session_id, turn_id, role, id)` index; turn count is bounded by live turns.
fn open_rows(c: &Connection, sid: &str, tids: &[String], cap: i64) -> Result<Vec<Msg>> {
    let ph = (2..=tids.len() + 1).map(|i| format!("?{i}")).collect::<Vec<_>>().join(",");
    let lim = tids.len() + 2;
    let mut args: Vec<&dyn rusqlite::ToSql> = vec![&sid];
    args.extend(tids.iter().map(|t| t as &dyn rusqlite::ToSql));
    args.push(&cap);
    let mut rows =
        messages(c, &format!("WHERE session_id=?1 AND turn_id IN ({ph}) ORDER BY id DESC LIMIT ?{lim}"), &args)?;
    rows.reverse();
    Ok(rows)
}

/// `(session_id, turn_id)` of turns the harness still considers running, newest first.
/// Bounded so crash residue cannot blow up `IN (...)` lists or the cursor state.
fn open_turns(c: &Connection) -> Result<Vec<(String, String)>> {
    let mut st = c.prepare(
        "SELECT session_id, turn_id FROM local_runtime_turn_ingress WHERE status='accepted' ORDER BY rowid DESC LIMIT 64",
    )?;
    let rows = st
        .query_map([], |r| Ok((text(r, 0).unwrap_or_default(), text(r, 1).unwrap_or_default())))?
        .flatten()
        .filter(|(s, t): &(String, String)| !s.is_empty() && !t.is_empty())
        .collect();
    Ok(rows)
}

fn num(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(Value::as_u64).unwrap_or(0)
}

/// Model id announced by an execution diagnostic payload (`msg_type 3`).
fn diag_model(content: &Value) -> Option<String> {
    let parsed;
    let v = match content {
        Value::String(s) => {
            parsed = serde_json::from_str::<Value>(s).ok()?;
            &parsed
        }
        o => o,
    };
    (v.get("kind").and_then(Value::as_str) == Some("model_phase_started"))
        .then(|| v.get("modelId").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned))
        .flatten()
}

/// Stored `tool_call_status` arrives as number or numeric string.
fn call_status(tc: &Value) -> Option<i64> {
    match tc.get("tool_call_status") {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
}

/// JSON-encoded-in-string fields (`tool_call_args`, `tool_call_result_data`) or plain values.
fn json_val(v: Option<&Value>) -> Value {
    match v {
        Some(Value::String(s)) => serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.clone())),
        Some(x) => x.clone(),
        None => Value::Null,
    }
}

fn tool_output(v: Option<&Value>) -> String {
    let raw = match v {
        Some(Value::String(s)) => serde_json::from_str::<Value>(s).unwrap_or_else(|_| Value::String(s.clone())),
        Some(x) => x.clone(),
        None => return String::new(),
    };
    let t = text_of(raw.get("content").unwrap_or(&Value::Null));
    if t.is_empty() { raw.to_string() } else { t }
}

/// Model reported by the per-turn context telemetry on assistant rows.
fn telemetry_model(v: &Value) -> Option<String> {
    v.get("context_usage_telemetry")
        .and_then(|t| t.get("model").and_then(Value::as_str))
        .or_else(|| v.get("model").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn pick_model(models: &[(String, String, i64)], sid: &str) -> Option<String> {
    models.iter().filter(|(s, _, _)| s == sid).max_by_key(|(_, _, t)| *t).map(|(_, m, _)| m.clone())
}

/// `open`: the row's turn is still accepted, so emitted events are stream snapshots
/// (partial) and will be re-emitted when the payload is rewritten in place.
fn expand(m: Msg, open: bool, out: &mut Vec<Pending>, models: &mut Vec<(String, String, i64)>, batch: &mut Batch) {
    let Some(data) = m.data else {
        batch.bad_lines += 1;
        return;
    };
    let Ok(v) = serde_json::from_str::<Value>(&data) else {
        batch.bad_lines += 1;
        return;
    };
    let role = m.role.or_else(|| v.get("role").and_then(Value::as_str).map(str::to_owned));
    let content = v.get("msg_content").unwrap_or(&Value::Null);
    let slot = 1 + m.id.rem_euclid(1021);
    let base = format!("m:{}", m.msg_id);
    match role.as_deref() {
        Some("user") => {
            let t = text_of(content);
            if !t.trim().is_empty() {
                out.push(Pending::new(&m.sid, m.ts, slot, base, Body::UserMessage { text: t, synthetic: false }));
            }
        }
        Some("assistant") => {
            let stream = |p: Pending| if open { p.stream() } else { p };
            if let Some(t) = v.get("thinking_content").map(text_of).filter(|t| !t.trim().is_empty()) {
                out.push(stream(Pending::new(&m.sid, m.ts, slot, format!("{base}:r"), Body::Reasoning { text: t })));
            }
            let t = text_of(content);
            if !t.trim().is_empty() {
                out.push(stream(Pending::new(
                    &m.sid,
                    m.ts,
                    slot,
                    base.clone(),
                    Body::AssistantMessage { text: t, model: None },
                )));
            }
            if let Some(calls) = v.get("tool_calls").and_then(Value::as_array) {
                for (i, tc) in calls.iter().enumerate() {
                    let call_id = tc
                        .get("tool_call_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("{base}:c{i}"));
                    let name = tc.get("tool_name").and_then(Value::as_str).unwrap_or("").to_owned();
                    out.push(stream(Pending::new(
                        &m.sid,
                        m.ts,
                        slot,
                        format!("{base}:t:{call_id}"),
                        Body::ToolCall {
                            call_id: call_id.clone(),
                            name: name.clone(),
                            input: json_val(tc.get("tool_call_args")),
                        },
                    )));
                    if matches!(call_status(tc), Some(2) | Some(3)) {
                        let is_error = call_status(tc) == Some(3);
                        out.push(Pending::new(
                            &m.sid,
                            m.ts,
                            slot,
                            format!("{base}:o:{call_id}"),
                            Body::ToolResult {
                                call_id,
                                name: Some(name),
                                output: tool_output(tc.get("tool_call_result_data")),
                                is_error,
                            },
                        ));
                    }
                }
            }
            if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
                let usage = Usage {
                    input: num(u, "input_tokens"),
                    output: num(u, "output_tokens"),
                    cache_read: num(u, "cache_read"),
                    cache_write: num(u, "cache_write"),
                    model: telemetry_model(&v),
                    ..Default::default()
                };
                if usage != Usage::default() {
                    out.push(stream(Pending::new(&m.sid, m.ts, slot, format!("{base}:u"), Body::Usage(usage))));
                }
            }
            if let Some(model) = telemetry_model(&v) {
                models.push((m.sid.clone(), model, m.ts));
            }
        }
        None => match diag_model(content) {
            Some(model) => models.push((m.sid.clone(), model, m.ts)),
            None => {
                if v.get("msg_type").and_then(Value::as_i64) != Some(3) {
                    batch.unknown.push("msg_type".into());
                }
            }
        },
        Some(other) => batch.unknown.push(format!("role={other}")),
    }
}

const TURN_SELECT: &str =
    "SELECT turn_id, session_id, status, accepted_at_ms, completed_at_ms FROM local_runtime_turn_ingress";

struct Turn {
    id: String,
    sid: String,
    status: String,
    accepted: i64,
    completed: Option<i64>,
}

fn turn(r: &Row) -> Turn {
    Turn {
        id: text(r, 0).unwrap_or_default(),
        sid: text(r, 1).unwrap_or_default(),
        status: text(r, 2).unwrap_or_default(),
        accepted: int(r, 3).unwrap_or(0),
        completed: int(r, 4).filter(|t| *t > 0),
    }
}

fn turns(c: &Connection, filter: &str, args: &[&dyn rusqlite::ToSql]) -> Result<Vec<Turn>> {
    let mut st = c.prepare(&format!("{TURN_SELECT} {filter}"))?;
    let rows = st.query_map(args, |r| Ok(turn(r)))?.flatten().collect();
    Ok(rows)
}

impl Turn {
    fn start(&self) -> Pending {
        Pending::new(&self.sid, self.accepted, SLOT_START, format!("turn:{}:start", self.id), Body::TurnStart {})
    }

    fn end(&self) -> Option<Pending> {
        (self.status != "accepted").then(|| {
            let ts = self.completed.unwrap_or(self.accepted);
            let reason = Some(self.status.clone()).filter(|s| !s.is_empty());
            Pending::new(&self.sid, ts, SLOT_END, format!("turn:{}:end", self.id), Body::TurnEnd { reason })
        })
    }
}

fn flush(mut pending: Vec<Pending>, batch: &mut Batch) {
    pending.sort_by_key(|p| p.pos);
    for p in pending {
        let (sid, ev) = p.into_event();
        batch.items.push((sid, Record::Event(ev)));
    }
}

impl MiniMax {
    fn cursor_for(&self, st: State) -> Cursor {
        let md = std::fs::metadata(&self.db).ok();
        Cursor {
            offset: st.row_id.max(0) as u64,
            size: md.as_ref().map_or(0, |m| m.len()),
            mtime_ms: md.as_ref().map_or(0, file_mtime_ms),
            state: serde_json::to_value(st).unwrap_or_default(),
        }
    }

    fn summary(&self, c: &Connection, sc: &Schema, mut st: State, reset: bool) -> Result<ReadOutput> {
        let mut batch = Batch::default();
        let mut pending = Vec::new();
        let mut next: Vec<OpenRow> = Vec::new();
        let mut accepted: HashMap<String, Vec<String>> = HashMap::new();
        for (sid, tid) in open_turns(c)? {
            accepted.entry(sid).or_default().push(tid);
        }
        let list = sessions(c, sc, "ORDER BY updated_at_ms DESC LIMIT ?1", &[&MAX_SESSIONS])?;
        for s in list {
            let (n, nt) = if s.hot { (HOT_WINDOW, HOT_TURNS) } else { (COLD_WINDOW, COLD_TURNS) };
            let mut patch = s.patch;
            let tids = accepted.get(&s.id).cloned().unwrap_or_default();
            let mut models: Vec<(String, String, i64)> = Vec::new();
            let mut rows = messages(
                c,
                "WHERE session_id=?1 AND role IS NOT NULL AND id<=?2 ORDER BY created_at_ms DESC, id DESC LIMIT ?3",
                &[&s.id, &st.row_id, &n],
            )?;
            if !tids.is_empty() {
                // The open turn may hold more rows than the window covers; pull its tail too.
                rows.extend(open_rows(c, &s.id, &tids, n)?.into_iter().filter(|m| m.id <= st.row_id));
                rows.sort_by_key(|m| m.id);
                rows.dedup_by_key(|m| m.id);
            }
            for m in messages(
                c,
                "WHERE session_id=?1 AND role IS NULL AND data_json LIKE '%model_phase_started%' ORDER BY id DESC LIMIT 1",
                &[&s.id],
            )? {
                expand(m, false, &mut pending, &mut models, &mut batch);
            }
            for m in rows {
                let open = !m.turn.is_empty() && tids.contains(&m.turn);
                if open && m.role.as_deref() == Some("assistant") {
                    next.push(OpenRow { id: m.id, hash: hash_of(&m) });
                }
                expand(m, open, &mut pending, &mut models, &mut batch);
            }
            patch.model = pick_model(&models, &s.id);
            batch.items.push((s.id.clone(), Record::Meta(patch)));
            for t in turns(c, "WHERE session_id=?1 ORDER BY accepted_at_ms DESC LIMIT ?2", &[&s.id, &nt])? {
                pending.push(t.start());
                pending.extend(t.end());
            }
        }
        next.sort_by_key(|o| o.id);
        next.drain(..next.len().saturating_sub(OPEN_CAP));
        st.open = next;
        st.turns = accepted.values().flatten().cloned().collect();
        flush(pending, &mut batch);
        Ok(ReadOutput { cursor: self.cursor_for(st), batch, summary: true, reset })
    }

    fn follow(&self, c: &Connection, sc: &Schema, prev: &State, mut st: State) -> Result<ReadOutput> {
        let mut batch = Batch::default();
        let mut pending = Vec::new();
        let mut models: Vec<(String, String, i64)> = Vec::new();
        let mut next: Vec<OpenRow> = Vec::new();
        let open_ids = open_turns(c)?;
        let open_turns: HashSet<&str> = open_ids.iter().map(|(_, t)| t.as_str()).collect();
        for m in messages(c, "WHERE id>?1 AND id<=?2 ORDER BY id", &[&prev.row_id, &st.row_id])? {
            let open = !m.turn.is_empty() && open_turns.contains(m.turn.as_str());
            if open && m.role.as_deref() == Some("assistant") {
                next.push(OpenRow { id: m.id, hash: hash_of(&m) });
            }
            expand(m, open, &mut pending, &mut models, &mut batch);
        }
        // In-place rewrites: assistant rows of open turns keep their rowid, so re-fetch the
        // tracked set by primary key. Rows whose turn closed get one final sweep here; the
        // `pos` slot ordering keeps those updates ahead of the matching `turn_end`.
        let known: HashMap<i64, &OpenRow> = prev.open.iter().map(|o| (o.id, o)).collect();
        if !known.is_empty() {
            let ids: Vec<i64> = known.keys().copied().collect();
            let ph = (1..=ids.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(",");
            let args: Vec<&dyn rusqlite::ToSql> = ids.iter().map(|i| i as &dyn rusqlite::ToSql).collect();
            for m in messages(c, &format!("WHERE id IN ({ph}) ORDER BY id"), &args)? {
                let Some(old) = known.get(&m.id) else { continue };
                let fresh = hash_of(&m);
                if open_turns.contains(m.turn.as_str()) {
                    next.push(OpenRow { id: m.id, hash: fresh });
                    if fresh != old.hash {
                        expand(m, true, &mut pending, &mut models, &mut batch);
                    }
                } else {
                    expand(m, false, &mut pending, &mut models, &mut batch);
                }
            }
        }
        next.sort_by_key(|o| o.id);
        next.drain(..next.len().saturating_sub(OPEN_CAP));
        st.open = next;
        // Turn rows are inserted at accept time, so rowid pages starts. A turn that was
        // open at the previous read and is closed now yields only its end - by id, never
        // by a MAX(completed_at_ms) window, which one skewed timestamp could poison.
        for t in turns(c, "WHERE rowid>?1 AND rowid<=?2 ORDER BY rowid", &[&prev.turn_row, &st.turn_row])? {
            pending.push(t.start());
            pending.extend(t.end());
        }
        if !prev.turns.is_empty() {
            let ph = (1..=prev.turns.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(",");
            let args: Vec<&dyn rusqlite::ToSql> = prev.turns.iter().map(|t| t as &dyn rusqlite::ToSql).collect();
            for t in turns(c, &format!("WHERE status<>'accepted' AND turn_id IN ({ph}) ORDER BY rowid"), &args)? {
                pending.extend(t.end());
            }
        }
        st.turns = open_ids.into_iter().map(|(_, t)| t).collect();
        let mut metas: HashSet<String> = HashSet::new();
        for s in sessions(c, sc, "WHERE updated_at_ms>?1 AND updated_at_ms<=?2", &[&prev.sess_ms, &st.sess_ms])? {
            let mut patch = s.patch;
            patch.model = pick_model(&models, &s.id);
            metas.insert(s.id.clone());
            batch.items.push((s.id, Record::Meta(patch)));
        }
        for sid in models.iter().map(|(s, _, _)| s.clone()).collect::<HashSet<_>>() {
            if !metas.contains(&sid) {
                let model = pick_model(&models, &sid);
                batch.items.push((sid, Record::Meta(MetaPatch { model, ..Default::default() })));
            }
        }
        flush(pending, &mut batch);
        Ok(ReadOutput { cursor: self.cursor_for(st), batch, summary: false, reset: false })
    }
}

impl Adapter for MiniMax {
    fn info(&self) -> HarnessInfo {
        HarnessInfo { id: ID, name: "MiniMax Code" }
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.db.parent().filter(|p| p.is_dir()).map(Path::to_path_buf).into_iter().collect()
    }

    fn source_for(&self, path: &Path) -> Option<PathBuf> {
        crate::sqlite::source_for_db(&self.db, path)
    }

    fn discover(&self) -> Vec<PathBuf> {
        if self.db.is_file() { vec![self.db.clone()] } else { Vec::new() }
    }

    fn read(&self, src: &Path, cursor: Option<&Cursor>) -> Result<ReadOutput> {
        let c = open_ro(src)?;
        c.execute_batch("BEGIN")?;
        let sc = Schema::load(&c)?;
        let (wal_size, wal_mtime) = wal_sig(src);
        let max = |sql: &str| -> Result<i64> { Ok(c.query_row(sql, [], |r| r.get(0))?) };
        let st = State {
            row_id: max("SELECT COALESCE(MAX(id),0) FROM local_runtime_message_rows")?,
            turn_row: max("SELECT COALESCE(MAX(rowid),0) FROM local_runtime_turn_ingress")?,
            sess_ms: max("SELECT COALESCE(MAX(updated_at_ms),0) FROM local_runtime_sessions")?,
            wal_size,
            wal_mtime,
            ..State::default()
        };
        let prev = cursor.and_then(|c| serde_json::from_value::<State>(c.state.clone()).ok());
        match (cursor, prev) {
            (Some(_), Some(p)) if p.row_id <= st.row_id && p.turn_row <= st.turn_row && p.sess_ms <= st.sess_ms => {
                self.follow(&c, &sc, &p, st)
            }
            (cur, _) => self.summary(&c, &sc, st, cur.is_some()),
        }
    }

    fn history(&self, src: &Path, session_id: &str, q: &HistoryQuery) -> Result<Vec<Event>> {
        let c = open_ro(src)?;
        let limit = q.limit.max(1) as i64;
        let before = q.before.map_or(i64::MAX, |b| b.min(i64::MAX as u64) as i64);
        let bound_ms = before / 1024;
        let mut batch = Batch::default();
        let mut pending = Vec::new();
        let tids: Vec<String> = open_turns(&c)?.into_iter().filter(|(s, _)| s == session_id).map(|(_, t)| t).collect();
        let mut rows = messages(
            &c,
            "WHERE session_id=?1 AND role IS NOT NULL AND created_at_ms<=?2 ORDER BY created_at_ms DESC, id DESC LIMIT ?3",
            &[&session_id, &bound_ms, &limit],
        )?;
        if !tids.is_empty() {
            rows.extend(open_rows(&c, session_id, &tids, limit)?.into_iter().filter(|m| m.ts <= bound_ms));
            rows.sort_by_key(|m| m.id);
            rows.dedup_by_key(|m| m.id);
        }
        for m in rows {
            let open = !m.turn.is_empty() && tids.contains(&m.turn);
            expand(m, open, &mut pending, &mut Vec::new(), &mut batch);
        }
        for t in turns(
            &c,
            "WHERE session_id=?1 AND accepted_at_ms<=?2 ORDER BY accepted_at_ms DESC LIMIT ?3",
            &[&session_id, &bound_ms, &limit],
        )? {
            pending.push(t.start());
        }
        for t in turns(
            &c,
            "WHERE session_id=?1 AND status<>'accepted' AND COALESCE(completed_at_ms,accepted_at_ms)<=?2 ORDER BY COALESCE(completed_at_ms,accepted_at_ms) DESC LIMIT ?3",
            &[&session_id, &bound_ms, &limit],
        )? {
            pending.extend(t.end());
        }
        pending.retain(|p| p.pos < before);
        pending.sort_by_key(|p| p.pos);
        // Keep the newest `limit` events, but never split events sharing one position.
        let mut cut = pending.len().saturating_sub(q.limit.max(1));
        while cut > 0 && pending[cut - 1].pos == pending[cut].pos {
            cut -= 1;
        }
        Ok(pending.split_off(cut).into_iter().map(|p| p.into_event().1).collect())
    }

    /// Work while the app holds a turn `accepted`; crash residue (rows never closed
    /// because the app died mid-turn) is ignored past `CRASH_CAP_MS`.
    fn live(&self) -> Option<Vec<LiveSession>> {
        if !self.db.is_file() {
            return None;
        }
        let procs = self.procs.get();
        if procs.is_empty() {
            return Some(Vec::new());
        }
        let pid = procs.iter().find(|p| p.files.iter().any(|f| f == &self.db)).map_or(procs[0].pid, |p| p.pid);
        let Ok(c) = open_ro(&self.db) else { return Some(Vec::new()) };
        let Ok(mut st) = c.prepare(
            "SELECT DISTINCT session_id FROM local_runtime_turn_ingress WHERE status='accepted' AND accepted_at_ms > ?1",
        ) else {
            return Some(Vec::new());
        };
        let ids: Vec<String> = st
            .query_map([now_ms() - CRASH_CAP_MS], |r| Ok(text(r, 0).unwrap_or_default()))
            .ok()?
            .flatten()
            .filter(|s| !s.is_empty())
            .collect();
        Some(ids.into_iter().map(|id| LiveSession { id, pid, status: Some(Status::Work) }).collect())
    }

    fn live_roots(&self) -> Vec<PathBuf> {
        self.db.parent().map(Path::to_path_buf).into_iter().collect()
    }

    fn changed(&self, src: &Path, cursor: &Cursor) -> bool {
        let Ok(md) = std::fs::metadata(src) else { return true };
        if md.len() != cursor.size || file_mtime_ms(&md) != cursor.mtime_ms {
            return true;
        }
        let (size, mtime) = wal_sig(src);
        match serde_json::from_value::<State>(cursor.state.clone()) {
            Ok(st) => st.wal_size != size || st.wal_mtime != mtime,
            Err(_) => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testkit::{Fixture, group, kinds};
    use rusqlite::params;
    use serde_json::json;
    use uniflo_schema::Status;

    const SCHEMA: &str = "
        CREATE TABLE local_runtime_sessions (session_id TEXT PRIMARY KEY, record_json TEXT NOT NULL,
            updated_at_ms INTEGER NOT NULL, agent_name TEXT, status TEXT, archived INTEGER NOT NULL DEFAULT 0,
            parent_session_id TEXT, workspace_dir TEXT, title TEXT, created_at_ms INTEGER);
        CREATE TABLE local_runtime_message_rows (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL,
            msg_id TEXT NOT NULL, role TEXT, turn_id TEXT, created_at_ms INTEGER NOT NULL, data_json TEXT NOT NULL,
            UNIQUE(session_id, msg_id));
        CREATE TABLE local_runtime_turn_ingress (turn_id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
            status TEXT NOT NULL, accepted_at_ms INTEGER NOT NULL, completed_at_ms INTEGER);";

    const T0: i64 = 1_900_000_000_000;

    struct Db {
        fx: Fixture,
        db: PathBuf,
        w: Connection,
        ad: MiniMax,
    }

    fn db() -> Db {
        db_with(|_| false)
    }

    fn db_with(keep: fn(&str) -> bool) -> Db {
        let fx = Fixture::new();
        let db = fx.root().join(DB_NAME);
        let w = Connection::open(&db).unwrap();
        w.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(())).unwrap();
        w.execute_batch(SCHEMA).unwrap();
        let ad = MiniMax { db: db.clone(), procs: ProcCache::new(PROC_TTL, keep) };
        Db { fx, db, w, ad }
    }

    impl Db {
        fn session(&self, id: &str, t: i64) {
            self.w
                .execute(
                    "INSERT INTO local_runtime_sessions(session_id,record_json,updated_at_ms,workspace_dir,title,created_at_ms)
                     VALUES(?1,'{}',?2,'/w','Title',?2)",
                    params![id, t],
                )
                .unwrap();
        }
        fn row(&self, sid: &str, msg_id: &str, role: Option<&str>, t: i64, data: &str) {
            self.w
                .execute(
                    "INSERT INTO local_runtime_message_rows(session_id,msg_id,role,created_at_ms,data_json) VALUES(?1,?2,?3,?4,?5)",
                    params![sid, msg_id, role, t, data],
                )
                .unwrap();
        }
        fn user(&self, sid: &str, msg_id: &str, t: i64, text: &str) {
            self.row(sid, msg_id, Some("user"), t, &json!({"msg_type":1,"role":"user","msg_content":text}).to_string());
        }
        fn assistant(&self, sid: &str, msg_id: &str, t: i64, text: &str) {
            let d = json!({"msg_type":1,"role":"assistant","msg_content":text,"thinking_content":"hmm","finish_reason":"stop",
                "usage":{"input_tokens":10,"output_tokens":5,"cache_read":2}});
            self.row(sid, msg_id, Some("assistant"), t, &d.to_string());
        }
        /// Assistant row inside turn `turn` (the app stamps `turn_id` on every row).
        fn arrow(&self, sid: &str, msg_id: &str, turn: &str, t: i64, data: &str) {
            self.w
                .execute(
                    "INSERT INTO local_runtime_message_rows(session_id,msg_id,role,turn_id,created_at_ms,data_json) VALUES(?1,?2,'assistant',?3,?4,?5)",
                    params![sid, msg_id, turn, t, data],
                )
                .unwrap();
        }
        fn turn(&self, id: &str, sid: &str, status: &str, acc: i64, done: Option<i64>) {
            self.w
                .execute(
                    "INSERT INTO local_runtime_turn_ingress(turn_id,session_id,status,accepted_at_ms,completed_at_ms) VALUES(?1,?2,?3,?4,?5)",
                    params![id, sid, status, acc, done],
                )
                .unwrap();
        }
        fn set_data(&self, msg_id: &str, data: &str) {
            self.w
                .execute("UPDATE local_runtime_message_rows SET data_json=?1 WHERE msg_id=?2", params![data, msg_id])
                .unwrap();
        }
    }

    fn seed(d: &Db) {
        d.session("s1", T0);
        d.turn("t1", "s1", "completed", T0 + 1, Some(T0 + 400));
        d.user("s1", "u1", T0 + 2, "hello");
        d.row(
            "s1",
            "d1",
            None,
            T0 + 3,
            r#"{"msg_type":3,"msg_content":"{\"kind\":\"model_phase_started\",\"modelId\":\"mm-1\"}"}"#,
        );
        d.assistant("s1", "a1", T0 + 300, "hi there");
        d.session("s2", T0 + 10);
        d.turn("t2", "s2", "accepted", T0 + 11, None);
        d.user("s2", "u2", T0 + 12, "working");
    }

    #[test]
    fn summary_meta_events_and_status() {
        let d = db();
        seed(&d);
        let out = d.ad.read(&d.db, None).unwrap();
        assert!(out.summary);
        let g = group(out.batch);
        let s1 = &g["s1"];
        assert_eq!(s1.meta.title.as_deref(), Some("Title"));
        assert_eq!(s1.meta.cwd.as_deref(), Some("/w"));
        assert_eq!(s1.meta.model.as_deref(), Some("mm-1"));
        assert_eq!(s1.meta.started_at, Some(T0));
        assert_eq!(
            kinds(&s1.events),
            ["turn_start", "user_message", "reasoning", "assistant_message", "usage", "turn_end"]
        );
        assert_eq!(s1.status(), Status::Idle);
        assert_eq!(g["s2"].status(), Status::Work);
        assert_eq!(kinds(&g["s2"].events), ["turn_start", "user_message"]);
    }

    #[test]
    fn follow_emits_new_rows_and_turn_completion() {
        let d = db();
        seed(&d);
        let first = d.ad.read(&d.db, None).unwrap();
        d.assistant("s2", "a2", T0 + 500, "done");
        d.w.execute(
            "UPDATE local_runtime_turn_ingress SET status='completed', completed_at_ms=?1 WHERE turn_id='t2'",
            [T0 + 600],
        )
        .unwrap();
        d.w.execute("UPDATE local_runtime_sessions SET updated_at_ms=?1 WHERE session_id='s2'", [T0 + 600]).unwrap();
        d.session("s3", T0 + 700);
        d.turn("t3", "s3", "accepted", T0 + 701, None);
        let second = d.ad.read(&d.db, Some(&first.cursor)).unwrap();
        assert!(!second.summary);
        assert!(second.cursor.offset > first.cursor.offset);
        let g = group(second.batch);
        assert_eq!(g.keys().cloned().collect::<Vec<_>>(), ["s2", "s3"]);
        assert_eq!(kinds(&g["s2"].events), ["reasoning", "assistant_message", "usage", "turn_end"]);
        assert_eq!(g["s2"].status(), Status::Idle);
        assert_eq!(kinds(&g["s3"].events), ["turn_start"]);
        assert_eq!(g["s3"].meta.cwd.as_deref(), Some("/w"));
        let third = d.ad.read(&d.db, Some(&second.cursor)).unwrap();
        assert!(third.batch.items.is_empty());
    }

    #[test]
    fn in_place_rewrites_stream_until_turn_close() {
        let d = db();
        d.session("s2", T0);
        d.turn("t2", "s2", "accepted", T0 + 1, None);
        d.arrow("s2", "a2", "t2", T0 + 2, &json!({"role":"assistant","thinking_content":"half"}).to_string());
        let first = d.ad.read(&d.db, None).unwrap();
        let g = group(first.batch);
        assert_eq!(kinds(&g["s2"].events), ["turn_start", "reasoning"]);
        assert!(g["s2"].events[1].partial, "open-turn text streams as partial");
        let partial_id = g["s2"].events[1].id.clone();
        // The app rewrites the same row in place as the block completes.
        d.set_data(
            "a2",
            &json!({"role":"assistant","thinking_content":"half done","msg_content":"answer"}).to_string(),
        );
        let second = d.ad.read(&d.db, Some(&first.cursor)).unwrap();
        let g = group(second.batch);
        assert_eq!(kinds(&g["s2"].events), ["reasoning", "assistant_message"]);
        assert_eq!(g["s2"].events[0].id, partial_id, "re-write reuses the stable event id");
        assert!(g["s2"].events[1].partial);
        // Turn closes: one final non-partial sweep, then turn_end.
        d.w.execute(
            "UPDATE local_runtime_turn_ingress SET status='completed', completed_at_ms=?1 WHERE turn_id='t2'",
            [T0 + 500],
        )
        .unwrap();
        let third = d.ad.read(&d.db, Some(&second.cursor)).unwrap();
        let g = group(third.batch);
        assert_eq!(kinds(&g["s2"].events), ["reasoning", "assistant_message", "turn_end"]);
        assert!(g["s2"].events[..2].iter().all(|e| !e.partial), "final sweep clears partial");
        let fourth = d.ad.read(&d.db, Some(&third.cursor)).unwrap();
        assert!(fourth.batch.items.is_empty(), "closed turns are not re-swept");
    }

    #[test]
    fn out_of_order_completion_still_emits_turn_end() {
        // A turn that completed with a timestamp below an already-seen completion
        // (clock skew / coarse granularity) must not lose its turn_end.
        let d = db();
        d.session("s1", T0);
        d.turn("big", "s1", "completed", T0 + 1, Some(T0 + 10_000));
        d.turn("late", "s1", "accepted", T0 + 2, None);
        let first = d.ad.read(&d.db, None).unwrap();
        d.w.execute(
            "UPDATE local_runtime_turn_ingress SET status='completed', completed_at_ms=?1 WHERE turn_id='late'",
            [T0 + 3],
        )
        .unwrap();
        let second = d.ad.read(&d.db, Some(&first.cursor)).unwrap();
        let g = group(second.batch);
        assert_eq!(kinds(&g["s1"].events), ["turn_end"]);
        assert_eq!(g["s1"].events[0].id, "turn:late:end");
        assert_eq!(g["s1"].status(), Status::Idle);
    }

    #[test]
    fn assistant_tool_calls_expand_to_calls_and_results() {
        let d = db();
        d.session("s1", T0);
        d.turn("t1", "s1", "accepted", T0 + 1, None);
        d.arrow(
            "s1",
            "a1",
            "t1",
            T0 + 2,
            &json!({"role":"assistant","msg_content":"run",
                "tool_calls":[
                    {"tool_name":"bash","tool_call_id":"c1","tool_call_status":2,
                     "tool_call_args":"{\"command\":\"ls\"}","tool_call_result_data":"{\"content\":[{\"type\":\"text\",\"text\":\"a.txt\"}]}"},
                    {"tool_name":"mcp_browser","tool_call_id":"c2","tool_call_status":"3",
                     "tool_call_args":{"action":"screenshot"},"tool_call_result_data":"{\"content\":[],\"details\":{\"ok\":false}}"},
                    {"tool_name":"read","tool_call_id":"c3","tool_call_status":1,"tool_call_args":"{\"path\":\"/x\"}"}
                ]})
                .to_string(),
        );
        let out = d.ad.read(&d.db, None).unwrap();
        let g = group(out.batch);
        assert_eq!(
            kinds(&g["s1"].events),
            ["turn_start", "assistant_message", "tool_call", "tool_result", "tool_call", "tool_result", "tool_call"]
        );
        let evs = &g["s1"].events;
        match &evs[2].body {
            Body::ToolCall { call_id, name, input } => {
                assert_eq!((call_id.as_str(), name.as_str()), ("c1", "bash"));
                assert_eq!(input["command"], "ls", "string-encoded args are decoded");
            }
            b => panic!("{b:?}"),
        }
        match &evs[3].body {
            Body::ToolResult { call_id, name, output, is_error } => {
                assert_eq!(call_id, "c1");
                assert_eq!(name.as_deref(), Some("bash"));
                assert_eq!(output, "a.txt");
                assert!(!is_error);
            }
            b => panic!("{b:?}"),
        }
        match &evs[5].body {
            Body::ToolResult { call_id, is_error, output, .. } => {
                assert_eq!(call_id, "c2");
                assert!(is_error, "status 3 is a failed call");
                assert!(output.contains("ok"), "empty content list falls back to raw JSON");
            }
            b => panic!("{b:?}"),
        }
        assert!(evs[2].partial && evs[6].partial, "open turn: calls may still be live");
        assert!(!evs[3].partial && !evs[5].partial, "a finished result is never partial");
    }

    #[test]
    fn telemetry_and_diag_models_pick_latest() {
        let d = db();
        d.session("s1", T0);
        d.turn("t1", "s1", "completed", T0 + 1, Some(T0 + 900));
        d.row(
            "s1",
            "d1",
            None,
            T0 + 2,
            r#"{"msg_type":3,"msg_content":"{\"kind\":\"model_phase_started\",\"modelId\":\"mm-old\"}"}"#,
        );
        d.arrow(
            "s1",
            "a1",
            "t1",
            T0 + 300,
            &json!({"role":"assistant","msg_content":"x","context_usage_telemetry":{"model":"mm-new"}}).to_string(),
        );
        let g = group(d.ad.read(&d.db, None).unwrap().batch);
        assert_eq!(g["s1"].meta.model.as_deref(), Some("mm-new"), "newest per-turn telemetry wins");
    }

    #[test]
    fn live_without_app_process_is_empty() {
        let d = db();
        d.session("s2", T0);
        d.turn("t2", "s2", "accepted", now_ms(), None);
        // keep never matches any process: the app is gone -> nothing live, no DB trust.
        assert_eq!(d.ad.live(), Some(Vec::new()));
    }

    #[test]
    fn history_pages_backwards_across_messages_and_turns() {
        let d = db();
        d.session("s1", T0);
        for i in 0..4 {
            let t = T0 + i * 100;
            d.turn(&format!("t{i}"), "s1", "completed", t, Some(t + 50));
            d.user("s1", &format!("u{i}"), t + 1, &format!("q{i}"));
        }
        let all = d.ad.history(&d.db, "s1", &HistoryQuery { before: None, limit: 100 }).unwrap();
        assert_eq!(all.len(), 12);
        assert!(all.windows(2).all(|w| w[0].pos < w[1].pos));
        let p1 = d.ad.history(&d.db, "s1", &HistoryQuery { before: None, limit: 5 }).unwrap();
        assert_eq!(p1.len(), 5);
        assert_eq!(p1.last().unwrap().id, all.last().unwrap().id);
        let p2 = d.ad.history(&d.db, "s1", &HistoryQuery { before: p1[0].pos, limit: 5 }).unwrap();
        assert_eq!(p2.len(), 5);
        assert!(p2.last().unwrap().pos < p1[0].pos);
        let p3 = d.ad.history(&d.db, "s1", &HistoryQuery { before: p2[0].pos, limit: 5 }).unwrap();
        assert_eq!(p3.len(), 2);
        let ids: Vec<_> = p3.iter().chain(&p2).chain(&p1).map(|e| e.id.clone()).collect();
        assert_eq!(ids, all.iter().map(|e| e.id.clone()).collect::<Vec<_>>());
    }

    #[test]
    fn source_mapping_and_changed() {
        let d = db();
        let r = d.fx.root();
        for n in [DB_NAME, "runtime-state.sqlite-wal", "runtime-state.sqlite-shm"] {
            assert_eq!(d.ad.source_for(&r.join(n)), Some(d.db.clone()), "{n}");
        }
        assert_eq!(d.ad.source_for(&r.join("backups/runtime-state.sqlite")), None);
        assert_eq!(d.ad.source_for(&r.join("other.sqlite")), None);
        seed(&d);
        let out = d.ad.read(&d.db, None).unwrap();
        assert!(!d.ad.changed(&d.db, &out.cursor));
        d.user("s2", "u9", T0 + 20, "more");
        assert!(d.ad.changed(&d.db, &out.cursor));
    }

    #[test]
    fn odd_rows_do_not_panic() {
        let d = db();
        d.session("s1", T0);
        d.row("s1", "x1", Some("user"), T0 + 1, "{not json");
        d.row("s1", "x2", Some("assistant"), T0 + 2, r#"{"msg_content":null,"usage":"oops"}"#);
        d.row("s1", "x3", Some("user"), T0 + 3, r#"{"msg_content":[{"type":"text","text":"blocks"}]}"#);
        d.row("s1", "x4", None, T0 + 4, r#"{"msg_type":3,"msg_content":"not json"}"#);
        d.row("s1", "x5", Some("robot"), T0 + 5, "{}");
        d.w.execute(
            "INSERT INTO local_runtime_sessions(session_id,record_json,updated_at_ms) VALUES('bare','{}',1)",
            [],
        )
        .unwrap();
        let out = d.ad.read(&d.db, None).unwrap();
        assert_eq!(out.batch.bad_lines, 1);
        assert_eq!(out.batch.unknown, ["role=robot"]);
        let g = group(out.batch);
        assert_eq!(kinds(&g["s1"].events), ["user_message"]);
        assert!(g["bare"].events.is_empty());
    }
}
