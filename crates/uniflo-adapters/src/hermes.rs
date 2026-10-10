//! Hermes agent SQLite store (`~/.hermes/state.db`, WAL).
//!
//! `sessions` carries metadata, `messages` the transcript (autoincrement `id` is both the
//! follow cursor and the paging `pos`). Assistant rows fan out into reasoning, text and one
//! tool call per `tool_calls` entry; `tool` rows become tool results; an assistant row whose
//! `finish_reason` is a terminal one (not `tool_calls`) closes the turn.
//!
//! Token and cost columns exist only per session; they become one session-level `usage`
//! event (id `session:usage`, no `pos`) re-emitted with the session row.

use crate::common::{output_with_reasoning, reported_cost};
use crate::sqlite::{col, columns, int, nonempty, open_ro, real, text, wal_sig};
use anyhow::Result;
use rusqlite::{Connection, Row};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uniflo_core::adapter::Batch;
use uniflo_core::util::{file_mtime_ms, home, json_arg, now_ms};
use uniflo_core::{Adapter, Cursor, HarnessInfo, HistoryQuery, MetaPatch, ReadOutput, Record};
use uniflo_schema::{Body, Event, Usage, session_key};

const ID: &str = "hermes";
const HOT_WINDOW: usize = 50;
const COLD_WINDOW: usize = 8;
const HOT_MS: i64 = 24 * 3600 * 1000;

pub fn adapters() -> Vec<Arc<dyn Adapter>> {
    vec![Arc::new(Hermes { db: home().join(".hermes/state.db") })]
}

pub struct Hermes {
    db: PathBuf,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    max_id: i64,
    max_sess: i64,
    wal_size: u64,
    wal_mtime: i64,
}

fn ms(secs: Option<f64>) -> i64 {
    secs.filter(|s| s.is_finite() && *s > 0.0).map_or(0, |s| (s * 1000.0) as i64)
}

/// Where `messages` rows are visible: skips compacted / inactive ones when the columns exist.
struct Schema {
    msg_filter: String,
    msg_select: String,
    sess_select: String,
}

impl Schema {
    fn load(c: &Connection) -> Result<Schema> {
        let m = columns(c, "messages")?;
        let s = columns(c, "sessions")?;
        let mut f = String::new();
        if m.contains("active") {
            f.push_str(" AND COALESCE(active,1)<>0");
        }
        if m.contains("compacted") {
            f.push_str(" AND COALESCE(compacted,0)=0");
        }
        let msg_select = format!(
            "SELECT id, session_id, role, content, tool_call_id, tool_calls, tool_name, timestamp, finish_reason, {}, {} FROM messages",
            col(&m, "reasoning"),
            col(&m, "reasoning_content")
        );
        let sess_select = format!(
            "SELECT id, {}, {}, {}, {}, {}, {}, rowid, {}, {}, {}, {}, {}, {}, {}, {}, {} FROM sessions",
            col(&s, "title"),
            col(&s, "cwd"),
            col(&s, "model"),
            col(&s, "parent_session_id"),
            col(&s, "started_at"),
            col(&s, "title_source"),
            col(&s, "input_tokens"),
            col(&s, "output_tokens"),
            col(&s, "cache_read_tokens"),
            col(&s, "cache_write_tokens"),
            col(&s, "reasoning_tokens"),
            col(&s, "actual_cost_usd"),
            col(&s, "estimated_cost_usd"),
            col(&s, "last_activity_at"),
            col(&s, "ended_at"),
        );
        Ok(Schema { msg_filter: f, msg_select, sess_select })
    }
}

struct SessRow {
    id: String,
    patch: MetaPatch,
    hot: bool,
    usage: Option<Event>,
}

fn sess_row(r: &Row) -> SessRow {
    let id = text(r, 0).unwrap_or_default();
    let started = ms(real(r, 5));
    let parent = nonempty(r, 4).filter(|p| *p != id);
    let rank = if nonempty(r, 6).as_deref() == Some("derived") { 1 } else { 2 };
    let n = |i: usize| int(r, i).unwrap_or(0).max(0) as u64;
    let reasoning = n(12);
    let usage = Usage {
        input: n(8),
        output: output_with_reasoning(n(9), reasoning),
        cache_read: n(10),
        cache_write: n(11),
        reasoning,
        model: nonempty(r, 3),
        cost_usd: reported_cost(real(r, 13).map(Value::from).as_ref())
            .or_else(|| reported_cost(real(r, 14).map(Value::from).as_ref())),
    };
    let usage = (usage.input + usage.output + usage.cache_read + usage.cache_write > 0).then(|| {
        let ts = [ms(real(r, 15)), ms(real(r, 16)), started].into_iter().find(|t| *t > 0).unwrap_or(0);
        Event { pos: None, ..ev(&id, "session:usage".into(), ts, 0, Body::Usage(usage)) }
    });
    SessRow {
        usage,
        patch: MetaPatch {
            parent,
            title: nonempty(r, 1).map(|t| (rank, t)),
            cwd: nonempty(r, 2),
            model: nonempty(r, 3),
            started_at: Some(started).filter(|t| *t > 0),
            updated_at: None,
        },
        hot: started > now_ms() - HOT_MS,
        id,
    }
}

fn sessions(c: &Connection, sc: &Schema, filter: &str, args: &[&dyn rusqlite::ToSql]) -> Result<Vec<SessRow>> {
    let mut st = c.prepare(&format!("{} {filter}", sc.sess_select))?;
    let rows = st.query_map(args, |r| Ok(sess_row(r)))?.flatten().collect();
    Ok(rows)
}

struct Raw {
    id: i64,
    sid: String,
    role: String,
    content: Option<String>,
    call_id: Option<String>,
    calls: Option<String>,
    tool_name: Option<String>,
    ts: i64,
    finish: Option<String>,
    reasoning: Option<String>,
}

fn raw(r: &Row) -> Raw {
    Raw {
        id: r.get(0).unwrap_or(0),
        sid: text(r, 1).unwrap_or_default(),
        role: text(r, 2).unwrap_or_default(),
        content: text(r, 3),
        call_id: nonempty(r, 4),
        calls: nonempty(r, 5),
        tool_name: nonempty(r, 6),
        ts: ms(real(r, 7)),
        finish: nonempty(r, 8),
        reasoning: nonempty(r, 9).or_else(|| nonempty(r, 10)),
    }
}

fn ev(sid: &str, id: String, ts: i64, pos: i64, body: Body) -> Event {
    Event {
        id,
        session: session_key(ID, sid),
        ts,
        pos: Some(pos.max(0) as u64),
        partial: false,
        truncated: false,
        body,
    }
}

fn expand(m: Raw, batch: &mut Batch) {
    let Raw { id, sid, ts, .. } = m;
    let mut out: Vec<Body> = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    let mut push = |suffix: String, b: Body| {
        ids.push(format!("m{id}{suffix}"));
        out.push(b);
    };
    match m.role.as_str() {
        "user" => {
            if let Some(t) = m.content.filter(|t| !t.trim().is_empty()) {
                push(String::new(), Body::UserMessage { text: t, synthetic: false });
            }
        }
        "assistant" => {
            if let Some(t) = m.reasoning {
                push(":r".into(), Body::Reasoning { text: t });
            }
            if let Some(t) = m.content.filter(|t| !t.trim().is_empty()) {
                push(String::new(), Body::AssistantMessage { text: t, model: None });
            }
            if let Some(calls) = m.calls {
                match serde_json::from_str::<Value>(&calls) {
                    Ok(Value::Array(a)) => {
                        for (i, c) in a.iter().enumerate() {
                            let f = c.get("function").unwrap_or(&Value::Null);
                            let call_id = ["call_id", "id"]
                                .iter()
                                .filter_map(|k| c.get(*k).and_then(Value::as_str))
                                .find(|s| !s.is_empty())
                                .map_or_else(|| format!("m{id}:c{i}"), str::to_owned);
                            let name = f.get("name").and_then(Value::as_str).unwrap_or("tool").to_owned();
                            let input = json_arg(f.get("arguments").unwrap_or(&Value::Null));
                            push(format!(":c{i}"), Body::ToolCall { call_id, name, input });
                        }
                    }
                    _ => batch.bad_lines += 1,
                }
            }
            if let Some(f) = m.finish.filter(|f| f != "tool_calls") {
                push(":e".into(), Body::TurnEnd { reason: Some(f) });
            }
        }
        "tool" => {
            let call_id = m.call_id.unwrap_or_else(|| format!("m{id}"));
            push(
                String::new(),
                Body::ToolResult { call_id, name: m.tool_name, output: m.content.unwrap_or_default(), is_error: false },
            );
        }
        other => batch.unknown.push(format!("role={other}")),
    }
    for (eid, body) in ids.into_iter().zip(out) {
        batch.items.push((sid.clone(), Record::Event(ev(&sid, eid, ts, id, body))));
    }
}

fn messages(c: &Connection, sc: &Schema, filter: &str, args: &[&dyn rusqlite::ToSql]) -> Result<Vec<Raw>> {
    let mut st = c.prepare(&format!("{} WHERE 1=1{} {filter}", sc.msg_select, sc.msg_filter))?;
    let rows = st.query_map(args, |r| Ok(raw(r)))?.flatten().collect();
    Ok(rows)
}

impl Hermes {
    fn cursor_for(&self, st: State) -> Cursor {
        let md = std::fs::metadata(&self.db).ok();
        Cursor {
            offset: st.max_id.max(0) as u64,
            size: md.as_ref().map_or(0, |m| m.len()),
            mtime_ms: md.as_ref().map_or(0, file_mtime_ms),
            state: serde_json::to_value(st).unwrap_or_default(),
        }
    }

    fn state(&self, c: &Connection, src: &Path) -> Result<State> {
        let (wal_size, wal_mtime) = wal_sig(src);
        Ok(State {
            max_id: c.query_row("SELECT COALESCE(MAX(id),0) FROM messages", [], |r| r.get(0))?,
            max_sess: c.query_row("SELECT COALESCE(MAX(rowid),0) FROM sessions", [], |r| r.get(0))?,
            wal_size,
            wal_mtime,
        })
    }

    fn summary(&self, c: &Connection, sc: &Schema, st: State, reset: bool) -> Result<ReadOutput> {
        let mut batch = Batch::default();
        for s in sessions(c, sc, "", &[])? {
            batch.items.push((s.id.clone(), Record::Meta(s.patch)));
            if let Some(u) = s.usage {
                batch.items.push((s.id.clone(), Record::Event(u)));
            }
            let n = if s.hot { HOT_WINDOW } else { COLD_WINDOW };
            let mut rows = messages(
                c,
                sc,
                "AND session_id=?1 AND id<=?2 ORDER BY id DESC LIMIT ?3",
                &[&s.id, &st.max_id, &(n as i64)],
            )?;
            rows.reverse();
            for m in rows {
                expand(m, &mut batch);
            }
        }
        Ok(ReadOutput { cursor: self.cursor_for(st), batch, summary: true, reset })
    }

    fn follow(&self, c: &Connection, sc: &Schema, prev: &State, st: State) -> Result<ReadOutput> {
        let mut batch = Batch::default();
        let rows = messages(c, sc, "AND id>?1 AND id<=?2 ORDER BY id", &[&prev.max_id, &st.max_id])?;
        let mut touched: Vec<String> = Vec::new();
        for m in &rows {
            if !touched.contains(&m.sid) {
                touched.push(m.sid.clone());
            }
        }
        for s in sessions(c, sc, "WHERE rowid>?1 AND rowid<=?2", &[&prev.max_sess, &st.max_sess])? {
            if !touched.contains(&s.id) {
                touched.push(s.id.clone());
            }
        }
        let mut usage = Vec::new();
        for id in &touched {
            for s in sessions(c, sc, "WHERE id=?1", &[id])? {
                batch.items.push((s.id.clone(), Record::Meta(s.patch)));
                usage.extend(s.usage.map(|u| (s.id, Record::Event(u))));
            }
        }
        for m in rows {
            expand(m, &mut batch);
        }
        batch.items.extend(usage);
        Ok(ReadOutput { cursor: self.cursor_for(st), batch, summary: false, reset: false })
    }
}

impl Adapter for Hermes {
    fn info(&self) -> HarnessInfo {
        HarnessInfo { id: ID, name: "Hermes" }
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
        let st = self.state(&c, src)?;
        let prev = cursor.and_then(|c| serde_json::from_value::<State>(c.state.clone()).ok());
        match (cursor, prev) {
            (Some(_), Some(p)) if p.max_id <= st.max_id && p.max_sess <= st.max_sess => self.follow(&c, &sc, &p, st),
            (cur, _) => self.summary(&c, &sc, st, cur.is_some()),
        }
    }

    fn read_all(&self, src: &Path, ids: &[String], sink: &mut dyn FnMut(&str, Record)) -> Result<Cursor> {
        let c = open_ro(src)?;
        c.execute_batch("BEGIN")?;
        let sc = Schema::load(&c)?;
        let st = self.state(&c, src)?;
        let mut usage = Vec::new();
        for s in sessions(&c, &sc, "", &[])? {
            sink(&s.id, Record::Meta(s.patch));
            usage.extend(s.usage.map(|u| (s.id, u)));
        }
        for id in ids {
            let mut batch = Batch::default();
            for m in messages(&c, &sc, "AND session_id=?1 AND id<=?2 ORDER BY id", &[id, &st.max_id])? {
                expand(m, &mut batch);
            }
            for (sid, r) in batch.items {
                sink(&sid, r);
            }
            if let Some(i) = usage.iter().position(|(s, _)| s == id) {
                let (sid, u) = usage.swap_remove(i);
                sink(&sid, Record::Event(u));
            }
        }
        Ok(self.cursor_for(st))
    }

    fn history(&self, src: &Path, session_id: &str, q: &HistoryQuery) -> Result<Vec<Event>> {
        let c = open_ro(src)?;
        let sc = Schema::load(&c)?;
        let before = q.before.map_or(i64::MAX, |b| b.min(i64::MAX as u64) as i64);
        let mut rows = messages(
            &c,
            &sc,
            "AND session_id=?1 AND id<?2 ORDER BY id DESC LIMIT ?3",
            &[&session_id, &before, &(q.limit.max(1) as i64)],
        )?;
        rows.reverse();
        let mut batch = Batch::default();
        for m in rows {
            expand(m, &mut batch);
        }
        Ok(batch
            .items
            .into_iter()
            .filter_map(|(_, r)| match r {
                Record::Event(e) => Some(e),
                Record::Meta(_) => None,
            })
            .collect())
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
    use uniflo_schema::Status;

    const SCHEMA: &str = "
        CREATE TABLE sessions (id TEXT PRIMARY KEY, source TEXT NOT NULL, model TEXT, parent_session_id TEXT,
            started_at REAL NOT NULL, ended_at REAL, end_reason TEXT, title TEXT, cwd TEXT, title_source TEXT);
        CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, role TEXT NOT NULL,
            content TEXT, tool_call_id TEXT, tool_calls TEXT, tool_name TEXT, timestamp REAL NOT NULL,
            token_count INTEGER, finish_reason TEXT, reasoning TEXT, reasoning_content TEXT,
            active INTEGER NOT NULL DEFAULT 1, compacted INTEGER NOT NULL DEFAULT 0);";

    struct Db {
        fx: Fixture,
        db: PathBuf,
        w: Connection,
        ad: Hermes,
    }

    fn db() -> Db {
        let fx = Fixture::new();
        let db = fx.root().join("state.db");
        let w = Connection::open(&db).unwrap();
        w.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(())).unwrap();
        w.execute_batch(SCHEMA).unwrap();
        let ad = Hermes { db: db.clone() };
        Db { fx, db, w, ad }
    }

    impl Db {
        fn session(&self, id: &str, started: f64) {
            self.w
                .execute(
                    "INSERT INTO sessions(id,source,model,started_at,title,cwd) VALUES(?1,'cli','m1',?2,'T','/w')",
                    params![id, started],
                )
                .unwrap();
        }
        fn msg(
            &self,
            sid: &str,
            role: &str,
            content: Option<&str>,
            t: f64,
            extra: (Option<&str>, Option<&str>, Option<&str>),
        ) {
            self.w
                .execute(
                    "INSERT INTO messages(session_id,role,content,timestamp,finish_reason,tool_calls,tool_call_id) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    params![sid, role, content, t, extra.0, extra.1, extra.2],
                )
                .unwrap();
        }
    }

    const CALLS: &str = r#"[{"id":"x","call_id":"call_1","type":"function","function":{"name":"bash","arguments":"{\"cmd\":\"ls\"}"}}]"#;

    fn seed(d: &Db) {
        d.session("a", 1_900_000_000.0);
        d.msg("a", "user", Some("hi"), 1_900_000_001.0, (None, None, None));
        d.msg("a", "assistant", Some("sure"), 1_900_000_002.0, (Some("tool_calls"), Some(CALLS), None));
        d.msg("a", "tool", Some("out"), 1_900_000_003.0, (None, None, Some("call_1")));
        d.msg("a", "assistant", Some("done"), 1_900_000_004.0, (Some("stop"), None, None));
        d.session("b", 1_900_000_010.0);
        d.msg("b", "user", Some("go"), 1_900_000_011.0, (None, None, None));
    }

    #[test]
    fn summary_has_meta_events_and_status() {
        let d = db();
        seed(&d);
        let out = d.ad.read(&d.db, None).unwrap();
        assert!(out.summary);
        let g = group(out.batch);
        let a = &g["a"];
        assert_eq!(a.meta.title.as_deref(), Some("T"));
        assert_eq!(a.meta.cwd.as_deref(), Some("/w"));
        assert_eq!(a.meta.model.as_deref(), Some("m1"));
        assert_eq!(a.meta.started_at, Some(1_900_000_000_000));
        assert_eq!(
            kinds(&a.events),
            ["user_message", "assistant_message", "tool_call", "tool_result", "assistant_message", "turn_end"]
        );
        assert_eq!(a.status(), Status::Idle);
        assert_eq!(g["b"].status(), Status::Work);
        let call = a.events.iter().find_map(|e| match &e.body {
            Body::ToolCall { call_id, name, input } => Some((call_id.clone(), name.clone(), input.clone())),
            _ => None,
        });
        assert_eq!(call, Some(("call_1".into(), "bash".into(), serde_json::json!({"cmd":"ls"}))));
        assert!(a.events.iter().any(|e| matches!(&e.body, Body::ToolResult { call_id, .. } if call_id == "call_1")));
    }

    #[test]
    fn session_columns_become_one_session_usage_event() {
        let d = db();
        d.w.execute_batch(
            "ALTER TABLE sessions ADD COLUMN input_tokens INTEGER; ALTER TABLE sessions ADD COLUMN output_tokens INTEGER;
             ALTER TABLE sessions ADD COLUMN cache_read_tokens INTEGER; ALTER TABLE sessions ADD COLUMN reasoning_tokens INTEGER;
             ALTER TABLE sessions ADD COLUMN actual_cost_usd REAL; ALTER TABLE sessions ADD COLUMN estimated_cost_usd REAL;",
        )
        .unwrap();
        seed(&d);
        d.w.execute("UPDATE sessions SET input_tokens=50, output_tokens=8, cache_read_tokens=900, reasoning_tokens=3, estimated_cost_usd=0.04 WHERE id='a'", [])
            .unwrap();
        let usage = |evs: &[Event]| -> Vec<Usage> {
            evs.iter().filter_map(|e| if let Body::Usage(u) = &e.body { Some(u.clone()) } else { None }).collect()
        };
        let g = group(d.ad.read(&d.db, None).unwrap().batch);
        let u = usage(&g["a"].events);
        assert_eq!(u.len(), 1);
        assert_eq!((u[0].input, u[0].output, u[0].cache_read, u[0].reasoning), (50, 8, 900, 3));
        assert_eq!((u[0].cost_usd, u[0].model.as_deref()), (Some(0.04), Some("m1")));
        assert!(usage(&g["b"].events).is_empty(), "no tokens, no event");

        let mut seen = Vec::new();
        d.ad.read_all(&d.db, &["a".into()], &mut |_, r| {
            if let Record::Event(e) = r {
                seen.push(e)
            }
        })
        .unwrap();
        assert_eq!(seen.last().map(|e| e.id.as_str()), Some("session:usage"), "after the full history");
        assert_eq!(usage(&seen).len(), 1);
    }

    #[test]
    fn follow_emits_only_new_rows_and_advances() {
        let d = db();
        seed(&d);
        let first = d.ad.read(&d.db, None).unwrap();
        d.msg("b", "assistant", Some("ok"), 1_900_000_012.0, (Some("stop"), None, None));
        d.session("c", 1_900_000_020.0);
        let second = d.ad.read(&d.db, Some(&first.cursor)).unwrap();
        assert!(!second.summary);
        assert!(second.cursor.offset > first.cursor.offset);
        let g = group(second.batch);
        assert_eq!(g.keys().cloned().collect::<Vec<_>>(), ["b", "c"]);
        assert_eq!(kinds(&g["b"].events), ["assistant_message", "turn_end"]);
        assert!(g["c"].events.is_empty() && g["c"].meta.cwd.is_some());
        let third = d.ad.read(&d.db, Some(&second.cursor)).unwrap();
        assert!(third.batch.items.is_empty());
    }

    #[test]
    fn history_pages_backwards() {
        let d = db();
        d.session("a", 1_900_000_000.0);
        for i in 0..6 {
            d.msg("a", "user", Some(&format!("m{i}")), 1_900_000_001.0 + i as f64, (None, None, None));
        }
        let p1 = d.ad.history(&d.db, "a", &HistoryQuery { before: None, limit: 3 }).unwrap();
        let texts = |v: &[Event]| -> Vec<String> {
            v.iter()
                .map(|e| match &e.body {
                    Body::UserMessage { text, .. } => text.clone(),
                    _ => String::new(),
                })
                .collect()
        };
        assert_eq!(texts(&p1), ["m3", "m4", "m5"]);
        let before = p1[0].pos;
        let p2 = d.ad.history(&d.db, "a", &HistoryQuery { before, limit: 3 }).unwrap();
        assert_eq!(texts(&p2), ["m0", "m1", "m2"]);
        let p3 = d.ad.history(&d.db, "a", &HistoryQuery { before: p2[0].pos, limit: 3 }).unwrap();
        assert!(p3.is_empty());
    }

    #[test]
    fn source_mapping() {
        let d = db();
        let r = d.fx.root();
        for n in ["state.db", "state.db-wal", "state.db-shm"] {
            assert_eq!(d.ad.source_for(&r.join(n)), Some(d.db.clone()), "{n}");
        }
        assert_eq!(d.ad.source_for(&r.join("state-snapshots/state.db")), None);
        assert_eq!(d.ad.source_for(&r.join("other.db")), None);
        assert_eq!(d.ad.discover(), vec![d.db.clone()]);
    }

    #[test]
    fn changed_tracks_db_and_wal() {
        let d = db();
        seed(&d);
        let out = d.ad.read(&d.db, None).unwrap();
        assert!(!d.ad.changed(&d.db, &out.cursor));
        d.msg("b", "user", Some("more"), 1_900_000_030.0, (None, None, None));
        assert!(d.ad.changed(&d.db, &out.cursor));
        let out2 = d.ad.read(&d.db, Some(&out.cursor)).unwrap();
        assert!(!d.ad.changed(&d.db, &out2.cursor));
    }

    #[test]
    fn odd_rows_do_not_panic_and_hidden_rows_are_skipped() {
        let d = db();
        d.session("a", 1_900_000_000.0);
        d.msg("a", "user", None, 1_900_000_001.0, (None, None, None));
        d.msg("a", "assistant", None, 1_900_000_002.0, (None, Some("{not json"), None));
        d.msg("a", "assistant", Some("hidden"), 1_900_000_003.0, (None, None, None));
        d.msg("a", "mystery", Some("?"), 1_900_000_004.0, (None, None, None));
        d.msg("a", "tool", None, 1_900_000_005.0, (None, None, None));
        d.w.execute("UPDATE messages SET active=0 WHERE content='hidden'", []).unwrap();
        let out = d.ad.read(&d.db, None).unwrap();
        assert_eq!(out.batch.bad_lines, 1);
        assert_eq!(out.batch.unknown, ["role=mystery"]);
        let g = group(out.batch);
        assert_eq!(kinds(&g["a"].events), ["tool_result"]);
    }
}
