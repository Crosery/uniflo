//! Devin CLI session store: `cli/sessions.db` under the first data root that has one —
//! `$XDG_DATA_HOME/devin`, `~/.local/share/devin`, `~/Library/Application Support/devin`,
//! `%APPDATA%\devin`.
//!
//! `sessions(id, title, working_directory, model, created_at, last_activity_at, hidden,
//! main_chain_id, …)` and `message_nodes(row_id, session_id, node_id, parent_node_id,
//! chat_message, created_at)` (unix seconds). Messages form a tree (retries and edits branch
//! off); the visible transcript is the chain from `main_chain_id` back through
//! `parent_node_id` to the root. Appends that extend the chain are followed by row; a main
//! chain that moved to another branch re-reads the store (`reset`). `hidden = 1` sessions
//! (compaction helpers, deleted) are not listed. `chat_message` is JSON: `role`, `content`,
//! `tool_calls`, `thinking`, `tool_call_id`, `metadata{is_user_input, telemetry.source,
//! generation_model, metrics{input_tokens, output_tokens, cache_read_tokens,
//! cache_creation_tokens}}`; an assistant message without tool calls ends the turn.

use crate::sqlite::{columns, int, nonempty, open_ro, text, wal_sig};
use anyhow::Result;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uniflo_core::adapter::Batch;
use uniflo_core::util::{file_mtime_ms, home, json_arg, str_of, string_of, text_of};
use uniflo_core::{Adapter, Cursor, HarnessInfo, HistoryQuery, MetaPatch, ReadOutput, Record};
use uniflo_schema::{Body, Event, Usage, session_key};

const ID: &str = "devin";
/// Chain nodes per session sampled by a summary read.
const WINDOW: usize = 24;

pub fn adapters() -> Vec<Arc<dyn Adapter>> {
    let h = home();
    let mut roots: Vec<PathBuf> = Vec::new();
    roots.extend(std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()).map(|v| PathBuf::from(v).join("devin")));
    roots.push(h.join(".local/share/devin"));
    roots.push(h.join("Library/Application Support/devin"));
    roots.extend(std::env::var_os("APPDATA").filter(|v| !v.is_empty()).map(|v| PathBuf::from(v).join("devin")));
    let dbs: Vec<PathBuf> = roots.iter().map(|r| r.join("cli").join("sessions.db")).collect();
    let db = dbs.iter().find(|p| p.is_file()).unwrap_or(&dbs[0]).clone();
    vec![Arc::new(Devin { db })]
}

pub struct Devin {
    db: PathBuf,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    max_row: i64,
    /// Visible session → its `main_chain_id` at the last read.
    leaves: BTreeMap<String, Option<i64>>,
    wal_size: u64,
    wal_mtime: i64,
}

struct Schema {
    sess: String,
    row: &'static str,
}

impl Schema {
    fn load(c: &Connection) -> Result<Schema> {
        let s = columns(c, "sessions")?;
        let m = columns(c, "message_nodes")?;
        let opt = |n: &str| if s.contains(n) { n.to_owned() } else { "NULL".to_owned() };
        let hidden = if s.contains("hidden") { " WHERE COALESCE(hidden,0)<>1" } else { "" };
        Ok(Schema {
            sess: format!(
                "SELECT id, {}, {}, {}, {}, {}, {} FROM sessions{hidden}",
                opt("title"),
                opt("working_directory"),
                opt("model"),
                opt("created_at"),
                opt("last_activity_at"),
                opt("main_chain_id")
            ),
            row: if m.contains("row_id") { "row_id" } else { "rowid" },
        })
    }
}

struct Sess {
    id: String,
    patch: MetaPatch,
    model: Option<String>,
    leaf: Option<i64>,
}

fn ms(secs: Option<i64>) -> Option<i64> {
    secs.filter(|s| *s > 0).map(|s| if s > 100_000_000_000 { s } else { s * 1000 })
}

fn sessions(c: &Connection, sc: &Schema) -> Result<Vec<Sess>> {
    let mut st = c.prepare(&sc.sess)?;
    let v = st
        .query_map([], |r| {
            let model = nonempty(r, 3);
            Ok(Sess {
                id: text(r, 0).unwrap_or_default(),
                patch: MetaPatch {
                    title: nonempty(r, 1).map(|t| (2, t)),
                    cwd: nonempty(r, 2),
                    model: model.clone(),
                    started_at: ms(int(r, 4)),
                    updated_at: ms(int(r, 5)),
                    ..Default::default()
                },
                model,
                leaf: int(r, 6),
            })
        })?
        .flatten()
        .collect();
    Ok(v)
}

/// Row ids of the visible chain, root first: `main_chain_id` back to the root, or every
/// node in time order when there is no usable leaf.
fn chain(c: &Connection, sc: &Schema, sid: &str, leaf: Option<i64>) -> Result<Vec<(i64, i64)>> {
    let mut st = c.prepare(&format!(
        "SELECT {}, node_id, parent_node_id FROM message_nodes WHERE session_id=?1 ORDER BY created_at, {}",
        sc.row, sc.row
    ))?;
    let all: Vec<(i64, i64, Option<i64>)> =
        st.query_map([sid], |r| Ok((int(r, 0).unwrap_or(0), int(r, 1).unwrap_or(0), int(r, 2))))?.flatten().collect();
    let by_node: HashMap<i64, usize> = all.iter().enumerate().map(|(i, n)| (n.1, i)).collect();
    let linear = || all.iter().map(|n| (n.0, n.1)).collect();
    let Some(mut at) = leaf.and_then(|l| by_node.get(&l).copied()) else { return Ok(linear()) };
    let mut out = vec![(all[at].0, all[at].1)];
    while let Some(p) = all[at].2 {
        match by_node.get(&p) {
            Some(&i) if out.len() < all.len() => {
                at = i;
                out.push((all[at].0, all[at].1));
            }
            // A dangling parent or a cycle: the tree is unusable, show everything.
            _ => return Ok(linear()),
        }
    }
    out.reverse();
    Ok(out)
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

/// Compaction prompts Devin sends as a user turn.
fn compaction_request(t: &str) -> bool {
    let t = t.trim_start();
    t.starts_with("Conversation to summarize:") || t.starts_with("Now summarize the conversation")
}

fn expand(sid: &str, row: i64, node: i64, ts: i64, msg: &Value, sess_model: Option<&str>, batch: &mut Batch) {
    let mut out: Vec<(String, Body)> = Vec::new();
    let meta = msg.get("metadata").unwrap_or(&Value::Null);
    let content = text_of(msg.get("content").unwrap_or(&Value::Null));
    match str_of(msg, "role").unwrap_or("") {
        "user" => {
            let internal = meta.pointer("/telemetry/source").and_then(Value::as_str).is_some_and(|s| s != "user")
                || meta.get("is_user_input") == Some(&Value::Bool(false))
                || compaction_request(&content);
            if !content.trim().is_empty() {
                out.push((String::new(), Body::UserMessage { text: content, synthetic: internal }));
            }
        }
        "assistant" => {
            if let Some(m) = string_of(meta, "generation_model") {
                batch.items.push((sid.to_owned(), Record::Meta(MetaPatch { model: Some(m), ..Default::default() })));
            }
            let model = string_of(meta, "generation_model").or_else(|| sess_model.map(str::to_owned));
            let thinking = match msg.get("thinking") {
                Some(Value::String(s)) => Some(s.clone()),
                Some(o) => string_of(o, "thinking"),
                None => None,
            };
            if let Some(t) = thinking.filter(|t| !t.trim().is_empty()) {
                out.push((":r".into(), Body::Reasoning { text: t }));
            }
            if content.trim_start().starts_with("<summary>") {
                out.push((String::new(), Body::System { subtype: "compact".into(), text: content }));
            } else if !content.trim().is_empty() {
                out.push((String::new(), Body::AssistantMessage { text: content, model }));
            }
            let calls = msg.get("tool_calls").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
            for (i, call) in calls.iter().enumerate() {
                let f = call.get("function").unwrap_or(call);
                let call_id = string_of(call, "id").unwrap_or_else(|| format!("n{node}:c{i}"));
                let name = str_of(f, "name").unwrap_or("tool").to_owned();
                let input = json_arg(f.get("arguments").unwrap_or(&Value::Null));
                out.push((format!(":c{i}"), Body::ToolCall { call_id, name, input }));
            }
            if calls.is_empty() {
                out.push((":e".into(), Body::TurnEnd { reason: Some("completed".into()) }));
            }
        }
        "tool" => {
            let call_id = str_of(msg, "tool_call_id").unwrap_or("").to_owned();
            out.push((String::new(), Body::ToolResult { call_id, name: None, output: content, is_error: false }));
        }
        "system" => {
            if !content.trim().is_empty() {
                out.push((String::new(), Body::System { subtype: "system".into(), text: content }));
            }
        }
        role => batch.unknown.push(format!("role={role}")),
    }
    if let Some(m) = meta.get("metrics").filter(|m| m.is_object()) {
        let n = |k: &str| m.get(k).and_then(Value::as_u64).unwrap_or(0);
        let usage = Usage {
            input: n("input_tokens"),
            output: n("output_tokens"),
            cache_read: n("cache_read_tokens"),
            cache_write: n("cache_creation_tokens"),
            reasoning: 0,
            model: string_of(meta, "generation_model"),
            cost_usd: None,
        };
        if usage.input + usage.output + usage.cache_read + usage.cache_write > 0 {
            // Before the turn end, like the step it accounts for.
            let at = out.iter().position(|(_, b)| matches!(b, Body::TurnEnd { .. })).unwrap_or(out.len());
            out.insert(at, (":u".into(), Body::Usage(usage)));
        }
    }
    for (suffix, body) in out {
        batch.items.push((sid.to_owned(), Record::Event(ev(sid, format!("n{node}{suffix}"), ts, row, body))));
    }
}

/// Decode the given chain nodes (row id, node id) of one session, in order.
fn emit(c: &Connection, sc: &Schema, s: &Sess, nodes: &[(i64, i64)], batch: &mut Batch) -> Result<()> {
    let mut st =
        c.prepare_cached(&format!("SELECT chat_message, created_at FROM message_nodes WHERE {}=?1", sc.row))?;
    for &(row, node) in nodes {
        let Some((raw, created)) = st.query_row([row], |r| Ok((text(r, 0), int(r, 1)))).ok() else { continue };
        match raw.as_deref().map(serde_json::from_str::<Value>) {
            Some(Ok(msg)) => expand(&s.id, row, node, ms(created).unwrap_or(0), &msg, s.model.as_deref(), batch),
            _ => batch.bad_lines += 1,
        }
    }
    Ok(())
}

impl Devin {
    fn cursor_for(&self, st: State) -> Cursor {
        let md = std::fs::metadata(&self.db).ok();
        Cursor {
            offset: st.max_row.max(0) as u64,
            size: md.as_ref().map_or(0, |m| m.len()),
            mtime_ms: md.as_ref().map_or(0, file_mtime_ms),
            state: serde_json::to_value(st).unwrap_or_default(),
        }
    }

    fn open(&self, src: &Path) -> Result<(Connection, Schema, Vec<Sess>, State)> {
        let c = open_ro(src)?;
        c.execute_batch("BEGIN")?;
        let sc = Schema::load(&c)?;
        let ss = sessions(&c, &sc)?;
        let (wal_size, wal_mtime) = wal_sig(src);
        let st = State {
            max_row: c
                .query_row(&format!("SELECT COALESCE(MAX({}),0) FROM message_nodes", sc.row), [], |r| r.get(0))?,
            leaves: ss.iter().map(|s| (s.id.clone(), s.leaf)).collect(),
            wal_size,
            wal_mtime,
        };
        Ok((c, sc, ss, st))
    }

    fn summary(&self, c: &Connection, sc: &Schema, ss: &[Sess], st: State, reset: bool) -> Result<ReadOutput> {
        let mut batch = Batch::default();
        for s in ss {
            let nodes = chain(c, sc, &s.id, s.leaf)?;
            if nodes.is_empty() {
                continue;
            }
            batch.items.push((s.id.clone(), Record::Meta(s.patch.clone())));
            emit(c, sc, s, &nodes[nodes.len().saturating_sub(WINDOW)..], &mut batch)?;
        }
        Ok(ReadOutput { cursor: self.cursor_for(st), batch, summary: true, reset })
    }

    /// Chain growth since `prev`; `None` when a main chain moved to another branch.
    fn follow(&self, c: &Connection, sc: &Schema, ss: &[Sess], prev: &State) -> Result<Option<Batch>> {
        let mut batch = Batch::default();
        for s in ss {
            let new: Vec<(i64, i64)> = match prev.leaves.get(&s.id) {
                Some(old) if *old == s.leaf && s.leaf.is_some() => continue,
                Some(Some(old)) => {
                    let nodes = chain(c, sc, &s.id, s.leaf)?;
                    match nodes.iter().position(|n| n.1 == *old) {
                        Some(i) if s.leaf.is_some() => nodes[i + 1..].to_vec(),
                        _ => return Ok(None),
                    }
                }
                // No leaf before and after: every node is visible, follow by row.
                Some(None) if s.leaf.is_none() => {
                    chain(c, sc, &s.id, None)?.into_iter().filter(|n| n.0 > prev.max_row).collect()
                }
                Some(None) => return Ok(None),
                None => chain(c, sc, &s.id, s.leaf)?,
            };
            if new.is_empty() {
                continue;
            }
            batch.items.push((s.id.clone(), Record::Meta(s.patch.clone())));
            emit(c, sc, s, &new, &mut batch)?;
        }
        Ok(Some(batch))
    }
}

impl Adapter for Devin {
    fn info(&self) -> HarnessInfo {
        HarnessInfo { id: ID, name: "Devin" }
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
        let (c, sc, ss, st) = self.open(src)?;
        let prev = cursor.and_then(|c| serde_json::from_value::<State>(c.state.clone()).ok());
        if let Some(p) = prev.as_ref().filter(|p| p.max_row <= st.max_row)
            && let Some(batch) = self.follow(&c, &sc, &ss, p)?
        {
            return Ok(ReadOutput { cursor: self.cursor_for(st), batch, summary: false, reset: false });
        }
        self.summary(&c, &sc, &ss, st, cursor.is_some())
    }

    fn read_all(&self, src: &Path, ids: &[String], sink: &mut dyn FnMut(&str, Record)) -> Result<Cursor> {
        let (c, sc, ss, st) = self.open(src)?;
        for s in &ss {
            if ids.contains(&s.id) {
                sink(&s.id, Record::Meta(s.patch.clone()));
                let mut batch = Batch::default();
                emit(&c, &sc, s, &chain(&c, &sc, &s.id, s.leaf)?, &mut batch)?;
                for (sid, r) in batch.items {
                    sink(&sid, r);
                }
            }
        }
        Ok(self.cursor_for(st))
    }

    fn history(&self, src: &Path, session_id: &str, q: &HistoryQuery) -> Result<Vec<Event>> {
        let (c, sc, ss, _) = self.open(src)?;
        let Some(s) = ss.iter().find(|s| s.id == session_id) else { return Ok(Vec::new()) };
        let before = q.before.map_or(i64::MAX, |b| b.min(i64::MAX as u64) as i64);
        let nodes: Vec<(i64, i64)> = chain(&c, &sc, &s.id, s.leaf)?.into_iter().filter(|n| n.0 < before).collect();
        let mut batch = Batch::default();
        emit(&c, &sc, s, &nodes[nodes.len().saturating_sub(q.limit.max(1))..], &mut batch)?;
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
    use crate::common::testkit::{group, kinds};
    use rusqlite::params;
    use serde_json::json;
    use uniflo_schema::Status;

    const SCHEMA: &str = "
        CREATE TABLE sessions (id TEXT PRIMARY KEY, title TEXT, working_directory TEXT, model TEXT, agent_mode TEXT,
            created_at INTEGER, last_activity_at INTEGER, hidden INTEGER DEFAULT 0, main_chain_id INTEGER);
        CREATE TABLE message_nodes (row_id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL,
            node_id INTEGER NOT NULL, parent_node_id INTEGER, chat_message TEXT NOT NULL, created_at INTEGER NOT NULL);";

    struct Db {
        _dir: tempfile::TempDir,
        db: PathBuf,
        w: Connection,
    }

    fn db() -> Db {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("sessions.db");
        let w = Connection::open(&db).unwrap();
        w.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(())).unwrap();
        w.execute_batch(SCHEMA).unwrap();
        Db { _dir: dir, db, w }
    }

    impl Db {
        fn node(&self, sid: &str, node: i64, parent: Option<i64>, msg: Value, t: i64) {
            self.w
                .execute(
                    "INSERT INTO message_nodes(session_id,node_id,parent_node_id,chat_message,created_at) VALUES(?1,?2,?3,?4,?5)",
                    params![sid, node, parent, msg.to_string(), t],
                )
                .unwrap();
        }
        fn leaf(&self, sid: &str, leaf: i64) {
            self.w.execute("UPDATE sessions SET main_chain_id=?2 WHERE id=?1", params![sid, leaf]).unwrap();
        }
    }

    fn texts(evs: &[Event]) -> Vec<String> {
        evs.iter()
            .filter_map(|e| match &e.body {
                Body::UserMessage { text, .. } | Body::AssistantMessage { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn main_chain_hidden_sessions_usage_and_branch_switch() {
        let d = db();
        d.w.execute(
            "INSERT INTO sessions(id,title,working_directory,model,created_at,last_activity_at,hidden) VALUES
             ('d1','Fix flaky test','/w/repo','swe-1',1790000000,1790000100,0),
             ('d2','compaction helper','/w/repo','swe-1',1790000000,1790000100,1)",
            [],
        )
        .unwrap();
        let metrics = json!({"input_tokens":50,"output_tokens":12,"cache_read_tokens":400,"cache_creation_tokens":30});
        d.node("d1", 1, None, json!({"role":"user","content":"fix the flaky test","metadata":{"is_user_input":true,"telemetry":{"source":"user"}}}), 1790000001);
        d.node(
            "d1",
            2,
            Some(1),
            json!({"role":"assistant","content":"","thinking":{"thinking":"look at it","signature":"s"},
                "tool_calls":[{"id":"tc1","name":"run","arguments":{"cmd":"cargo test"}}],
                "metadata":{"generation_model":"swe-1.5","metrics":metrics}}),
            1790000002,
        );
        d.node("d1", 3, Some(2), json!({"role":"tool","tool_call_id":"tc1","content":"1 failed"}), 1790000003);
        // A retry branch off node 3, then the main line.
        d.node("d1", 4, Some(3), json!({"role":"assistant","content":"retry answer","metadata":{}}), 1790000004);
        d.node(
            "d1",
            5,
            Some(3),
            json!({"role":"assistant","content":"main answer","metadata":{"generation_model":"swe-1.5"}}),
            1790000005,
        );
        d.node(
            "d1",
            6,
            Some(5),
            json!({"role":"user","content":"keepalive","metadata":{"telemetry":{"source":"cache_keepalive"}}}),
            1790000006,
        );
        d.node("d2", 1, None, json!({"role":"user","content":"Conversation to summarize: …"}), 1790000007);
        d.leaf("d1", 5);
        d.leaf("d2", 1);
        let a = Devin { db: d.db.clone() };
        let out = a.read(&d.db, None).unwrap();
        let g = group(out.batch);
        assert_eq!(g.keys().collect::<Vec<_>>(), ["d1"], "hidden session is not listed");
        let r = &g["d1"];
        assert_eq!(
            kinds(&r.events),
            ["user_message", "reasoning", "tool_call", "usage", "tool_result", "assistant_message", "turn_end"]
        );
        assert_eq!(
            texts(&r.events),
            ["fix the flaky test", "main answer"],
            "side branch and nodes past the leaf are not shown"
        );
        assert!(matches!(&r.events[3].body, Body::Usage(u)
            if (u.input, u.output, u.cache_read, u.cache_write) == (50, 12, 400, 30) && u.model.as_deref() == Some("swe-1.5")));
        assert!(
            matches!(&r.events[5].body, Body::AssistantMessage { model, .. } if model.as_deref() == Some("swe-1.5"))
        );
        assert!(matches!(&r.events[2].body, Body::ToolCall { input, .. } if input["cmd"] == "cargo test"));
        assert_eq!(r.events[0].ts, 1790000001000);
        assert_eq!(r.status(), Status::Idle);
        assert_eq!(r.meta.title.as_deref(), Some("Fix flaky test"));
        assert_eq!(r.meta.cwd.as_deref(), Some("/w/repo"));
        assert_eq!(r.meta.model.as_deref(), Some("swe-1.5"), "the newest step's model wins over the session's");
        assert_eq!(r.meta.started_at, Some(1790000000000));
        assert!(r.unknown.is_empty() && r.bad_lines == 0);

        // The chain grows by an internal keepalive: followed by row, marked synthetic.
        d.leaf("d1", 6);
        let out2 = a.read(&d.db, Some(&out.cursor)).unwrap();
        assert!(!out2.reset && !out2.summary);
        let g2 = group(out2.batch);
        assert!(matches!(&g2["d1"].events[..], [e] if matches!(&e.body, Body::UserMessage { synthetic: true, .. })));

        // Main chain moves to the retry branch: the store is re-read and matches it.
        d.leaf("d1", 4);
        let out3 = a.read(&d.db, Some(&out2.cursor)).unwrap();
        assert!(out3.reset);
        let g3 = group(out3.batch);
        assert_eq!(texts(&g3["d1"].events), ["fix the flaky test", "retry answer"]);
        let h = a.history(&d.db, "d1", &HistoryQuery { before: None, limit: 50 }).unwrap();
        assert_eq!(texts(&h), ["fix the flaky test", "retry answer"]);
        let h2 = a.history(&d.db, "d1", &HistoryQuery { before: Some(3), limit: 50 }).unwrap();
        assert_eq!(kinds(&h2), ["user_message", "reasoning", "tool_call", "usage"]);
        let ro = open_ro(&d.db).unwrap();
        assert!(ro.execute("DELETE FROM sessions", []).is_err(), "read-only");
    }

    #[test]
    fn old_schema_without_leaf_shows_every_node() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("sessions.db");
        let w = Connection::open(&db).unwrap();
        w.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, title TEXT, working_directory TEXT, model TEXT, created_at INTEGER, last_activity_at INTEGER);
             CREATE TABLE message_nodes (row_id INTEGER PRIMARY KEY, session_id TEXT, node_id INTEGER, parent_node_id INTEGER, chat_message TEXT, created_at INTEGER);
             INSERT INTO sessions VALUES ('o1','Old','/w','m',1790000000,1790000000);
             INSERT INTO message_nodes VALUES (1,'o1',1,NULL,'{\"role\":\"user\",\"content\":\"hi\"}',1790000001);",
        )
        .unwrap();
        let a = Devin { db: db.clone() };
        let out = a.read(&db, None).unwrap();
        w.execute(
            "INSERT INTO message_nodes VALUES (2,'o1',2,1,'{\"role\":\"assistant\",\"content\":\"hello\"}',1790000002)",
            [],
        )
        .unwrap();
        let out2 = a.read(&db, Some(&out.cursor)).unwrap();
        let g = group(out2.batch);
        assert_eq!(kinds(&g["o1"].events), ["assistant_message", "turn_end"]);
        assert_eq!(g["o1"].status(), Status::Idle);
    }
}
