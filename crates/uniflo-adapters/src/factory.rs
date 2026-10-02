//! Factory Droid transcripts.
//!
//! Layout: `~/.factory/sessions/<cwd-slug>/<uuid>.jsonl` (+ sibling `<uuid>.settings.json`, which
//! only supplies the model). Messages use Anthropic-style content blocks; there is no stop
//! reason, so a text-only assistant reply ends the turn.

use crate::common::under_any;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uniflo_core::util::{home, json_arg, str_of, string_of, text_of, ts};
use uniflo_core::{Adapter, Cx, HarnessInfo, JsonlAdapter, LineDecoder, SourceId};
use uniflo_schema::Body;

pub struct Factory {
    roots: Vec<PathBuf>,
}

pub fn adapters() -> Vec<Arc<dyn Adapter>> {
    vec![Arc::new(JsonlAdapter::new(Factory { roots: vec![home().join(".factory/sessions")] }))]
}

impl LineDecoder for Factory {
    /// Model name read from the settings sidecar at `session_start`.
    type State = String;

    fn info(&self) -> HarnessInfo {
        HarnessInfo { id: "factory", name: "Factory Droid" }
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }

    fn is_source(&self, p: &Path) -> bool {
        p.extension().is_some_and(|e| e == "jsonl") && under_any(p, &self.roots)
    }

    fn identify(&self, p: &Path) -> Option<SourceId> {
        Some(SourceId { id: p.file_stem()?.to_str()?.to_owned(), parent: None })
    }

    fn decode(&self, v: &Value, cx: &mut Cx<'_, String>) {
        let ty = str_of(v, "type").unwrap_or("");
        let t = v.get("timestamp").and_then(ts).unwrap_or(0);
        let id = str_of(v, "id").map(str::to_owned).unwrap_or_else(|| format!("o{}", cx.pos));
        match ty {
            "session_start" => {
                if let Some(cwd) = string_of(v, "cwd") {
                    cx.meta().cwd = Some(cwd);
                }
                if let Some(title) = string_of(v, "title") {
                    cx.meta().title = Some((1, title));
                }
                if let Some(title) = string_of(v, "sessionTitle") {
                    cx.meta().title = Some((2, title));
                }
                let settings = cx.src.with_extension("settings.json");
                if let Some(model) = std::fs::read(settings)
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                    .and_then(|s| string_of(&s, "model"))
                {
                    *cx.state = model.clone();
                    cx.meta().model = Some(model);
                }
            }
            "message" => message(v, id, t, cx),
            "compaction_state" => {
                let text = str_of(v, "summaryText").unwrap_or("").to_owned();
                cx.emit(id, t, Body::System { subtype: "compact".into(), text });
            }
            _ => cx.unknown(format!("type={ty}")),
        }
    }
}

fn message(v: &Value, id: String, t: i64, cx: &mut Cx<'_, String>) {
    let Some(m) = v.get("message") else { return };
    let content = m.get("content").unwrap_or(&Value::Null);
    let role = str_of(m, "role").unwrap_or("");
    if !matches!(role, "user" | "assistant") {
        cx.unknown(format!("message.role={role}"));
        return;
    }
    let blocks: Vec<Value> = match content {
        Value::String(s) => vec![serde_json::json!({"type":"text","text":s})],
        Value::Array(a) => a.clone(),
        _ => Vec::new(),
    };
    let model = Some(cx.state.clone()).filter(|m| !m.is_empty());
    let (mut calls, mut texts) = (0, 0);
    let mut user_text = String::new();
    for (i, b) in blocks.iter().enumerate() {
        let ty = str_of(b, "type").unwrap_or("");
        let eid = format!("{id}#{i}");
        let body = match (role, ty) {
            ("user", "text") => {
                if !user_text.is_empty() {
                    user_text.push('\n');
                }
                user_text.push_str(str_of(b, "text").unwrap_or(""));
                continue;
            }
            ("user", "tool_result") => Body::ToolResult {
                call_id: str_of(b, "tool_use_id").unwrap_or("").to_owned(),
                name: None,
                output: text_of(b.get("content").unwrap_or(&Value::Null)),
                is_error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
            },
            ("user", other) => {
                if !user_text.is_empty() {
                    user_text.push('\n');
                }
                user_text.push_str(&format!("[{other}]"));
                continue;
            }
            ("assistant", "text") => {
                texts += 1;
                Body::AssistantMessage { text: str_of(b, "text").unwrap_or("").to_owned(), model: model.clone() }
            }
            ("assistant", "thinking") => Body::Reasoning { text: str_of(b, "thinking").unwrap_or("").to_owned() },
            ("assistant", "redacted_thinking") => Body::Reasoning { text: "[redacted]".into() },
            ("assistant", "tool_use") => {
                calls += 1;
                Body::ToolCall {
                    call_id: str_of(b, "id").unwrap_or("").to_owned(),
                    name: str_of(b, "name").unwrap_or("").to_owned(),
                    input: json_arg(b.get("input").unwrap_or(&Value::Null)),
                }
            }
            (_, other) => {
                cx.unknown(format!("{role}.block={other}"));
                continue;
            }
        };
        cx.emit(eid, t, body);
    }
    if !user_text.is_empty() {
        let synthetic = user_text.trim_start().starts_with("<system-reminder>");
        cx.emit(format!("{id}#t"), t, Body::UserMessage { text: user_text, synthetic });
    }
    if role == "assistant" && calls == 0 && texts > 0 {
        cx.emit(format!("{id}:end"), t, Body::TurnEnd { reason: Some("stop".into()) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testkit::{Fixture, kinds};
    use serde_json::json;
    use uniflo_schema::Status;

    fn fa(fx: &Fixture) -> JsonlAdapter<Factory> {
        JsonlAdapter::new(Factory { roots: vec![fx.root().join("sessions")] })
    }

    fn l(v: Value) -> String {
        format!("{v}\n")
    }

    fn msg(id: &str, role: &str, content: Value) -> String {
        l(
            json!({"type":"message","id":id,"timestamp":"2026-10-02T12:00:00Z","message":{"role":role,"content":content}}),
        )
    }

    fn start() -> String {
        l(json!({"type":"session_start","id":"u-1","title":"First words","sessionTitle":"Named","cwd":"/w"}))
    }

    #[test]
    fn full_turn_maps_every_record_kind() {
        let fx = Fixture::new();
        let a = fa(&fx);
        fx.write("sessions/-w/u-1.settings.json", &json!({"model":"droid-m"}).to_string());
        let mut s = start();
        s += &msg("m1", "user", json!([{"type":"text","text":"hi"}]));
        s += &msg(
            "m2",
            "assistant",
            json!([{"type":"thinking","thinking":"hm"},{"type":"tool_use","id":"t1","name":"Execute","input":{"command":"ls"}}]),
        );
        s += &msg("m3", "user", json!([{"type":"tool_result","tool_use_id":"t1","content":"ok","is_error":true}]));
        s += &l(json!({"type":"compaction_state","id":"c1","timestamp":"2026-10-02T12:00:03Z","summaryText":"sum"}));
        s += &msg("m4", "assistant", json!([{"type":"text","text":"done"}]));
        let p = fx.write("sessions/-w/u-1.jsonl", &s);
        let r = fx.index(&a, &p);
        assert_eq!(
            kinds(&r.events),
            vec!["user_message", "reasoning", "tool_call", "tool_result", "system", "assistant_message", "turn_end"]
        );
        assert_eq!(r.status(), Status::Idle);
        assert!(r.unknown.is_empty(), "{:?}", r.unknown);
        assert_eq!(r.meta.title.as_deref(), Some("Named"));
        assert_eq!(r.meta.cwd.as_deref(), Some("/w"));
        assert_eq!(r.meta.model.as_deref(), Some("droid-m"));
        assert!(
            matches!(&r.events[2].body, Body::ToolCall { call_id, name, input } if call_id == "t1" && name == "Execute" && input["command"] == "ls")
        );
        assert!(matches!(&r.events[3].body, Body::ToolResult { is_error: true, output, .. } if output == "ok"));
        assert!(matches!(&r.events[4].body, Body::System { subtype, text } if subtype == "compact" && text == "sum"));
        assert_eq!(r.events[0].ts, 1790942400000);
        assert!(matches!(&r.events[5].body, Body::AssistantMessage { model: Some(m), .. } if m == "droid-m"));
    }

    #[test]
    fn mid_turn_is_work_and_unknown_reported() {
        let fx = Fixture::new();
        let a = fa(&fx);
        let mut s = start();
        s += &msg("m1", "user", json!("plain string"));
        s += &msg("m2", "assistant", json!([{"type":"tool_use","id":"t1","name":"Read","input":{}}]));
        s += &l(json!({"type":"weird","id":"x"}));
        let p = fx.write("sessions/-w/u-1.jsonl", &s);
        let r = fx.index(&a, &p);
        assert_eq!(r.status(), Status::Work);
        assert_eq!(r.unknown, vec!["type=weird".to_string()]);
        assert_eq!(r.preview.as_deref(), Some("plain string"));
    }

    #[test]
    fn is_source_and_identity() {
        let fx = Fixture::new();
        let a = fa(&fx);
        let main = fx.write("sessions/-w/abc.jsonl", "");
        let settings = fx.write("sessions/-w/abc.settings.json", "{}");
        let other = fx.write("elsewhere/abc.jsonl", "");
        assert!(uniflo_core::Adapter::source_for(&a, &main).is_some());
        assert!(uniflo_core::Adapter::source_for(&a, &settings).is_none());
        assert!(uniflo_core::Adapter::source_for(&a, &other).is_none());
        assert_eq!(uniflo_core::Adapter::discover(&a), vec![main.clone()]);
        assert_eq!(a.decoder.identify(&main), Some(SourceId { id: "abc".into(), parent: None }));
    }
}
