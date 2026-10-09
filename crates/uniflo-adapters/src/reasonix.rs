//! Reasonix transcripts.
//!
//! Source: `~/.reasonix/projects/<slug>/sessions/<ts>-<model>.events.jsonl`, an append-only log of
//! `append` / `replace` rows holding message batches (the plain `.jsonl` next to it is a rewritten
//! snapshot and is deliberately not a source). Event ids derive from the message index so a
//! `replace` row upserts the messages it rewrites.

use crate::common::under_any;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uniflo_core::util::{home, json_arg, str_of, string_of, ts};
use uniflo_core::{Adapter, Cx, HarnessInfo, JsonlAdapter, LineDecoder, SourceId};
use uniflo_schema::Body;

pub struct Reasonix {
    roots: Vec<PathBuf>,
}

pub fn adapters() -> Vec<Arc<dyn Adapter>> {
    vec![Arc::new(JsonlAdapter::new(Reasonix { roots: vec![home().join(".reasonix/projects")] }))]
}

const SUFFIX: &str = ".events.jsonl";

/// `20260801-022448.055723000-deepseek-v4-flash` → `deepseek-v4-flash`.
fn model_from_stem(stem: &str) -> Option<&str> {
    let m = stem.splitn(3, '-').nth(2)?;
    (!m.is_empty()).then_some(m)
}

impl LineDecoder for Reasonix {
    /// Whether the sidecar metadata was already applied.
    type State = bool;

    fn info(&self) -> HarnessInfo {
        HarnessInfo { id: "reasonix", name: "Reasonix" }
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }

    fn is_source(&self, p: &Path) -> bool {
        p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.len() > SUFFIX.len() && n.ends_with(SUFFIX))
            && p.parent().and_then(|d| d.file_name()).is_some_and(|d| d == "sessions")
            && under_any(p, &self.roots)
    }

    /// <base>.events.jsonl and every <base>.* sibling (snapshot, meta, checkpoints).
    fn cleanup_targets(&self, src: &Path) -> Option<Vec<PathBuf>> {
        uniflo_core::cleanup::targets::with_siblings(src, src.file_name()?.to_str()?.strip_suffix(SUFFIX)?)
    }

    fn identify(&self, p: &Path) -> Option<SourceId> {
        let id = p.file_name()?.to_str()?.strip_suffix(SUFFIX)?.to_owned();
        Some(SourceId { id, parent: None })
    }

    fn decode(&self, v: &Value, cx: &mut Cx<'_, bool>) {
        if !*cx.state {
            *cx.state = true;
            sidecar(cx);
        }
        let kind = str_of(v, "type").unwrap_or("");
        let base = match kind {
            "append" => v.get("message_index").and_then(Value::as_u64).unwrap_or(0),
            "replace" => 0,
            _ => {
                cx.unknown(format!("type={kind}"));
                return;
            }
        };
        let row_ts = v.get("created_at").and_then(ts).unwrap_or(0);
        for (i, m) in
            v.get("messages").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default().iter().enumerate()
        {
            message(m, base + i as u64, row_ts, cx);
        }
    }
}

/// Title/model from `<stem>.jsonl.meta`, falling back to the model in the file name.
fn sidecar(cx: &mut Cx<'_, bool>) {
    let stem = cx.src.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(SUFFIX)).unwrap_or("");
    let fallback = model_from_stem(stem).map(str::to_owned);
    let meta = std::fs::read(cx.src.with_file_name(format!("{stem}.jsonl.meta")))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    let model = meta.as_ref().and_then(|m| string_of(m, "model")).or(fallback);
    if let Some(model) = model {
        cx.meta().model = Some(model);
    }
    if let Some(title) = meta.as_ref().and_then(|m| string_of(m, "topic_title")) {
        cx.meta().title = Some((2, title));
    }
}

fn message(m: &Value, idx: u64, row_ts: i64, cx: &mut Cx<'_, bool>) {
    let id = format!("m{idx}");
    let t = m.get("createdAt").and_then(ts).unwrap_or(row_ts);
    let content = str_of(m, "content").unwrap_or("");
    match str_of(m, "role").unwrap_or("") {
        "system" => {}
        "user" => {
            if !content.is_empty() {
                cx.emit(id, t, Body::UserMessage { text: content.to_owned(), synthetic: false });
            }
        }
        "tool" => {
            let body = Body::ToolResult {
                call_id: str_of(m, "tool_call_id").unwrap_or("").to_owned(),
                name: string_of(m, "name"),
                output: content.to_owned(),
                is_error: false,
            };
            cx.emit(id, t, body);
        }
        "assistant" => {
            if let Some(r) = string_of(m, "reasoning_content") {
                cx.emit(format!("{id}:r"), t, Body::Reasoning { text: r });
            }
            if !content.is_empty() {
                cx.emit(format!("{id}:t"), t, Body::AssistantMessage { text: content.to_owned(), model: None });
            }
            let calls = m.get("tool_calls").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
            for (j, c) in calls.iter().enumerate() {
                let body = Body::ToolCall {
                    call_id: str_of(c, "id").unwrap_or("").to_owned(),
                    name: str_of(c, "name").unwrap_or("").to_owned(),
                    input: json_arg(c.get("arguments").unwrap_or(&Value::Null)),
                };
                cx.emit(format!("{id}:c{j}"), t, body);
            }
            if calls.is_empty() && !content.is_empty() {
                cx.emit(format!("{id}:end"), t, Body::TurnEnd { reason: Some("stop".into()) });
            }
        }
        other => cx.unknown(format!("role={other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testkit::{Fixture, kinds};
    use serde_json::json;
    use uniflo_schema::Status;

    fn rx(fx: &Fixture) -> JsonlAdapter<Reasonix> {
        JsonlAdapter::new(Reasonix { roots: vec![fx.root().join("projects")] })
    }

    fn row(ty: &str, idx: u64, msgs: Value) -> String {
        format!("{}\n", json!({"type":ty,"message_index":idx,"created_at":"2026-10-02T12:00:00.5Z","messages":msgs}))
    }

    const SRC: &str = "projects/-w/sessions/20261002-120000.000000000-model-x.events.jsonl";

    #[test]
    fn full_turn_maps_every_record_kind() {
        let fx = Fixture::new();
        let a = rx(&fx);
        fx.write(
            "projects/-w/sessions/20261002-120000.000000000-model-x.jsonl.meta",
            &json!({"model":"vendor/model-x","topic_title":"Topic"}).to_string(),
        );
        let mut s = row(
            "replace",
            0,
            json!([{"role":"system","content":"sys"},{"role":"user","content":"hi","createdAt":1790942400000i64}]),
        );
        s += &row(
            "append",
            2,
            json!([{"role":"assistant","content":"","reasoning_content":"hmm","tool_calls":[{"id":"c1","name":"shell","arguments":"{\"cmd\":\"ls\"}"}]}]),
        );
        s += &row("append", 3, json!([{"role":"tool","content":"out","tool_call_id":"c1","name":"shell"}]));
        s += &row("append", 4, json!([{"role":"assistant","content":"done"}]));
        let p = fx.write(SRC, &s);
        let r = fx.index(&a, &p);
        assert_eq!(
            kinds(&r.events),
            vec!["user_message", "reasoning", "tool_call", "tool_result", "assistant_message", "turn_end"]
        );
        let ids: Vec<_> = r.events.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["m1", "m2:r", "m2:c0", "m3", "m4:t", "m4:end"]);
        assert_eq!(r.status(), Status::Idle);
        assert!(r.unknown.is_empty(), "{:?}", r.unknown);
        assert_eq!(r.id, "20261002-120000.000000000-model-x");
        assert_eq!(r.meta.model.as_deref(), Some("vendor/model-x"));
        assert_eq!(r.meta.title.as_deref(), Some("Topic"));
        assert!(
            matches!(&r.events[2].body, Body::ToolCall { call_id, name, input } if call_id == "c1" && name == "shell" && input["cmd"] == "ls")
        );
        assert!(
            matches!(&r.events[3].body, Body::ToolResult { call_id, name: Some(n), output, .. } if call_id == "c1" && n == "shell" && output == "out")
        );
        assert_eq!(r.events[0].ts, 1790942400000);
        assert_eq!(r.events[1].ts, 1790942400500);
    }

    #[test]
    fn mid_turn_is_work_model_falls_back_to_file_name_and_unknown_reported() {
        let fx = Fixture::new();
        let a = rx(&fx);
        let mut s = row("append", 0, json!([{"role":"user","content":"go"},{"role":"mystery"}]));
        s += &row("append", 2, json!([{"role":"assistant","tool_calls":[{"id":"c","name":"t","arguments":"{}"}]}]));
        s += &row("snapshot", 0, json!([]));
        let p = fx.write(SRC, &s);
        let r = fx.index(&a, &p);
        assert_eq!(r.status(), Status::Work);
        assert_eq!(r.meta.model.as_deref(), Some("model-x"));
        assert_eq!(r.unknown, vec!["role=mystery".to_string(), "type=snapshot".to_string()]);
    }

    #[test]
    fn replace_rows_upsert_by_message_index() {
        let fx = Fixture::new();
        let a = rx(&fx);
        let mut s = row("append", 0, json!([{"role":"user","content":"old"}]));
        s += &row("replace", 0, json!([{"role":"user","content":"new"}]));
        let p = fx.write(SRC, &s);
        let r = fx.index(&a, &p);
        assert_eq!(r.events.len(), 2);
        assert_eq!(r.events[0].id, r.events[1].id);
    }

    #[test]
    fn is_source_and_identity() {
        let fx = Fixture::new();
        let a = rx(&fx);
        let ev = fx.write(SRC, "");
        let snapshot = fx.write("projects/-w/sessions/20261002-120000.000000000-model-x.jsonl", "");
        let wrong_dir = fx.write("projects/-w/sessions/x.ckpt/a.events.jsonl", "");
        let outside = fx.write("elsewhere/sessions/a.events.jsonl", "");
        assert!(uniflo_core::Adapter::source_for(&a, &ev).is_some());
        for p in [&snapshot, &wrong_dir, &outside] {
            assert!(uniflo_core::Adapter::source_for(&a, p).is_none(), "{p:?}");
        }
        assert_eq!(uniflo_core::Adapter::discover(&a), vec![ev.clone()]);
        assert_eq!(
            a.decoder.identify(&ev),
            Some(SourceId { id: "20261002-120000.000000000-model-x".into(), parent: None })
        );
    }
}
