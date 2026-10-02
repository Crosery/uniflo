//! MiniMax Code app SQLite runtime store (`~/.minimax/v2/sqlite/runtime-state.sqlite`, WAL).
//!
//! Sessions come from `local_runtime_sessions`; messages from `local_runtime_message_rows`
//! (user / assistant rows with optional thinking and usage; role-less `msg_type 3` rows are
//! execution diagnostics, mined only for the model id). Turn boundaries are explicit:
//! `local_runtime_turn_ingress` rows yield `turn_start` (accepted) and `turn_end`
//! (completed / failed / aborted), so a turn accepted but not finished reads as `work`.
//!
//! `pos` = `ts_ms * 1024 + slot` (start 0, messages 1..=1021, end 1023) so messages and turn
//! events share one chronological paging space.

use crate::sqlite::{col, columns, int, nonempty, open_ro, text, wal_sig};
use anyhow::Result;
use rusqlite::{Connection, Row};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uniflo_core::adapter::Batch;
use uniflo_core::util::{file_mtime_ms, home, now_ms, text_of};
use uniflo_core::{Adapter, Cursor, HarnessInfo, HistoryQuery, MetaPatch, ReadOutput, Record};
use uniflo_schema::{Body, Event, Usage, session_key};

const ID: &str = "minimax";
const DB_NAME: &str = "runtime-state.sqlite";
const HOT_WINDOW: i64 = 50;
const COLD_WINDOW: i64 = 8;
const HOT_TURNS: i64 = 25;
const COLD_TURNS: i64 = 4;
const HOT_MS: i64 = 24 * 3600 * 1000;
const MAX_SESSIONS: i64 = 5000;
const SLOT_START: i64 = 0;
const SLOT_END: i64 = 1023;

pub fn adapters() -> Vec<Arc<dyn Adapter>> {
    vec![Arc::new(MiniMax { db: home().join(".minimax/v2/sqlite").join(DB_NAME) })]
}

pub struct MiniMax {
    db: PathBuf,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    row_id: i64,
    acc_ms: i64,
    comp_ms: i64,
    sess_ms: i64,
    wal_size: u64,
    wal_mtime: i64,
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
}

impl Pending {
    fn new(sid: &str, ts: i64, slot: i64, id: String, body: Body) -> Pending {
        Pending { sid: sid.to_owned(), pos: ts * 1024 + slot, ts, id, body }
    }

    fn into_event(self) -> (String, Event) {
        let session = session_key(ID, &self.sid);
        let ev = Event {
            id: self.id,
            session,
            ts: self.ts,
            pos: Some(self.pos.max(0) as u64),
            partial: false,
            truncated: false,
            body: self.body,
        };
        (self.sid, ev)
    }
}

const MSG_SELECT: &str =
    "SELECT id, session_id, msg_id, role, created_at_ms, data_json FROM local_runtime_message_rows";

struct Msg {
    id: i64,
    sid: String,
    msg_id: String,
    role: Option<String>,
    ts: i64,
    data: Option<String>,
}

fn msg(r: &Row) -> Msg {
    Msg {
        id: int(r, 0).unwrap_or(0),
        sid: text(r, 1).unwrap_or_default(),
        msg_id: text(r, 2).unwrap_or_default(),
        role: nonempty(r, 3),
        ts: int(r, 4).unwrap_or(0),
        data: text(r, 5),
    }
}

fn messages(c: &Connection, filter: &str, args: &[&dyn rusqlite::ToSql]) -> Result<Vec<Msg>> {
    let mut st = c.prepare(&format!("{MSG_SELECT} {filter}"))?;
    let rows = st.query_map(args, |r| Ok(msg(r)))?.flatten().collect();
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

fn expand(m: Msg, out: &mut Vec<Pending>, models: &mut Vec<(String, String)>, batch: &mut Batch) {
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
            if let Some(t) = v.get("thinking_content").map(text_of).filter(|t| !t.trim().is_empty()) {
                out.push(Pending::new(&m.sid, m.ts, slot, format!("{base}:r"), Body::Reasoning { text: t }));
            }
            let t = text_of(content);
            if !t.trim().is_empty() {
                out.push(Pending::new(
                    &m.sid,
                    m.ts,
                    slot,
                    base.clone(),
                    Body::AssistantMessage { text: t, model: None },
                ));
            }
            if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
                let usage = Usage {
                    input: num(u, "input_tokens"),
                    output: num(u, "output_tokens"),
                    cache_read: num(u, "cache_read"),
                    cache_write: num(u, "cache_write"),
                    reasoning: 0,
                };
                if usage != Usage::default() {
                    out.push(Pending::new(&m.sid, m.ts, slot, format!("{base}:u"), Body::Usage(usage)));
                }
            }
        }
        None => match diag_model(content) {
            Some(model) => models.push((m.sid, model)),
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

    fn summary(&self, c: &Connection, sc: &Schema, st: State, reset: bool) -> Result<ReadOutput> {
        let mut batch = Batch::default();
        let mut pending = Vec::new();
        let list = sessions(c, sc, "ORDER BY updated_at_ms DESC LIMIT ?1", &[&MAX_SESSIONS])?;
        for s in list {
            let (n, nt) = if s.hot { (HOT_WINDOW, HOT_TURNS) } else { (COLD_WINDOW, COLD_TURNS) };
            let mut patch = s.patch;
            let latest = messages(
                c,
                "WHERE session_id=?1 AND role IS NULL AND data_json LIKE '%model_phase_started%' ORDER BY id DESC LIMIT 1",
                &[&s.id],
            )?;
            let mut models = Vec::new();
            for m in latest {
                expand(m, &mut pending, &mut models, &mut batch);
            }
            patch.model = models.pop().map(|(_, m)| m);
            batch.items.push((s.id.clone(), Record::Meta(patch)));
            let rows = messages(
                c,
                "WHERE session_id=?1 AND role IS NOT NULL AND id<=?2 ORDER BY created_at_ms DESC, id DESC LIMIT ?3",
                &[&s.id, &st.row_id, &n],
            )?;
            for m in rows {
                expand(m, &mut pending, &mut Vec::new(), &mut batch);
            }
            for t in turns(c, "WHERE session_id=?1 ORDER BY accepted_at_ms DESC LIMIT ?2", &[&s.id, &nt])? {
                pending.push(t.start());
                pending.extend(t.end());
            }
        }
        flush(pending, &mut batch);
        Ok(ReadOutput { cursor: self.cursor_for(st), batch, summary: true, reset })
    }

    fn follow(&self, c: &Connection, sc: &Schema, prev: &State, st: State) -> Result<ReadOutput> {
        let mut batch = Batch::default();
        let mut pending = Vec::new();
        let mut models: Vec<(String, String)> = Vec::new();
        for m in messages(c, "WHERE id>?1 AND id<=?2 ORDER BY id", &[&prev.row_id, &st.row_id])? {
            expand(m, &mut pending, &mut models, &mut batch);
        }
        for t in turns(
            c,
            "WHERE accepted_at_ms>?1 AND accepted_at_ms<=?2 ORDER BY accepted_at_ms",
            &[&prev.acc_ms, &st.acc_ms],
        )? {
            pending.push(t.start());
            pending.extend(t.end());
        }
        // Turns completed after being reported as accepted: only their end is new.
        for t in turns(
            c,
            "WHERE completed_at_ms>?1 AND completed_at_ms<=?2 AND accepted_at_ms<=?3 ORDER BY completed_at_ms",
            &[&prev.comp_ms, &st.comp_ms, &prev.acc_ms],
        )? {
            pending.extend(t.end());
        }
        let mut metas: Vec<String> = Vec::new();
        for s in sessions(c, sc, "WHERE updated_at_ms>?1 AND updated_at_ms<=?2", &[&prev.sess_ms, &st.sess_ms])? {
            let mut patch = s.patch;
            patch.model = models.iter().rev().find(|(sid, _)| *sid == s.id).map(|(_, m)| m.clone());
            metas.push(s.id.clone());
            batch.items.push((s.id, Record::Meta(patch)));
        }
        for (sid, model) in models {
            if !metas.contains(&sid) {
                batch.items.push((sid, Record::Meta(MetaPatch { model: Some(model), ..Default::default() })));
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
            acc_ms: max("SELECT COALESCE(MAX(accepted_at_ms),0) FROM local_runtime_turn_ingress")?,
            comp_ms: max("SELECT COALESCE(MAX(completed_at_ms),0) FROM local_runtime_turn_ingress")?,
            sess_ms: max("SELECT COALESCE(MAX(updated_at_ms),0) FROM local_runtime_sessions")?,
            wal_size,
            wal_mtime,
        };
        let prev = cursor.and_then(|c| serde_json::from_value::<State>(c.state.clone()).ok());
        match (cursor, prev) {
            (Some(_), Some(p)) if p.row_id <= st.row_id && p.acc_ms <= st.acc_ms && p.sess_ms <= st.sess_ms => {
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
        let rows = messages(
            &c,
            "WHERE session_id=?1 AND role IS NOT NULL AND created_at_ms<=?2 ORDER BY created_at_ms DESC, id DESC LIMIT ?3",
            &[&session_id, &bound_ms, &limit],
        )?;
        for m in rows {
            expand(m, &mut pending, &mut Vec::new(), &mut batch);
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
        let fx = Fixture::new();
        let db = fx.root().join(DB_NAME);
        let w = Connection::open(&db).unwrap();
        w.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(())).unwrap();
        w.execute_batch(SCHEMA).unwrap();
        let ad = MiniMax { db: db.clone() };
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
        fn turn(&self, id: &str, sid: &str, status: &str, acc: i64, done: Option<i64>) {
            self.w
                .execute(
                    "INSERT INTO local_runtime_turn_ingress(turn_id,session_id,status,accepted_at_ms,completed_at_ms) VALUES(?1,?2,?3,?4,?5)",
                    params![id, sid, status, acc, done],
                )
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
