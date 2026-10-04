//! OpenCode-family SQLite stores: OpenCode, Kilo, ZCode and Mimocode share the
//! `session` / `message` / `part` schema (JSON `data` columns, epoch-ms times).
//!
//! - one assistant `message` per LLM call; its `parts` hold text, reasoning, tool state
//!   (`pending → running → completed|error`, updated in place) and step usage
//! - the turn ends when an assistant message completes with `finish != "tool-calls"`
//!
//! Event `pos` = `part.rowid * 4 + slot` (0 part, 1 tool result, 3 message end), so history
//! pages by part rowid. Follow mode reads new rows by rowid and re-reads only parts and
//! messages that were still open, by primary key — no scans of large tables.

use crate::sqlite::{col, columns, int, nonempty, open_ro, source_for_db, text, wal_sig};
use anyhow::Result;
use rusqlite::{Connection, Row};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uniflo_core::adapter::Batch;
use uniflo_core::util::{file_mtime_ms, home, now_ms, str_of, string_of};
use uniflo_core::{Adapter, Cursor, HarnessInfo, HistoryQuery, LiveSession, MetaPatch, ReadOutput, Record};
use uniflo_schema::{Body, Event, Usage, session_key};

/// Sessions updated this recently get their latest parts replayed in a summary (for status).
const HOT_MS: i64 = 7 * 86_400_000;
const SUMMARY_PARTS: i64 = 40;
const OPEN_CAP: usize = 512;

fn data_home() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Some(local) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
            return local;
        }
    }
    home().join(".local/share")
}

pub fn adapters() -> Vec<Arc<dyn Adapter>> {
    let h = home();
    let d = data_home();
    let mk = |id, name, db: PathBuf| Arc::new(OpenCode { info: HarnessInfo { id, name }, db }) as Arc<dyn Adapter>;
    vec![
        mk("opencode", "OpenCode", d.join("opencode/opencode.db")),
        mk("kilo", "Kilo Code", d.join("kilo/kilo.db")),
        mk("zcode", "ZCode", h.join(".zcode/cli/db/db.sqlite")),
        mk("mimocode", "MiMo Code", d.join("mimocode/mimocode.db")),
    ]
}

pub struct OpenCode {
    pub info: HarnessInfo,
    pub db: PathBuf,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct State {
    part_rowid: i64,
    msg_rowid: i64,
    sess_ts: i64,
    open_parts: Vec<String>,
    open_msgs: Vec<String>,
    wal_size: u64,
    wal_mtime: i64,
}

const PART_SELECT: &str = "SELECT p.rowid, p.id, p.message_id, p.session_id, p.time_created, p.data, m.data, \
     (SELECT max(rowid) FROM part WHERE message_id = p.message_id) FROM part p JOIN message m ON m.id = p.message_id";

struct PartRow {
    rowid: i64,
    id: String,
    msg_id: String,
    sid: String,
    ts: i64,
    part: Value,
    msg: Value,
    last_of_msg: bool,
}

fn part_row(r: &Row) -> PartRow {
    let rowid = int(r, 0).unwrap_or(0);
    PartRow {
        rowid,
        id: text(r, 1).unwrap_or_default(),
        msg_id: text(r, 2).unwrap_or_default(),
        sid: text(r, 3).unwrap_or_default(),
        ts: int(r, 4).unwrap_or(0),
        part: text(r, 5).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null),
        msg: text(r, 6).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null),
        last_of_msg: int(r, 7) == Some(rowid),
    }
}

fn parts(c: &Connection, filter: &str, args: &[&dyn rusqlite::ToSql]) -> Result<Vec<PartRow>> {
    let mut st = c.prepare(&format!("{PART_SELECT} {filter}"))?;
    let rows = st.query_map(args, |r| Ok(part_row(r)))?.flatten().collect();
    Ok(rows)
}

fn msg_done(m: &Value) -> bool {
    str_of(m, "role") != Some("assistant")
        || m.pointer("/time/completed").is_some_and(|t| !t.is_null())
        || m.get("error").is_some_and(|e| !e.is_null())
}

struct Emitter<'a> {
    harness: &'a str,
    batch: &'a mut Batch,
    open_parts: Vec<String>,
    open_msgs: Vec<String>,
}

impl Emitter<'_> {
    fn push(&mut self, sid: &str, id: String, ts: i64, pos: i64, partial: bool, body: Body) {
        let e = Event {
            id,
            session: session_key(self.harness, sid),
            ts,
            pos: Some(pos.max(0) as u64),
            partial,
            truncated: false,
            body,
        };
        self.batch.items.push((sid.to_owned(), Record::Event(e)));
    }

    fn part(&mut self, p: &PartRow) {
        let role = str_of(&p.msg, "role").unwrap_or("");
        let done = msg_done(&p.msg);
        let model = string_of(&p.msg, "modelID");
        if role == "assistant"
            && let Some(m) = &model
        {
            self.batch
                .items
                .push((p.sid.clone(), Record::Meta(MetaPatch { model: Some(m.clone()), ..Default::default() })));
        }
        let base = p.rowid * 4;
        let ts = p.part.pointer("/time/start").and_then(Value::as_i64).unwrap_or(p.ts);
        let streaming = !done && p.part.pointer("/time/end").is_none_or(Value::is_null);
        let mut open = false;
        match str_of(&p.part, "type").unwrap_or("") {
            "text" => {
                let text = str_of(&p.part, "text").unwrap_or("").to_owned();
                if role == "user" {
                    let synthetic = p.part.get("synthetic").and_then(Value::as_bool).unwrap_or(false);
                    self.push(&p.sid, p.id.clone(), ts, base, false, Body::UserMessage { text, synthetic });
                } else {
                    open = streaming;
                    self.push(&p.sid, p.id.clone(), ts, base, streaming, Body::AssistantMessage { text, model });
                }
            }
            "reasoning" => {
                open = streaming;
                let text = str_of(&p.part, "text").unwrap_or("").to_owned();
                self.push(&p.sid, p.id.clone(), ts, base, streaming, Body::Reasoning { text });
            }
            "tool" => {
                let st = p.part.get("state").unwrap_or(&Value::Null);
                let status = str_of(st, "status").unwrap_or("");
                let finished = matches!(status, "completed" | "error");
                open = !finished;
                let call_id = str_of(&p.part, "callID").unwrap_or(&p.id).to_owned();
                let name = str_of(&p.part, "tool").unwrap_or("").to_owned();
                let input = st.get("input").cloned().unwrap_or(Value::Null);
                let call = Body::ToolCall { call_id: call_id.clone(), name: name.clone(), input };
                self.push(&p.sid, p.id.clone(), ts, base, !finished, call);
                if finished {
                    let output = string_of(st, "output").or_else(|| string_of(st, "error")).unwrap_or_default();
                    let end = st.pointer("/time/end").and_then(Value::as_i64).unwrap_or(ts);
                    let res = Body::ToolResult { call_id, name: Some(name), output, is_error: status == "error" };
                    self.push(&p.sid, format!("{}:r", p.id), end, base + 1, false, res);
                }
            }
            "step-finish" => {
                let t = p.part.get("tokens").unwrap_or(&Value::Null);
                let n = |ptr: &str| t.pointer(ptr).and_then(Value::as_u64).unwrap_or(0);
                let usage = Usage {
                    input: n("/input"),
                    output: n("/output"),
                    cache_read: n("/cache/read"),
                    cache_write: n("/cache/write"),
                    reasoning: n("/reasoning"),
                };
                self.push(&p.sid, p.id.clone(), ts, base, false, Body::Usage(usage));
            }
            "patch" => {
                let files: Vec<&str> = p
                    .part
                    .get("files")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                self.push(
                    &p.sid,
                    p.id.clone(),
                    ts,
                    base,
                    false,
                    Body::System { subtype: "patch".into(), text: files.join("\n") },
                );
            }
            "file" => {
                let name = string_of(&p.part, "filename").or_else(|| string_of(&p.part, "url")).unwrap_or_default();
                self.push(&p.sid, p.id.clone(), ts, base, false, Body::System { subtype: "file".into(), text: name });
            }
            "compaction" => self.push(
                &p.sid,
                p.id.clone(),
                ts,
                base,
                false,
                Body::System { subtype: "compact".into(), text: String::new() },
            ),
            "step-start" | "snapshot" | "agent" | "subtask" | "retry" | "timeline" => {}
            other => self.batch.unknown.push(format!("part={other}")),
        }
        if open && self.open_parts.len() < OPEN_CAP && !self.open_parts.contains(&p.id) {
            self.open_parts.push(p.id.clone());
        }
        if p.last_of_msg {
            self.message_end(&p.sid, &p.msg_id, &p.msg, base + 3);
        }
    }

    /// Error / turn-end events of a completed assistant message, or remember it as open.
    fn message_end(&mut self, sid: &str, msg_id: &str, m: &Value, pos: i64) {
        if str_of(m, "role") != Some("assistant") {
            return;
        }
        if !msg_done(m) {
            if self.open_msgs.len() < OPEN_CAP && !self.open_msgs.iter().any(|x| x == msg_id) {
                self.open_msgs.push(msg_id.to_owned());
            }
            return;
        }
        let ts = m
            .pointer("/time/completed")
            .and_then(Value::as_i64)
            .or_else(|| m.pointer("/time/created").and_then(Value::as_i64))
            .unwrap_or(0);
        let mut reason = string_of(m, "finish");
        if let Some(err) = m.get("error").filter(|e| !e.is_null()) {
            let name = str_of(err, "name").unwrap_or("error");
            let msg = err.pointer("/data/message").and_then(Value::as_str).unwrap_or("");
            self.push(
                sid,
                format!("{msg_id}:err"),
                ts,
                pos - 1,
                false,
                Body::System { subtype: "error".into(), text: format!("{name}: {msg}") },
            );
            reason = Some(if name == "MessageAbortedError" { "aborted".into() } else { "error".into() });
        }
        if reason.as_deref() != Some("tool-calls") {
            self.push(sid, format!("{msg_id}:end"), ts, pos, false, Body::TurnEnd { reason });
        }
    }
}

struct SessSchema {
    select: String,
}

impl SessSchema {
    fn load(c: &Connection) -> Result<Self> {
        let s = columns(c, "session")?;
        Ok(SessSchema {
            select: format!(
                "SELECT id, {}, {}, title, time_created, time_updated FROM session",
                col(&s, "parent_id"),
                col(&s, "directory")
            ),
        })
    }
}

fn sess_meta(r: &Row) -> (String, MetaPatch, i64) {
    let id = text(r, 0).unwrap_or_default();
    let updated = int(r, 5).unwrap_or(0);
    let patch = MetaPatch {
        parent: nonempty(r, 1).filter(|p| *p != id),
        title: nonempty(r, 3).map(|t| (2, t)),
        cwd: nonempty(r, 2),
        model: None,
        started_at: int(r, 4).filter(|t| *t > 0),
        updated_at: Some(updated).filter(|t| *t > 0),
    };
    (id, patch, updated)
}

fn max_of(c: &Connection, sql: &str) -> Result<i64> {
    Ok(c.query_row(sql, [], |r| r.get::<_, Option<i64>>(0))?.unwrap_or(0))
}

impl OpenCode {
    fn snapshot_state(&self, c: &Connection, src: &Path) -> Result<State> {
        let (wal_size, wal_mtime) = wal_sig(src);
        Ok(State {
            part_rowid: max_of(c, "SELECT max(rowid) FROM part")?,
            msg_rowid: max_of(c, "SELECT max(rowid) FROM message")?,
            sess_ts: max_of(c, "SELECT max(time_updated) FROM session")?,
            open_parts: Vec::new(),
            open_msgs: Vec::new(),
            wal_size,
            wal_mtime,
        })
    }

    fn cursor(&self, src: &Path, st: &State) -> Cursor {
        let md = std::fs::metadata(src).ok();
        Cursor {
            offset: st.part_rowid.max(0) as u64,
            size: md.as_ref().map_or(0, |m| m.len()),
            mtime_ms: md.as_ref().map_or(0, file_mtime_ms),
            state: serde_json::to_value(st).unwrap_or_default(),
        }
    }

    fn summary(&self, c: &Connection, mut st: State, reset: bool, src: &Path) -> Result<ReadOutput> {
        let mut batch = Batch::default();
        let ss = SessSchema::load(c)?;
        let mut sessions = Vec::new();
        {
            let mut q = c.prepare(&ss.select)?;
            for row in q.query_map([], |r| Ok(sess_meta(r)))?.flatten() {
                sessions.push(row);
            }
        }
        let hot_after = now_ms() - HOT_MS;
        let mut em =
            Emitter { harness: self.info.id, batch: &mut batch, open_parts: Vec::new(), open_msgs: Vec::new() };
        for (id, patch, updated) in sessions {
            em.batch.items.push((id.clone(), Record::Meta(patch)));
            if updated < hot_after {
                continue;
            }
            let mut rows = parts(
                c,
                "WHERE p.session_id = ?1 AND p.rowid <= ?2 ORDER BY p.rowid DESC LIMIT ?3",
                &[&id, &st.part_rowid, &SUMMARY_PARTS],
            )?;
            rows.reverse();
            for p in &rows {
                em.part(p);
            }
        }
        st.open_parts = em.open_parts;
        st.open_msgs = em.open_msgs;
        Ok(ReadOutput { cursor: self.cursor(src, &st), batch, summary: true, reset })
    }

    fn follow(&self, c: &Connection, prev: State, mut st: State, src: &Path) -> Result<ReadOutput> {
        let mut batch = Batch::default();
        let ss = SessSchema::load(c)?;
        {
            let mut q = c.prepare(&format!("{} WHERE time_updated > ?1 AND time_updated <= ?2", ss.select))?;
            for (id, patch, _) in q.query_map([prev.sess_ts, st.sess_ts], |r| Ok(sess_meta(r)))?.flatten() {
                batch.items.push((id, Record::Meta(patch)));
            }
        }
        let mut em =
            Emitter { harness: self.info.id, batch: &mut batch, open_parts: Vec::new(), open_msgs: Vec::new() };
        // Parts that were still streaming, re-read by primary key (upserts).
        for id in &prev.open_parts {
            for p in parts(c, "WHERE p.id = ?1 AND p.rowid <= ?2", &[id, &prev.part_rowid])? {
                em.part(&p);
            }
        }
        for p in parts(c, "WHERE p.rowid > ?1 AND p.rowid <= ?2 ORDER BY p.rowid", &[&prev.part_rowid, &st.part_rowid])?
        {
            em.part(&p);
        }
        // Messages that were open, plus new message rows that have no parts yet.
        let mut q = c.prepare(
            "SELECT m.id, m.session_id, m.data, (SELECT max(rowid) FROM part WHERE message_id = m.id) FROM message m WHERE m.id = ?1",
        )?;
        let mut check: Vec<String> = prev.open_msgs.clone();
        {
            let mut n = c.prepare("SELECT id FROM message WHERE rowid > ?1 AND rowid <= ?2")?;
            for id in n.query_map([prev.msg_rowid, st.msg_rowid], |r| r.get::<_, String>(0))?.flatten() {
                if !check.contains(&id) {
                    check.push(id);
                }
            }
        }
        for id in check {
            if em.open_msgs.contains(&id) {
                continue;
            }
            let row = q.query_row([&id], |r| Ok((text(r, 1).unwrap_or_default(), text(r, 2), int(r, 3))));
            let Ok((sid, Some(data), last)) = row else { continue };
            let Ok(m) = serde_json::from_str::<Value>(&data) else { continue };
            let pos = last.map_or(0, |r| r * 4 + 3);
            // A message whose last part arrived in this batch already emitted its end.
            if last.is_some_and(|r| r > prev.part_rowid) && msg_done(&m) {
                continue;
            }
            em.message_end(&sid, &id, &m, pos);
        }
        st.open_parts = em.open_parts;
        st.open_msgs = em.open_msgs;
        Ok(ReadOutput { cursor: self.cursor(src, &st), batch, summary: false, reset: false })
    }
}

impl Adapter for OpenCode {
    fn info(&self) -> HarnessInfo {
        self.info
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.db.parent().filter(|p| p.is_dir()).map(Path::to_path_buf).into_iter().collect()
    }

    fn source_for(&self, path: &Path) -> Option<PathBuf> {
        source_for_db(&self.db, path)
    }

    fn discover(&self) -> Vec<PathBuf> {
        if self.db.is_file() { vec![self.db.clone()] } else { Vec::new() }
    }

    fn read(&self, src: &Path, cursor: Option<&Cursor>) -> Result<ReadOutput> {
        let c = open_ro(src)?;
        c.execute_batch("BEGIN")?;
        let st = self.snapshot_state(&c, src)?;
        let prev = cursor.and_then(|cur| serde_json::from_value::<State>(cur.state.clone()).ok());
        let out = match prev {
            Some(p) if p.part_rowid <= st.part_rowid && p.msg_rowid <= st.msg_rowid => self.follow(&c, p, st, src),
            Some(_) => self.summary(&c, st, true, src),
            None => self.summary(&c, st, false, src),
        };
        c.execute_batch("COMMIT")?;
        out
    }

    fn history(&self, src: &Path, session_id: &str, q: &HistoryQuery) -> Result<Vec<Event>> {
        let c = open_ro(src)?;
        let before = q.before.map_or(i64::MAX, |b| (b / 4) as i64);
        let limit = q.limit.max(1) as i64;
        let mut rows = parts(
            &c,
            "WHERE p.session_id = ?1 AND p.rowid < ?2 ORDER BY p.rowid DESC LIMIT ?3",
            &[&session_id, &before, &limit],
        )?;
        rows.reverse();
        let mut batch = Batch::default();
        let mut em =
            Emitter { harness: self.info.id, batch: &mut batch, open_parts: Vec::new(), open_msgs: Vec::new() };
        for p in &rows {
            em.part(p);
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
        match serde_json::from_value::<State>(cursor.state.clone()) {
            Ok(st) => (st.wal_size, st.wal_mtime) != wal_sig(src),
            Err(_) => true,
        }
    }

    fn live(&self) -> Option<Vec<LiveSession>> {
        if !self.db.is_file() {
            return None;
        }
        let pred: fn(&str) -> bool = match self.info.id {
            "kilo" => is_kilo,
            "mimocode" => is_mimocode,
            "zcode" => is_zcode,
            _ => is_opencode,
        };
        let procs = uniflo_core::procs::list(pred, None);
        if procs.is_empty() {
            return Some(Vec::new());
        }
        let c = open_ro(&self.db).ok()?;
        let mut out = Vec::new();
        for p in procs {
            let Some(cwd) = p.cwd.as_ref().and_then(|c| c.to_str()) else { continue };
            let mut stmt =
                c.prepare("SELECT id FROM session WHERE directory = ?1 ORDER BY time_updated DESC LIMIT 1").ok()?;
            let sid: Option<String> = stmt.query_row([&cwd], |r| Ok(text(r, 0))).ok().flatten();
            if let Some(id) = sid {
                out.push(LiveSession { id, pid: p.pid, status: None });
            }
        }
        Some(out)
    }
}

fn is_kilo(args: &str) -> bool {
    is_proc("kilo", args)
}

fn is_opencode(args: &str) -> bool {
    is_proc("opencode", args)
}

fn is_mimocode(args: &str) -> bool {
    is_proc("mimocode", args)
}

fn is_zcode(args: &str) -> bool {
    is_proc("zcode", args)
}

fn is_proc(id: &str, args: &str) -> bool {
    let mut it = args.split_whitespace();
    let Some(first) = it.next() else { return false };
    let base = first.rsplit('/').next().unwrap_or(first);
    let target = if matches!(base, "bun" | "node" | "deno" | "tsx") { it.next().unwrap_or("") } else { first };
    let target_base = target.rsplit('/').next().unwrap_or(target);
    match id {
        "kilo" => target_base == "kilo" || target_base == "kilocode",
        "opencode" => target_base == "opencode",
        "mimocode" => target_base == "mimocode",
        "zcode" => target_base == "zcode",
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testkit::{group, kinds};
    use rusqlite::params;
    use serde_json::json;
    use uniflo_schema::Status;

    struct Db {
        _dir: tempfile::TempDir,
        path: PathBuf,
        w: Connection,
        ad: OpenCode,
    }

    fn db() -> Db {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.db");
        let w = Connection::open(&path).unwrap();
        w.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT, slug TEXT, directory TEXT, title TEXT,
               version TEXT, time_created INTEGER, time_updated INTEGER);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);",
        )
        .unwrap();
        let ad = OpenCode { info: HarnessInfo { id: "opencode", name: "OpenCode" }, db: path.clone() };
        Db { _dir: dir, path, w, ad }
    }

    impl Db {
        fn session(&self, id: &str, parent: Option<&str>, updated: i64) {
            self.w
                .execute(
                    "INSERT OR REPLACE INTO session VALUES (?1,'p',?2,'s','/work','Title '||?1,'1',?3,?3)",
                    params![id, parent, updated],
                )
                .unwrap();
        }
        fn msg(&self, id: &str, sid: &str, data: Value) {
            let t = now_ms();
            self.w
                .execute(
                    "INSERT OR REPLACE INTO message VALUES (?1,?2,?3,?3,?4)",
                    params![id, sid, t, data.to_string()],
                )
                .unwrap();
        }
        fn part(&self, id: &str, mid: &str, sid: &str, data: Value) {
            let t = now_ms();
            self.w
                .execute("INSERT INTO part VALUES (?1,?2,?3,?4,?4,?5)", params![id, mid, sid, t, data.to_string()])
                .unwrap();
        }
        fn update_part(&self, id: &str, data: Value) {
            self.w
                .execute(
                    "UPDATE part SET data=?2, time_updated=time_updated+1 WHERE id=?1",
                    params![id, data.to_string()],
                )
                .unwrap();
        }
    }

    fn seed_turn(d: &Db) {
        let now = now_ms();
        d.session("ses_1", None, now);
        d.msg("msg_u", "ses_1", json!({"role":"user","time":{"created":now}}));
        d.part("prt_1", "msg_u", "ses_1", json!({"type":"text","text":"refactor it"}));
        d.msg("msg_a", "ses_1", json!({"role":"assistant","modelID":"glm","time":{"created":now}}));
        d.part("prt_2", "msg_a", "ses_1", json!({"type":"reasoning","text":"plan","time":{"start":now,"end":now}}));
        d.part("prt_3", "msg_a", "ses_1", json!({"type":"tool","callID":"call_1","tool":"bash","state":{"status":"running","input":{"command":"ls"}}}));
    }

    #[test]
    fn summary_maps_parts_and_tracks_open_work() {
        let d = db();
        seed_turn(&d);
        d.session("ses_old", Some("ses_1"), 1_000);
        let out = d.ad.read(&d.path, None).unwrap();
        assert!(out.summary);
        let g = group(out.batch);
        let s = &g["ses_1"];
        assert_eq!(kinds(&s.events), vec!["user_message", "reasoning", "tool_call"]);
        assert!(s.events[2].partial);
        assert_eq!(s.status(), Status::Work);
        assert_eq!(s.meta.cwd.as_deref(), Some("/work"));
        assert_eq!(s.meta.model.as_deref(), Some("glm"));
        assert_eq!(g["ses_old"].meta.parent.as_deref(), Some("ses_1"));
        assert!(g["ses_old"].events.is_empty(), "cold sessions only contribute metadata");
        let st: State = serde_json::from_value(out.cursor.state).unwrap();
        assert_eq!(st.open_parts, vec!["prt_3"]);
        assert_eq!(st.open_msgs, vec!["msg_a"]);
    }

    #[test]
    fn follow_upserts_open_parts_and_closes_turn() {
        let d = db();
        seed_turn(&d);
        let first = d.ad.read(&d.path, None).unwrap();
        assert!(!d.ad.changed(&d.path, &first.cursor));
        d.update_part("prt_3", json!({"type":"tool","callID":"call_1","tool":"bash","state":{"status":"completed","input":{"command":"ls"},"output":"a.rs","time":{"start":1,"end":2}}}));
        d.part("prt_4", "msg_a", "ses_1", json!({"type":"step-finish","reason":"tool-calls","tokens":{"input":9,"output":3,"reasoning":1,"cache":{"read":4,"write":0}}}));
        let t = now_ms();
        d.w.execute(
            "UPDATE message SET data=?2 WHERE id=?1",
            params![
                "msg_a",
                json!({"role":"assistant","modelID":"glm","finish":"tool-calls","time":{"created":t,"completed":t}})
                    .to_string()
            ],
        )
        .unwrap();
        d.msg("msg_b", "ses_1", json!({"role":"assistant","modelID":"glm","time":{"created":t}}));
        d.part("prt_5", "msg_b", "ses_1", json!({"type":"text","text":"done","time":{"start":t,"end":t}}));
        d.w.execute(
            "UPDATE message SET data=?2 WHERE id=?1",
            params![
                "msg_b",
                json!({"role":"assistant","finish":"stop","time":{"created":t,"completed":t}}).to_string()
            ],
        )
        .unwrap();
        assert!(d.ad.changed(&d.path, &first.cursor));

        let out = d.ad.read(&d.path, Some(&first.cursor)).unwrap();
        assert!(!out.summary);
        let g = group(out.batch);
        let s = &g["ses_1"];
        assert_eq!(kinds(&s.events), vec!["tool_call", "tool_result", "usage", "assistant_message", "turn_end"]);
        assert!(!s.events[0].partial, "tool call upserted as finished");
        assert!(
            matches!(&s.events[1].body, Body::ToolResult { output, call_id, .. } if output == "a.rs" && call_id == "call_1")
        );
        assert!(matches!(&s.events[2].body, Body::Usage(u) if u.input == 9 && u.cache_read == 4));
        assert!(matches!(&s.events[4].body, Body::TurnEnd { reason: Some(r) } if r == "stop"));
        let st: State = serde_json::from_value(out.cursor.state.clone()).unwrap();
        assert!(st.open_parts.is_empty() && st.open_msgs.is_empty());

        let again = d.ad.read(&d.path, Some(&out.cursor)).unwrap();
        assert!(again.batch.items.iter().all(|(_, r)| matches!(r, Record::Meta(_))), "no duplicate events");
    }

    #[test]
    fn errors_end_the_turn_and_history_pages() {
        let d = db();
        let now = now_ms();
        d.session("ses_e", None, now);
        for i in 0..10 {
            d.msg(&format!("u{i}"), "ses_e", json!({"role":"user"}));
            d.part(&format!("p{i}"), &format!("u{i}"), "ses_e", json!({"type":"text","text":format!("q{i}")}));
        }
        d.msg("a", "ses_e", json!({"role":"assistant","error":{"name":"MessageAbortedError","data":{"message":"stop"}},"time":{"created":now}}));
        d.part("pa", "a", "ses_e", json!({"type":"text","text":"partial answer","time":{"start":now}}));
        let g = group(d.ad.read(&d.path, None).unwrap().batch);
        let s = &g["ses_e"];
        assert_eq!(s.status(), Status::Idle);
        assert!(matches!(&s.events.last().unwrap().body, Body::TurnEnd { reason: Some(r) } if r == "aborted"));

        let page1 = d.ad.history(&d.path, "ses_e", &HistoryQuery { before: None, limit: 4 }).unwrap();
        assert_eq!(
            kinds(&page1),
            vec!["user_message", "user_message", "user_message", "assistant_message", "system", "turn_end"]
        );
        let page2 = d.ad.history(&d.path, "ses_e", &HistoryQuery { before: page1[0].pos, limit: 100 }).unwrap();
        assert_eq!(page2.len(), 7);
        assert!(page2.last().unwrap().pos < page1[0].pos);
    }

    #[test]
    fn source_mapping() {
        let d = db();
        let dir = d.path.parent().unwrap();
        assert_eq!(d.ad.source_for(&dir.join("opencode.db-wal")), Some(d.path.clone()));
        assert_eq!(d.ad.source_for(&dir.join("snapshot/x")), None);
        assert_eq!(d.ad.discover(), vec![d.path.clone()]);
    }

    #[test]
    fn kilo_adapter_properties_and_process_matching() {
        assert!(is_kilo("kilo"));
        assert!(is_kilo("/usr/local/bin/kilo"));
        assert!(is_kilo("node /usr/local/bin/kilo"));
        assert!(is_kilo("bun /Users/user/.bun/bin/kilocode"));
        assert!(!is_kilo("vim /tmp/kilo"));
    }
}
