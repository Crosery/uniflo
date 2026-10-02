//! Cursor agent CLI transcripts.
//!
//! Layout: `~/.cursor/projects/[<slug>/]agent-transcripts/<chat>/<chat>.jsonl`. Records are
//! `{role, message:{content[...]}}` with no ids or timestamps, so events use position ids and
//! `ts = 0`. Tool calls carry no id (call ids are position-derived) and results are not logged;
//! a text-only assistant record ends the turn.

use crate::common::under_any;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uniflo_core::util::{home, json_arg, str_of};
use uniflo_core::{Adapter, Cx, HarnessInfo, JsonlAdapter, LineDecoder, SourceId};
use uniflo_schema::Body;

pub struct Cursor {
    roots: Vec<PathBuf>,
}

pub fn adapters() -> Vec<Arc<dyn Adapter>> {
    vec![Arc::new(JsonlAdapter::new(Cursor { roots: vec![home().join(".cursor/projects")] }))]
}

/// Cursor wraps the human's prompt in `<user_query>` tags.
fn unwrap_query(s: &str) -> &str {
    let t = s.trim();
    t.strip_prefix("<user_query>").and_then(|r| r.strip_suffix("</user_query>")).map_or(s, str::trim)
}

impl LineDecoder for Cursor {
    type State = ();

    fn info(&self) -> HarnessInfo {
        HarnessInfo { id: "cursor", name: "Cursor Agent" }
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }

    fn is_source(&self, p: &Path) -> bool {
        let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else { return false };
        p.extension().is_some_and(|e| e == "jsonl")
            && p.parent().and_then(|d| d.file_name()).is_some_and(|d| d == stem)
            && p.iter().any(|c| c == "agent-transcripts")
            && under_any(p, &self.roots)
    }

    fn identify(&self, p: &Path) -> Option<SourceId> {
        Some(SourceId { id: p.file_stem()?.to_str()?.to_owned(), parent: None })
    }

    fn decode(&self, v: &Value, cx: &mut Cx<'_, ()>) {
        let role = str_of(v, "role").unwrap_or("");
        let content = v.pointer("/message/content").unwrap_or(&Value::Null);
        let parts: Vec<Value> = match content {
            Value::String(s) => vec![serde_json::json!({"type":"text","text":s})],
            Value::Array(a) => a.clone(),
            _ => Vec::new(),
        };
        match role {
            "user" => {
                let mut text = String::new();
                for p in &parts {
                    let piece = match str_of(p, "type").unwrap_or("") {
                        "text" => unwrap_query(str_of(p, "text").unwrap_or("")).to_owned(),
                        other => format!("[{other}]"),
                    };
                    if piece.is_empty() {
                        continue;
                    }
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&piece);
                }
                if !text.is_empty() {
                    cx.emit_at(0, Body::UserMessage { text, synthetic: false });
                }
            }
            "assistant" => {
                let (mut calls, mut texts) = (0, 0);
                for (i, p) in parts.iter().enumerate() {
                    let body = match str_of(p, "type").unwrap_or("") {
                        "text" => {
                            texts += 1;
                            Body::AssistantMessage { text: str_of(p, "text").unwrap_or("").to_owned(), model: None }
                        }
                        "tool_use" => {
                            calls += 1;
                            Body::ToolCall {
                                call_id: format!("c{}.{i}", cx.pos),
                                name: str_of(p, "name").unwrap_or("").to_owned(),
                                input: json_arg(p.get("input").unwrap_or(&Value::Null)),
                            }
                        }
                        other => {
                            cx.unknown(format!("assistant.part={other}"));
                            continue;
                        }
                    };
                    cx.emit_at(0, body);
                }
                if calls == 0 && texts > 0 {
                    cx.emit_at(0, Body::TurnEnd { reason: Some("stop".into()) });
                }
            }
            other => cx.unknown(format!("role={other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testkit::{Fixture, kinds};
    use serde_json::json;
    use uniflo_schema::Status;

    fn cu(fx: &Fixture) -> JsonlAdapter<Cursor> {
        JsonlAdapter::new(Cursor { roots: vec![fx.root().join("projects")] })
    }

    fn l(v: Value) -> String {
        format!("{v}\n")
    }

    const P: &str = "projects/slug/agent-transcripts/chat1/chat1.jsonl";

    #[test]
    fn full_turn_maps_every_record_kind() {
        let fx = Fixture::new();
        let a = cu(&fx);
        let mut s = l(
            json!({"role":"user","message":{"content":[{"type":"text","text":"<user_query>\nfix it\n</user_query>"}]}}),
        );
        s += &l(
            json!({"role":"assistant","message":{"content":[{"type":"text","text":"looking"},{"type":"tool_use","name":"Shell","input":{"command":"ls"}}]}}),
        );
        s += &l(json!({"role":"assistant","message":{"content":[{"type":"text","text":"done"}]}}));
        let p = fx.write(P, &s);
        let r = fx.index(&a, &p);
        assert_eq!(
            kinds(&r.events),
            vec!["user_message", "assistant_message", "tool_call", "assistant_message", "turn_end"]
        );
        assert_eq!(r.status(), Status::Idle);
        assert!(r.unknown.is_empty(), "{:?}", r.unknown);
        assert_eq!(r.preview.as_deref(), Some("fix it"));
        assert!(
            matches!(&r.events[2].body, Body::ToolCall { name, input, call_id } if name == "Shell" && input["command"] == "ls" && !call_id.is_empty())
        );
        assert!(r.events.iter().all(|e| e.ts == 0));
        let ids: std::collections::HashSet<_> = r.events.iter().map(|e| &e.id).collect();
        assert_eq!(ids.len(), r.events.len(), "position ids are unique");
    }

    #[test]
    fn mid_turn_is_work_and_unknown_reported() {
        let fx = Fixture::new();
        let a = cu(&fx);
        let mut s = l(json!({"role":"user","message":{"content":"plain"}}));
        s += &l(json!({"role":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{}}]}}));
        s += &l(json!({"role":"assistant","message":{"content":[{"type":"mystery"}]}}));
        s += &l(json!({"role":"tool"}));
        let p = fx.write(P, &s);
        let r = fx.index(&a, &p);
        assert_eq!(r.status(), Status::Work);
        assert_eq!(r.unknown, vec!["assistant.part=mystery".to_string(), "role=tool".to_string()]);
    }

    #[test]
    fn is_source_and_identity() {
        let fx = Fixture::new();
        let a = cu(&fx);
        let main = fx.write(P, "");
        let global = fx.write("projects/agent-transcripts/chat2/chat2.jsonl", "");
        let mismatch = fx.write("projects/slug/agent-transcripts/chat3/other.jsonl", "");
        let not_transcripts = fx.write("projects/slug/chat4/chat4.jsonl", "");
        let outside = fx.write("elsewhere/agent-transcripts/chat5/chat5.jsonl", "");
        for p in [&main, &global] {
            assert!(uniflo_core::Adapter::source_for(&a, p).is_some(), "{p:?}");
        }
        for p in [&mismatch, &not_transcripts, &outside] {
            assert!(uniflo_core::Adapter::source_for(&a, p).is_none(), "{p:?}");
        }
        assert_eq!(uniflo_core::Adapter::discover(&a).len(), 2);
        assert_eq!(a.decoder.identify(&main), Some(SourceId { id: "chat1".into(), parent: None }));
    }
}
