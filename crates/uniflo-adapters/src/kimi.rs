//! Kimi Code sessions.
//!
//! Layout: `$KIMI_CODE_HOME` (when it holds `sessions/`) or `~/.kimi-code`, then
//! `sessions/wd_<name>_<hash>/{session_,ses_}<uuid>/agents/main/wire.jsonl` (the session id
//! is that directory name; other agents' wires are internal). `wire.jsonl` is the agent's
//! append-only operation log: `turn.prompt` / `turn.steer` carry the input with an `origin`
//! (anything but the user's own input is synthetic); `context.append_message` mirrors it
//! (the echo is skipped) and carries migrated history; `context.append_loop_event` streams
//! one step: `step.begin`, `content.part` (`text` / `think`), `tool.call`, `tool.result`,
//! `step.end`. A step that ends without tool calls (or whose `finishReason` is not
//! `tool_use`) ends the turn; older builds also write `turn.ended`, which shares that turn
//! end's id. History pages start at a `turn.prompt`. `usage.record` is one model call
//! (`inputOther` excludes the cache). Siblings rewritten in place: `state.json` (title, fork
//! parent; "New Session" is a placeholder) and `<home>/session_index.jsonl` (cwd).

use crate::common::{FileSig, Sidecar, WithSidecars, file_sig, under_any};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use uniflo_core::util::{home, json_arg, str_of, string_of, text_of, ts};
use uniflo_core::{Adapter, Cx, HarnessInfo, LineDecoder, MetaPatch, Record, SourceId};
use uniflo_schema::{Body, Usage};

pub fn adapters() -> Vec<Arc<dyn Adapter>> {
    let home = std::env::var_os("KIMI_CODE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.join("sessions").is_dir())
        .unwrap_or_else(|| home().join(".kimi-code"));
    vec![Arc::new(WithSidecars::new(Kimi::new(home)))]
}

pub struct Kimi {
    root: PathBuf,
    index: PathBuf,
    /// Last `state.json` that parsed, per path: it is rewritten by truncate-then-write.
    states: Mutex<HashMap<PathBuf, SessionState>>,
    /// `session_index.jsonl` (sessionId → workDir) keyed by its signature.
    cwds: Mutex<(FileSig, Arc<HashMap<String, String>>)>,
}

impl Kimi {
    fn new(home: PathBuf) -> Self {
        Kimi {
            root: home.join("sessions"),
            index: home.join("session_index.jsonl"),
            states: Mutex::new(HashMap::new()),
            cwds: Mutex::new((Vec::new(), Arc::default())),
        }
    }
}

#[derive(Debug, Default, Clone)]
struct SessionState {
    title: Option<(u8, String)>,
    forked_from: Option<String>,
    work_dir: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// Visible inputs whose `context.append_message` echo is still to come.
    echo: u32,
    /// Tool calls in the open step.
    calls: u32,
    /// The file has step records; without them a tool-free assistant message ends the turn.
    steps: bool,
    /// Offset of the input that opened the current turn: every end of that turn (a tool-free
    /// step, then an explicit `turn.ended` in builds that write one) shares one id.
    turn: Option<u64>,
    model: Option<String>,
}

/// Operations that carry nothing for the unified stream.
const IGNORED: &[&str] = &[
    "context.clear",
    "context.undo",
    "context.update_token_count",
    "context_size.measured",
    "cron.add",
    "cron.cursor",
    "cron.delete",
    "forked",
    "full_compaction.begin",
    "full_compaction.cancel",
    "full_compaction.complete",
    "goal.clear",
    "goal.create",
    "goal.update",
    "llm.tools_snapshot",
    "mcp.tools_discovered",
    "micro_compaction.apply",
    "permission.record_approval_result",
    "permission.rules.add",
    "permission.set_mode",
    "plan_mode.cancel",
    "plan_mode.enter",
    "plan_mode.exit",
    "skill.activate",
    "swarm_mode.enter",
    "swarm_mode.exit",
    "task.started",
    "task.terminated",
    "tools.register_user_tool",
    "tools.reset_active_tools",
    "tools.set_active_tools",
    "tools.unregister_user_tool",
    "tools.update_store",
    "turn.started",
];

/// Origins that are never the user's own input.
const SYNTHETIC_ORIGINS: &[&str] = &[
    "background_task",
    "compaction_summary",
    "cron_job",
    "cron_missed",
    "hook_result",
    "injection",
    "retry",
    "system_trigger",
];

/// Whether an input `origin` is the user's: missing (older records) counts as theirs; slash
/// commands only when the user typed them; shell commands only in input phase.
fn by_user(origin: Option<&Value>, cx: &mut Cx<'_, State>) -> bool {
    let Some(o) = origin.filter(|o| !o.is_null()) else { return true };
    match str_of(o, "kind").unwrap_or("") {
        "user" => true,
        "skill_activation" | "plugin_command" => str_of(o, "trigger") == Some("user-slash"),
        "shell_command" => str_of(o, "phase") == Some("input"),
        k if SYNTHETIC_ORIGINS.contains(&k) => false,
        k => {
            cx.unknown(format!("origin.kind={k}"));
            false
        }
    }
}

impl LineDecoder for Kimi {
    type State = State;

    fn info(&self) -> HarnessInfo {
        HarnessInfo { id: "kimi", name: "Kimi Code" }
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    fn is_source(&self, p: &Path) -> bool {
        let name = |n: usize| p.ancestors().nth(n).and_then(|a| a.file_name()).and_then(|s| s.to_str()).unwrap_or("");
        name(0) == "wire.jsonl"
            && name(1) == "main"
            && name(2) == "agents"
            && (name(3).starts_with("session_") || name(3).starts_with("ses_"))
            && name(4).starts_with("wd_")
            && p.ancestors().nth(5) == Some(self.root.as_path())
    }

    fn identify(&self, p: &Path) -> Option<SourceId> {
        Some(SourceId { id: p.ancestors().nth(3)?.file_name()?.to_str()?.to_owned(), parent: None })
    }

    fn max_depth(&self) -> usize {
        4
    }

    fn page_start(&self, _prev: &Value, record: &Value) -> bool {
        str_of(record, "type") == Some("turn.prompt")
    }

    fn decode(&self, v: &Value, cx: &mut Cx<'_, State>) {
        let t = v.get("time").and_then(ts).unwrap_or(0);
        match str_of(v, "type").unwrap_or("") {
            "metadata" => cx.meta().started_at = v.get("created_at").and_then(ts),
            "config.update" => {
                if let Some(cwd) = string_of(v, "cwd") {
                    cx.meta().cwd = Some(cwd);
                }
                if let Some(m) = string_of(v, "modelAlias") {
                    model(cx, m);
                }
            }
            "llm.request" => {
                if let Some(m) = string_of(v, "model") {
                    model(cx, m);
                }
            }
            ty @ ("turn.prompt" | "turn.steer") => {
                cx.state.turn = Some(cx.pos);
                if ty == "turn.prompt" {
                    cx.state.echo = 0;
                }
                let user = by_user(v.get("origin"), cx);
                let text = text_of(v.get("input").unwrap_or(&Value::Null));
                if !text.trim().is_empty() {
                    cx.emit_at(t, Body::UserMessage { text, synthetic: !user });
                    if user {
                        cx.state.echo += 1;
                    }
                }
            }
            "context.append_message" => match v.get("message") {
                Some(m) => message(m, t, cx),
                None => cx.unknown("context.append_message=no message"),
            },
            // Desktop builds wrap the message once more; user / notify rows duplicate inputs.
            "agent.message.appended" => match v.pointer("/message/message") {
                Some(m) if matches!(str_of(m, "role"), Some("assistant" | "tool")) => message(m, t, cx),
                Some(m) if matches!(str_of(m, "role"), Some("user" | "notify")) => {}
                _ => cx.unknown("agent.message.appended=shape"),
            },
            "context.append_loop_event" => loop_event(v.get("event").unwrap_or(&Value::Null), t, cx),
            "context.apply_compaction" => {
                let text = v.get("summary").or_else(|| v.get("contextSummary")).map_or_else(String::new, |s| {
                    s.as_str().map_or_else(|| text_of(s.get("content").unwrap_or(s)), str::to_owned)
                });
                cx.emit_at(t, Body::System { subtype: "compact".into(), text });
            }
            "usage.record" => {
                let u = v.get("usage").unwrap_or(&Value::Null);
                let n = |a: &str, b: &str| u.get(a).or_else(|| u.get(b)).and_then(Value::as_u64).unwrap_or(0);
                let usage = Usage {
                    input: n("inputOther", "input"),
                    output: n("output", "output"),
                    cache_read: n("inputCacheRead", "cacheRead"),
                    cache_write: n("inputCacheCreation", "cacheWrite"),
                    reasoning: 0,
                    model: string_of(v, "model").or_else(|| cx.state.model.clone()),
                    cost_usd: None,
                };
                cx.emit(format!("o{}:usage", cx.pos), t, Body::Usage(usage));
            }
            "turn.cancel" => turn_end(cx, t, Some("cancelled".into())),
            "turn.ended" => turn_end(cx, t, string_of(v, "reason")),
            ty if IGNORED.contains(&ty) || ty.starts_with("turn.step.") => {}
            ty => cx.unknown(format!("type={ty}")),
        }
    }
}

fn turn_end(cx: &mut Cx<'_, State>, t: i64, reason: Option<String>) {
    match cx.state.turn {
        Some(at) => cx.emit(format!("o{at}:end"), t, Body::TurnEnd { reason }),
        None => cx.emit_at(t, Body::TurnEnd { reason }),
    };
}

fn model(cx: &mut Cx<'_, State>, m: String) {
    cx.meta().model = Some(m.clone());
    cx.state.model = Some(m);
}

/// Assistant content parts: `text` → message, `think` → reasoning.
fn parts(content: &Value, t: i64, cx: &mut Cx<'_, State>) {
    let one = [content.clone()];
    let list: &[Value] = match content {
        Value::Array(a) => a,
        Value::Null => &[],
        _ => &one,
    };
    for p in list {
        let body = match (p.as_str(), str_of(p, "type")) {
            (Some(s), _) => Body::AssistantMessage { text: s.to_owned(), model: cx.state.model.clone() },
            (_, Some("text")) => Body::AssistantMessage {
                text: str_of(p, "text").unwrap_or("").to_owned(),
                model: cx.state.model.clone(),
            },
            (_, Some("think")) => Body::Reasoning { text: str_of(p, "think").unwrap_or("").to_owned() },
            (_, other) => {
                cx.unknown(format!("part={}", other.unwrap_or("?")));
                continue;
            }
        };
        cx.emit_at(t, body);
    }
}

fn tool_call(cx: &mut Cx<'_, State>, t: i64, call_id: &str, name: &str, args: &Value) {
    cx.state.calls += 1;
    let body = Body::ToolCall { call_id: call_id.to_owned(), name: name.to_owned(), input: json_arg(args) };
    cx.emit(format!("call:{call_id}"), t, body);
}

fn tool_result(cx: &mut Cx<'_, State>, t: i64, call_id: &str, output: &Value, is_error: bool) {
    let output = output.as_str().map_or_else(|| text_of(output), str::to_owned);
    let body = Body::ToolResult { call_id: call_id.to_owned(), name: None, output, is_error };
    cx.emit(format!("result:{call_id}"), t, body);
}

fn message(m: &Value, t: i64, cx: &mut Cx<'_, State>) {
    let content = m.get("content").unwrap_or(&Value::Null);
    match str_of(m, "role").unwrap_or("") {
        "user" => {
            let user = by_user(m.get("origin"), cx);
            if !user {
                return;
            }
            if cx.state.echo > 0 {
                cx.state.echo -= 1;
                return;
            }
            let text = text_of(content);
            if !text.trim().is_empty() {
                cx.emit_at(t, Body::UserMessage { text, synthetic: false });
            }
        }
        "assistant" => {
            parts(content, t, cx);
            let calls = m.get("toolCalls").and_then(Value::as_array).cloned().unwrap_or_default();
            for c in &calls {
                let f = c.get("function").unwrap_or(c);
                let id = str_of(c, "id").unwrap_or("");
                tool_call(cx, t, id, str_of(f, "name").unwrap_or("tool"), f.get("arguments").unwrap_or(&Value::Null));
            }
            if !cx.state.steps && calls.is_empty() {
                turn_end(cx, t, Some("completed".into()));
            }
        }
        "tool" => {
            let id = str_of(m, "toolCallId").unwrap_or("").to_owned();
            tool_result(cx, t, &id, content, m.get("isError").and_then(Value::as_bool).unwrap_or(false));
        }
        "system" => {}
        role => cx.unknown(format!("message.role={role}")),
    }
}

fn loop_event(e: &Value, t: i64, cx: &mut Cx<'_, State>) {
    match str_of(e, "type").unwrap_or("") {
        "step.begin" => {
            cx.state.steps = true;
            cx.state.calls = 0;
        }
        "step.end" => {
            let last = str_of(e, "finishReason").map_or(cx.state.calls == 0, |r| r != "tool_use");
            if last {
                turn_end(cx, t, Some("completed".into()));
            }
        }
        "content.part" => parts(e.get("part").unwrap_or(&Value::Null), t, cx),
        "tool.call" => {
            let id = str_of(e, "toolCallId").unwrap_or("").to_owned();
            tool_call(cx, t, &id, str_of(e, "name").unwrap_or("tool"), e.get("args").unwrap_or(&Value::Null));
        }
        "tool.result" => {
            let id = str_of(e, "toolCallId").unwrap_or("").to_owned();
            let r = e.get("result").unwrap_or(&Value::Null);
            tool_result(
                cx,
                t,
                &id,
                r.get("output").unwrap_or(&Value::Null),
                r.get("isError") == Some(&Value::Bool(true)),
            );
        }
        "tool.progress" | "step.retrying" => {}
        ty => cx.unknown(format!("loop_event={ty}")),
    }
}

impl Kimi {
    fn session_dir(src: &Path) -> &Path {
        src.ancestors().nth(3).unwrap_or(src)
    }

    /// `state.json`, or the last version that parsed when it is mid-rewrite.
    fn state_of(&self, dir: &Path) -> SessionState {
        let path = dir.join("state.json");
        let parsed =
            std::fs::read(&path).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).filter(Value::is_object);
        let mut last = self.states.lock().unwrap();
        let Some(s) = parsed else { return last.get(&path).cloned().unwrap_or_default() };
        let custom = s.get("isCustomTitle").and_then(Value::as_bool);
        let title = match (string_of(&s, "title"), custom, string_of(&s, "customTitle")) {
            (Some(t), Some(c), _) => Some((if c { 3 } else { 2 }, t)),
            (_, _, Some(t)) => Some((3, t)),
            (Some(t), None, None) => Some((2, t)),
            _ => None,
        }
        .map(|(r, t)| (r, t.trim().to_owned()))
        .filter(|(_, t)| !t.is_empty() && t != "New Session");
        let st = SessionState {
            title,
            forked_from: string_of(&s, "forkedFrom").map(|f| f.trim().to_owned()).filter(|f| !f.is_empty()),
            work_dir: string_of(&s, "workDir"),
        };
        last.insert(path, st.clone());
        st
    }

    fn cwd_index(&self) -> Arc<HashMap<String, String>> {
        let sig = file_sig(std::slice::from_ref(&self.index));
        let mut g = self.cwds.lock().unwrap();
        if g.0 != sig {
            let text = std::fs::read_to_string(&self.index).unwrap_or_default();
            let map = text
                .lines()
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .filter_map(|v| Some((string_of(&v, "sessionId")?, string_of(&v, "workDir")?)))
                .collect();
            *g = (sig, Arc::new(map));
        }
        g.1.clone()
    }
}

impl Sidecar for Kimi {
    fn transcript_of(&self, p: &Path) -> Option<PathBuf> {
        let t = p.parent()?.join("agents/main/wire.jsonl");
        (p.file_name()? == "state.json" && under_any(p, std::slice::from_ref(&self.root)) && self.is_source(&t))
            .then_some(t)
    }

    fn sidecars(&self, src: &Path) -> Vec<PathBuf> {
        vec![Self::session_dir(src).join("state.json"), self.index.clone()]
    }

    fn sidecar(&self, src: &Path, _: &mut State) -> Vec<Record> {
        let dir = Self::session_dir(src);
        let id = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let st = self.state_of(dir);
        let cwd = self.cwd_index().get(id).cloned().or(st.work_dir);
        vec![Record::Meta(MetaPatch {
            parent: st.forked_from.filter(|f| f != id),
            title: st.title,
            cwd,
            ..Default::default()
        })]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testkit::{Fixture, group, kinds};
    use serde_json::json;
    use uniflo_core::HistoryQuery;
    use uniflo_schema::{Event, Status};

    const S: &str = "session_88888888-aaaa-bbbb-cccc-000000000008";
    const W: &str = "sessions/wd_app_abc123";

    fn l(v: Value) -> String {
        format!("{v}\n")
    }

    fn loop_ev(t: f64, e: Value) -> String {
        l(json!({"type":"context.append_loop_event","time":t,"event":e}))
    }

    fn wire() -> String {
        let mut s = l(json!({"type":"metadata","protocol_version":"1.3","created_at":1790000000000i64}));
        s += &l(json!({"type":"config.update","time":1790000000.0,"profileName":"agent","modelAlias":"kimi-k2"}));
        s += &l(json!({"type":"tools.set_active_tools","time":1790000000.0,"names":["Read"]}));
        s += &l(
            json!({"type":"turn.prompt","time":1790000001.0,"input":[{"type":"text","text":"fix the leak"}],"origin":{"kind":"user"}}),
        );
        // The context mirror of the prompt above.
        s += &l(
            json!({"type":"context.append_message","time":1790000001.0,"message":{"role":"user","content":[{"type":"text","text":"fix the leak"}]}}),
        );
        s += &loop_ev(1790000002.0, json!({"type":"step.begin","uuid":"s1"}));
        s += &loop_ev(1790000002.5, json!({"type":"content.part","part":{"type":"think","think":"where is it"}}));
        s +=
            &loop_ev(1790000003.0, json!({"type":"tool.call","toolCallId":"k1","name":"Read","args":{"path":"qr.ts"}}));
        s += &loop_ev(
            1790000004.0,
            json!({"type":"tool.result","toolCallId":"k1","result":{"output":"code","isError":false}}),
        );
        s += &loop_ev(1790000004.5, json!({"type":"step.end"}));
        s += &l(
            json!({"type":"usage.record","time":1790000004.6,"model":"kimi-k2","usage":{"inputOther":120,"output":30,"inputCacheRead":900,"inputCacheCreation":0},"usageScope":"turn"}),
        );
        s += &loop_ev(1790000005.0, json!({"type":"step.begin","uuid":"s2"}));
        s += &loop_ev(1790000005.5, json!({"type":"content.part","part":{"type":"text","text":"Fixed the cleanup."}}));
        s += &loop_ev(1790000006.0, json!({"type":"step.end"}));
        s += &l(
            json!({"type":"context.apply_compaction","time":1790000007.0,"summary":"earlier work","compactedCount":4}),
        );
        s += &l(
            json!({"type":"turn.prompt","time":1790000008.0,"input":[{"type":"text","text":"cron fired"}],"origin":{"kind":"system_trigger"}}),
        );
        s
    }

    fn setup(fx: &Fixture) -> (WithSidecars<Kimi>, PathBuf) {
        let a = WithSidecars::new(Kimi::new(fx.root().to_path_buf()));
        let p = fx.write(&format!("{W}/{S}/agents/main/wire.jsonl"), &wire());
        fx.write(
            &format!("{W}/{S}/state.json"),
            &json!({"createdAt":"2026-09-01T00:00:00Z","title":"New Session","isCustomTitle":false,"workDir":"/stale"})
                .to_string(),
        );
        fx.write(
            "session_index.jsonl",
            &(l(json!({"sessionId":S,"sessionDir":"/x","workDir":"/w/app"}))
                + &l(json!({"sessionId":"session_other","workDir":"/w/other"}))),
        );
        (a, p)
    }

    #[test]
    fn full_turn_echo_synthetic_compaction_and_index_cwd() {
        let fx = Fixture::new();
        let (a, p) = setup(&fx);
        fx.write(&format!("{W}/{S}/agents/sub-1/wire.jsonl"), &wire());
        let r = fx.index(&a, &p);
        assert_eq!(r.id, S);
        assert_eq!(
            kinds(&r.events),
            [
                "user_message",
                "reasoning",
                "tool_call",
                "tool_result",
                "usage",
                "assistant_message",
                "turn_end",
                "system",
                "user_message"
            ]
        );
        let users: Vec<_> = r.events.iter().filter(|e| matches!(e.body, Body::UserMessage { .. })).collect();
        assert_eq!(users.len(), 2, "the echo makes no second user message");
        assert!(matches!(&users[0].body, Body::UserMessage { text, synthetic: false } if text == "fix the leak"));
        assert!(matches!(&users[1].body, Body::UserMessage { text, synthetic: true } if text == "cron fired"));
        assert!(
            matches!(&r.events[7].body, Body::System { subtype, text } if subtype == "compact" && text == "earlier work")
        );
        assert!(matches!(&r.events[4].body, Body::Usage(u)
            if (u.input, u.output, u.cache_read) == (120, 30, 900) && u.model.as_deref() == Some("kimi-k2")));
        assert!(
            matches!(&r.events[5].body, Body::AssistantMessage { model, .. } if model.as_deref() == Some("kimi-k2"))
        );
        assert_eq!(r.events[0].ts, 1790000001000);
        // Ends on a synthetic input: the turn it opened is in progress.
        assert_eq!(r.status(), Status::Work);
        assert_eq!(r.meta.title, None, "\"New Session\" is a placeholder");
        assert_eq!(r.preview.as_deref(), Some("fix the leak"), "label falls back to the first input");
        assert_eq!(r.meta.cwd.as_deref(), Some("/w/app"), "session_index.jsonl wins");
        assert_eq!(r.meta.model.as_deref(), Some("kimi-k2"));
        assert_eq!(r.meta.started_at, Some(1790000000000));
        assert!(r.unknown.is_empty(), "{:?}", r.unknown);
        assert_eq!(uniflo_core::Adapter::discover(&a), vec![p.clone()], "only the main agent's wire");
    }

    #[test]
    fn idle_after_tool_free_step_and_title_survives_truncated_state() {
        let fx = Fixture::new();
        let (a, p) = setup(&fx);
        let st = fx.root().join(format!("{W}/{S}/state.json"));
        std::fs::write(
            &st,
            json!({"title":"Fix QR leak","isCustomTitle":true,"forkedFrom":"session_parent"}).to_string(),
        )
        .unwrap();
        let w = wire();
        let lines: Vec<&str> = w.lines().collect();
        std::fs::write(&p, lines[..lines.len() - 2].join("\n") + "\n").unwrap();
        let r = fx.index(&a, &p);
        assert_eq!(r.status(), Status::Idle);
        assert_eq!(r.meta.title.as_deref(), Some("Fix QR leak"));
        assert_eq!(r.meta.title_rank, 3);
        assert_eq!(r.meta.parent.as_deref(), Some("session_parent"));
        // Kimi rewrites state.json by truncating first: an empty file keeps the last title.
        std::fs::write(&st, "").unwrap();
        let out = uniflo_core::Adapter::read(&a, &p, None).unwrap();
        let g = group(out.batch);
        assert_eq!(g[S].meta.title.as_deref(), Some("Fix QR leak"));
        assert_eq!(uniflo_core::Adapter::source_for(&a, &st), Some(p.clone()));
    }

    #[test]
    fn metadata_only_wire_is_not_listed() {
        let fx = Fixture::new();
        let (a, p) = setup(&fx);
        let mut s = l(json!({"type":"metadata","protocol_version":"1.3","created_at":1790000000000i64}));
        s += &l(json!({"type":"config.update","time":1790000000.0,"profileName":"agent","systemPrompt":"x"}));
        s += &l(json!({"type":"tools.set_active_tools","time":1790000000.0,"names":[]}));
        s += &l(json!({"type":"config.update","time":1790000000.0}));
        std::fs::write(&p, s).unwrap();
        let out = uniflo_core::Adapter::read(&a, &p, None).unwrap();
        assert!(!out.batch.items.iter().any(|(_, r)| matches!(r, Record::Event(_))));
        assert!(out.batch.unknown.is_empty());
        let shown =
            out.batch.items.iter().any(|(_, r)| matches!(r, Record::Meta(m) if m.title.is_some() || m.cwd.is_some()));
        assert!(!shown, "no sidecar metadata for a shell without conversation");
    }

    #[test]
    fn explicit_turn_end_after_a_tool_free_step_is_one_turn_end() {
        let fx = Fixture::new();
        let (a, p) = setup(&fx);
        let w = wire();
        let mut s: String = w.lines().take(14).map(|l| format!("{l}\n")).collect();
        s += &l(json!({"type":"turn.ended","time":1790000006.5,"turnId":1,"reason":"completed"}));
        s += &l(json!({"type":"turn.prompt","time":1790000010.0,"input":"again","origin":{"kind":"user"}}));
        s += &l(
            json!({"type":"context.append_message","time":1790000010.0,"message":{"role":"user","content":"again"}}),
        );
        s += &loop_ev(1790000011.0, json!({"type":"step.begin","uuid":"s3"}));
        s += &loop_ev(1790000011.5, json!({"type":"content.part","part":{"type":"text","text":"Sure."}}));
        s += &loop_ev(1790000012.0, json!({"type":"step.end","finishReason":"end_turn"}));
        s += &l(json!({"type":"turn.ended","time":1790000012.5,"turnId":2,"reason":"completed"}));
        std::fs::write(&p, &s).unwrap();
        let r = fx.index(&a, &p);
        let mut ends: Vec<&str> =
            r.events.iter().filter(|e| matches!(e.body, Body::TurnEnd { .. })).map(|e| e.id.as_str()).collect();
        ends.dedup();
        let prompts: Vec<String> = s
            .lines()
            .scan(0usize, |at, l| {
                let here = *at;
                *at += l.len() + 1;
                Some((here, l))
            })
            .filter(|(_, l)| l.contains("\"turn.prompt\""))
            .map(|(at, _)| format!("o{at}:end"))
            .collect();
        assert_eq!(ends, prompts, "one id per turn, shared by step.end and turn.ended");
        assert_eq!(r.status(), Status::Idle);
        assert!(r.unknown.is_empty(), "{:?}", r.unknown);

        // History pages start at a prompt: same ids and kinds as the whole read.
        let index = uniflo_core::jsonl::dedupe_events(r.events.iter().cloned().map(Record::Event).collect());
        for limit in [1, 3, 200] {
            let mut paged = Vec::new();
            let mut before = None;
            for _ in 0..50 {
                let page = a.history(&p, S, &HistoryQuery { before, limit }).unwrap();
                let Some(first) = page.first() else { break };
                before = first.pos;
                paged.splice(0..0, page);
            }
            let ids = |evs: &[Event]| {
                let mut v: Vec<(String, &'static str)> = evs.iter().map(|e| (e.id.clone(), e.body.kind())).collect();
                v.sort();
                v
            };
            assert_eq!(ids(&paged), ids(&index), "limit {limit}");
        }
    }

    #[test]
    fn shell_metadata_arrives_with_the_first_conversation() {
        let fx = Fixture::new();
        let (a, p) = setup(&fx);
        let w = wire();
        let lines: Vec<&str> = w.lines().collect();
        std::fs::write(&p, lines[..3].join("\n") + "\n").unwrap();
        let out = uniflo_core::Adapter::read(&a, &p, None).unwrap();
        assert!(out.batch.items.is_empty(), "an empty shell is not listed");
        // The conversation starts while the daemon follows the file.
        fx.append(&p, &(lines[3..].join("\n") + "\n"));
        let out2 = uniflo_core::Adapter::read(&a, &p, Some(&out.cursor)).unwrap();
        assert!(!out2.summary);
        let g = group(out2.batch);
        assert_eq!(g[S].meta.model.as_deref(), Some("kimi-k2"), "from the shell's config.update");
        assert_eq!(g[S].meta.started_at, Some(1790000000000), "from the shell's metadata record");
        assert_eq!(g[S].meta.cwd.as_deref(), Some("/w/app"));
        assert_eq!(kinds(&g[S].events)[0], "user_message");
        let out3 = uniflo_core::Adapter::read(&a, &p, Some(&out2.cursor)).unwrap();
        assert!(out3.batch.items.is_empty(), "sent once");
    }

    #[test]
    fn migrated_history_without_steps() {
        let fx = Fixture::new();
        let (a, p) = setup(&fx);
        let mut s = l(json!({"type":"context.append_message","message":{"role":"user","content":"hello"}}));
        s += &l(
            json!({"type":"context.append_message","message":{"role":"assistant","content":[{"type":"text","text":"calling"}],"toolCalls":[{"type":"function","id":"c1","function":{"name":"Shell","arguments":"{\"cmd\":\"ls\"}"}}]}}),
        );
        s += &l(json!({"type":"context.append_message","message":{"role":"tool","toolCallId":"c1","content":"a.txt"}}));
        s += &l(
            json!({"type":"context.append_message","message":{"role":"assistant","content":[{"type":"text","text":"done"}]}}),
        );
        std::fs::write(&p, s).unwrap();
        let r = fx.index(&a, &p);
        assert_eq!(
            kinds(&r.events),
            ["user_message", "assistant_message", "tool_call", "tool_result", "assistant_message", "turn_end"]
        );
        assert!(matches!(&r.events[2].body, Body::ToolCall { input, .. } if input["cmd"] == "ls"));
        assert_eq!(r.status(), Status::Idle);
    }
}
