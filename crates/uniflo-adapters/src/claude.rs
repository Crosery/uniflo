//! Claude Code transcript family: Claude Code, Qoder and Qwen Work share one JSONL schema.
//!
//! Layout: `<root>/<cwd-slug>/<session>.jsonl`, sub-agents under
//! `<root>/<cwd-slug>/<session>/subagents/[workflows/<wf>/]agent-<id>.jsonl`.
//! Each assistant content block is its own line; lines of one API message share `message.id`.

use crate::common::{ends_turn_reason, under_any};
use serde_json::Value;
use std::path::{Path, PathBuf};
use uniflo_core::util::{home, json_arg, pid_alive, str_of, string_of, text_of, ts};
use uniflo_core::{Cx, HarnessInfo, JsonlAdapter, LineDecoder, LiveSession, SourceId};
use uniflo_schema::{Body, Usage};

pub struct ClaudeFamily {
    pub info: HarnessInfo,
    pub roots: Vec<PathBuf>,
    /// `~/.claude/sessions/<pid>.json` registry of running processes.
    pub live_dir: Option<PathBuf>,
}

/// Claude Code, Qoder and Qwen Work.
pub fn adapters() -> Vec<std::sync::Arc<dyn uniflo_core::Adapter>> {
    vec![std::sync::Arc::new(claude()), std::sync::Arc::new(qoder()), std::sync::Arc::new(qwen())]
}

pub fn claude() -> JsonlAdapter<ClaudeFamily> {
    let h = home();
    JsonlAdapter::new(ClaudeFamily {
        info: HarnessInfo { id: "claude", name: "Claude Code" },
        roots: vec![h.join(".claude/projects")],
        live_dir: Some(h.join(".claude/sessions")),
    })
}

pub fn qoder() -> JsonlAdapter<ClaudeFamily> {
    let h = home();
    JsonlAdapter::new(ClaudeFamily {
        info: HarnessInfo { id: "qoder", name: "Qoder" },
        roots: vec![h.join(".qoder/projects"), h.join(".qoder-cn/projects")],
        live_dir: None,
    })
}

pub fn qwen() -> JsonlAdapter<ClaudeFamily> {
    JsonlAdapter::new(ClaudeFamily {
        info: HarnessInfo { id: "qwen", name: "Qwen Work" },
        roots: vec![home().join(".qwenworkcn/projects")],
        live_dir: None,
    })
}

/// Record types that carry nothing for the unified stream.
const IGNORED: &[&str] = &[
    "attachment",
    "file-history-snapshot",
    "file-history-delta",
    "last-prompt",
    "mode",
    "permission-mode",
    "atis-latch",
    "queue-operation",
    "pr-link",
    "cost-state",
    "active-leaf",
    "workspace-directories",
    "worktree-state",
    "progress",
    "tag",
    "session-meta",
    "resend-fork-notice",
    "continued-in",
    "fork-context-ref",
    "agent-setting",
];

/// Leading markers of user-role text that the harness injected rather than a human typed.
const SYNTHETIC_PREFIXES: &[&str] = &[
    "<task-notification>",
    "<system-reminder>",
    "<cross-session-message",
    "<user-prompt-submit-hook>",
    "<local-command-caveat>",
    "Caveat:",
    "This session is being continued",
    "Stop hook feedback",
    "Another Claude session sent a message",
    "[Your previous response",
];

/// User-role text that is really slash-command plumbing.
const COMMAND_PREFIXES: &[&str] =
    &["<command-name>", "<command-message>", "<local-command-stdout>", "<local-command-stderr>"];

fn is_subagent_file(p: &Path) -> bool {
    p.components().any(|c| c.as_os_str() == "subagents")
}

impl LineDecoder for ClaudeFamily {
    type State = ();

    fn info(&self) -> HarnessInfo {
        self.info
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }

    /// `<slug>/<id>.jsonl` or `…/subagents/…/agent-*.jsonl` (workflow `journal.jsonl` files are not sessions).
    fn is_source(&self, p: &Path) -> bool {
        if p.extension().is_none_or(|e| e != "jsonl") || !under_any(p, &self.roots) {
            return false;
        }
        let Some(rel) = self.roots.iter().find_map(|r| p.strip_prefix(r).ok()) else { return false };
        match rel.iter().count() {
            2 => true,
            n if n > 2 => {
                is_subagent_file(p) && p.file_name().and_then(|f| f.to_str()).is_some_and(|f| f.starts_with("agent-"))
            }
            _ => false,
        }
    }

    fn identify(&self, p: &Path) -> Option<SourceId> {
        let id = p.file_stem()?.to_str()?.to_owned();
        let comps: Vec<&str> = p.iter().filter_map(|c| c.to_str()).collect();
        let parent =
            comps.iter().position(|c| *c == "subagents").and_then(|i| i.checked_sub(1)).map(|i| comps[i].to_owned());
        Some(SourceId { id, parent })
    }

    fn decode(&self, v: &Value, cx: &mut Cx<'_, ()>) {
        let ty = str_of(v, "type").unwrap_or("");
        let t = v.get("timestamp").and_then(ts).unwrap_or(0);
        if let Some(cwd) = string_of(v, "cwd") {
            cx.meta().cwd = Some(cwd);
        }
        // Old versions inlined sub-agent traffic into the parent file.
        if v.get("isSidechain").and_then(Value::as_bool) == Some(true) && !is_subagent_file(cx.src) {
            return;
        }
        match ty {
            "user" => user(v, t, cx),
            "assistant" => assistant(v, t, cx),
            "system" => system(v, t, cx),
            "summary" => title(cx, 1, str_of(v, "summary")),
            "ai-title" => title(cx, 2, str_of(v, "aiTitle")),
            "agent-name" => title(cx, 2, str_of(v, "agentName").or_else(|| str_of(v, "name"))),
            "custom-title" => title(cx, 3, str_of(v, "customTitle")),
            "runtime-config" => {
                if let Some(m) = string_of(v, "model") {
                    cx.meta().model = Some(m);
                }
            }
            _ if IGNORED.contains(&ty) => {}
            _ => cx.unknown(format!("type={ty}")),
        }
    }

    fn live(&self) -> Option<Vec<LiveSession>> {
        let dir = self.live_dir.as_ref()?;
        let mut out = Vec::new();
        for ent in std::fs::read_dir(dir).ok()?.flatten() {
            let p = ent.path();
            if p.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let Ok(bytes) = std::fs::read(&p) else { continue };
            let Ok(v) = serde_json::from_slice::<Value>(&bytes) else { continue };
            let (Some(pid), Some(id)) = (v.get("pid").and_then(Value::as_u64), string_of(&v, "sessionId")) else {
                continue;
            };
            // `status` in this registry goes stale; only liveness is trusted.
            if pid_alive(pid as u32) {
                out.push(LiveSession { id, pid: pid as u32, status: None });
            }
        }
        Some(out)
    }

    fn live_roots(&self) -> Vec<PathBuf> {
        self.live_dir.iter().cloned().collect()
    }
}

fn title(cx: &mut Cx<'_, ()>, rank: u8, t: Option<&str>) {
    if let Some(t) = t.filter(|t| !t.trim().is_empty()) {
        cx.meta().title = Some((rank, t.to_owned()));
    }
}

fn uuid(v: &Value) -> String {
    str_of(v, "uuid").unwrap_or("").to_owned()
}

/// Classify one piece of user-role text and emit it.
fn user_text(cx: &mut Cx<'_, ()>, id: String, t: i64, text: &str, synthetic: bool) {
    let trimmed = text.trim_start();
    if trimmed.starts_with("[Request interrupted by user") {
        cx.emit(id, t, Body::TurnEnd { reason: Some("interrupted".into()) });
    } else if COMMAND_PREFIXES.iter().any(|p| trimmed.starts_with(p)) {
        cx.emit(id, t, Body::System { subtype: "command".into(), text: text.to_owned() });
    } else {
        let synthetic = synthetic || SYNTHETIC_PREFIXES.iter().any(|p| trimmed.starts_with(p));
        cx.emit(id, t, Body::UserMessage { text: text.to_owned(), synthetic });
    }
}

fn user(v: &Value, t: i64, cx: &mut Cx<'_, ()>) {
    let id = uuid(v);
    let origin = v.pointer("/origin/kind").and_then(Value::as_str);
    let synthetic = v.get("isMeta").and_then(Value::as_bool) == Some(true)
        || v.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
        || origin.is_some_and(|k| k != "human");
    let content = v.pointer("/message/content").unwrap_or(&Value::Null);
    match content {
        Value::String(s) => user_text(cx, id, t, s, synthetic),
        Value::Array(blocks) => {
            let mut text = String::new();
            for (i, b) in blocks.iter().enumerate() {
                match str_of(b, "type").unwrap_or("") {
                    "tool_result" => {
                        let call_id = str_of(b, "tool_use_id").unwrap_or("").to_owned();
                        let output = text_of(b.get("content").unwrap_or(&Value::Null));
                        let is_error = b.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                        cx.emit(format!("{id}#{i}"), t, Body::ToolResult { call_id, name: None, output, is_error });
                    }
                    "text" => {
                        let s = str_of(b, "text").unwrap_or("");
                        if s.trim_start().starts_with("[Request interrupted by user") {
                            user_text(cx, format!("{id}#{i}"), t, s, synthetic);
                        } else {
                            push_line(&mut text, s);
                        }
                    }
                    other => push_line(&mut text, &format!("[{other}]")),
                }
            }
            if !text.is_empty() {
                user_text(cx, format!("{id}#t"), t, &text, synthetic);
            }
        }
        _ => {}
    }
}

fn push_line(buf: &mut String, s: &str) {
    if s.is_empty() {
        return;
    }
    if !buf.is_empty() {
        buf.push('\n');
    }
    buf.push_str(s);
}

fn assistant(v: &Value, t: i64, cx: &mut Cx<'_, ()>) {
    let id = uuid(v);
    let Some(msg) = v.get("message") else { return };
    let model = string_of(msg, "model").filter(|m| m != "<synthetic>");
    if let Some(m) = &model {
        cx.meta().model = Some(m.clone());
    }
    let api_error = v.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true);
    if let Some(blocks) = msg.get("content").and_then(Value::as_array) {
        for (i, b) in blocks.iter().enumerate() {
            let eid = format!("{id}#{i}");
            let body = match str_of(b, "type").unwrap_or("") {
                "text" if api_error => {
                    Body::System { subtype: "api_error".into(), text: str_of(b, "text").unwrap_or("").to_owned() }
                }
                "text" => {
                    Body::AssistantMessage { text: str_of(b, "text").unwrap_or("").to_owned(), model: model.clone() }
                }
                "thinking" => Body::Reasoning { text: str_of(b, "thinking").unwrap_or("").to_owned() },
                "redacted_thinking" => Body::Reasoning { text: "[redacted]".into() },
                "tool_use" | "server_tool_use" | "mcp_tool_use" => Body::ToolCall {
                    call_id: str_of(b, "id").unwrap_or("").to_owned(),
                    name: str_of(b, "name").unwrap_or("").to_owned(),
                    input: json_arg(b.get("input").unwrap_or(&Value::Null)),
                },
                ty if ty.ends_with("_tool_result") => Body::ToolResult {
                    call_id: str_of(b, "tool_use_id").unwrap_or("").to_owned(),
                    name: None,
                    output: text_of(b.get("content").unwrap_or(&Value::Null)),
                    is_error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                },
                other => {
                    cx.unknown(format!("assistant.block={other}"));
                    continue;
                }
            };
            cx.emit(eid, t, body);
        }
    }
    let mid = str_of(msg, "id").unwrap_or(&id).to_owned();
    // `<synthetic>` messages (API errors, local notices) were never a model call.
    let synthetic = str_of(msg, "model") == Some("<synthetic>");
    if let Some(u) = msg.get("usage").filter(|u| u.is_object() && !synthetic) {
        // Anthropic shape already matches the contract: input / cache read / cache creation
        // are disjoint, thinking is billed inside output_tokens. Lines of one message repeat
        // the usage; the shared id makes them one step.
        let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
        let usage = Usage {
            input: n("input_tokens"),
            output: n("output_tokens"),
            cache_read: n("cache_read_input_tokens"),
            cache_write: n("cache_creation_input_tokens"),
            reasoning: u.pointer("/output_tokens_details/thinking_tokens").and_then(Value::as_u64).unwrap_or(0),
            model: model.clone(),
            cost_usd: None,
        };
        cx.emit(format!("{mid}:usage"), t, Body::Usage(usage));
    }
    if let Some(reason) = str_of(msg, "stop_reason").filter(|r| ends_turn_reason(r)) {
        cx.emit(format!("{mid}:end"), t, Body::TurnEnd { reason: Some(reason.to_owned()) });
    }
}

fn system(v: &Value, t: i64, cx: &mut Cx<'_, ()>) {
    let sub = str_of(v, "subtype").unwrap_or("system");
    let id = uuid(v);
    if sub == "turn_duration" {
        cx.emit(id, t, Body::TurnEnd { reason: Some("turn_duration".into()) });
        return;
    }
    let text = v.get("content").map(text_of).unwrap_or_default();
    cx.emit(id, t, Body::System { subtype: sub.to_owned(), text });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testkit::{Fixture, kinds};
    use serde_json::json;
    use uniflo_schema::Status;

    fn fam(root: &Path) -> JsonlAdapter<ClaudeFamily> {
        JsonlAdapter::new(ClaudeFamily {
            info: HarnessInfo { id: "claude", name: "Claude Code" },
            roots: vec![root.to_path_buf()],
            live_dir: Some(root.join("live")),
        })
    }

    fn line(v: Value) -> String {
        format!("{v}\n")
    }

    #[test]
    fn full_turn_maps_every_record_kind() {
        let fx = Fixture::new();
        let a = fam(fx.root());
        let base = json!({"cwd":"/w","sessionId":"s1","version":"2","gitBranch":"main"});
        let with = |extra: Value| {
            let mut o = base.clone();
            o.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            line(o)
        };
        let mut s = String::new();
        s += &with(
            json!({"type":"user","uuid":"u1","timestamp":"2026-10-02T12:00:00Z","origin":{"kind":"human"},"message":{"role":"user","content":"fix the bug"}}),
        );
        s += &with(
            json!({"type":"assistant","uuid":"a1","timestamp":"2026-10-02T12:00:01Z","message":{"id":"m1","model":"claude-x","content":[{"type":"thinking","thinking":"hmm"}],"stop_reason":"tool_use","usage":{"input_tokens":5,"output_tokens":2,"cache_read_input_tokens":1,"cache_creation_input_tokens":3}}}),
        );
        s += &with(
            json!({"type":"assistant","uuid":"a2","timestamp":"2026-10-02T12:00:02Z","message":{"id":"m1","model":"claude-x","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}}],"stop_reason":"tool_use"}}),
        );
        s += &with(
            json!({"type":"user","uuid":"u2","timestamp":"2026-10-02T12:00:03Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"a.txt"}],"is_error":false}]}}),
        );
        s += &with(
            json!({"type":"assistant","uuid":"a3","timestamp":"2026-10-02T12:00:04Z","message":{"id":"m2","model":"claude-x","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"}}),
        );
        s += &with(
            json!({"type":"system","subtype":"turn_duration","uuid":"y1","timestamp":"2026-10-02T12:00:05Z","durationMs":5000}),
        );
        s += &line(json!({"type":"ai-title","aiTitle":"Fix bug","sessionId":"s1"}));
        s += &line(json!({"type":"custom-title","customTitle":"My title","sessionId":"s1"}));
        s += &line(json!({"type":"ai-title","aiTitle":"Later generated","sessionId":"s1"}));
        s += &line(json!({"type":"permission-mode","permissionMode":"x","sessionId":"s1"}));
        let p = fx.write("-w/s1.jsonl", &s);

        let r = fx.index(&a, &p);
        assert_eq!(
            kinds(&r.events),
            vec![
                "user_message",
                "reasoning",
                "usage",
                "tool_call",
                "tool_result",
                "assistant_message",
                "turn_end",
                "turn_end"
            ]
        );
        assert_eq!(r.status(), Status::Idle);
        assert_eq!(r.meta.title.as_deref(), Some("My title"), "custom title outranks generated ones");
        assert_eq!(r.meta.cwd.as_deref(), Some("/w"));
        assert_eq!(r.meta.model.as_deref(), Some("claude-x"));
        assert!(r.unknown.is_empty(), "{:?}", r.unknown);
        let call = &r.events[3];
        assert_eq!(call.id, "a2#0");
        assert!(
            matches!(&call.body, Body::ToolCall { call_id, name, input } if call_id == "toolu_1" && name == "Bash" && input["command"] == "ls")
        );
        assert!(
            matches!(&r.events[4].body, Body::ToolResult { call_id, output, is_error: false, .. } if call_id == "toolu_1" && output == "a.txt")
        );
        assert!(
            matches!(&r.events[2].body, Body::Usage(u) if u.input == 5 && u.cache_write == 3 && u.model.as_deref() == Some("claude-x"))
        );
        assert_eq!(r.events[0].ts, 1790942400000);
    }

    #[test]
    fn synthetic_and_command_user_text() {
        let fx = Fixture::new();
        let a = fam(fx.root());
        let mut s = String::new();
        s += &line(json!({"type":"user","uuid":"u1","message":{"content":"<command-name>/model</command-name>"}}));
        s += &line(
            json!({"type":"user","uuid":"u2","message":{"content":"<task-notification>done</task-notification>"}}),
        );
        s += &line(json!({"type":"user","uuid":"u3","isMeta":true,"message":{"content":"meta"}}));
        s += &line(json!({"type":"user","uuid":"u4","origin":{"kind":"peer"},"message":{"content":"hello from peer"}}));
        s += &line(
            json!({"type":"user","uuid":"u5","message":{"content":[{"type":"text","text":"look"},{"type":"image","source":{}}]}}),
        );
        s += &line(
            json!({"type":"user","uuid":"u6","message":{"content":[{"type":"text","text":"[Request interrupted by user for tool use]"}]}}),
        );
        let p = fx.write("-w/s2.jsonl", &s);
        let r = fx.index(&a, &p);
        let flags: Vec<_> = r
            .events
            .iter()
            .map(|e| match &e.body {
                Body::UserMessage { synthetic, text } => format!("user:{synthetic}:{text}"),
                b => b.kind().to_owned(),
            })
            .collect();
        assert_eq!(
            flags,
            vec![
                "system",
                "user:true:<task-notification>done</task-notification>",
                "user:true:meta",
                "user:true:hello from peer",
                "user:false:look\n[image]",
                "turn_end"
            ]
        );
        assert_eq!(r.preview.as_deref(), Some("look [image]"));
    }

    #[test]
    fn subagent_files_keep_sidechain_and_link_parent() {
        let fx = Fixture::new();
        let a = fam(fx.root());
        let rec = line(json!({"type":"user","uuid":"u1","isSidechain":true,"message":{"content":"task"}}));
        let main = fx.write("-w/root-sess.jsonl", &rec);
        let sub = fx.write("-w/root-sess/subagents/workflows/wf_1/agent-abc.jsonl", &rec);
        assert!(
            fx.index_all(&a, &main).values().all(|r| r.events.is_empty()),
            "sidechain lines in main files belong to sub-agents"
        );
        let r = fx.index(&a, &sub);
        assert_eq!(r.events.len(), 1);
        assert_eq!(r.id, "agent-abc");
        assert_eq!(r.meta.parent.as_deref(), Some("root-sess"));
        let d = &a.decoder;
        assert!(!d.is_source(&fx.root().join("-w/root-sess/subagents/workflows/wf_1/journal.jsonl")));
        assert!(!d.is_source(&fx.root().join("-w/root-sess/tool-results/x.jsonl")));
        assert!(!d.is_source(&fx.root().join("top.jsonl")));
        assert!(d.is_source(&sub));
    }

    #[test]
    fn api_errors_and_unknown_types() {
        let fx = Fixture::new();
        let a = fam(fx.root());
        let mut s = line(
            json!({"type":"assistant","uuid":"a","isApiErrorMessage":true,"message":{"model":"<synthetic>","content":[{"type":"text","text":"API Error: 529"}],"stop_reason":"stop_sequence"}}),
        );
        s += &line(json!({"type":"brand-new-thing"}));
        let p = fx.write("-w/s3.jsonl", &s);
        let r = fx.index(&a, &p);
        assert!(matches!(&r.events[0].body, Body::System { subtype, .. } if subtype == "api_error"));
        assert_eq!(r.meta.model, None, "synthetic model is not a model");
        assert_eq!(r.unknown, vec!["type=brand-new-thing".to_string()]);
    }

    #[test]
    fn live_registry_only_reports_alive_pids() {
        let fx = Fixture::new();
        let a = fam(fx.root());
        fx.write("live/1.json", &json!({"pid": std::process::id(), "sessionId": "me", "status": "busy"}).to_string());
        fx.write("live/2.json", &json!({"pid": 9_999_999, "sessionId": "dead"}).to_string());
        fx.write("live/3.key", "secret");
        let live = uniflo_core::Adapter::live(&a).unwrap();
        assert_eq!(live, vec![LiveSession { id: "me".into(), pid: std::process::id(), status: None }]);
    }
}
