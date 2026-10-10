//! Craft Agents sessions.
//!
//! Layout: `~/.craft-agent/workspaces/<workspace>/sessions/<session>/session.jsonl`; the
//! session id is `<workspace dir>/<session dir>` (session names are unique per workspace
//! only). Line 1 is the SessionHeader (no `type`): `name`, `workingDirectory`, `model`,
//! `parentSessionId`, `createdAt` / `lastMessageAt`, `hidden`. Every other line is a
//! StoredMessage: `user` / `assistant` / `plan` / `tool` (one row carries `toolInput` and
//! `toolResult`) / `info` / `warning` / `error`; rows with `parentToolUseId` are a
//! sub-agent's internals. An assistant reply with no tool row after it ends the turn.
//!
//! Craft rewrites the whole file (temp file → unlink → rename), so the source is the session
//! directory, every read re-reads the file with `reset`, and while the file is briefly
//! missing the last result stands. `{{SESSION_PATH}}` in any string is the session
//! directory. The workspace `config.json` (holds a server token) is never read. The header's
//! `tokenUsage` is a session total that the Claude engine's own transcript (indexed as
//! `claude`) already counts step by step, so no usage is emitted here.

use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uniflo_core::adapter::Batch;
use uniflo_core::util::{file_mtime_ms, home, json_arg, str_of, string_of, ts};
use uniflo_core::{Adapter, Cursor, HarnessInfo, HistoryQuery, MetaPatch, ReadOutput, Record};
use uniflo_schema::{Body, Event, session_key};

const ID: &str = "craft";
const FILE: &str = "session.jsonl";

pub fn adapters() -> Vec<Arc<dyn Adapter>> {
    vec![Arc::new(Craft { root: home().join(".craft-agent/workspaces") })]
}

pub struct Craft {
    root: PathBuf,
}

fn dirs(p: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(p).map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect()).unwrap_or_default()
}

/// Replace `{{SESSION_PATH}}` in every string.
fn expand_paths(v: &mut Value, dir: &str) {
    match v {
        Value::String(s) if s.contains("{{SESSION_PATH}}") => *s = s.replace("{{SESSION_PATH}}", dir),
        Value::Array(a) => a.iter_mut().for_each(|x| expand_paths(x, dir)),
        Value::Object(o) => o.values_mut().for_each(|x| expand_paths(x, dir)),
        _ => {}
    }
}

fn expand_home(p: String) -> String {
    match p.strip_prefix("~/") {
        Some(rest) => home().join(rest).display().to_string(),
        None if p == "~" => home().display().to_string(),
        None => p,
    }
}

impl Craft {
    /// `<root>/<ws>/sessions/<s>` for any path at or below a session directory.
    fn session_dir(&self, p: &Path) -> Option<PathBuf> {
        let rel = p.strip_prefix(&self.root).ok()?;
        let mut it = rel.iter();
        let (ws, sessions, s) = (it.next()?, it.next()?, it.next()?);
        (sessions == "sessions").then(|| self.root.join(ws).join(sessions).join(s))
    }

    fn native_id(dir: &Path) -> Option<String> {
        let s = dir.file_name()?.to_str()?;
        let ws = dir.parent()?.parent()?.file_name()?.to_str()?;
        Some(format!("{ws}/{s}"))
    }

    /// Parse the whole transcript. `None`: no header or a hidden / draft session (not listed).
    fn parse(&self, dir: &Path, bytes: &[u8]) -> Option<Batch> {
        let id = Self::native_id(dir)?;
        let ws = id.split_once('/').map_or("", |(w, _)| w).to_owned();
        let key = session_key(ID, &id);
        let dir_s = dir.display().to_string();
        let mut batch = Batch { bytes: bytes.len() as u64, ..Default::default() };
        let mut rows: Vec<Value> = Vec::new();
        for line in bytes.split(|b| *b == b'\n') {
            let line = line.trim_ascii();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_slice::<Value>(line) {
                Ok(mut v) => {
                    expand_paths(&mut v, &dir_s);
                    rows.push(v);
                }
                Err(_) => batch.bad_lines += 1,
            }
        }
        let head = rows.first().filter(|h| h.is_object() && h.get("type").is_none())?;
        if head.get("hidden") == Some(&Value::Bool(true)) || head.get("taskDraft") == Some(&Value::Bool(true)) {
            return None;
        }
        let parent = string_of(head, "parentSessionId")
            .or_else(|| {
                let p = str_of(head, "branchFromSessionPath")?;
                p.rsplit(['/', '\\']).find(|s| !s.is_empty()).map(str::to_owned)
            })
            .map(|p| format!("{ws}/{p}"))
            .filter(|p| *p != id);
        let title = string_of(head, "name").map(|t| (2, t)).or_else(|| {
            head.get("branchFromMessageId").is_none().then(|| string_of(head, "preview").map(|t| (1, t))).flatten()
        });
        let meta = MetaPatch {
            parent,
            title,
            cwd: string_of(head, "workingDirectory").or_else(|| string_of(head, "workspaceRootPath")).map(expand_home),
            model: string_of(head, "model").map(|m| m.strip_prefix("pi/").map_or(m.clone(), str::to_owned)),
            started_at: head.get("createdAt").and_then(ts),
            updated_at: head.get("lastMessageAt").and_then(ts),
        };
        batch.items.push((id.clone(), Record::Meta(meta)));
        let model = string_of(head, "model");
        let msgs = &rows[1..];
        for (i, m) in msgs.iter().enumerate() {
            let pos = i as u64 + 1;
            let t = m.get("timestamp").and_then(ts).unwrap_or(0);
            let mid = string_of(m, "id").unwrap_or_else(|| format!("o{pos}"));
            let nested = str_of(m, "parentToolUseId").is_some_and(|p| !p.is_empty());
            let content = str_of(m, "content").unwrap_or("").to_owned();
            let mut out: Vec<(String, Body)> = Vec::new();
            match str_of(m, "type").unwrap_or("") {
                "user" => {
                    let mut text = content;
                    for a in m.get("attachments").and_then(Value::as_array).into_iter().flatten() {
                        if let Some(n) = str_of(a, "name") {
                            text.push_str(&format!("\n[Attached file: {n}]"));
                        }
                    }
                    let synthetic = m.get("hidden") == Some(&Value::Bool(true));
                    if !text.trim().is_empty() {
                        out.push((mid.clone(), Body::UserMessage { text: text.trim_start().to_owned(), synthetic }));
                    }
                }
                "assistant" | "plan" if !nested || str_of(m, "type") == Some("plan") => {
                    if !content.trim().is_empty() {
                        out.push((mid.clone(), Body::AssistantMessage { text: content, model: model.clone() }));
                        // The reply ends the turn unless the agent goes on with a tool.
                        let next = msgs[i + 1..].iter().find(|n| str_of(n, "type") != Some("status"));
                        if next.is_none_or(|n| str_of(n, "type") == Some("user")) {
                            out.push((format!("{mid}:end"), Body::TurnEnd { reason: Some("stop".into()) }));
                        }
                    }
                }
                "tool" if !nested => {
                    let call_id = string_of(m, "toolUseId").unwrap_or_else(|| mid.clone());
                    let name = str_of(m, "toolName").unwrap_or("tool").to_owned();
                    let input = json_arg(m.get("toolInput").unwrap_or(&Value::Null));
                    out.push((
                        format!("{mid}:call"),
                        Body::ToolCall { call_id: call_id.clone(), name: name.clone(), input },
                    ));
                    if let Some(r) = m.get("toolResult").filter(|r| !r.is_null()) {
                        let output = r.as_str().map_or_else(|| r.to_string(), str::to_owned);
                        let is_error =
                            m.get("isError") == Some(&Value::Bool(true)) || str_of(m, "toolStatus") == Some("error");
                        out.push((
                            format!("{mid}:result"),
                            Body::ToolResult { call_id, name: Some(name), output, is_error },
                        ));
                    }
                }
                "assistant" | "tool" => {}
                ty @ ("info" | "warning" | "error" | "auth-request") => {
                    let text =
                        if content.is_empty() { string_of(m, "errorTitle").unwrap_or_default() } else { content };
                    out.push((mid.clone(), Body::System { subtype: ty.to_owned(), text }));
                }
                "status" => {}
                ty => batch.unknown.push(format!("type={ty}")),
            }
            for (eid, body) in out {
                let e = Event {
                    id: eid,
                    session: key.clone(),
                    ts: t,
                    pos: Some(pos),
                    partial: false,
                    truncated: false,
                    body,
                };
                batch.items.push((id.clone(), Record::Event(e)));
            }
        }
        Some(batch)
    }

    fn load(&self, src: &Path) -> Result<Option<(Batch, Cursor)>> {
        let file = src.join(FILE);
        // Between Craft's unlink and rename the file is missing: keep what was read.
        let (bytes, md) = match std::fs::read(&file).and_then(|b| std::fs::metadata(&file).map(|m| (b, m))) {
            Ok(x) => x,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let cursor = Cursor { offset: md.len(), size: md.len(), mtime_ms: file_mtime_ms(&md), state: Value::Null };
        Ok(Some((self.parse(src, &bytes).unwrap_or_default(), cursor)))
    }
}

impl Adapter for Craft {
    fn info(&self) -> HarnessInfo {
        HarnessInfo { id: ID, name: "Craft Agents" }
    }

    fn roots(&self) -> Vec<PathBuf> {
        if self.root.is_dir() { vec![self.root.clone()] } else { Vec::new() }
    }

    /// The session directory for its transcript (and Craft's temp file next to it).
    fn source_for(&self, path: &Path) -> Option<PathBuf> {
        let dir = self.session_dir(path)?;
        let rest = path.strip_prefix(&dir).ok()?;
        let first = rest.iter().next();
        (first.is_none() || first.and_then(|f| f.to_str()).is_some_and(|f| f.starts_with(FILE))).then_some(dir)
    }

    fn discover(&self) -> Vec<PathBuf> {
        dirs(&self.root).iter().flat_map(|ws| dirs(&ws.join("sessions"))).filter(|s| s.join(FILE).is_file()).collect()
    }

    fn read(&self, src: &Path, cursor: Option<&Cursor>) -> Result<ReadOutput> {
        let Some((batch, c)) = self.load(src)? else {
            return Ok(ReadOutput { cursor: cursor.cloned().unwrap_or_default(), ..Default::default() });
        };
        Ok(ReadOutput { cursor: c, batch, summary: cursor.is_none(), reset: cursor.is_some() })
    }

    fn history(&self, src: &Path, _session_id: &str, q: &HistoryQuery) -> Result<Vec<Event>> {
        let Some((batch, _)) = self.load(src)? else { return Ok(Vec::new()) };
        let mut evs: Vec<Event> = batch
            .items
            .into_iter()
            .filter_map(|(_, r)| match r {
                Record::Event(e) => Some(e),
                Record::Meta(_) => None,
            })
            .filter(|e| q.before.is_none_or(|b| e.pos.unwrap_or(0) < b))
            .collect();
        let skip = evs.len().saturating_sub(q.limit.max(1));
        Ok(evs.split_off(skip))
    }

    fn read_all(&self, src: &Path, _sessions: &[String], sink: &mut dyn FnMut(&str, Record)) -> Result<Cursor> {
        let Some((batch, c)) = self.load(src)? else { return Ok(Cursor::default()) };
        for (id, r) in batch.items {
            sink(&id, r);
        }
        Ok(c)
    }

    fn changed(&self, src: &Path, cursor: &Cursor) -> bool {
        std::fs::metadata(src.join(FILE))
            .is_ok_and(|md| md.len() != cursor.size || file_mtime_ms(&md) != cursor.mtime_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testkit::{Fixture, group, kinds};
    use serde_json::json;
    use uniflo_core::{Engine, EngineOptions};
    use uniflo_schema::Status;

    const S: &str = "ws1/sessions/260901-brave-otter";

    fn l(v: Value) -> String {
        format!("{v}\n")
    }

    fn transcript() -> String {
        let mut s = l(json!({"id":"260901-brave-otter","name":"Tidy imports","workingDirectory":"/w/app",
            "model":"pi/claude-sonnet-4.5","parentSessionId":"260831-calm-fox","createdAt":1790000000000i64,
            "lastMessageAt":1790000060000i64,"tokenUsage":{"inputTokens":10,"outputTokens":5}}));
        s +=
            &l(json!({"id":"m1","type":"user","content":"tidy {{SESSION_PATH}}/notes.md","timestamp":1790000001000i64,
            "attachments":[{"name":"a.png"}]}));
        s += &l(json!({"id":"m2","type":"tool","toolUseId":"tu1","toolName":"Read","toolInput":{"path":"x"},
            "toolResult":"ok","timestamp":1790000002000i64}));
        s += &l(json!({"id":"m3","type":"tool","toolUseId":"tu2","toolName":"Bash","toolInput":{},
            "parentToolUseId":"tu1","timestamp":1790000002500i64}));
        s += &l(json!({"id":"m4","type":"assistant","content":"Done.","timestamp":1790000003000i64}));
        s += &l(json!({"id":"m5","type":"status","content":"thinking"}));
        s
    }

    #[test]
    fn header_messages_and_tool_rows() {
        let fx = Fixture::new();
        let a = Craft { root: fx.root().join("workspaces") };
        fx.write("workspaces/ws1/config.json", r#"{"id":"ws1","serverToken":"secret"}"#);
        fx.write(&format!("workspaces/{S}/{FILE}"), &transcript());
        let dir = fx.root().join(format!("workspaces/{S}"));
        assert_eq!(a.discover(), vec![dir.clone()]);
        let out = a.read(&dir, None).unwrap();
        let g = group(out.batch);
        let r = &g["ws1/260901-brave-otter"];
        assert_eq!(kinds(&r.events), ["user_message", "tool_call", "tool_result", "assistant_message", "turn_end"]);
        let expect = format!("tidy {}/notes.md\n[Attached file: a.png]", dir.display());
        assert!(matches!(&r.events[0].body, Body::UserMessage { text, synthetic: false } if *text == expect));
        assert!(
            matches!(&r.events[2].body, Body::ToolResult { call_id, output, .. } if call_id == "tu1" && output == "ok")
        );
        assert_eq!(r.status(), Status::Idle);
        assert_eq!(r.meta.title.as_deref(), Some("Tidy imports"));
        assert_eq!(r.meta.cwd.as_deref(), Some("/w/app"));
        assert_eq!(r.meta.model.as_deref(), Some("claude-sonnet-4.5"));
        assert_eq!(r.meta.parent.as_deref(), Some("ws1/260831-calm-fox"));
        assert_eq!(r.meta.started_at, Some(1790000000000));
        assert!(r.unknown.is_empty(), "{:?}", r.unknown);
        assert!(!r.events.iter().any(|e| matches!(&e.body, Body::Usage(_))));
        // The workspace config is never a source.
        assert_eq!(a.source_for(&fx.root().join("workspaces/ws1/config.json")), None);
        assert_eq!(a.source_for(&dir.join("session.jsonl.tmp")), Some(dir.clone()));
        assert_eq!(a.source_for(&dir.join("attachments/a.png")), None);
    }

    #[tokio::test]
    async fn whole_file_rewrite_keeps_session_and_reads_new_version() {
        let fx = Fixture::new();
        let root = fx.root().join("workspaces");
        let cfg = fx.write("workspaces/ws1/config.json", r#"{"serverToken":"secret"}"#);
        // Unreadable: any attempt to read the workspace config would surface as an error.
        #[cfg(unix)]
        std::fs::set_permissions(&cfg, std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();
        let file = fx.write(&format!("workspaces/{S}/{FILE}"), &transcript());
        let opts = EngineOptions { cache_path: None, usage: false, data_dir: None, ..Default::default() };
        let engine = Engine::new(vec![Arc::new(Craft { root })], opts);
        engine.index();
        let key = "craft:ws1/260901-brave-otter";
        assert_eq!(engine.session(key).unwrap().status, Status::Idle);

        // Craft's rewrite: write a temp file, unlink the original, rename.
        let tmp = file.with_file_name("session.jsonl.tmp");
        let next = transcript()
            + &l(json!({"id":"m6","type":"user","content":"and the tests","timestamp":1790000070000i64}))
            + &l(json!({"id":"m7","type":"assistant","content":"Tests tidied.","timestamp":1790000071000i64}));
        std::fs::write(&tmp, &next).unwrap();
        engine.notify_path(tmp.clone()).await;
        std::fs::remove_file(&file).unwrap();
        engine.notify_path(file.clone()).await;
        assert!(engine.session(key).is_some(), "session survives the missing-file window");
        std::fs::rename(&tmp, &file).unwrap();
        engine.notify_path(file.clone()).await;
        let s = engine.session(key).unwrap();
        assert_eq!(s.status, Status::Idle);
        assert_eq!(engine.stats().read_errors, 0);
        let evs = engine.history(key, &HistoryQuery { before: None, limit: 100 }).unwrap();
        let ids: Vec<&str> = evs.iter().map(|e| e.id.as_str()).collect();
        let mut uniq = ids.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(uniq.len(), ids.len(), "no duplicates");
        assert_eq!(ids.iter().filter(|i| i.starts_with("m6")).count(), 1);
        assert!(matches!(&evs.last().unwrap().body, Body::TurnEnd { .. }));
        assert_eq!(kinds(&evs[evs.len() - 3..]), ["user_message", "assistant_message", "turn_end"]);
    }

    #[test]
    fn rewrite_read_is_a_reset_and_missing_file_keeps_cursor() {
        let fx = Fixture::new();
        let a = Craft { root: fx.root().join("workspaces") };
        let file = fx.write(&format!("workspaces/{S}/{FILE}"), &transcript());
        let dir = file.parent().unwrap().to_path_buf();
        let out = a.read(&dir, None).unwrap();
        assert!(out.summary && !out.reset);
        std::fs::remove_file(&file).unwrap();
        assert!(!a.changed(&dir, &out.cursor), "nothing to read while the file is missing");
        let gap = a.read(&dir, Some(&out.cursor)).unwrap();
        assert!(gap.batch.items.is_empty() && !gap.reset);
        assert_eq!(gap.cursor, out.cursor);
        std::fs::write(&file, transcript() + &l(json!({"id":"m6","type":"user","content":"more"}))).unwrap();
        assert!(a.changed(&dir, &out.cursor));
        let again = a.read(&dir, Some(&gap.cursor)).unwrap();
        assert!(again.reset && !again.summary);
        assert_eq!(group(again.batch)["ws1/260901-brave-otter"].status(), Status::Work);
        let hidden = l(json!({"name":"x","hidden":true})) + &l(json!({"id":"u","type":"user","content":"hi"}));
        std::fs::write(&file, hidden).unwrap();
        assert!(a.read(&dir, None).unwrap().batch.items.is_empty(), "hidden sessions are not listed");
    }
}
