//! Grok CLI (xAI) sessions.
//!
//! Layout: `~/.grok/sessions/<percent-encoded cwd>/<session uuid>/`. `updates.jsonl` is an
//! ACP-style stream (`session/update` / `_x.ai/session/update`, `params.update.sessionUpdate`):
//! user, agent and thought text arrive as fragments merged into one message without trimming
//! (whitespace at fragment boundaries is content); `tool_call` + `tool_call_update` carry a
//! call and back-fill its result; `turn_completed{stop_reason}` ends the turn. Siblings are
//! rewritten in place: `summary.json` (title, cwd, model, times), `usage.json` (`turns[]`,
//! one usage per turn, `turnNumber` = `promptIndex + 1`), `events.jsonl` (`turn_ended`, the
//! turn end of builds that write no `turn_completed`). `<parent>/subagents/*/meta.json`
//! names a parent and its child sessions. A session holding only hook lines is not listed.

use crate::common::{FileSig, Sidecar, WithSidecars, file_sig, under_any};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uniflo_core::util::{home, json_arg, str_of, string_of, ts};
use uniflo_core::{Adapter, Cx, HarnessInfo, LineDecoder, MetaPatch, Record, SourceId};
use uniflo_schema::{Body, Event, Usage, session_key};

const ID: &str = "grok";
/// The subagent map is rebuilt at most this often (one walk serves a whole index pass).
const PARENTS_TTL: Duration = Duration::from_secs(2);
/// `events.jsonl` is append-only; only its newest turn record matters.
const EVENTS_TAIL: u64 = 64 * 1024;

pub fn adapters() -> Vec<Arc<dyn Adapter>> {
    vec![Arc::new(WithSidecars::new(Grok::new(home().join(".grok/sessions"))))]
}

pub struct Grok {
    root: PathBuf,
    /// child session id → parent session id, from `subagents/*/meta.json`.
    parents: Mutex<(Option<Instant>, HashMap<String, String>)>,
    /// Parsed `usage.json` per session directory, keyed by its signature.
    usage: Mutex<HashMap<PathBuf, UsageFile>>,
}

type UsageFile = (FileSig, Arc<Vec<TurnUsage>>);

impl Grok {
    fn new(root: PathBuf) -> Self {
        Grok { root, parents: Mutex::new((None, HashMap::new())), usage: Mutex::new(HashMap::new()) }
    }
}

const USER: u8 = 0;
const AGENT: u8 = 1;
const THOUGHT: u8 = 2;

/// Fragments of one message being merged.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Run {
    kind: u8,
    /// Offset of the first fragment: the message's id and `pos`.
    pos: u64,
    ts: i64,
    text: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    open: Option<Run>,
    /// `promptIndex` (0-based) of the newest user fragment; its turn is `prompt + 1`.
    prompt: Option<u64>,
    model: Option<String>,
    /// callId → tool title until the call's final result, to name results.
    names: HashMap<String, String>,
    /// Newest activity (ms): a turn end taken from `events.jsonl` must not precede it.
    last: i64,
    /// Highest turn ended by `turn_completed`.
    done: u64,
    /// Highest turn whose usage was emitted.
    usage_sent: u64,
}

#[derive(Debug, Clone)]
pub struct TurnUsage {
    turn: u64,
    ts: i64,
    usage: Usage,
}

/// Update kinds that carry nothing for the unified stream.
const IGNORED: &[&str] = &[
    "hook_execution",
    "background_tasks",
    "available_commands_update",
    "current_mode_update",
    "plan",
    "task_backgrounded",
    "task_completed",
    "auto_compact_started",
    "compaction_checkpoint",
];

impl LineDecoder for Grok {
    type State = State;

    fn info(&self) -> HarnessInfo {
        HarnessInfo { id: ID, name: "Grok" }
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    fn is_source(&self, p: &Path) -> bool {
        p.file_name().is_some_and(|n| n == "updates.jsonl") && p.ancestors().nth(3) == Some(self.root.as_path())
    }

    fn identify(&self, p: &Path) -> Option<SourceId> {
        Some(SourceId { id: p.parent()?.file_name()?.to_str()?.to_owned(), parent: None })
    }

    fn max_depth(&self) -> usize {
        2
    }

    fn decode(&self, v: &Value, cx: &mut Cx<'_, State>) {
        let Some(u) = v.pointer("/params/update") else {
            cx.unknown("params.update=missing");
            return;
        };
        let t = v
            .pointer("/params/_meta/agentTimestampMs")
            .and_then(Value::as_i64)
            .or_else(|| v.get("timestamp").and_then(ts))
            .unwrap_or(0);
        match str_of(u, "sessionUpdate").unwrap_or("") {
            kind @ ("user_message_chunk" | "agent_message_chunk" | "agent_thought_chunk") => {
                if let Some(m) = u.get("_meta") {
                    if let Some(i) = m.get("promptIndex").and_then(Value::as_u64) {
                        cx.state.prompt = Some(i);
                    }
                    if let Some(model) = string_of(m, "modelId") {
                        cx.meta().model = Some(model.clone());
                        cx.state.model = Some(model);
                    }
                }
                let k = match kind {
                    "user_message_chunk" => USER,
                    "agent_message_chunk" => AGENT,
                    _ => THOUGHT,
                };
                fragment(cx, k, t, &chunk_text(u.get("content").unwrap_or(&Value::Null)));
            }
            "tool_call" => {
                close(cx);
                let call_id = str_of(u, "toolCallId").unwrap_or("").to_owned();
                let name = string_of(u, "title").or_else(|| string_of(u, "kind")).unwrap_or_else(|| "tool".into());
                cx.state.names.insert(call_id.clone(), name.clone());
                let input = json_arg(u.get("rawInput").unwrap_or(&Value::Null));
                cx.emit(format!("call:{call_id}"), t, Body::ToolCall { call_id, name, input });
                tool_update(cx, t, u);
            }
            "tool_call_update" => {
                close(cx);
                tool_update(cx, t, u);
            }
            "turn_completed" => {
                close(cx);
                let turn = cx.state.prompt.map(|p| p + 1);
                if let Some(n) = turn {
                    if let Some(tu) = self.turn_usage(cx.src, n) {
                        let usage = Usage { model: cx.state.model.clone(), ..tu.usage };
                        cx.emit(format!("turn{n}:usage"), t, Body::Usage(usage));
                        cx.state.usage_sent = cx.state.usage_sent.max(n);
                    }
                    cx.state.done = cx.state.done.max(n);
                }
                let id = turn.map_or_else(|| format!("o{}:end", cx.pos), |n| format!("turn{n}:end"));
                cx.emit(id, t, Body::TurnEnd { reason: string_of(u, "stop_reason") });
            }
            "retry_state" | "error" => {
                let text = string_of(u, "message").unwrap_or_default();
                let sub = if str_of(u, "error_type") == Some("api") { "api_error" } else { "error" };
                cx.emit_at(t, Body::System { subtype: sub.into(), text });
            }
            "auto_compact_completed" => {
                cx.emit_at(t, Body::System { subtype: "compact".into(), text: String::new() });
            }
            ty if IGNORED.contains(&ty) => {}
            ty => cx.unknown(format!("sessionUpdate={ty}")),
        }
        cx.state.last = cx.state.last.max(t);
    }

    fn finish(&self, cx: &mut Cx<'_, State>) {
        if let Some(r) = cx.state.open.clone() {
            emit_run(cx, r).partial = true;
        }
    }
}

/// ACP content block → text; non-text blocks become `[type]`.
fn chunk_text(c: &Value) -> String {
    match str_of(c, "type") {
        Some("text") => str_of(c, "text").unwrap_or("").to_owned(),
        Some(other) => format!("[{other}]"),
        None => c.as_str().unwrap_or("").to_owned(),
    }
}

/// `tool_call(_update).content[]`: `{type:"content", content:<block>}`, diffs, terminals.
fn tool_output(u: &Value) -> String {
    let mut out = Vec::new();
    for item in u.get("content").and_then(Value::as_array).into_iter().flatten() {
        let piece = match str_of(item, "type") {
            Some("content") => chunk_text(item.get("content").unwrap_or(&Value::Null)),
            Some("diff") => format!("[diff {}]", str_of(item, "path").unwrap_or("")),
            Some(other) => format!("[{other}]"),
            None => continue,
        };
        if !piece.is_empty() {
            out.push(piece);
        }
    }
    if out.is_empty()
        && let Some(raw) = u.get("rawOutput").filter(|r| !r.is_null())
    {
        return raw.as_str().map_or_else(|| raw.to_string(), str::to_owned);
    }
    out.join("\n")
}

fn fragment(cx: &mut Cx<'_, State>, kind: u8, t: i64, piece: &str) {
    match &mut cx.state.open {
        Some(r) if r.kind == kind => r.text.push_str(piece),
        _ => {
            close(cx);
            cx.state.open = Some(Run { kind, pos: cx.pos, ts: t, text: piece.to_owned() });
        }
    }
}

fn close(cx: &mut Cx<'_, State>) {
    if let Some(r) = cx.state.open.take() {
        emit_run(cx, r);
    }
}

fn emit_run<'c>(cx: &'c mut Cx<'_, State>, r: Run) -> &'c mut Event {
    let body = match r.kind {
        USER => Body::UserMessage { text: r.text, synthetic: false },
        AGENT => Body::AssistantMessage { text: r.text, model: cx.state.model.clone() },
        _ => Body::Reasoning { text: r.text },
    };
    let e = cx.emit(format!("o{}", r.pos), r.ts, body);
    e.pos = Some(r.pos);
    e
}

fn tool_update(cx: &mut Cx<'_, State>, t: i64, u: &Value) {
    let call_id = str_of(u, "toolCallId").unwrap_or("").to_owned();
    let status = str_of(u, "status").unwrap_or("");
    if str_of(u, "sessionUpdate") == Some("tool_call_update")
        && let Some(raw) = u.get("rawInput").filter(|r| !r.is_null())
    {
        let name = cx.state.names.get(&call_id).cloned().unwrap_or_else(|| "tool".into());
        cx.emit(format!("call:{call_id}"), t, Body::ToolCall { call_id: call_id.clone(), name, input: json_arg(raw) });
    }
    let output = tool_output(u);
    let terminal = matches!(status, "completed" | "failed");
    if !terminal && output.is_empty() {
        return;
    }
    let name = if terminal { cx.state.names.remove(&call_id) } else { cx.state.names.get(&call_id).cloned() };
    let is_error = status == "failed";
    cx.emit(format!("result:{call_id}"), t, Body::ToolResult { call_id, name, output, is_error });
}

/// Token counts of one `usage.json` turn, normalized to the usage contract. `totalTokens`
/// tells whether `inputTokens` holds the cache and whether `outputTokens` holds thinking;
/// without it the OpenAI-compatible shape (cache inside input) is assumed.
fn normalize(t: &Value) -> Usage {
    let n = |k: &str| t.get(k).and_then(Value::as_u64).unwrap_or(0);
    let (i, o, cr, cw, r, total) = (
        n("inputTokens"),
        n("outputTokens"),
        n("cachedReadTokens"),
        n("cacheCreationTokens"),
        n("reasoningTokens"),
        n("totalTokens"),
    );
    let cache_apart = total > 0 && (total == i + cr + cw + o || total == i + cr + cw + o + r) && cr + cw > 0;
    let thinking_apart = total > 0 && (total == i + o + r || total == i + cr + cw + o + r) && r > 0;
    let input = if cache_apart { i } else { i.saturating_sub(cr + cw) };
    let output = if thinking_apart { o + r } else { crate::common::output_with_reasoning(o, r) };
    Usage { input, output, cache_read: cr, cache_write: cw, reasoning: r, model: None, cost_usd: None }
}

impl Grok {
    fn session_dir(src: &Path) -> &Path {
        src.parent().unwrap_or(src)
    }

    fn usage_turns(&self, src: &Path) -> Arc<Vec<TurnUsage>> {
        let path = Self::session_dir(src).join("usage.json");
        let sig = file_sig(std::slice::from_ref(&path));
        let mut cache = self.usage.lock().unwrap();
        if let Some((s, v)) = cache.get(&path)
            && *s == sig
        {
            return v.clone();
        }
        let turns: Vec<TurnUsage> = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| v.get("turns").and_then(Value::as_array).cloned())
            .into_iter()
            .flatten()
            .filter_map(|t| {
                let usage = normalize(&t);
                let turn = t.get("turnNumber").and_then(Value::as_u64)?;
                (usage.input + usage.output + usage.cache_read + usage.cache_write > 0).then(|| TurnUsage {
                    turn,
                    ts: t.get("endedAt").and_then(ts).unwrap_or(0),
                    usage,
                })
            })
            .collect();
        let v = Arc::new(turns);
        cache.insert(path, (sig, v.clone()));
        v
    }

    fn turn_usage(&self, src: &Path, n: u64) -> Option<TurnUsage> {
        self.usage_turns(src).iter().find(|t| t.turn == n).cloned()
    }

    fn parent_of(&self, id: &str) -> Option<String> {
        let mut g = self.parents.lock().unwrap();
        if !g.1.contains_key(id) && g.0.is_none_or(|at| at.elapsed() > PARENTS_TTL) {
            *g = (Some(Instant::now()), self.scan_parents());
        }
        g.1.get(id).cloned()
    }

    fn scan_parents(&self) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for session in dirs(&self.root).iter().flat_map(|g| dirs(g)) {
            for sub in dirs(&session.join("subagents")) {
                let Some(v) = read_json(&sub.join("meta.json")) else { continue };
                let parent = string_of(&v, "parent_session_id")
                    .or_else(|| session.file_name().and_then(|n| n.to_str()).map(str::to_owned));
                let Some(parent) = parent else { continue };
                for k in ["child_session_id", "subagent_id"] {
                    if let Some(child) = string_of(&v, k).filter(|c| *c != parent) {
                        out.entry(child).or_insert_with(|| parent.clone());
                    }
                }
            }
        }
        out
    }

    /// Newest `turn_started` / `turn_ended` in `events.jsonl`: (turn number, ended, ts, outcome).
    fn last_turn_event(src: &Path) -> Option<(u64, bool, i64, Option<String>)> {
        let mut f = std::fs::File::open(Self::session_dir(src).join("events.jsonl")).ok()?;
        let len = f.metadata().ok()?.len();
        f.seek(SeekFrom::Start(len.saturating_sub(EVENTS_TAIL))).ok()?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).ok()?;
        let mut turn = None;
        let mut last = None;
        for line in buf.split(|b| *b == b'\n') {
            let Ok(v) = serde_json::from_slice::<Value>(line) else { continue };
            let t = v.get("ts").and_then(ts).unwrap_or(0);
            match str_of(&v, "type") {
                Some("turn_started") => {
                    turn = v.get("turn_number").and_then(Value::as_u64).map(|n| n + 1);
                    last = turn.map(|n| (n, false, t, None));
                }
                Some("turn_ended") => last = turn.map(|n| (n, true, t, string_of(&v, "outcome"))),
                _ => {}
            }
        }
        last
    }
}

fn dirs(p: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(p).map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect()).unwrap_or_default()
}

fn read_json(p: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(p).ok()?).ok()
}

/// `%2FUsers%2Fme` → `/Users/me`.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(h) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(h);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl Sidecar for Grok {
    fn transcript_of(&self, p: &Path) -> Option<PathBuf> {
        if !under_any(p, std::slice::from_ref(&self.root)) {
            return None;
        }
        let name = p.file_name()?.to_str()?;
        if matches!(name, "summary.json" | "usage.json" | "events.jsonl") {
            let t = p.parent()?.join("updates.jsonl");
            return self.is_source(&t).then_some(t);
        }
        // A new `<parent>/subagents/<x>/meta.json` re-reads the child it names.
        if name == "meta.json" && p.parent()?.parent()?.file_name()? == "subagents" {
            let child = string_of(&read_json(p)?, "child_session_id")?;
            return dirs(&self.root).into_iter().map(|g| g.join(&child).join("updates.jsonl")).find(|t| t.is_file());
        }
        None
    }

    fn sidecars(&self, src: &Path) -> Vec<PathBuf> {
        let d = Self::session_dir(src);
        vec![d.join("summary.json"), d.join("usage.json"), d.join("events.jsonl")]
    }

    fn sidecar(&self, src: &Path, st: &mut State) -> Vec<Record> {
        let dir = Self::session_dir(src);
        let id = dir.file_name().and_then(|n| n.to_str()).unwrap_or("").to_owned();
        let mut m = MetaPatch { parent: self.parent_of(&id), ..Default::default() };
        if let Some(s) = read_json(&dir.join("summary.json")) {
            m.title = string_of(&s, "generated_title")
                .map(|t| (2, t))
                .or_else(|| string_of(&s, "session_summary").map(|t| (1, t)));
            m.cwd = s.pointer("/info/cwd").and_then(Value::as_str).filter(|c| !c.is_empty()).map(str::to_owned);
            m.model = string_of(&s, "current_model_id");
            m.started_at = s.get("created_at").and_then(ts);
            m.updated_at = s.get("last_active_at").or_else(|| s.get("updated_at")).and_then(ts);
        }
        if m.cwd.is_none() {
            m.cwd = dir.parent().and_then(|g| g.file_name()).and_then(|n| n.to_str()).map(percent_decode);
        }
        let key = session_key(ID, &id);
        let mut out = vec![Record::Meta(m)];
        let ev = |eid: String, ts: i64, body: Body| {
            Record::Event(Event {
                id: eid,
                session: key.clone(),
                ts,
                pos: None,
                partial: false,
                truncated: false,
                body,
            })
        };
        let sent = st.usage_sent;
        for t in self.usage_turns(src).iter().filter(|t| t.turn > sent) {
            let usage = Usage { model: st.model.clone(), ..t.usage.clone() };
            out.push(ev(format!("turn{}:usage", t.turn), t.ts, Body::Usage(usage)));
            st.usage_sent = st.usage_sent.max(t.turn);
        }
        if let Some((n, true, t, outcome)) = Self::last_turn_event(src)
            && n > st.done
            && t >= st.last
        {
            out.push(ev(format!("turn{n}:end"), t, Body::TurnEnd { reason: outcome }));
            st.done = n;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testkit::{Fixture, group, kinds};
    use serde_json::json;
    use uniflo_schema::Status;

    const G: &str = "sessions/%2Fw%2Fproj";
    const P: &str = "11111111-aaaa-bbbb-cccc-000000000001";
    const C: &str = "22222222-aaaa-bbbb-cccc-000000000002";

    fn gk(fx: &Fixture) -> WithSidecars<Grok> {
        WithSidecars::new(Grok::new(fx.root().join("sessions")))
    }

    fn upd(t: i64, update: Value) -> String {
        let v = json!({"timestamp": t, "method": "_x.ai/session/update",
            "params": {"sessionId": "x", "_meta": {"agentTimestampMs": t * 1000}, "update": update}});
        format!("{v}\n")
    }

    fn chunk(kind: &str, text: &str, t: i64) -> String {
        let mut u = json!({"sessionUpdate": kind, "content": {"type": "text", "text": text}});
        if kind == "user_message_chunk" {
            u["_meta"] = json!({"modelId": "grok-code-fast-1", "promptIndex": 0});
        }
        upd(t, u)
    }

    fn hook(t: i64) -> String {
        upd(
            t,
            json!({"sessionUpdate":"hook_execution","event_name":"session_start","runs":[{"name":"h","status":"ok"}]}),
        )
    }

    fn full_turn() -> String {
        let mut s = hook(1790000000);
        s += &chunk("user_message_chunk", "fix ", 1790000001);
        s += &chunk("user_message_chunk", "the bug ", 1790000001);
        s += &chunk("agent_thought_chunk", "Looking", 1790000002);
        s += &chunk("agent_thought_chunk", " closer\n", 1790000002);
        s += &upd(
            1790000003,
            json!({"sessionUpdate":"tool_call","toolCallId":"call_1","title":"Read","kind":"read",
                "status":"pending","rawInput":{"path":"src/a.rs"}}),
        );
        s += &upd(
            1790000004,
            json!({"sessionUpdate":"tool_call_update","toolCallId":"call_1","status":"completed",
                "content":[{"type":"content","content":{"type":"text","text":"fn a() {}"}}]}),
        );
        s += &chunk("agent_message_chunk", " Fixed", 1790000005);
        s += &chunk("agent_message_chunk", " it. ", 1790000005);
        s += &upd(
            1790000006,
            json!({"sessionUpdate":"turn_completed","stop_reason":"end_turn","agent_result":"x","elapsed_ms":10,"prompt_id":"p"}),
        );
        s
    }

    fn write_session(fx: &Fixture) -> PathBuf {
        let p = fx.write(&format!("{G}/{P}/updates.jsonl"), &full_turn());
        fx.write(
            &format!("{G}/{P}/summary.json"),
            &json!({"generated_title":"Fix the bug","session_summary":"s","current_model_id":"grok-4",
                "info":{"cwd":"/w/proj","id":P},"created_at":"2026-09-21T10:00:00.000Z",
                "updated_at":"2026-09-21T10:05:00.000Z","last_active_at":"2026-09-21T10:06:00.000Z"})
            .to_string(),
        );
        fx.write(
            &format!("{G}/{P}/usage.json"),
            &json!({"sessionId":P,"session":{"inputTokens":1200},"turns":[{"turnNumber":1,
                "endedAt":"2026-09-21T10:05:00.000Z","inputTokens":1200,"outputTokens":80,"cachedReadTokens":1000,
                "cacheCreationTokens":0,"reasoningTokens":30,"totalTokens":1310,"modelCalls":2,"turnCount":1}]})
            .to_string(),
        );
        fx.write(
            &format!("{G}/{P}/subagents/explore-1/meta.json"),
            &json!({"parent_session_id":P,"child_session_id":C,"subagent_id":C}).to_string(),
        );
        p
    }

    #[test]
    fn full_turn_merges_fragments_and_reads_siblings() {
        let fx = Fixture::new();
        let a = gk(&fx);
        let p = write_session(&fx);
        let r = fx.index(&a, &p);
        assert_eq!(
            kinds(&r.events),
            ["user_message", "reasoning", "tool_call", "tool_result", "assistant_message", "usage", "turn_end"]
        );
        // Fragments concatenate verbatim: leading / trailing spaces survive.
        assert!(matches!(&r.events[0].body, Body::UserMessage { text, synthetic: false } if text == "fix the bug "));
        assert!(matches!(&r.events[1].body, Body::Reasoning { text } if text == "Looking closer\n"));
        assert!(matches!(&r.events[4].body, Body::AssistantMessage { text, model } if text == " Fixed it. "
            && model.as_deref() == Some("grok-code-fast-1")));
        assert!(
            matches!(&r.events[2].body, Body::ToolCall { call_id, name, input } if call_id == "call_1" && name == "Read" && input["path"] == "src/a.rs")
        );
        assert!(matches!(&r.events[3].body, Body::ToolResult { call_id, name: Some(n), output, is_error: false }
            if call_id == "call_1" && n == "Read" && output == "fn a() {}"));
        // total = input + output + reasoning: cache inside input, thinking outside output.
        assert!(matches!(&r.events[5].body, Body::Usage(u)
            if (u.input, u.cache_read, u.output, u.reasoning) == (200, 1000, 110, 30) && u.model.as_deref() == Some("grok-code-fast-1")));
        assert!(matches!(&r.events[6].body, Body::TurnEnd { reason: Some(x) } if x == "end_turn"));
        assert_eq!(r.events[0].ts, 1790000001000);
        assert_eq!(r.status(), Status::Idle);
        assert_eq!(r.meta.title.as_deref(), Some("Fix the bug"));
        assert_eq!(r.meta.cwd.as_deref(), Some("/w/proj"));
        assert_eq!(r.meta.model.as_deref(), Some("grok-4"));
        assert_eq!(r.meta.started_at, uniflo_core::util::ts_str("2026-09-21T10:00:00.000Z"));
        assert!(r.unknown.is_empty(), "{:?}", r.unknown);
        let h =
            uniflo_core::Adapter::history(&a, &p, P, &uniflo_core::HistoryQuery { before: None, limit: 50 }).unwrap();
        assert_eq!(kinds(&h), kinds(&r.events));
    }

    #[test]
    fn subagent_session_points_at_parent_and_hook_only_shell_is_unlisted() {
        let fx = Fixture::new();
        let a = gk(&fx);
        write_session(&fx);
        let child = fx.write(&format!("sessions/%2Ftmp%2Fwt/{C}/updates.jsonl"), &full_turn());
        let r = fx.index(&a, &child);
        assert_eq!(r.id, C);
        assert_eq!(r.meta.parent.as_deref(), Some(P));
        assert_eq!(r.meta.cwd.as_deref(), Some("/tmp/wt"), "falls back to the decoded group directory");
        let shell = fx.write(&format!("{G}/33333333-aaaa-bbbb-cccc-000000000003/updates.jsonl"), &(hook(1) + &hook(2)));
        fx.write(&format!("{G}/33333333-aaaa-bbbb-cccc-000000000003/summary.json"), r#"{"current_model_id":"grok-4"}"#);
        let out = uniflo_core::Adapter::read(&a, &shell, None).unwrap();
        assert!(out.batch.items.is_empty(), "hook-only shell yields no session");
        assert!(out.batch.unknown.is_empty());
        let found = uniflo_core::Adapter::discover(&a);
        assert_eq!(found.len(), 3);
        assert_eq!(
            uniflo_core::Adapter::source_for(&a, &fx.root().join(format!("{G}/{P}/summary.json"))),
            Some(fx.root().join(format!("{G}/{P}/updates.jsonl")))
        );
        assert_eq!(
            uniflo_core::Adapter::source_for(&a, &fx.root().join(format!("{G}/{P}/subagents/explore-1/meta.json"))),
            Some(child.clone())
        );
    }

    #[test]
    fn streaming_follow_flushes_partial_then_completes() {
        let fx = Fixture::new();
        let a = gk(&fx);
        let p = fx.write(
            &format!("{G}/{P}/updates.jsonl"),
            &(chunk("user_message_chunk", "hi", 1790000001) + &chunk("agent_message_chunk", "Hel", 1790000002)),
        );
        let out = uniflo_core::Adapter::read(&a, &p, None).unwrap();
        let g = group(out.batch);
        let evs = &g[P].events;
        assert_eq!(kinds(evs), ["user_message", "assistant_message"]);
        assert!(evs[1].partial);
        assert_eq!(g[P].status(), Status::Work);
        fx.append(
            &p,
            &(chunk("agent_message_chunk", "lo", 1790000003)
                + &upd(1790000004, json!({"sessionUpdate":"turn_completed","stop_reason":"end_turn"}))),
        );
        let out2 = uniflo_core::Adapter::read(&a, &p, Some(&out.cursor)).unwrap();
        let g2 = group(out2.batch);
        let evs2 = &g2[P].events;
        assert_eq!(kinds(evs2), ["assistant_message", "turn_end"]);
        assert_eq!(evs2[0].id, evs[1].id);
        assert!(!evs2[0].partial);
        assert!(matches!(&evs2[0].body, Body::AssistantMessage { text, .. } if text == "Hello"));
    }

    #[test]
    fn late_usage_and_events_turn_end_arrive_through_siblings() {
        let fx = Fixture::new();
        let a = gk(&fx);
        let dir = format!("{G}/{P}");
        let p = fx.write(&format!("{dir}/updates.jsonl"), &chunk("user_message_chunk", "go", 1790000001));
        let out = uniflo_core::Adapter::read(&a, &p, None).unwrap();
        assert!(!uniflo_core::Adapter::changed(&a, &p, &out.cursor));
        // Builds without `turn_completed`: the turn ends in events.jsonl, usage lands after.
        fx.write(
            &format!("{dir}/events.jsonl"),
            &format!(
                "{}\n{}\n",
                json!({"ts":"2026-09-21T10:00:00Z","type":"turn_started","turn_number":0,"model_id":"grok-4"}),
                json!({"ts":"2030-01-01T00:00:00Z","type":"turn_ended","outcome":"completed"})
            ),
        );
        fx.write(
            &format!("{dir}/usage.json"),
            &json!({"turns":[{"turnNumber":1,"endedAt":"2030-01-01T00:00:00Z","inputTokens":10,"outputTokens":5}]})
                .to_string(),
        );
        assert!(uniflo_core::Adapter::changed(&a, &p, &out.cursor), "a sibling change is a change");
        let out2 = uniflo_core::Adapter::read(&a, &p, Some(&out.cursor)).unwrap();
        let g = group(out2.batch);
        assert_eq!(kinds(&g[P].events), ["usage", "turn_end"]);
        assert!(matches!(&g[P].events[0].body, Body::Usage(u) if (u.input, u.output) == (10, 5)));
        let out3 = uniflo_core::Adapter::read(&a, &p, Some(&out2.cursor)).unwrap();
        assert!(out3.batch.items.is_empty(), "nothing new, nothing re-sent");
    }

    #[test]
    fn usage_normalization_follows_total() {
        let u = normalize(
            &json!({"inputTokens":100,"outputTokens":20,"cachedReadTokens":300,"reasoningTokens":5,"totalTokens":425}),
        );
        assert_eq!((u.input, u.cache_read, u.output, u.reasoning), (100, 300, 25, 5), "cache and thinking both apart");
        let u = normalize(
            &json!({"inputTokens":400,"outputTokens":20,"cachedReadTokens":300,"reasoningTokens":5,"totalTokens":420}),
        );
        assert_eq!((u.input, u.cache_read, u.output, u.reasoning), (100, 300, 20, 5), "both inside");
        assert_eq!(percent_decode("%2FUsers%2Fme%20x"), "/Users/me x");
    }
}
