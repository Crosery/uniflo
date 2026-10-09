//! Kiro CLI sessions.
//!
//! Layout: `~/.kiro/sessions/cli/<uuid>.jsonl` plus a `<uuid>.json` sidecar rewritten in
//! place (cwd, title, created / updated times, `session_state.rts_model_state.model_info`).
//! Lines are `{kind: "Prompt" | "AssistantMessage", data: {content: [{kind: "text", data}],
//! meta: {timestamp: <unix s>}}}` with no ids (position ids); an assistant text message ends
//! the turn.

use crate::common::{Sidecar, WithSidecars, under_any};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uniflo_core::util::{home, str_of, string_of, ts};
use uniflo_core::{Adapter, Cx, HarnessInfo, LineDecoder, MetaPatch, Record, SourceId};
use uniflo_schema::Body;

pub fn adapters() -> Vec<Arc<dyn Adapter>> {
    vec![Arc::new(WithSidecars::new(Kiro { root: home().join(".kiro/sessions/cli") }))]
}

pub struct Kiro {
    root: PathBuf,
}

/// `[{kind:"text", data}]` → text; other blocks become `[kind]`.
fn blocks_text(content: &Value) -> String {
    let mut out: Vec<String> = Vec::new();
    for b in content.as_array().map(Vec::as_slice).unwrap_or_default() {
        let piece = match str_of(b, "kind").or_else(|| str_of(b, "type")) {
            Some("text") => str_of(b, "data").or_else(|| str_of(b, "text")).unwrap_or("").to_owned(),
            Some(other) => format!("[{other}]"),
            None => continue,
        };
        if !piece.is_empty() {
            out.push(piece);
        }
    }
    out.join("\n")
}

impl LineDecoder for Kiro {
    type State = ();

    fn info(&self) -> HarnessInfo {
        HarnessInfo { id: "kiro", name: "Kiro" }
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    fn is_source(&self, p: &Path) -> bool {
        p.extension().is_some_and(|e| e == "jsonl") && p.parent() == Some(self.root.as_path())
    }

    fn identify(&self, p: &Path) -> Option<SourceId> {
        Some(SourceId { id: p.file_stem()?.to_str()?.to_owned(), parent: None })
    }

    /// `<uuid>.jsonl` and its `<uuid>.json` sidecar.
    fn cleanup_targets(&self, src: &Path) -> Option<Vec<PathBuf>> {
        uniflo_core::cleanup::targets::with_siblings(src, src.file_stem()?.to_str()?)
    }

    fn max_depth(&self) -> usize {
        0
    }

    fn decode(&self, v: &Value, cx: &mut Cx<'_, ()>) {
        let data = v.get("data").unwrap_or(&Value::Null);
        let t = data.pointer("/meta/timestamp").and_then(ts).unwrap_or(0);
        let text = blocks_text(data.get("content").unwrap_or(&Value::Null));
        match str_of(v, "kind").unwrap_or("") {
            "Prompt" => {
                if !text.is_empty() {
                    cx.emit_at(t, Body::UserMessage { text, synthetic: false });
                }
            }
            "AssistantMessage" => {
                if !text.is_empty() {
                    cx.emit_at(t, Body::AssistantMessage { text, model: None });
                    cx.emit_at(t, Body::TurnEnd { reason: Some("stop".into()) });
                }
            }
            kind => cx.unknown(format!("kind={kind}")),
        }
    }
}

impl Sidecar for Kiro {
    fn transcript_of(&self, p: &Path) -> Option<PathBuf> {
        let t = p.with_extension("jsonl");
        (p.extension().is_some_and(|e| e == "json") && under_any(p, std::slice::from_ref(&self.root)))
            .then_some(t)
            .filter(|t| self.is_source(t))
    }

    fn sidecars(&self, src: &Path) -> Vec<PathBuf> {
        vec![src.with_extension("json")]
    }

    fn sidecar(&self, src: &Path, _: &mut ()) -> Vec<Record> {
        let Some(s) =
            std::fs::read(src.with_extension("json")).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        else {
            return Vec::new();
        };
        let info = s.pointer("/session_state/rts_model_state/model_info").unwrap_or(&Value::Null);
        vec![Record::Meta(MetaPatch {
            title: string_of(&s, "title").map(|t| (2, t)),
            cwd: string_of(&s, "cwd"),
            model: string_of(info, "model_id").or_else(|| string_of(info, "model_name")),
            started_at: s.get("created_at").and_then(ts),
            updated_at: s.get("updated_at").and_then(ts),
            ..Default::default()
        })]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testkit::{Fixture, kinds};
    use serde_json::json;
    use uniflo_core::util::ts_str;
    use uniflo_schema::Status;

    const ID: &str = "44444444-aaaa-bbbb-cccc-000000000004";

    fn line(kind: &str, text: &str, t: i64) -> String {
        format!(
            "{}\n",
            json!({"kind": kind, "data": {"content": [{"kind":"text","data": text}], "meta": {"timestamp": t}}})
        )
    }

    #[test]
    fn prompt_and_reply_with_sidecar_metadata() {
        let fx = Fixture::new();
        let a = WithSidecars::new(Kiro { root: fx.root().join("sessions/cli") });
        let mut s = line("Prompt", "split the scanner", 1790000000);
        s += &line("AssistantMessage", "Split it.", 1790000005);
        s += &format!("{}\n", json!({"kind":"ToolLog","data":{"note":"x"}}));
        let p = fx.write(&format!("sessions/cli/{ID}.jsonl"), &s);
        fx.write(
            &format!("sessions/cli/{ID}.json"),
            &json!({"session_id": ID, "cwd": "/w/app", "title": "Split scanner", "created_at": "2026-09-01T08:00:00Z",
                "updated_at": "2026-09-01T08:10:00Z", "session_state": {"version": "v1", "rts_model_state":
                {"model_info": {"model_name": "Claude Sonnet", "model_id": "claude-sonnet-4.5", "context_window_tokens": 200000}}}})
            .to_string(),
        );
        fx.write(&format!("sessions/cli/{ID}.history"), "x");
        let r = fx.index(&a, &p);
        assert_eq!(r.id, ID);
        assert_eq!(kinds(&r.events), ["user_message", "assistant_message", "turn_end"]);
        assert!(
            matches!(&r.events[0].body, Body::UserMessage { text, synthetic: false } if text == "split the scanner")
        );
        assert_eq!(r.events[0].ts, 1790000000000);
        assert_eq!(r.status(), Status::Idle);
        assert_eq!(r.meta.title.as_deref(), Some("Split scanner"));
        assert_eq!(r.meta.cwd.as_deref(), Some("/w/app"));
        assert_eq!(r.meta.model.as_deref(), Some("claude-sonnet-4.5"));
        assert_eq!(r.meta.started_at, ts_str("2026-09-01T08:00:00Z"));
        assert_eq!(r.unknown, ["kind=ToolLog"]);
        assert_eq!(uniflo_core::Adapter::discover(&a), vec![p.clone()]);
        assert_eq!(uniflo_core::Adapter::source_for(&a, &p.with_extension("json")), Some(p.clone()));
        assert_eq!(uniflo_core::Adapter::source_for(&a, &p.with_extension("history")), None);
    }

    #[test]
    fn prompt_only_is_working_and_sidecar_change_is_a_change() {
        let fx = Fixture::new();
        let a = WithSidecars::new(Kiro { root: fx.root().join("sessions/cli") });
        let p = fx.write(&format!("sessions/cli/{ID}.jsonl"), &line("Prompt", "go", 1790000000));
        let out = uniflo_core::Adapter::read(&a, &p, None).unwrap();
        let g = crate::common::testkit::group(out.batch);
        assert_eq!(g[ID].status(), Status::Work);
        assert!(g[ID].meta.title.is_none());
        fx.write(&format!("sessions/cli/{ID}.json"), &json!({"title": "Go"}).to_string());
        assert!(uniflo_core::Adapter::changed(&a, &p, &out.cursor));
        let out2 = uniflo_core::Adapter::read(&a, &p, Some(&out.cursor)).unwrap();
        let g2 = crate::common::testkit::group(out2.batch);
        assert_eq!(g2[ID].meta.title.as_deref(), Some("Go"));
    }
}
