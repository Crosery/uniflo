//! Google Antigravity (IDE + CLI) step transcripts:
//! `~/.gemini/antigravity{,-cli}/brain/<conversation>/.system_generated/logs/transcript.jsonl`.
//!
//! Each line is a step `{step_index, type, source, status, created_at, content…}`.
//! `PLANNER_RESPONSE` steps carry the model's text/thinking/tool calls (without call ids);
//! the following tool steps (`RUN_COMMAND`, `VIEW_FILE`, …) are their results, matched in order.
//! A step may be re-written as its status moves `RUNNING` → `DONE`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use uniflo_core::util::{home, json_arg, str_of, text_of, ts};
use uniflo_core::{Cx, HarnessInfo, JsonlAdapter, LineDecoder, SourceId};
use uniflo_schema::Body;

pub struct Antigravity {
    pub roots: Vec<PathBuf>,
}

pub fn adapters() -> Vec<std::sync::Arc<dyn uniflo_core::Adapter>> {
    vec![std::sync::Arc::new(antigravity())]
}

pub fn antigravity() -> JsonlAdapter<Antigravity> {
    let h = home();
    JsonlAdapter::new(Antigravity {
        roots: vec![h.join(".gemini/antigravity/brain"), h.join(".gemini/antigravity-cli/brain")],
    })
}

#[derive(Default, Serialize, Deserialize)]
pub struct Links {
    /// Tool calls waiting for their result step: (call_id, name).
    pending: VecDeque<(String, String)>,
    /// Highest planner step whose calls were queued (re-written steps must not re-queue).
    planner_max: i64,
    /// Recently assigned result steps → call id, so re-written results keep their link.
    assigned: VecDeque<(i64, String)>,
}

const SYSTEM_STEPS: &[&str] =
    &["SYSTEM_MESSAGE", "CHECKPOINT", "CONVERSATION_HISTORY", "DIRECTORY_RULES", "EPHEMERAL_MESSAGE"];

impl LineDecoder for Antigravity {
    type State = Links;

    fn info(&self) -> HarnessInfo {
        HarnessInfo { id: "antigravity", name: "Antigravity" }
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }

    fn is_source(&self, p: &Path) -> bool {
        p.file_name().is_some_and(|n| n == "transcript.jsonl")
            && p.parent().and_then(|d| d.file_name()).is_some_and(|d| d == "logs")
            && crate::common::under_any(p, &self.roots)
    }

    fn identify(&self, p: &Path) -> Option<SourceId> {
        // brain/<id>/.system_generated/logs/transcript.jsonl
        let id = p.parent()?.parent()?.parent()?.file_name()?.to_str()?.to_owned();
        Some(SourceId { id, parent: None })
    }

    fn max_depth(&self) -> usize {
        4
    }

    fn decode(&self, v: &Value, cx: &mut Cx<'_, Links>) {
        let step = v.get("step_index").and_then(Value::as_i64).unwrap_or(-1);
        let t = v.get("created_at").and_then(ts).unwrap_or(0);
        let ty = str_of(v, "type").unwrap_or("");
        let status = str_of(v, "status").unwrap_or("");
        let running = status == "RUNNING";
        let content = text_of(v.get("content").unwrap_or(&Value::Null));
        let sid = format!("s{step}");
        match ty {
            "USER_INPUT" => {
                cx.emit(sid, t, Body::UserMessage { text: content, synthetic: false });
            }
            "PLANNER_RESPONSE" => {
                if let Some(th) = v.get("thinking").map(text_of).filter(|s| !s.is_empty()) {
                    cx.emit(format!("{sid}:r"), t, Body::Reasoning { text: th }).partial = running;
                }
                if !content.is_empty() {
                    cx.emit(format!("{sid}:a"), t, Body::AssistantMessage { text: content, model: None }).partial =
                        running;
                }
                let calls = v.get("tool_calls").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
                let fresh = step > cx.state.planner_max;
                for (i, c) in calls.iter().enumerate() {
                    let call_id = format!("{sid}:{i}");
                    let name = str_of(c, "name").unwrap_or("").to_owned();
                    if fresh {
                        cx.state.pending.push_back((call_id.clone(), name.clone()));
                    }
                    let input = json_arg(c.get("args").unwrap_or(&Value::Null));
                    cx.emit(format!("{sid}:c{i}"), t, Body::ToolCall { call_id, name, input }).partial = running;
                }
                if fresh {
                    cx.state.planner_max = step;
                }
                if calls.is_empty() && status == "DONE" {
                    cx.emit(format!("{sid}:end"), t, Body::TurnEnd { reason: Some("done".into()) });
                }
            }
            "GENERIC" => {
                cx.emit(sid, t, Body::AssistantMessage { text: content, model: None }).partial = running;
            }
            "ERROR_MESSAGE" => {
                let text = v.get("error").map(text_of).filter(|s| !s.is_empty()).unwrap_or(content);
                cx.emit(sid, t, Body::System { subtype: "error".into(), text });
            }
            ty if SYSTEM_STEPS.contains(&ty) => {
                cx.emit(sid, t, Body::System { subtype: ty.to_ascii_lowercase(), text: content });
            }
            "" => cx.unknown("type="),
            // Any other MODEL step is a tool execution result.
            ty => {
                let call_id = link(cx.state, step);
                let failed = status == "ERROR" || v.get("exit_code").and_then(Value::as_i64).is_some_and(|c| c != 0);
                let body = Body::ToolResult {
                    call_id,
                    name: Some(ty.to_ascii_lowercase()),
                    output: content,
                    is_error: failed,
                };
                cx.emit(sid, t, body).partial = running;
            }
        }
    }
}

fn link(st: &mut Links, step: i64) -> String {
    if let Some((_, id)) = st.assigned.iter().find(|(s, _)| *s == step) {
        return id.clone();
    }
    let id = st.pending.pop_front().map_or_else(|| format!("s{step}"), |(id, _)| id);
    st.assigned.push_back((step, id.clone()));
    if st.assigned.len() > 64 {
        st.assigned.pop_front();
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testkit::{Fixture, kinds};
    use serde_json::json;
    use uniflo_core::{Adapter, HistoryQuery};
    use uniflo_schema::Status;

    fn l(v: Value) -> String {
        format!("{v}\n")
    }

    #[test]
    fn steps_link_calls_to_results_in_order() {
        let fx = Fixture::new();
        let a = JsonlAdapter::new(Antigravity { roots: vec![fx.root().to_path_buf()] });
        let st = |i: i64, ty: &str, status: &str, extra: Value| {
            let mut o =
                json!({"step_index":i,"source":"MODEL","type":ty,"status":status,"created_at":"2026-08-24T14:18:26Z"});
            o.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            l(o)
        };
        let mut s = st(0, "USER_INPUT", "DONE", json!({"source":"USER_EXPLICIT","content":"check disk"}));
        s += &st(
            1,
            "PLANNER_RESPONSE",
            "DONE",
            json!({"thinking":"use df","tool_calls":[{"name":"run_command","args":{"CommandLine":"df -h"}},{"name":"view_file","args":{"path":"/x"}}]}),
        );
        s += &st(2, "RUN_COMMAND", "RUNNING", json!({"content":""}));
        s += &st(2, "RUN_COMMAND", "DONE", json!({"content":"ok","exit_code":0}));
        s += &st(3, "VIEW_FILE", "ERROR", json!({"content":"missing"}));
        s += &st(4, "PLANNER_RESPONSE", "DONE", json!({"content":"Disk is fine"}));
        let p = fx.write("brain/conv-1/.system_generated/logs/transcript.jsonl", &s);
        let r = fx.index(&a, &p);
        assert_eq!(r.id, "conv-1");
        assert_eq!(r.status(), Status::Idle);
        let h = a.history(&p, "conv-1", &HistoryQuery { before: None, limit: 100 }).unwrap();
        assert_eq!(
            kinds(&h),
            vec![
                "user_message",
                "reasoning",
                "tool_call",
                "tool_call",
                "tool_result",
                "tool_result",
                "assistant_message",
                "turn_end"
            ]
        );
        assert!(
            matches!(&h[4].body, Body::ToolResult { call_id, is_error: false, output, .. } if call_id == "s1:0" && output == "ok")
        );
        assert!(!h[4].partial, "DONE rewrite replaced RUNNING");
        assert!(matches!(&h[5].body, Body::ToolResult { call_id, is_error: true, .. } if call_id == "s1:1"));
        assert!(
            !a.decoder
                .is_source(&fx.root().join("brain/conv-1/.system_generated/logs/chunks/transcript/00000000.jsonl"))
        );
    }
}
