//! Prime Agent transcripts (`~/.prime/agent/sessions/<uuid>.jsonl`)
//! and subagent transcripts (`~/.prime/agent/session-artifacts/<root-uuid>/**/<sub-uuid>.jsonl`).
//!
//! Layout:
//! - Root sessions: `<root>/sessions/<uuid>.jsonl`
//! - Subagent sessions: `<root>/session-artifacts/<root-uuid>/sub-<dir>/<sub-uuid>.jsonl`
//!   (nested subagents can live under `.../sub-<id1>/sub-<id2>/<nested-uuid>.jsonl`).
//! - Each subagent transcript begins with a `session` record containing `parentSession`.
//! - Subagent task name is given by `session_info` (`name`), and task prompt by
//!   `custom_message` with `customType: "agent_message"` starting with `[task from parent]`.
//! - Live workers register in `~/.prime/agent/daemon-workers/<worker-id>/<id>.json` with PID,
//!   root session ID, and active worker socket.

use crate::common::{ends_turn_reason, under_any};
use serde_json::Value;
use std::collections::HashSet;
use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;
use uniflo_core::procs::ProcCache;
use uniflo_core::util::{home, json_arg, pid_alive, str_of, string_of, text_of, ts};
use uniflo_core::{Cx, HarnessInfo, JsonlAdapter, LineDecoder, LiveSession, SourceId};
use uniflo_schema::{Body, Usage};

pub struct PrimeDecoder {
    pub info: HarnessInfo,
    pub roots: Vec<PathBuf>,
    procs: Option<ProcCache>,
}

pub fn adapters() -> Vec<std::sync::Arc<dyn uniflo_core::Adapter>> {
    vec![std::sync::Arc::new(prime())]
}

const PROC_TTL: Duration = Duration::from_secs(3);

fn is_jsonl(p: &Path) -> bool {
    p.extension().is_some_and(|e| e == "jsonl")
}

fn entry(args: &str, pred: impl Fn(&str) -> bool) -> bool {
    let mut it = args.split_whitespace();
    let Some(first) = it.next() else { return false };
    let base = first.rsplit('/').next().unwrap_or(first);
    if matches!(base, "bun" | "node" | "deno" | "tsx") { it.next().is_some_and(pred) } else { pred(first) }
}

fn is_prime(args: &str) -> bool {
    entry(args, |a| a == "prime-agent" || a.ends_with("/prime-agent"))
}

pub fn prime() -> JsonlAdapter<PrimeDecoder> {
    let h = home();
    let roots = vec![h.join(".prime/agent/sessions"), h.join(".prime/agent/session-artifacts")];
    JsonlAdapter::new(PrimeDecoder {
        info: HarnessInfo { id: "prime", name: "Prime Agent" },
        roots,
        procs: Some(ProcCache::new(PROC_TTL, is_prime).with_files(is_jsonl)),
    })
}

/// Read the parent session stem from the first few lines of a transcript.
fn header_parent(p: &Path) -> Option<String> {
    let f = std::fs::File::open(p).ok()?;
    let r = std::io::BufReader::new(f.take(8192));
    r.lines().take(5).map_while(Result::ok).find_map(|line| {
        let v: Value = serde_json::from_str(line.trim()).ok()?;
        if str_of(&v, "type") == Some("session") {
            let ps = string_of(&v, "parentSession").or_else(|| string_of(&v, "parent_session"))?;
            let stem = Path::new(&ps).file_stem()?.to_str()?;
            return Some(stem.to_owned());
        }
        None
    })
}

/// Recursively find all transcript filenames under `dir` (excluding semantic edges).
fn collect_subagents(dir: &Path, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_subagents(&path, out);
        } else if path.extension().is_some_and(|e| e == "jsonl")
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            && stem != "semantic-edges"
            && !stem.contains('.')
        {
            out.push(stem.to_owned());
        }
    }
}

const IGNORED: &[&str] = &[
    "thinking_level_change",
    "service_tier_change",
    "session_state",
    "git_state",
    "custom",
    "harness_state",
    "branch_summary",
    "label",
];

impl LineDecoder for PrimeDecoder {
    type State = ();

    fn info(&self) -> HarnessInfo {
        self.info
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }

    fn max_depth(&self) -> usize {
        8
    }

    fn is_source(&self, p: &Path) -> bool {
        p.extension().is_some_and(|e| e == "jsonl")
            && !p.file_stem().is_some_and(|s| s == "semantic-edges" || s.to_string_lossy().contains('.'))
            && under_any(p, &self.roots)
    }

    fn identify(&self, p: &Path) -> Option<SourceId> {
        let stem = p.file_stem()?.to_str()?.to_owned();
        let parent = header_parent(p).or_else(|| {
            let comps: Vec<&str> = p.iter().filter_map(|c| c.to_str()).collect();
            comps
                .iter()
                .position(|c| *c == "session-artifacts")
                .and_then(|i| comps.get(i + 1))
                .filter(|root_id| **root_id != stem)
                .map(|s| (*s).to_owned())
        });
        Some(SourceId { id: stem, parent })
    }

    fn live(&self) -> Option<Vec<LiveSession>> {
        let mut out = Vec::new();
        let mut claimed = HashSet::new();

        // 1. Inspect on-disk daemon-workers registry (~/.prime/agent/daemon-workers/*/*.json)
        let workers_root = home().join(".prime/agent/daemon-workers");
        if let Ok(rd) = std::fs::read_dir(&workers_root) {
            for entry in rd.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                if let Ok(inner) = std::fs::read_dir(path) {
                    for file in inner.flatten() {
                        let p = file.path();
                        if p.extension().is_some_and(|e| e == "json")
                            && let Ok(content) = std::fs::read_to_string(&p)
                            && let Ok(v) = serde_json::from_str::<Value>(&content)
                        {
                            let pid = v.get("pid").and_then(Value::as_u64).map(|p| p as u32);
                            let root_id = string_of(&v, "rootSessionId");
                            if let (Some(pid), Some(root_id)) = (pid, root_id)
                                && pid_alive(pid)
                            {
                                if claimed.insert(root_id.clone()) {
                                    out.push(LiveSession { id: root_id.clone(), pid, status: None });
                                }
                                // Claim all subagents of this active root session
                                let artifacts = home().join(".prime/agent/session-artifacts").join(&root_id);
                                let mut subs = Vec::new();
                                collect_subagents(&artifacts, &mut subs);
                                for sub_id in subs {
                                    if claimed.insert(sub_id.clone()) {
                                        out.push(LiveSession { id: sub_id, pid, status: None });
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // 2. Fallback to CLI process probe for direct TUI / interactive runs
        if let Some(pc) = &self.procs {
            for p in pc.get() {
                for f in &p.files {
                    if self.is_source(f)
                        && let Some(sid) = self.identify(f)
                        && claimed.insert(sid.id.clone())
                    {
                        out.push(LiveSession { id: sid.id, pid: p.pid, status: None });
                    }
                }
            }
        }

        Some(out)
    }

    fn decode(&self, v: &Value, cx: &mut Cx<'_, ()>) {
        let t = v.get("timestamp").and_then(ts).unwrap_or(0);
        let lid = str_of(v, "id").unwrap_or("").to_owned();
        match str_of(v, "type").unwrap_or("") {
            "session" => {
                let m = cx.meta();
                m.cwd = string_of(v, "cwd");
                m.started_at = Some(t).filter(|t| *t > 0);
                if let Some(title) = string_of(v, "title") {
                    m.title = Some((2, title));
                }
                if let Some(ps) = string_of(v, "parentSession").or_else(|| string_of(v, "parent_session")) {
                    let parent_stem = Path::new(&ps).file_stem().and_then(|s| s.to_str()).unwrap_or(&ps);
                    m.parent = Some(parent_stem.to_owned());
                }
            }
            "session_info" => {
                if let Some(name) = string_of(v, "name").filter(|s| !s.trim().is_empty()) {
                    cx.meta().title = Some((3, name));
                }
            }
            "title" | "title_change" => {
                if let Some(title) = string_of(v, "title").filter(|s| !s.trim().is_empty()) {
                    let rank = if str_of(v, "source") == Some("auto") { 2 } else { 3 };
                    cx.meta().title = Some((rank, title));
                }
            }
            "model_change" => {
                if let Some(model) = string_of(v, "modelId").or_else(|| string_of(v, "model")) {
                    cx.meta().model = Some(model);
                }
            }
            "session_init" => {
                if let Some(model) = string_of(v, "resolvedModel") {
                    cx.meta().model = Some(model);
                }
            }
            "message" => message(v.get("message").unwrap_or(&Value::Null), &lid, t, cx),
            "custom_message" => {
                let text = text_of(v.get("content").unwrap_or(&Value::Null));
                let sub = string_of(v, "customType").unwrap_or_else(|| "custom".into());
                if sub == "agent_message" && text.starts_with("[task from parent]") {
                    cx.emit(lid, t, Body::UserMessage { text, synthetic: false });
                } else {
                    cx.emit(lid, t, Body::System { subtype: sub, text });
                }
            }
            "child_usage_attributed" => {
                if let Some(u) = v.get("childUsage").filter(|u| u.is_object()) {
                    let n = |a: &str| u.get(a).and_then(Value::as_u64).unwrap_or(0);
                    let usage = Usage {
                        input: n("input"),
                        output: n("output"),
                        cache_read: n("cacheRead"),
                        cache_write: n("cacheWrite"),
                        reasoning: 0,
                    };
                    cx.emit(format!("{lid}:usage"), t, Body::Usage(usage));
                }
            }
            "agent_status" => {
                if let Some(s) = v.get("status").and_then(|s| string_of(s, "summary")) {
                    cx.emit(lid, t, Body::System { subtype: "status".into(), text: s });
                }
            }
            "compaction" => {
                let text = string_of(v, "shortSummary").or_else(|| string_of(v, "summary")).unwrap_or_default();
                cx.emit(lid, t, Body::System { subtype: "compact".into(), text });
            }
            other if IGNORED.contains(&other) => {}
            other => cx.unknown(format!("type={other}")),
        }
    }
}

fn message(m: &Value, lid: &str, t: i64, cx: &mut Cx<'_, ()>) {
    let t = if t > 0 { t } else { m.get("timestamp").and_then(ts).unwrap_or(0) };
    let content = m.get("content").unwrap_or(&Value::Null);
    match str_of(m, "role").unwrap_or("") {
        "user" => user(content, lid, t, cx),
        "assistant" => assistant(m, content, lid, t, cx),
        "toolResult" => {
            cx.emit(
                lid,
                t,
                Body::ToolResult {
                    call_id: str_of(m, "toolCallId").unwrap_or("").to_owned(),
                    name: string_of(m, "toolName"),
                    output: text_of(content),
                    is_error: m.get("isError").and_then(Value::as_bool).unwrap_or(false),
                },
            );
        }
        "bashExecution" => {
            let text = format!("$ {}\n{}", str_of(m, "command").unwrap_or(""), str_of(m, "output").unwrap_or(""));
            cx.emit(lid, t, Body::System { subtype: "bash".into(), text });
        }
        role @ ("system" | "developer") => {
            cx.emit(lid, t, Body::System { subtype: role.to_owned(), text: text_of(content) });
        }
        role => cx.unknown(format!("message.role={role}")),
    }
}

fn user(content: &Value, lid: &str, t: i64, cx: &mut Cx<'_, ()>) {
    let mut text = String::new();
    if let Value::Array(blocks) = content {
        for (i, b) in blocks.iter().enumerate() {
            if str_of(b, "type") == Some("tool_result") {
                cx.emit(
                    format!("{lid}#{i}"),
                    t,
                    Body::ToolResult {
                        call_id: str_of(b, "tool_use_id").unwrap_or("").to_owned(),
                        name: None,
                        output: text_of(b.get("content").unwrap_or(&Value::Null)),
                        is_error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                    },
                );
            } else {
                let piece = text_of(&Value::Array(vec![b.clone()]));
                if !piece.is_empty() {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&piece);
                }
            }
        }
    } else {
        text = text_of(content);
    }
    if !text.is_empty() {
        let synthetic = text.starts_with("[Wait interrupted") || text.starts_with("<system-reminder>");
        cx.emit(lid.to_owned(), t, Body::UserMessage { text, synthetic });
    }
}

fn assistant(m: &Value, content: &Value, lid: &str, t: i64, cx: &mut Cx<'_, ()>) {
    let model = string_of(m, "model");
    if let Some(model) = &model {
        cx.meta().model = Some(model.clone());
    }
    let mut calls = 0;
    let mut texts = 0;
    for (i, b) in content.as_array().map(Vec::as_slice).unwrap_or_default().iter().enumerate() {
        let body = match str_of(b, "type").unwrap_or("") {
            "text" => {
                texts += 1;
                Body::AssistantMessage { text: str_of(b, "text").unwrap_or("").to_owned(), model: model.clone() }
            }
            "thinking" => Body::Reasoning { text: str_of(b, "thinking").unwrap_or("").to_owned() },
            "redactedThinking" | "redacted_thinking" => Body::Reasoning { text: "[redacted]".into() },
            "toolCall" | "tool_use" => {
                calls += 1;
                Body::ToolCall {
                    call_id: str_of(b, "id").unwrap_or("").to_owned(),
                    name: str_of(b, "name").unwrap_or("").to_owned(),
                    input: json_arg(b.get("arguments").or_else(|| b.get("input")).unwrap_or(&Value::Null)),
                }
            }
            other => {
                cx.unknown(format!("assistant.block={other}"));
                continue;
            }
        };
        cx.emit(format!("{lid}#{i}"), t, body);
    }
    if let Some(u) = m.get("usage").filter(|u| u.is_object()) {
        let n = |a: &str| u.get(a).and_then(Value::as_u64).unwrap_or(0);
        let usage = Usage {
            input: n("input"),
            output: n("output"),
            cache_read: n("cacheRead"),
            cache_write: n("cacheWrite"),
            reasoning: 0,
        };
        cx.emit(format!("{lid}:usage"), t, Body::Usage(usage));
    }
    if let Some(stop) = str_of(m, "stopReason") {
        if ends_turn_reason(stop) {
            cx.emit(format!("{lid}:end"), t, Body::TurnEnd { reason: Some(stop.to_owned()) });
        }
    } else if calls == 0 && texts > 0 {
        cx.emit(format!("{lid}:end"), t, Body::TurnEnd { reason: None });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testkit::{Fixture, kinds};
    use serde_json::json;
    use uniflo_schema::Status;

    fn l(v: Value) -> String {
        format!("{v}\n")
    }

    #[test]
    fn prime_root_and_subagent_roundtrip() {
        let fx = Fixture::new();
        let sessions_root = fx.root().join("sessions");
        let artifacts_root = fx.root().join("session-artifacts");
        let a = JsonlAdapter::new(PrimeDecoder {
            info: HarnessInfo { id: "prime", name: "Prime Agent" },
            roots: vec![sessions_root.clone(), artifacts_root.clone()],
            procs: None,
        });

        // 1. Root session
        let root_id = "01a1073d-cca3-765c-9ae9-08b39ab7c888";
        let mut root_lines = l(json!({
            "type": "session",
            "version": 3,
            "id": root_id,
            "timestamp": "2026-10-04T14:07:33.027Z",
            "cwd": "/prime/work",
            "rlmDepth": 0
        }));
        root_lines += &l(json!({
            "type": "message",
            "id": "u1",
            "message": { "role": "user", "content": [{ "type": "text", "text": "research cn models" }] }
        }));
        root_lines += &l(json!({
            "type": "message",
            "id": "a1",
            "message": {
                "role": "assistant",
                "content": [{ "type": "text", "text": "spawning subagents" }],
                "stopReason": "stop"
            }
        }));
        let root_path = fx.write(&format!("sessions/{root_id}.jsonl"), &root_lines);
        let root_res = fx.index(&a, &root_path);
        assert_eq!(root_res.id, root_id);
        assert_eq!(root_res.meta.parent, None);
        assert_eq!(root_res.meta.cwd.as_deref(), Some("/prime/work"));
        assert_eq!(root_res.status(), Status::Idle);

        // 2. Subagent session with parentSession and session_info name
        let sub_id = "01a1073f-5f3c-73cc-95b8-7392764e7e81";
        let mut sub_lines = l(json!({
            "type": "session",
            "version": 3,
            "id": sub_id,
            "timestamp": "2026-10-04T14:08:00.000Z",
            "cwd": "/prime/work",
            "parentSession": root_path.to_str().unwrap(),
            "rlmDepth": 1
        }));
        sub_lines += &l(json!({
            "type": "session_info",
            "id": "s_info",
            "name": "cn-models"
        }));
        sub_lines += &l(json!({
            "type": "custom_message",
            "id": "c1",
            "customType": "agent_message",
            "content": "[task from parent]\n\n任务：调研国产大模型"
        }));
        sub_lines += &l(json!({
            "type": "message",
            "id": "a_sub",
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "thinking", "thinking": "analyzing models" },
                    { "type": "toolCall", "id": "call_1", "name": "web_search", "arguments": { "q": "deepseek" } }
                ],
                "stopReason": "toolUse"
            }
        }));

        let sub_rel = format!("session-artifacts/{root_id}/sub-12345678/{sub_id}.jsonl");
        let sub_path = fx.write(&sub_rel, &sub_lines);
        let sub_res = fx.index(&a, &sub_path);

        assert_eq!(sub_res.id, sub_id);
        assert_eq!(sub_res.meta.parent.as_deref(), Some(root_id));
        assert_eq!(sub_res.meta.title.as_deref(), Some("cn-models"));
        assert_eq!(kinds(&sub_res.events), vec!["user_message", "reasoning", "tool_call"]);
        // Ongoing tool call -> Work status
        assert_eq!(sub_res.status(), Status::Work);
        assert!(sub_res.unknown.is_empty(), "{:?}", sub_res.unknown);
    }
}
