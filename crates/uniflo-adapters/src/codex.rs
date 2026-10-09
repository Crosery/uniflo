//! OpenAI Codex rollouts: `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`.
//!
//! `response_item` lines are the transcript (Responses API items); `event_msg`
//! lines carry turn boundaries (`task_started` / `task_complete` / `turn_aborted`)
//! and token counts. Other `event_msg` kinds duplicate response items and are skipped.
//!
//! Usage: newer rollouts write a `token_usage_record` per response (authoritative, keyed
//! by `response_id`); once one appears, `token_count` lines only repeat them and are
//! skipped. Older rollouts only have `token_count`; a repeat whose `total_token_usage` did
//! not move reports no new call (rate-limit refreshes) and is skipped too. OpenAI counts
//! cached prompt tokens inside `input_tokens` and reasoning inside `output_tokens`.

use crate::common::output_with_reasoning;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use uniflo_core::util::{home, json_arg, str_of, string_of, text_of, ts};
use uniflo_core::{Cx, HarnessInfo, JsonlAdapter, LineDecoder, SourceId};
use uniflo_schema::{Body, Usage};

pub struct Codex {
    pub roots: Vec<PathBuf>,
}

pub fn adapters() -> Vec<std::sync::Arc<dyn uniflo_core::Adapter>> {
    vec![std::sync::Arc::new(codex())]
}

pub fn codex() -> JsonlAdapter<Codex> {
    let h = home();
    JsonlAdapter::new(Codex { roots: vec![h.join(".codex/sessions"), h.join(".codex/archived_sessions")] })
}

const IGNORED: &[&str] = &["world_state", "inter_agent_communication_metadata"];

/// Usage dedupe and model state carried across lines.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    model: Option<String>,
    /// This rollout writes `token_usage_record`s; its `token_count`s repeat them.
    #[serde(default)]
    records: bool,
    /// `total_token_usage` of the last `token_count`.
    #[serde(default)]
    last_total: Option<[u64; 5]>,
}

/// Leading markers of user-role items the CLI injects (environment, instructions).
const INJECTED: &[&str] = &[
    "<environment_context>",
    "<user_instructions>",
    "# AGENTS.md instructions",
    "<permissions instructions>",
    "<turn_aborted>",
    "<subagent_notification>",
    "<collaboration_mode>",
    "<skill>",
];

impl LineDecoder for Codex {
    type State = State;

    fn info(&self) -> HarnessInfo {
        HarnessInfo { id: "codex", name: "Codex" }
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }

    fn is_source(&self, p: &Path) -> bool {
        p.extension().is_some_and(|e| e == "jsonl")
            && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("rollout-"))
            && crate::common::under_any(p, &self.roots)
    }

    fn cleanup_targets(&self, src: &Path) -> Option<Vec<PathBuf>> {
        uniflo_core::cleanup::targets::file(src)
    }

    fn identify(&self, p: &Path) -> Option<SourceId> {
        let stem = p.file_stem()?.to_str()?;
        // rollout-2026-10-02T18-11-56-<36-char uuid>
        let id = if stem.len() > 36 { &stem[stem.len() - 36..] } else { stem };
        Some(SourceId { id: id.to_owned(), parent: None })
    }

    fn decode(&self, v: &Value, cx: &mut Cx<'_, State>) {
        let t = v.get("timestamp").and_then(ts).unwrap_or(0);
        let p = v.get("payload").unwrap_or(&Value::Null);
        match str_of(v, "type").unwrap_or("") {
            "session_meta" => {
                let m = cx.meta();
                m.cwd = string_of(p, "cwd");
                m.started_at = p.get("timestamp").and_then(ts).or(Some(t)).filter(|t| *t > 0);
                m.parent = p
                    .pointer("/source/subagent/thread_spawn/parent_thread_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            "turn_context" => {
                if let Some(model) = string_of(p, "model") {
                    cx.state.model = Some(model.clone());
                    cx.meta().model = Some(model);
                }
                if let Some(cwd) = string_of(p, "cwd") {
                    cx.meta().cwd = Some(cwd);
                }
            }
            "response_item" => response_item(p, t, cx),
            "event_msg" => event_msg(p, t, cx),
            "token_usage_record" => {
                let Some(u) = p.get("usage").filter(|u| u.is_object()) else { return };
                let usage = openai_usage(u, cx.state.model.clone());
                cx.state.records = true;
                match string_of(p, "response_id") {
                    Some(r) => cx.emit(format!("{r}:usage"), t, Body::Usage(usage)),
                    None => cx.emit_at(t, Body::Usage(usage)),
                };
            }
            "compacted" => {
                cx.emit_at(
                    t,
                    Body::System { subtype: "compact".into(), text: string_of(p, "message").unwrap_or_default() },
                );
            }
            ty if IGNORED.contains(&ty) => {}
            ty => cx.unknown(format!("type={ty}")),
        }
    }
}

fn item_id(p: &Value) -> Option<String> {
    string_of(p, "id")
}

fn emit(cx: &mut Cx<'_, State>, id: Option<String>, t: i64, body: Body) {
    match id {
        Some(id) => cx.emit(id, t, body),
        None => cx.emit_at(t, body),
    };
}

fn response_item(p: &Value, t: i64, cx: &mut Cx<'_, State>) {
    let id = item_id(p);
    let call_id = || str_of(p, "call_id").or_else(|| str_of(p, "id")).unwrap_or("").to_owned();
    let body = match str_of(p, "type").unwrap_or("") {
        "message" => {
            let text = text_of(p.get("content").unwrap_or(&Value::Null));
            match str_of(p, "role").unwrap_or("") {
                "user" => {
                    let synthetic = INJECTED.iter().any(|m| text.trim_start().starts_with(m));
                    Body::UserMessage { text, synthetic }
                }
                "assistant" => Body::AssistantMessage { text, model: None },
                role => {
                    Body::System { subtype: if role.is_empty() { "message".into() } else { role.to_owned() }, text }
                }
            }
        }
        "reasoning" => {
            let mut text = p.get("summary").map(text_of).unwrap_or_default();
            if text.is_empty() {
                text = p.get("content").map(text_of).unwrap_or_default();
            }
            if text.is_empty() {
                return;
            }
            Body::Reasoning { text }
        }
        "function_call" => Body::ToolCall {
            call_id: call_id(),
            name: str_of(p, "name").unwrap_or("").to_owned(),
            input: json_arg(p.get("arguments").unwrap_or(&Value::Null)),
        },
        "custom_tool_call" => Body::ToolCall {
            call_id: call_id(),
            name: str_of(p, "name").unwrap_or("").to_owned(),
            input: p.get("input").cloned().unwrap_or_default(),
        },
        "local_shell_call" => Body::ToolCall {
            call_id: call_id(),
            name: "shell".into(),
            input: p.get("action").cloned().unwrap_or_default(),
        },
        "web_search_call" => {
            let cid = call_id();
            let input = p.get("action").or_else(|| p.get("arguments")).cloned().unwrap_or_default();
            emit(cx, id.clone(), t, Body::ToolCall { call_id: cid.clone(), name: "web_search".into(), input });
            let status = str_of(p, "status");
            if status == Some("completed") || status.is_none() {
                let output = if let Some(queries) = p.pointer("/action/queries").and_then(Value::as_array) {
                    let qs: Vec<_> = queries.iter().filter_map(Value::as_str).collect();
                    if qs.is_empty() {
                        string_of(p, "query").unwrap_or_else(|| "Search completed".into())
                    } else {
                        format!(
                            "Search queries executed:\n{}",
                            qs.iter().map(|q| format!("• {q}")).collect::<Vec<_>>().join("\n")
                        )
                    }
                } else if let Some(q) = string_of(p, "query")
                    .or_else(|| p.pointer("/action/query").and_then(Value::as_str).map(str::to_owned))
                {
                    format!("Search query: {q}")
                } else {
                    "Search completed".to_owned()
                };
                let res_id = id.map(|i| format!("{i}:res")).unwrap_or_else(|| format!("{cid}:res"));
                cx.emit(
                    res_id,
                    t,
                    Body::ToolResult { call_id: cid, name: Some("web_search".into()), output, is_error: false },
                );
            }
            return;
        }
        "image_generation_call" | "tool_search_call" => {
            let cid = call_id();
            let name = str_of(p, "type").unwrap_or("").trim_end_matches("_call").to_owned();
            let input = p.get("action").or_else(|| p.get("arguments")).cloned().unwrap_or_default();
            emit(cx, id.clone(), t, Body::ToolCall { call_id: cid.clone(), name: name.clone(), input });
            if str_of(p, "status") == Some("completed") {
                let output = string_of(p, "revised_prompt")
                    .or_else(|| p.pointer("/arguments/query").and_then(Value::as_str).map(str::to_owned))
                    .unwrap_or_else(|| format!("{name} completed"));
                let res_id = id.map(|i| format!("{i}:res")).unwrap_or_else(|| format!("{cid}:res"));
                cx.emit(res_id, t, Body::ToolResult { call_id: cid, name: Some(name), output, is_error: false });
            }
            return;
        }
        "function_call_output" | "custom_tool_call_output" | "tool_search_output" => {
            let (output, is_error) = tool_output(p.get("output").unwrap_or(&Value::Null));
            Body::ToolResult { call_id: call_id(), name: None, output, is_error }
        }
        "agent_message" => {
            Body::System { subtype: "agent_message".into(), text: text_of(p.get("content").unwrap_or(&Value::Null)) }
        }
        "compaction" => Body::System { subtype: "compact".into(), text: String::new() },
        other => {
            cx.unknown(format!("response_item={other}"));
            return;
        }
    };
    emit(cx, id, t, body);
}

/// Tool output is text, a `{output, metadata:{exit_code}}` JSON string, or content parts.
fn tool_output(v: &Value) -> (String, bool) {
    let s = match v {
        Value::String(s) => s.clone(),
        other => text_of(other),
    };
    if s.starts_with('{')
        && let Ok(o) = serde_json::from_str::<Value>(&s)
        && let Some(out) = o.get("output").and_then(Value::as_str)
    {
        let code = o.pointer("/metadata/exit_code").and_then(Value::as_i64).unwrap_or(0);
        return (out.to_owned(), code != 0);
    }
    let failed = s
        .strip_prefix("Exit code: ")
        .and_then(|r| r.split_whitespace().next())
        .and_then(|c| c.parse::<i64>().ok())
        .is_some_and(|c| c != 0);
    (s, failed)
}

/// Responses-API token counts → the usage contract (cached and cache-write prompt tokens
/// are inside `input_tokens`; reasoning inside `output_tokens`).
fn openai_usage(u: &Value, model: Option<String>) -> Usage {
    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    let (cached, written) = (n("cached_input_tokens"), n("cache_write_input_tokens"));
    let reasoning = n("reasoning_output_tokens");
    Usage {
        input: n("input_tokens").saturating_sub(cached + written),
        output: output_with_reasoning(n("output_tokens"), reasoning),
        cache_read: cached,
        cache_write: written,
        reasoning,
        model,
        cost_usd: None,
    }
}

fn event_msg(p: &Value, t: i64, cx: &mut Cx<'_, State>) {
    let turn = str_of(p, "turn_id").unwrap_or("");
    match str_of(p, "type").unwrap_or("") {
        "task_started" => {
            cx.emit(format!("turn:{turn}:start"), t, Body::TurnStart {});
        }
        "task_complete" => {
            cx.emit(format!("turn:{turn}:end"), t, Body::TurnEnd { reason: Some("complete".into()) });
        }
        "turn_aborted" => {
            let reason = string_of(p, "reason").unwrap_or_else(|| "aborted".into());
            cx.emit(format!("turn:{turn}:end"), t, Body::TurnEnd { reason: Some(reason) });
        }
        "token_count" => {
            let Some(u) = p.pointer("/info/last_token_usage").filter(|u| u.is_object()) else { return };
            let total = p.pointer("/info/total_token_usage").map(|x| {
                let n = |k: &str| x.get(k).and_then(Value::as_u64).unwrap_or(0);
                [
                    n("input_tokens"),
                    n("cached_input_tokens"),
                    n("output_tokens"),
                    n("reasoning_output_tokens"),
                    n("total_tokens"),
                ]
            });
            let repeat = total.is_some() && total == cx.state.last_total;
            if total.is_some() {
                cx.state.last_total = total;
            }
            if cx.state.records || repeat {
                return;
            }
            let usage = openai_usage(u, cx.state.model.clone());
            cx.emit_at(t, Body::Usage(usage));
        }
        "error" | "stream_error" => {
            cx.emit_at(t, Body::System { subtype: "error".into(), text: string_of(p, "message").unwrap_or_default() });
        }
        // Everything else mirrors response items or UI state.
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testkit::{Fixture, kinds};
    use serde_json::json;
    use uniflo_core::Adapter;
    use uniflo_schema::Status;

    const ID: &str = "01a0fc19-6031-7671-8bb4-22f1aa7f90c3";

    fn adapter(fx: &Fixture) -> JsonlAdapter<Codex> {
        JsonlAdapter::new(Codex { roots: vec![fx.root().to_path_buf()] })
    }

    fn l(ty: &str, payload: Value) -> String {
        format!("{}\n", json!({"timestamp":"2026-10-02T10:11:56.000Z","type":ty,"payload":payload}))
    }

    #[test]
    fn turn_lifecycle_and_items() {
        let fx = Fixture::new();
        let a = adapter(&fx);
        let mut s = l("session_meta", json!({"id":ID,"timestamp":"2026-10-02T10:11:56Z","cwd":"/repo","source":"cli"}));
        s += &l("turn_context", json!({"turn_id":"t1","cwd":"/repo","model":"gpt-x"}));
        s += &l("event_msg", json!({"type":"task_started","turn_id":"t1"}));
        s += &l(
            "response_item",
            json!({"type":"message","role":"developer","content":[{"type":"input_text","text":"rules"}]}),
        );
        s += &l(
            "response_item",
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>x</environment_context>"}]}),
        );
        s += &l(
            "response_item",
            json!({"type":"message","id":"msg_u","role":"user","content":[{"type":"input_text","text":"run tests"}]}),
        );
        s += &l(
            "response_item",
            json!({"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"plan"}],"encrypted_content":"zz"}),
        );
        s += &l("response_item", json!({"type":"reasoning","id":"rs_2","summary":[],"encrypted_content":"zz"}));
        s += &l(
            "response_item",
            json!({"type":"function_call","id":"fc_1","name":"exec_command","arguments":"{\"cmd\":\"cargo test\"}","call_id":"call_1"}),
        );
        s += &l(
            "response_item",
            json!({"type":"function_call_output","call_id":"call_1","output":"Exit code: 101\nWall time: 1s\nfailed"}),
        );
        s += &l(
            "response_item",
            json!({"type":"custom_tool_call","call_id":"call_2","name":"apply_patch","input":"*** Begin Patch"}),
        );
        s += &l(
            "response_item",
            json!({"type":"custom_tool_call_output","call_id":"call_2","output":"{\"output\":\"Success\",\"metadata\":{\"exit_code\":0}}"}),
        );
        s += &l(
            "event_msg",
            json!({"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":7,"reasoning_output_tokens":3}}}),
        );
        s += &l(
            "response_item",
            json!({"type":"message","id":"msg_a","role":"assistant","content":[{"type":"output_text","text":"fixed"}]}),
        );
        s += &l("event_msg", json!({"type":"item_completed","item":{}}));
        s += &l("event_msg", json!({"type":"task_complete","turn_id":"t1"}));
        s += &l("world_state", json!({}));
        let p = fx.write(&format!("2026/10/02/rollout-2026-10-02T18-11-56-{ID}.jsonl"), &s);

        let r = fx.index(&a, &p);
        assert_eq!(r.id, ID);
        assert_eq!(
            kinds(&r.events),
            vec![
                "turn_start",
                "system",
                "user_message",
                "user_message",
                "reasoning",
                "tool_call",
                "tool_result",
                "tool_call",
                "tool_result",
                "usage",
                "assistant_message",
                "turn_end"
            ]
        );
        assert!(matches!(&r.events[2].body, Body::UserMessage { synthetic: true, .. }));
        assert!(matches!(&r.events[3].body, Body::UserMessage { synthetic: false, text } if text == "run tests"));
        assert_eq!(r.events[3].id, "msg_u");
        assert!(matches!(&r.events[5].body, Body::ToolCall { input, .. } if input["cmd"] == "cargo test"));
        assert!(matches!(&r.events[6].body, Body::ToolResult { is_error: true, call_id, .. } if call_id == "call_1"));
        assert!(matches!(&r.events[8].body, Body::ToolResult { is_error: false, output, .. } if output == "Success"));
        assert!(matches!(&r.events[9].body, Body::Usage(u)
            if (u.input, u.cache_read, u.output, u.reasoning) == (60, 40, 7, 3) && u.model.as_deref() == Some("gpt-x")));
        assert_eq!(r.status(), Status::Idle);
        assert_eq!(r.meta.cwd.as_deref(), Some("/repo"));
        assert_eq!(r.meta.model.as_deref(), Some("gpt-x"));
        assert_eq!(r.preview.as_deref(), Some("run tests"));
        assert!(r.unknown.is_empty(), "{:?}", r.unknown);
    }

    #[test]
    fn usage_record_wins_and_repeated_token_counts_are_skipped() {
        let fx = Fixture::new();
        let a = adapter(&fx);
        let tc = |last: Value, total: Value| {
            l("event_msg", json!({"type":"token_count","info":{"last_token_usage":last,"total_token_usage":total}}))
        };
        let u1 = json!({"input_tokens":100,"cached_input_tokens":40,"output_tokens":10,"reasoning_output_tokens":4,"total_tokens":110});
        let u2 = json!({"input_tokens":200,"cached_input_tokens":150,"output_tokens":5,"reasoning_output_tokens":0,"total_tokens":205});
        let t1 = json!({"input_tokens":100,"total_tokens":110});
        let t2 = json!({"input_tokens":300,"total_tokens":315});

        // Older rollout: token_count only; a refresh with an unchanged total is no new call.
        let mut s = l("session_meta", json!({"id":ID,"cwd":"/r"}));
        s += &l("turn_context", json!({"model":"gpt-x"}));
        s += &tc(u1.clone(), t1.clone());
        s += &tc(u1.clone(), t1.clone());
        s += &tc(u2.clone(), t2.clone());
        let p = fx.write(&format!("old/rollout-2026-10-02T18-11-56-{ID}.jsonl"), &s);
        let r = fx.index(&a, &p);
        let usage: Vec<_> =
            r.events.iter().filter_map(|e| if let Body::Usage(u) = &e.body { Some(u) } else { None }).collect();
        assert_eq!(usage.len(), 2);
        assert_eq!((usage[1].input, usage[1].cache_read, usage[1].output), (50, 150, 5));

        // Newer rollout: token_usage_record is authoritative, every token_count repeats it.
        let mut s = l("session_meta", json!({"id":ID,"cwd":"/r"}));
        s += &l("turn_context", json!({"model":"gpt-y"}));
        s += &l("token_usage_record", json!({"response_id":"resp_1","usage":u1}));
        s += &l("response_item", json!({"type":"function_call_output","call_id":"c","output":"ok"}));
        s += &tc(json!({"input_tokens":1}), t1.clone());
        s += &tc(json!({"input_tokens":1}), t1);
        s += &l("token_usage_record", json!({"response_id":"resp_2","usage":u2}));
        s += &tc(json!({"input_tokens":2}), t2);
        let p = fx.write(&format!("new/rollout-2026-10-02T18-11-56-{ID}.jsonl"), &s);
        let r = fx.index(&a, &p);
        let usage: Vec<_> = r.events.iter().filter(|e| matches!(e.body, Body::Usage(_))).collect();
        assert_eq!(usage.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), vec!["resp_1:usage", "resp_2:usage"]);
        assert!(matches!(&usage[0].body, Body::Usage(u)
            if (u.input, u.cache_read, u.output, u.reasoning) == (60, 40, 10, 4) && u.model.as_deref() == Some("gpt-y")));
        assert!(r.unknown.is_empty(), "{:?}", r.unknown);
    }

    #[test]
    fn mid_turn_is_work_and_subagent_links_parent() {
        let fx = Fixture::new();
        let a = adapter(&fx);
        let mut s = l(
            "session_meta",
            json!({"id":ID,"cwd":"/r","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent-1","depth":1}}}}),
        );
        s += &l("event_msg", json!({"type":"task_started","turn_id":"t1"}));
        s += &l("response_item", json!({"type":"function_call","name":"x","arguments":"{}","call_id":"c"}));
        let p = fx.write(&format!("rollout-2026-10-02T18-11-56-{ID}.jsonl"), &s);
        let r = fx.index(&a, &p);
        assert_eq!(r.status(), Status::Work);
        assert_eq!(r.meta.parent.as_deref(), Some("parent-1"));

        fx.append(&p, &l("event_msg", json!({"type":"turn_aborted","turn_id":"t1","reason":"interrupted"})));
        let out = a.read(&p, None).unwrap();
        let r = crate::common::testkit::group(out.batch).pop_first().unwrap().1;
        assert_eq!(r.status(), Status::Idle);
    }

    #[test]
    fn non_rollout_files_are_not_sources() {
        let fx = Fixture::new();
        let a = adapter(&fx);
        assert!(!a.decoder.is_source(&fx.root().join("session_index.jsonl")));
        assert!(!a.decoder.is_source(Path::new("/elsewhere/rollout-x.jsonl")));
        assert!(a.decoder.is_source(&fx.root().join("2026/rollout-x.jsonl")));
    }
}
