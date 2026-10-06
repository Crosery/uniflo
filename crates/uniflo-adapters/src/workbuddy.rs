//! WorkBuddy (Tencent CodeBuddy-derived) transcripts.
//!
//! Layout: `~/.workbuddy/projects/<cwd-slug>/<session>.jsonl`, sub-agents under
//! `<cwd-slug>/<session>/subagents/agent-<id>.jsonl`. Every record carries `id`, `timestamp`
//! (ms) and `cwd`; messages, tool calls, tool results and reasoning are separate records.
//! There is no explicit turn end: a completed assistant `message` ends the turn, and any
//! following tool call flips the status back to work.

use crate::common::under_any;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uniflo_core::util::{home, json_arg, pid_alive, str_of, string_of, text_of, ts};
use uniflo_core::{Adapter, Cx, HarnessInfo, JsonlAdapter, LineDecoder, LiveSession, SourceId};
use uniflo_schema::{Body, Usage};

pub struct WorkBuddy {
    roots: Vec<PathBuf>,
    /// `~/.workbuddy/sessions/<pid>.json` registry of running processes.
    live_dir: PathBuf,
}

pub fn adapters() -> Vec<Arc<dyn Adapter>> {
    let base = home().join(".workbuddy");
    vec![Arc::new(JsonlAdapter::new(WorkBuddy { roots: vec![base.join("projects")], live_dir: base.join("sessions") }))]
}

/// Record types that carry nothing for the unified stream.
const IGNORED: &[&str] = &["file-history-snapshot", "session-meta", "resend-fork-notice"];

impl LineDecoder for WorkBuddy {
    type State = ();

    fn info(&self) -> HarnessInfo {
        HarnessInfo { id: "workbuddy", name: "WorkBuddy" }
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }

    fn is_source(&self, p: &Path) -> bool {
        p.extension().is_some_and(|e| e == "jsonl") && under_any(p, &self.roots)
    }

    fn identify(&self, p: &Path) -> Option<SourceId> {
        let id = p.file_stem()?.to_str()?.to_owned();
        let comps: Vec<&str> = p.iter().filter_map(|c| c.to_str()).collect();
        let parent =
            comps.iter().rposition(|c| *c == "subagents").and_then(|i| i.checked_sub(1)).map(|i| comps[i].to_owned());
        Some(SourceId { id, parent })
    }

    fn decode(&self, v: &Value, cx: &mut Cx<'_, ()>) {
        let ty = str_of(v, "type").unwrap_or("");
        let t = v.get("timestamp").and_then(ts).unwrap_or(0);
        if let Some(cwd) = string_of(v, "cwd") {
            cx.meta().cwd = Some(cwd);
        }
        let id = str_of(v, "id").map(str::to_owned).unwrap_or_else(|| format!("o{}", cx.pos));
        match ty {
            "message" => match str_of(v, "role").unwrap_or("") {
                "user" => user(v, id, t, cx),
                "assistant" => assistant(v, id, t, cx),
                other => cx.unknown(format!("message.role={other}")),
            },
            "function_call" => {
                let body = Body::ToolCall {
                    call_id: str_of(v, "callId").unwrap_or("").to_owned(),
                    name: str_of(v, "name").unwrap_or("").to_owned(),
                    input: json_arg(v.get("arguments").unwrap_or(&Value::Null)),
                };
                cx.emit(id, t, body);
            }
            "function_call_result" => {
                let mut output = match v.get("output") {
                    Some(o @ Value::Object(_)) => o.get("text").map(text_of).unwrap_or_default(),
                    Some(o) => text_of(o),
                    None => String::new(),
                };
                let err = v.pointer("/providerData/toolResult/error").and_then(Value::as_str).filter(|e| !e.is_empty());
                if output.is_empty()
                    && let Some(e) = err
                {
                    output = e.to_owned();
                }
                let body = Body::ToolResult {
                    call_id: str_of(v, "callId").unwrap_or("").to_owned(),
                    name: string_of(v, "name"),
                    output,
                    is_error: err.is_some(),
                };
                cx.emit(id, t, body);
            }
            "reasoning" => {
                let text = text_of(v.get("rawContent").unwrap_or(&Value::Null));
                cx.emit(id, t, Body::Reasoning { text });
            }
            "ai-title" => {
                if let Some(title) = string_of(v, "aiTitle") {
                    cx.meta().title = Some((2, title));
                }
            }
            _ if IGNORED.contains(&ty) => {}
            _ => cx.unknown(format!("type={ty}")),
        }
    }

    fn live(&self) -> Option<Vec<LiveSession>> {
        let mut out = Vec::new();
        for ent in std::fs::read_dir(&self.live_dir).ok()?.flatten() {
            let p = ent.path();
            if p.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let Ok(bytes) = std::fs::read(&p) else { continue };
            let Ok(v) = serde_json::from_slice::<Value>(&bytes) else { continue };
            let (Some(pid), Some(id)) = (v.get("pid").and_then(Value::as_u64), string_of(&v, "sessionId")) else {
                continue;
            };
            if pid_alive(pid as u32) {
                out.push(LiveSession { id, pid: pid as u32, status: None });
            }
        }
        Some(out)
    }

    fn live_roots(&self) -> Vec<PathBuf> {
        vec![self.live_dir.clone()]
    }
}

/// Harness-injected user-role text that carries no human query.
const INJECTED_PREFIXES: &[&str] =
    &["<system-reminder", "<task-notification>", "<image_local_path>", "Caveat:", "Please continue with the"];

/// WorkBuddy sends the human's prompt inside one `input_text` part, wrapped in a
/// several-KiB `<system-reminder data-role="user-context">` blob:
/// `…</system-reminder>\n<user_query>你好</user_query>`. Splitting keeps the typed text
/// as the real message and reports the surrounding context as synthetic.
fn split_user_query(s: &str) -> Option<(String, String)> {
    let open = s.find("<user_query>")?;
    let body = &s[open + "<user_query>".len()..];
    let close = body.rfind("</user_query>")?;
    let mut ctx = s[..open].to_owned();
    ctx.push_str(&body[close + "</user_query>".len()..]);
    Some((body[..close].trim().to_owned(), ctx))
}

fn user(v: &Value, id: String, t: i64, cx: &mut Cx<'_, ()>) {
    let parts: Vec<Value> = match v.get("content") {
        Some(Value::String(s)) => vec![Value::String(s.clone())],
        Some(Value::Array(a)) => a.clone(),
        _ => return,
    };
    let mut typed = Vec::new();
    let mut ctx = Vec::new();
    for p in &parts {
        // String content (older records) has no part type: it is the text itself.
        let raw = match str_of(p, "type").unwrap_or("") {
            "" => p.as_str().unwrap_or("").to_owned(),
            "input_text" | "text" => str_of(p, "text").unwrap_or("").to_owned(),
            "image_blob_ref" => {
                typed.push(format!("[图片 {}]", str_of(p, "mime").unwrap_or("image")));
                continue;
            }
            other => {
                ctx.push(format!("[{other}]"));
                continue;
            }
        };
        match split_user_query(&raw) {
            Some((q, c)) => {
                typed.push(q);
                ctx.push(c);
            }
            None if INJECTED_PREFIXES.iter().any(|m| raw.trim_start().starts_with(m)) => ctx.push(raw),
            None => typed.push(raw),
        }
    }
    let keep = |v: Vec<String>| v.into_iter().filter(|s| !s.trim().is_empty()).collect::<Vec<_>>().join("\n");
    let meta = v.pointer("/providerData/isMeta").and_then(Value::as_bool) == Some(true);
    let ctx = keep(ctx);
    if !ctx.is_empty() && !meta {
        cx.emit(format!("{id}:ctx"), t, Body::UserMessage { text: ctx, synthetic: true });
    }
    let text = keep(typed);
    if !text.is_empty() {
        cx.emit(id, t, Body::UserMessage { text, synthetic: meta });
    }
}

fn assistant(v: &Value, id: String, t: i64, cx: &mut Cx<'_, ()>) {
    let model = v.pointer("/providerData/model").and_then(Value::as_str).filter(|m| !m.is_empty()).map(str::to_owned);
    if let Some(m) = &model {
        cx.meta().model = Some(m.clone());
    }
    let completed = str_of(v, "status") == Some("completed");
    let text = text_of(v.get("content").unwrap_or(&Value::Null));
    if !text.is_empty() {
        cx.emit(id.clone(), t, Body::AssistantMessage { text, model }).partial = !completed;
    }
    if let Some(u) = v.pointer("/providerData/usage").filter(|u| u.is_object()) {
        let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
        let usage = Usage { input: n("inputTokens"), output: n("outputTokens"), ..Default::default() };
        if usage.input + usage.output > 0 {
            cx.emit(format!("{id}:usage"), t, Body::Usage(usage));
        }
    }
    if completed {
        cx.emit(format!("{id}:end"), t, Body::TurnEnd { reason: Some("completed".into()) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testkit::{Fixture, kinds};
    use serde_json::json;
    use uniflo_schema::Status;

    fn wb(fx: &Fixture) -> JsonlAdapter<WorkBuddy> {
        JsonlAdapter::new(WorkBuddy { roots: vec![fx.root().join("projects")], live_dir: fx.root().join("sessions") })
    }

    fn l(v: Value) -> String {
        format!("{v}\n")
    }

    fn rec(id: &str, extra: Value) -> String {
        let mut o = json!({"id":id,"timestamp":1790942400000i64,"cwd":"/w","sessionId":"s1"});
        o.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        l(o)
    }

    #[test]
    fn full_turn_maps_every_record_kind() {
        let fx = Fixture::new();
        let a = wb(&fx);
        let mut s = rec(
            "u1",
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":"do it"},{"type":"image_blob_ref"}]}),
        );
        s += &rec("i1", json!({"type":"file-history-snapshot","snapshot":{}}));
        s += &rec("r1", json!({"type":"reasoning","rawContent":[{"type":"reasoning_text","text":"think"}]}));
        s += &rec("f1", json!({"type":"function_call","callId":"c1","name":"Bash","arguments":"{\"command\":\"ls\"}"}));
        s += &rec(
            "f1r",
            json!({"type":"function_call_result","callId":"c1","name":"Bash","status":"completed","output":{"type":"text","text":"a.txt"}}),
        );
        s += &rec(
            "f2r",
            json!({"type":"function_call_result","callId":"c2","name":"Read","status":"completed","output":{"type":"text","text":""},"providerData":{"toolResult":{"error":"boom"}}}),
        );
        s += &rec(
            "a1",
            json!({"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"done"}],"providerData":{"model":"m-1","usage":{"inputTokens":7,"outputTokens":3}}}),
        );
        s += &rec("t1", json!({"type":"ai-title","aiTitle":"A title"}));
        s += &rec("m1", json!({"type":"session-meta","meta":{}}));
        s += &rec("n1", json!({"type":"resend-fork-notice"}));
        let p = fx.write("projects/-w/s1.jsonl", &s);
        let r = fx.index(&a, &p);
        assert_eq!(
            kinds(&r.events),
            vec![
                "user_message",
                "reasoning",
                "tool_call",
                "tool_result",
                "tool_result",
                "assistant_message",
                "usage",
                "turn_end"
            ]
        );
        assert_eq!(r.status(), Status::Idle);
        assert!(r.unknown.is_empty(), "{:?}", r.unknown);
        assert!(
            matches!(&r.events[0].body, Body::UserMessage { text, synthetic: false } if text == "do it\n[图片 image]")
        );
        assert!(
            matches!(&r.events[2].body, Body::ToolCall { call_id, name, input } if call_id == "c1" && name == "Bash" && input["command"] == "ls")
        );
        assert!(
            matches!(&r.events[3].body, Body::ToolResult { output, is_error: false, name: Some(n), .. } if output == "a.txt" && n == "Bash")
        );
        assert!(matches!(&r.events[4].body, Body::ToolResult { output, is_error: true, .. } if output == "boom"));
        assert!(matches!(&r.events[6].body, Body::Usage(u) if u.input == 7 && u.output == 3));
        assert_eq!(r.events[0].ts, 1790942400000);
        assert_eq!(r.meta.title.as_deref(), Some("A title"));
        assert_eq!(r.meta.cwd.as_deref(), Some("/w"));
        assert_eq!(r.meta.model.as_deref(), Some("m-1"));
    }

    #[test]
    fn mid_turn_is_work_and_incomplete_is_partial() {
        let fx = Fixture::new();
        let a = wb(&fx);
        let mut s = rec("u1", json!({"type":"message","role":"user","content":[{"type":"input_text","text":"go"}]}));
        s += &rec(
            "a1",
            json!({"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"ok"}]}),
        );
        s += &rec("f1", json!({"type":"function_call","callId":"c1","name":"Bash","arguments":"{}"}));
        let p = fx.write("projects/-w/s1.jsonl", &s);
        assert_eq!(fx.index(&a, &p).status(), Status::Work);
        let s2 = rec(
            "a2",
            json!({"type":"message","role":"assistant","status":"incomplete","content":[{"type":"output_text","text":"par"}]}),
        );
        let p2 = fx.write("projects/-w/s2.jsonl", &s2);
        let r = fx.index(&a, &p2);
        assert!(r.events[0].partial);
        assert_eq!(r.status(), Status::Work);
    }

    #[test]
    fn meta_user_is_synthetic_and_unknown_is_reported() {
        let fx = Fixture::new();
        let a = wb(&fx);
        let mut s = rec(
            "u1",
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":"ctx"}],"providerData":{"isMeta":true}}),
        );
        s += &rec("z", json!({"type":"brand-new"}));
        let p = fx.write("projects/-w/s3.jsonl", &s);
        let r = fx.index(&a, &p);
        assert!(matches!(&r.events[0].body, Body::UserMessage { synthetic: true, .. }));
        assert_eq!(r.unknown, vec!["type=brand-new".to_string()]);
    }

    #[test]
    fn user_query_is_split_from_injected_context() {
        let fx = Fixture::new();
        let a = wb(&fx);
        // Real shape (macOS and Windows): one input_text part carrying the whole blob.
        let s = rec(
            "u1",
            json!({"type":"message","role":"user","content":[{"type":"input_text",
                "text":"<system-reminder data-role=\"user-context\">\n<user_info>\nOS Version: win32\n</user_info>\n</system-reminder>\n<user_query>你好</user_query>"}]}),
        );
        let p = fx.write("projects/-w/s5.jsonl", &s);
        let r = fx.index(&a, &p);
        assert_eq!(kinds(&r.events), ["user_message", "user_message"]);
        // Injected context first (collapsible), then the human's actual text.
        assert!(
            matches!(&r.events[0].body, Body::UserMessage { text, synthetic: true } if text.contains("OS Version") && !text.contains("你好"))
        );
        assert!(matches!(&r.events[1].body, Body::UserMessage { text, synthetic: false } if text == "你好"));
        // Preview must come from the typed text, not the blob.
        assert_eq!(r.preview.as_deref(), Some("你好"));
        // Pure injected records (no query at all) stay synthetic with no typed twin.
        let s2 = rec(
            "u2",
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":"<task-notification>\n<task-id>x</task-id>\n</task-notification>"}]}),
        );
        let p2 = fx.write("projects/-w/s6.jsonl", &s2);
        let r2 = fx.index(&a, &p2);
        assert_eq!(kinds(&r2.events), ["user_message"]);
        assert!(matches!(&r2.events[0].body, Body::UserMessage { synthetic: true, .. }));
    }

    #[test]
    fn is_source_and_identity() {
        let fx = Fixture::new();
        let a = wb(&fx);
        let main = fx.write("projects/-w/root1.jsonl", "");
        let sub = fx.write("projects/-w/root1/subagents/agent-ab12.jsonl", "");
        let other = fx.write("other/-w/x.jsonl", "");
        let txt = fx.write("projects/-w/x.txt", "");
        for (p, want) in [(&main, true), (&sub, true), (&other, false), (&txt, false)] {
            assert_eq!(uniflo_core::Adapter::source_for(&a, p).is_some(), want, "{p:?}");
        }
        let found = uniflo_core::Adapter::discover(&a);
        assert_eq!(found.len(), 2);
        let d = &a.decoder;
        assert_eq!(d.identify(&main), Some(SourceId { id: "root1".into(), parent: None }));
        assert_eq!(d.identify(&sub), Some(SourceId { id: "agent-ab12".into(), parent: Some("root1".into()) }));
    }

    #[test]
    fn live_registry_only_reports_alive_pids() {
        let fx = Fixture::new();
        let a = wb(&fx);
        fx.write("sessions/1.json", &json!({"pid": std::process::id(), "sessionId": "me"}).to_string());
        fx.write("sessions/2.json", &json!({"pid": 9_999_999, "sessionId": "dead"}).to_string());
        let live = uniflo_core::Adapter::live(&a).unwrap();
        assert_eq!(live, vec![LiveSession { id: "me".into(), pid: std::process::id(), status: None }]);
    }
}
