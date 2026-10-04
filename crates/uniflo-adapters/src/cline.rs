//! Cline / Roo Code / Kodu task transcripts.
//!
//! Layout: `<root>/<task-id>/ui_messages.json`.
//! Each task stores UI message objects (`say` / `ask`, `ts`, `text`, `partial`).
//! Tool calls appear as `ask == "tool"` (JSON arguments), tool results as `say == "tool"`
//! (JSON output); commands as `say == "command"` and `say == "command_output"`;
//! task completion as `say == "completion_result"`.

use crate::common::under_any;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
#[cfg(not(target_os = "windows"))]
use uniflo_core::util::home;
use uniflo_core::util::{str_of, string_of};
use uniflo_core::{Cx, HarnessInfo, JsonlAdapter, LineDecoder, SourceId};
use uniflo_schema::{Body, Usage};

pub struct ClineFamily {
    pub info: HarnessInfo,
    pub roots: Vec<PathBuf>,
}

/// Cline, Roo Code, and Kodu adapters.
pub fn adapters() -> Vec<std::sync::Arc<dyn uniflo_core::Adapter>> {
    vec![std::sync::Arc::new(cline()), std::sync::Arc::new(roo()), std::sync::Arc::new(kodu())]
}

fn vscode_storage_roots(ext: &str) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let apps = ["Code", "Cursor", "Windsurf", "VSCodium", "Code - Insiders", "Positron", "Trae"];
    #[cfg(target_os = "macos")]
    {
        let h = home();
        for app in &apps {
            roots.push(
                h.join("Library/Application Support").join(app).join("User/globalStorage").join(ext).join("tasks"),
            );
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(appdata) = std::env::var_os("APPDATA").map(PathBuf::from) {
            for app in &apps {
                roots.push(appdata.join(app).join("User/globalStorage").join(ext).join("tasks"));
            }
        }
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        let h = home();
        for app in &apps {
            roots.push(h.join(".config").join(app).join("User/globalStorage").join(ext).join("tasks"));
        }
    }
    roots
}

pub fn cline() -> JsonlAdapter<ClineFamily> {
    JsonlAdapter::new(ClineFamily {
        info: HarnessInfo { id: "cline", name: "Cline" },
        roots: vscode_storage_roots("saoudrizwan.claude-dev"),
    })
}

pub fn roo() -> JsonlAdapter<ClineFamily> {
    let mut roots = vscode_storage_roots("rooveterinaryinc.roo-cline");
    roots.extend(vscode_storage_roots("roovscode.roo-cline"));
    roots.extend(vscode_storage_roots("kilocode.kilo-code"));
    JsonlAdapter::new(ClineFamily { info: HarnessInfo { id: "roo", name: "Roo Code" }, roots })
}

pub fn kodu() -> JsonlAdapter<ClineFamily> {
    JsonlAdapter::new(ClineFamily {
        info: HarnessInfo { id: "kodu", name: "Kodu" },
        roots: vscode_storage_roots("kodu-ai.kodu"),
    })
}

impl LineDecoder for ClineFamily {
    type State = ();

    fn info(&self) -> HarnessInfo {
        self.info
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }

    fn max_depth(&self) -> usize {
        3
    }

    fn whole_file(&self, _p: &Path) -> bool {
        true
    }

    fn is_source(&self, p: &Path) -> bool {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name == "ui_messages.json" {
            return under_any(p, &self.roots);
        }
        if name == "api_conversation_history.json" {
            return under_any(p, &self.roots) && !p.with_file_name("ui_messages.json").exists();
        }
        false
    }

    fn identify(&self, p: &Path) -> Option<SourceId> {
        let id = p.parent()?.file_name()?.to_str()?.to_owned();
        Some(SourceId { id, parent: None })
    }

    fn decode(&self, v: &Value, cx: &mut Cx<'_, ()>) {
        let Some(items) = v.as_array() else { return };
        if cx.src.file_name().is_some_and(|n| n == "api_conversation_history.json") {
            decode_api_history(items, cx);
            return;
        }
        let mut last_call_id = String::new();
        let mut last_cmd_id = String::new();
        for (idx, item) in items.iter().enumerate() {
            let ts = item.get("ts").and_then(Value::as_i64).unwrap_or(0);
            let say = str_of(item, "say").unwrap_or("");
            let ask = str_of(item, "ask").unwrap_or("");
            let text = str_of(item, "text").unwrap_or("");
            let partial = item.get("partial").and_then(Value::as_bool).unwrap_or(false);

            if say == "task" || (idx == 0 && say.is_empty() && ask.is_empty() && !text.is_empty()) {
                if ts > 0 && cx.meta().started_at.is_none() {
                    cx.meta().started_at = Some(ts);
                }
                let first_line = text.lines().next().unwrap_or(text).trim();
                if !first_line.is_empty() && cx.meta().title.is_none() {
                    cx.meta().title = Some((2, first_line.to_owned()));
                }
                let ev =
                    cx.emit(format!("{ts}:u{idx}"), ts, Body::UserMessage { text: text.to_owned(), synthetic: false });
                ev.partial = partial;
            } else if say == "user_feedback" {
                let ev =
                    cx.emit(format!("{ts}:u{idx}"), ts, Body::UserMessage { text: text.to_owned(), synthetic: false });
                ev.partial = partial;
            } else if say == "text" {
                let ev =
                    cx.emit(format!("{ts}:a{idx}"), ts, Body::AssistantMessage { text: text.to_owned(), model: None });
                ev.partial = partial;
            } else if say == "reasoning" {
                let ev = cx.emit(format!("{ts}:r{idx}"), ts, Body::Reasoning { text: text.to_owned() });
                ev.partial = partial;
            } else if ask == "tool" || say == "tool" {
                let parsed: Option<Value> = serde_json::from_str(text).ok();
                let tool_name = parsed.as_ref().and_then(|p| str_of(p, "tool")).unwrap_or("tool").to_owned();
                if ask == "tool" {
                    let call_id = format!("call_{ts}_{idx}");
                    last_call_id = call_id.clone();
                    let input = parsed.unwrap_or_else(|| json!({ "raw": text }));
                    let ev = cx.emit(format!("{ts}:c{idx}"), ts, Body::ToolCall { call_id, name: tool_name, input });
                    ev.partial = partial;
                } else {
                    let call_id =
                        if !last_call_id.is_empty() { last_call_id.clone() } else { format!("call_{ts}_{idx}") };
                    let output = parsed
                        .as_ref()
                        .and_then(|p| str_of(p, "content").or_else(|| str_of(p, "output")))
                        .unwrap_or(text)
                        .to_owned();
                    let is_error =
                        parsed.as_ref().and_then(|p| p.get("isError").and_then(Value::as_bool)).unwrap_or(false);
                    let ev = cx.emit(
                        format!("{ts}:r{idx}"),
                        ts,
                        Body::ToolResult { call_id, name: Some(tool_name), output, is_error },
                    );
                    ev.partial = partial;
                }
            } else if say == "command" || ask == "command" {
                let call_id = format!("call_{ts}_{idx}");
                last_cmd_id = call_id.clone();
                let ev = cx.emit(
                    format!("{ts}:cmd{idx}"),
                    ts,
                    Body::ToolCall { call_id, name: "execute_command".into(), input: json!({ "command": text }) },
                );
                ev.partial = partial;
            } else if say == "command_output" {
                let call_id = if !last_cmd_id.is_empty() { last_cmd_id.clone() } else { format!("call_{ts}_{idx}") };
                let ev = cx.emit(
                    format!("{ts}:cmdr{idx}"),
                    ts,
                    Body::ToolResult {
                        call_id,
                        name: Some("execute_command".into()),
                        output: text.to_owned(),
                        is_error: false,
                    },
                );
                ev.partial = partial;
            } else if say == "api_req_started" || say == "api_req_finished" {
                if let Ok(p) = serde_json::from_str::<Value>(text) {
                    let n = |k: &str| p.get(k).and_then(Value::as_u64).unwrap_or(0);
                    let usage = Usage {
                        input: n("tokensIn"),
                        output: n("tokensOut"),
                        cache_read: n("cacheReads"),
                        cache_write: n("cacheWrites"),
                        reasoning: 0,
                    };
                    if usage.input > 0 || usage.output > 0 {
                        cx.emit(format!("{ts}:usage{idx}"), ts, Body::Usage(usage));
                    }
                    if let Some(model) = string_of(&p, "model") {
                        cx.meta().model = Some(model);
                    }
                }
            } else if say == "completion_result" || ask == "completion_result" {
                cx.emit(format!("{ts}:end{idx}"), ts, Body::TurnEnd { reason: Some("complete".into()) });
            } else if ask == "followup" {
                cx.emit(format!("{ts}:ask{idx}"), ts, Body::AssistantMessage { text: text.to_owned(), model: None });
                cx.emit(format!("{ts}:end{idx}"), ts, Body::TurnEnd { reason: Some("ask_followup".into()) });
            } else if say == "error" || ask == "api_req_failed" {
                cx.emit(format!("{ts}:err{idx}"), ts, Body::System { subtype: "error".into(), text: text.to_owned() });
            } else if !text.is_empty() {
                let sub = if !say.is_empty() {
                    say
                } else if !ask.is_empty() {
                    ask
                } else {
                    "system"
                };
                cx.emit(format!("{ts}:sys{idx}"), ts, Body::System { subtype: sub.into(), text: text.to_owned() });
            }
        }
    }
}

fn decode_api_history(items: &[Value], cx: &mut Cx<'_, ()>) {
    let mut ts = cx.meta().started_at.unwrap_or(0);
    for (idx, item) in items.iter().enumerate() {
        let role = str_of(item, "role").unwrap_or("");
        let content = item.get("content").unwrap_or(&Value::Null);
        match role {
            "user" => {
                if let Value::Array(blocks) = content {
                    for (b_idx, b) in blocks.iter().enumerate() {
                        if str_of(b, "type") == Some("tool_result") {
                            let call_id = str_of(b, "tool_use_id").unwrap_or("").to_owned();
                            let output = match b.get("content") {
                                Some(Value::String(s)) => s.clone(),
                                Some(v) => v.to_string(),
                                None => String::new(),
                            };
                            let is_error = b.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                            cx.emit(
                                format!("u{idx}_{b_idx}"),
                                ts,
                                Body::ToolResult { call_id, name: None, output, is_error },
                            );
                        } else if let Some(text) = str_of(b, "text") {
                            if cx.meta().title.is_none() && !text.trim().is_empty() {
                                let first_line = text.lines().next().unwrap_or(text).trim();
                                cx.meta().title = Some((2, first_line.to_owned()));
                            }
                            cx.emit(
                                format!("u{idx}_{b_idx}"),
                                ts,
                                Body::UserMessage { text: text.to_owned(), synthetic: false },
                            );
                        }
                    }
                } else if let Some(text) = content.as_str() {
                    if cx.meta().title.is_none() && !text.trim().is_empty() {
                        let first_line = text.lines().next().unwrap_or(text).trim();
                        cx.meta().title = Some((2, first_line.to_owned()));
                    }
                    cx.emit(format!("u{idx}"), ts, Body::UserMessage { text: text.to_owned(), synthetic: false });
                }
            }
            "assistant" => {
                let mut calls = 0;
                let mut texts = 0;
                if let Value::Array(blocks) = content {
                    for (b_idx, b) in blocks.iter().enumerate() {
                        match str_of(b, "type").unwrap_or("") {
                            "text" => {
                                texts += 1;
                                let text = str_of(b, "text").unwrap_or("").to_owned();
                                cx.emit(format!("a{idx}_{b_idx}"), ts, Body::AssistantMessage { text, model: None });
                            }
                            "thinking" => {
                                let text = str_of(b, "thinking").unwrap_or("").to_owned();
                                cx.emit(format!("a{idx}_{b_idx}"), ts, Body::Reasoning { text });
                            }
                            "tool_use" => {
                                calls += 1;
                                let call_id = str_of(b, "id").unwrap_or("").to_owned();
                                let name = str_of(b, "name").unwrap_or("").to_owned();
                                let input = b.get("input").cloned().unwrap_or_else(|| json!({}));
                                cx.emit(format!("a{idx}_{b_idx}"), ts, Body::ToolCall { call_id, name, input });
                            }
                            _ => {}
                        }
                    }
                }
                if idx == items.len() - 1 && calls == 0 && texts > 0 {
                    cx.emit(format!("end{idx}"), ts, Body::TurnEnd { reason: Some("end_turn".into()) });
                }
            }
            _ => {}
        }
        ts += 1000;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testkit::{Fixture, kinds};
    use uniflo_schema::Status;

    #[test]
    fn cline_task_roundtrip() {
        let fx = Fixture::new();
        let adapter = JsonlAdapter::new(ClineFamily {
            info: HarnessInfo { id: "cline", name: "Cline" },
            roots: vec![fx.root().to_path_buf()],
        });

        let json_body = serde_json::to_string(&json!([
            {
                "ts": 1715000000000i64,
                "type": "say",
                "say": "task",
                "text": "Fix the bug in main.rs"
            },
            {
                "ts": 1715000001000i64,
                "type": "say",
                "say": "text",
                "text": "Inspecting file..."
            },
            {
                "ts": 1715000002000i64,
                "type": "ask",
                "ask": "tool",
                "text": "{\"tool\":\"read_file\",\"path\":\"main.rs\"}"
            },
            {
                "ts": 1715000003000i64,
                "type": "say",
                "say": "tool",
                "text": "{\"tool\":\"read_file\",\"content\":\"fn main() {}\",\"isError\":false}"
            },
            {
                "ts": 1715000004000i64,
                "type": "say",
                "say": "command",
                "text": "cargo test"
            },
            {
                "ts": 1715000005000i64,
                "type": "say",
                "say": "command_output",
                "text": "test result: ok"
            },
            {
                "ts": 1715000006000i64,
                "type": "say",
                "say": "api_req_finished",
                "text": "{\"tokensIn\":120,\"tokensOut\":45,\"cacheReads\":0,\"cacheWrites\":0,\"model\":\"claude-3-5-sonnet\"}"
            },
            {
                "ts": 1715000007000i64,
                "type": "say",
                "say": "completion_result",
                "text": "Task finished."
            }
        ])).unwrap();

        let path = fx.write("task_123/ui_messages.json", &json_body);
        let r = fx.index(&adapter, &path);

        assert_eq!(r.id, "task_123");
        assert_eq!(r.meta.title.as_deref(), Some("Fix the bug in main.rs"));
        assert_eq!(r.meta.model.as_deref(), Some("claude-3-5-sonnet"));
        assert_eq!(
            kinds(&r.events),
            vec![
                "user_message",
                "assistant_message",
                "tool_call",
                "tool_result",
                "tool_call",
                "tool_result",
                "usage",
                "turn_end"
            ]
        );
        assert_eq!(r.status(), Status::Idle);
    }

    #[test]
    fn cline_api_history_fallback_roundtrip() {
        let fx = Fixture::new();
        let adapter = JsonlAdapter::new(ClineFamily {
            info: HarnessInfo { id: "cline", name: "Cline" },
            roots: vec![fx.root().to_path_buf()],
        });

        let json_body = serde_json::to_string(&json!([
            {
                "role": "user",
                "content": "Create a new hello world file"
            },
            {
                "role": "assistant",
                "content": [
                    { "type": "thinking", "thinking": "Planning file creation..." },
                    { "type": "tool_use", "id": "call_99", "name": "write_to_file", "input": { "path": "hello.txt" } }
                ]
            },
            {
                "role": "user",
                "content": [
                    { "type": "tool_result", "tool_use_id": "call_99", "content": "File created" }
                ]
            },
            {
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "Successfully created hello.txt" }
                ]
            }
        ]))
        .unwrap();

        // Write only api_conversation_history.json without ui_messages.json
        let path = fx.write("task_456/api_conversation_history.json", &json_body);
        let r = fx.index(&adapter, &path);

        assert_eq!(r.id, "task_456");
        assert_eq!(r.meta.title.as_deref(), Some("Create a new hello world file"));
        assert_eq!(
            kinds(&r.events),
            vec!["user_message", "reasoning", "tool_call", "tool_result", "assistant_message", "turn_end"]
        );
        assert_eq!(r.status(), Status::Idle);
    }
}
