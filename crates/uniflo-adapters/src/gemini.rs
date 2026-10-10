//! Gemini CLI chats: `~/.gemini/tmp/<project>/chats/session-*.jsonl` (append-only with
//! in-place message updates) and legacy `session-*.json` (whole document).
//!
//! JSONL lines are a header (`sessionId`, `startTime`…), full message records keyed by
//! `id` (re-written as the message streams / tools finish) and `{"$set":{"messages":[…]}}`
//! snapshots. Message ids are reused as event ids, so updates become upserts.
//! `<project>` maps back to a cwd through `~/.gemini/projects.json`.

use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use uniflo_core::util::{home, json_arg, str_of, string_of, text_of, ts};
use uniflo_core::{Cx, HarnessInfo, JsonlAdapter, LineDecoder, SourceId};
use uniflo_schema::{Body, Usage};

pub struct Gemini {
    pub root: PathBuf,
    pub projects_file: PathBuf,
    projects: RwLock<HashMap<String, String>>,
}

pub fn adapters() -> Vec<std::sync::Arc<dyn uniflo_core::Adapter>> {
    vec![std::sync::Arc::new(gemini())]
}

pub fn gemini() -> JsonlAdapter<Gemini> {
    let h = home();
    JsonlAdapter::new(Gemini::new(h.join(".gemini/tmp"), h.join(".gemini/projects.json")))
}

impl Gemini {
    pub fn new(root: PathBuf, projects_file: PathBuf) -> Self {
        Gemini { root, projects_file, projects: RwLock::new(HashMap::new()) }
    }

    /// `tmp/<name>/chats/x` → cwd registered for `<name>`.
    fn cwd_for(&self, src: &Path) -> Option<String> {
        let name = src.strip_prefix(&self.root).ok()?.iter().next()?.to_str()?.to_owned();
        if let Some(c) = self.projects.read().unwrap().get(&name) {
            return Some(c.clone());
        }
        let v: Value = serde_json::from_slice(&std::fs::read(&self.projects_file).ok()?).ok()?;
        let map: HashMap<String, String> = v
            .get("projects")?
            .as_object()?
            .iter()
            .filter_map(|(path, n)| Some((n.as_str()?.to_owned(), path.clone())))
            .collect();
        let found = map.get(&name).cloned();
        *self.projects.write().unwrap() = map;
        found
    }
}

impl LineDecoder for Gemini {
    type State = ();

    fn info(&self) -> HarnessInfo {
        HarnessInfo { id: "gemini", name: "Gemini CLI" }
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    fn is_source(&self, p: &Path) -> bool {
        p.starts_with(&self.root)
            && p.parent().and_then(|d| d.file_name()).is_some_and(|d| d == "chats")
            && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("session-"))
            && p.extension().is_some_and(|e| e == "jsonl" || e == "json")
    }

    fn cleanup_targets(&self, src: &Path) -> Option<Vec<PathBuf>> {
        uniflo_core::cleanup::targets::file(src)
    }

    fn identify(&self, p: &Path) -> Option<SourceId> {
        Some(SourceId { id: p.file_stem()?.to_str()?.to_owned(), parent: None })
    }

    fn whole_file(&self, p: &Path) -> bool {
        p.extension().is_some_and(|e| e == "json")
    }

    fn max_depth(&self) -> usize {
        3
    }

    fn decode(&self, v: &Value, cx: &mut Cx<'_, ()>) {
        if v.get("sessionId").is_some() && v.get("startTime").is_some() {
            let cwd = self.cwd_for(cx.src);
            let m = cx.meta();
            m.started_at = v.get("startTime").and_then(ts);
            m.cwd = cwd;
            if let Some(msgs) = v.get("messages").and_then(Value::as_array) {
                for msg in msgs {
                    message(msg, cx);
                }
            }
            return;
        }
        if let Some(set) = v.get("$set") {
            for msg in set.get("messages").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default() {
                message(msg, cx);
            }
            return;
        }
        message(v, cx);
    }
}

fn message(m: &Value, cx: &mut Cx<'_, ()>) {
    let id = str_of(m, "id").unwrap_or("").to_owned();
    let t = m.get("timestamp").and_then(ts).unwrap_or(0);
    match str_of(m, "type").unwrap_or("") {
        "user" => {
            let text = text_of(
                m.get("displayContent").filter(|d| !d.is_null()).or_else(|| m.get("content")).unwrap_or(&Value::Null),
            );
            cx.emit(id, t, Body::UserMessage { text, synthetic: false });
        }
        "gemini" => gemini_msg(m, &id, t, cx),
        ty @ ("info" | "error" | "warning") => {
            cx.emit(
                id,
                t,
                Body::System { subtype: ty.to_owned(), text: text_of(m.get("content").unwrap_or(&Value::Null)) },
            );
        }
        other => cx.unknown(format!("message.type={other}")),
    }
}

fn gemini_msg(m: &Value, id: &str, t: i64, cx: &mut Cx<'_, ()>) {
    let model = string_of(m, "model");
    if let Some(model) = &model {
        cx.meta().model = Some(model.clone());
    }
    for (i, th) in m.get("thoughts").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default().iter().enumerate()
    {
        let subject = str_of(th, "subject").unwrap_or("");
        let desc = str_of(th, "description").unwrap_or("");
        let text = if subject.is_empty() { desc.to_owned() } else { format!("{subject}\n{desc}") };
        let tt = th.get("timestamp").and_then(ts).unwrap_or(t);
        cx.emit(format!("{id}:t{i}"), tt, Body::Reasoning { text });
    }
    let text = text_of(m.get("content").unwrap_or(&Value::Null));
    if !text.is_empty() {
        cx.emit(id.to_owned(), t, Body::AssistantMessage { text, model: model.clone() });
    }
    let calls = m.get("toolCalls").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    for (i, tc) in calls.iter().enumerate() {
        let call_id = str_of(tc, "id").unwrap_or("").to_owned();
        let name = str_of(tc, "name").unwrap_or("").to_owned();
        let status = str_of(tc, "status").unwrap_or("");
        let tt = tc.get("timestamp").and_then(ts).unwrap_or(t);
        let done = matches!(status, "success" | "error" | "cancelled");
        let ev = cx.emit(
            format!("{id}:c{i}"),
            tt,
            Body::ToolCall {
                call_id: call_id.clone(),
                name: name.clone(),
                input: json_arg(tc.get("args").unwrap_or(&Value::Null)),
            },
        );
        ev.partial = !done;
        if done {
            let output = match tc.get("resultDisplay") {
                Some(Value::String(s)) if !s.is_empty() => s.clone(),
                _ => tool_result_text(tc.get("result").unwrap_or(&Value::Null)),
            };
            cx.emit(
                format!("{id}:r{i}"),
                tt,
                Body::ToolResult { call_id, name: Some(name), output, is_error: status != "success" },
            );
        }
    }
    // `input` (prompt) includes `cached`; `tool` is prompt-side too; `output` excludes `thoughts`.
    if let Some(tok) = m.get("tokens").filter(|t| t.is_object()) {
        let n = |k: &str| tok.get(k).and_then(Value::as_u64).unwrap_or(0);
        let cached = n("cached");
        let thoughts = n("thoughts");
        cx.emit(
            format!("{id}:u"),
            t,
            Body::Usage(Usage {
                input: (n("input") + n("tool")).saturating_sub(cached),
                output: n("output") + thoughts,
                cache_read: cached,
                cache_write: 0,
                reasoning: thoughts,
                model: model.clone(),
                cost_usd: None,
            }),
        );
    }
    // A reply without tool calls is the end of the turn; tool calls mean the loop continues.
    if calls.is_empty() && m.get("content").is_some_and(|c| !text_of(c).is_empty()) {
        cx.emit(format!("{id}:end"), t, Body::TurnEnd { reason: Some("stop".into()) });
    }
}

/// `result` is `[{functionResponse:{response:{output|error}}}]` in most versions.
fn tool_result_text(v: &Value) -> String {
    let mut out = String::new();
    for part in v.as_array().map(Vec::as_slice).unwrap_or_default() {
        let resp = part.pointer("/functionResponse/response").unwrap_or(part);
        let piece = resp.get("output").or_else(|| resp.get("error")).map(text_of).unwrap_or_else(|| text_of(resp));
        if !piece.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&piece);
        }
    }
    if out.is_empty() { text_of(v) } else { out }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testkit::{Fixture, kinds};
    use serde_json::json;
    use uniflo_core::{Adapter, HistoryQuery};
    use uniflo_schema::Status;

    fn adapter(fx: &Fixture) -> JsonlAdapter<Gemini> {
        fx.write("projects.json", &json!({"projects":{"/Users/me/proj":"proj"}}).to_string());
        JsonlAdapter::new(Gemini::new(fx.root().join("tmp"), fx.root().join("projects.json")))
    }

    fn l(v: Value) -> String {
        format!("{v}\n")
    }

    #[test]
    fn upserts_tool_completion_and_turn_end() {
        let fx = Fixture::new();
        let a = adapter(&fx);
        let mut s = l(
            json!({"sessionId":"sid","projectHash":"h","startTime":"2026-06-08T00:48:01Z","lastUpdated":"x","kind":"main"}),
        );
        s += &l(json!({"id":"u1","timestamp":"2026-06-08T00:48:02Z","type":"user","content":[{"text":"list files"}]}));
        s += &l(
            json!({"id":"g1","timestamp":"2026-06-08T00:48:03Z","type":"gemini","content":"","model":"gemini-x","thoughts":[{"subject":"Plan","description":"ls","timestamp":"2026-06-08T00:48:03Z"}],"toolCalls":[{"id":"tc1","name":"run_shell","args":{"command":"ls"},"status":"executing"}]}),
        );
        let p = fx.write("tmp/proj/chats/session-2026-06-08T00-48-0106487a.jsonl", &s);
        let r = fx.index(&a, &p);
        assert_eq!(kinds(&r.events), vec!["user_message", "reasoning", "tool_call"]);
        assert!(r.events[2].partial, "executing tool call is still streaming");
        assert_eq!(r.status(), Status::Work);
        assert_eq!(r.meta.cwd.as_deref(), Some("/Users/me/proj"));

        // Same message id re-written once the tool finished, then the final reply.
        fx.append(&p, &l(json!({"id":"g1","timestamp":"2026-06-08T00:48:03Z","type":"gemini","content":"","model":"gemini-x","thoughts":[],"tokens":{"input":100,"output":5,"cached":60,"thoughts":9},"toolCalls":[{"id":"tc1","name":"run_shell","args":{"command":"ls"},"status":"success","result":[{"functionResponse":{"response":{"output":"a.txt"}}}]}]})));
        fx.append(&p, &l(json!({"id":"g2","timestamp":"2026-06-08T00:48:05Z","type":"gemini","content":"There is a.txt","model":"gemini-x"})));
        let h = a.history(&p, "session-2026-06-08T00-48-0106487a", &HistoryQuery { before: None, limit: 50 }).unwrap();
        assert_eq!(
            kinds(&h),
            vec!["user_message", "reasoning", "tool_call", "tool_result", "usage", "assistant_message", "turn_end"]
        );
        assert!(!h[2].partial, "upsert replaced the partial tool call");
        assert!(matches!(&h[3].body, Body::ToolResult { output, is_error: false, .. } if output == "a.txt"));
        // Usage contract: cached out of input, thoughts inside output.
        assert!(matches!(&h[4].body, Body::Usage(u)
            if (u.input, u.cache_read, u.output, u.reasoning) == (40, 60, 14, 9) && u.model.as_deref() == Some("gemini-x")));
    }

    #[test]
    fn set_snapshots_and_legacy_json() {
        let fx = Fixture::new();
        let a = adapter(&fx);
        let s = l(
            json!({"$set":{"messages":[{"id":"u","timestamp":"2026-06-08T00:00:00Z","type":"user","content":[{"text":"q"}]},{"id":"e","timestamp":"2026-06-08T00:00:01Z","type":"error","content":"quota"}],"lastUpdated":"x"}}),
        );
        let p = fx.write("tmp/other/chats/session-a.jsonl", &s);
        let r = fx.index(&a, &p);
        assert_eq!(kinds(&r.events), vec!["user_message", "system"]);
        assert_eq!(r.meta.cwd, None, "unknown project stays unknown");

        let doc = json!({"sessionId":"s","projectHash":"h","startTime":"2026-04-05T05:13:53Z","lastUpdated":"x","messages":[{"id":"1","timestamp":"2026-04-05T05:13:54Z","type":"user","content":"hi"},{"id":"2","timestamp":"2026-04-05T05:13:55Z","type":"gemini","content":"hello"}]});
        let p = fx.write("tmp/proj/chats/session-2026-04-05T05-13-53355d54.json", &doc.to_string());
        let r = fx.index(&a, &p);
        assert_eq!(kinds(&r.events), vec!["user_message", "assistant_message", "turn_end"]);
        assert_eq!(r.events[0].pos, Some(1));
        assert_eq!(r.meta.cwd.as_deref(), Some("/Users/me/proj"));
        assert!(!a.decoder.is_source(&fx.root().join("tmp/proj/logs.json")));
    }
}
