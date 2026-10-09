//! CodeBuddy Code (Tencent) transcripts: the WorkBuddy kernel, decoded by
//! [`crate::workbuddy::WorkBuddy`].
//!
//! Layout: `$CODEBUDDY_CONFIG_DIR` (when it holds `projects/`) or `~/.codebuddy`, then
//! `projects/<cwd-slug>/<session>.jsonl`, sub-agents under `<session>/subagents/agent-*.jsonl`.
//! Usage comes from `providerData.rawUsage` (OpenAI completions shape, cache inside the
//! prompt count) when the record has no `providerData.usage`.

use crate::workbuddy::WorkBuddy;
use std::path::PathBuf;
use std::sync::Arc;
use uniflo_core::util::home;
use uniflo_core::{Adapter, HarnessInfo, JsonlAdapter};

pub fn adapters() -> Vec<Arc<dyn Adapter>> {
    let base = std::env::var_os("CODEBUDDY_CONFIG_DIR")
        .map(PathBuf::from)
        .filter(|p| p.join("projects").is_dir())
        .unwrap_or_else(|| home().join(".codebuddy"));
    vec![Arc::new(JsonlAdapter::new(codebuddy(&base)))]
}

fn codebuddy(base: &std::path::Path) -> WorkBuddy {
    WorkBuddy::new(HarnessInfo { id: "codebuddy", name: "CodeBuddy" }, base)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testkit::{Fixture, kinds};
    use serde_json::{Value, json};
    use uniflo_core::{LineDecoder, SourceId};
    use uniflo_schema::{Body, Status};

    fn rec(id: &str, extra: Value) -> String {
        let mut o = json!({"id":id,"timestamp":1790942400000i64,"cwd":"/w/cb","sessionId":"cb1"});
        o.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        format!("{o}\n")
    }

    #[test]
    fn turn_titles_raw_usage_and_subagent() {
        let fx = Fixture::new();
        let a = JsonlAdapter::new(codebuddy(fx.root()));
        let raw = json!({"prompt_tokens":1000,"completion_tokens":60,"total_tokens":1060,
            "prompt_tokens_details":{"cached_tokens":700},"completion_tokens_details":{"reasoning_tokens":25}});
        let mut s =
            rec("u1", json!({"type":"message","role":"user","content":[{"type":"input_text","text":"refactor"}]}));
        s += &rec("r1", json!({"type":"reasoning","rawContent":[{"type":"reasoning_text","text":"plan"}]}));
        s += &rec(
            "f1",
            json!({"type":"function_call","callId":"c1","name":"Read","arguments":"{\"path\":\"a.rs\"}",
                "providerData":{"requestModelName":"hunyuan-t1","rawUsage":raw}}),
        );
        s += &rec(
            "f1r",
            json!({"type":"function_call_result","callId":"c1","name":"Read","status":"completed","output":{"type":"text","text":"fn a"}}),
        );
        s += &rec("t1", json!({"type":"topic","topic":"Refactoring"}));
        s += &rec("t2", json!({"type":"ai-title","aiTitle":"Refactor a.rs"}));
        s += &rec("t3", json!({"type":"custom-title","customTitle":"My refactor"}));
        s += &rec("t4", json!({"type":"ai-title","aiTitle":"(No content)"}));
        s += &rec(
            "a1",
            json!({"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"done"}],
                "providerData":{"requestModelName":"hunyuan-t1"}}),
        );
        s += &rec("m1", json!({"type":"turn-metrics","durationMs":10}));
        let p = fx.write("projects/-w-cb/cb1.jsonl", &s);
        let r = fx.index(&a, &p);
        assert_eq!(
            kinds(&r.events),
            ["user_message", "reasoning", "tool_call", "usage", "tool_result", "assistant_message", "turn_end"]
        );
        assert_eq!(r.meta.title.as_deref(), Some("My refactor"));
        assert!(matches!(&r.events[3].body, Body::Usage(u)
            if (u.input, u.cache_read, u.output, u.reasoning) == (300, 700, 60, 25) && u.model.as_deref() == Some("hunyuan-t1")));
        assert_eq!(r.status(), Status::Idle);
        assert_eq!(r.meta.cwd.as_deref(), Some("/w/cb"));
        assert_eq!(r.meta.model.as_deref(), Some("hunyuan-t1"));
        assert!(r.unknown.is_empty(), "{:?}", r.unknown);

        let sub = fx.write(
            "projects/-w-cb/cb1/subagents/agent-x1.jsonl",
            &rec("s1", json!({"type":"message","role":"user","content":"sub"})),
        );
        assert_eq!(a.decoder.identify(&sub), Some(SourceId { id: "agent-x1".into(), parent: Some("cb1".into()) }));
        assert_eq!(fx.index(&a, &sub).meta.parent.as_deref(), Some("cb1"));
        assert_eq!(uniflo_core::Adapter::info(&a).id, "codebuddy");
    }

    #[test]
    fn placeholder_ai_title_falls_back_to_topic() {
        let fx = Fixture::new();
        let a = JsonlAdapter::new(codebuddy(fx.root()));
        let mut s = rec("u1", json!({"type":"message","role":"user","content":"hi"}));
        s += &rec("t1", json!({"type":"topic","topic":"Greeting"}));
        s += &rec("t2", json!({"type":"ai-title","aiTitle":"/compact"}));
        let p = fx.write("projects/-w-cb/cb2.jsonl", &s);
        assert_eq!(fx.index(&a, &p).meta.title.as_deref(), Some("Greeting"));
    }
}
